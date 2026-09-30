# Warden

A small Rust supervisor that runs N copies of a Bun (or Node) HTTP app on one
machine, with zero-downtime deploys. systemd supervises Warden, Warden
supervises the workers, and the Linux kernel spreads connections across them
with `SO_REUSEPORT`. **Warden never sits on the request path.**

```
Cloudflare → Nginx → 127.0.0.1:3000 ──kernel SO_REUSEPORT──► worker 1..N  (Bun / NestJS)
                                                              ▲
systemd → warden  (spawn · watch · restart · drain · gate · report)
```

It replaces "4 systemd units / PM2 + an nginx upstream list" with one service,
one config file and one port. It is inspired by [Platformatic Watt](https://github.com/platformatic/platformatic)'s
worker model, but it is a single ~3 MB binary that embeds no JavaScript runtime.

- **Process mode** (default, production): N Bun processes. One crash affects one worker.
- **Worker mode** (experimental): one Bun process running N `Worker` threads, about half the memory.
  If that process crashes, all workers go down together.

## Why these design choices

Everything below was checked experimentally before it was designed. The details
are in [`docs/architecture.md`](docs/architecture.md) (findings F1–F14) and
[`research/`](research).

- **Bun's `node:http` ignores `listen({ reusePort: true })`**, so NestJS/Express
  on Bun can't share a port. Warden's embedded shim wraps `Bun.serve` and fixes
  this without app changes.
- **Bun exits instantly on SIGTERM.** In testing, restarts dropped about 15 of
  every 50k requests. The shim drains first (closes the listener, sends
  `Connection: close`, waits), and reloads then dropped **0**.
- **A crashed Bun Worker leaks its listening socket.** About 1 in 4 new
  connections then silently hang. The shim closes the socket from inside the
  dying Worker, and Warden replaces the host.
- **Health checks through a shared port hit a random worker.** The shim gives
  each worker a private Unix socket, so Warden can check *that* worker.

## Quick start

```sh
cargo build --release                       # → target/release/warden (Linux x86_64 / ARM64)
cp warden.example.toml warden.toml          # edit [app]
./target/release/warden check -c warden.toml
./target/release/warden start -c warden.toml
```

Your app only has to listen on `process.env.PORT`. The minimal config:

```toml
[app]
name = "api"
args = ["run", "dist/main.js"]
working_directory = "/srv/api/current"
port = 3000

[workers]
count = 4

[health]
path = "/health"          # checked on each worker's private socket
```

## Deploying without downtime

```sh
# ship the new release (e.g. new dir + atomic `current` symlink swap), then:
warden safe-reload -c /etc/warden/api.toml     # or: systemctl reload warden@api
```

`safe-reload` goes through these stages. It exits non-zero on failure, so a deploy script can stop there.

1. **Preflight** (like `nginx -t`): re-reads and validates the config and checks
   that the command and entry script exist. It also runs `reload.preflight` if
   set. If anything fails, nothing is touched and the running config stays in
   effect. If a later step fails, the previous config is restored too, so crash
   restarts don't roll forward to the rejected version.
2. It refuses to start if any worker is already down or unhealthy.
3. **Canary**: one new worker starts *next to* the old one. It must be listening,
   pass `health_passes` checks on its private socket, pass `verify_command` if
   set, and stay healthy through `canary_soak` seconds of real traffic. Only
   then is the old worker drained. **If the canary fails it is stopped, and
   every worker still runs the previous version.**
4. The remaining workers are replaced one at a time with the same gates, with an
   optional `pause` between them. The rollout **halts on the first failure**.

```
$ warden safe-reload                      # 4 workers, canary_soak = 10
safe-reload started
  [0/4] worker 1 (canary): health checks 1/3
  [0/4] worker 1 (canary): health checks 3/3
  [0/4] worker 1 (canary): soaking, 9s left
  ...
  [0/4] worker 1: draining old process (pid 8141)
  [1/4] worker 2: health checks 2/3
  [1/4] worker 2: draining old process (pid 8142)
  ...
safe-reload complete: 4 worker(s) replaced in 20.1s

$ warden safe-reload                      # a release whose /health returns 503
safe-reload started
  [0/4] worker 1 (canary): health checks 0/3
warden: safe-reload failed at worker 1: new worker keeps failing health checks: HTTP 503.
Rolled back: every worker still runs the previous version. The previous config is back in effect.
$ echo $?
1
```

With a health path configured, the gates are mandatory. A worker that can't be
checked fails them; they are never skipped. With `port_strategy = "offset"`,
workers can't overlap, so each old worker is stopped before its replacement
starts, and a failure can't be rolled back.

`warden reload` (also SIGHUP) runs the same gates without the canary soak,
fleet check or pauses. `warden restart N` replaces one worker through the gates.

## Staying up for weeks

| | What happens | Config |
|---|---|---|
| Crash | Restart with exponential backoff. After too many restarts in a window, the worker is FAILED and retried after a cooldown. | `[restart]` |
| Unhealthy worker | Replaced gracefully (new worker ready first) after `failure_threshold` failed checks | `[health] on_failure = "replace"` |
| Hung event loop | The shim's heartbeat stops, and the worker is killed and restarted | `[watchdog] timeout` |
| Memory leak | Graceful replacement when RSS stays above the limit | `[limits] max_memory` |
| Slow degradation | Recycle every worker after a lifetime, ±10% jitter | `[limits] max_lifetime` |
| Stop / shutdown | SIGTERM to each process group, drain, SIGKILL after `grace_period` | `[shutdown]` |

## CLI

```
warden start                 run in the foreground (what systemd runs)
warden status [--json]       app + worker table (state, pid, uptime, restarts, RSS, CPU, health)
warden safe-reload           production deploy (see above)
warden reload                rolling restart through the gates
warden restart [N]           replace worker N through the gates; without N: stop + start all
warden scale N               change the worker count (not saved to the config)
warden logs [-n 50] [-f]     recent / live log lines
warden stop | shutdown       stop workers (supervisor stays) | stop everything
warden check                 validate the config
```

```
$ warden status
Application: travelerwe-api
Mode:        process
Workers:     4
Ready:       4
PID:         8139
Uptime:      27s
Memory:      4.0 MB (supervisor)
Last:        safe-reload FAILED - safe-reload failed at worker 1: new worker keeps failing
             health checks: HTTP 503. Rolled back: every worker still runs the previous version. ...

Worker   Status      PID      Uptime   Restarts  RSS        CPU     Health   Last exit
1        RUNNING     8172     25s      1         40.1 MB    0.0%    ok       -
2        RUNNING     8180     13s      1         40.1 MB    0.0%    ok       -
3        RUNNING     8188     10s      1         40.2 MB    0.0%    ok       -
4        RUNNING     8196     8s       1         40.2 MB    0.0%    ok       -
```

The CLI talks to the running supervisor over a Unix socket with mode 0600.
That socket's directory also holds the shim every worker preloads and the
per-worker health sockets. Warden refuses to use the directory unless it
owns it, it is not a symlink, and no other user can write to it.

## Production setup (systemd)

- [`contrib/warden@.service`](contrib/warden@.service): `Type=notify`, so the unit
  counts as started only once every worker is listening. It uses
  `ExecReload=warden safe-reload` and `KillMode=mixed`, and runs as an
  unprivileged user. Warden never needs root. Each instance gets its own
  `RuntimeDirectory=warden/%i`; set `[control] socket =
  "/run/warden/<name>/control.sock"`.
- [`contrib/99-warden.conf`](contrib/99-warden.conf): sets
  `net.ipv4.tcp_migrate_req = 1`. Without it, a few connections queued on a
  closing listener get reset during reloads. Measured: 9–15 per worker-mode
  reload, 0 with the setting. Warden logs a warning at startup when it's off.
- nginx: `upstream api { server 127.0.0.1:3000; keepalive 64; }` A single
  upstream entry is enough; the kernel does the balancing.
- Logs go to stdout in journald format (timestamps dropped, priority prefixes
  added). Use `journalctl -u warden@api`.
- Metrics: set `[metrics] listen = "127.0.0.1:9464"` to get Prometheus text at `/metrics`.

## Benchmarks

The same apps under PM2, Platformatic Watt, nginx, `serve` and Warden, on one
machine. Everything in the tables below is produced by one command, and anyone
can re-run it:

```sh
cargo xtask bench            # all suites (~25 min on 2 CPUs); rewrites the tables below
cargo xtask bench --quick    # a 5-minute smoke test
```

What each suite does, what each number means and the fairness rules are in
[`bench/README.md`](bench/README.md); findings, caveats and the before/after
log of every optimisation are in [`docs/benchmarks.md`](docs/benchmarks.md).
The machine is small (2 CPUs shared with the load generator), so compare
columns, not absolute numbers.

<!-- bench:start -->
<!-- bench:end -->

## Development

```sh
cargo test                  # unit + integration tests (integration tests need `bun` and `node` on PATH)
cargo clippy --all-targets
cargo xtask bench           # benchmarks (see above); `cargo xtask bench --help`
```

All `unsafe` code is in [`src/sys.rs`](src/sys.rs): system calls the standard
library doesn't expose, and the few that measurably pay on a hot path (the
static server and log capture), each with a SAFETY note and tests. The rest
of the crate is `#![deny(unsafe_code)]`.

Layout: `src/supervisor.rs` (event loop), `src/supervisor/rollout.rs` (gates,
canary, rollback), `src/supervisor/upkeep.rs` (watchdog, recycling),
`src/process.rs` (spawning, fd-3 IPC), `shim/` (embedded JS), `tests/`,
`bench/`, `research/`.

Limitations:

- Linux is authoritative. macOS works for development, but has no `/proc`
  readiness or per-connection balancing.
- Worker mode needs Bun and the shim.
- Node apps share the port through Warden's shim (`--import`, Node ≥ 22.12
  for `reusePort`); older Node needs `port_strategy = "offset"`.
- If Warden is SIGKILLed, workers are signalled via `PR_SET_PDEATHSIG` (direct
  children only). Under systemd the cgroup takes care of the rest.
- Don't run Warden as PID 1 in a container; use `tini`, or `docker run --init`, to reap orphans.
- A log consumer that can't keep up (a stuck journald, a full disk) costs
  log lines, never supervision: lines past the queue bounds are dropped,
  reported in the log and counted (`log_lines_dropped` in `status --json`,
  `warden_log_lines_dropped_total` in the metrics). With
  `[logging] max_lines_per_sec = 0` Warden keeps every line by slowing a
  flooding app down instead (for at most a second per read).

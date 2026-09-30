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

If you know PM2 you know most of it: `warden` in place of `pm2`. `warden help`
has everything; the differences are in the right column.

| PM2 | Warden | Difference |
|---|---|---|
| `pm2 start app.js -i 4 --name api` | `warden start app.js -i 4 --name api` | Waits until the app is up and says so if it isn't (`--no-wait` returns at once). Any program or command line works, as with PM2 |
| `pm2 list`, `pm2 jlist` | `warden list`, `warden list --json` | ~2 ms instead of ~160 ms |
| `pm2 describe api` | `warden describe api` | Also the last exits and the last rollout |
| `pm2 reload api` | `warden restart api`, `warden reload api` | One worker at a time through health gates; a failure stops and rolls back. `restart --hard` is PM2's `restart` |
| | `warden deploy api` | Preflight, canary with soak, then the rest, with rollback |
| `pm2 logs api` | `warden logs api` | `--history --grep --since 2h --json` over rotated and gzipped files, pipe-friendly |
| `pm2 serve dist 8080` | `warden serve dist 8080` | A static server on par with nginx (see Benchmarks) |
| `pm2 save`, `resurrect`, `startup` | `warden save`, `resurrect`, `startup` | One systemd unit per app, no daemon |
| `pm2 monit` | `warden top` | |
| | `warden doctor` | Environment problems (kernel settings, limits, ports, permissions), each with its fix |

Every problem Warden logs says what happened, why, what it did and how to fix
it; [`docs/troubleshooting.md`](docs/troubleshooting.md) collects them by symptom.

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
cargo xtask bench            # all suites (~20 min on 2 CPUs); rewrites the tables below
cargo xtask bench --quick    # a ~10-minute smoke test
```

What each suite does, what each number means and the fairness rules are in
[`bench/README.md`](bench/README.md); findings, caveats and the before/after
log of every optimisation are in [`docs/benchmarks.md`](docs/benchmarks.md).
The machine is small (2 CPUs shared with the load generator), so compare
columns, not absolute numbers.

<!-- bench:start -->
<!-- Generated by `cargo xtask bench`; edit xtask/src/main.rs or bench/*.ts, not this text. -->

Measured 2026-09-30 on 2 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 8 GB RAM, Linux 6.18.44-fc-v50 x86_64. The load generator (oha, 64 connections) runs on the same CPUs as the apps, so compare columns with each other, not with other machines. Raw numbers: `bench/results/latest/`.

### Node.js app (node:http)

The same app under each manager: 4 workers, one port. PM2 in cluster mode, Watt with 4 worker threads, Warden with 4 processes.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | watt | warden-process |
|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 226 / 91 | 350.8 / 150 | 496.4 / 473.8 | 233.6 / 96.8 |
| total RAM after load: RSS / PSS (MB) | 282.1 / 129.5 | 388.3 / 186.8 | 530.6 / 508.1 | 289.6 / 135.4 |
| manager RAM (MB) | 0 | 66.4 | (in total) | 6.4 |
| manager idle CPU (%) | 0 | 0.2 | 3.4 | 0 |
| startup to 4 serving (ms) | 188 | 879 | 3143 | 211 |
| crash recovery (ms) | n/a (nothing restarts it) | 352 | 1532 | 139 |
| requests failed during a crash | n/a | 2 of 2548 | 1 of 7237 | 0 of 3365 |
| rolling restart under load: failed | n/a | 0 of 2018 (794 ms) | 0 of 3193 (3594 ms) | 0 of 2896 (533 ms) |
| status command (ms) | n/a | 168.8 | 509.7 | 2.1 |
| /plaintext req/s | 65830 | 51625 | 54211 | 64100 |
| /plaintext p50 / p99 (ms) | 0.9 / 3.44 | 0.58 / 7.97 | 0.73 / 7.3 | 0.89 / 4.05 |
| /plaintext errors | 0 | 0 | 0 | 0 |
| /json req/s | 65786 | 52856 | 50015 | 65866 |
| /json p50 / p99 (ms) | 0.91 / 2.98 | 0.61 / 8.13 | 1.01 / 5.32 | 0.92 / 2.79 |
| /json errors | 0 | 0 | 0 | 0 |
| /cpu req/s | 827 | 847 | 815 | 813 |
| /cpu p50 / p99 (ms) | 77.11 / 155.74 | 74.4 / 145.4 | 76.12 / 158.27 | 77.78 / 159.31 |
| /cpu errors | 0 | 0 | 0 | 0 |

### NestJS app on Node.js

A minimal NestJS (Express) app with the same endpoints, 4 workers.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | watt | warden-process |
|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 399.5 / 229.4 | 486.6 / 268.6 | 587.3 / 564.8 | 404.4 / 232.3 |
| total RAM after load: RSS / PSS (MB) | 695.3 / 525.1 | 811.9 / 593.6 | 1130.9 / 1108.4 | 701 / 528.5 |
| manager RAM (MB) | 0 | 66.5 | (in total) | 6.1 |
| manager idle CPU (%) | 0 | 0.2 | 2.4 | 0 |
| startup to 4 serving (ms) | 1264 | 1944 | 4897 | 1360 |
| crash recovery (ms) | n/a (nothing restarts it) | 1636 | 2130 | 729 |
| requests failed during a crash | n/a | 2 of 3052 | 0 of 4337 | 1 of 2544 |
| rolling restart under load: failed | n/a | 0 of 1226 (1838 ms) | 0 of 1900 (4666 ms) | 0 of 1691 (1614 ms) |
| status command (ms) | n/a | 168.2 | 510.8 | 1.7 |
| /plaintext req/s | 10008 | 8042 | 8683 | 9911 |
| /plaintext p50 / p99 (ms) | 5.5 / 25.65 | 6.89 / 28.24 | 6.07 / 31.56 | 5.68 / 23.98 |
| /plaintext errors | 0 | 0 | 0 | 0 |
| /json req/s | 10062 | 9154 | 9321 | 9707 |
| /json p50 / p99 (ms) | 5.86 / 20.11 | 6.27 / 23.56 | 6.15 / 26.69 | 6.22 / 19.9 |
| /json errors | 0 | 0 | 0 | 0 |
| /cpu req/s | 720 | 730 | 764 | 764 |
| /cpu p50 / p99 (ms) | 81.12 / 246.25 | 91.37 / 142.56 | 82.64 / 146.01 | 79.64 / 148.88 |
| /cpu errors | 0 | 0 | 0 | 0 |

### Bun app (Bun.serve)

4 workers. PM2 has no cluster mode for Bun, so it runs 4 fork-mode instances sharing the port with reusePort. Watt does not run Bun apps.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | warden-process | warden-worker |
|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 153.6 / 53.5 | 287.4 / 147.9 | 176.6 / 68.7 | 74.1 / 55.5 |
| total RAM after load: RSS / PSS (MB) | 174.9 / 58.1 | 292.2 / 144 | 198.8 / 74.5 | 79.2 / 58.1 |
| manager RAM (MB) | 0 | 64.8 | 6.8 | 5.1 |
| manager idle CPU (%) | 0 | 0.2 | 0 | 0 |
| startup to 4 serving (ms) | 58 | 650 | 81 | 90 |
| crash recovery (ms) | n/a (nothing restarts it) | 176 | 57 | 114 |
| requests failed during a crash | n/a | 0 of 11570 | 0 of 11405 | 0 of 11102 |
| rolling restart under load: failed | n/a | 0 of 12228 (643 ms) | 0 of 10309 (259 ms) | 0 of 11297 (259 ms) |
| status command (ms) | n/a | 171.3 | 1.8 | 1.7 |
| /plaintext req/s | 95660 | 83766 | 96353 | 98247 |
| /plaintext p50 / p99 (ms) | 0.4 / 4.34 | 0.67 / 3.77 | 0.45 / 4.01 | 0.42 / 4.23 |
| /plaintext errors | 0 | 0 | 0 | 0 |
| /json req/s | 87817 | 74886 | 83323 | 82981 |
| /json p50 / p99 (ms) | 0.6 / 3.3 | 0.78 / 2.8 | 0.56 / 3.96 | 0.59 / 4.86 |
| /json errors | 0 | 0 | 0 | 0 |
| /cpu req/s | 1243 | 1238 | 1252 | 1243 |
| /cpu p50 / p99 (ms) | 49.51 / 100.72 | 48.57 / 98.22 | 52.61 / 96.84 | 48.48 / 99.18 |
| /cpu errors | 0 | 0 | 0 | 0 |

### NestJS app on Bun

The NestJS app on Bun, 4 workers; Warden also in worker (thread) mode.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | warden-process | warden-worker |
|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 340 / 197.9 | 421.8 / 261.8 | 347.8 / 203.1 | 179.8 / 157.8 |
| total RAM after load: RSS / PSS (MB) | 432.4 / 288.3 | 528.5 / 366.6 | 434.2 / 287.4 | 346.1 / 323.9 |
| manager RAM (MB) | 0 | 65.4 | 6.2 | 5.1 |
| manager idle CPU (%) | 0 | 0.2 | 0 | 0 |
| startup to 4 serving (ms) | 757 | 1390 | 771 | 744 |
| crash recovery (ms) | n/a (nothing restarts it) | 839 | 458 | 920 |
| requests failed during a crash | n/a | 0 of 5405 | 0 of 4625 | 0 of 2931 |
| rolling restart under load: failed | n/a | 2406 of 6090 (1387 ms) | 0 of 3654 (1311 ms) | 0 of 3444 (1065 ms) |
| status command (ms) | n/a | 167.3 | 1.7 | 1.8 |
| /plaintext req/s | 28624 | 25591 | 28313 | 24612 |
| /plaintext p50 / p99 (ms) | 1.44 / 11.42 | 1.6 / 15.13 | 1.4 / 12.09 | 1.31 / 19.77 |
| /plaintext errors | 0 | 0 | 0 | 0 |
| /json req/s | 28880 | 26338 | 28285 | 25560 |
| /json p50 / p99 (ms) | 1.4 / 10.41 | 1.72 / 12.63 | 1.61 / 10.71 | 1.45 / 17.14 |
| /json errors | 0 | 0 | 0 | 0 |
| /cpu req/s | 1155 | 1101 | 1182 | 1161 |
| /cpu p50 / p99 (ms) | 56.23 / 105.78 | 52.37 / 104.63 | 49.08 / 93.58 | 49.43 / 94.92 |
| /cpu errors | 0 | 0 | 0 | 0 |

### Static files

`warden serve` against nginx, `pm2 serve` and the `serve` package, 4 workers each (serve has no cluster mode).

4 workers · nginx/1.24.0 (Ubuntu) · pm2 6.0.14 · serve 14.2.6 · node v22.22.2 · warden 0.1.0

| | warden | nginx | pm2-serve | serve (1 process) |
|---|---|---|---|---|
| processes | 5 | 5 | 5 | 1 |
| total RAM idle: RSS / PSS (MB) | 23.4 / 6.3 | 24.1 / 12.7 | 320.5 / 131.4 | 97.2 / 81.6 |
| total RAM after load: RSS / PSS (MB) | 24.8 / 7.6 | 25.6 / 12.9 | 466.9 / 273.7 | 188.8 / 173.2 |
| startup (ms) | 18 | 16 | 676 | 346 |
| /index.html req/s | 86994 | 88526 | 16549 | 16439 |
| /index.html p50 / p99 (ms) | 0.54 / 3.51 | 0.52 / 5.34 | 3.05 / 16.87 | 3.65 / 7.18 |
| /index.html errors | 0 | 0 | 0 | 0 |
| /assets/app.3f9a2c1b.js req/s | 67542 | 72738 | 12814 | 1663 |
| /assets/app.3f9a2c1b.js p50 / p99 (ms) | 0.64 / 6.56 | 0.71 / 4.23 | 4.19 / 18.58 | 37.04 / 75.19 |
| /assets/app.3f9a2c1b.js errors | 0 | 0 | 0 | 0 |
| /media/video.bin req/s | 4756 | 4431 | 2006 | 1012 |
| /media/video.bin p50 / p99 (ms) | 13.58 / 27.27 | 13.4 / 33.58 | 30 / 81.53 | 58.61 / 104.92 |
| /media/video.bin MB/s | 4749.7 | 4424.5 | 2000.1 | 1006.1 |
| /media/video.bin errors | 0 | 0 | 0 | 0 |
| /index.html (new connection each) req/s | 28595 | 27712 | 4476 | 9835 |
| /index.html (new connection each) p50 / p99 (ms) | 2.15 / 5.11 | 2.2 / 5.66 | 12.9 / 40.82 | 6.17 / 11.93 |
| /index.html (new connection each) errors | 0 | 0 | 0 | 0 |

### Log-heavy apps

What capturing worker output costs the manager. Both write the app's stdout to a log file.

node v22.22.2 · pm2 6.0.14 · warden 0.1.0

Steady: 4 workers × 5000 lines/s × 10 s

| | warden | pm2 |
|---|---|---|
| lines written / in the log file | 200000 / 200000 | 200000 / 200000 |
| manager CPU (s) | 0.34 | 1.43 |
| manager peak RAM (MB) | 5.5 | 74.8 |

Flood: 1 worker writing 200 MB to stdout as fast as it is read

| | warden | pm2 | warden (keep all) |
|---|---|---|---|
| time to write it (ms) | 235 | 1287 | 603 |
| throughput (MB/s) | 851.1 | 155.4 | 331.7 |
| manager CPU (s) | 0.18 | 1.59 | 0.72 |
| manager CPU per GB (s) | 0.92 | 8.14 | 3.69 |
| manager peak RAM (MB) | 5.6 | 81.9 | 5.7 |
| lines in the log file | 10000 | 2076388 | 2076388 |

### Many apps on one host

10 apps (one idle Node process each): the manager's memory and CPU, and how fast everyday commands answer.

10 apps, one idle Node process each · node v22.22.2 · pm2 6.0.14 · warden 0.1.0

| | warden | pm2 |
|---|---|---|
| manager processes | 10 | 1 |
| manager RAM, all apps: RSS (MB) | 49.1 | 76.9 |
| manager RAM, all apps: PSS (MB) | 7.3 | 36.1 |
| manager idle CPU (%) | 0 | 0.7 |
| start 10 apps (ms) | 579 | 2181 |
| `list` (ms) | 2.6 | 167.9 |
| `list --json` (ms) | 2.7 | 160.3 |
| `describe app3` (ms) | 1.9 | 161.7 |
| `logs app3 --nostream` (ms) | 1.7 | 169.4 |
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

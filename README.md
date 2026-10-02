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

## Install

Prebuilt archives are attached to each
[GitHub Release](https://github.com/oceanwap/warden/releases). Where no
release has been published yet (or to run the latest `main`), build from
source (see [Quick start](#quick-start) and [Development](#development)):

```sh
git clone https://github.com/oceanwap/warden.git
cd warden
cargo build --release                       # → target/release/warden
```

`install.sh` picks the release archive for your OS and CPU, checks it against
the release's `SHA256SUMS`, and puts `warden` in `/usr/local/bin` (as root) or
`~/.local/bin`. With no release published it stops with a message saying the
release files are missing. `WARDEN_DOWNLOAD_URL` points it
at a directory that already holds the release files (a mirror, or CI testing
an unreleased build).

Each release has two products for Linux
(x86_64, arm64) and macOS (arm64, x86_64):

- **CLI only**: `warden-<version>-<os>-<arch>.tar.gz` with the `warden`
  binary, this README and `contrib/` (systemd units, sysctl file, nginx site). The Linux
  binaries need glibc 2.28 or newer (RHEL 8, Debian 10, Ubuntu 18.10+).
- **GUI + CLI**: `warden-gui-<version>-<os>-<arch>` (`.tar.gz` on Linux, a
  zipped `Warden.app` on macOS) with `warden-gui` next to `warden` (see
  [GUI](#gui)). On Linux the GUI links only glibc (2.35+: built on Ubuntu
  22.04) and loads the display libraries at run time, which every desktop
  has: `libxkbcommon`, then on Wayland `libwayland-client` and
  `libwayland-cursor`, on X11 `libX11`, `libX11-xcb`, `libXcursor`, `libXi`
  and `libxkbcommon-x11`. It draws on the CPU
  (tiny-skia): no GPU, Vulkan or OpenGL driver is needed. macOS: the app
  is not notarized by Apple yet, so the first open is refused ("Apple could
  not verify…"); then System Settings → Privacy & Security → Open Anyway
  (macOS 15 dropped the right-click → Open shortcut), or
  `xattr -dr com.apple.quarantine /Applications/Warden.app`. The CLI
  installed by `install.sh` is not affected.

After a Release exists:

```sh
curl -fsSLO https://github.com/oceanwap/warden/releases/latest/download/install.sh
sh install.sh                        # or: WARDEN_VERSION=0.1.0 sh install.sh
```

By hand: download the archive and `SHA256SUMS`, check that
`sha256sum <archive>` (macOS: `shasum -a 256 <archive>`) prints the hash on
the archive's line in `SHA256SUMS`, then copy `warden` onto your PATH.

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
   optional `pause` between them (each old worker drains while the next one is
   replaced). The rollout **halts on the first failure**.

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

**WebSockets and SSE.** A long-lived connection never finishes by itself, so
a draining worker would hold it until `grace_period` and then be killed, and
its clients would see a reset (WebSocket 1006, a broken EventSource). Instead,
the shim gives these connections `[shutdown] long_lived_timeout` (default
2 s) to end by themselves, then closes each WebSocket with **1001 (Going
Away)** and ends each SSE response cleanly after its last complete event.
Clients reconnect, and land on the new workers: EventSource does this by
itself, and WebSocket clients should reconnect on close. This covers `Bun.serve`
(`websocket` handlers; `text/event-stream` bodies from a ReadableStream, a
`type: "direct"` stream or an async generator) and `node:http` on Node and
Bun (WebSockets taken over through `'upgrade'`, as the `ws` library does;
SSE written with `res.write`), in process and worker mode. Other streamed
responses, such as downloads, are never cut short: they finish like any
request in flight, within `grace_period`. Each worker logs what it closed:
`closed long-lived connections … websockets=3 sse=12`.

**Drains overlap.** A rolling restart doesn't wait for each old worker to
finish: once its replacement listens and has passed the gates, the old one
stops taking connections and drains in the background while the next
worker is replaced. So 4 workers holding WebSocket clients restart in about
one `long_lived_timeout` plus the startups (2.3-2.5 s measured), not one
per worker (~8.5 s). Up to `[reload] max_draining` (default 4) old workers
drain at once; past that the next replacement waits for one to exit, since
each holds its memory until then (`max_draining = 1`: one worker at a time, as
before). The command returns, and the rollout counts as done, once every
old worker has exited. `warden status` lists the ones still draining as
`2 (old)  DRAINING`.

**Faster rollouts**: `[reload] surge = 2` (or `"all"`) starts that many new
workers at once, each next to the worker it replaces. Once every one of them has
passed the gates, the old ones drain together and the next batch starts. If one
fails, every new worker of the batch is stopped and the old ones keep serving.
safe-reload still runs its canary alone first. The cost is memory: surge N runs up
to N extra workers for a few seconds (`"all"`: twice the workers), and old ones
still draining count too: a rollout runs at most max(`surge`, `max_draining`)
processes beyond the worker count. It needs
workers that can overlap, so not with `port_strategy = "offset"`.

**Release pinning** (`[app] pin_release`, on by default): Warden resolves a
`current` symlink in `working_directory` when it starts and when a reload,
safe-reload or restart begins, and starts workers in that real path (with
`args` that go through the symlink rewritten too). A worker that crashes after
you swapped the symlink but before you reloaded comes back on the release the
others run, not the new one. `warden status` shows the pinned release. If
that directory is deleted, the next restart falls back to `current` and logs a
warning.

## Staying up for weeks

| | What happens | Config |
|---|---|---|
| Crash | Restart with exponential backoff. After too many restarts in a window, the worker is FAILED and retried after a cooldown. | `[restart]` |
| Crash, without the startup time | A hot standby (started, app initialized, not listening) takes the dead worker's slot in a few milliseconds; a new standby starts in the background | `[workers] standby` |
| Unhealthy worker | Replaced gracefully (new worker ready first) after `failure_threshold` failed checks | `[health] on_failure = "replace"` |
| Hung event loop | The shim's heartbeat stops, and the worker is killed and restarted | `[watchdog] timeout` |
| Slow event loop | Each heartbeat carries the worker's event-loop delay over the last second (p50/p99/max, sampled natively every 100 ms: no cost per request). `warden list`/`top`/`describe` show p99 (`loop p99`), `status --json` all three (`loop_delay`), Prometheus `warden_worker_event_loop_delay_{p50,p99,max}_seconds`; a WARN when p99 stays high for 10 s | `[watchdog] loop_delay_warn` |
| Memory leak | Graceful replacement when RSS stays above the limit | `[limits] max_memory` |
| Slow degradation | Recycle every worker after a lifetime, ±10% jitter | `[limits] max_lifetime` |
| Stop / shutdown | SIGTERM to each process group, drain (WebSockets closed with 1001 and SSE streams ended after `long_lived_timeout`), SIGKILL after `grace_period` | `[shutdown]` |
| Why it died | `last_exit` and the log line say who ended a worker: a crash (`SIGSEGV`, `SIGABRT`), the kernel's OOM killer (from its cgroup's `oom_kill` count; `probably …` when other workers of that cgroup died of SIGKILL at the same moment), Warden, or another process, with a hint for the fix | |

### Hot standbys: crash recovery in milliseconds

A crashed worker normally comes back after the runtime and the app have
started again: in the benchmark 54 ms for a small Bun app, 129 ms for
node:http, 687 ms for NestJS on Node. With standbys, that work is done
before the crash:

```toml
[workers]
count = 4
standby = 1      # one extra worker, started but not listening
```

Warden starts the standby once the workers are ready. The shim lets the app
initialize completely but holds back its listen on `app.port` (Bun.serve,
node:http, Express, NestJS; servers on other ports start as usual), so the
standby takes no traffic. When a worker dies, Warden tells the standby to
listen: it joins the shared port in about a millisecond and becomes that
worker (its number, `NODE_APP_INSTANCE`, its log label). A new standby then
starts in the background. Measured from `kill -9` to the port answering
again, with one worker: 4-15 ms (debug build, idle machine), whatever the
app's startup time; in the benchmark (4 workers, under load) 17-57 ms
against 54-687 ms without a standby and 166-1,182 ms under PM2.

- A promotion is the slot's restart: it is counted, backed off and ends in
  FAILED like any restart; the standby only saves the startup.
- A standby must pass the same health gates as a rollout's new worker before
  it can be promoted, and while idle gets the health checks and the
  watchdog; a failing one is replaced, never promoted. Standbys that keep
  crashing back off, then stop until `failed_cooldown` or `warden reset`.
- Deploys: during `reload`, `safe-reload` or `restart`, crashed workers
  restart the normal way; when the deploy succeeds the standbys (still on the
  previous version) are replaced, and when it rolls back they stay.
  Recycling (memory, lifetime, health) uses a standby as the replacement.
- Memory: a standby costs about one idle worker (RSS: 42 MB for a small Bun
  app, 57 MB on Node, 84 MB for NestJS on Bun, ~100 MB on Node), since the
  app is fully loaded; much of that is shared with the workers (one standby
  next to two Bun workers: +41 MB RSS, +11 MB PSS).
- Code that runs only on instance 0 and decides at startup (a cron) sees a
  number past the workers' in a standby; after promotion `NODE_APP_INSTANCE`
  is the slot's, and `process.on("warden:promote", ...)` runs late setup.

`warden status` lists standbys after the workers as `s1`, `s2`… (`WARMING`
then `STANDBY`; `standbys` in `--json`), `warden events` and the GUI show
their events as `worker s1`, and the GUI counts their memory.
A standby starts in the pinned release (`pin_release`) and is promoted only
while that is still the workers' release. Process mode only, with
`app.port`, a shared port and the shim (bun and node commands).

## Environment variables

A worker starts with the supervisor's own environment (`PATH`, `HOME`, what
systemd or your shell gave it), then, each one winning over the ones
before:

1. `[app] env_file`: `KEY=value` lines (dotenv / systemd `EnvironmentFile`
   syntax), relative to the config file. Keep secrets there, mode 0600;
   `warden reload` reads it again.
2. `[app] env`.
3. Warden's variables below. They win over the two above: `env = { PORT =
   "8080" }` next to `port = 3000` gives the app 3000 (`warden env` marks
   the override).

`warden env api` prints exactly that for worker 1 (`api:3` for worker 3),
with where each value comes from; the app's values are hidden unless
`--show-secrets`. `warden config api` prints the whole effective config as
JSON, defaults included.

| Variable | Value | In |
|---|---|---|
| `PORT` | `[app] port`; with `port_strategy = "offset"`, `port + N - 1` for worker N | every worker and standby, `verify_command`; only with `app.port` |
| `NODE_APP_INSTANCE` (the name is `[app] instance_var`; `""` sets none) | worker N gets `N - 1` (PM2's 0-based index). A hot standby gets a number past the workers' until it is promoted, then its slot's | process workers and standbys; in worker mode each Worker (set by the shim) |
| `WARDEN_APP` | the app's name | everything Warden runs: workers, `verify_command`, `preflight` |
| `WARDEN_WORKER_ID` | the worker number, 1..N. `0` in a standby until promoted (then its slot's). Worker mode: each Worker 1..N, none in the host process | workers; `verify_command`: the worker it checks (`host` in worker mode, `s1`… for a standby) |
| `WARDEN_WORKER_COUNT` | workers configured when the process started (after `warden scale`, running workers keep the old count; a promoted standby gets the current one) | workers, standbys, worker-mode host and Workers |
| `WARDEN_MODE` | `process` or `worker` | workers |
| `WARDEN_STANDBY` | `1` in a hot standby until it is promoted | standbys, their `verify_command` |
| `WARDEN_WORKER_PID`, `WARDEN_WORKER_SOCKET`, `WARDEN_WORKER_SOCKETS` | the new worker's pid and private health socket(s) (space-separated; one per Worker in worker mode) | `verify_command` only |
| `WARDEN_WORKERS`, `WARDEN_ENTRY`, `WARDEN_SHIM` | how many Workers, the module each imports, the shim | worker-mode host process |
| `WARDEN_STATIC` | the `[static]` section as JSON | `warden serve` workers |
| `WARDEN_IPC_FD` | `3`: the shim's channel to Warden (readiness, heartbeat) | every worker |
| `WARDEN_HEARTBEAT_MS`, `WARDEN_DRAIN_MS`, `WARDEN_LONG_LIVED_MS`, `WARDEN_STOP_SIGNAL`, `WARDEN_WAIT_READY`, `WARDEN_REUSE_PORT`, `WARDEN_HEALTH_DIR`, `WARDEN_INSTANCE_VAR`, `WARDEN_INSTANCE` | settings for the shim: heartbeat period (1000), `shutdown.drain_ms`, `long_lived_timeout` in ms, `shutdown.signal`, `1` with `wait_ready`, `1` with a shared port, where private health sockets go, the instance variable's name, an internal process number (unique per supervisor run) | every worker |

PM2's `pm_id` and `name` are not set: use `WARDEN_WORKER_ID` and
`WARDEN_APP` (`warden pm2-migrate` leaves them out of the migrated env).

## CLI

If you know PM2 you know most of it: `warden` in place of `pm2`. `warden help`
has everything; the differences are in the right column.

| PM2 | Warden | Difference |
|---|---|---|
| `pm2 start app.js -i 4 --name api` | `warden start app.js -i 4 --name api` | Waits until the app is up and says so if it isn't (`--no-wait` returns at once). An app that can't start (every worker crashes before one is ready: a syntax error, a missing module, a port in use) fails at once with exit 1, its last error output and a hint; its workers are stopped and it stays listed as `errored`, as PM2 leaves it, until `warden start` again. Any program or command line works, as with PM2 |
| `pm2 list`, `pm2 jlist` | `warden list`, `warden list --json` | The same boxed table with an `id` column, ~2 ms instead of ~140-160 ms. One row per worker; colors on a terminal (`NO_COLOR` turns them off) |
| `pm2 restart 0`, `pm2 stop 1 2` | `warden restart 0`, `warden stop 1,2` | Every command that takes an app takes its id from `warden list`, a name, a namespace or `all`, one or several: `warden start 0,1,2`, `warden stop 0-3`, `warden restart api web:2` (`:2`: one worker). Ids are numbered the first time Warden sees an app (alphabetically for the first batch, then in creation order), kept in `ids.json` in the state directory, and never change; only `warden delete` frees one. Use names in scripts |
| `pm2 describe api` | `warden describe api` | A key \| value box and the workers' box, like PM2's; also the last exits and the last rollout. `status <app>`, `daemon status` and `doctor` are boxes too. Like PM2, `start`, `stop`, `restart`, `reload`, `delete`, `scale`, `reset`, `resurrect` and `serve` print the app table when they finish, on a terminal (set `WARDEN_TABLE=1` to get it in a log; a script's output is unchanged) |
| `pm2 reload api` | `warden restart api`, `warden reload api` | One worker at a time through health gates; a failure stops and rolls back. `restart --hard` is PM2's `restart` |
| | `warden deploy api` | Preflight, canary with soak, then the rest, with rollback |
| `pm2 logs api` | `warden logs api` | `--history --grep --since 2h --json` over rotated and gzipped files (every worker's with `per_worker_files`), pipe-friendly |
| `pm2 flush api` | `warden flush api` | The same: empties the in-memory buffer and the current log files (Warden's, each worker's out and err file), lists them, keeps rotated ones; safe while workers write |
| `pm2 serve dist 8080` | `warden serve dist 8080` | A static server as fast as nginx, faster on small files (see Benchmarks) |
| `pm2 save`, `resurrect`, `startup` | `warden save`, `resurrect`, `startup` | One systemd unit per app (root or `--user`), a launchd job on macOS; see [Surviving reboots and crashes](#surviving-reboots-and-crashes) |
| `pm2 monit` | `warden top`, `warden events` | `events`: every worker, rollout and supervisor event as it happens (`--json` for scripts, `--logs` for output) |
| PM2's daemon | `warden daemon` (wardend) | Optional: live events for every app on one socket, and restarts dead supervisors. Apps never depend on it; killing it stops nothing. `warden start` starts it (`WARDEN_NO_DAEMON=1` doesn't) |
| | `warden doctor` | Environment problems (kernel settings, limits, ports, permissions), each with its fix |

### Moving from PM2

```sh
warden pm2-migrate --dry-run                   # what it would write, from `pm2 jlist`
warden pm2-migrate                             # <app>.toml + <app>.env (0600) + MIGRATION.md
warden pm2-migrate --cutover same-port         # PM2 stops each app, Warden starts it; PM2 is restored on failure
warden pm2-migrate --finalize                  # remove the migrated apps from PM2, `warden save`
```

It reads the running daemon (`--from dump` for `pm2 save`'s file, or an
ecosystem file with `--env production`). Env values go to a 0600 file, never
into the config, and only the variables the app was given: `pm2 jlist` also
carries the whole shell of whoever ran `pm2 start`, which is left out and
listed by name in `MIGRATION.md` for review. PM2's defaults carry over where
apps depend on them (SIGINT to stop, `NODE_APP_INSTANCE`). `--cutover
overlap` runs both side by side first (apps that share their port with
`reusePort`); `new-port:<p>` starts Warden on another port for you to switch
the proxy.

Every problem Warden logs says what happened, why, what it did and how to fix
it; [`docs/troubleshooting.md`](docs/troubleshooting.md) collects them by symptom.

```
$ warden status travelerwe-api
 travelerwe-api
┌───────────┬──────────────────────────────────────────────┐
│ status    │ online                                       │
│ id        │ 0                                            │
│ namespace │ default                                      │
│ mode      │ process                                      │
│ workers   │ 4 configured, 4 ready                        │
│ pid       │ 8139                                         │
│ uptime    │ 27s                                          │
│ release   │ /srv/apps/travelerwe/api/releases/2026-09-30 │
│ memory    │ 4.0 MB (supervisor)                          │
└───────────┴──────────────────────────────────────────────┘
 Workers
┌────────┬─────────┬──────┬────────┬───┬──────┬─────────┬──────────┬────────┬───────────┐
│ worker │ status  │ pid  │ uptime │ ↺ │ cpu  │ mem     │ loop p99 │ health │ last exit │
├────────┼─────────┼──────┼────────┼───┼──────┼─────────┼──────────┼────────┼───────────┤
│ 1      │ RUNNING │ 8172 │ 25s    │ 1 │ 0.0% │ 40.1 MB │ 0.21ms   │ ok     │ -         │
│ 2      │ RUNNING │ 8180 │ 13s    │ 1 │ 0.0% │ 40.1 MB │ 0.18ms   │ ok     │ -         │
│ 3      │ RUNNING │ 8188 │ 10s    │ 1 │ 0.0% │ 40.2 MB │ 0.20ms   │ ok     │ -         │
│ 4      │ RUNNING │ 8196 │ 8s     │ 1 │ 0.0% │ 40.2 MB │ 0.19ms   │ ok     │ -         │
└────────┴─────────┴──────┴────────┴───┴──────┴─────────┴──────────┴────────┴───────────┘
```

The CLI talks to the running supervisor over a Unix socket with mode 0600.
That socket's directory also holds the shim every worker preloads and the
per-worker health sockets. Warden refuses to use the directory unless it
owns it, it is not a symlink, and no other user can write to it.

## Static files

`warden serve dist 8080` (or a `[static]` section, see
`warden.example.toml`) runs Warden's own file server as the app's workers:
supervised, health-checked and reloaded like any app. It speaks HTTP/1.1
with keep-alive: ETag / Last-Modified and 304s, single ranges,
precompressed `.br` / `.gz` siblings, SPA fallback, `404.html`, Basic auth.
Paths can't leave the root (`..`, symlinks out, NUL), and files are opened
with `openat2(RESOLVE_BENEATH)` on Linux.

Small files are served from memory. Each worker keeps complete responses
(headers and body) of recently used files up to `cache_max_file` (64 KB), in
at most `cache_size` (16 MB, least recently used out first; `0` turns it
off), so a hit is one system call: a `send(2)` from memory, or, for a body
of 8 KB or more, a `sendfile(2)` from a sealed in-memory file (memfd), where
the kernel takes the pages by reference instead of copying them (48 KB
script: 28 % less CPU per request, ahead of nginx; see
[docs/benchmarks.md](docs/benchmarks.md)). HEAD and 304s come from the same entry;
ranges and anything unusual take the normal path. A cached file is checked
against the disk at most every `cache_valid_ms` (1 s): an edit, a deletion
or a symlink swapped in shows within that time (on NFS, within the
attribute cache time). A file changed in the last 2 s is served but not
cached. A deploy that swaps a `current` symlink needs a rolling restart
anyway (each worker resolves the root once), which starts with an empty
cache. With `access_log = true` each line ends in `cache=hit` or
`cache=miss`, and each worker prints its cache counters when it stops.

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
- nginx: [`contrib/nginx.conf`](contrib/nginx.conf), a commented site file:
  one upstream address (the kernel does the balancing) with keep-alive,
  retries for idempotent requests only, WebSockets and SSE, X-Forwarded-*
  headers, a `/health` for a load balancer. A test restarts the workers
  under load through it without a failed request. Load balancers (AWS
  ALB/NLB, GCP, Cloudflare), no proxy at all, and the timeouts that must
  agree with Warden's drain: [`docs/proxies.md`](docs/proxies.md).
- Logs go to stdout in journald format (timestamps dropped, priority prefixes
  added). Use `journalctl -u warden@api`.
- Metrics: set `[metrics] listen = "127.0.0.1:9464"` to get Prometheus text at `/metrics`.
- `sudo warden startup` installs the unit (with this binary's path, running
  as root like `sudo warden start`), the sysctl file and
  [`contrib/wardend.service`](contrib/wardend.service) for you: see the next
  section.

## Surviving reboots and crashes

`warden save` records which apps run, with their worker counts. `warden
startup` makes them come back after a reboot, and keeps them up after a
crash, with the service manager the host has:

| Host | `warden startup` installs | After a reboot | After a crash |
|---|---|---|---|
| Linux, root (`sudo warden startup`) | `warden@.service` (enabled for every saved app), `wardend.service`, the sysctl file | systemd starts each app's unit | systemd restarts the supervisor |
| Linux, a user (`warden startup`, or `--user` as root) | the same two units in `~/.config/systemd/user`, and `loginctl enable-linger` so they run without a login | your user manager starts at boot and starts the units | your user manager restarts the supervisor |
| macOS | a launchd job: `~/Library/LaunchAgents/io.github.oceanwap.warden.daemon.plist` (as root `/Library/LaunchDaemons/`: at boot, no login needed) running `warden daemon --resurrect` | launchd starts wardend, which starts the saved apps | wardend restarts dead supervisors; launchd restarts wardend |
| Containers, other init systems | nothing (it says what to run instead) | `warden resurrect` in the entrypoint, or `warden daemon --resurrect` as the entrypoint | with `warden daemon --resurrect`, wardend restarts dead supervisors |

- Under systemd each app is its own unit (cgroup, limits, `journalctl -u
  warden@api`), and wardend never starts apps there: it would start them
  twice. `wardend.service` adds `warden events` and restarts supervisors
  that `warden start` launched outside a unit.
- The apps come back as the user that ran them: system units run as root,
  like the `sudo warden start` that started the apps (with root's saved
  worker counts and runtime directory, so `warden list` and wardend find
  them). For another user, `systemctl edit warden@<app>` and set `User=`
  and `Group=`; the app's files must then be theirs.
- The unit reads `<config dir>/<app>.toml` (`/etc/warden` for root,
  `~/.config/warden` for a user); `startup` says how to link a config that
  lives elsewhere.
- User units and the launchd job carry your `PATH`, so `bun` and `node`
  resolve, and Warden's directory variables (`WARDEN_HOME`, ...).
- Lingering needs root on some systems: `startup` then prints the exact
  `sudo loginctl enable-linger <you>`. Until then the apps start when you log
  in and stop when you log out.
- macOS LaunchAgents start at login to the desktop. On a Mac you only reach
  over SSH, use `sudo warden startup` (a LaunchDaemon).
- `warden unstartup` removes all of it; running apps keep running. Under
  systemd, `warden@.service` stays while apps still run under it (a running
  unit whose file is removed is left half configured), disabled: `warden
  kill`, then `warden unstartup` again removes it. `warden kill` stops
  wardend's unit or job too, so it stays down until the next boot.
- CI checks all of this against real systemd (system and user units, a
  restart of the user manager) and launchd (LaunchAgent, LaunchDaemon):
  `.github/workflows/service-managers.yml`.
- In a container, run `warden daemon --resurrect` under an init
  (`docker run --init`, tini) that reaps orphaned processes.
- `--resurrect` runs once per boot: when launchd restarts a crashed wardend,
  apps you stopped since boot stay stopped (`warden resurrect` starts them).

### wardend: one socket for every app

`warden daemon` (started by `warden start`, or by the units above) watches
every supervisor on the host and pushes what happens to `warden events` and
the [GUI](#gui):

```
$ warden events
12:00:01 wardend pid=4211 version=0.1.0 (Ctrl-C to stop)
12:00:01 api running (supervised by wardend, pid 4208)
12:00:01 api 4/4 workers ready
12:00:09 api worker 2 crashed pid=4230 exit code 3
12:00:09 api worker 2 restarting in_ms=0
12:00:09 api worker 2 starting pid=4262
12:00:09 api worker 2 ready pid=4262 startup_ms=31
```

A supervisor that dies (`kill -9`, OOM) is started again with backoff when
nothing else would (systemd restarts its own units; one run in a terminal is
yours); a hung one is reported, never killed, because its workers are still
serving. The protocol, for scripts and other clients:
[`docs/protocol.md`](docs/protocol.md).

**Alerts.** Put rules in `<config dir>/wardend.toml` (`/etc/warden` for
root, `~/.config/warden` for a user) and wardend tells a command or a
webhook when something goes wrong:

```toml
[[alert]]
on = ["crash_loop", "gave_up", "rollout_failed", "unresponsive", "worker_failed", "oom"]   # or ["all"]
apps = ["api"]                                    # optional; default every app
webhook = "https://hooks.slack.com/services/…"    # POSTed as JSON, through curl
min_interval = "5m"                               # repeats within it are counted and sent as one

[[alert]]
on = ["all"]
command = ["/usr/local/bin/notify", "--channel", "ops"]   # the alert as JSON on stdin
```

The kinds also include `died`, `unhealthy`, `recycled` and `recovered`
(healthy again after an alert). `warden daemon check` validates the file
with every problem and its line; `warden daemon reload` (or SIGHUP) applies
it, and a broken file keeps the rules in force. Deliveries never hold
wardend up: a bounded queue, 10 s per try, one retry. Webhooks go through
`curl` (Warden has no TLS stack of its own), with the URL on curl's stdin,
never in a process list or a log line. Details:
[Alerts](docs/protocol.md#alerts).

**History.** wardend keeps the last 24 hours of every app's CPU, memory,
workers ready and restarts, and the host's CPU, memory and load, from the
statuses it already receives (a sample per 10 s; at most 135 KiB per app).
It saves them every minute and when it stops (`<state dir>/wardend-history.bin`,
written atomically), so a restart of wardend (an upgrade, a crash, a
reboot) keeps the charts; a damaged file is moved aside with a warning.
The GUI charts them; scripts ask `{"cmd":"history"}`
([Resource history](docs/protocol.md#resource-history)).

## GUI

`warden-gui` is a native window (Rust, [iced](https://iced.rs)) on wardend:
every app with its state, workers, CPU and memory, pushed live; each app's
workers, rollout progress, events, logs and charts of its last 1, 6 or 24
hours (CPU, memory, restarts, workers ready), with the host's CPU and memory
as sparklines in the header; and the CLI's actions (reload,
safe reload, rolling or hard restart, restart one worker, scale, stop,
start, reset), adding an app (`warden start …`) and editing its config
(checked with `warden check` before it is saved). It is a separate process:
closing or killing it touches nothing, and apps never depend on it.

```sh
warden-gui                                   # this machine's wardend (`Start wardend` if it is not running)
warden-gui --ssh deploy@web-1                # a remote host, through an SSH tunnel (your agent and keys)
```

It idles at about 21 MB resident and 0.1–0.2% CPU with 10 apps (21.5 MB with
the History tab showing a full day). Details, the SSH
setup and the measurements: [`gui/README.md`](gui/README.md).

## Benchmarks

The same apps under PM2, Platformatic Watt, nginx, `serve` and Warden, on one
machine. Everything in the tables below is produced by one command, and anyone
can re-run it:

```sh
cargo xtask bench            # all suites (~25 min on 2 CPUs); rewrites the tables below
cargo xtask bench --quick    # a ~10-minute smoke test
```

What each suite does, what each number means and the fairness rules are in
[`bench/README.md`](bench/README.md); findings, caveats and the before/after
log of every optimisation are in [`docs/benchmarks.md`](docs/benchmarks.md).
The machine is small (2 CPUs shared with the load generator), so compare
columns, not absolute numbers. The same suites also run on GitHub's hosted
x86_64 and ARM64 runners (`.github/workflows/bench.yml`: push to the
`bench` branch or run it by hand), each suite's table an annotation on the
run.

In short (this run, against PM2 6.0.14):

- **Requests:** Warden adds nothing; node:http 73.5k req/s (bare 66.9k, PM2
  55.3k), p99 2.4 ms (PM2 8.5 ms).
- **Crash recovery:** 74-719 ms, or 25-54 ms with a hot standby (PM2
  162-1,554 ms).
- **Rolling restarts:** no failed request in any run; PM2 lost 2,093 of
  6,571 for NestJS on Bun. WebSockets and SSE end cleanly (0 abnormal
  closes; PM2 cut every one). With 100 long-lived connections open, all 4
  workers are replaced in 2.3-2.5 s.
- **Static files:** faster than nginx on small files (102k vs 93k req/s; 78k
  vs 77k on a 48 KB script; 6.0 vs 5.0 GB/s on a large one) with less memory
  (10 vs 13 MB PSS after load).
- **Logs:** a tenth of PM2's CPU per GB; `worker_output = "direct"` keeps
  every line at 851 MB/s (PM2 177 MB/s).
- **Manager:** about 1 MB PSS per app (wardend included), `list` in 2.6 ms
  (PM2 156 ms).

<!-- bench:start -->
<!-- Generated by `cargo xtask bench`; edit xtask/src/main.rs or bench/*.ts, not this text. -->

Measured 2026-10-01 on 2 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 8 GB RAM, Linux 6.18.44-fc-v50 x86_64. The load generator (oha, 64 connections) runs on the same CPUs as the apps, so compare columns with each other, not with other machines. Raw numbers: `bench/results/latest/`.

> Note: net.ipv4.tcp_migrate_req is 0: a rolling restart can reset a connection that was waiting in a closing worker's accept queue, for every manager. `sysctl -w net.ipv4.tcp_migrate_req=1` (what `warden startup` configures) makes it 0.

### Node.js app (node:http)

The same app under each manager: 4 workers, one port. PM2 in cluster mode, Watt with 4 worker threads, Warden with 4 processes.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | shim | pm2 | watt | warden-process | warden-surge | warden-standby |
|---|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 226.3 / 95.9 | 230.2 / 98.9 | 351.1 / 155.1 | 487.7 / 478.1 | 239.6 / 106.3 | 244.5 / 111.1 | 294.7 / 119.3 |
| total RAM after load: RSS / PSS (MB) | 280.9 / 134 | 291.3 / 143.8 | 378.4 / 181.8 | 631.3 / 621.8 | 302.1 / 152.4 | - | 350.2 / 158.5 |
| manager RAM (MB) | 0 | 0 | 66 | (in total) | 9.3 | 8.3 | 8.4 |
| manager idle CPU (%) | 0 | 0 | 0.2 | 2.8 | 0.2 | - | 0.2 |
| startup to 4 serving (ms) | 173 | 200 | 1003 | 3252 | 204 | 253 | 210 |
| crash recovery (ms) | n/a (nothing restarts it) | n/a (nothing restarts it) | 320 | 1357 | 140 | - | 48 |
| requests failed during a crash | n/a | n/a | 2 of 2581 | 2 of 6735 | 0 of 4246 | - | 8 of 3585 |
| rolling restart under load: failed | n/a | n/a | 0 of 2048 (798 ms) | 0 of 3250 (3277 ms) | 0 of 3686 (302 ms) | 0 of 2585 (301 ms) | 0 of 3859 (298 ms) |
| status command (ms) | n/a | n/a | 149.4 | 424.8 | 1.7 | - | 1.6 |
| /plaintext req/s | 66868 | 69817 | 55278 | 50152 | 73502 | - | 72860 |
| /plaintext p50 / p99 (ms) | 0.85 / 3.83 | 0.87 / 2.73 | 0.47 / 8.45 | 1.06 / 5.32 | 0.83 / 2.36 | - | 0.83 / 2.5 |
| /plaintext CPU per request (µs) | 18.57 | 18.1 | 20.6 | 27.74 | 17.11 | - | 17.28 |
| /plaintext errors | 0 | 0 | 0 | 0 | 0 | - | 0 |
| /json req/s | 68653 | 69890 | 58760 | 50045 | 71591 | - | 69452 |
| /json p50 / p99 (ms) | 0.84 / 3.11 | 0.85 / 2.94 | 0.58 / 8.1 | 0.94 / 6.35 | 0.82 / 2.8 | - | 0.88 / 2.6 |
| /json CPU per request (µs) | 18.34 | 17.93 | 19.48 | 27.65 | 17.57 | - | 18.11 |
| /json errors | 0 | 0 | 0 | 0 | 0 | - | 0 |
| /cpu req/s | 876 | 903 | 876 | 898 | 918 | - | 885 |
| /cpu p50 / p99 (ms) | 71.24 / 143.12 | 71.33 / 139.88 | 70.95 / 136.5 | 64.97 / 142.21 | 66.59 / 126.74 | - | 66.6 / 135.09 |
| /cpu CPU per request (µs) | 2249.19 | 2181.71 | 2243.53 | 2198.36 | 2148.93 | - | 2227.33 |
| /cpu errors | 0 | 0 | 0 | 0 | 0 | - | 0 |

### NestJS app on Node.js

A minimal NestJS (Express) app with the same endpoints, 4 workers.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | watt | warden-process | warden-surge | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 420.2 / 255.4 | 487.8 / 274.2 | 585.1 / 575.5 | 422.1 / 255.1 | 426.2 / 259.1 | 523.9 / 302.8 |
| total RAM after load: RSS / PSS (MB) | 700 / 534.8 | 810.5 / 596.6 | 1131.3 / 1121.7 | 716.4 / 548.7 | - | 791.2 / 569.6 |
| manager RAM (MB) | 0 | 67.6 | (in total) | 7.7 | 7.8 | 7.8 |
| manager idle CPU (%) | 0 | 0.2 | 2.2 | 0 | - | 0 |
| startup to 4 serving (ms) | 1073 | 1862 | 4071 | 1428 | 1188 | 1195 |
| crash recovery (ms) | n/a (nothing restarts it) | 1554 | 1901 | 719 | - | 54 |
| requests failed during a crash | n/a | 2 of 3254 | 0 of 4039 | 7 of 2732 | - | 7 of 1327 |
| rolling restart under load: failed | n/a | 0 of 1211 (1622 ms) | 0 of 1928 (4781 ms) | 0 of 1830 (1566 ms) | 0 of 1751 (1158 ms) | 0 of 1391 (1868 ms) |
| status command (ms) | n/a | 144.2 | 408.4 | 1.8 | - | 1.7 |
| /plaintext req/s | 10585 | 9143 | 8675 | 10251 | - | 11782 |
| /plaintext p50 / p99 (ms) | 5.04 / 23.65 | 5.7 / 24.61 | 6.35 / 29.85 | 5.22 / 21.23 | - | 4.63 / 20.64 |
| /plaintext CPU per request (µs) | 161.49 | 186.19 | 200.12 | 155.27 | - | 144.78 |
| /plaintext errors | 0 | 0 | 0 | 0 | - | 0 |
| /json req/s | 10834 | 9570 | 10228 | 11013 | - | 10224 |
| /json p50 / p99 (ms) | 5.39 / 18.97 | 5.98 / 21.34 | 5.69 / 23.89 | 4.84 / 22.44 | - | 5.7 / 20.31 |
| /json CPU per request (µs) | 156.76 | 176.48 | 167.32 | 152.87 | - | 166.06 |
| /json errors | 0 | 0 | 0 | 0 | - | 0 |
| /cpu req/s | 812 | 801 | 794 | 819 | - | 847 |
| /cpu p50 / p99 (ms) | 83.02 / 146.12 | 78.56 / 111.41 | 70.29 / 136.36 | 78.6 / 137.1 | - | 74.87 / 135.62 |
| /cpu CPU per request (µs) | 2420.62 | 2452.89 | 2478.74 | 2387.97 | - | 2318.01 |
| /cpu errors | 0 | 0 | 0 | 0 | - | 0 |

### Bun app (Bun.serve)

4 workers. PM2 has no cluster mode for Bun, so it runs 4 fork-mode instances sharing the port with reusePort. Watt does not run Bun apps.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | shim | pm2 | warden-process | warden-surge | warden-worker | warden-standby |
|---|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 152.9 / 53.3 | 169.2 / 64.3 | 288.7 / 163.5 | 178 / 70.9 | 179.7 / 72.5 | 77.3 / 59 | 219.8 / 81.6 |
| total RAM after load: RSS / PSS (MB) | 174.5 / 58 | 192.8 / 70.7 | 292.5 / 158.6 | 199.9 / 76.4 | - | 82.4 / 61.5 | 242.9 / 88.1 |
| manager RAM (MB) | 0 | 0 | 65.6 | 8.5 | 10.7 | 8.7 | 8.8 |
| manager idle CPU (%) | 0 | 0 | 0 | 0 | - | 0 | 0 |
| startup to 4 serving (ms) | 45 | 67 | 645 | 105 | 88 | 109 | 81 |
| crash recovery (ms) | n/a (nothing restarts it) | n/a (nothing restarts it) | 162 | 74 | - | 153 | 25 |
| requests failed during a crash | n/a | n/a | 7 of 11597 | 1 of 12199 | - | 1 of 14463 | 8 of 11070 |
| rolling restart under load: failed | n/a | n/a | 0 of 13222 (564 ms) | 0 of 10012 (268 ms) | 0 of 11103 (271 ms) | 0 of 12706 (260 ms) | 0 of 11137 (272 ms) |
| status command (ms) | n/a | n/a | 146.6 | 1.6 | - | 1.6 | 1.8 |
| /plaintext req/s | 103845 | 106561 | 88416 | 104282 | - | 103181 | 98890 |
| /plaintext p50 / p99 (ms) | 0.38 / 3.85 | 0.37 / 3.65 | 0.59 / 4.17 | 0.41 / 3.67 | - | 0.37 / 4.37 | 0.37 / 4.5 |
| /plaintext CPU per request (µs) | 9.91 | 9.72 | 12.86 | 9.95 | - | 10.05 | 10.43 |
| /plaintext errors | 0 | 0 | 0 | 0 | - | 0 | 0 |
| /json req/s | 93376 | 88091 | 79910 | 91179 | - | 89322 | 96802 |
| /json p50 / p99 (ms) | 0.5 / 3.71 | 0.58 / 3.52 | 0.74 / 2.61 | 0.53 / 3.45 | - | 0.53 / 4.07 | 0.53 / 3.13 |
| /json CPU per request (µs) | 12.16 | 12.97 | 15.26 | 12.56 | - | 12.83 | 11.7 |
| /json errors | 0 | 0 | 0 | 0 | - | 0 | 0 |
| /cpu req/s | 1336 | 1325 | 1322 | 1340 | - | 1368 | 1396 |
| /cpu p50 / p99 (ms) | 46.46 / 92.32 | 45.79 / 89.71 | 48.69 / 75.08 | 46.86 / 95.09 | - | 47.92 / 101.37 | 50.72 / 103.13 |
| /cpu CPU per request (µs) | 1468.74 | 1475.87 | 1485.5 | 1462.72 | - | 1428.63 | 1408.1 |
| /cpu errors | 0 | 0 | 0 | 0 | - | 0 | 0 |

### NestJS app on Bun

The NestJS app on Bun, 4 workers; Warden also in worker (thread) mode.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | warden-process | warden-surge | warden-worker | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 339 / 197.7 | 421.6 / 276 | 348.6 / 206.5 | 350.4 / 206.6 | 188.6 / 167 | 436.3 / 250.1 |
| total RAM after load: RSS / PSS (MB) | 435.5 / 292 | 512.7 / 365.2 | 432.4 / 286.3 | - | 278.9 / 257.1 | 539.8 / 351.3 |
| manager RAM (MB) | 0 | 64.8 | 7.9 | 9.9 | 8.3 | 9.7 |
| manager idle CPU (%) | 0 | 0 | 0.2 | - | 0 | 0 |
| startup to 4 serving (ms) | 729 | 1220 | 813 | 872 | 872 | 919 |
| crash recovery (ms) | n/a (nothing restarts it) | 673 | 467 | - | 1094 | 52 |
| requests failed during a crash | n/a | 8 of 6336 | 5 of 4640 | - | 0 of 3906 | 3 of 2727 |
| rolling restart under load: failed | n/a | 2093 of 6571 (1359 ms) | 0 of 3582 (1310 ms) | 0 of 1849 (1108 ms) | 0 of 2949 (1069 ms) | 0 of 3691 (1359 ms) |
| status command (ms) | n/a | 142 | 1.7 | - | 2.1 | 1.7 |
| /plaintext req/s | 30169 | 32174 | 38060 | - | 32189 | 35693 |
| /plaintext p50 / p99 (ms) | 1.41 / 10.59 | 1.36 / 12.45 | 0.93 / 9.57 | - | 1.06 / 17.02 | 1.27 / 9.28 |
| /plaintext CPU per request (µs) | 47.77 | 48.43 | 38.29 | - | 43.62 | 40.32 |
| /plaintext errors | 0 | 0 | 0 | - | 0 | 0 |
| /json req/s | 28378 | 33303 | 32923 | - | 28435 | 32241 |
| /json p50 / p99 (ms) | 1.42 / 11.62 | 1.47 / 8.89 | 1.42 / 9.62 | - | 1.27 / 15.53 | 1.24 / 10.94 |
| /json CPU per request (µs) | 51.04 | 46.25 | 42.95 | - | 47.66 | 41.96 |
| /json errors | 0 | 0 | 0 | - | 0 | 0 |
| /cpu req/s | 1268 | 1197 | 1268 | - | 1090 | 1299 |
| /cpu p50 / p99 (ms) | 47.25 / 95.19 | 48.57 / 100.06 | 50.63 / 106.35 | - | 53.48 / 128.13 | 48.63 / 94.28 |
| /cpu CPU per request (µs) | 1243.87 | 1619.19 | 1524.81 | - | 1546.7 | 1507.64 |
| /cpu errors | 0 | 0 | 0 | - | 0 | 0 |

### Static files

`warden serve` against nginx, `pm2 serve` and the `serve` package, 4 workers each (serve has no cluster mode).

4 workers · nginx/1.24.0 (Ubuntu) · pm2 6.0.14 · serve 14.2.6 · node v22.22.2 · warden 0.1.0

| | warden | warden-nocache | nginx | pm2-serve | serve (1 process) |
|---|---|---|---|---|---|
| processes | 5 | 5 | 5 | 5 | 1 |
| total RAM idle: RSS / PSS (MB) | 28.8 / 8 | 30.7 / 9.6 | 24.3 / 13 | 320.3 / 135.8 | 101.1 / 99 |
| total RAM after load: RSS / PSS (MB) | 31.1 / 10.2 | 33 / 11.8 | 25.8 / 13.2 | 592.9 / 403.9 | 190.4 / 188.2 |
| startup (ms) | 18 | 11 | 14 | 674 | 377 |
| /index.html req/s | 101894 | 77774 | 93387 | 14414 | 18095 |
| /index.html p50 / p99 (ms) | 0.42 / 3.85 | 0.6 / 4.69 | 0.4 / 5.37 | 3.45 / 20.32 | 3.47 / 5.88 |
| /index.html server CPU per request (µs) | 9.01 | 13.9 | 11.32 | 122.66 | 0 |
| /index.html errors | 0 | 0 | 0 | 0 | 0 |
| /assets/app.3f9a2c1b.js req/s | 77934 | 72546 | 76847 | 12291 | 1845 |
| /assets/app.3f9a2c1b.js p50 / p99 (ms) | 0.65 / 4.22 | 0.61 / 6.47 | 0.68 / 4.04 | 3.7 / 30.34 | 33.68 / 57.26 |
| /assets/app.3f9a2c1b.js server CPU per request (µs) | 11.39 | 14.09 | 12.61 | 139.41 | 857.35 |
| /assets/app.3f9a2c1b.js errors | 0 | 0 | 0 | 0 | 0 |
| /media/video.bin req/s | 5991 | 5839 | 4972 | 2481 | 1222 |
| /media/video.bin p50 / p99 (ms) | 10.52 / 22.93 | 10.76 / 23.8 | 12.3 / 26.96 | 22.01 / 82.3 | 49.47 / 82.78 |
| /media/video.bin server CPU per request (µs) | 202.07 | 208.47 | 266.57 | 621.35 | 1027.99 |
| /media/video.bin MB/s | 5985.1 | 5834.8 | 4965.6 | 2474.4 | 1215.5 |
| /media/video.bin errors | 0 | 0 | 0 | 0 | 0 |
| /index.html (new connection each) req/s | 32547 | 28227 | 29666 | 4511 | 9840 |
| /index.html (new connection each) p50 / p99 (ms) | 1.89 / 4.59 | 2.08 / 6.86 | 2.11 / 4.96 | 13.3 / 36.36 | 6.23 / 11.49 |
| /index.html (new connection each) server CPU per request (µs) | 22.71 | 28.83 | 26.97 | 365.75 | 0 |
| /index.html (new connection each) errors | 0 | 0 | 0 | 0 | 0 |

### Log-heavy apps

What capturing worker output costs the manager. Both write the app's stdout to a log file; warden-direct splices it there unparsed (worker_output = "direct").

node v22.22.2 · pm2 6.0.14 · warden 0.1.0

Steady: 4 workers × 5000 lines/s × 10 s

| | warden | warden-direct | pm2 |
|---|---|---|---|
| lines written / in the log file | 200000 / 200000 | 200000 / 200000 | 200000 / 200000 |
| manager CPU (s) | 0.3 | 0.11 | 1.27 |
| manager peak RAM (MB) | 7.1 | 6.6 | 74.6 |

Flood: 1 worker writing 200 MB to stdout as fast as it is read

| | warden | warden-direct | pm2 | warden (keep all) |
|---|---|---|---|---|
| time to write it (ms) | 247 | 235 | 1130 | 528 |
| throughput (MB/s) | 809.7 | 851.1 | 177 | 378.8 |
| manager CPU (s) | 0.13 | 0.17 | 1.4 | 0.66 |
| manager CPU per GB (s) | 0.67 | 0.87 | 7.17 | 3.38 |
| manager peak RAM (MB) | 6.9 | 6.3 | 89.9 | 7.1 |
| lines in the log file | 10000 | 2076388 | 2076388 | 2076388 |

### Many apps on one host

10 apps (one idle Node process each): the manager's memory and CPU, and how fast everyday commands answer. Warden's managers are one supervisor per app plus wardend.

10 apps, one idle Node process each · node v22.22.2 · pm2 6.0.14 · warden 0.1.0

| | warden | pm2 |
|---|---|---|
| manager processes | 11 | 1 |
| manager RAM, all apps: RSS (MB) | 72.6 | 80.2 |
| manager RAM, all apps: PSS (MB) | 11.9 | 40 |
| manager idle CPU (%) | 0 | 0.7 |
| start 10 apps (ms) | 411 | 1954 |
| `list` (ms) | 2.6 | 156.4 |
| `list --json` (ms) | 2.4 | 139.2 |
| `describe app3` (ms) | 1.9 | 142.2 |
| `logs app3 --nostream` (ms) | 2 | 166 |

### WebSockets and SSE through a rolling restart

50 WebSocket and 50 SSE clients stay connected while all 4 workers are replaced (`pm2 reload`: cluster mode for Node, fork mode for Bun; `warden restart` with the default `long_lived_timeout` of 2 s). Each client reconnects at once. Clean: a WebSocket close frame or the SSE stream's last chunk; abnormal: cut without one (a browser reports 1006, EventSource an error).

4 workers · 50 WebSocket + 50 SSE clients · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · warden 0.1.0

| | Node: pm2 (cluster) | Node: warden | Bun: pm2 (fork) | Bun: warden | Bun: warden (worker mode) |
|---|---|---|---|---|---|
| rolling restart, until all new workers answer (ms) | 800 | 2528 | 596 | 2272 | 2272 |
| until every client is back on a new worker (ms) | not all within 90 s (98/100) | 2330 | 417 | 2163 | 2152 |
| WebSocket closes: clean / abnormal | 0 / 69 | 50 / 0 | 0 / 79 | 50 / 0 | 50 / 0 |
| WebSocket close codes | 69× 1006 (no close frame) | 50× 1001 | 79× 1006 (no close frame) | 50× 1001 | 50× 1001 |
| SSE ends: clean / abnormal | 0 / 65 | 50 / 0 | 0 / 76 | 50 / 0 | 50 / 0 |
| SSE ends | 65× cut (no last chunk) | 50× end of stream | 76× cut (no last chunk) | 50× end of stream | 50× end of stream |
| failed reconnects (WebSocket + SSE) | 0 | 0 | 202 | 0 | 0 |
| plain requests during the restart: failed / sent | 0 / 109 | 0 / 410 | 5 / 82 | 0 / 399 | 0 / 389 |
<!-- bench:end -->

## Platforms

- **Linux x86_64 and ARM64**: supported for production. CI runs the unit
  and integration tests on both (and formatting and clippy on x86_64).
- **macOS**: for development only, to run an app locally with the same
  config. CI builds it and runs clippy and the unit tests (Apple Silicon).
  What it lacks:
  - `SO_REUSEPORT` does not load-balance on macOS: several workers can
    share the port, but connections are not spread across them. Use
    `[workers] count = 1` locally.
  - Node has no `reusePort` on macOS at all (libuv offers it only where the
    kernel balances), so only one Node worker can listen on the port, and a
    reload, which starts the new worker next to the old one, fails: use
    `warden restart --hard`, or `port_strategy = "offset"`. Warden says so
    at start. Bun apps share the port as on Linux.
  - No parent-death signal: workers outlive a supervisor killed with SIGKILL.
  - `warden serve` checks static paths with realpath instead of `openat2`
    (same confinement, slower).
  - No out-of-memory attribution: a worker the kernel killed for memory
    shows as `killed by SIGKILL`, not as an OOM kill (macOS has no cgroup
    events to read).
  - CPU, memory, the user, listening ports, a process's environment and
    the host's numbers come from macOS's own interfaces (libproc, `sysctl`
    and Mach calls) through a small adapter per OS (`src/platform/`), so
    the CPU and mem columns, readiness by port, wardend's `host` events and
    its restart of a supervisor with the environment it was started with
    all work as on Linux. `warden doctor` says what the OS you are on
    cannot do.
- **Windows**: not supported, and not planned for 0.1. Warden is built on
  Unix sockets, signals, process groups and `SO_REUSEPORT`, none of which
  Windows has in the same form. **WSL2 works as Linux** (it is a real Linux
  kernel): follow the Linux instructions inside the distribution, and keep
  the app on the Linux filesystem. [docs/windows.md](docs/windows.md) has the
  tips and the plan for a native build, with the one hard problem (sharing a
  port between workers) spelled out.

## Development

**Toolchain.** CI builds with Rust `1.95.0` (pinned in the workflows, because a
new release brings new clippy lints and `-D warnings` turns them into
failures; bump it on purpose). The crates declare the lowest versions they are
meant to build with: `warden` and `warden-protocol` 1.85, `warden-gui` 1.88
(iced 0.14). Newer stable toolchains work, but only the pinned one is tested.

```sh
rustup toolchain install 1.95.0
cargo test                  # unit + integration tests (integration tests need `bun` and `node` on PATH)
cargo clippy --all-targets
cargo test --workspace --bins --tests          # also protocol/ and gui/ (the GUI's own tests)
cargo clippy --workspace --all-targets
cargo run -p warden-gui     # the GUI
cargo xtask bench           # benchmarks (see above); `cargo xtask bench --help`
cargo xtask chaos           # chaos soak: a fleet under load, random faults, invariants checked (docs/chaos.md)
cargo release 0.2.0         # a release (see Releasing below); `cargo xtask release --help`
cargo dist-macos            # the macOS release archives, built on this Mac (see Releasing below)
scripts/mac-check.sh        # on a Mac: builds, runs the platform tests and drives a real supervisor and
                            # wardend (CPU, memory, user, ports, environment, host events, static latency); the report
                            # is target/mac-check/report.txt
```

The workspace: `warden` (this directory), `protocol/` (the wire types, serde
only, shared by `warden` and the GUI), `gui/` (`warden-gui`) and `xtask/`.
Plain `cargo build` and `cargo test` here mean `warden` only.

All `unsafe` code is in [`src/sys.rs`](src/sys.rs) and its macOS half
[`src/sys/darwin.rs`](src/sys/darwin.rs) (`protocol/` and `gui/` have none:
`#![forbid(unsafe_code)]`): system calls the standard library doesn't expose,
and the few that measurably pay on a hot path (the static server and log
capture), each with a SAFETY note and tests. The rest of the crate is
`#![deny(unsafe_code)]`.

What differs per OS (reading a process's memory, owner, ports and
environment, the host's load, the boot id) sits behind one trait with an
implementation per OS in [`src/platform/`](src/platform/): `linux.rs` reads
`/proc`, `macos.rs` calls libproc and `sysctl`, `other.rs` answers nothing.
The adapter is chosen when Warden is built; the same contract tests run
against the real OS on Linux (CI) and on a Mac.

Layout: `src/supervisor.rs` (event loop), `src/supervisor/rollout.rs` (gates,
canary, rollback), `src/supervisor/upkeep.rs` (watchdog, recycling),
`src/process.rs` (spawning, fd-3 IPC), `src/platform/` (the per-OS adapters),
`shim/` (embedded JS), `tests/`, `bench/`, `research/`.

Limitations:

- Linux is authoritative. macOS works for development; see
  [Platforms](#platforms) for what it lacks.
- Worker mode needs Bun and the shim.
- Node apps share the port through Warden's shim (`--import`, Node ≥ 22.12
  for `reusePort`); older Node needs `port_strategy = "offset"`.
- Outside a drain the shim adds no work to a request: a `Bun.serve` app's
  own fetch handler answers it, and on Node it keeps one entry per open
  connection, nothing per request. When a drain starts, each `Bun.serve`
  server gets handlers that add `Connection: close` through
  `server.reload()`: `fetch`, every function in `routes` (a static
  `Response` there becomes a function answering with a copy of it) and
  `error`; the shim intercepts `reload()` on Bun's server prototype, so an
  app's own `server.reload()` still works (and a drain never brings back a
  handler the app replaced). A Node `https` server is tracked on its TLS
  connections. Known gap: `http2` servers are not tracked.
- The shim reports to Warden over a blocking socket. A supervisor that stops
  reading (SIGSTOPped, or hung) for several minutes fills it with
  heartbeats, and the workers then block writing the next one; a dead
  supervisor is not the same (workers are signalled and carry on or exit).
  `[watchdog]` on the supervisor itself (systemd's `WatchdogSec=`) is what
  covers a hung supervisor.
- On Bun, the shim wraps `Response` and `ReadableStream`, so it can end SSE
  bodies in a drain (one call frame per `new Response`, which JSC inlines:
  no measurable difference, `bench/shim-cost.ts`). The wrappers pass for
  Bun's own: `instanceof` (also for `fetch()` responses), `constructor`,
  `name`, `length`, the statics, subclassing, the error without `new`, and
  the source text (`Function.prototype.toString` is wrapped for that: one
  lookup per call). What still differs: they are other function objects,
  `Bun.inspect(Response)` shows `[Function: Response]` rather than
  `[class Response]`, and their own property names list `prototype` before
  the statics. `[shutdown] long_lived_timeout = 0` leaves both untouched.
- If Warden is SIGKILLed, workers are signalled via `PR_SET_PDEATHSIG` (direct
  children only). Under systemd the cgroup takes care of the rest.
- Don't run Warden as PID 1 in a container; use `tini`, or `docker run --init`, to reap orphans.
- A log consumer that can't keep up (a stuck journald, a full disk) costs
  log lines, never supervision: lines past the queue bounds are dropped,
  reported in the log and counted (`log_lines_dropped` in `status --json`,
  `warden_log_lines_dropped_total` in the metrics). With
  `[logging] max_lines_per_sec = 0` Warden keeps every line by slowing a
  flooding app down instead (for at most a second per read).

### Releasing

```sh
cargo release 0.2.0 --dry-run   # every step printed, nothing changed
cargo release 0.2.0             # or: patch | minor | major
```

`cargo release` (`cargo xtask release`) checks that `main` is clean and in
sync with origin, that the tag is new and CI passed for the commit, sets the
version in `Cargo.toml`, `protocol/` and `gui/` (and `Cargo.lock`), runs fmt,
clippy and the tests, then commits `Release v<version>`, tags
`v<version>` and pushes both. The tag starts the Release workflow, which
builds the CLI and the GUI for Linux, checksums them with the macOS archives,
tests `install.sh` and publishes the
[GitHub Release](https://github.com/oceanwap/warden/releases). The macOS
archives are built on the Mac that runs `cargo release` (macOS runners cost
ten times a Linux minute): it builds, checks and uploads them to a draft
release after the push, and the workflow takes them from there. The command
follows the workflow to the end and prints the release's files. Pushing the
tag needs the right to push tags to the repository. macOS notarization is
still a TODO. Every step, the options and what to do when one fails:
[`docs/releasing.md`](docs/releasing.md).

## License

Warden is open source, under the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option: use it, change it and
ship it, commercially too. Contributions are accepted under the same terms.
Release archives list the licenses of the third-party crates compiled in
(`THIRD-PARTY-LICENSES.txt`; `about.toml` keeps that list to permissive
licenses).

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

Every release on [GitHub Releases](https://github.com/oceanwap/warden/releases)
has two products for Linux (x86_64, arm64) and macOS (arm64, x86_64):

- **CLI only**: `warden-<version>-<os>-<arch>.tar.gz` with the `warden`
  binary, this README and `contrib/` (systemd units, sysctl file). The Linux
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

`install.sh` installs the CLI: it picks the archive for your OS and CPU,
checks it against the release's `SHA256SUMS`, and puts `warden` in
`/usr/local/bin` (as root) or `~/.local/bin`:

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

**Faster rollouts**: `[reload] surge = 2` (or `"all"`) starts that many new
workers at once, each next to the worker it replaces. Once every one of them has
passed the gates, the old ones drain together, then the next batch starts. If one
fails, every new worker of the batch is stopped and the old ones keep serving.
safe-reload still runs its canary alone first. The cost is memory: surge N runs up
to N extra workers for a few seconds (`"all"`: twice the workers). It needs
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
| Memory leak | Graceful replacement when RSS stays above the limit | `[limits] max_memory` |
| Slow degradation | Recycle every worker after a lifetime, ±10% jitter | `[limits] max_lifetime` |
| Stop / shutdown | SIGTERM to each process group, drain (WebSockets closed with 1001 and SSE streams ended after `long_lived_timeout`), SIGKILL after `grace_period` | `[shutdown]` |
| Why it died | `last_exit` and the log line say who ended a worker: a crash (`SIGSEGV`, `SIGABRT`), the kernel's OOM killer (from the cgroup's `oom_kill` count), Warden, or another process, with a hint for the fix | |

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
then `STANDBY`; `standbys` in `--json`), and the GUI counts their memory.
A standby starts in the pinned release (`pin_release`) and is promoted only
while that is still the workers' release. Process mode only, with
`app.port`, a shared port and the shim (bun and node commands).

## CLI

If you know PM2 you know most of it: `warden` in place of `pm2`. `warden help`
has everything; the differences are in the right column.

| PM2 | Warden | Difference |
|---|---|---|
| `pm2 start app.js -i 4 --name api` | `warden start app.js -i 4 --name api` | Waits until the app is up and says so if it isn't (`--no-wait` returns at once). Any program or command line works, as with PM2 |
| `pm2 list`, `pm2 jlist` | `warden list`, `warden list --json` | ~2 ms instead of ~140-160 ms |
| `pm2 describe api` | `warden describe api` | Also the last exits and the last rollout |
| `pm2 reload api` | `warden restart api`, `warden reload api` | One worker at a time through health gates; a failure stops and rolls back. `restart --hard` is PM2's `restart` |
| | `warden deploy api` | Preflight, canary with soak, then the rest, with rollback |
| `pm2 logs api` | `warden logs api` | `--history --grep --since 2h --json` over rotated and gzipped files, pipe-friendly |
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
$ warden status
Application: travelerwe-api
Mode:        process
Workers:     4
Ready:       4
PID:         8139
Uptime:      27s
Release:     /srv/apps/travelerwe/api/releases/2026-09-30
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
off), so a hit is one `send(2)`. HEAD and 304s come from the same entry;
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
- nginx: `upstream api { server 127.0.0.1:3000; keepalive 64; }` A single
  upstream entry is enough; the kernel does the balancing.
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
columns, not absolute numbers.

In short (this run, against PM2 6.0.14):

- **Requests:** Warden adds nothing; node:http 76k req/s (bare 72k, PM2 61k),
  p99 2.4 ms (PM2 8.0 ms).
- **Crash recovery:** 54-687 ms, or 17-57 ms with a hot standby (PM2
  166-1,182 ms).
- **Rolling restarts:** no failed request in any run; PM2 lost 1,904 of
  6,225 for NestJS on Bun. WebSockets and SSE end cleanly (0 abnormal
  closes; PM2 cut every one).
- **Static files:** faster than nginx on small files (108k vs 101k req/s)
  with 40 % less memory.
- **Logs:** a twelfth of PM2's CPU per GB; `worker_output = "direct"` keeps
  every line at 823 MB/s (PM2 163 MB/s).
- **Manager:** about 1 MB PSS per app (wardend included), `list` in 2 ms
  (PM2 150 ms).

<!-- bench:start -->
<!-- Generated by `cargo xtask bench`; edit xtask/src/main.rs or bench/*.ts, not this text. -->

Measured 2026-10-01 on 2 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 8 GB RAM, Linux 6.18.44-fc-v50 x86_64. The load generator (oha, 64 connections) runs on the same CPUs as the apps, so compare columns with each other, not with other machines. Raw numbers: `bench/results/latest/`.

### Node.js app (node:http)

The same app under each manager: 4 workers, one port. PM2 in cluster mode, Watt with 4 worker threads, Warden with 4 processes.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | watt | warden-process | warden-surge | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 219.3 / 94.4 | 347.4 / 154.1 | 486.7 / 477.1 | 237.2 / 108.6 | 233.8 / 102.6 | 289.4 / 116.5 |
| total RAM after load: RSS / PSS (MB) | 281.7 / 140.1 | 383.3 / 189.2 | 577.6 / 568.1 | 299 / 153.9 | - | 356.2 / 166.7 |
| manager RAM (MB) | 0 | 66 | (in total) | 7.9 | 8.1 | 8.2 |
| manager idle CPU (%) | 0 | 0.2 | 2 | 0.2 | - | 0.2 |
| startup to 4 serving (ms) | 171 | 810 | 3889 | 215 | 198 | 195 |
| crash recovery (ms) | n/a (nothing restarts it) | 289 | 1231 | 129 | - | 44 |
| requests failed during a crash | n/a | 2 of 2693 | 1 of 6442 | 1 of 4163 | - | 1 of 3779 |
| rolling restart under load: failed | n/a | 0 of 2036 (728 ms) | 0 of 3457 (3493 ms) | 0 of 4251 (295 ms) | 0 of 3062 (302 ms) | 0 of 4036 (318 ms) |
| status command (ms) | n/a | 135.1 | 380.1 | 1.7 | - | 1.9 |
| /plaintext req/s | 71665 | 61056 | 66661 | 76207 | - | 76530 |
| /plaintext p50 / p99 (ms) | 0.79 / 3.89 | 0.39 / 8.01 | 0.88 / 3.14 | 0.8 / 2.38 | - | 0.8 / 2.29 |
| /plaintext errors | 0 | 0 | 0 | 0 | - | 0 |
| /json req/s | 75356 | 58774 | 64556 | 76709 | - | 72233 |
| /json p50 / p99 (ms) | 0.79 / 2.62 | 0.49 / 8.16 | 0.91 / 3.18 | 0.81 / 2.14 | - | 0.79 / 3.6 |
| /json errors | 0 | 0 | 0 | 0 | - | 0 |
| /cpu req/s | 936 | 965 | 953 | 972 | - | 963 |
| /cpu p50 / p99 (ms) | 60.68 / 158.08 | 65.38 / 121.22 | 65.73 / 127.15 | 62.32 / 131.02 | - | 63.4 / 131.68 |
| /cpu errors | 0 | 0 | 0 | 0 | - | 0 |

### NestJS app on Node.js

A minimal NestJS (Express) app with the same endpoints, 4 workers.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | watt | warden-process | warden-surge | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 394.1 / 230.4 | 491.6 / 279 | 585.9 / 576.3 | 415.7 / 249.9 | 420 / 254.1 | 514 / 294.4 |
| total RAM after load: RSS / PSS (MB) | 686.4 / 522.2 | 814.3 / 601.4 | 1098.9 / 1089.4 | 712.8 / 546.6 | - | 790 / 570.1 |
| manager RAM (MB) | 0 | 65.6 | (in total) | 7.7 | 7.8 | 7.7 |
| manager idle CPU (%) | 0 | 0.2 | 2.8 | 0 | - | 0 |
| startup to 4 serving (ms) | 1040 | 1647 | 3796 | 1138 | 1168 | 1194 |
| crash recovery (ms) | n/a (nothing restarts it) | 1182 | 1609 | 687 | - | 57 |
| requests failed during a crash | n/a | 2 of 2863 | 2 of 4544 | 1 of 2821 | - | 1 of 1482 |
| rolling restart under load: failed | n/a | 0 of 1365 (1576 ms) | 0 of 2329 (3974 ms) | 0 of 1779 (1556 ms) | 0 of 1719 (1268 ms) | 0 of 1504 (1584 ms) |
| status command (ms) | n/a | 125.1 | 367.3 | 1.4 | - | 1.6 |
| /plaintext req/s | 11667 | 9474 | 10444 | 12079 | - | 11569 |
| /plaintext p50 / p99 (ms) | 4.77 / 20.16 | 5.98 / 21.71 | 5.16 / 24.84 | 4.75 / 19.45 | - | 4.95 / 20.53 |
| /plaintext errors | 0 | 0 | 0 | 0 | - | 0 |
| /json req/s | 11776 | 9875 | 11233 | 11624 | - | 11882 |
| /json p50 / p99 (ms) | 4.93 / 17.75 | 5.7 / 17.32 | 5.12 / 21.75 | 4.97 / 18.15 | - | 4.99 / 17 |
| /json errors | 0 | 0 | 0 | 0 | - | 0 |
| /cpu req/s | 887 | 897 | 909 | 892 | - | 910 |
| /cpu p50 / p99 (ms) | 72.46 / 131.64 | 70.63 / 102.24 | 65.17 / 121.54 | 69.44 / 117.44 | - | 71.3 / 121.51 |
| /cpu errors | 0 | 0 | 0 | 0 | - | 0 |

### Bun app (Bun.serve)

4 workers. PM2 has no cluster mode for Bun, so it runs 4 fork-mode instances sharing the port with reusePort. Watt does not run Bun apps.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | warden-process | warden-surge | warden-worker | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 149.5 / 52.5 | 282.6 / 159.8 | 181.9 / 75.7 | 184.2 / 77 | 79.6 / 61.8 | 222.8 / 85.6 |
| total RAM after load: RSS / PSS (MB) | 171.3 / 57.5 | 287.6 / 155.8 | 203 / 80.4 | - | 82 / 61.6 | 244.2 / 90 |
| manager RAM (MB) | 0 | 63.5 | 10.1 | 8.6 | 8.5 | 8.6 |
| manager idle CPU (%) | 0 | 0.2 | 0.2 | - | 0 | 0 |
| startup to 4 serving (ms) | 51 | 588 | 81 | 111 | 91 | 75 |
| crash recovery (ms) | n/a (nothing restarts it) | 166 | 54 | - | 94 | 17 |
| requests failed during a crash | n/a | 0 of 14006 | 0 of 12628 | - | 0 of 12963 | 0 of 13425 |
| rolling restart under load: failed | n/a | 0 of 14434 (571 ms) | 0 of 12751 (262 ms) | 0 of 12624 (267 ms) | 0 of 12136 (265 ms) | 0 of 14183 (260 ms) |
| status command (ms) | n/a | 131.3 | 1.5 | - | 1.6 | 1.4 |
| /plaintext req/s | 113495 | 92372 | 110899 | - | 108505 | 113419 |
| /plaintext p50 / p99 (ms) | 0.38 / 3.61 | 0.59 / 4.1 | 0.33 / 3.71 | - | 0.33 / 4.16 | 0.38 / 3.56 |
| /plaintext errors | 0 | 0 | 0 | - | 0 | 0 |
| /json req/s | 98762 | 88880 | 95859 | - | 98815 | 98858 |
| /json p50 / p99 (ms) | 0.47 / 3.25 | 0.64 / 2.43 | 0.51 / 3.46 | - | 0.45 / 3.98 | 0.51 / 3.17 |
| /json errors | 0 | 0 | 0 | - | 0 | 0 |
| /cpu req/s | 1481 | 1458 | 1428 | - | 1407 | 1474 |
| /cpu p50 / p99 (ms) | 42.64 / 100.14 | 34.5 / 82.07 | 44.57 / 99.38 | - | 41.83 / 90.3 | 42.5 / 94.84 |
| /cpu errors | 0 | 0 | 0 | - | 0 | 0 |

### NestJS app on Bun

The NestJS app on Bun, 4 workers; Warden also in worker (thread) mode.

4 workers · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · wattpm 3.71.0 · warden 0.1.0

| | bare | pm2 | warden-process | warden-surge | warden-worker | warden-standby |
|---|---|---|---|---|---|---|
| total RAM idle: RSS / PSS (MB) | 340.8 / 200 | 425.3 / 279.9 | 349.5 / 205.6 | 351.8 / 208.1 | 187.7 / 166.2 | 434.7 / 248.4 |
| total RAM after load: RSS / PSS (MB) | 435.7 / 292.7 | 515.5 / 368.1 | 430.7 / 284.7 | - | 279.6 / 257.8 | 517.1 / 328.6 |
| manager RAM (MB) | 0 | 65.4 | 7.7 | 8.1 | 8.2 | 8.3 |
| manager idle CPU (%) | 0 | 0 | 0 | - | 0 | 0 |
| startup to 4 serving (ms) | 685 | 1187 | 830 | 773 | 749 | 877 |
| crash recovery (ms) | n/a (nothing restarts it) | 529 | 406 | - | 999 | 45 |
| requests failed during a crash | n/a | 0 of 5160 | 0 of 5332 | - | 0 of 3836 | 0 of 2898 |
| rolling restart under load: failed | n/a | 1904 of 6225 (1119 ms) | 0 of 3764 (1297 ms) | 0 of 2198 (964 ms) | 0 of 3765 (839 ms) | 0 of 4561 (1376 ms) |
| status command (ms) | n/a | 129.5 | 1.6 | - | 2 | 1.8 |
| /plaintext req/s | 40054 | 35844 | 41050 | - | 34549 | 39648 |
| /plaintext p50 / p99 (ms) | 1.08 / 8.81 | 1.23 / 11.02 | 1.18 / 7.73 | - | 1 / 15.05 | 1.11 / 8.31 |
| /plaintext errors | 0 | 0 | 0 | - | 0 | 0 |
| /json req/s | 38120 | 33690 | 38079 | - | 35367 | 36604 |
| /json p50 / p99 (ms) | 1.15 / 8.5 | 1.42 / 8.82 | 0.95 / 9.44 | - | 1.01 / 11.83 | 1.14 / 8.45 |
| /json errors | 0 | 0 | 0 | - | 0 | 0 |
| /cpu req/s | 1364 | 1294 | 1362 | - | 1359 | 1373 |
| /cpu p50 / p99 (ms) | 46.45 / 94.16 | 53.4 / 84.72 | 48.92 / 96.98 | - | 46.69 / 97.13 | 43.59 / 92.85 |
| /cpu errors | 0 | 0 | 0 | - | 0 | 0 |

### Static files

`warden serve` against nginx, `pm2 serve` and the `serve` package, 4 workers each (serve has no cluster mode).

4 workers · nginx/1.24.0 (Ubuntu) · pm2 6.0.14 · serve 14.2.6 · node v22.22.2 · warden 0.1.0

| | warden | warden-nocache | nginx | pm2-serve | serve (1 process) |
|---|---|---|---|---|---|
| processes | 5 | 5 | 5 | 5 | 1 |
| total RAM idle: RSS / PSS (MB) | 29.1 / 8 | 29.8 / 8.6 | 24.4 / 13 | 317.4 / 133.8 | 95.2 / 93.1 |
| total RAM after load: RSS / PSS (MB) | 32.2 / 10.5 | 32.6 / 10.8 | 25.8 / 13.2 | 529.3 / 341.3 | 196.5 / 194.4 |
| startup (ms) | 14 | 12 | 11 | 610 | 258 |
| /index.html req/s | 107596 | 99393 | 100780 | 18294 | 19295 |
| /index.html p50 / p99 (ms) | 0.37 / 3.71 | 0.51 / 3.3 | 0.26 / 6.11 | 2.92 / 11.29 | 3.15 / 5.04 |
| /index.html errors | 0 | 0 | 0 | 0 | 0 |
| /assets/app.3f9a2c1b.js req/s | 74728 | 75882 | 82569 | 14937 | 2001 |
| /assets/app.3f9a2c1b.js p50 / p99 (ms) | 0.71 / 4.11 | 0.56 / 5.91 | 0.61 / 4.18 | 3.2 / 18.29 | 30.88 / 52.67 |
| /assets/app.3f9a2c1b.js errors | 0 | 0 | 0 | 0 | 0 |
| /media/video.bin req/s | 5149 | 6674 | 4940 | 2722 | 1307 |
| /media/video.bin p50 / p99 (ms) | 12.01 / 26.76 | 9.2 / 20.97 | 12.35 / 26.07 | 22.11 / 77.4 | 45.17 / 82.25 |
| /media/video.bin MB/s | 5142.7 | 6667.7 | 4933.4 | 2715.4 | 1300.9 |
| /media/video.bin errors | 0 | 0 | 0 | 0 | 0 |
| /index.html (new connection each) req/s | 35343 | 32865 | 31602 | 5060 | 10840 |
| /index.html (new connection each) p50 / p99 (ms) | 1.77 / 4.06 | 1.88 / 4.42 | 1.99 / 4.66 | 11.05 / 39.25 | 5.45 / 12.63 |
| /index.html (new connection each) errors | 0 | 0 | 0 | 0 | 0 |

### Log-heavy apps

What capturing worker output costs the manager. Both write the app's stdout to a log file; warden-direct splices it there unparsed (worker_output = "direct").

node v22.22.2 · pm2 6.0.14 · warden 0.1.0

Steady: 4 workers × 5000 lines/s × 10 s

| | warden | warden-direct | pm2 |
|---|---|---|---|
| lines written / in the log file | 200000 / 200000 | 200000 / 200000 | 200000 / 200000 |
| manager CPU (s) | 0.29 | 0.11 | 1.23 |
| manager peak RAM (MB) | 6.9 | 6.5 | 74.6 |

Flood: 1 worker writing 200 MB to stdout as fast as it is read

| | warden | warden-direct | pm2 | warden (keep all) |
|---|---|---|---|---|
| time to write it (ms) | 226 | 243 | 1226 | 603 |
| throughput (MB/s) | 885 | 823 | 163.1 | 331.7 |
| manager CPU (s) | 0.12 | 0.13 | 1.5 | 0.66 |
| manager CPU per GB (s) | 0.61 | 0.67 | 7.68 | 3.38 |
| manager peak RAM (MB) | 7 | 6.3 | 95.2 | 7.2 |
| lines in the log file | 10000 | 2076388 | 2076388 | 2076388 |

### Many apps on one host

10 apps (one idle Node process each): the manager's memory and CPU, and how fast everyday commands answer. Warden's managers are one supervisor per app plus wardend.

10 apps, one idle Node process each · node v22.22.2 · pm2 6.0.14 · warden 0.1.0

| | warden | pm2 |
|---|---|---|
| manager processes | 11 | 1 |
| manager RAM, all apps: RSS (MB) | 70.7 | 73.9 |
| manager RAM, all apps: PSS (MB) | 11.6 | 34.3 |
| manager idle CPU (%) | 0.1 | 0.7 |
| start 10 apps (ms) | 571 | 2000 |
| `list` (ms) | 2.3 | 150 |
| `list --json` (ms) | 2.2 | 149.3 |
| `describe app3` (ms) | 1.8 | 142.3 |
| `logs app3 --nostream` (ms) | 1.5 | 145.5 |

### WebSockets and SSE through a rolling restart

50 WebSocket and 50 SSE clients stay connected while all 4 workers are replaced (`pm2 reload`: cluster mode for Node, fork mode for Bun; `warden restart` with the default `long_lived_timeout` of 2 s). Each client reconnects at once. Clean: a WebSocket close frame or the SSE stream's last chunk; abnormal: cut without one (a browser reports 1006, EventSource an error).

4 workers · 50 WebSocket + 50 SSE clients · bun 1.3.13 · node v22.22.2 · pm2 6.0.14 · warden 0.1.0

| | Node: pm2 (cluster) | Node: warden | Bun: pm2 (fork) | Bun: warden | Bun: warden (worker mode) |
|---|---|---|---|---|---|
| rolling restart, until all new workers answer (ms) | 709 | 8562 | 565 | 8304 | 2268 |
| until every client is back on a new worker (ms) | not all within 90 s (98/100) | 8392 | 369 | 8264 | 2155 |
| WebSocket closes: clean / abnormal | 0 / 62 | 69 / 0 | 0 / 73 | 83 / 0 | 50 / 0 |
| WebSocket close codes | 62× 1006 (no close frame) | 69× 1001 | 73× 1006 (no close frame) | 83× 1001 | 50× 1001 |
| SSE ends: clean / abnormal | 0 / 70 | 67 / 0 | 0 / 82 | 64 / 0 | 50 / 0 |
| SSE ends | 70× cut (no last chunk) | 67× end of stream | 82× cut (no last chunk) | 64× end of stream | 50× end of stream |
| failed reconnects (WebSocket + SSE) | 0 | 0 | 300 | 0 | 0 |
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
  - No parent-death signal: workers outlive a supervisor killed with SIGKILL.
  - `warden serve` checks static paths with realpath instead of `openat2`
    (same confinement, slower).
  - No `/proc`: the CPU and RSS columns and metrics are empty, wardend
    sends no `host` events, and it restarts a supervisor with its own
    environment (the launchd job's `PATH`) instead of the one it was started
    with.
- **Windows**: not supported. WSL2 works as Linux.

## Development

```sh
cargo test                  # unit + integration tests (integration tests need `bun` and `node` on PATH)
cargo clippy --all-targets
cargo test --workspace --bins --tests          # also protocol/ and gui/ (the GUI's own tests)
cargo clippy --workspace --all-targets
cargo run -p warden-gui     # the GUI
cargo xtask bench           # benchmarks (see above); `cargo xtask bench --help`
cargo xtask chaos           # chaos soak: a fleet under load, random faults, invariants checked (docs/chaos.md)
cargo release 0.2.0         # a release (see Releasing below); `cargo xtask release --help`
```

The workspace: `warden` (this directory), `protocol/` (the wire types, serde
only, shared by `warden` and the GUI), `gui/` (`warden-gui`) and `xtask/`.
Plain `cargo build` and `cargo test` here mean `warden` only.

All `unsafe` code is in [`src/sys.rs`](src/sys.rs) (`protocol/` and `gui/`
have none: `#![forbid(unsafe_code)]`): system calls the standard
library doesn't expose, and the few that measurably pay on a hot path (the
static server and log capture), each with a SAFETY note and tests. The rest
of the crate is `#![deny(unsafe_code)]`.

Layout: `src/supervisor.rs` (event loop), `src/supervisor/rollout.rs` (gates,
canary, rollback), `src/supervisor/upkeep.rs` (watchdog, recycling),
`src/process.rs` (spawning, fd-3 IPC), `shim/` (embedded JS), `tests/`,
`bench/`, `research/`.

Limitations:

- Linux is authoritative. macOS works for development; see
  [Platforms](#platforms) for what it lacks.
- Worker mode needs Bun and the shim.
- Node apps share the port through Warden's shim (`--import`, Node ≥ 22.12
  for `reusePort`); older Node needs `port_strategy = "offset"`.
- On Bun, the shim wraps `Response` and `ReadableStream`, so it can end SSE
  bodies in a drain (about 20 ns per `new Response`). The wrappers pass for
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
builds the CLI and the GUI for Linux and macOS, checksums them, tests
`install.sh` and publishes the
[GitHub Release](https://github.com/oceanwap/warden/releases); the command
follows it to the end and prints the release's files. Pushing the tag needs
the right to push tags to the repository. macOS notarization is still a
TODO. Every step, the options and what to do when one fails:
[`docs/releasing.md`](docs/releasing.md).

## License

Warden is open source, under the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option: use it, change it and
ship it, commercially too. Contributions are accepted under the same terms.
Release archives list the licenses of the third-party crates compiled in
(`THIRD-PARTY-LICENSES.txt`; `about.toml` keeps that list to permissive
licenses).

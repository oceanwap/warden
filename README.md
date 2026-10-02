# Warden

A fast, crash-safe supervisor for Bun and Node apps. It runs N copies of your
HTTP app on one machine, with a PM2-like CLI and zero-downtime deploys.

- **A supervisor for Bun and Node.** Warden runs the workers; the Linux kernel
  spreads connections across them with `SO_REUSEPORT`. Warden never sits on the
  request path.
- **A CLI you already know.** `warden start`, `list`, `logs`, `restart`, `save`,
  `startup`: PM2's commands, and `warden pm2-migrate` to import your apps.
- **Zero-downtime rolling reloads.** New workers pass health checks before old
  ones drain; a canary and automatic rollback guard every deploy. WebSockets and
  SSE streams end cleanly.
- **A GUI.** A native window with live state, logs and charts for every app on a
  host, or on a remote one over SSH.
- **Static file serving.** `warden serve dist 8080`, supervised like any app.
- **Built in Rust.** One binary of about 5 MB that embeds no JavaScript runtime.

```
Cloudflare → Nginx → 127.0.0.1:3000 ──kernel SO_REUSEPORT──► worker 1..N  (Bun / NestJS)
                                                              ▲
systemd → warden  (spawn · watch · restart · drain · gate · report)
```

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/oceanwap/warden/main/install.sh | sh
```

This puts `warden` in `/usr/local/bin` (as root) or `~/.local/bin`, after
checking the download against the release's `SHA256SUMS`. Options: `--gui` also
installs the GUI, `--version v0.2.0` picks a release, `--uninstall` removes it
(after the pipe: `sh -s -- --gui`). Linux and macOS, x86_64 and arm64. All the
details, and downloading the archives by hand: [docs/install.md](docs/install.md).

Debian, Ubuntu, Fedora and RHEL: `.deb` and `.rpm` packages ([docs/packages.md](docs/packages.md)). macOS: a `.dmg` with the GUI and the CLI.

From source (needs Rust; Warden is not on crates.io). Also the way to run the
latest `main` when no release has been published yet:

```sh
git clone https://github.com/oceanwap/warden.git && cd warden
cargo build --release          # target/release/warden
cargo install --path .         # or: put it in ~/.cargo/bin
```

Linux is the production platform. macOS is for development, and Windows works
through WSL2: see [docs/platforms.md](docs/platforms.md).

## Quick start

Your app only has to listen on `process.env.PORT`.

```sh
warden start server.js --name api -i 4 --port 3000   # 4 workers; waits until the app is up
warden list                                          # every app and worker, with ids
warden logs api                                      # recent lines, then follow
warden restart api                                   # replace workers one at a time, no downtime
warden reload api                                    # the same, re-reading the config first
warden deploy api                                    # safest: preflight, canary, then the rest, with rollback
warden stop api
```

An app with a config file: `warden start -c warden.toml` (see
[Configuration](#configuration)). `warden doctor` checks the host for problems
Warden knows about, with a fix for each.

Bring the apps back after a reboot or a crash:

```sh
warden save                    # remember the running apps and their worker counts
warden startup                 # systemd units (sudo for system units) or a launchd job on macOS
warden resurrect               # start what `save` remembered
```

After upgrading the binary, `warden update` saves, kills and resurrects:
every supervisor and wardend restart from the new binary. The apps stop for a
few seconds, and it asks first on a terminal.

Every command and option: [docs/commands.md](docs/commands.md).

### wardend: one socket for every app

wardend is the host daemon behind `warden events` and the GUI, and it is always
on. `warden start` and `warden resurrect` start it, a supervisor starts it again
if it dies without a clean exit, and only `warden kill` stops everything,
wardend included. Apps never depend on it. Alert rules (a command or a webhook
on crash loops, failed rollouts, OOM kills) live in `wardend.toml`, are checked
by `warden check -c wardend.toml` and `warden doctor`, and are read again when
the file changes. See [docs/wardend.md](docs/wardend.md).

### Parallel starts

`warden resurrect`, `warden update` and `warden start a b c` start apps at the
same time, but no more than one per CPU core by default. `--parallel N` (`-j N`)
or `$WARDEN_PARALLEL` changes that.

## Coming from PM2

`warden` in place of `pm2` gets you most of the way:

| PM2 | Warden |
|---|---|
| `pm2 start app.js -i 4 --name api` | `warden start app.js -i 4 --name api` |
| `pm2 list`, `pm2 jlist` | `warden list`, `warden list --json` |
| `pm2 logs api`, `pm2 flush api` | `warden logs api`, `warden flush api` |
| `pm2 reload api` | `warden reload api` (one worker at a time through health gates, rolls back on failure) |
| `pm2 restart api` | `warden restart --hard api` |
| `pm2 stop api`, `pm2 delete api` | `warden stop api`, `warden delete api` |
| `pm2 monit` | `warden top`, `warden events` |
| `pm2 save`, `resurrect`, `startup`, `update` | `warden save`, `resurrect`, `startup`, `update` |
| `pm2 start app.js --watch` | `warden start app.js --watch` (opt-in, a gated rolling restart: [docs/watch.md](docs/watch.md)) |
| `pm2 serve dist 8080` | `warden serve dist 8080` |

To move your running apps:

```sh
warden pm2-migrate --dry-run                   # what it would write, from `pm2 jlist`
warden pm2-migrate                             # <app>.toml + <app>.env (0600) + MIGRATION.md
warden pm2-migrate --cutover same-port         # PM2 stops each app, Warden starts it; PM2 is restored on failure
warden pm2-migrate --finalize                  # remove the migrated apps from PM2, `warden save`
```

Env values go to a 0600 file, never into the config. The command-by-command
differences, and what `pm2-migrate` carries over: [docs/comparison.md](docs/comparison.md).

## Configuration

One TOML file per app. A minimal one (everything else has a default):

```toml
[app]
name = "api"
command = "bun"
args = ["run", "dist/main.js"]
working_directory = "/srv/apps/api/current"
port = 3000

[workers]
count = 4

[health]
enabled = true
path = "/health"          # checked on each worker's private socket
```

`warden check -c warden.toml` validates it. Every key, with its default:
[docs/configuration.md](docs/configuration.md); an annotated file to copy:
[warden.example.toml](warden.example.toml).

## Static files

`warden serve dist 8080` runs Warden's own file server as the app's workers, so
it is supervised, health-checked and reloaded like any app. It does ETag and 304s,
ranges, precompressed `.br` / `.gz` files, SPA fallback, `404.html` and Basic
auth, and keeps small files in memory. It is as fast as nginx, and faster on
small files. Details and the `[static]` keys: [docs/static-serving.md](docs/static-serving.md).

## GUI

![Warden GUI: every app with its state, workers, CPU and memory](docs/gui-main-screen.png)

`warden-gui` is a native window on wardend: every app with its state, workers,
CPU and memory, live; logs, events and 1, 6 or 24 hour charts per app; and the
CLI's actions (reload, restart, scale, stop, start), adding an app and editing
its config. `warden-gui --ssh deploy@web-1` connects to a remote host. It is a
separate process: closing it touches nothing. See [gui/README.md](gui/README.md).

## Benchmarks

The same apps under PM2, Platformatic Watt, nginx and Warden, on one small
machine (2 CPUs shared with the load generator, so compare columns, not
absolute numbers). Measured 2026-10-01 against PM2 6.0.14:

| | Warden | Compared with |
|---|---|---|
| Requests, node:http, 4 workers | 73.5k req/s, p99 2.4 ms | PM2 55.3k req/s, p99 8.5 ms |
| Crash recovery, Bun app | 74 ms, 25 ms with a hot standby | PM2 162 ms |
| Rolling restart under load, NestJS on Bun | 0 failed requests | PM2 lost 2,093 of 6,571 |
| `list` with 10 apps | 2.6 ms | PM2 156 ms |
| Manager memory, 10 apps (PSS) | 11.9 MB | PM2 40 MB |
| Static 1.5 KB page | 101.9k req/s | nginx 93.4k req/s |

Source, method, caveats and the other suites: see [docs/benchmarks.md](docs/benchmarks.md)
and [bench/README.md](bench/README.md). `cargo xtask bench` re-runs everything.

## Documentation

| | |
|---|---|
| [Install](docs/install.md) | The installer, release archives, checksums, uninstalling |
| [Linux packages](docs/packages.md) | `.deb` and `.rpm`: what they hold, what they leave alone |
| [Commands](docs/commands.md) | Every `warden` command and option |
| [Configuration](docs/configuration.md) | Every config key, environment variables |
| [Deploying without downtime](docs/deploys.md) | Reload, safe-reload, canary, rollback, WebSockets and SSE |
| [Staying up](docs/reliability.md) | Crash, hang and leak handling, hot standbys |
| [Production setup](docs/production.md) | systemd units, `warden startup`, reboots, nginx |
| [wardend](docs/wardend.md) | The host daemon: events, alerts, history |
| [Static serving](docs/static-serving.md) | `warden serve` and `[static]` |
| [File watching](docs/watch.md) | `[watch]`, `warden start --watch` |
| [Behind a proxy](docs/proxies.md) | nginx, AWS ALB/NLB, GCP, Cloudflare, no proxy |
| [Troubleshooting](docs/troubleshooting.md) | Problems by symptom, each with a fix |
| [Platforms](docs/platforms.md), [Windows](docs/windows.md) | Linux, macOS, Windows (WSL2), runtime requirements, the plan for a native build |
| [Compared with PM2](docs/comparison.md) | Command mapping, `pm2-migrate`, systemd and Watt |
| [How it works](docs/how-it-works.md) | The design in short, limitations |
| [Benchmarks](docs/benchmarks.md) | Numbers, method and optimisation log |
| [Architecture](docs/architecture.md), [Protocol](docs/protocol.md) | The design in full, with the experiments; the control and event sockets, for scripts and clients |
| [Development](docs/development.md), [Releasing](docs/releasing.md) | Toolchain, tests, workspace layout; cutting a release |
| [Review process](docs/review-process.md), [Chaos soak](docs/chaos.md) | How changes to the supervisor are reviewed; `cargo xtask chaos` |
| [GUI](gui/README.md) | `warden-gui` in detail |
| [Changelog](CHANGELOG.md) | What changed in each version |

## Contributing and license

Contributions are welcome: see [CONTRIBUTING.md](CONTRIBUTING.md), and
[SECURITY.md](SECURITY.md) for reporting a vulnerability.

Warden is open source, under the [MIT License](LICENSE-MIT) or the
[Apache License 2.0](LICENSE-APACHE), at your option: use it, change it and
ship it, commercially too. Contributions are accepted under the same terms.
Release archives list the licenses of the third-party crates compiled in
(`THIRD-PARTY-LICENSES.txt`; `about.toml` keeps that list to permissive
licenses).

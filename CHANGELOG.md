# Changelog

All notable changes to Warden are documented here.
Format inspired by [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased] — 0.1.0-unreleased

Current `main`. There is no GitHub Release or tag yet; the first one is cut
with `cargo release 0.1.0` on a Mac (see [`docs/releasing.md`](docs/releasing.md)).

### Added

- Process-mode supervisor for Bun/Node HTTP workers: `SO_REUSEPORT`, graceful
  drain, gated rollouts (`reload` / `deploy`), restart backoff, hot standbys,
  release pinning (`[app] pin_release`), exit reasons (including the kernel
  OOM killer).
- Rolling restarts that overlap the drains of old workers
  (`[reload] max_draining`), and WebSocket / SSE connections closed cleanly
  during a drain (`[shutdown] long_lived_timeout`).
- Experimental worker mode (Bun Workers + embedded shim).
- `warden update`: save, stop every supervisor and wardend, start them again
  from the warden binary on disk (a supervisor keeps the code it started with,
  so this is how a running host picks up an upgrade); the GUI has the same
  under Settings, "Restart everything". `warden list` and the GUI say when a
  supervisor is older than the warden asking, and which numbers it lacks.
- wardend is always on: `warden start` and `warden resurrect` start it, a
  supervisor starts it again when it dies without a clean exit (a crash or
  `kill -9`), and only `warden kill` stops it. There is no `warden daemon`
  command; `warden check -c wardend.toml` and `warden doctor` check the alert
  rules, and wardend reads the file again by itself when it changes.
- `warden resurrect`, `warden update` and `warden start a b c` start the apps at the same
  time, one per CPU core at once so a big host is not asked to start everything together:
  `--parallel N` (`-j N`, `all` for no limit) or `$WARDEN_PARALLEL` changes it; wardend's own
  `--resurrect` follows `$WARDEN_PARALLEL` too.
- The workers' directory (the folder a static site serves) shows under the
  project name in the GUI and in `warden describe`.
- GUI settings: Warden or the desktop's own colors (macOS, GNOME and
  Ubuntu), and light, dark or automatic.
- `wardend` control daemon: second-level supervision, alerts
  (webhook/command), a 24 h resource history that survives restarts, a
  Unix-socket protocol shared with the CLI and GUI (`protocol/`,
  [`docs/protocol.md`](docs/protocol.md)).
- Native GUI (`warden-gui`): live status, logs, history charts, actions;
  optional single-host SSH tunnel (`--ssh`).
- CLI: start/stop/reload/scale/status/logs, `doctor`, `flush`, `startup` /
  `unstartup` (systemd and launchd), PM2 migration, per-worker event-loop
  delay, `warden start` that fails fast when an app cannot start.
- `warden list` as a boxed table like `pm2 list`, with an id for every app
  (kept in the state directory's `ids.json`), and ids, lists and ranges as
  targets for every command: `warden start 0,1,2`, `warden stop 0-3`,
  `warden restart api web:2`.
- `warden describe`, `status <app>`, `daemon status` and `doctor` print boxed
  tables like PM2's, and `start`, `stop`, `restart`, `reload`, `delete`,
  `scale`, `reset`, `resurrect` and `serve` print the app table afterwards
  (on a terminal, or with `WARDEN_TABLE=1`). Wide cells wrap at spaces when
  the terminal width is known.
- [`docs/windows.md`](docs/windows.md): why Warden does not run natively on
  Windows, how to use it under WSL2, and the plan for a native build.
- Platform adapters (`src/platform/`): one trait for what Warden asks the OS
  (a process's memory, CPU, owner, ports, environment and working directory,
  every process's command line, the host's load, the boot id), with a Linux
  (`/proc`), a macOS (libproc, `sysctl`, Mach) and an "other Unix" (answers
  nothing) implementation, chosen at build time and checked by one set of
  contract tests. On macOS the CPU and mem columns, `warden list`'s ports
  check, wardend's `host` events, PM2 daemon detection and restarting a
  supervisor with its original environment now work (they were empty).
- `warden doctor` checks the OS adapter live (a `platform` row: it asks about
  its own process and says what it could not read), and
  `scripts/mac-check.sh` runs that, the platform tests and a smoke test of a
  real supervisor and wardend on a Mac (and times `warden serve`), and writes
  a report.
- The ports and Unix sockets each app really listens on, read from the OS
  (`/proc` on Linux, libproc on macOS) for the worker and the processes it
  started, so a wrapper like `npm run start` or `turbo` and a monorepo's
  several ports are found: a `ports` column in `warden list` and the workers
  table, a `ports` row in `describe` and `status` (with `all interfaces` or
  `localhost only`, and a warning when the configured `port` is not one of
  them), `warden ports [app] [--json]` with a URL for each, and
  `status.workers[].listening` in the protocol. Warden's own health sockets
  are not shown.
- A `user` column in `warden list` and the workers table, and a `user` row in
  `status` and `describe`: who the app's processes run as (`Status.user`,
  [`docs/protocol.md`](docs/protocol.md)).
- `pm2-migrate` leaves out environment names an env file cannot hold (PM2
  records one named after the app) and lists them in `MIGRATION.md`, instead
  of writing an env file Warden refuses to read.
- Built-in static file server with an in-memory cache.
- Log capture and `worker_output = "direct"` (splice into files), rotation,
  history and search.
- Benchmarks (`cargo xtask bench`), a seeded chaos soak (`cargo xtask chaos`),
  and a release script (`cargo release`).
- CI: Linux (x86_64, arm64) and macOS builds and tests; a workflow that checks
  `warden startup` against real systemd and launchd; benchmarks on x86_64 and
  ARM64 runners; a short chaos run.
- `install.sh` for the release assets, and a Release workflow that builds and
  tests the Linux archives and can also be started by hand from GitHub
  (Actions → Release → Run workflow), which creates the tag itself once
  everything is built and tested. The macOS archives are built on a Mac
  (`cargo dist-macos`, run by `cargo release`) and published with the rest.
- [`contrib/nginx.conf`](contrib/nginx.conf) and
  [`docs/proxies.md`](docs/proxies.md) for running behind nginx or a cloud load
  balancer.

### Notes

- macOS GUI notarization is not done (it needs an Apple Developer account).
- `SECURITY.md` says how to report a vulnerability.

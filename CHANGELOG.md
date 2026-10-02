# Changelog

All notable changes to Warden are documented here.
Format inspired by [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased] — 0.1.0-unreleased

Current `main`. There is no GitHub Release or tag yet; the first one is cut
with `cargo release 0.1.0` (the macOS archives are built on a GitHub runner, or
on your Mac with `--macos local`; see [`docs/releasing.md`](docs/releasing.md)).

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
  everything is built and tested. The macOS archives are built on a GitHub
  macOS runner (the default) or on a Mac (`cargo release --macos local`, which
  runs `cargo dist-macos`), chosen per release, and published with the rest.
- [`contrib/nginx.conf`](contrib/nginx.conf) and
  [`docs/proxies.md`](docs/proxies.md) for running behind nginx or a cloud load
  balancer.
- Opt-in file watching: `[watch]` (`enabled`, `paths`, `ignore`, `debounce_ms`,
  `interval_ms`, `max_files`) and `warden start --watch [--ignore-watch ...]
  [--watch-delay ...]` restart an app with the same gated rolling restart
  `warden restart` does when its files change (off by default; a polling scan,
  debounced and throttled; [`docs/watch.md`](docs/watch.md)). `warden list`
  has a `watching` column, `describe` a `watch` row and `status.watching` is in
  the protocol. `pm2-migrate` maps PM2's `watch`, `ignore_watch` and
  `watch_delay` to `[watch]` instead of reporting them as unsupported.
- `[static] html_max_age` and `warden serve --html-max-age N`: seconds browsers
  may reuse HTML pages (unset: revalidate on every load, as before; at most
  one year; `private` with `basic_auth`).
- `install.sh` options: `--gui`, `--version`, `--uninstall`, `--prefix`,
  `--dry-run`, `--modify-path`; musl/glibc detection, a PATH hint, and every
  download verified before anything is installed
  ([`docs/install.md`](docs/install.md), `scripts/test-install.sh`).
- Linux packages: `.deb` and `.rpm` for `warden` and `warden-gui` (amd64/arm64,
  x86_64/aarch64), built with nfpm in the Release workflow and tested by
  installing them in containers ([`docs/packages.md`](docs/packages.md)); a
  macOS `.dmg` with `Warden.app` next to the zip.
- The Release workflow can be run as a dry run (build and check everything,
  publish nothing) and writes the release notes from this file.
- `warden migrate-wattpm [dir|file]` imports a Platformatic Watt project
  (checked against wattpm 3.71.0). It writes one config and one 0600 env file
  per application, plus `MIGRATION-wattpm.md`, which lists every setting as
  mapped, approximated, unsupported or to check. A Gateway and applications
  without a runnable command are reported, not converted. Options: `--dry-run`,
  `--apps`, `--out`, `--prefix`, `--command <id>=<line>`, `--overwrite`,
  `--cutover overlap|new-port:<port>` (starts Warden's copy and never stops
  wattpm). [`docs/wattpm.md`](docs/wattpm.md): what Watt and Warden each do,
  the wattpm-to-warden command mapping, and who should and should not move.
- macOS: a supervisor killed with SIGKILL no longer leaves a second set of
  workers behind. The next start of the app stops the workers it left running
  (recorded as pid and start time under the state directory; a reused pid is
  never touched), and `warden doctor` lists them.
- GUI: icons on buttons, tabs, state badges, chips, dialogs and Settings;
  Settings scrolls in short windows. "Install command line tool" in Settings
  (and a one-time banner on the first run from Warden.app) links `warden` into
  `/usr/local/bin` (asking for the administrator password if needed, else
  `~/.local/bin` with a PATH hint), and can remove the link; on Linux it links
  into `~/.local/bin`.
- CI runs `tests/static_perf.rs` (with strace on Linux).
- macOS: the supervisor that stops a killed supervisor's workers keeps answering
  while it does (`status` shows `sweep`), obeys SIGTERM at once without starting
  workers, and `warden start` waits for the sweep instead of reporting a failed
  start. Oversized or non-regular files in the record directory are not read.
- GUI: a damaged `gui.json` is kept as `gui.json.bad` instead of overwritten;
  "Restart everything" shows which `warden` runs and aims it at the connected
  wardend, runs up to 15 minutes and offers `warden resurrect` if it fails;
  Cancel on the administrator prompt installs nothing; text contrast is 4.5:1 in
  every look; the machines menu scrolls; replies from before a host switch are
  dropped; the tunnel's ssh ends with the window on Linux.
- `migrate-wattpm` and `pm2-migrate` never write through a symlink, keep an
  existing env file unless `--overwrite`, and keep secrets out of reports, dry
  runs and error text (a `{SECRET}` in a shell command is read from the 0600
  env file; in other commands the config is written 0600).

### Changed

- README is short and aimed at people installing and using Warden; the detail
  moved to `docs/` (commands, configuration, comparison, deploys, platforms,
  static serving and more). `cargo xtask bench` now writes its tables to
  `docs/benchmarks.md` (`--no-docs`; `--no-readme` still works).
- Static serving: big files go out in 1 MiB `sendfile` pieces, so a download no
  longer holds up other requests on its worker (a 1 KB request next to a 10 MB
  download: 5.4 ms to 0.34 ms at p50); bodies of 1.25 MiB and more use
  `TCP_CORK` (21-38% less CPU per 2-100 MB response); 404, 401, 405 and listing
  responses are one packet, and HEAD to them no longer carries a body; a new
  connection costs two fewer syscalls. On macOS `SO_NOSIGPIPE` is set once per
  connection and head plus body go out in one `sendfile` (not yet run on a
  Mac). Measured on a loaded 2-CPU VM: [`docs/benchmarks.md`](docs/benchmarks.md).
- Static serving, the worker's own cost per request: the request head is
  parsed in place (no allocation), the 10 s and 15 s waits for a request are
  enforced by one task ticking once a second instead of a timer per request
  (so they hold to within about a second), and a new connection whose request
  has arrived and whose answer is cached is answered from the accept loop
  (`accept`, `recv`, `send`, `close`; no task, no epoll registration). User-space
  CPU per request is down by a fifth on a kept-alive connection and by 60 % on
  a new one; a cached 1 KB page on a new connection costs 13.1 µs of server CPU
  instead of 16.8 (nginx 19.4), 4 system calls instead of 9; the exit summary
  says how many requests the accept loop answered. Keep-alive throughput
  is bound by the kernel and does not change. Measured on a loaded 2-CPU VM, with
  a note on why a single pinned client misreads it:
  [`docs/benchmarks.md`](docs/benchmarks.md).
- `Status.watching` (the `watching` column) says whether a watcher is running
  now, not only that `[watch]` is enabled: it reads `disabled` for a stopped
  app or a watcher that died (`describe` says "set, but not running").
- All `unsafe` code is in `src/sys.rs` and `src/sys/darwin.rs`: the two
  `pre_exec` blocks moved behind safe wrappers.
- Two load-flaky integration tests (hot-standby takeover, OOM attribution of
  two SIGKILL deaths) are deterministic: they were races in the tests.
- `warden start --watch` is no longer rejected ("Warden is for production").
  `warden serve --watch` is an error: the file server reads from disk.

### Notes

- macOS GUI notarization is not done (it needs an Apple Developer account).
- `SECURITY.md` says how to report a vulnerability.

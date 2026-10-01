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
- `wardend` control daemon: second-level supervision, alerts
  (webhook/command), a 24 h resource history that survives restarts, a
  Unix-socket protocol shared with the CLI and GUI (`protocol/`,
  [`docs/protocol.md`](docs/protocol.md)).
- Native GUI (`warden-gui`): live status, logs, history charts, actions;
  optional single-host SSH tunnel (`--ssh`).
- CLI: start/stop/reload/scale/status/logs, `doctor`, `flush`, `startup` /
  `unstartup` (systemd and launchd), PM2 migration, per-worker event-loop
  delay, `warden start` that fails fast when an app cannot start.
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

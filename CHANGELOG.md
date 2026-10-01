# Changelog

All notable changes to Warden are documented here.
Format inspired by [Keep a Changelog](https://keepachangelog.com/).

## [Unreleased] — 0.1.0-unreleased

Current `main` (no GitHub Release or tag yet). Summary from the README and
tree as of this changelog's introduction:

### Added

- Process-mode supervisor for Bun/Node HTTP workers: `SO_REUSEPORT`, graceful
  drain, gated rollouts (`reload` / `safe-reload`), restart backoff, hot
  standbys, release pinning (`[app] pin_release`).
- Experimental worker mode (Bun Workers + embedded shim).
- `wardend` control daemon: multi-app host, alerts (webhook/command),
  in-memory resource history (24 h), Unix-socket protocol shared with the CLI
  and GUI (`protocol/`).
- Native GUI (`warden-gui`): live status, logs, history charts, actions;
  optional single-host SSH tunnel (`--ssh`).
- CLI: start/stop/reload/scale/status/logs, `doctor`, `startup` /
  `unstartup` (systemd and launchd), fleet helpers.
- CI: Linux/macOS builds and tests; service-manager workflow against real
  systemd and launchd (see `.github/workflows/service-managers.yml`).
- `install.sh` for future GitHub Release assets (no release published yet;
  build from source today).

### Notes

- Toolchain / MSRV: Rust **1.98** (`RUST_TOOLCHAIN=1.98.1` in workflows).
- macOS GUI notarization is still TODO.
- First public binary release is deferred until cutting a tag via
  `cargo xtask release` (see `docs/releasing.md`).

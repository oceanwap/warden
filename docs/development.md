# Development

**Toolchain.** CI builds with Rust `1.95.0` (pinned in the workflows, because a
new release brings new clippy lints and `-D warnings` turns them into
failures; bump it on purpose). The crates declare the lowest versions they are
meant to build with: `warden` and `warden-protocol` 1.85, `warden-gui` 1.88
(iced 0.14). Newer stable toolchains work, but only the pinned one is tested.

```sh
rustup toolchain install 1.95.0
cargo build --release       # → target/release/warden
cargo test                  # unit + integration tests (integration tests need `bun` and `node` on PATH)
cargo clippy --all-targets
cargo test --workspace --bins --tests          # also protocol/ and gui/ (the GUI's own tests)
cargo clippy --workspace --all-targets
cargo run -p warden-gui     # the GUI
cargo xtask bench           # benchmarks, all suites (~25 min on 2 CPUs); `--quick` is a ~10-minute smoke test
                            # (see benchmarks.md and ../bench/README.md); `cargo xtask bench --help`
cargo xtask chaos           # chaos soak: a fleet under load, random faults, invariants checked (chaos.md)
cargo release 0.2.0         # a release (see Releasing below); `cargo xtask release --help`
cargo dist-macos            # the macOS release archives, built on this Mac (see Releasing below)
scripts/mac-check.sh        # on a Mac: builds, runs the platform tests and drives a real supervisor and
                            # wardend (CPU, memory, user, ports, environment, host events, static latency); the report
                            # is target/mac-check/report.txt
```

The workspace: `warden` (the repository root), `protocol/` (the wire types, serde
only, shared by `warden` and the GUI), `gui/` (`warden-gui`) and `xtask/`.
Plain `cargo build` and `cargo test` here mean `warden` only.

All `unsafe` code is in [`src/sys.rs`](../src/sys.rs) and its macOS half
[`src/sys/darwin.rs`](../src/sys/darwin.rs) (`protocol/` and `gui/` have none:
`#![forbid(unsafe_code)]`): system calls the standard library doesn't expose,
and the few that measurably pay on a hot path (the static server and log
capture), each with a SAFETY note and tests. The rest of the crate is
`#![deny(unsafe_code)]`.

What differs per OS (reading a process's memory, owner, ports and
environment, the host's load, the boot id) sits behind one trait with an
implementation per OS in [`src/platform/`](../src/platform/): `linux.rs` reads
`/proc`, `macos.rs` calls libproc and `sysctl`, `other.rs` answers nothing.
The adapter is chosen when Warden is built; the same contract tests run
against the real OS on Linux (CI) and on a Mac.

Layout: `src/supervisor.rs` (event loop), `src/supervisor/rollout.rs` (gates,
canary, rollback), `src/supervisor/upkeep.rs` (watchdog, recycling),
`src/process.rs` (spawning, fd-3 IPC), `src/platform/` (the per-OS adapters),
`shim/` (embedded JS), `tests/`, `bench/`, `research/`.

For the design behind these files, see [`architecture.md`](architecture.md);
for what reviewers look for, [`review-process.md`](review-process.md); for the
limits users should know about, [`how-it-works.md`](how-it-works.md#limitations).

## Releasing

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
archives and the `.dmg` are built on the Mac that runs `cargo release`: it builds, checks and uploads them to a draft
release after the push, and the workflow takes them from there. The command
follows the workflow to the end and prints the release's files. Pushing the
tag needs the right to push tags to the repository. macOS notarization is
still a TODO. Every step, the options and what to do when one fails:
[`releasing.md`](releasing.md).

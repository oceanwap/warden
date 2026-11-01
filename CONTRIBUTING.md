# Contributing

Thank you for considering a contribution to Warden.

- **Review bar**: production supervisors need careful review. Read
  [`docs/review-process.md`](docs/review-process.md) before changing
  `src/supervisor*`, `src/process.rs`, or `shim/`, and for milestone work.
- **Build**: Rust 1.95 is what CI uses (see docs/development.md). `cargo test`,
  `cargo clippy --all-targets`; integration tests need `bun` and `node`.
- **Rust standards** (CI fails on any of these; run them before pushing):
  - `cargo fmt --all` (`rustfmt.toml`: 120 columns).
  - `cargo clippy --workspace --all-targets -- -D warnings`. The extra lints
    are in `[workspace.lints.clippy]` in `Cargo.toml`: no `dbg!`, `todo!`
    or `unimplemented!`, no `mem::forget`, no unused `async`, and every
    `unsafe` block has a `// SAFETY:` comment saying why it is sound.
  - `unsafe` code lives only in `src/sys.rs` (and `src/sys/`) and
    `gui/src/parent_death.rs`;
    everywhere else `#![deny(unsafe_code)]` applies (`protocol/` forbids it).
  - To silence a lint, use `#[allow(clippy::...)]` on the smallest item and
    a one-line reason after it. A bare `allow` gets a review comment.
  - `cargo deny check advisories sources bans` (`deny.toml`; the Audit
    workflow, weekly and on dependency changes): no known-vulnerable or
    yanked crates, crates.io only. New dependencies must be permissive
    licenses (`about.toml`).
  - Do not add work to the request path for the sake of style: hot-path
    code is changed for a measured reason (`docs/benchmarks.md`).
- **Style**: prefer small, reviewable PRs.
- **Security**: report privately — see [`SECURITY.md`](SECURITY.md).
- **License**: contributions are accepted under MIT OR Apache-2.0, same as
  the project.

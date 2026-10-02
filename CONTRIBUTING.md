# Contributing

Thank you for considering a contribution to Warden.

- **Review bar**: production supervisors need careful review. Read
  [`docs/review-process.md`](docs/review-process.md) before changing
  `src/supervisor*`, `src/process.rs`, or `shim/`, and for milestone work.
- **Build**: Rust 1.95 is what CI uses (see docs/development.md). `cargo test`,
  `cargo clippy --all-targets`; integration tests need `bun` and `node`.
- **Style**: `cargo fmt`. Prefer small, reviewable PRs.
- **Security**: report privately — see [`SECURITY.md`](SECURITY.md).
- **License**: contributions are accepted under MIT OR Apache-2.0, same as
  the project.

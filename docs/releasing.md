# Releasing

A release is a tag `v<version>` on `main`, matching the version in
`Cargo.toml`. The tag starts `.github/workflows/release.yml`, which builds,
tests and publishes the GitHub Release. One command does the part before it
and then follows the workflow to the end:

```sh
cargo release 0.2.0 --dry-run   # every step printed, the read-only checks run, nothing changed
cargo release 0.2.0             # or: cargo release patch | minor | major
cargo release 0.1.0             # Cargo.toml's version, never tagged: tagged as is
cargo xtask release --help      # every option
```

`cargo release` is a cargo alias (`.cargo/config.toml`) for
`cargo xtask release`, in `xtask/src/release.rs`. If the third-party
`cargo-release` is installed, cargo warns that the alias shadows it; use
`cargo xtask release`, the same command.

## From GitHub, without a checkout

The same workflow can publish by hand, for when you have no checkout or can't
push tags from where you are:

1. Make sure `main` has the commit to release, with the version in
   `Cargo.toml` (and `protocol/Cargo.toml`, `gui/Cargo.toml`, `Cargo.lock`)
   already at the number to release, and that **CI is green on it**.
2. **Actions → Release → Run workflow**, choose the branch `main`, tick
   **Publish the release**, and run it.

The first job checks what `cargo release` checks before it tags, and stops
with a message saying how to fix it: the run is on `main`; the tag
`v<Cargo.toml version>` does not exist yet; the `CI` workflow has a passed
run on this very commit (tick **Publish even without a green CI run** only
if you are sure). Then it builds, packages and install-tests everything, and
only when all of that passed does the last job create the GitHub Release and
the tag `v<version>` at that commit, with generated notes. A failed build
leaves no tag behind. Run it without ticking the box for a dry run: the
files are kept as workflow artifacts.

This path does not change the version: to release another number, commit
the bump to `main` first (`cargo release <version>` does that and the tag
locally in one go).

## The steps

Each step is printed as it runs. The first failure stops the release and
says how to fix it; a `--dry-run` goes on and lists every problem. Until the
commit, a failure undoes the version edit, so the tree is as it was.

1. **Preconditions.** git is installed; the branch is `main` (or the one
   named by `--branch`); the working tree is clean (untracked files too);
   `git fetch origin` works and the branch is neither ahead of nor behind
   origin; the tag `v<version>` exists neither here nor on origin
   (`git ls-remote --tags origin`); the version is valid semver, newer than
   every `v*` tag and not older than `Cargo.toml`. Without `--yes`, stdin
   must be a terminal (someone answers the confirmation).
2. **CI status** of the commit being released (`--no-ci-check` skips it).
   The GitHub API (`GET /repos/<owner>/<repo>/actions/runs?head_sha=…`,
   owner and repo from `git remote get-url origin`) must show a passed run
   of the `CI` workflow. Still running: it stops, or waits with
   `--wait-ci`. This comes before the slow local checks, so a red CI stops
   the release early. Two more workflows are worth a look before a
   release, though they do not gate it: **Service managers** (every push:
   `warden startup`/`unstartup` on real systemd and launchd) and **Chaos**
   (pushes to `main` and daily: a 3-minute `cargo xtask chaos --release`
   with a fixed seed, Linux and macOS); their results are annotations on
   the run.
3. **Version.** If it differs, `[package] version` is set in `Cargo.toml`,
   `protocol/Cargo.toml` and `gui/Cargo.toml` (released together, always the
   same version: the workflow checks both `warden --version` and
   `warden-gui --version`). Only that line changes; comments and
   formatting stay. `xtask/` is never released and stays 0.0.0. Then
   `cargo update --workspace --offline` (online if the index isn't cached)
   updates the workspace's own entries in `Cargo.lock`, and the diff
   summary is shown.
4. **Local checks** (`--skip-checks` skips them): `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace --bins --lib`; with `--full`, also
   `cargo test --test integration` (needs `bun` and `node`).
5. **Confirmation.** What will be committed, tagged and pushed, and what the
   workflow then does; `Release v<version>? [y/N]` unless `--yes`.
6. **Commit and tag.** First, the checkout must still be what CI and the
   checks ran on: HEAD on the same commit and branch, nothing changed but
   the version files. If something committed meanwhile (an editor, a hook,
   another terminal), the release stops and the version edit is undone.
   The version files are committed as `Release v<version>` (none when the
   version was already right), with exactly the checked commit as its
   parent (`git commit-tree`, then `git update-ref`, which refuses if the
   branch moved; commit hooks don't run, `commit.gpgsign` is honoured),
   then the annotated tag `v<version>`, "Warden <version>", on that commit's
   sha. Trailers: `--trailer "Key: value"` (repeatable), plus any configured
   with git's `trailer.<key>.cmd`.
7. **Push**: `git push --atomic --force-with-lease=refs/heads/main:<origin's
   sha from step 1> origin <sha>:refs/heads/main refs/tags/v<version>`:
   exactly the tagged commit and the tag, both or neither, and only while
   origin's `main` is still where step 1 saw it, so a push to origin in the
   meantime is refused, never overwritten. If origin refuses (no right to
   push tags from this environment), the commit and tag stay in the
   checkout and the exact command to run where pushing is allowed is
   printed. Running `cargo release` again sees the unpushed tag and says
   the same.
8. **Release workflow** (`--no-wait` skips following it). The run for the
   tag, polled about every 30 s (slower if the API's rate limit would run
   out), each job printed as its state changes; at the end the release URL
   and its assets, or the failed jobs and the run URL.

`GITHUB_TOKEN` or `GH_TOKEN`, if set, authenticates the API calls (a higher
rate limit, and a private repository works); the token goes to curl on
stdin, never on a command line. Without one, the API allows 60 requests an
hour, enough to follow a release at a slower pace.

## What the Release workflow does

`.github/workflows/release.yml`, on a pushed `v*` tag, or run by hand with
**Publish the release** ticked (above):

- **meta**: the tag must match `Cargo.toml`'s version, or nothing is built.
  By hand: the same, plus the checks above.
- **licenses**: the third-party notices (cargo-about); fails on a license
  `about.toml` doesn't accept.
- **cli**: `warden` for Linux x86_64 and arm64 (glibc 2.28 baseline, built
  with cargo-zigbuild) and macOS arm64 and x86_64; checks the binary
  (stripped, glibc symbols, `--version`), then packages it.
- **gui**: `warden-gui` with the CLI, for the same four platforms: a
  `.tar.gz` on Linux, a zipped, ad-hoc signed `Warden.app` on macOS.
- **checksums**: `SHA256SUMS` over every archive and `install.sh`.
- **install-test**: `install.sh` against those very files on Linux x86_64,
  Linux arm64 and macOS, as a user and as root; a tampered archive must be
  refused.
- **publish**: the GitHub Release, with generated notes; a version with a
  `-` (`0.2.0-rc.1`) is marked as a pre-release.

Nothing is published unless every build and install test passed. Pushes to
the `ci-portability` branch and manual runs without the box ticked do
everything but publish (the files are kept as workflow artifacts for 14
days).

## Where the artifacts land

`https://github.com/oceanwap/warden/releases/tag/v<version>`:

- `warden-<version>-<os>-<arch>.tar.gz`: the CLI (`linux-x86_64`,
  `linux-arm64`, `macos-arm64`, `macos-x86_64`);
- `warden-gui-<version>-linux-<arch>.tar.gz` and
  `warden-gui-<version>-macos-<arch>.zip`: the GUI with the CLI;
- `install.sh` and `SHA256SUMS`.

`https://github.com/oceanwap/warden/releases/latest/download/install.sh`
always serves the newest release that is not a pre-release (README,
"Install").

## Push rights

Pushing the tag starts the release, so it needs write access to the
repository (and, if a ruleset protects `v*` tags, the right to create them).
The workflow needs nothing more: its `publish` job uses the run's own token
(`contents: write`), also when it creates the tag itself in a run started by
hand; running a workflow needs write access to the repository too. From an
environment that may push to branches but not tags, the release stops at
step 7 with the commit and the tag kept: run the printed `git push --atomic …`
where you can push tags, or use the workflow by hand.

GitHub Actions must be able to run: with a failed payment or a spending limit
reached, every job is refused ("recent account payments have failed…"), and
so is a release (**Settings → Billing & plans**).

## When something fails

- **Before the push**: nothing left this machine, and the version edit is
  undone. Fix the cause and run the same command again.
- **The push**: see step 7. With `--atomic`, origin got nothing.
- **The workflow**: nothing is published. If it was flaky, re-run the failed
  jobs (`gh run rerun <id> --failed`, or "Re-run failed jobs" on the run
  page). If the code must change: `git push --delete origin v<version>` and
  `git tag -d v<version>`, fix it on `main`, let CI pass, release again.
- **After publishing**: don't move a published tag; release a patch.

## TODO

- **macOS notarization.** `Warden.app` is ad-hoc signed but not notarized,
  so Gatekeeper refuses the first open (README, "Install", says how to open
  it anyway). It needs the owner's Apple Developer ID in repository secrets;
  the steps are in `release.yml` (`TODO(signing)`). The CLI installed by
  `install.sh` is not affected.

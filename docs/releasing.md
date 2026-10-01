# Releasing

A release is a tag `v<version>` on `main`, matching the version in
`Cargo.toml`. The tag starts `.github/workflows/release.yml`, which builds and
tests the Linux archives and publishes the GitHub Release. The macOS archives
are built on a Mac instead (see "The macOS archives" below: macOS runner
minutes cost ten times a Linux minute). One command, run on that Mac, does the
part before the workflow, builds the macOS archives and then follows the
workflow to the end:

```sh
cargo release 0.2.0 --dry-run   # every step printed, the read-only checks run, nothing changed
cargo release 0.2.0             # or: cargo release patch | minor | major
cargo release 0.1.0             # Cargo.toml's version, never tagged: tagged as is
cargo xtask release --help      # every option
cargo dist-macos                # only the macOS archives (below)
```

`cargo release` is a cargo alias (`.cargo/config.toml`) for
`cargo xtask release`, in `xtask/src/release.rs`. If the third-party
`cargo-release` is installed, cargo warns that the alias shadows it; use
`cargo xtask release`, the same command.

## From GitHub, without a checkout

The same workflow can publish by hand, for when you can't push tags from
where you are (you still need a Mac for the macOS archives):

1. Make sure `main` has the commit to release, with the version in
   `Cargo.toml` (and `protocol/Cargo.toml`, `gui/Cargo.toml`, `Cargo.lock`)
   already at the number to release, and that **CI is green on it**.
2. **Actions → Release → Run workflow**, choose the branch `main`, tick
   **Publish the release**, and run it.
3. On a Mac, with that very commit checked out (`git pull`): `cargo dist-macos`.
   The workflow's `macos` job is waiting for the files (up to an hour).

The first job checks what `cargo release` checks before it tags, and stops
with a message saying how to fix it: the run is on `main`; the tag
`v<Cargo.toml version>` does not exist yet; the `CI` workflow has a passed
run on this very commit (tick **Publish even without a green CI run** only
if you are sure). Then it builds, packages and install-tests the Linux
archives, takes the macOS ones from the draft release, and only when all of
that passed does the last job publish the release and create the tag
`v<version>` at that commit, with generated notes. A failed build leaves no
tag behind. Run it without ticking the box for a dry run: the Linux archives
are kept as workflow artifacts (no macOS ones, no release).

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
   printed (the macOS archives are then not built: run the printed push on
   a Mac, then `cargo dist-macos`). Running `cargo release` again sees the unpushed tag and says
   the same.
8. **macOS archives** (`--no-macos` skips them; not on a Mac, they are
   skipped with a note). `scripts/dist-macos.sh --branch main`, run after the
   push, while the workflow's Linux builds are already running: it builds,
   checks and uploads the macOS archives to the draft release (below). If it
   fails, nothing is undone: the tag is on origin and the workflow's `macos`
   job waits an hour for the files; fix the cause, run `cargo dist-macos`.
9. **Release workflow** (`--no-wait` skips following it). The run for the
   tag, polled about every 30 s (slower if the API's rate limit would run
   out), each job printed as its state changes; at the end the release URL
   and its assets, or the failed jobs and the run URL.

`GITHUB_TOKEN` or `GH_TOKEN`, if set, authenticates the API calls (a higher
rate limit, and a private repository works); the token goes to curl on
stdin, never on a command line. Without one, the API allows 60 requests an
hour, enough to follow a release at a slower pace.

## What the Release workflow does

`.github/workflows/release.yml`, on a pushed `v*` tag, or run by hand with
**Publish the release** ticked (above). All on Linux runners:

- **meta**: the tag must match `Cargo.toml`'s version, or nothing is built.
  By hand: the same, plus the checks above.
- **licenses**: the third-party notices (cargo-about); fails on a license
  `about.toml` doesn't accept.
- **cli**: `warden` for Linux x86_64 and arm64 (glibc 2.28 baseline, built
  with cargo-zigbuild); checks the binary (stripped, glibc symbols,
  `--version`), then packages it.
- **gui**: `warden-gui` with the CLI, for Linux x86_64 and arm64 (a `.tar.gz`).
- **macos** (a real release only): waits, up to an hour, for the macOS
  archives in the draft release for the tag, built on a Mac. It takes them
  only when `macos-build-info.txt` names this very commit and a clean tree,
  and checks that each archive holds Mach-O binaries of the architecture in
  its name and the version in its `Info.plist`. It prints what is missing
  every few minutes (an annotation on the run).
- **checksums**: `SHA256SUMS` over every archive (macOS ones included) and
  `install.sh`.
- **install-test**: `install.sh` against those very files on Linux x86_64 and
  Linux arm64, as a user and as root; a tampered archive must be refused. (The
  macOS install is tested on the Mac by `scripts/dist-macos.sh`.)
- **publish**: adds the Linux archives, `SHA256SUMS` and `install.sh` to the
  draft, writes generated notes and publishes it (the tag is created there,
  in a run started by hand); a version with a `-` (`0.2.0-rc.1`) is marked as
  a pre-release.

Nothing is published unless every build, the macOS check and the install
tests passed. Manual runs without the box ticked do everything but the macOS
archives and the publishing (the Linux files are kept as workflow artifacts for
14 days).

## The macOS archives

Built on a Mac by `scripts/dist-macos.sh` (`cargo dist-macos`; `cargo release`
runs it after the push): `warden-<version>-macos-{arm64,x86_64}.tar.gz` and
`warden-gui-<version>-macos-{arm64,x86_64}.zip` (an ad-hoc signed
`Warden.app`), with the same contents the Linux archives have. Either kind of
Mac builds both architectures.

What you need: Xcode's command line tools, `rustup` (the script installs the
toolchain `release.yml` pins), `cargo install cargo-about --locked --features
cli` (the third-party notices), and the GitHub CLI, logged in with a right to
write releases (`brew install gh && gh auth login`).

What it does: refuses a dirty tree (the archives must come from the commit
they are released as); builds release binaries for both targets; checks that
the binary says the right `--version` (an Intel build on an Apple silicon Mac
is run through Rosetta, if installed); packages; runs `install.sh` on the
archive for this Mac (checksum checked, a tampered archive refused); and
uploads everything to a **draft** release for `v<version>` (created at the
branch if there is none), `macos-build-info.txt` last. That file records the
commit, the date, the macOS and Rust versions; the workflow takes the archives
only when its commit is the one being released, so files from another commit
are never published. Nothing is published from the Mac: the workflow does it.
It refuses a version that already has a published release.

`scripts/test-dist-macos.sh` tests the script's logic on Linux, with stubs for
the Mac tools (CI runs it); only a run on a Mac tests the real compilers and
`codesign`. `--no-upload` builds without touching GitHub; `--help` lists the
options.

To build macOS on GitHub's runners again (for instance if the repository
becomes public, where they are free), restore the macOS entries in the `cli`,
`gui` and `install-test` matrices from this file's history, before commit
"macOS archives are built on a Mac".

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
so is a release (**Settings → Billing & plans**). A release needs only Linux
minutes (a few cents), so a small spending limit is enough; the macOS archives
need no Actions at all, and are uploaded even while Actions are blocked.

## Minutes

On a private repository every runner minute is billed against the plan's
included minutes (Free: 2,000 a month, no carry-over), and a macOS minute
counts ten times a Linux one. So the workflows spend them where they matter:

| Workflow | Linux x86_64 | Linux ARM64 | macOS |
|---|---|---|---|
| **CI** | every push and pull request | main, pull requests, by hand | main, pull requests, by hand |
| **Service managers** | every push | | main, pull requests, by hand |
| **Chaos** | main, daily, by hand | | by hand only |
| **Bench** | `bench` branch, by hand | `bench` branch, by hand | |
| **Release** | tag, by hand | tag, by hand | none: built on your Mac |

A commit that only changes docs, `*.md` files or `bench/results/` skips every
job but the Linux x86_64 ones (`.github/code-changed.sh` decides; Linux x86_64
still runs so the commit has a green CI run to release). To test a scratch
branch on macOS or ARM64, start CI by hand on it (**Actions → CI → Run
workflow**). Run **Chaos** by hand, macOS ticked, before a release. A release
costs about an hour of Linux minutes and no macOS ones.

## When something fails

- **Before the push**: nothing left this machine, and the version edit is
  undone. Fix the cause and run the same command again.
- **The push**: see step 7. With `--atomic`, origin got nothing.
- **The macOS archives** (step 8): the tag is pushed and the workflow's
  `macos` job waits an hour. Fix what the script said, run `cargo dist-macos`
  (it replaces the draft's macOS files; run it on the released commit). If the
  hour is over, re-run the failed jobs.
- **The workflow**: nothing is published. If it was flaky, re-run the failed
  jobs (`gh run rerun <id> --failed`, or "Re-run failed jobs" on the run
  page). If the code must change: `git push --delete origin v<version>` and
  `git tag -d v<version>`, delete the draft release (its macOS files were built
  from the old commit), fix it on `main`, let CI pass, release again.
- **After publishing**: don't move a published tag; release a patch.

## TODO

- **macOS notarization.** `Warden.app` is ad-hoc signed but not notarized,
  so Gatekeeper refuses the first open (README, "Install", says how to open
  it anyway). It needs the owner's Apple Developer ID; the archives are now
  built on the owner's Mac, so the certificate can stay in its keychain and
  no secrets go to CI. The steps are in `scripts/dist-macos.sh`
  (`TODO(signing)`). The CLI installed by `install.sh` is not affected.

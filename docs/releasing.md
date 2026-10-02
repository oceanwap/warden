# Releasing

A release is a tag `v<version>` on `main`, matching the version in
`Cargo.toml`. The tag starts `.github/workflows/release.yml`, which builds and
tests the Linux archives and packages and publishes the GitHub Release. The
macOS archives (the CLI, the zipped `Warden.app` and the `.dmg`) have two
possible sources, **chosen per release**: a GitHub-hosted macOS runner (the
default, no Mac needed) or your own Mac (see "The macOS archives" below). One
command does the part before the workflow and then follows the workflow to the
end:

```sh
cargo release 0.2.0 --dry-run        # every step printed, the read-only checks run, nothing changed
cargo release 0.2.0                  # macOS archives built on a runner by the workflow (or: patch | minor | major)
cargo release 0.2.0 --macos local    # macOS archives built on this Mac and uploaded to the draft release
cargo release 0.1.0                  # Cargo.toml's version, never tagged: tagged as is
cargo xtask release --help           # every option
cargo dist-macos                     # the Mac's half of --macos local: build and upload (below)
```

`cargo release` is a cargo alias (`.cargo/config.toml`) for
`cargo xtask release`, in `xtask/src/release.rs`. If the third-party
`cargo-release` is installed, cargo warns that the alias shadows it; use
`cargo xtask release`, the same command.

Before it: put the release's section in `CHANGELOG.md` and push it (it becomes
the release notes, see "Release notes"), and try the workflow once without
publishing (next section).

## A dry run first

**Actions → Release → Run workflow**, on any branch, with **Publish the
release** left unticked, builds everything the release has, exactly as a
release would, and publishes nothing: no tag, no GitHub Release, no draft. The
Linux archives and packages are built and tested (the packages installed in
containers, `install.sh` against the archives), `SHA256SUMS` is written, and the
files are kept for 14 days as the workflow artifacts `release-<version>` (every
file) and `release-notes` (what the release page will say; also in the run's
summary). Run it before tagging, and after any change to `release.yml`,
`scripts/dist-macos.sh`, `scripts/dist-linux-packages.sh` or
`contrib/nfpm*.yaml`: the workflow itself is only checked for syntax
(`actionlint`, in CI) until it runs.

The run's **macOS archives** input (`macos_source`) decides what the dry run
proves about macOS:

- `runner` (the default): the workflow builds the macOS archives on a macOS
  runner and checks them, so the dry run proves the macOS build too (both
  architectures, the `.dmg` really mounted, `install.sh` run on the arm64
  files) and the artifact `release-<version>` holds them, with `SHA256SUMS`
  covering them. It is the longest part of the run.
- `local`: the archives would come from your Mac, which a dry run does not wait
  for or take: no macOS archives, as before. (`cargo dist-macos --no-upload` on
  the Mac builds and checks them without touching GitHub.)

A pushed tag `v*` is always the real release; run by hand, only the ticked box
is (below).

## From GitHub, without a checkout

The same workflow can publish by hand, for when you can't push tags from
where you are (a Mac is needed only if you pick the source `local`):

1. Make sure `main` has the commit to release, with the version in
   `Cargo.toml` (and `protocol/Cargo.toml`, `gui/Cargo.toml`, `Cargo.lock`)
   already at the number to release, and that **CI is green on it**.
2. **Actions → Release → Run workflow**, choose the branch `main`, tick
   **Publish the release**, choose where the macOS archives are built
   (**macOS archives**: `runner`, the default, or `local`), and run it.
3. Only for `local`: on a Mac, with that very commit checked out
   (`git pull`): `cargo dist-macos`. The workflow's `macos-local` job is
   waiting for the files (up to an hour). With `runner` there is nothing to do.

The first job checks what `cargo release` checks before it tags, and stops
with a message saying how to fix it: the run is on `main`; the tag
`v<Cargo.toml version>` does not exist yet; the `CI` workflow has a passed
run on this very commit (tick **Publish even without a green CI run** only
if you are sure). Then it builds, packages and install-tests the Linux
archives and packages, builds the macOS ones on a runner (or takes them from
the draft release your Mac filled), and only when all of that passed does the
last job publish the release and create the tag `v<version>` at that commit,
with the notes from `CHANGELOG.md`. A failed build leaves no tag behind. Run it
without ticking the box for a dry run (above).

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
   every `v*` tag and not older than `Cargo.toml`. With `--macos local` on a
   Mac, also `scripts/dist-macos.sh --check` (the tools and the `gh` login the
   macOS archives need), so that a missing one stops the release before anything
   is pushed; with `--macos runner` no Mac tool is needed and nothing is run.
   Without `--yes`, stdin must be a terminal (someone answers the confirmation).
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
5. **Confirmation.** What will be committed, tagged and pushed, what the
   workflow then does and where the macOS archives come from (the line
   `macos: runner` or `macos: local` the tag will carry); `Release
   v<version>? [y/N]` unless `--yes`.
6. **Commit and tag.** First, the checkout must still be what CI and the
   checks ran on: HEAD on the same commit and branch, nothing changed but
   the version files. If something committed meanwhile (an editor, a hook,
   another terminal), the release stops and the version edit is undone.
   The version files are committed as `Release v<version>` (none when the
   version was already right), with exactly the checked commit as its
   parent (`git commit-tree`, then `git update-ref`, which refuses if the
   branch moved; commit hooks don't run, `commit.gpgsign` is honoured),
   then the annotated tag `v<version>` on that commit's sha: its message is
   "Warden <version>", then a paragraph with the line `macos: runner` or
   `macos: local` (`--macos`), which the workflow reads to decide where the
   macOS archives are built. Trailers: `--trailer "Key: value"` (repeatable), plus any configured
   with git's `trailer.<key>.cmd`.
7. **Push**: `git push --atomic --force-with-lease=refs/heads/main:<origin's
   sha from step 1> origin <sha>:refs/heads/main refs/tags/v<version>`:
   exactly the tagged commit and the tag, both or neither, and only while
   origin's `main` is still where step 1 saw it, so a push to origin in the
   meantime is refused, never overwritten. If origin refuses (no right to
   push tags from this environment), the commit and tag stay in the
   checkout and the exact command to run where pushing is allowed is
   printed (with `--macos local` the macOS archives are then not built: run
   the printed push on a Mac, then `cargo dist-macos`). Running `cargo release`
   again sees the unpushed tag and says the same.
8. **macOS archives.** With `--macos runner` (the default) there is nothing to
   do here: the workflow builds them. With `--macos local`:
   `scripts/dist-macos.sh --branch main`, run after the push, while the
   workflow's Linux builds are already running: it builds, checks and uploads
   the macOS archives to the draft release (below). `--no-macos` skips it, and
   not on a Mac it is skipped with a note: the tag still says `macos: local`
   and the workflow waits for a Mac's upload. If the script fails, nothing is
   undone: the tag is on origin and the workflow's `macos-local` job waits an
   hour for the files; fix the cause, run `cargo dist-macos`.
9. **Release workflow** (`--no-wait` skips following it). The run for the
   tag, polled about every 30 s (slower if the API's rate limit would run
   out), each job printed as its state changes; at the end the release URL
   and its assets, or the failed jobs and the run URL.

`GITHUB_TOKEN` or `GH_TOKEN`, if set, authenticates the API calls (a higher
rate limit, and a private repository works); the token goes to curl on
stdin, never on a command line. Without one, the API allows 60 requests an
hour, enough to follow a release at a slower pace.

## What the Release workflow does

`.github/workflows/release.yml`, on a pushed `v*` tag (the release), or run by
hand: a dry run, or with **Publish the release** ticked (above). On Linux
runners, except the `macos-runner` job:

- **meta**: the version in `Cargo.toml` must be `X.Y.Z` or `X.Y.Z-PRERELEASE`,
  and a pushed tag must match it, or nothing is built. By hand with the box
  ticked: the same, plus the checks above (on `main`, no such tag yet, a green
  CI run). It also writes the release notes (below) and keeps them as the
  artifact `release-notes`, on a dry run too. And it decides where the macOS
  archives are built, its output `macos_source` (`runner` or `local`): the
  input of that name when run by hand; for a pushed tag, the line
  `macos: runner` or `macos: local` in the tag's annotation (a lightweight tag,
  or no such line, means `runner`; any other value stops the run before
  anything is built). Of the two macOS jobs below exactly one runs.
- **licenses**: the third-party notices (cargo-about); fails on a license
  `about.toml` doesn't accept.
- **cli**: `warden` for Linux x86_64 and arm64 (glibc 2.28 baseline, built
  with cargo-zigbuild); checks the binary (stripped, glibc symbols,
  `--version`), then packages it.
- **gui**: `warden-gui` with the CLI, for Linux x86_64 and arm64 (a `.tar.gz`).
- **packages**: from those very binaries, `.deb` and `.rpm` for `warden` and
  `warden-gui` ([`docs/packages.md`](packages.md)), built with nfpm by
  `scripts/dist-linux-packages.sh` on a runner of the packages' own
  architecture. `scripts/test-linux-packages.sh` then inspects every package
  (what it holds, and that it holds no unit, no sysctl file and no script
  that starts anything) and installs it in Debian, Ubuntu, Fedora and Rocky
  Linux containers: `dpkg -i` / `rpm -i`, `warden --version`, `warden doctor`,
  the GUI package, a reinstall and the removal. A missing tool or container
  runtime fails the job (`--require`).
- **macos-runner** (`macos_source` `runner`, a dry run too): on a GitHub-hosted
  `macos-15` runner (Apple silicon) with the pinned Rust toolchain, both macOS
  targets and `cargo-about`, runs `scripts/dist-macos.sh --no-upload`, the very
  script a Mac runs: the CLI archives, `Warden.app` (ad-hoc signed) in the zips
  and the `.dmg` disks, for arm64 and x86_64, `install.sh` run on the arm64
  files, `macos-build-info.txt`. Nothing is sent to GitHub, and the job has no
  write permission. It then checks the files again with a Mac's own tools
  (architecture of every binary, licenses, the version in `Info.plist`, the
  `.dmg` really mounted, the build info vouching for each file) and keeps them
  as the artifact `macos-assets` ("The macOS archives", below).
- **macos-local** (`macos_source` `local`, a real release only): waits, up to
  an hour, for the macOS archives in the draft release for the tag, built on a
  Mac. It takes the archives only when `macos-build-info.txt` names this very
  commit and a clean tree and every archive matches the checksum listed there
  (a half-finished or stale upload is waited out, never taken), and checks that
  each holds Mach-O binaries of the architecture in its name and the version in
  its `Info.plist`. It retries API errors, and prints what is missing every few
  minutes (an annotation on the run). Skipped when the source is `runner`.
- **checksums**: checks that every file a release holds is there (the list is
  written out in the job: a job that produced less must not make a smaller
  release), then `SHA256SUMS` over every archive, package (macOS ones included) and
  `install.sh`. It runs only when the macOS job the release chose succeeded (the
  other one is skipped, which is fine); a dry run with the source `local` has no
  macOS files, and none are expected then.
- **install-test**: `install.sh` against those very files on Linux x86_64 and
  Linux arm64, as a user and as root; a tampered archive must be refused. (The
  macOS install is tested by `scripts/dist-macos.sh` on the Mac or the runner
  that built the files.)
- **publish**: the only job that writes (with `macos-local`, which only reads
  the draft). It uploads every file to the draft (made here, empty, when the
  macOS archives came from a runner and no Mac made one; the macOS files
  replaced by the very bytes `SHA256SUMS` lists), downloads the draft and checks it against
  `SHA256SUMS`, file for file, **before** anything is public, then publishes it
  with the notes (the tag is created there, in a run started by hand); a
  version with a `-` (`0.2.0-rc.1`) is marked as a pre-release. It does
  nothing to a release that is already published (a re-run). A last step
  downloads what was published and checks it again.

Nothing is published unless every build, the packages' tests, the macOS job
the release chose and the install tests passed. Dry runs do everything but the
publishing (and, with the macOS source `local`, the macOS archives); the files
are kept as workflow artifacts for 14 days.

Actions are named by major version (`actions/checkout@v5`); Dependabot
(`.github/dependabot.yml`) proposes the bumps weekly. Only `publish` and
`macos-local` have `contents: write`, and they run no third-party action (the
build jobs, `macos-runner` among them, only read). No job uses a cache (a
release is built from the sources, not from what an earlier run left behind).
Runners are `ubuntu-24.04` and `ubuntu-24.04-arm`, named rather than `-latest`,
which GitHub moves to Ubuntu 26.04 between 2026-10-19 and 2026-11-19, and
`macos-15` for the macOS build, named for the same reason; the GUI is built on
`ubuntu-22.04` for its older glibc (see the TODO at the end).

### Release notes

The notes of the GitHub Release come from `CHANGELOG.md`
(`.github/release-notes.sh`, which you can run yourself:
`.github/release-notes.sh 0.2.0`): the section whose heading names the
version, like `## [0.2.0] — 2026-11-02`; a pre-release has a section of its
own. With none, the `## [Unreleased]` section (without its lead-in before the
first `###`), and a warning on the run. With neither, only the footer, which
points at [`docs/install.md`](install.md), [`docs/packages.md`](packages.md) and
how to check `SHA256SUMS`. `cargo release` does not touch the changelog:
before the release, rename the `[Unreleased]` heading to `[<version>]` with the
date, commit it and push it (`cargo release` needs the branch level with origin
and tags the commit you are on, or the one it makes on top of it for the
version bump). A dry run shows what the notes will be.

## The macOS archives

`warden-<version>-macos-{arm64,x86_64}.tar.gz` (the CLI),
`warden-gui-<version>-macos-{arm64,x86_64}.zip` (a zipped, ad-hoc signed
`Warden.app`) and `Warden-<version>-macos-{arm64,x86_64}.dmg`, with the same
contents the Linux archives have, plus `macos-build-info.txt`. Both
architectures are always built for a release, on either kind of machine, by the same script,
`scripts/dist-macos.sh`, and there are two ways to run it, **chosen per
release**:

| | `runner` (the default) | `local` |
|---|---|---|
| Where | the workflow's `macos-runner` job, on a GitHub-hosted macOS runner | your Mac, started by `cargo release --macos local` (or `cargo dist-macos`) |
| The command | `cargo release 0.2.0` | `cargo release 0.2.0 --macos local` |
| The script runs as | `scripts/dist-macos.sh --no-upload` | `scripts/dist-macos.sh --branch main`, which uploads to the draft release |
| You need | nothing but a checkout | a Mac with Xcode's command line tools, `rustup`, `cargo-about` and `gh` (logged in) |
| What it proves in a dry run | everything about macOS: it builds and checks the archives | nothing: a dry run takes no macOS archives |
| Signing | ad-hoc | ad-hoc (Developer ID later: see the TODO at the end) |

### Choosing

- With `cargo release`: `--macos runner` (the default) or `--macos local`.
  Either way the choice is written into the tag, as a line in its annotation:
  `macos: runner` or `macos: local` (`git show v0.2.0` shows it). That is what
  the workflow reads, so a tag pushed by any means says where its macOS
  archives come from.
- By hand in the workflow (**Actions → Release → Run workflow**): the input
  **macOS archives**, `runner` or `local`.
- A tag made without `cargo release`: `git tag -a v0.2.0 -m "Warden 0.2.0" -m
  "macos: local"`. A lightweight tag (`git tag v0.2.0`) or an annotation
  without the line means `runner`. A line with any other value (`macos: mac`)
  stops the run before anything is built.
- Exactly one of the two macOS jobs runs: the other is skipped, which the rest
  of the workflow accepts. A failure of the chosen one stops the release.

When to prefer which:

- `runner` for the ordinary release. Nothing depends on a Mac being at hand, on
  its login or on its toolchain; the build is the same every time, a dry run
  proves it before the tag exists, and nothing is uploaded from a laptop. It is
  the slowest job of the workflow (below), and its files are ad-hoc signed.
- `local` when the archives should come from the owner's Mac: the signing is
  ad-hoc on both paths today, but a Developer ID certificate can stay in that
  Mac's keychain and not in CI, so signing and notarization (TODO below) can
  start on this path without secrets in the repository. It is also the way out
  when the `runner` build fails for a reason on GitHub's side (see "When
  something fails" for switching a tag that is already pushed).

### Built on a runner (`runner`)

The `macos-runner` job runs on `macos-15` (Apple silicon), named rather than
`macos-latest` for the same reason the Linux runners are: the label moves to a
newer macOS, Xcode and SDK on GitHub's schedule, and these binaries are linked
against the SDK of whichever image runs. It installs the Rust toolchain
`release.yml` pins, both macOS targets and `cargo-about` (from source, with the
version the `licenses` job uses), logs the macOS, Xcode and SDK versions it got,
and runs `scripts/dist-macos.sh --no-upload`: the very script a Mac runs, which
builds, packages, runs `install.sh` on the arm64 archive and writes
`macos-build-info.txt`, and sends nothing anywhere (no `gh`, no draft, no tag).
The job has no write permission.

The Intel archives are cross-built on the arm64 runner (`x86_64-apple-darwin`).
`warden` and `warden-gui` are Rust with system frameworks and no C code of
their own (no `cc` crate in their dependency tree), so Xcode's linker and SDK
are all the cross-build needs. Rosetta is only needed to *run* the x86_64
binaries: where the runner has it the Intel files are run too, where it does
not the job says so in an annotation and checks them without running them.

It then checks the files again with a Mac's own tools, in a step of its own
("Check the files for real"), because the script's checks run on the machine
that built them and these should be checked as they will be downloaded:

- `macos-build-info.txt` names this run's commit and a clean tree, and every
  archive matches the checksum it lists;
- in the CLI archive, the licenses and notices, and a `warden` that is a Mach-O
  64-bit binary of exactly the archive's architecture (`file` and `lipo -archs`)
  and says `warden <version>`;
- in the zip and in the `.dmg`, `Warden.app` holds `warden-gui` and `warden` of
  that architecture, the five license files, an `Info.plist` whose
  `CFBundleShortVersionString` and `CFBundleVersion` are the release's version,
  a valid ad-hoc signature (`codesign --verify`), and both programs say their
  version;
- the `.dmg` is really mounted (`hdiutil attach -nobrowse -readonly`, detached
  again whatever happens) and holds the link to `/Applications`.

The files are kept as the artifact `macos-assets` (14 days), which `checksums`
takes like the other jobs' files, so `SHA256SUMS` and the release cover them;
a dry run has them in `release-<version>`.

Time: the release profile has fat LTO and one codegen unit, and the job builds
four binaries (`warden` and `warden-gui`, each for two targets) with no cache,
so it is the longest job of the workflow. The first runs have not been
measured; the job's limit is 90 minutes, and `cargo dist-macos --no-upload` on
a Mac gives the build part of the figure.

### Built on your Mac (`local`)

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
commit, the date, the macOS and Rust versions and the checksum of every
archive; the workflow takes the archives only when its commit is the one being
released and each archive matches, so files from another commit, or a mix of
two builds, are never published. When it replaces a draft's files it deletes
the old info file first, and it refuses to upload part of a release (`--arch`
and `--no-gui` need `--no-upload`). It refuses a dirty tree, a HEAD that is not
the commit the tag names (here or on origin), a HEAD that moved during the
build, and a version that already has a published release (also one published
while it was building). Nothing is published from the Mac: the workflow does it.
`cargo dist-macos --check` only checks that this Mac has what a release needs.

`cargo dist-macos --no-upload` builds and checks everything and touches
nothing on GitHub (no `gh`, no tag or release is looked at, and the commit need
not be tagged): what the runner job runs, and the way to try the build on a Mac
before a release or to reproduce a failure of the runner job. `--help` lists
the options.

`scripts/test-dist-macos.sh` tests the script's logic on Linux, with stubs for
the Mac tools (CI runs it); only a run on a Mac or on the runner tests the real
compilers, `codesign` and `hdiutil`.

## Where the artifacts land

`https://github.com/oceanwap/warden/releases/tag/v<version>`:

- `warden-<version>-<os>-<arch>.tar.gz`: the CLI (`linux-x86_64`,
  `linux-arm64`, `macos-arm64`, `macos-x86_64`);
- `warden-gui-<version>-linux-<arch>.tar.gz` and
  `warden-gui-<version>-macos-<arch>.zip` and
  `Warden-<version>-macos-<arch>.dmg`: the GUI with the CLI;
- `warden_<version>-1_<amd64|arm64>.deb`, `warden-gui_<version>-1_<amd64|arm64>.deb`,
  `warden-<version>-1.<x86_64|aarch64>.rpm`, `warden-gui-<version>-1.<x86_64|aarch64>.rpm`:
  the Linux packages ([`docs/packages.md`](packages.md)); `<version>` is the
  release's, `0.2.0-rc.1` for a pre-release (inside the package it is
  `0.2.0~rc.1`, which sorts before `0.2.0`);
- `install.sh` and `SHA256SUMS`.

`https://github.com/oceanwap/warden/releases/latest/download/install.sh`
always serves the newest release that is not a pre-release
([`docs/install.md`](install.md)).

## Push rights

Pushing the tag starts the release, so it needs write access to the
repository (and, if a ruleset protects `v*` tags, the right to create them).
The workflow needs nothing more: its `publish` job uses the run's own token
(`contents: write`), also when it creates the tag itself in a run started by
hand; running a workflow needs write access to the repository too. From an
environment that may push to branches but not tags, the release stops at
step 7 with the commit and the tag kept: run the printed `git push --atomic …`
where you can push tags, or use the workflow by hand.

## Which workflow runs when

Standard GitHub-hosted runners are free for a public repository, so nothing
here is rationed for cost; the ARM64 and macOS jobs skip a commit that only
changes docs because they are slow, not because they are dear.

| Workflow | Linux x86_64 | Linux ARM64 | macOS |
|---|---|---|---|
| **CI** | every push and pull request | main, pull requests, by hand | main, pull requests, by hand |
| **Service managers** | every push (also warden from the `.deb`) | | main, pull requests, by hand |
| **Chaos** | main, daily, by hand | | by hand only |
| **Bench** | `bench` branch, by hand | `bench` branch, by hand | |
| **Release** | tag, by hand | tag, by hand | tag, by hand (the `macos-runner` job; with the source `local`, none: built on your Mac) |

A commit that only changes docs, `*.md` files or `bench/results/` skips every
job but the Linux x86_64 ones (`.github/code-changed.sh` decides; Linux x86_64
still runs so the commit has a green CI run to release). To test a scratch
branch on macOS or ARM64, start CI by hand on it (**Actions → CI → Run
workflow**). Run **Chaos** by hand, macOS ticked, before a release. A release
takes about an hour of Linux runner time, most of it the builds; the macOS
job (source `runner`) runs alongside them, and the workflow ends when the
slowest job does.

## When something fails

- **Before the push**: nothing left this machine, and the version edit is
  undone. Fix the cause and run the same command again.
- **The push**: see step 7. With `--atomic`, origin got nothing.
- **The macOS archives, source `local`** (step 8): the tag is pushed and the
  workflow's `macos-local` job waits an hour. Fix what the script said, run
  `cargo dist-macos` (it replaces the draft's macOS files; run it on the
  released commit). If the hour is over, re-run the failed jobs.
- **The `macos-runner` job** (source `runner`) failed: nothing is published,
  and the other jobs' results stay, so re-running only this job is enough. Open
  its log; the step that failed says what:
  - *Rust targets*, *Install cargo-about*: a network or crates.io problem or a
    version that is not there; re-run the job.
  - *Build, package and check*: the script's own message (it says what it
    wants, as it does on a Mac). To reproduce it, run `cargo dist-macos
    --no-upload` on a Mac at the released commit (no GitHub needed). A failure
    to mount a `.dmg` is `hdiutil`, which the script retries; re-run the job
    before suspecting the code.
  - *Check the files for real*: an `::error::` annotation names the archive and
    the check that failed: not a Mach-O 64-bit binary of the architecture in
    the name, a license file missing, an `Info.plist` version that is not the
    release's, a `warden --version` that is wrong, or `macos-build-info.txt`
    not matching the file. These are findings about the archives, not
    flakiness: fix them on `main` and release again. (A `.dmg` that will not
    mount may be `hdiutil` having a bad moment: re-run once.) A notice "No
    Rosetta" is not a failure: the x86_64 files were checked without being run.
  - *Timed out* (the limit is 90 minutes): re-run the job; if it times out
    again the build is too slow for the runner (the first runs have not
    been measured), and the source `local` is the way out.

  To run the job again, see "The workflow" below.
- **Switching the source of a tag that is already pushed**, for when the chosen
  one cannot deliver (the runner's macOS job keeps failing, or no Mac is at
  hand after all). The annotation is read at the start of the run, so cancel the
  run, make the tag again at the same commit with the other line, and push it
  anew (this is for a tag whose release is **not published**; a published tag
  stays as it is):

  ```sh
  gh run cancel <run id>                  # the run for the tag, if one is going
  git tag -f -a v0.2.0 -m "Warden 0.2.0" -m "macos: runner" 'v0.2.0^{commit}'
  git push --delete origin v0.2.0
  git push origin v0.2.0                  # starts the workflow again
  ```

  Use `macos: local` to switch the other way, then run `cargo dist-macos` on
  the Mac. Going from `local` to `runner`, delete the draft release the Mac
  made, so that it holds nothing of the old build. The tag stays on the same
  commit, so nothing else changes. (With no tag pushed yet, there is nothing to
  switch: pick the other `macos_source` when you start the release.)
- **The workflow**: nothing is published (the draft is checked before it goes
  public). If it was flaky, re-run the failed jobs (`gh run rerun <id> --failed`, or "Re-run failed jobs" on the run
  page). If the code must change: `git push --delete origin v<version>` and
  `git tag -d v<version>`, delete the draft release (its macOS files, if a Mac
  made them, were built from the old commit), fix it on `main`, let CI pass,
  release again.
- **After publishing**: don't move a published tag; release a patch.

## TODO

- **The GUI's runner.** The GUI is built on `ubuntu-22.04` (and
  `ubuntu-22.04-arm`) so that it needs only glibc 2.35. GitHub began retiring
  those images on 2026-09-17: brownouts (jobs that fail on purpose) on 2027-03-23,
  03-30, 04-06 and 04-13, gone on 2027-04-17. Before then, build the GUI in a
  `container: ubuntu:22.04` on the 24.04 runners (the container needs a C
  toolchain, `jq`, `file` and `binutils` installed first), or accept glibc
  2.39 as the baseline, which would stop the GUI on Ubuntu 22.04 and Debian 12.
- **Package signing and a repository.** The `.deb` and `.rpm` files are
  unsigned and are downloads, not an apt or dnf repository
  ([`docs/packages.md`](packages.md)). Signing needs the owner's GPG key:
  nfpm signs both formats (`signature:` in `contrib/nfpm*.yaml`, key and
  passphrase as repository secrets), and the public key must be published
  somewhere users can fetch it.
- **The packages' maintainer.** Packages say `Warden authors
  <https://github.com/oceanwap/warden>`. Set the repository variable
  `WARDEN_PKG_MAINTAINER` (Settings → Secrets and variables → Actions →
  Variables) to `Name <address>` for a real one.
- **macOS notarization.** `Warden.app` is ad-hoc signed but not notarized,
  so Gatekeeper refuses the first open (docs/install.md says how to open
  it anyway), on either source of the archives. It needs the owner's Apple
  Developer ID. On the `local` source the certificate can stay in the Mac's
  keychain and no secrets go to CI; on the `runner` source it would have to
  be repository secrets (the certificate and the notarization credentials),
  which the job would import into a temporary keychain. The steps are in
  `scripts/dist-macos.sh` (`TODO(signing)`). The CLI installed by
  `install.sh` is not affected.

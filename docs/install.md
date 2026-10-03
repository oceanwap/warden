# Install

Warden is one program, `warden` (the CLI, the supervisor and wardend are the
same binary), plus an optional GUI, `warden-gui`. Releases carry both for
Linux and macOS on x86_64 and arm64. There are four ways in: the installer
script (this page, first), the release files by hand, a Linux package
([Linux packages](#linux-packages-deb-rpm)), and building from source.

- [The installer](#the-installer)
- [Options](#options) and [environment variables](#environment-variables)
- [Where things go](#where-things-go)
- [The GUI](#the-gui)
- [Release files](#release-files)
- [Linux packages (.deb, .rpm)](#linux-packages-deb-rpm)
- [By hand: checking and unpacking the files](#by-hand)
- [From source](#from-source)
- [Upgrading](#upgrading) and [uninstalling](#uninstalling)
- [Mirrors, offline hosts, a private repository](#mirrors-offline-hosts-a-private-repository)
- [How the download is checked](#how-the-download-is-checked)
- [Troubleshooting](#troubleshooting)

## The installer

```sh
curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh
```

`install.sh` works out your OS and CPU, downloads the matching archive from
the latest release together with the release's `SHA256SUMS`, checks the
archive against it, and puts `warden` in `/usr/local/bin` (as root) or
`~/.local/bin`. If that folder is not on your `PATH` it prints the line to add
for your shell. Nothing else on the machine is touched, and it never asks
anything or runs `sudo` by itself.

Both of these serve the script (they work once the repository is public and
a release has been published; until then see
[a private repository](#mirrors-offline-hosts-a-private-repository), or
[build from source](#from-source)):

| URL | Is |
|---|---|
| `https://github.com/oceanwap/warden/releases/latest/download/install.sh` | the script published with the newest release (not a pre-release) |
| `https://raw.githubusercontent.com/oceanwap/warden/main/install.sh` | the script on `main`, whatever the newest release is |

Options go after `sh -s --` when the script comes through a pipe:

```sh
curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh -s -- --gui --version v0.2.0
```

Or download it first, read it, and run it:

```sh
curl -fsSLO https://github.com/oceanwap/warden/releases/latest/download/install.sh
sh install.sh --dry-run          # what it would do
sh install.sh --gui
```

For the GUI there are two shortcuts: `warden gui-install` (for an installed
`warden`, the GUI of the same version) and `install-gui.sh`, the same installer
with `--gui`, from the same release:

```sh
curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install-gui.sh | sh
```

**Safe under `curl | sh`.** The script never reads its standard input (it has
no prompts), every command is inside a function that the last line calls with
an end marker, and a script that is cut short anywhere, even just before that
last line, refuses to run (`this script is incomplete`) instead of running
half of itself. It is POSIX `sh` (dash, bash, the macOS `/bin/sh`, busybox),
checked with ShellCheck, and `scripts/test-install.sh` runs it against fake
releases (no network, no root).

**What you need**

| | |
|---|---|
| OS and CPU | Linux or macOS (Windows: use WSL2, which counts as Linux: [Platforms](platforms.md)); x86_64 or arm64 (`aarch64` is the same as `arm64`) |
| Linux libc | glibc 2.28 or newer for the CLI (RHEL 8, Debian 10, Ubuntu 18.10 and newer); glibc 2.35 or newer for the GUI. musl (Alpine) has no build: see [Troubleshooting](#troubleshooting) |
| macOS | macOS 11 or newer (the minimum `Warden.app` declares) |
| Tools | `curl` or `wget`; `sha256sum`, `shasum` or `openssl`; `tar`, `awk`, `sed`, `grep`, `mktemp`. macOS has them all; `--gui` on a Mac also uses `ditto` or `unzip` (and `hdiutil` for the disk image) |

## Options

| Option | Does |
|---|---|
| `--version VERSION` | Install this release (`v0.2.0` or `0.2.0`) instead of the latest. Also `WARDEN_VERSION`; the flag wins. A pre-release (`0.2.0-rc.1`) is only ever installed this way |
| `--gui` | Also install the GUI: see [The GUI](#the-gui) |
| `--gui-only` | Install the GUI and leave the CLI as it is (what `warden gui-install` runs). On Linux with no `warden` yet, it installs the CLI from the GUI archive; a different version of the two gets a warning |
| `--uninstall` | Remove what the installer installs; `--uninstall --gui` removes the GUI too: see [Uninstalling](#uninstalling) |
| `--dir DIR`, `--prefix DIR` | The folder for the programs (the same as `WARDEN_INSTALL_DIR`). Created if missing |
| `--app-dir DIR` | macOS `--gui`: the folder for `Warden.app` (the same as `WARDEN_APP_DIR`) |
| `--modify-path` | If the folder is not on `PATH`, add it to your shell's startup file (see below). Without this flag **no file of yours is edited**: the line to add is printed |
| `--dry-run` | Print what would be done and install nothing. It still downloads `SHA256SUMS` (a small text file), to find the release's files and to tell you if there is none for your platform |
| `--no-verify` | Do not check the download against `SHA256SUMS`. A loud warning is printed. Discouraged: see [How the download is checked](#how-the-download-is-checked) |
| `-h`, `--help` | The options and variables |

An unknown option, or a missing value, exits with status 2 and installs
nothing. Exit status 1 is any other failure. Every download and check comes
before the first file is installed, so a failed download or a checksum
mismatch installs nothing, and a program that cannot run never replaces one
that works.

**`--modify-path`** adds two lines to one file, chosen by `$SHELL`:

| Shell | File | Line |
|---|---|---|
| bash | `~/.bashrc` (macOS: `~/.bash_profile`) | `export PATH="$HOME/.local/bin:$PATH"` |
| zsh | `${ZDOTDIR:-~}/.zshrc` | the same |
| fish | `~/.config/fish/conf.d/warden.fish` | `fish_add_path "$HOME/.local/bin"` |
| anything else | `~/.profile` | the same as bash |

(`$HOME/.local/bin` stands for the folder that was installed to.) A comment
above the line says where it came from. Running it again adds nothing, and
`--uninstall` takes exactly those two lines out again, leaving the rest of
the file as it was. Without the flag, the installer prints the same line and
the `echo '…' >> ~/.zshrc` that adds it, and changes nothing. In fish,
`fish_add_path "$HOME/.local/bin"` typed once at the prompt is enough.

## Environment variables

| Variable | Does |
|---|---|
| `WARDEN_VERSION` | The release to install, as `--version` (the flag wins) |
| `WARDEN_INSTALL_DIR` | The folder for the programs, as `--dir` (the flag wins) |
| `WARDEN_APP_DIR` | macOS `--gui`: the folder for `Warden.app`, as `--app-dir` |
| `WARDEN_DOWNLOAD_URL` | A folder holding the release files instead of GitHub: `https://…` or `file:///…`. See [Mirrors](#mirrors-offline-hosts-a-private-repository) |
| `XDG_DATA_HOME` | Linux `--gui`: where the menu entry and the icon go (default `~/.local/share`) |
| `SHELL`, `HOME`, `TMPDIR`, `PATH` | Read as usual: the shell for the hints, the user's home, where the download is unpacked, whether the folder is on `PATH` |
| `HTTPS_PROXY`, `NO_PROXY`, … | `curl` and `wget` use them as always |

## Where things go

| | As a user | As root |
|---|---|---|
| `warden` | `~/.local/bin/warden` | `/usr/local/bin/warden` |
| `warden-gui` (Linux, `--gui`) | next to `warden` | next to `warden` |
| Menu entry (Linux, `--gui`) | `~/.local/share/applications/warden-gui.desktop` | `/usr/local/share/applications/warden-gui.desktop` |
| Icon (Linux, `--gui`) | `~/.local/share/icons/hicolor/256x256/apps/warden.png` | `/usr/local/share/icons/hicolor/256x256/apps/warden.png` |
| `Warden.app` (macOS, `--gui`) | `/Applications` if you can write there (an administrator can), else `~/Applications` | `/Applications` |

`--dir`, `--app-dir` and `XDG_DATA_HOME` change those. A program is copied
next to its place, run once with `--version`, and renamed over the old file: a
running `warden` keeps its old file, nobody sees a half-written binary, and a
binary that cannot run on this system never replaces one that can.

The archive also holds the systemd units, the sysctl file and an nginx
example (`contrib/`). The installer does not copy them: `warden startup`
installs the units for you, with the path of the `warden` that ran it
([Production setup](production.md)).

## The GUI

`--gui` installs `warden` and `warden-gui`. Every GUI package carries the CLI:
the archives hold `warden` next to `warden-gui`, `Warden.app` has it inside (the
app offers to link it into your `PATH` on first launch), and the Linux
`warden-gui` packages ship `/usr/bin/warden` ([Packages](packages.md)).

**Linux.** `warden-gui` goes next to `warden`, the menu entry and the icon to
the folders above, so Warden shows up in the applications menu. The entry's
`Exec=` names the program by its full path (a menu starts programs with a
`PATH` of its own, which may lack `~/.local/bin`). The GUI needs glibc 2.35 or
newer and a desktop session (Wayland or X11); it draws on the CPU, so no GPU
driver is needed. It loads the display libraries at run time, which a desktop
has: `libxkbcommon`, then on Wayland `libwayland-client` and
`libwayland-cursor`, on X11 `libX11`, `libX11-xcb`, `libXcursor`, `libXi` and
`libxkbcommon-x11`. A server without them can use the CLI, and
`warden-gui --ssh user@host` from a desktop to reach it ([GUI](../gui/README.md)).

**macOS.** `Warden.app` (the GUI, with the CLI inside it) is unpacked from the
release's `.zip` (or from the `.dmg`, if a release has only that), checked
with `codesign --verify`, and put in `/Applications` or `~/Applications`,
replacing an older Warden.app of the same bundle id; a `Warden.app` that is
somebody else's is never touched. It also installs the `warden` CLI as usual.
Start the app from Launchpad, or `open -a Warden`.

The app is signed ad hoc, not notarized by Apple. A file fetched by `curl`
carries no quarantine flag, so macOS shows no first-open dialog for an app
installed this way. If you download the `.dmg` or the `.zip` in a browser
instead, macOS refuses the first open ("Apple could not verify…"): open
System Settings, Privacy & Security, and press Open Anyway (macOS 15 dropped
the right-click, Open shortcut), or run
`xattr -dr com.apple.quarantine /Applications/Warden.app`.

## Release files

Each release at <https://github.com/oceanwap/warden/releases> holds, for
`<os>` = `linux` or `macos` and `<arch>` = `x86_64` or `arm64`:

| File | Holds |
|---|---|
| `warden-<version>-<os>-<arch>.tar.gz` | The CLI: `warden`, `README.md`, the licenses and `contrib/` (systemd units, sysctl file, nginx site), in a folder of the same name |
| `warden-gui-<version>-linux-<arch>.tar.gz` | The GUI with the CLI: `warden-gui`, `warden`, `README.md`, `README-GUI.md`, the licenses, `contrib/`, `warden-gui.desktop` and `warden.png` |
| `warden-gui-<version>-macos-<arch>.zip` | `Warden.app` (GUI and CLI inside), ad-hoc signed |
| `Warden-<version>-macos-<arch>.dmg` | A disk image (volume `Warden`) with that `Warden.app` and a link to `/Applications`: drag one onto the other |
| `install.sh` | This installer, as of the release |
| `SHA256SUMS` | The SHA-256 of every other file above |
| `macos-build-info.txt` | The commit and checksums of the macOS files, which are built on a GitHub macOS runner or on a Mac ([Releasing](releasing.md)) |

`--version` takes the number as in the file names (`0.2.0`), with or without
the `v` of the tag.

## Linux packages (.deb, .rpm)

Every release also carries `.deb` and `.rpm` packages for Linux, for hosts
that want `apt` or `dnf` to know what is installed. See
[packages.md](packages.md). Use the packages or `install.sh`, not both at
once: with both installed, the one found first on the `PATH` runs.

## By hand

Download an archive and `SHA256SUMS` from the release page (or with `curl`,
`gh release download`, a browser), check the archive against its line in
`SHA256SUMS`, and copy the program to a folder on your `PATH`:

```sh
V=0.2.0; F=warden-$V-linux-x86_64          # macOS: warden-$V-macos-arm64 (Apple silicon) or -x86_64
B=https://github.com/oceanwap/warden/releases/download/v$V
curl -fsSLO $B/$F.tar.gz
curl -fsSLO $B/SHA256SUMS
grep " $F.tar.gz\$" SHA256SUMS | sha256sum -c    # macOS: shasum -a 256 -c    (prints "$F.tar.gz: OK")
tar -xzf $F.tar.gz
mkdir -p ~/.local/bin
install -m 0755 $F/warden ~/.local/bin/warden    # or: sudo install -m 0755 $F/warden /usr/local/bin/warden
warden --version
```

`sha256sum -c` prints `FAILED` and exits 1 if the file does not match; then
do not use it. (`sha256sum -c SHA256SUMS --ignore-missing` checks every
file you downloaded at once, where your `sha256sum` has `--ignore-missing`.)

The GUI by hand:

- **Linux:** unpack `warden-gui-<version>-linux-<arch>.tar.gz`; put
  `warden-gui` next to `warden`; copy `warden-gui.desktop` to
  `~/.local/share/applications/` and change its `Exec=warden-gui` to the full
  path of the program; copy `warden.png` to
  `~/.local/share/icons/hicolor/256x256/apps/warden.png`.
- **macOS:** open the `.dmg` and drag `Warden.app` onto `Applications`; or
  unzip the `.zip` and move `Warden.app` to `/Applications` (use
  `ditto -x -k <zip> <folder>`, not a tool that drops attributes, so the
  signature stays valid). Then see the first-open note in [The GUI](#the-gui).

## From source

Needs Rust: 1.85 or newer for the CLI, 1.88 for the GUI (the releases are
built with the toolchain pinned in `.github/workflows/release.yml`). Warden is
not on crates.io.

```sh
git clone https://github.com/oceanwap/warden.git && cd warden
cargo build --release                       # target/release/warden
cargo build --release -p warden-gui         # target/release/warden-gui (optional)
cargo install --path .                      # or: put warden in ~/.cargo/bin
```

On a Mac, `scripts/make-app.sh` (after building both programs) makes
`target/Warden.app` with the icon, signed ad hoc; `scripts/make-app.sh
~/Applications` puts it in your applications folder. This is also the way to
run the latest `main`, or a release that has not been published yet. See
[Development](development.md) for the toolchain and the tests.

## Upgrading

Run the installer again. It replaces `warden` (and the GUI, with `--gui`) and
tells you what it replaced:

```sh
curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh
sh install.sh --version v0.2.0        # a specific release, also an older one
```

**The apps keep running the old version until their supervisors restart.**
wardend (the host daemon, which `warden start` starts in the background
unless `WARDEN_NO_DAEMON=1`) and every app's supervisor are processes that keep
the code they started with: a new file on disk does not reach them. If wardend is running,
the installer says so at the end. To move everything to the new binary:

```sh
warden update
```

`warden update` is `warden save`, then stopping every supervisor and wardend,
then `warden resurrect`: the apps stop for a few seconds (it asks first on a
terminal; `--yes` skips that). If the save fails, nothing is stopped. Do it when that is acceptable, or schedule it like a deploy; until
then the old supervisors keep your apps up as before. A release from before
`warden update` existed gets the `save`, `kill --yes` and `resurrect`
commands in the note instead.

## Uninstalling

```sh
sh install.sh --uninstall            # warden
sh install.sh --uninstall --gui      # and the GUI: warden-gui, menu entry and icon, or Warden.app
```

Add `--dir DIR` (and `--app-dir DIR`) if you installed somewhere else; the
same defaults are used otherwise. `--dry-run` shows what would go. It removes:

- the `warden` in the folder, **only if it answers `--version` as Warden** (a
  program of another name or origin stays, with a warning);
- with `--gui`: `warden-gui` (same check), its menu entry (only if its `Exec=`
  names `warden-gui`) and the icon; on macOS `Warden.app` in `/Applications`
  and `~/Applications`, only where its bundle id is `io.github.oceanwap.warden`;
- the PATH lines `--modify-path` added, and nothing else in those files.

It does not remove, and says so: your apps' configs and saved state
(`~/.config/warden` and `~/.local/state/warden`, or `/etc/warden` and
`/var/lib/warden` as root), the systemd units or launchd job that `warden
startup` made (`warden unstartup` removes them: **run it before uninstalling**,
since it needs the program), the logs, and anything running. wardend and the
supervisors keep running after their program file is gone; stop them first
with `warden kill --yes` (the uninstaller warns if wardend is up).

A program installed by hand or from source: delete the files you put there
(`rm ~/.local/bin/warden`, `rm -r /Applications/Warden.app`, `cargo uninstall
warden`).

## Mirrors, offline hosts, a private repository

`WARDEN_DOWNLOAD_URL` points the installer at a folder that holds the
release's files (`SHA256SUMS` and the archives, under their release names)
instead of GitHub. The folder can be a web server in your network or a local
directory:

```sh
WARDEN_DOWNLOAD_URL=https://mirror.example.com/warden/v0.2.0 sh install.sh
WARDEN_DOWNLOAD_URL=file:///srv/warden-release sh install.sh --version 0.2.0
```

With no `--version` the version is the one in the file names of that folder,
so a folder must hold one version only. This is also how CI tests an
unreleased build (`.github/workflows/release.yml` serves its own artifacts
this way) and how a host with no internet gets Warden: copy the files over
and use `file://`.

**While the repository is private** the release URLs answer 404 to anyone not
logged in, so the one-liner does not work. With the GitHub CLI logged in:

```sh
gh release download v0.2.0 --repo oceanwap/warden --dir /tmp/warden-release
WARDEN_DOWNLOAD_URL=file:///tmp/warden-release sh /tmp/warden-release/install.sh --version 0.2.0
```

## How the download is checked

The installer fetches `SHA256SUMS` and the archive from the same place, and
refuses the archive unless its SHA-256 is the one listed. That catches a
damaged or truncated download, a stale cache and a mirror that is out of step.
It cannot catch a server that serves a bad archive *and* a matching
`SHA256SUMS`: for that, the HTTPS connection to GitHub is what you trust, as
with any release download. To check against a copy of the hashes from
elsewhere, compare by hand ([By hand](#by-hand)).

Everything is downloaded and checked before the first file is installed, so a
mismatch in the GUI archive stops the CLI install too. `--no-verify` skips the
check and installs whatever arrives, and the installer says so loudly; use it
only for a mirror you trust that publishes no `SHA256SUMS` (and then also name
the version, since without `SHA256SUMS` the installer cannot tell which file
is the latest).

## Troubleshooting

| You see | Why | Fix |
|---|---|---|
| `cannot download …/SHA256SUMS` | No release has been published, the repository is private, you are offline or behind a proxy, or `--version` names a release that does not exist (the message then asks `is v… a Warden release?`) | Check `curl -fsSI https://github.com/oceanwap/warden/releases/latest/download/SHA256SUMS`. Behind a proxy set `HTTPS_PROXY`. Private repository: [above](#mirrors-offline-hosts-a-private-repository). Otherwise [build from source](#from-source) |
| `checksum mismatch for …` | The file was damaged on the way, or the server's files do not match its `SHA256SUMS` (a mirror out of date, a caching proxy). Nothing was installed | Run it again in a minute; try without the proxy or mirror. If it keeps happening, do not install: report it at <https://github.com/oceanwap/warden/issues>. `--no-verify` would install it anyway, which is the discouraged way round |
| `no linux-x86_64 archive in …/SHA256SUMS` (or `has no …tar.gz`) | That release has no build for this OS and CPU, or the mirror folder is incomplete | The message lists what the release does have. Another release (`--version`), or build from source |
| `this system uses musl libc (Alpine?)` | The Linux archives are linked against glibc (2.28 and newer); there is no musl build | A glibc image or host (Debian, Ubuntu, Fedora, RHEL 8+), or build from source with `cargo` |
| `glibc 2.17 is too old` | The CLI needs glibc 2.28+ (the GUI 2.35+) | A newer distribution, or build from source. For the GUI only, install without `--gui` |
| `cannot tell which C library this system uses` (a warning) | `getconf` and `ldd` say nothing useful | It goes on assuming glibc. If the program then does not run, see the next row |
| `the warden binary does not run on this system` | The downloaded program failed `--version`: a libc that is too old, or a folder mounted `noexec` | Nothing was replaced. Another `--dir` (not on a `noexec` mount), or [build from source](#from-source) |
| `cannot write to /usr/local/bin` or `cannot create …` | The folder is not yours | `sudo sh install.sh`, or a folder you own: `--dir ~/.local/bin` |
| `… is not on your PATH, so "warden" is not found yet` | The install folder is not in `PATH` (normal for `~/.local/bin` on macOS and some Linux) | Run the line it prints, or `--modify-path` next time ([Options](#options)) |
| `… comes before … on your PATH, so a plain warden still runs that one` | An older `warden` (a package, `cargo install`) is earlier on `PATH` | Remove it, or put the new folder first. `command -v warden` shows which one runs |
| `needs curl or wget`, `needs sha256sum, shasum or openssl`, `needs 'tar'` | A tool is missing (minimal containers) | Install it (`apt install curl`, `apk add curl`, `brew install curl`), or download the files yourself ([By hand](#by-hand)) |
| `this script is incomplete (was the download cut short?)` | The script was cut off on the way; nothing ran | Run the one-liner again |
| `wardend is running, and so are your apps' supervisors` | The note after an upgrade: they still run the old version | `warden update` ([Upgrading](#upgrading)) |
| `…/Warden.app exists and is not Warden's` | Another application called Warden is in the folder | Move it, or `--app-dir ~/Applications` |
| `the Warden.app in … fails codesign --verify` | The unpacked app's signature is broken | Run it again; with `ditto` missing, install the command line tools. If it persists, report it |
| macOS: "Apple could not verify Warden.app" | The app is not notarized; a browser-downloaded copy is quarantined | [The GUI](#the-gui): Open Anyway, or `xattr -dr com.apple.quarantine /Applications/Warden.app` |
| Linux: `warden-gui` does not start, or names a missing library | A display library is missing, or there is no desktop session (an SSH shell, a container) | Install the libraries listed in [The GUI](#the-gui); from a server use `warden-gui --ssh` on a desktop |
| The GUI is not in the applications menu | A menu may read new entries only at login | Log out and in, or `update-desktop-database ~/.local/share/applications`; start `warden-gui` meanwhile |

More about running Warden itself: [Troubleshooting](troubleshooting.md),
`warden doctor`.

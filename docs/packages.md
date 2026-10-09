# Linux packages

Every [GitHub Release](https://github.com/oceanwap/warden/releases) carries
`.deb` and `.rpm` packages for Linux x86_64 and arm64, built from the very
binaries of the `.tar.gz` archives (so they need the same glibc 2.28 or newer:
Debian 10, Ubuntu 18.10, RHEL 8) and checked in `SHA256SUMS` like every other
file. Two packages:

| Package | Holds | Needs |
|---|---|---|
| `warden` | the CLI, `/usr/bin/warden` (wardend is `warden wardend`), the licenses and examples | glibc 2.28 |
| `warden-gui` | the desktop GUI, `/usr/bin/warden-gui`, its menu entry and icons, **and the CLI** (the same `/usr/bin/warden`) | glibc 2.35 (Ubuntu 22.04, Debian 12, Fedora 36, ...), a desktop session |

`warden-gui` is the whole of Warden for a desktop: it contains the CLI, provides
`warden` and replaces the `warden` package, so install one of the two, not both.
Installing `warden-gui` over `warden` swaps them (apt and dnf do it without
asking; with `dpkg -i` or `rpm -U` too), and removing `warden-gui` removes the
CLI with it. To go back to the CLI alone, install `warden` again.

The packages are for hosts that want `apt` or `dnf` to know what is installed
and where it came from. `install.sh` and the archives (docs/install.md) do
the same job without root and on any distribution; use whichever you like, but
not both at once (see "Upgrade").

## Install

The file names carry the release's version (`0.1.0` below) and the
architecture: `amd64` or `arm64` for `.deb`, `x86_64` or `aarch64` for `.rpm`.

```sh
# Debian, Ubuntu
curl -fsSLO https://github.com/oceanwap/warden/releases/download/v0.1.0/warden_0.1.0-1_amd64.deb
sudo apt install ./warden_0.1.0-1_amd64.deb

# Fedora, RHEL, Rocky, Alma
curl -fsSLO https://github.com/oceanwap/warden/releases/download/v0.1.0/warden-0.1.0-1.x86_64.rpm
sudo dnf install ./warden-0.1.0-1.x86_64.rpm
```

`apt install ./file.deb` and `dnf install ./file.rpm` fetch the dependencies
(the C library, and for the GUI the icon theme and the display libraries).
The CLI package needs only the C library, so `sudo dpkg -i` and `sudo rpm -i`
work for it too, without a package manager's network. The GUI the same way,
with or without `warden` installed:

```sh
sudo apt install ./warden-gui_0.1.0-1_amd64.deb      # or: sudo dnf install ./warden-gui-0.1.0-1.x86_64.rpm
```

To check a download against the release's checksums (put the files next to
`SHA256SUMS`):

```sh
sha256sum -c SHA256SUMS --ignore-missing
```

The packages are not signed yet (see the TODO at the end), so that checksum
file, fetched from the release page over HTTPS, is the check there is.

A pre-release (`0.2.0-rc.1`) has the same file names with its version
(`warden_0.2.0-rc.1-1_amd64.deb`); inside, the package says `0.2.0~rc.1`,
which `apt` and `dnf` order before `0.2.0`.

## What is in the packages

**warden**

| Path | What |
|---|---|
| `/usr/bin/warden` | the CLI and wardend, one binary |
| `/usr/share/doc/warden/` | `README.md`, `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD-PARTY-LICENSES.txt` |
| `/usr/share/doc/warden/examples/` | `warden.example.toml`, `wardend.service`, `warden@.service`, `99-warden.conf`, `nginx.conf`: to read and copy, never active |

**warden-gui** (the CLI's files above, and)

| Path | What |
|---|---|
| `/usr/bin/warden-gui` | the GUI |
| `/usr/share/applications/warden-gui.desktop` | the menu entry (the same file as in the GUI archive) |
| `/usr/share/icons/hicolor/<size>/apps/warden.png`, `.../scalable/apps/warden.svg` | the icon, 16 to 512 px and scalable |
| `/usr/share/doc/warden-gui/` | `README.md` (the GUI's), the licenses, `THIRD-PARTY-LICENSES-GUI.txt`, `FONT-LICENSES.txt` |

The GUI draws on the CPU and loads the display libraries (`libxkbcommon`, and
Wayland's or X11's) when its window opens, which a dependency scanner cannot
see: they are listed as recommendations, which every desktop already has.

There is no man page and there are no shell completions: Warden has none (`warden --help`).

## What installing does not do

A package installs files and runs nothing else. This is on purpose, and tested
(`scripts/test-linux-packages.sh` fails a package that breaks any of it):

- **It starts and enables nothing.** There is no post-install script. No
  app, no wardend, no unit is running or enabled after `apt install`.
  `sudo warden startup` is the step that brings the apps back after a reboot
  ([`production.md`](production.md)): it writes `warden@.service` and
  `wardend.service` to `/etc/systemd/system` with the path of the binary that
  ran it (`/usr/bin/warden` here) and the right environment, and enables them
  for the saved apps.
- **It ships no systemd unit.** The units in `contrib/` are examples under
  `/usr/share/doc/warden/examples/`, not in `/usr/lib/systemd/system`, for two
  reasons. `warden startup` generates its own, which override a packaged one
  anyway. And Warden counts a `warden@.service` or `wardend.service` in
  `/usr/lib/systemd/system` as "startup is set up" (`has_unit` in
  `src/fleet.rs`): `warden start` would then leave wardend to systemd, and
  `warden doctor` would say the saved apps come back after a reboot when
  nothing was enabled.
- **It does not change kernel settings.** `contrib/99-warden.conf`
  (`net.ipv4.tcp_migrate_req = 1`, so a rolling restart does not reset queued
  connections) is an example too; `warden startup` installs it as
  `/etc/sysctl.d/99-warden.conf` and applies it, and `warden doctor` warns
  while it is off. To set it without `warden startup`:
  `sudo cp /usr/share/doc/warden/examples/99-warden.conf /etc/sysctl.d/ && sudo sysctl --system`.
- **It creates no user, no group and nothing under `/etc/warden`,
  `/var/lib/warden` or `/run/warden`.** Your app configs (`/etc/warden/<app>.toml`
  as root), saved state and logs are yours, and no package operation removes them.

Not done, and why:

- **Alpine (`.apk`).** The binaries link glibc (that is how they keep glibc's
  malloc and the `preadv2` static-file path, see `release.yml`), and Alpine is musl.
- **Arch Linux.** `scripts/dist-linux-packages.sh --formats archlinux` builds
  `warden-<version>-1-x86_64.pkg.tar.zst` (and the GUI's), whose layout was
  inspected but which has not been installed on Arch, so none is released.
- **AppImage, Flatpak, Snap.** Not now. The GUI is a client of the host's
  wardend socket under `/run/warden` and runs the host's `warden`; a sandbox
  would need holes for both, and an AppImage would carry copies of binaries
  that must match the host's `warden`. The `.tar.gz` and the packages cover it.
- **An apt or dnf repository.** The packages are downloads from the release
  page; nothing updates them by itself. Hosting a signed repository needs the
  owner's GPG key (TODO).

## Upgrade

Install the new package over the old one:

```sh
sudo apt install ./warden_0.2.0-1_amd64.deb         # the CLI alone, or
sudo apt install ./warden-gui_0.2.0-1_amd64.deb     # the GUI with the CLI
sudo dnf install ./warden-0.2.0-1.x86_64.rpm        # or ./warden-gui-0.2.0-1.x86_64.rpm; or: sudo rpm -U ...
```

The package replaces the binary on disk and restarts nothing: a running
supervisor keeps the code it started with. `warden list` and the GUI say when
one is older than the `warden` asking, and which numbers it lacks. To move
the host to the new binary, run

```sh
sudo warden update
```

which saves and moves every supervisor and wardend to the binary on disk. The
apps keep serving: their workers are handed to the new supervisors (an app
without a keeper is restarted instead, in parallel with the others, one per
CPU core at a time; `--parallel` changes that). The GUI has the same under
Settings, "Restart everything".

Units that `warden startup` wrote name the binary's path: `/usr/bin/warden` for
a package, so an upgrade keeps them valid. If you move from `install.sh`
(`/usr/local/bin/warden`, or `~/.local/bin`) to the package, run `sudo warden
startup` again from the packaged binary to rewrite them, and remove the old
binary. With both installed, the one found first on the PATH runs.

## Uninstall

```sh
sudo warden unstartup                  # if you ran `warden startup`: removes the units it wrote
sudo warden kill --yes                 # if apps are still running and should stop
sudo apt remove warden-gui             # or warden: whichever is installed (dnf the same)
```

Removing leaves your configs, saved state, logs and anything `warden startup`
wrote. If it finds the units `warden startup` wrote still in
`/etc/systemd/system`, the package says so while it is removed (they would
run a binary that is gone, so apps would not come back after the next boot)
and changes nothing: it stops no app and deletes nothing. If the binary is
already gone, install the package again to run `warden unstartup`, or
`systemctl disable --now wardend.service`, `systemctl disable warden@<app>` for
each app, and delete the unit files.

## Building them yourself

The recipes are [`contrib/nfpm.yaml`](../contrib/nfpm.yaml) (`warden`) and
[`contrib/nfpm-gui.yaml`](../contrib/nfpm-gui.yaml) (`warden-gui`) for
[nfpm](https://nfpm.goreleaser.com), run by `scripts/dist-linux-packages.sh`.
The release workflow uses nfpm 2.47.0.

```sh
go install github.com/goreleaser/nfpm/v2/cmd/nfpm@v2.47.0
cargo build --release --locked           # or the release's binaries: warden, warden-gui in one directory
scripts/dist-linux-packages.sh --notices <dir> target/release 0.1.0 x86_64     # → dist/*.deb, dist/*.rpm, dist/SHA256SUMS
scripts/test-linux-packages.sh dist      # inspection (dpkg-deb, rpm), then installation in containers
```

`--notices` is a directory with `THIRD-PARTY-LICENSES.txt` (and
`THIRD-PARTY-LICENSES-GUI.txt` for the GUI), the output of `cargo about
generate` as in the release workflow's `licenses` job; `--no-gui` skips the GUI
package. The binaries' `--version` must say the version you pass. The same
inputs give the same package, byte for byte (the files are stamped with the
last commit's time, or `$SOURCE_DATE_EPOCH`). The test script installs each
package in Debian 12 and 11, Ubuntu 22.04 and 24.04, Fedora and Rocky Linux 8
containers (docker or podman) and, where the C library is too old for the GUI,
checks that the GUI package is refused; without a container runtime it does the
inspection only and says so.

## TODO

- **Sign the packages** (TODO(owner)). nfpm signs `.deb` and `.rpm` with a GPG
  key (`signature:` in the recipes); the key and its passphrase go in as
  repository secrets, and the public key must be published where users can
  fetch it, with the commands to trust it. Until then the checksums in
  `SHA256SUMS` are the only check. A repository (`apt`, `dnf`) is the step after.
- **The maintainer.** Packages say `Warden authors <https://github.com/oceanwap/warden>`;
  set the repository variable `WARDEN_PKG_MAINTAINER` to `Name <address>`
  (see [`releasing.md`](releasing.md)).

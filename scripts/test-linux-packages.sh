#!/usr/bin/env bash
# Tests the Linux packages scripts/dist-linux-packages.sh wrote, in two steps.
#
# 1. Inspection, on this machine, with dpkg-deb and rpm (a step is skipped,
#    with a message, when its tool is missing): the control data and the file
#    list of every package. What it holds, with the right owner and mode; what
#    it must NOT hold: no systemd unit, no sysctl file, nothing under /etc or
#    /usr/local, no setuid file, and no maintainer script but the pre-removal
#    note (a package must not start, enable or configure anything). warden-gui
#    holds everything warden does (the same /usr/bin/warden) and takes its
#    place: provides warden, conflicts with and replaces it (rpm: obsoletes).
# 2. Installation, in containers (docker or podman; skipped with a message when
#    there is none): in each image `dpkg -i` or `rpm -i` the warden package,
#    `warden --version`, `warden doctor`, a reinstall; then warden-gui over it
#    (apt or dnf, then dpkg or rpm: warden is replaced, the CLI stays, and only
#    the notes that should print do), a reinstall, its removal (the CLI goes
#    with it), the warden package back in its place, the removals, which must
#    leave nothing behind, and warden-gui alone on a clean system. On images
#    too old for the GUI (glibc < 2.35) warden-gui must be refused, leaving the
#    CLI as it was.
#
# Usage: scripts/test-linux-packages.sh [OPTIONS] [DIST_DIR]
#
#   DIST_DIR   where the packages are (default: dist); the ones for this
#              machine's architecture are tested
#
# Options:
#   --version V        the version the binaries report (default: from the
#                      package file names)
#   --image F:IMAGE:G  F is deb or rpm, G is yes (the GUI installs there) or no
#                      (it must be refused). Repeat to replace the default list:
#                        deb:debian:12:yes deb:debian:11:no deb:ubuntu:22.04:yes
#                        deb:ubuntu:24.04:yes rpm:fedora:latest:yes rpm:rockylinux:8:no
#                      (rockylinux:8 and debian:11 are the oldest the CLI claims
#                      to run on: glibc 2.28 and 2.31)
#   --runtime NAME     docker or podman (default: the first that answers)
#   --static-only      step 1 only
#   --require          a missing container runtime, dpkg-deb or rpm is a
#                      failure, not a skip (the release workflow passes it)
#   -h, --help
#
# Exit status: 0 when everything that ran passed.
set -uo pipefail

usage() { sed -n '2,/^set -uo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }

dist=dist version="" runtime="" static_only=0 require=0
images=()
while [ $# -gt 0 ]; do
  case "$1" in
    --version) version=${2:?--version needs a value}; shift 2 ;;
    --image) images+=("${2:?--image needs FAMILY:IMAGE:GUI}"); shift 2 ;;
    --runtime) runtime=${2:?--runtime needs docker or podman}; shift 2 ;;
    --static-only) static_only=1; shift ;;
    --require) require=1; shift ;;
    -h | --help) usage; exit 0 ;;
    -*) echo "test-linux-packages: unknown option $1 (--help lists them)" >&2; exit 2 ;;
    *) dist=$1; shift ;;
  esac
done
[ "${#images[@]}" -gt 0 ] || images=(deb:debian:12:yes deb:debian:11:no deb:ubuntu:22.04:yes deb:ubuntu:24.04:yes rpm:fedora:latest:yes rpm:rockylinux:8:no)

[ -d "$dist" ] || { echo "test-linux-packages: $dist is not a directory" >&2; exit 2; }
dist=$(cd "$dist" && pwd)

case "$(uname -m)" in
  x86_64 | amd64) debarch=amd64 rpmarch=x86_64 ;;
  aarch64 | arm64) debarch=arm64 rpmarch=aarch64 ;;
  *) echo "test-linux-packages: no packages for $(uname -m)" >&2; exit 2 ;;
esac

failures=0
pass() { echo "  ok: $*"; }
# In GitHub Actions a failure is also an annotation: job logs need a login to
# read, annotations don't.
annotate() { # text
  [ "${GITHUB_ACTIONS:-}" = true ] || return 0
  local t=${1//%/%25}
  t=${t//$'\r'/%0D}
  echo "::error title=test-linux-packages::${t//$'\n'/%0A}"
}
fail() { echo "  FAIL: $*" >&2; failures=$((failures + 1)); annotate "$*"; }
skip() { # message: a skip, or a failure with --require
  if [ "$require" = 1 ]; then fail "$* (--require)"; else echo "  skip: $*"; fi
}
one() { # glob: the single file it matches, or nothing
  local f
  for f in $1; do [ -f "$f" ] && { echo "$f"; return 0; }; done
  return 1
}

deb_cli=$(one "$dist/warden_*_${debarch}.deb") || deb_cli=""
deb_gui=$(one "$dist/warden-gui_*_${debarch}.deb") || deb_gui=""
rpm_cli=$(one "$dist/warden-[0-9]*.${rpmarch}.rpm") || rpm_cli=""
rpm_gui=$(one "$dist/warden-gui-[0-9]*.${rpmarch}.rpm") || rpm_gui=""
if [ -z "$deb_cli$rpm_cli" ]; then
  echo "test-linux-packages: no warden package for $debarch/$rpmarch in $dist (scripts/dist-linux-packages.sh writes them)" >&2
  exit 2
fi

# What the file names say: warden_0.2.0-rc.1-1_amd64.deb, warden-0.2.0-rc.1-1.x86_64.rpm:
# the version, as the binaries report it, then the package revision after the
# last hyphen. Inside the packages a pre-release is spelled 0.2.0~rc.1.
if [ -n "$deb_cli" ]; then
  evr=${deb_cli##*/warden_}
  evr=${evr%%_*}
else
  evr=${rpm_cli##*/warden-}
  evr=${evr%."$rpmarch".rpm}
fi
release=${evr##*-}
[ -n "$version" ] || version=${evr%-*}
pkgver=${version/-/\~}
echo "packages in $dist for $debarch/$rpmarch: version $version, revision $release"

# ---------------------------------------------------------------- inspection

# Paths (one per line) that must be in a package, and prefixes that must not.
cli_paths='./usr/bin/warden
./usr/share/doc/warden/LICENSE-MIT
./usr/share/doc/warden/LICENSE-APACHE
./usr/share/doc/warden/THIRD-PARTY-LICENSES.txt
./usr/share/doc/warden/README.md
./usr/share/doc/warden/examples/warden.example.toml
./usr/share/doc/warden/examples/wardend.service
./usr/share/doc/warden/examples/warden@.service
./usr/share/doc/warden/examples/99-warden.conf
./usr/share/doc/warden/examples/nginx.conf'
# The GUI's own: warden-gui has these AND every one of warden's.
gui_paths='./usr/bin/warden-gui
./usr/share/applications/warden-gui.desktop
./usr/share/icons/hicolor/16x16/apps/warden.png
./usr/share/icons/hicolor/32x32/apps/warden.png
./usr/share/icons/hicolor/64x64/apps/warden.png
./usr/share/icons/hicolor/128x128/apps/warden.png
./usr/share/icons/hicolor/256x256/apps/warden.png
./usr/share/icons/hicolor/512x512/apps/warden.png
./usr/share/icons/hicolor/scalable/apps/warden.svg
./usr/share/doc/warden-gui/LICENSE-MIT
./usr/share/doc/warden-gui/LICENSE-APACHE
./usr/share/doc/warden-gui/THIRD-PARTY-LICENSES-GUI.txt
./usr/share/doc/warden-gui/FONT-LICENSES.txt
./usr/share/doc/warden-gui/README.md'
forbidden='^\./(etc|usr/local|lib/systemd|usr/lib/systemd|lib/sysctl\.d|usr/lib/sysctl\.d|var|opt|srv)(/|$)'

# check_files LABEL EXPECTED-PATHS LISTING
#   LISTING: lines "MODE OWNER GROUP PATH" (MODE as ls prints it, drwxr-xr-x)
check_files() {
  local label=$1 want=$2 listing=$3 p bad
  while IFS= read -r p; do
    awk -v p="$p" '{ q = $4; sub(/\/$/, "", q); if (q == p) found = 1 } END { exit !found }' <<<"$listing" ||
      fail "$label: $p is missing"
  done <<<"$want"
  bad=$(re=$forbidden awk '$4 ~ ENVIRON["re"] { print $4 }' <<<"$listing")
  [ -z "$bad" ] || fail "$label: files a package must not install: $(tr '\n' ' ' <<<"$bad")"
  bad=$(awk '$2 != "root" || $3 != "root" { print $4 " (" $2 ":" $3 ")" }' <<<"$listing")
  [ -z "$bad" ] || fail "$label: not owned by root: $(tr '\n' ' ' <<<"$bad")"
  bad=$(awk '$1 ~ /[sStT]/ { print $4 " (" $1 ")" }' <<<"$listing")
  [ -z "$bad" ] || fail "$label: setuid, setgid or sticky: $(tr '\n' ' ' <<<"$bad")"
  bad=$(awk '$1 ~ /^-/ && $1 != "-rwxr-xr-x" && $1 != "-rw-r--r--" { print $4 " (" $1 ")" }' <<<"$listing")
  [ -z "$bad" ] || fail "$label: unexpected file modes: $(tr '\n' ' ' <<<"$bad")"
  bad=$(awk '$1 == "-rwxr-xr-x" && $4 !~ /^\.\/usr\/bin\// { print $4 }' <<<"$listing")
  [ -z "$bad" ] || fail "$label: executable outside /usr/bin: $(tr '\n' ' ' <<<"$bad")"
}

inspect_deb() { # file name expected-paths [needs-gui-dependencies]
  local f=$1 name=$2 want=$3 label tmp field before=$failures
  label=${f##*/}
  tmp=$(mktemp -d)
  for field in Package:"$name" Version:"$pkgver-$release" Architecture:"$debarch"; do
    [ "$(dpkg-deb -f "$f" "${field%%:*}")" = "${field#*:}" ] || fail "$label: ${field%%:*} is '$(dpkg-deb -f "$f" "${field%%:*}")', expected '${field#*:}'"
  done
  dpkg-deb -f "$f" Maintainer Description >/dev/null || fail "$label: control data unreadable"
  # ls-style listing: -rw-r--r-- root/root 123 2026-10-02 08:28 ./path
  check_files "$label" "$want" "$(dpkg-deb -c "$f" | awk '{ split($2, o, "/"); print $1, o[1], o[2], $6 }')"
  dpkg-deb -e "$f" "$tmp/control"
  for script in preinst postinst postrm config triggers conffiles; do
    [ ! -e "$tmp/control/$script" ] || fail "$label: has a $script (a package must not start, enable or configure anything)"
  done
  if [ "$name" = warden-gui ] && command -v desktop-file-validate >/dev/null 2>&1; then
    dpkg-deb --fsys-tarfile "$f" | tar -xO ./usr/share/applications/warden-gui.desktop >"$tmp/warden-gui.desktop"
    desktop-file-validate "$tmp/warden-gui.desktop" || fail "$label: the desktop entry does not validate"
  fi
  # Both have the pre-removal note: /usr/bin/warden goes with either.
  [ -x "$tmp/control/prerm" ] || fail "$label: the pre-removal note (prerm) is missing"
  # It only prints, whatever it is called with.
  for args in remove upgrade failed-upgrade deconfigure "remove in-favour warden-gui 1.0-1"; do
    # shellcheck disable=SC2086 # the words are the arguments
    "$tmp/control/prerm" $args </dev/null >/dev/null 2>&1 || fail "$label: prerm $args failed"
  done
  rm -rf "$tmp"
  [ "$failures" = "$before" ] && pass "$label inspected"
}

inspect_rpm() { # file name expected-paths
  local f=$1 name=$2 want=$3 label got scripts before=$failures
  label=${f##*/}
  got=$(rpm -qp --queryformat '%{NAME} %{VERSION}-%{RELEASE} %{ARCH}' "$f" 2>/dev/null)
  [ "$got" = "$name $pkgver-$release $rpmarch" ] || fail "$label: name, version and architecture are '$got', expected '$name $pkgver-$release $rpmarch'"
  # mode as ls prints it: rpm gives 0100755 / 040755; make it -rwxr-xr-x / drwxr-xr-x
  check_files "$label" "$want" "$(rpm -qp --queryformat '[%{FILEMODES:perms} %{FILEUSERNAME} %{FILEGROUPNAME} .%{FILENAMES}\n]' "$f")"
  scripts=$(rpm -qp --scripts "$f" 2>/dev/null | grep -E '^[a-z]+ scriptlet' | cut -d' ' -f1 | sort | tr '\n' ' ')
  [ "$scripts" = "preuninstall " ] || fail "$label: scriptlets are '$scripts', expected only the pre-removal note (preuninstall)"
  rpm -qp --requires "$f" | grep -q '^glibc >= ' || fail "$label: no glibc requirement"
  [ "$failures" = "$before" ] && pass "$label inspected"
}

echo "== inspection"
if [ -n "$deb_cli" ]; then
  if command -v dpkg-deb >/dev/null 2>&1; then
    inspect_deb "$deb_cli" warden "$cli_paths"
    if [ -n "$deb_gui" ]; then
      inspect_deb "$deb_gui" warden-gui "$cli_paths
$gui_paths"
      label=${deb_gui##*/}
      # It takes warden's place: dpkg and apt remove warden when it is installed.
      for field in "Provides:warden (= $pkgver-$release)" Conflicts:warden Replaces:warden; do
        got=$(dpkg-deb -f "$deb_gui" "${field%%:*}")
        [ "$got" = "${field#*:}" ] || fail "$label: ${field%%:*} is '$got', expected '${field#*:}'"
      done
      dpkg-deb -f "$deb_gui" Depends | grep -q 'libc6 (>= 2.35)' || fail "$label: no libc6 (>= 2.35)"
      if dpkg-deb -f "$deb_gui" Depends | grep -qw warden; then fail "$label: depends on warden, but holds the CLI itself"; fi
      # Everything the warden package has, and the very same CLI.
      missing=$(comm -23 <(dpkg-deb -c "$deb_cli" | awk '{ print $6 }' | sort) <(dpkg-deb -c "$deb_gui" | awk '{ print $6 }' | sort))
      [ -z "$missing" ] || fail "$label: lacks what the warden package has: $(tr '\n' ' ' <<<"$missing")"
      [ "$(dpkg-deb --fsys-tarfile "$deb_cli" | tar -xO ./usr/bin/warden | sha256sum)" = "$(dpkg-deb --fsys-tarfile "$deb_gui" | tar -xO ./usr/bin/warden | sha256sum)" ] ||
        fail "$label: its /usr/bin/warden is not the warden package's"
    fi
    dpkg-deb -f "$deb_cli" Depends | grep -q 'libc6 (>= 2.28)' || fail "${deb_cli##*/}: no libc6 (>= 2.28)"
  else
    skip "dpkg-deb not found: the .deb files were not inspected"
  fi
else
  fail "no warden_*_${debarch}.deb in $dist"
fi
if [ -n "$rpm_cli" ]; then
  if command -v rpm >/dev/null 2>&1; then
    # rpm lists the files it owns, and directories too: they are not checked as files.
    inspect_rpm "$rpm_cli" warden "$cli_paths"
    if [ -n "$rpm_gui" ]; then
      inspect_rpm "$rpm_gui" warden-gui "$cli_paths
$gui_paths"
      label=${rpm_gui##*/}
      # It takes warden's place: dnf (and rpm -U) remove warden when it is installed.
      rpm -qp --provides "$rpm_gui" | grep -qx "warden = $pkgver-$release" || fail "$label: does not provide warden = $pkgver-$release"
      rpm -qp --obsoletes "$rpm_gui" | grep -qx "warden <= $pkgver-$release" || fail "$label: does not obsolete warden <= $pkgver-$release"
      got=$(rpm -qp --conflicts "$rpm_gui")
      [ -z "$got" ] || fail "$label: conflicts with $got (Obsoletes is what lets dnf swap the packages)"
      if rpm -qp --requires "$rpm_gui" | grep -q '^warden'; then fail "$label: requires warden, but holds the CLI itself"; fi
      missing=$(comm -23 <(rpm -qpl "$rpm_cli" | sort) <(rpm -qpl "$rpm_gui" | sort))
      [ -z "$missing" ] || fail "$label: lacks what the warden package has: $(tr '\n' ' ' <<<"$missing")"
    fi
  else
    skip "rpm not found: the .rpm files were not inspected"
  fi
else
  fail "no warden-*.${rpmarch}.rpm in $dist"
fi
if [ "$static_only" = 1 ]; then
  [ "$failures" = 0 ] && { echo "inspection passed"; exit 0; }
  echo "$failures check(s) failed" >&2
  exit 1
fi

# --------------------------------------------------------------- containers

echo "== installation in containers"
if [ -z "$runtime" ]; then
  for r in docker podman; do
    if command -v "$r" >/dev/null 2>&1 && "$r" info >/dev/null 2>&1; then runtime=$r; break; fi
  done
fi
if [ -z "$runtime" ]; then
  if command -v docker >/dev/null 2>&1 || command -v podman >/dev/null 2>&1; then
    skip "a container runtime is installed but does not answer (is its daemon running?): packages not installed in containers"
  else
    skip "no container runtime (docker or podman): packages not installed in containers"
  fi
  [ "$failures" = 0 ] && exit 0
  echo "$failures check(s) failed" >&2
  exit 1
fi
echo "  runtime: $runtime"

# The script run inside the image (sh, not bash: minimal images). It gets the
# family, the version, the package file names, and GUI: yes (the GUI package
# must install), no (it must be refused: glibc too old) or skip (not tested).
inside=$(cat <<'EOS'
set -eu
export DEBIAN_FRONTEND=noninteractive
say() { echo "    $*"; }
die() { echo "    FAIL: $*" >&2; exit 1; }
cli=/pkgs/$CLI_PKG
gui=/pkgs/$GUI_PKG
out=/tmp/out.txt

say "$(. /etc/os-release && echo "$PRETTY_NAME"), glibc $(ldd --version 2>&1 | sed -n '1s/.* //p')"

# The low-level tool and the package manager, for each family.
if [ "$FAMILY" = deb ]; then
  low_install() { dpkg -i "$1"; }
  low_remove() { dpkg -r "$1"; }
  pm_install() { apt-get install -y -qq "$1"; }
  installed() { [ "$(dpkg-query -W -f='${Status}' "$1" 2>/dev/null)" = "install ok installed" ]; }
  owner() { dpkg -S "$1" 2>/dev/null | sed 's/: .*//'; }
  refusal='libc6|Depends|unmet'
else
  low_install() { rpm -i "$1"; }
  low_remove() { rpm -e "$1"; }
  pm_install() { dnf install -y -q "$1"; }
  installed() { rpm -q --quiet "$1"; }
  owner() { rpm -qf --queryformat '%{NAME}\n' "$1" 2>/dev/null; }
  refusal='glibc|nothing provides|requires'
fi
run() { # what command...: runs it, its output in $out; on failure shows it and stops
  what=$1
  shift
  "$@" >"$out" 2>&1 || { cat "$out"; die "$what failed"; }
}
# An image that leaves /usr/share/doc out (Ubuntu's minimized ones: dpkg
# path-exclude) has no examples on disk; the inspection found them in the package.
docs_kept() { ! grep -qsx 'path-exclude=/usr/share/doc/\*' /etc/dpkg/dpkg.cfg.d/*; }

# Which package has the CLI now (warden or warden-gui), and that all of it works.
holds_cli() { # package
  if [ "$1" = warden ]; then other=warden-gui; else other=warden; fi
  installed "$1" || die "$1 is not installed"
  ! installed "$other" || die "$other is still installed (with $1)"
  [ "$(owner /usr/bin/warden)" = "$1" ] || die "/usr/bin/warden is $(owner /usr/bin/warden)'s, not $1's"
  # Found on PATH, maybe through a symlink (Fedora 42+: /usr/sbin is /usr/bin).
  w=$(command -v warden) || die "warden is not on PATH"
  [ "$(readlink -f "$w")" = /usr/bin/warden ] || die "warden is $w, not /usr/bin/warden"
  [ "$(warden --version)" = "warden $VERSION" ] || die "warden --version says '$(warden --version)'"
  if ! docs_kept; then :; elif [ ! -f /usr/share/doc/warden/examples/wardend.service ]; then die "the example unit is missing"; fi
  if [ "$1" = warden-gui ]; then
    [ "$(warden-gui --version)" = "warden-gui $VERSION" ] || die "warden-gui --version says '$(warden-gui --version)'"
    [ -f /usr/share/applications/warden-gui.desktop ] || die "no desktop entry"
    [ -f /usr/share/icons/hicolor/256x256/apps/warden.png ] || die "no icon"
    [ -f /usr/share/icons/hicolor/scalable/apps/warden.svg ] || die "no scalable icon"
  else
    [ ! -e /usr/bin/warden-gui ] || die "/usr/bin/warden-gui is still there"
    [ ! -e /usr/share/applications/warden-gui.desktop ] || die "the menu entry is still there"
  fi
  [ "$FAMILY" = deb ] || rpm -V "$1" || die "rpm -V $1: files changed after the install"
}
no_note() { # when
  ! grep -q 'this host was set up' "$out" || { cat "$out"; die "$1 printed a removal note, but /usr/bin/warden stays"; }
}
gone() { # when
  for f in /usr/bin/warden /usr/bin/warden-gui /usr/share/doc/warden /usr/share/doc/warden-gui \
           /usr/share/applications/warden-gui.desktop /usr/share/icons/hicolor/256x256/apps/warden.png; do
    [ ! -e "$f" ] || die "$f is still there after $1"
  done
}

# 1. The CLI package alone, with the package tool's lowest level.
low_install "$cli" >/dev/null
holds_cli warden
say "warden --version: $(warden --version)"
warden doctor >/tmp/doctor.txt 2>&1 || { cat /tmp/doctor.txt; die "warden doctor failed"; }
say "warden doctor: $(tail -n 1 /tmp/doctor.txt)"

# 2. Installing it started and enabled nothing.
for f in /etc/systemd/system/warden@.service /etc/systemd/system/wardend.service /etc/sysctl.d/99-warden.conf \
         /usr/lib/systemd/system/warden@.service /usr/lib/systemd/system/wardend.service /lib/systemd/system/wardend.service; do
  [ ! -e "$f" ] || die "the package installed $f"
done
[ ! -S /run/warden/wardend.sock ] || die "installing the package started wardend"
docs_kept || say "this image leaves /usr/share/doc out: the examples were checked in the package"

# 3. Installing the same version again (a repair) works.
if [ "$FAMILY" = deb ]; then dpkg -i "$cli" >/dev/null; else rpm -i --replacepkgs "$cli"; fi
holds_cli warden

# From here on the host looks set up by `sudo warden startup`: the removal
# notes print only when /usr/bin/warden really goes away.
mkdir -p /etc/systemd/system
: >/etc/systemd/system/wardend.service
[ "$FAMILY" = rpm ] || apt-get update -qq >/dev/null

# 4. warden-gui, which holds the CLI too and takes the warden package's place.
case "$GUI" in
  yes)
    # 4a. The package manager swaps them (apt removes warden first, so its
    #     note may print; dnf installs warden-gui first, and it must not).
    run "installing warden-gui over warden" pm_install "$gui"
    holds_cli warden-gui
    [ "$FAMILY" = deb ] || no_note "dnf replacing warden"
    say "warden-gui --version: $(warden-gui --version), and it replaced the warden package"
    # 4b. A reinstall (a repair) keeps it all, quietly.
    if [ "$FAMILY" = deb ]; then run "reinstalling warden-gui" dpkg -i "$gui"; else run "reinstalling warden-gui" rpm -i --replacepkgs "$gui"; fi
    no_note "a reinstall"
    holds_cli warden-gui
    # 4c. Removing it removes the CLI too, and says what that leaves behind.
    run "removing warden-gui" low_remove warden-gui
    grep -q 'warden-gui: this host was set up' "$out" || { cat "$out"; die "removing warden-gui did not print its note"; }
    gone "removing warden-gui"
    say "removing warden-gui took the CLI with it, and printed the note"
    # 4d. The low-level tool swaps them too: dpkg removes warden in favour of
    #     warden-gui, rpm -U obsoletes it. Quietly.
    low_install "$cli" >/dev/null
    if [ "$FAMILY" = deb ]; then
      run "dpkg -i warden-gui over warden" dpkg -i "$gui"
      grep -q 'in favour of warden-gui' "$out" || { cat "$out"; die "dpkg did not remove warden in favour of warden-gui"; }
    else
      run "rpm -U warden-gui over warden" rpm -U "$gui"
    fi
    no_note "replacing warden"
    holds_cli warden-gui
    # 4e. And back to the CLI alone: apt removes warden-gui for warden; rpm
    #     needs warden-gui removed first (warden-gui obsoletes warden).
    if [ "$FAMILY" = deb ]; then
      run "installing warden over warden-gui" pm_install "$cli"
    else
      low_remove warden-gui >/dev/null
      low_install "$cli" >/dev/null
    fi
    holds_cli warden
    say "the low-level tool swaps them too, and warden takes warden-gui's place back"
    ;;
  no)
    if pm_install "$gui" >"$out" 2>&1; then die "warden-gui installed on a glibc that is too old"; fi
    say "warden-gui refused here, as it must be: $(grep -m1 -iE "$refusal" "$out" || tail -n 1 "$out")"
    [ ! -e /usr/bin/warden-gui ] || die "a refused install left /usr/bin/warden-gui"
    holds_cli warden
    ;;
esac

# 5. Removing the CLI says what `warden startup` left behind, and takes
#    everything (and only that).
run "removing warden" low_remove warden
grep -q '^warden: this host was set up' "$out" || { cat "$out"; die "removing warden did not print its note"; }
rm -f /etc/systemd/system/wardend.service
# A purge, for any conffiles (there are none: dpkg says the packages are gone already).
if [ "$FAMILY" = deb ]; then dpkg -P warden warden-gui >/dev/null 2>&1; fi
gone "the removal"
say "removed cleanly"

# 6. warden-gui alone, on a clean host: the CLI comes with it.
if [ "$GUI" = yes ]; then
  run "installing warden-gui alone" pm_install "$gui"
  holds_cli warden-gui
  run "removing warden-gui" low_remove warden-gui
  no_note "removing warden-gui from a host that was never set up"
  if [ "$FAMILY" = deb ]; then dpkg -P warden-gui >/dev/null 2>&1; fi
  gone "removing warden-gui"
  say "warden-gui alone brings the CLI, and takes it away again"
fi
EOS
)

for entry in "${images[@]}"; do
  family=${entry%%:*}
  rest=${entry#*:}
  gui=${rest##*:}
  image=${rest%:*}
  case "$family:$gui" in
    deb:yes | deb:no | rpm:yes | rpm:no) ;;
    *) fail "--image $entry: it is FAMILY:IMAGE:GUI, with deb or rpm and yes or no"; continue ;;
  esac
  if [ "$family" = deb ]; then cli_pkg=$deb_cli gui_pkg=$deb_gui; else cli_pkg=$rpm_cli gui_pkg=$rpm_gui; fi
  if [ -z "$cli_pkg" ]; then skip "$image: no .$family for this architecture"; continue; fi
  if [ -z "$gui_pkg" ]; then
    echo "  note: no warden-gui .$family here: the GUI is not tested on $image"
    gui=skip
  fi
  echo "  $image ($family, GUI: $gui)"
  out=$(mktemp)
  if "$runtime" run --rm -v "$dist:/pkgs:ro" \
    -e FAMILY="$family" -e VERSION="$version" -e GUI="$gui" \
    -e CLI_PKG="${cli_pkg##*/}" -e GUI_PKG="${gui_pkg##*/}" \
    "$image" sh -c "$inside" 2>&1 | tee "$out"; then
    pass "$image"
  else
    fail "$image:
$(tail -n 25 "$out")"
  fi
  rm -f "$out"
done

if [ "$failures" = 0 ]; then
  echo "all package tests passed"
  exit 0
fi
echo "$failures check(s) failed" >&2
exit 1

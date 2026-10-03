#!/usr/bin/env bash
# Tests the Linux packages scripts/dist-linux-packages.sh wrote, in two steps.
#
# 1. Inspection, on this machine, with dpkg-deb and rpm (a step is skipped,
#    with a message, when its tool is missing): the control data and the file
#    list of every package. What it holds, with the right owner and mode; what
#    it must NOT hold: no systemd unit, no sysctl file, nothing under /etc or
#    /usr/local, no setuid file, and no maintainer script but the pre-removal
#    note (a package must not start, enable or configure anything).
# 2. Installation, in containers (docker or podman; skipped with a message when
#    there is none): in each image `dpkg -i` or `rpm -i` the warden package,
#    `warden --version`, `warden doctor`, then the warden-gui package, a
#    reinstall, and the removal, which must leave nothing behind. On images too
#    old for the GUI (glibc < 2.35) the GUI package must be refused.
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
  if [ "$name" = warden ]; then
    [ -x "$tmp/control/prerm" ] || fail "$label: the pre-removal note (prerm) is missing"
    # It only prints, whatever it is called with.
    for arg in remove upgrade failed-upgrade deconfigure; do
      "$tmp/control/prerm" "$arg" </dev/null >/dev/null 2>&1 || fail "$label: prerm $arg failed"
    done
  else
    [ ! -e "$tmp/control/prerm" ] || fail "$label: has a prerm"
  fi
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
  if [ "$name" = warden ]; then
    [ "$scripts" = "preuninstall " ] || fail "$label: scriptlets are '$scripts', expected only the pre-removal note (preuninstall)"
  else
    [ -z "$scripts" ] || fail "$label: has scriptlets: $scripts"
  fi
  rpm -qp --requires "$f" | grep -q '^glibc >= ' || fail "$label: no glibc requirement"
  [ "$failures" = "$before" ] && pass "$label inspected"
}

echo "== inspection"
if [ -n "$deb_cli" ]; then
  if command -v dpkg-deb >/dev/null 2>&1; then
    inspect_deb "$deb_cli" warden "$cli_paths"
    if [ -n "$deb_gui" ]; then
      inspect_deb "$deb_gui" warden-gui "$gui_paths"
      dpkg-deb -f "$deb_gui" Depends | grep -q "warden (= $pkgver-$release)" || fail "${deb_gui##*/}: does not depend on warden (= $pkgver-$release)"
      dpkg-deb -f "$deb_gui" Depends | grep -q 'libc6 (>= 2.35)' || fail "${deb_gui##*/}: no libc6 (>= 2.35)"
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
      inspect_rpm "$rpm_gui" warden-gui "$gui_paths"
      rpm -qp --requires "$rpm_gui" | grep -qx "warden = $pkgver-$release" || fail "${rpm_gui##*/}: does not require warden = $pkgver-$release"
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

say "$(. /etc/os-release && echo "$PRETTY_NAME"), glibc $(ldd --version 2>&1 | sed -n '1s/.* //p')"

# 1. The CLI package alone, with the package tool's lowest level.
if [ "$FAMILY" = deb ]; then dpkg -i "$cli" >/dev/null; else rpm -i "$cli"; fi
[ "$(command -v warden)" = /usr/bin/warden ] || die "warden is not /usr/bin/warden"
[ "$(warden --version)" = "warden $VERSION" ] || die "warden --version says '$(warden --version)'"
say "warden --version: $(warden --version)"
warden doctor >/tmp/doctor.txt 2>&1 || { cat /tmp/doctor.txt; die "warden doctor failed"; }
say "warden doctor: $(tail -n 1 /tmp/doctor.txt)"

# 2. Installing it started and enabled nothing.
for f in /etc/systemd/system/warden@.service /etc/systemd/system/wardend.service /etc/sysctl.d/99-warden.conf \
         /usr/lib/systemd/system/warden@.service /usr/lib/systemd/system/wardend.service /lib/systemd/system/wardend.service; do
  [ ! -e "$f" ] || die "the package installed $f"
done
[ ! -S /run/warden/wardend.sock ] || die "installing the package started wardend"
[ -f /usr/share/doc/warden/examples/wardend.service ] || die "the example unit is missing"

# 3. The GUI package.
if [ "$FAMILY" = deb ]; then
  apt-get update -qq >/dev/null
  install_gui() { apt-get install -y -qq "$gui"; }
  refusal='libc6|Depends|unmet'
else
  install_gui() { dnf install -y -q "$gui"; }
  refusal='glibc|nothing provides|requires'
fi
case "$GUI" in
  yes)
    install_gui >/dev/null
    [ "$(warden-gui --version)" = "warden-gui $VERSION" ] || die "warden-gui --version says '$(warden-gui --version)'"
    say "warden-gui --version: $(warden-gui --version)"
    [ -f /usr/share/applications/warden-gui.desktop ] || die "no desktop entry"
    [ -f /usr/share/icons/hicolor/256x256/apps/warden.png ] || die "no icon"
    [ -f /usr/share/icons/hicolor/scalable/apps/warden.svg ] || die "no scalable icon"
    ;;
  no)
    if install_gui >/tmp/gui.txt 2>&1; then die "warden-gui installed on a glibc that is too old"; fi
    say "warden-gui refused here, as it must be: $(grep -m1 -iE "$refusal" /tmp/gui.txt || tail -n 1 /tmp/gui.txt)"
    [ ! -e /usr/bin/warden-gui ] || die "a refused install left /usr/bin/warden-gui"
    ;;
esac
if [ "$FAMILY" = rpm ]; then
  rpm -V warden || die "rpm -V warden: files changed after the install"
  [ "$GUI" != yes ] || rpm -V warden-gui || die "rpm -V warden-gui: files changed after the install"
fi

# 4. Installing the same version again (a repair) works.
if [ "$FAMILY" = deb ]; then dpkg -i "$cli" >/dev/null; else rpm -i --replacepkgs "$cli"; fi
[ "$(warden --version)" = "warden $VERSION" ] || die "warden broke on a reinstall"

# 5. Removal takes everything and only that.
if [ "$FAMILY" = deb ]; then
  if [ "$GUI" = yes ]; then dpkg -r warden-gui >/dev/null; fi
  dpkg -r warden >/dev/null
  dpkg -P warden >/dev/null
  if [ "$GUI" = yes ]; then dpkg -P warden-gui >/dev/null; fi
else
  if [ "$GUI" = yes ]; then rpm -e warden-gui; fi
  rpm -e warden
fi
for f in /usr/bin/warden /usr/bin/warden-gui /usr/share/doc/warden /usr/share/doc/warden-gui /usr/share/applications/warden-gui.desktop \
         /usr/share/icons/hicolor/256x256/apps/warden.png; do
  [ ! -e "$f" ] || die "$f is still there after the removal"
done
say "removed cleanly"
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

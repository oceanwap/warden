#!/usr/bin/env bash
# Tests install.sh without a network, a release or root: it builds fake release
# folders (archives made of stand-in programs, a SHA256SUMS) in a temp folder,
# points the installer at them with WARDEN_DOWNLOAD_URL (file:// and, where
# python3 and curl are there, a local http server), and runs it in a throw-away
# HOME. uname, getconf, ldd, sysctl, ditto, hdiutil and codesign are stubs, so
# the same run covers Linux and macOS, x86_64 and arm64, musl and old glibc on
# any machine; the last cases use the real platform (and the real warden binary,
# when one is built). Nothing outside the temp folder is written; it is removed
# at the end, a failing run keeps it.
#
#   bash scripts/test-install.sh          (CI runs it; no root needed, root is fine)
#
# install-gui.sh (install.sh --gui, from the release) is tested the same way,
# and so is `warden gui-install` (the install.sh built into warden) when a
# warden that has it is built here.
#
#   INSTALL_SH=…/install.sh INSTALL_GUI_SH=…/install-gui.sh   test other copies
#
# What it cannot prove: that hdiutil, ditto and codesign behave on a Mac (the
# release script, scripts/dist-macos.sh, runs install.sh --gui against the real
# ones there), or that GitHub serves what the URLs name.
# shellcheck disable=SC2016,SC2012 # literal $ in what is searched for and written; ls -A on folders we made
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
installer=${INSTALL_SH:-$root/install.sh}
gui_installer=${INSTALL_GUI_SH:-$root/install-gui.sh}
T=$(mktemp -d)
keep=0
srv=""
cleanup() {
    if [ -n "$srv" ]; then kill "$srv" 2>/dev/null || true; fi
    if [ "$keep" = 0 ]; then rm -rf "$T"; fi
}
trap cleanup EXIT
mkdir -p "$T/tmp" "$T/stage" "$T/bin"
for tool in zip unzip tar; do
    command -v "$tool" >/dev/null 2>&1 || { echo "test-install.sh needs $tool (it builds fake releases with it)"; exit 1; }
done

fails=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
note() { printf '  --    %s\n' "$1"; }
check() { # description command...
    local d=$1
    shift
    if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi
}
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# ----------------------------------------------------------- the stubs
# Each logs to $FAKE_LOG. The platform they report comes from FAKE_* variables.
bin=$T/bin
cat >"$bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo "${FAKE_OS:-Linux}" ;;
  -m) echo "${FAKE_ARCH:-x86_64}" ;;
  *) exec /usr/bin/uname "$@" ;;
esac
EOF
cat >"$bin/getconf" <<'EOF'
#!/bin/sh
if [ "$1" = GNU_LIBC_VERSION ]; then
  if [ "${FAKE_GLIBC:-2.35}" = none ]; then echo "getconf: GNU_LIBC_VERSION: unknown variable" >&2; exit 1; fi
  echo "glibc ${FAKE_GLIBC:-2.35}"
  exit 0
fi
exec /usr/bin/getconf "$@"
EOF
cat >"$bin/ldd" <<'EOF'
#!/bin/sh
if [ "${FAKE_LIBC:-glibc}" = musl ]; then
  echo "musl libc (x86_64)" >&2
  echo "Version 1.2.4" >&2
  exit 1
fi
echo "ldd (GNU libc) 2.35"
EOF
cat >"$bin/sysctl" <<'EOF'
#!/bin/sh
echo "${FAKE_TRANSLATED:-0}"
EOF
cat >"$bin/codesign" <<'EOF'
#!/bin/sh
echo "codesign $*" >>"${FAKE_LOG:-/dev/null}"
exit "${FAKE_CODESIGN_RC:-0}"
EOF
cat >"$bin/ditto" <<'EOF'
#!/bin/sh
# ditto -x -k ZIP DIR | ditto SRC DST
echo "ditto $*" >>"${FAKE_LOG:-/dev/null}"
case "$1" in
  -x) unzip -q "$3" -d "$4" ;;
  *) cp -R "$1" "$2" ;;
esac
EOF
cat >"$bin/hdiutil" <<'EOF'
#!/bin/sh
# attach … -mountpoint DIR FILE | detach DIR. The "image" is a zip file.
echo "hdiutil $*" >>"${FAKE_LOG:-/dev/null}"
verb=$1
shift
case "$verb" in
  attach)
    m=
    while [ $# -gt 1 ]; do
      case "$1" in -mountpoint) m=$2; shift ;; esac
      shift
    done
    unzip -q "$1" -d "$m" ;;
esac
exit 0
EOF
cat >"$bin/update-desktop-database" <<'EOF'
#!/bin/sh
echo "update-desktop-database $*" >>"${FAKE_LOG:-/dev/null}"
EOF
chmod +x "$bin"/*
# A curl that answers GitHub's release URLs from a folder (FAKE_RELEASE), for the
# cases that use no WARDEN_DOWNLOAD_URL. It is in a folder of its own: the cases
# over http want the real curl.
mkdir -p "$T/curlbin"
cat >"$T/curlbin/curl" <<'EOF'
#!/bin/sh
# curl [-fsSL --retry 3] -o DEST URL: the URL is the last argument.
dest= url=
while [ $# -gt 0 ]; do
  case "$1" in -o) dest=$2; shift ;; -*) ;; *) url=$1 ;; esac
  shift
done
echo "curl $url" >>"$FAKE_LOG"
base=https://github.com/oceanwap/warden/releases
case "$url" in
  $base/latest/download/*) f=${url##*/} ;;
  $base/download/v${FAKE_TAG:-0.3.1}/*) f=${url##*/} ;;
  *) echo "curl: (22) The requested URL returned error: 404" >&2; exit 22 ;;
esac
cp "$FAKE_RELEASE/$f" "$dest"
EOF
chmod +x "$T/curlbin/curl"

# ----------------------------------------------------------- fake releases
# stub_program FILE NAME VERSION MODE: a program that answers like warden / warden-gui.
# MODE: ok | broken (--version fails) | noupdate (a --help without `update`).
stub_program() {
    {
        echo '#!/bin/sh'
        echo 'case "${1:-}" in'
        case "$4" in
            broken) echo "  --version) echo 'cannot run' >&2; exit 1 ;;" ;;
            *) echo "  --version) echo '$2 $3' ;;" ;;
        esac
        echo '  --help)'
        echo "    echo 'USAGE:'"
        if [ "$4" != noupdate ]; then
            echo "    echo '    update           Restart every supervisor and wardend'"
            echo "    echo '    gui-install      Install the desktop app of this version'"
        fi
        echo '    ;;'
        echo '  daemon) [ "${2:-}" = status ] && [ -f "${WARDEN_FAKE_RUNNING:-/nonexistent}" ] && exit 0; exit 1 ;;'
        echo 'esac'
    } >"$1"
    chmod +x "$1"
}

# sums DIR: SHA256SUMS over every other file, in sha256sum's format.
sums() {
    local f
    : >"$1/SHA256SUMS.new"
    for f in "$1"/*; do
        case "$f" in */SHA256SUMS | */SHA256SUMS.new) continue ;; esac
        printf '%s  %s\n' "$(sha256 "$f")" "$(basename "$f")" >>"$1/SHA256SUMS.new"
    done
    mv "$1/SHA256SUMS.new" "$1/SHA256SUMS"
}

# make_release DIR VERSION [FLAGS]: archives for linux and macos, x86_64 and arm64.
# FLAGS: broken, noupdate, nogui (no GUI files), nozip (macOS GUI only as a .dmg),
#        baddmg (a .dmg without Warden.app), nolinux (no Linux files).
make_release() {
    local d=$1 v=$2 flags=" ${*:3} " os arch n mode=ok s plist
    case $flags in *" broken "*) mode=broken ;; *" noupdate "*) mode=noupdate ;; esac
    mkdir -p "$d"
    for os in linux macos; do
        case $flags in *" nolinux "*) [ "$os" = linux ] && continue ;; esac
        for arch in x86_64 arm64; do
            n=warden-$v-$os-$arch
            s=$T/stage/$n
            mkdir -p "$s/contrib"
            stub_program "$s/warden" warden "$v" "$mode"
            echo readme >"$s/README.md"
            COPYFILE_DISABLE=1 tar -C "$T/stage" -czf "$d/$n.tar.gz" "$n"
        done
    done
    case $flags in *" nogui "*) sums "$d"; return 0 ;; esac
    for arch in x86_64 arm64; do
        if [[ $flags != *" nolinux "* ]]; then
            n=warden-gui-$v-linux-$arch
            s=$T/stage/$n
            mkdir -p "$s/contrib"
            stub_program "$s/warden" warden "$v" "$mode"
            stub_program "$s/warden-gui" warden-gui "$v" "$mode"
            printf '[Desktop Entry]\nType=Application\nName=Warden\nExec=warden-gui\nIcon=warden\nTerminal=false\n' >"$s/warden-gui.desktop"
            printf 'PNG' >"$s/warden.png"
            COPYFILE_DISABLE=1 tar -C "$T/stage" -czf "$d/$n.tar.gz" "$n"
        fi
        s=$T/stage/app-$v-$arch
        mkdir -p "$s/Warden.app/Contents/MacOS" "$s/Warden.app/Contents/Resources"
        stub_program "$s/Warden.app/Contents/MacOS/warden" warden "$v" "$mode"
        stub_program "$s/Warden.app/Contents/MacOS/warden-gui" warden-gui "$v" "$mode"
        plist=$s/Warden.app/Contents/Info.plist
        printf '<plist><dict><key>CFBundleIdentifier</key><string>io.github.oceanwap.warden</string><key>CFBundleShortVersionString</key><string>%s</string></dict></plist>\n' "$v" >"$plist"
        echo "new $v" >"$s/Warden.app/Contents/Resources/marker"
        case $flags in *" nozip "*) ;; *) (cd "$s" && zip -qr "$d/warden-gui-$v-macos-$arch.zip" Warden.app) ;; esac
        if [[ $flags == *" baddmg "* ]]; then
            mkdir -p "$s/empty/NotWarden"
            (cd "$s/empty" && zip -qr "$d/Warden-$v-macos-$arch.dmg" NotWarden)
        else
            (cd "$s" && zip -qr "$d/Warden-$v-macos-$arch.dmg" Warden.app)
        fi
    done
    sums "$d"
}

REL=$T/rel
make_release "$REL" 0.3.1
make_release "$T/rel-multi" 0.3.1
make_release "$T/rel-020" 0.2.0
cp "$T"/rel-020/* "$T/rel-multi/"
sums "$T/rel-multi"
make_release "$T/rel-old" 0.1.0 noupdate
make_release "$T/rel-broken" 0.3.1 broken
make_release "$T/rel-nogui" 0.3.1 nogui
make_release "$T/rel-dmgonly" 0.3.1 nozip
make_release "$T/rel-baddmg" 0.3.1 nozip baddmg
make_release "$T/rel-nolinux" 0.3.1 nolinux
# A release whose archives were changed after SHA256SUMS was written.
cp -R "$REL" "$T/rel-tampered"
for f in "$T"/rel-tampered/*.tar.gz "$T"/rel-tampered/*.zip; do printf 'x' >>"$f"; done
# The same, only the GUI files.
cp -R "$REL" "$T/rel-tampered-gui"
for f in "$T"/rel-tampered-gui/warden-gui-*; do printf 'x' >>"$f"; done
# SHA256SUMS lists wrong hashes (a mirror out of step), the archives are fine.
cp -R "$REL" "$T/rel-wrongsums"
sed 's/^[0-9a-f]\{8\}/00000000/' "$REL/SHA256SUMS" >"$T/rel-wrongsums/SHA256SUMS"
cp -R "$REL" "$T/rel-nosums"
rm -f "$T/rel-nosums/SHA256SUMS"

# ----------------------------------------------------------- running the installer
n_case=0
H="" D="" X="" A=""
new_case() { # a fresh HOME, install folder, data folder and app folder
    n_case=$((n_case + 1))
    H=$T/h$n_case
    D=$H/bin
    X=$H/share
    A=$H/Applications
    mkdir -p "$H"
    rm -f "$T/running" "$T/stub.log"
}
SH="sh"
RUN=$installer
out="" rc=0

# inst [VAR=value…] -- ARGS…: install.sh in a clean environment (HOME is $H, the
# platform is a Linux x86_64 with glibc 2.35, the release is $REL, the program
# goes to $D); later VAR=value override these. Output in $out, status in $rc.
inst() {
    local envs=()
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    # shellcheck disable=SC2086 # $SH may be "bash --posix"
    out=$(env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" LANG=C SHELL=/bin/bash \
        FAKE_OS=Linux FAKE_ARCH=x86_64 FAKE_GLIBC=2.35 FAKE_LIBC=glibc FAKE_LOG="$T/stub.log" \
        WARDEN_RUNTIME_DIR="$T/run" WARDEN_HOME="$T/wardenhome" WARDEN_NO_DAEMON=1 WARDEN_FAKE_RUNNING="$T/running" \
        XDG_DATA_HOME="$X" WARDEN_APP_DIR="$A" WARDEN_DOWNLOAD_URL="file://$REL" WARDEN_INSTALL_DIR="$D" \
        ${envs[@]+"${envs[@]}"} $SH "$RUN" "$@" </dev/null 2>&1)
    rc=$?
}
# The same through a pipe, as `curl … | sh -s -- ARGS` runs it.
inst_pipe() {
    local envs=()
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    # A pipe on purpose (`curl | sh` gives one, not a file), hence the cat.
    # shellcheck disable=SC2086,SC2002
    out=$(cat "$RUN" | env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" LANG=C SHELL=/bin/bash \
        FAKE_OS=Linux FAKE_ARCH=x86_64 FAKE_GLIBC=2.35 FAKE_LIBC=glibc FAKE_LOG="$T/stub.log" \
        WARDEN_RUNTIME_DIR="$T/run" WARDEN_HOME="$T/wardenhome" WARDEN_NO_DAEMON=1 WARDEN_FAKE_RUNNING="$T/running" \
        XDG_DATA_HOME="$X" WARDEN_APP_DIR="$A" WARDEN_DOWNLOAD_URL="file://$REL" WARDEN_INSTALL_DIR="$D" \
        ${envs[@]+"${envs[@]}"} $SH -s -- "$@" 2>&1)
    rc=$?
}

has() { printf '%s\n' "$out" | grep -Fq -- "$1"; }
hasnt() { ! has "$1"; }
has_in() { printf '%s\n' "$1" | grep -Fq -- "$2"; } # text needle
ok_has() { [ "$rc" = 0 ] && has "$1"; }  # succeeded, and said it
fail_has() { [ "$rc" != 0 ] && has "$1"; } # failed, and said it
ver_is() { [ "$("$1" --version 2>/dev/null)" = "$2" ]; }
file_has() { grep -Fq -- "$2" "$1"; }
# Nothing but the listed names (and no .new leftovers) in a folder.
only() { # dir names…
    local d=$1 got want
    shift
    got=$(ls -A "$d" 2>/dev/null | tr '\n' ' ')
    want=$(printf '%s\n' "$@" | tr '\n' ' ')
    [ "$got" = "$want" ]
}
scratch_clean() { [ -z "$(ls -A "$T/tmp")" ]; }

echo "install.sh ($installer)"

# ============================================================ 1. static
echo "-- the script itself"
check "sh -n" sh -n "$installer"
if command -v dash >/dev/null 2>&1; then check "dash -n" dash -n "$installer"; else note "dash is not installed: skipped"; fi
check "bash --posix -n" bash --posix -n "$installer"
if command -v shellcheck >/dev/null 2>&1; then check "shellcheck" shellcheck "$installer" "$root/scripts/test-install.sh"; else note "shellcheck is not installed: skipped"; fi
check "it is a POSIX sh script (#!/bin/sh)" test "$(head -n 1 "$installer")" = '#!/bin/sh'
check "it starts with set -eu" grep -qx 'set -eu' "$installer"
check "its last line calls main with the end marker" test "$(tail -n 1 "$installer")" = 'main "$@" --end-of-install-script'
check "it never reads its standard input (no read command)" sh -c "! grep -Ev '^[[:space:]]*#' '$installer' | grep -Eq '(^|[;&| ])read( |\$)'"
check "it does not run sudo itself (it only suggests it)" sh -c "! grep -E '^[[:space:]]*(sudo|su) ' '$installer'"

# ============================================================ 2. help and bad arguments
echo "-- help and arguments"
new_case
inst -- --help
check "--help exits 0" test "$rc" -eq 0
for o in --version --gui --gui-only --uninstall --prefix --dir --modify-path --dry-run --no-verify --help install-gui.sh WARDEN_VERSION WARDEN_INSTALL_DIR WARDEN_DOWNLOAD_URL; do
    check "--help lists $o" has "$o"
done
inst -- -h
check "-h is --help" has "Usage: install.sh"
check "--help installs nothing" test ! -e "$D"
inst -- --bogus
check "an unknown option exits 2" test "$rc" -eq 2
check "…naming it" has "unknown option '--bogus'"
inst -- --version
check "--version without a value exits 2" test "$rc" -eq 2
inst -- --version not-a-version
check "a version that is not one exits 2" test "$rc" -eq 2
check "…and says what to give" has "is not a version"
inst -- --dir
check "--dir without a value exits 2" test "$rc" -eq 2
check "none of these installed anything" test ! -e "$D"

# ============================================================ 3. default install
echo "-- install (the latest release, file://)"
new_case
inst --
check "installs, exit 0" test "$rc" -eq 0
check "warden is where WARDEN_INSTALL_DIR said" ver_is "$D/warden" "warden 0.3.1"
check "…executable" test -x "$D/warden"
check "…and the folder holds nothing else" only "$D" warden
check "the checksum was checked" has "checksum OK"
check "the temporary folder is gone" scratch_clean
check "no startup file of the user's was touched" test -z "$(find "$H" -maxdepth 1 -name '.*' ! -name . ! -name .. 2>/dev/null)"
check "it names the version it found" has "installed warden 0.3.1"
check "the next steps are printed" has "doctor"
check "…not the archive's contrib/ files (warden startup writes what a host needs)" hasnt "contrib/"
check "…and not the GUI, with no desktop session (no DISPLAY)" hasnt "gui-install"
inst --
check "installing again exits 0" test "$rc" -eq 0
check "…and says it was this version" has "it was this version already"
make_release "$T/rel-020b" 0.2.0
inst WARDEN_DOWNLOAD_URL="file://$T/rel-020b" --
check "an older version is installed over" ver_is "$D/warden" "warden 0.2.0"
inst --
check "an upgrade says what it replaced" has "(was: warden 0.2.0)"
check "…and leaves no temporary file next to the program" only "$D" warden

echo "-- where it installs by default"
new_case
inst WARDEN_INSTALL_DIR= -- --dry-run
if [ "$(id -u)" -eq 0 ]; then
    check "as root: /usr/local/bin" has "/usr/local/bin/warden"
else
    check "as a user: ~/.local/bin" has "$H/.local/bin/warden"
    inst WARDEN_INSTALL_DIR= --
    check "…and it installs there" ver_is "$H/.local/bin/warden" "warden 0.3.1"
fi
new_case
inst -- --dir "$H/viaflag/bin"
check "--dir" ver_is "$H/viaflag/bin/warden" "warden 0.3.1"
inst -- --prefix "$H/viaprefix"
check "--prefix" ver_is "$H/viaprefix/warden" "warden 0.3.1"
inst -- "--dir=$H/viaeq"
check "--dir=DIR" ver_is "$H/viaeq/warden" "warden 0.3.1"
inst WARDEN_INSTALL_DIR="$H/env-loses" -- --dir "$H/flag-wins"
check "a flag beats WARDEN_INSTALL_DIR" test -x "$H/flag-wins/warden"
mkdir -p "$H/cwd"
out=$(cd "$H/cwd" && env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" FAKE_LOG="$T/stub.log" WARDEN_DOWNLOAD_URL="file://$REL" WARDEN_NO_DAEMON=1 sh "$installer" --dir rel/bin 2>&1 </dev/null)
check "a relative --dir is relative to where it is run" test -x "$H/cwd/rel/bin/warden"
check "a folder with a space in its name" sh -c "env -i PATH='$bin:$PATH' HOME='$H' TMPDIR='$T/tmp' FAKE_LOG='$T/stub.log' WARDEN_DOWNLOAD_URL='file://$REL' sh '$installer' --dir '$H/a b/bin' >/dev/null 2>&1 && '$H/a b/bin/warden' --version | grep -q 'warden 0.3.1'"

# ============================================================ 4. versions
echo "-- versions"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" --
check "two versions in SHA256SUMS: the latest is ambiguous and refused" test "$rc" -ne 0
check "…saying to pick one" has "more than one linux-x86_64 archive"
check "…and nothing was installed" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version v0.2.0
check "--version v0.2.0" ver_is "$D/warden" "warden 0.2.0"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version 0.3.1
check "--version 0.3.1 (no v)" ver_is "$D/warden" "warden 0.3.1"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version=0.2.0
check "--version=0.2.0" ver_is "$D/warden" "warden 0.2.0"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" WARDEN_VERSION=v0.3.1 --
check "WARDEN_VERSION" ver_is "$D/warden" "warden 0.3.1"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" WARDEN_VERSION=0.3.1 -- --version 0.2.0
check "--version beats WARDEN_VERSION" ver_is "$D/warden" "warden 0.2.0"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version latest
check "--version latest is the same as none (and ambiguous here)" test "$rc" -ne 0
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version 9.9.9
check "a version the release does not have is refused" test "$rc" -ne 0
check "…and the release's files are listed" has "warden-0.2.0-linux-x86_64.tar.gz"
check "…nothing installed" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version v1.0.0-rc.1 --dry-run
check "a pre-release version (v1.0.0-rc.1) passes the version check (this release just has no such files)" fail_has "has no warden-1.0.0-rc.1-linux-x86_64.tar.gz"

# ============================================================ 5. dry run
echo "-- --dry-run"
new_case
cp -R "$REL" "$T/rel-nofiles"
rm -f "$T"/rel-nofiles/*.tar.gz "$T"/rel-nofiles/*.zip "$T"/rel-nofiles/*.dmg
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nofiles" -- --dry-run
check "a dry run exits 0 although the archives are not even there (nothing is downloaded)" test "$rc" -eq 0
check "…says it is a dry run" has "dry run"
check "…names the archive and its hash" has "warden-0.3.1-linux-x86_64.tar.gz  sha256 $(awk '$2=="warden-0.3.1-linux-x86_64.tar.gz"{print $1}' "$REL/SHA256SUMS")"
check "…and where it would install" has "$D/warden"
check "…without creating anything" test ! -e "$D"
check "…or leaving anything in the temp folder" scratch_clean
inst -- --dry-run --gui
check "--dry-run --gui lists the GUI files" has "warden-gui-0.3.1-linux-x86_64.tar.gz"
check "…and the menu entry" has "$X/applications/warden-gui.desktop"
check "…installing nothing" test ! -e "$D"
inst -- --dry-run --version 0.9.9
check "--dry-run finds out that a version does not exist" test "$rc" -ne 0
inst --
inst -- --dry-run --uninstall
check "--dry-run --uninstall exits 0" test "$rc" -eq 0
check "…says what it would remove" has "would remove $D/warden"
check "…and removes nothing" test -x "$D/warden"

# ============================================================ 6. uninstall
echo "-- --uninstall"
new_case
inst --
echo keep >"$D/other-tool"
inst -- --uninstall
check "--uninstall exits 0" test "$rc" -eq 0
check "…removes warden" test ! -e "$D/warden"
check "…and only that: another file in the folder stays" only "$D" other-tool
check "…and says what it did not touch" has "Not touched"
inst -- --uninstall
check "uninstalling when there is nothing exits 0" test "$rc" -eq 0
check "…and says so" has "nothing to remove"
printf '#!/bin/sh\necho "something else 1.0"\n' >"$D/warden"
chmod +x "$D/warden"
inst -- --uninstall
check "a program called warden that is not Warden is not removed" test -x "$D/warden"
check "…and the installer says why" has "not removing"
new_case
inst --
: >"$T/running"
inst -- --uninstall
check "uninstalling while wardend runs warns that it keeps running" has "running"
check "…and still removes the file" test ! -e "$D/warden"
rm -f "$T/running"

# ============================================================ 7. checksums
echo "-- checksums and downloads"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-multi" -- --version 0.2.0
inst WARDEN_DOWNLOAD_URL="file://$T/rel-tampered" --
check "a tampered archive is refused" test "$rc" -ne 0
check "…as a checksum mismatch" has "checksum mismatch for warden-0.3.1-linux-x86_64.tar.gz"
check "…with the hash it wanted and the one it got" has "expected  $(awk '$2=="warden-0.3.1-linux-x86_64.tar.gz"{print $1}' "$REL/SHA256SUMS")"
check "…with the way out" has "Try again in a minute"
check "…saying nothing was installed" has "Nothing was installed"
check "…and warning against --no-verify" has "discouraged"
check "…the program that was there stays, and no temporary file" sh -c "'$D/warden' --version | grep -q 'warden 0.2.0'"
check "…nothing left next to it" only "$D" warden
check "…and the temp folder is gone" scratch_clean
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-tampered" --
check "a tampered archive on a first install installs nothing" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-tampered-gui" -- --gui
check "a tampered GUI archive stops --gui" test "$rc" -ne 0
check "…and the CLI was not installed either (everything is checked first)" test ! -e "$D"
check "…nor a menu entry" test ! -e "$X"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-wrongsums" --
check "SHA256SUMS that does not match the files: refused" test "$rc" -ne 0
check "…and nothing installed" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-wrongsums" -- --no-verify
check "--no-verify installs it anyway" test "$rc" -eq 0
check "…the program is there" ver_is "$D/warden" "warden 0.3.1"
check "…with a loud warning" has "WARNING: --no-verify"
check "…saying it was not checked" has "NOT checked against SHA256SUMS"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nosums" --
check "no SHA256SUMS: refused" test "$rc" -ne 0
check "…naming the file it could not get" has "SHA256SUMS"
check "…nothing installed" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nosums" -- --no-verify
check "no SHA256SUMS and no version: --no-verify cannot tell which file to take" test "$rc" -ne 0
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nosums" -- --no-verify --version 0.3.1
check "…with a version it installs, warning loudly" sh -c "[ '$rc' = 0 ] && '$D/warden' --version | grep -q 'warden 0.3.1'"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nolinux" --
check "a release with no archive for this platform is refused" test "$rc" -ne 0
check "…saying which" has "no linux-x86_64 archive"
check "…and what it does have" has "warden-0.3.1-macos-arm64.tar.gz"
check "…pointing at building from source" has "cargo build --release"
inst WARDEN_DOWNLOAD_URL="file://$T/does-not-exist" --
check "an unreachable release is refused" test "$rc" -ne 0
check "…with a hint to build from source" has "cargo build --release"
cp -R "$REL" "$T/rel-missingfile"
rm -f "$T/rel-missingfile/warden-0.3.1-linux-x86_64.tar.gz"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-missingfile" --
check "an archive listed but missing is refused" test "$rc" -ne 0
check "…as a download failure" has "cannot download"
check "…nothing installed" test ! -e "$D"

# ============================================================ 8. a program that does not run
echo "-- a binary that cannot run"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-020b" --
inst WARDEN_DOWNLOAD_URL="file://$T/rel-broken" --
check "a downloaded program that does not run is refused" test "$rc" -ne 0
check "…saying so" has "does not run on this system"
check "…the working one stays" ver_is "$D/warden" "warden 0.2.0"
check "…and no half-installed file is left" only "$D" warden

# ============================================================ 9. PATH
echo "-- PATH"
new_case
inst --
check "a folder not on PATH: the hint says so" has "is not on your PATH"
check "…with the line for bash (export, with \$HOME)" has 'export PATH="$HOME/bin:$PATH"'
check "…for ~/.bashrc" has ">> ~/.bashrc"
check "…and says nothing was changed" has "nothing was changed in your files"
check "…and no file was created" test ! -e "$H/.bashrc"
inst SHELL=/usr/bin/zsh --
check "zsh: ~/.zshrc" has ">> ~/.zshrc"
inst SHELL=/usr/bin/fish --
check "fish: fish_add_path" has 'fish_add_path "$HOME/bin"'
check "…not an export" hasnt "export PATH"
inst SHELL=/bin/dash --
check "another shell: ~/.profile" has ">> ~/.profile"
inst SHELL= --
check "no SHELL at all: ~/.profile" has ">> ~/.profile"
inst FAKE_OS=Darwin FAKE_ARCH=arm64 SHELL=/bin/bash --
check "bash on macOS: ~/.bash_profile" has ">> ~/.bash_profile"
mkdir -p "$T/outside"
inst -- --dir "$T/outside/bin"
check "a folder outside HOME is named in full" has "export PATH=\"$T/outside/bin:\$PATH\""
inst PATH="$D:$bin:$PATH" --
check "a folder on PATH: no hint" hasnt "is not on your PATH"
check "…and the next steps say plain warden" has "  warden doctor"
mkdir -p "$H/other"
printf '#!/bin/sh\necho "warden 0.0.1"\n' >"$H/other/warden"
chmod +x "$H/other/warden"
inst PATH="$H/other:$D:$bin:$PATH" --
check "another warden earlier on PATH is pointed out" has "comes before $D on your PATH"

echo "-- --modify-path"
new_case
printf 'export FOO=1\n# my own line\n' >"$H/.bashrc"
cp "$H/.bashrc" "$T/bashrc.orig"
inst -- --modify-path
check "--modify-path exits 0" test "$rc" -eq 0
check "…adds the line to ~/.bashrc" file_has "$H/.bashrc" 'export PATH="$HOME/bin:$PATH"'
check "…under a marker comment" file_has "$H/.bashrc" "# added by the Warden installer"
check "…keeping what was there" file_has "$H/.bashrc" "# my own line"
check "…and says to open a new terminal" has "open a new terminal"
inst -- --modify-path
check "twice: the line is there once" test "$(grep -c 'export PATH="\$HOME/bin' "$H/.bashrc")" = 1
check "…and it says it is there already" has "already puts"
inst -- --uninstall
check "--uninstall takes the line and the marker out again" sh -c "! grep -q 'Warden installer' '$H/.bashrc' && ! grep -q 'HOME/bin' '$H/.bashrc'"
check "…leaving the file exactly as it was" cmp -s "$H/.bashrc" "$T/bashrc.orig"
inst SHELL=/bin/zsh -- --modify-path
check "zsh: ~/.zshrc is created" file_has "$H/.zshrc" 'export PATH="$HOME/bin:$PATH"'
inst SHELL=/bin/zsh -- --uninstall
check "…and uninstalling empties it of our lines" sh -c "! grep -q 'HOME/bin' '$H/.zshrc'"
inst SHELL=/usr/bin/fish -- --modify-path
check "fish: a file of its own in conf.d" file_has "$H/.config/fish/conf.d/warden.fish" 'fish_add_path "$HOME/bin"'
inst SHELL=/usr/bin/fish -- --uninstall
check "…removed again by --uninstall" test ! -e "$H/.config/fish/conf.d/warden.fish"
new_case
inst PATH="$D:$bin:$PATH" -- --modify-path
check "--modify-path with the folder already on PATH edits nothing" test ! -e "$H/.bashrc"

# ============================================================ 10. curl | sh
echo "-- piped into sh (curl | sh)"
new_case
inst_pipe -- --dir "$D"
check "the piped script installs" ver_is "$D/warden" "warden 0.3.1"
check "…exit 0" test "$rc" -eq 0
inst_pipe -- --uninstall
check "piped, with an argument: --uninstall" test ! -e "$D/warden"
inst_pipe -- --version 0.3.1 --dry-run
check "piped, with two arguments" has "dry run"
# A download cut short anywhere must run nothing.
new_case
size=$(wc -c <"$installer" | tr -d ' ')
cuts=0
bad=""
step=$((size / 60))
offsets=""
i=0
while [ "$i" -lt "$size" ]; do offsets="$offsets $i"; i=$((i + step)); done
# (size - 1 only lacks the final newline: that is the whole script.)
i=2
while [ "$i" -le 81 ]; do offsets="$offsets $((size - i))"; i=$((i + 1)); done
for off in $offsets; do
    rm -rf "$D"
    # shellcheck disable=SC2086
    cut_out=$(head -c "$off" "$installer" | env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" FAKE_LOG="$T/stub.log" \
        WARDEN_DOWNLOAD_URL="file://$REL" WARDEN_INSTALL_DIR="$D" WARDEN_NO_DAEMON=1 $SH -s -- 2>&1)
    cuts=$((cuts + 1))
    if [ -e "$D" ] || printf '%s' "$cut_out" | grep -q 'installed warden'; then bad="$bad $off"; fi
done
if [ -z "$bad" ]; then pass "a script cut at $cuts different places installs nothing"; else fail "a script cut at byte$bad installed something"; fi
rm -rf "$D"
cut_out=$(head -c "$((size - 25))" "$installer" | env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" WARDEN_DOWNLOAD_URL="file://$REL" WARDEN_INSTALL_DIR="$D" $SH -s -- 2>&1)
check "cut right after 'main \"\$@\"': refused, with a message to download again" has_in "$cut_out" "this script is incomplete"
check "…and nothing installed" test ! -e "$D"
check "the temp folder is gone after all that" scratch_clean

# ============================================================ 11. platforms
echo "-- platforms"
new_case
inst FAKE_LIBC=musl FAKE_GLIBC=none --
check "musl (Alpine) is refused" test "$rc" -ne 0
check "…saying why and what to do" has "musl"
check "…suggesting a build from source" has "cargo build --release"
check "…nothing installed" test ! -e "$D"
inst FAKE_GLIBC=2.17 --
check "glibc 2.17 is refused as too old" test "$rc" -ne 0
check "…saying 2.28 is needed" has "glibc 2.28+"
inst FAKE_GLIBC=2.28 --
check "glibc 2.28 (the baseline) installs" ver_is "$D/warden" "warden 0.3.1"
inst FAKE_GLIBC=3.0 --
check "a glibc 3.0 would too" test "$rc" -eq 0
new_case
inst FAKE_GLIBC=none FAKE_LIBC=glibc --
check "a libc it cannot identify: warns and goes on" ok_has "cannot tell which C library"
inst FAKE_ARCH=aarch64 --
check "aarch64 takes the arm64 archive" has "warden-0.3.1-linux-arm64.tar.gz"
inst FAKE_ARCH=amd64 --
check "amd64 takes the x86_64 archive" has "warden-0.3.1-linux-x86_64.tar.gz"
inst FAKE_ARCH=armv7l --
check "an armv7 CPU is refused" test "$rc" -ne 0
check "…naming the CPU" has "armv7l"
inst FAKE_OS=FreeBSD --
check "FreeBSD is refused" test "$rc" -ne 0
check "…naming the OS" has "FreeBSD"
inst FAKE_OS=Darwin FAKE_ARCH=arm64 FAKE_GLIBC=none FAKE_LIBC=musl --
check "macOS arm64 takes the macos-arm64 archive (and has no libc check)" ok_has "warden-0.3.1-macos-arm64.tar.gz"
inst FAKE_OS=Darwin FAKE_ARCH=x86_64 --
check "macOS x86_64 takes macos-x86_64" has "warden-0.3.1-macos-x86_64.tar.gz"
inst FAKE_OS=Darwin FAKE_ARCH=x86_64 FAKE_TRANSLATED=1 --
check "a shell under Rosetta takes the arm64 archive" has "warden-0.3.1-macos-arm64.tar.gz"
inst FAKE_OS=Darwin FAKE_ARCH=x86_64 FAKE_TRANSLATED=0 --
check "…and not when it is not translated" has "warden-0.3.1-macos-x86_64.tar.gz"

# ============================================================ 12. tools
echo "-- missing tools"
# A PATH with only what install.sh needs (and the stubs), so that a tool can be left out.
mkpath() { # dir tool…
    local d=$1 t p
    shift
    mkdir -p "$d"
    for t in sh awk sed tar gzip mktemp grep id head wc tr cat cp mv rm chmod mkdir dirname basename ln "$@"; do
        p=$(command -v "$t") || continue
        case "$p" in /*) ln -sf "$p" "$d/$t" ;; esac
    done
    for t in uname getconf ldd sysctl; do ln -sf "$bin/$t" "$d/$t"; done
}
new_case
mkpath "$T/path-nodl" sha256sum shasum openssl
inst PATH="$T/path-nodl" WARDEN_DOWNLOAD_URL=http://127.0.0.1:9/x --
check "no curl and no wget, an http download: refused" test "$rc" -ne 0
check "…saying it needs curl or wget" has "needs curl or wget"
inst PATH="$T/path-nodl" --
check "…but file:// needs neither" ver_is "$D/warden" "warden 0.3.1"
new_case
mkpath "$T/path-nosum" curl wget
inst PATH="$T/path-nosum" --
check "no sha256sum, shasum or openssl: refused" test "$rc" -ne 0
check "…saying which tools" has "needs sha256sum, shasum or openssl"
check "…nothing installed" test ! -e "$D"
inst PATH="$T/path-nosum" -- --no-verify
check "…--no-verify does not need one (and warns)" ok_has "WARNING"
new_case
mkpath "$T/path-notar" sha256sum shasum
rm -f "$T/path-notar/tar"
inst PATH="$T/path-notar" --
check "no tar: refused with its name" fail_has "needs 'tar'"
check "…nothing installed" test ! -e "$D"
new_case
mkpath "$T/path-shasum" shasum
rm -f "$T/path-shasum/sha256sum" "$T/path-shasum/openssl"
if [ -e "$T/path-shasum/shasum" ]; then
    inst PATH="$T/path-shasum" --
    check "shasum alone is enough to verify" ver_is "$D/warden" "warden 0.3.1"
else
    note "shasum is not installed: skipped"
fi
new_case
mkpath "$T/path-openssl" openssl
rm -f "$T/path-openssl/sha256sum" "$T/path-openssl/shasum"
if [ -e "$T/path-openssl/openssl" ]; then
    inst PATH="$T/path-openssl" --
    check "openssl alone is enough to verify" ver_is "$D/warden" "warden 0.3.1"
else
    note "openssl is not installed: skipped"
fi

# ============================================================ 13. running wardend
echo "-- wardend running"
new_case
inst --
check "wardend not running: no restart note" hasnt "wardend is running"
: >"$T/running"
inst --
check "wardend running: the note says so" has "wardend is running"
check "…says that supervisors keep the old code" has "still on the old version"
check "…and gives \`warden update\`" has "$D/warden update"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-old" --
check "a release without \`warden update\` gets the save, kill, resurrect commands instead" has "save && $D/warden kill --yes && $D/warden resurrect"
inst PATH="$D:$bin:$PATH" --
check "on PATH the command is just \`warden update\`" has "    warden update"
rm -f "$T/running"

# ============================================================ 14. the Linux GUI
echo "-- --gui on Linux"
new_case
inst -- --gui
check "--gui exits 0" test "$rc" -eq 0
check "…installs the CLI" ver_is "$D/warden" "warden 0.3.1"
check "…and the GUI program" ver_is "$D/warden-gui" "warden-gui 0.3.1"
check "…a menu entry that names the GUI by full path" file_has "$X/applications/warden-gui.desktop" "Exec=$D/warden-gui"
check "…keeping the rest of the entry" file_has "$X/applications/warden-gui.desktop" "Icon=warden"
check "…and the icon where the entry looks for it" test -f "$X/icons/hicolor/256x256/apps/warden.png"
check "…the desktop database is refreshed" file_has "$T/stub.log" "update-desktop-database $X/applications"
check "…no temporary file next to them" only "$D" warden warden-gui
check "…and the temp folder is gone" scratch_clean
check "…it mentions the menu entry" has "warden-gui.desktop"
inst -- --uninstall
check "--uninstall alone leaves the GUI" test -x "$D/warden-gui"
check "…and says how to remove it" has "--uninstall --gui"
check "…the CLI is gone" test ! -e "$D/warden"
inst -- --uninstall --gui
check "--uninstall --gui removes the GUI program" test ! -e "$D/warden-gui"
check "…the menu entry" test ! -e "$X/applications/warden-gui.desktop"
check "…and the icon" test ! -e "$X/icons/hicolor/256x256/apps/warden.png"
check "…and nothing else of ours is left" test -z "$(ls -A "$D")"
new_case
inst FAKE_GLIBC=2.31 -- --gui
check "--gui on glibc 2.31 is refused (the GUI needs 2.35)" test "$rc" -ne 0
check "…saying the CLI alone works" has "leave out --gui"
check "…nothing installed" test ! -e "$D"
inst FAKE_GLIBC=2.31 --
check "…the CLI alone installs on 2.31" test "$rc" -eq 0
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-nogui" -- --gui
check "a release without a GUI: refused" test "$rc" -ne 0
check "…saying so" has "has no GUI for linux-x86_64"
check "…and the CLI was not installed (it stops before)" test ! -e "$D"
new_case
inst -- --gui --dir "$H/with space/bin"
check "a folder with a space: the entry quotes the path" file_has "$X/applications/warden-gui.desktop" "Exec=\"$H/with space/bin/warden-gui\""
new_case
mkdir -p "$H/dollar"
inst -- --gui --dir "$H/dollar/\$x"
check "a folder a menu entry cannot name is refused" test "$rc" -ne 0
check "…before anything is installed" test ! -e "$H/dollar/\$x"
new_case
inst FAKE_ARCH=aarch64 -- --gui
check "aarch64: the arm64 GUI archive" has "warden-gui-0.3.1-linux-arm64.tar.gz"
new_case
inst -- --gui --no-verify
check "--gui --no-verify installs both, warning" sh -c "[ '$rc' = 0 ] && [ -x '$D/warden-gui' ]"

echo "-- the GUI in the next steps"
new_case
inst DISPLAY=:0 --
check "a desktop session (DISPLAY): the next steps offer warden gui-install" has "gui-install"
check "…in their column" has "$D/warden gui-install  add the desktop app"
inst WAYLAND_DISPLAY=wayland-0 --
check "…Wayland too" has "gui-install"
inst DISPLAY=:0 -- --gui
check "…not after --gui (it is there)" hasnt "gui-install"
inst DISPLAY=:0 FAKE_GLIBC=2.31 --
check "…not on a glibc too old for the GUI" hasnt "gui-install"
inst DISPLAY=:0 WARDEN_DOWNLOAD_URL="file://$T/rel-nogui" --
check "…not when the release has no GUI" hasnt "gui-install"
inst DISPLAY=:0 WARDEN_DOWNLOAD_URL="file://$T/rel-old" --
check "…not when the warden installed has no gui-install command" hasnt "gui-install"
inst FAKE_OS=Darwin FAKE_ARCH=arm64 --
check "macOS: offered, no DISPLAY needed" has "add the desktop app, Warden.app"

echo "-- --gui-only on Linux"
new_case
inst --
echo '# this very file' >>"$D/warden"
cp "$D/warden" "$T/cli-before"
inst -- --gui-only --dry-run
check "--gui-only --dry-run: the GUI archive" has "warden-gui-0.3.1-linux-x86_64.tar.gz"
check "…not the CLI's" hasnt "download   warden-0.3.1-linux-x86_64.tar.gz"
check "…and the CLI it leaves as it is" has "left as it is: $D/warden"
inst -- --gui-only
check "--gui-only exits 0" test "$rc" -eq 0
check "…installs the GUI program" ver_is "$D/warden-gui" "warden-gui 0.3.1"
check "…and its menu entry" file_has "$X/applications/warden-gui.desktop" "Exec=$D/warden-gui"
check "…leaves the CLI as it was" cmp -s "$D/warden" "$T/cli-before"
check "…without downloading the CLI archive" hasnt "downloading warden-0.3.1-linux-x86_64.tar.gz"
check "…and no warning: they are one version" hasnt "keep the two at one version"
check "…and no next steps (that was the CLI's install)" hasnt "Next steps"
inst -- --uninstall --gui-only
check "--uninstall --gui-only removes the GUI program" test ! -e "$D/warden-gui"
check "…and its menu entry" test ! -e "$X/applications/warden-gui.desktop"
check "…and leaves the CLI" cmp -s "$D/warden" "$T/cli-before"
inst -- --uninstall --gui-only
check "…again: nothing to remove, exit 0" ok_has "nothing to remove"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-020b" --
inst -- --gui-only
check "--gui-only next to an older CLI installs the GUI (the latest)" ver_is "$D/warden-gui" "warden-gui 0.3.1"
check "…keeps that CLI" ver_is "$D/warden" "warden 0.2.0"
check "…and warns that the two are not one version" has "keep the two at one version"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-020b" -- --gui-only --version v0.2.0
check "--gui-only --version: the GUI of the CLI's version, no warning" sh -c "[ '$rc' = 0 ] && '$D/warden-gui' --version | grep -q 'warden-gui 0.2.0' && ! printf '%s' \"\$0\" | grep -q 'one version'" "$out"
new_case
inst FAKE_GLIBC=2.31 -- --gui-only
check "--gui-only on glibc 2.31 is refused" test "$rc" -ne 0
check "…nothing installed" test ! -e "$D"
new_case
if PATH=/usr/bin:/bin command -v warden >/dev/null 2>&1; then
    note "this machine has a warden in /usr/bin or /bin: --gui-only with no CLI is not tested"
else
    inst PATH="$bin:/usr/bin:/bin" -- --gui-only --dry-run
    check "--gui-only --dry-run with no warden anywhere: says the CLI comes too" has "none found: $D/warden, from the GUI archive"
    inst PATH="$bin:/usr/bin:/bin" -- --gui-only
    check "--gui-only with no warden anywhere installs the GUI" ver_is "$D/warden-gui" "warden-gui 0.3.1"
    check "…and the CLI from the GUI archive (the GUI needs it)" ver_is "$D/warden" "warden 0.3.1"
    check "…saying why" has "the GUI needs it"
fi

# ============================================================ 15. the macOS GUI
echo "-- --gui on macOS (stubbed ditto, hdiutil, codesign)"
mac=(FAKE_OS=Darwin FAKE_ARCH=arm64)
new_case
inst "${mac[@]}" -- --gui
check "--gui exits 0" test "$rc" -eq 0
check "…installs the CLI" ver_is "$D/warden" "warden 0.3.1"
check "…and Warden.app in the app folder" test -x "$A/Warden.app/Contents/MacOS/warden-gui"
check "…with the bundled CLI" ver_is "$A/Warden.app/Contents/MacOS/warden" "warden 0.3.1"
check "…the zip was unpacked with ditto" file_has "$T/stub.log" "ditto -x -k"
check "…the signature was verified before installing" file_has "$T/stub.log" "codesign --verify --deep --strict"
check "…the app folder holds Warden.app and nothing else" only "$A" Warden.app
check "…and the temp folder is gone" scratch_clean
check "…it says how to start it" has "open -a Warden"
inst "${mac[@]}" -- --gui
check "installing again replaces the app" sh -c "[ '$rc' = 0 ] && [ -x '$A/Warden.app/Contents/MacOS/warden-gui' ]"
echo "old leftover" >"$A/Warden.app/Contents/Resources/stale"
inst "${mac[@]}" -- --gui
check "…the old bundle's files are gone" test ! -e "$A/Warden.app/Contents/Resources/stale"
check "…the new ones are there" file_has "$A/Warden.app/Contents/Resources/marker" "new 0.3.1"
check "…and no temporary bundle is left" only "$A" Warden.app
inst "${mac[@]}" -- --uninstall
check "--uninstall alone leaves the app" test -d "$A/Warden.app"
check "…and says how to remove it" has "--uninstall --gui"
inst "${mac[@]}" -- --uninstall --gui
check "--uninstall --gui removes Warden.app" test ! -e "$A/Warden.app"
check "…and leaves the app folder" test -d "$A"
new_case
mkdir -p "$A/Warden.app/Contents"
printf '<plist><dict><key>CFBundleIdentifier</key><string>com.example.other</string></dict></plist>\n' >"$A/Warden.app/Contents/Info.plist"
inst "${mac[@]}" -- --gui
check "a Warden.app that is not Warden's is not replaced" test "$rc" -ne 0
check "…saying so" has "is not Warden's"
check "…it is untouched" file_has "$A/Warden.app/Contents/Info.plist" com.example.other
check "…and the CLI was not installed" test ! -e "$D"
inst "${mac[@]}" -- --uninstall --gui
check "…and it is not removed by --uninstall --gui either" test -d "$A/Warden.app"
check "…with a warning" has "not removing"
new_case
inst "${mac[@]}" FAKE_CODESIGN_RC=1 -- --gui
check "an app that fails codesign --verify is refused" test "$rc" -ne 0
check "…saying why" has "fails codesign --verify"
check "…nothing installed" sh -c "[ ! -e '$D' ] && [ ! -e '$A' ]"
new_case
inst "${mac[@]}" WARDEN_DOWNLOAD_URL="file://$T/rel-dmgonly" -- --gui
check "a release with only the disk image: Warden.app comes from it" test -x "$A/Warden.app/Contents/MacOS/warden-gui"
check "…it was mounted read-only without opening a window" file_has "$T/stub.log" "hdiutil attach -nobrowse -readonly -noautoopen -quiet -mountpoint"
check "…and detached" file_has "$T/stub.log" "hdiutil detach"
check "…and the CLI too" ver_is "$D/warden" "warden 0.3.1"
new_case
inst "${mac[@]}" WARDEN_DOWNLOAD_URL="file://$T/rel-baddmg" -- --gui
check "a disk image without Warden.app is refused" test "$rc" -ne 0
check "…saying so" has "has no Warden.app"
check "…it was detached all the same" file_has "$T/stub.log" "hdiutil detach"
check "…and nothing was installed" sh -c "[ ! -e '$D' ] && [ ! -e '$A' ]"
new_case
inst FAKE_OS=Darwin FAKE_ARCH=x86_64 FAKE_TRANSLATED=1 -- --gui
check "under Rosetta the arm64 app is installed" has "warden-gui-0.3.1-macos-arm64.zip"
new_case
inst "${mac[@]}" WARDEN_APP_DIR= -- --gui --dry-run
if [ "$(id -u)" -eq 0 ]; then
    check "the default app folder as root: /Applications" has "/Applications/Warden.app"
elif [ ! -w /Applications ]; then
    check "the default app folder when /Applications is not writable: ~/Applications" has "$H/Applications/Warden.app"
else
    note "/Applications is writable here: the default app folder is not checked"
fi
inst "${mac[@]}" -- --gui --app-dir "$H/Apps"
check "--app-dir" test -d "$H/Apps/Warden.app"
new_case
inst "${mac[@]}" --
echo '# this very file' >>"$D/warden"
cp "$D/warden" "$T/cli-before"
inst "${mac[@]}" -- --gui-only
check "macOS --gui-only installs Warden.app" test -x "$A/Warden.app/Contents/MacOS/warden-gui"
check "…leaves the CLI as it was" cmp -s "$D/warden" "$T/cli-before"
check "…without downloading the CLI archive" hasnt "downloading warden-0.3.1-macos-arm64.tar.gz"
inst "${mac[@]}" -- --uninstall --gui-only
check "…--uninstall --gui-only removes Warden.app" test ! -e "$A/Warden.app"
check "…and leaves the CLI" cmp -s "$D/warden" "$T/cli-before"
new_case
if PATH=/usr/bin:/bin command -v warden >/dev/null 2>&1; then
    note "this machine has a warden in /usr/bin or /bin: macOS --gui-only with no CLI is not tested"
else
    inst "${mac[@]}" PATH="$bin:/usr/bin:/bin" -- --gui-only
    check "macOS --gui-only with no warden: Warden.app, which links its own CLI when it opens" sh -c "[ '$rc' = 0 ] && [ -d '$A/Warden.app' ]"
    check "…saying so" has "links its own"
    check "…and nothing goes in the CLI folder" test ! -e "$D"
fi

# ============================================================ 15b. GitHub's URLs
echo "-- the release URLs (a stubbed curl)"
gh=(PATH="$T/curlbin:$bin:$PATH" FAKE_RELEASE="$REL" WARDEN_DOWNLOAD_URL=)
base=https://github.com/oceanwap/warden/releases
new_case
inst "${gh[@]}" -- 
check "no WARDEN_DOWNLOAD_URL: installs from GitHub's URLs" ver_is "$D/warden" "warden 0.3.1"
check "…SHA256SUMS comes from the latest release" file_has "$T/stub.log" "curl $base/latest/download/SHA256SUMS"
check "…and the archive from the very release SHA256SUMS named (not from 'latest')" file_has "$T/stub.log" "curl $base/download/v0.3.1/warden-0.3.1-linux-x86_64.tar.gz"
check "…nothing is fetched from 'latest' but SHA256SUMS" test "$(grep -c "latest/download" "$T/stub.log")" = 1
check "…and it says where the archive is from" has "downloading warden-0.3.1-linux-x86_64.tar.gz from $base/download/v0.3.1"
new_case
inst "${gh[@]}" -- --version v0.3.1
check "--version: SHA256SUMS and the archive come from that tag" sh -c "grep -q 'curl $base/download/v0.3.1/SHA256SUMS' '$T/stub.log' && grep -q 'curl $base/download/v0.3.1/warden-0.3.1-linux-x86_64.tar.gz' '$T/stub.log' && ! grep -q latest '$T/stub.log'"
new_case
inst "${gh[@]}" -- --version 9.9.9
check "a tag that does not exist (404) is refused" test "$rc" -ne 0
check "…asking whether it is a Warden release" has "is v9.9.9 a Warden release"
check "…and installs nothing" test ! -e "$D"
new_case
inst "${gh[@]}" -- --gui
check "--gui: the GUI archive comes from the same tag" file_has "$T/stub.log" "curl $base/download/v0.3.1/warden-gui-0.3.1-linux-x86_64.tar.gz"
new_case
inst "${gh[@]}" FAKE_OS=Darwin FAKE_ARCH=arm64 -- --gui
check "macOS --gui: the zip comes from the same tag" file_has "$T/stub.log" "curl $base/download/v0.3.1/warden-gui-0.3.1-macos-arm64.zip"

# ============================================================ 16. a local http server (curl or wget)
echo "-- over http"
if command -v python3 >/dev/null 2>&1 && command -v curl >/dev/null 2>&1; then
    mkdir -p "$T/www"
    cp -R "$REL" "$T/www/rel"
    cp -R "$T/rel-tampered" "$T/www/tampered"
    python3 -u -m http.server 0 --bind 127.0.0.1 --directory "$T/www" >"$T/http.log" 2>&1 &
    srv=$!
    port=""
    for _ in $(seq 1 50); do
        port=$(sed -n 's/.*port \([0-9][0-9]*\).*/\1/p' "$T/http.log" | head -n 1)
        if [ -n "$port" ]; then break; fi
        sleep 0.1
    done
    if [ -n "$port" ]; then
        new_case
        inst WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/rel" --
        check "curl: installs from an http server" ver_is "$D/warden" "warden 0.3.1"
        inst WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/rel/" -- --version 0.3.1
        check "…a trailing slash on the URL is fine" test "$rc" -eq 0
        new_case
        inst WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/tampered" --
        check "curl: a tampered archive over http is refused" test "$rc" -ne 0
        check "…nothing installed" test ! -e "$D"
        inst WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/nowhere" --
        check "curl: a 404 is refused with a hint" sh -c "[ '$rc' != 0 ] && printf '%s' '$out' | grep -q 'cannot download'"
        inst WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/nowhere" -- --version 1.2.3
        check "…and with a version, asks whether it is a Warden release" has "is v1.2.3 a Warden release"
        if command -v wget >/dev/null 2>&1; then
            mkpath "$T/path-wget" sha256sum shasum wget
            new_case
            inst PATH="$T/path-wget" WARDEN_DOWNLOAD_URL="http://127.0.0.1:$port/rel" --
            check "wget alone downloads too" ver_is "$D/warden" "warden 0.3.1"
        else
            note "wget is not installed: skipped"
        fi
    else
        fail "the local http server did not start (see $T/http.log)"
    fi
    kill "$srv" 2>/dev/null || true
    srv=""
else
    note "python3 or curl is missing: the http cases are skipped"
fi

# ============================================================ 17. other shells
echo "-- other shells"
for s in dash "bash --posix" bash ksh mksh "busybox sh"; do
    if ! command -v "${s%% *}" >/dev/null 2>&1; then continue; fi
    SH=$s
    new_case
    inst -- --gui
    check "$s: install --gui" sh -c "[ '$rc' = 0 ] && [ -x '$D/warden-gui' ] && [ -f '$X/applications/warden-gui.desktop' ]"
    inst_pipe -- --uninstall --gui
    check "$s: piped --uninstall --gui" sh -c "[ '$rc' = 0 ] && [ ! -e '$D/warden' ] && [ ! -e '$D/warden-gui' ]"
done
SH="sh"

# ============================================================ 18. the real platform and program
echo "-- this machine, no stubs"
new_case
# The newer of the release and debug builds (a stale one may lack what is tested).
real_bin=""
for c in "$root/target/release/warden" "$root/target/debug/warden"; do
    if [ -x "$c" ] && "$c" --version >/dev/null 2>&1 && { [ -z "$real_bin" ] || [ "$c" -nt "$real_bin" ]; }; then real_bin=$c; fi
done
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64 | Linux-aarch64 | Darwin-arm64 | Darwin-x86_64) ;;
    *) real_bin="" ;;
esac
if [ -z "$real_bin" ]; then
    note "no warden binary built that runs on this machine (cargo build --release): skipped"
elif [ "$(uname -s)" = Linux ] && ! getconf GNU_LIBC_VERSION >/dev/null 2>&1; then
    note "this Linux has no glibc: skipped"
else
    case "$(uname -s)" in Linux) hos=linux ;; *) hos=macos ;; esac
    case "$(uname -m)" in x86_64) harch=x86_64 ;; *) harch=arm64 ;; esac
    # Rosetta: an x86_64 shell on an Apple silicon Mac is installed as arm64, which this binary is not.
    if [ "$hos" = macos ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then harch=arm64; fi
    hv=$("$real_bin" --version | awk '{print $2}')
    rd=$T/rel-real
    n=warden-$hv-$hos-$harch
    mkdir -p "$rd" "$T/stage/$n/contrib"
    cp "$real_bin" "$T/stage/$n/warden"
    COPYFILE_DISABLE=1 tar -C "$T/stage" -czf "$rd/$n.tar.gz" "$n"
    sums "$rd"
    out=$(env -i PATH="$PATH" HOME="$H" TMPDIR="$T/tmp" WARDEN_DOWNLOAD_URL="file://$rd" WARDEN_INSTALL_DIR="$D" \
        WARDEN_RUNTIME_DIR="$T/run" WARDEN_HOME="$T/wardenhome" WARDEN_NO_DAEMON=1 sh "$installer" </dev/null 2>&1)
    rc=$?
    check "the real warden is installed for the real platform ($hos-$harch), exit 0" test "$rc" -eq 0
    check "…and it runs and says warden $hv" ver_is "$D/warden" "warden $hv"
    check "…and the installed program answers --help" sh -c "env -i HOME='$H' WARDEN_RUNTIME_DIR='$T/run' WARDEN_HOME='$T/wardenhome' WARDEN_NO_DAEMON=1 '$D/warden' --help >/dev/null"
    # warden gui-install: the install.sh built into it, for its own version,
    # against a release of that version with a stand-in GUI (Linux: a Mac's
    # real codesign would refuse the unsigned stand-in app).
    if ! "$D/warden" --help 2>/dev/null | grep -q '^ *gui-install '; then
        note "this warden has no gui-install command (an old build? cargo build): warden gui-install is not tested"
    elif [ "$hos" != linux ]; then
        note "warden gui-install is tested on Linux only"
    elif [ "$(getconf GNU_LIBC_VERSION | awk '{ split($2, v, "."); print (v[1] > 2 || (v[1] == 2 && v[2] >= 35)) ? "yes" : "no" }')" != yes ]; then
        note "glibc older than 2.35: no GUI here, warden gui-install is not tested"
    else
        gn=warden-gui-$hv-linux-$harch
        mkdir -p "$T/stage/$gn"
        cp "$real_bin" "$T/stage/$gn/warden"
        stub_program "$T/stage/$gn/warden-gui" warden-gui "$hv" ok
        printf '[Desktop Entry]\nType=Application\nName=Warden\nExec=warden-gui\nIcon=warden\nTerminal=false\n' >"$T/stage/$gn/warden-gui.desktop"
        printf 'PNG' >"$T/stage/$gn/warden.png"
        COPYFILE_DISABLE=1 tar -C "$T/stage" -czf "$rd/$gn.tar.gz" "$gn"
        sums "$rd"
        gi() { # warden gui-install ARGS…, as this user, against the release in $rd
            out=$(env -i PATH="$PATH" HOME="$H" TMPDIR="$T/tmp" XDG_DATA_HOME="$X" WARDEN_DOWNLOAD_URL="file://$rd" \
                WARDEN_RUNTIME_DIR="$T/run" WARDEN_HOME="$T/wardenhome" WARDEN_NO_DAEMON=1 "$D/warden" gui-install "$@" </dev/null 2>&1)
            rc=$?
        }
        gi --dry-run
        check "warden gui-install --dry-run: the GUI of its own version" sh -c "[ '$rc' = 0 ] && printf '%s' \"\$0\" | grep -q '$gn.tar.gz'" "$out"
        check "…installing nothing" test ! -e "$D/warden-gui"
        gi
        check "warden gui-install installs the GUI next to it" ver_is "$D/warden-gui" "warden-gui $hv"
        check "…with its menu entry" file_has "$X/applications/warden-gui.desktop" "Exec=$D/warden-gui"
        check "…and leaves the CLI as it was" cmp -s "$D/warden" "$real_bin"
        gi --uninstall
        check "warden gui-install --uninstall removes the GUI" sh -c "[ '$rc' = 0 ] && [ ! -e '$D/warden-gui' ] && [ ! -e '$X/applications/warden-gui.desktop' ]"
        check "…and leaves the CLI" cmp -s "$D/warden" "$real_bin"
    fi
fi

# ============================================================ 19. install-gui.sh
echo "-- install-gui.sh ($gui_installer)"
check "sh -n" sh -n "$gui_installer"
if command -v shellcheck >/dev/null 2>&1; then check "shellcheck" shellcheck "$gui_installer"; else note "shellcheck is not installed: skipped"; fi
check "it is a POSIX sh script (#!/bin/sh)" test "$(head -n 1 "$gui_installer")" = '#!/bin/sh'
check "its last line calls main with the end marker" test "$(tail -n 1 "$gui_installer")" = 'main "$@" --end-of-install-script'
check "it never reads its standard input (no read command)" sh -c "! grep -Ev '^[[:space:]]*#' '$gui_installer' | grep -Eq '(^|[;&| ])read( |\$)'"
# A release has install.sh in it, listed in SHA256SUMS.
GREL=$T/rel-gui
cp -R "$REL" "$GREL"
cp "$installer" "$GREL/install.sh"
sums "$GREL"
cp -R "$GREL" "$T/rel-gui-tampered"
printf '\n# changed on the way\n' >>"$T/rel-gui-tampered/install.sh"
cp -R "$REL" "$T/rel-gui-noinstaller"
RUN=$gui_installer
new_case
inst WARDEN_DOWNLOAD_URL="file://$GREL" --
check "install-gui.sh installs the CLI" ver_is "$D/warden" "warden 0.3.1"
check "…and the GUI" ver_is "$D/warden-gui" "warden-gui 0.3.1"
check "…with its menu entry" file_has "$X/applications/warden-gui.desktop" "Exec=$D/warden-gui"
check "…and its temporary folder is gone" scratch_clean
inst WARDEN_DOWNLOAD_URL="file://$GREL" -- --uninstall
check "install-gui.sh --uninstall removes both" sh -c "[ '$rc' = 0 ] && [ ! -e '$D/warden' ] && [ ! -e '$D/warden-gui' ]"
new_case
inst WARDEN_DOWNLOAD_URL="file://$GREL" -- --dry-run
check "options go on to install.sh: --dry-run" sh -c "[ '$rc' = 0 ] && [ ! -e '$D' ]"
check "…which lists the GUI's files" has "warden-gui-0.3.1-linux-x86_64.tar.gz"
inst WARDEN_DOWNLOAD_URL="file://$GREL" -- --help
check "--help: what it is" has "Install Warden's GUI"
check "…and install.sh's options" has "--gui-only"
inst WARDEN_DOWNLOAD_URL="file://$GREL" FAKE_OS=Darwin FAKE_ARCH=arm64 --
check "macOS: Warden.app and the CLI" sh -c "[ '$rc' = 0 ] && [ -d '$A/Warden.app' ] && [ -x '$D/warden' ]"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-gui-tampered" --
check "an install.sh that does not match SHA256SUMS is refused" fail_has "checksum mismatch for install.sh"
check "…before anything is installed" test ! -e "$D"
inst WARDEN_DOWNLOAD_URL="file://$T/rel-gui-tampered" -- --no-verify
check "…--no-verify runs it all the same, saying so" sh -c "[ '$rc' = 0 ] && [ -x '$D/warden-gui' ] && printf '%s' \"\$0\" | grep -q 'NOT checked'" "$out"
new_case
inst WARDEN_DOWNLOAD_URL="file://$T/rel-gui-noinstaller" --
check "a release without install.sh is refused" fail_has "does not list install.sh"
inst WARDEN_DOWNLOAD_URL="file://$T/does-not-exist" --
check "an unreachable release is refused" fail_has "cannot download"
check "…nothing installed" test ! -e "$D"
new_case
inst_pipe WARDEN_DOWNLOAD_URL="file://$GREL" -- --dir "$D"
check "piped into sh (curl | sh): installs both" sh -c "[ '$rc' = 0 ] && [ -x '$D/warden' ] && [ -x '$D/warden-gui' ]"
new_case
size=$(wc -c <"$gui_installer" | tr -d ' ')
bad=""
for off in 0 $((size / 4)) $((size / 2)) $((size * 3 / 4)) $((size - 25)) $((size - 2)); do
    rm -rf "$D"
    cut_out=$(head -c "$off" "$gui_installer" | env -i PATH="$bin:$PATH" HOME="$H" TMPDIR="$T/tmp" FAKE_LOG="$T/stub.log" \
        XDG_DATA_HOME="$X" WARDEN_DOWNLOAD_URL="file://$GREL" WARDEN_INSTALL_DIR="$D" WARDEN_NO_DAEMON=1 sh -s -- 2>&1)
    if [ -e "$D" ]; then bad="$bad $off"; fi
done
if [ -z "$bad" ]; then pass "a script cut short installs nothing"; else fail "a script cut at byte$bad installed something"; fi
check "…saying it is incomplete" has_in "$cut_out" "this script is incomplete"
new_case
inst PATH="$T/curlbin:$bin:$PATH" FAKE_RELEASE="$GREL" WARDEN_DOWNLOAD_URL= --
check "GitHub's URLs: installs both" sh -c "[ '$rc' = 0 ] && [ -x '$D/warden' ] && [ -x '$D/warden-gui' ]"
check "…SHA256SUMS from the latest release" file_has "$T/stub.log" "curl $base/latest/download/SHA256SUMS"
check "…install.sh from the very release that SHA256SUMS is of" file_has "$T/stub.log" "curl $base/download/v0.3.1/install.sh"
check "…and nothing else from 'latest' (install.sh is told the version)" test "$(grep -c "latest/download" "$T/stub.log")" = 1
new_case
inst PATH="$T/curlbin:$bin:$PATH" FAKE_RELEASE="$GREL" WARDEN_DOWNLOAD_URL= -- --version v0.3.1
check "--version: everything from that tag" sh -c "[ '$rc' = 0 ] && grep -q 'curl $base/download/v0.3.1/install.sh' '$T/stub.log' && ! grep -q latest '$T/stub.log'"
RUN=$installer

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails check(s) failed. The temp folder is kept: $T"
    keep=1
    exit 1
fi
echo "all checks passed"

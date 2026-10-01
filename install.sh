#!/bin/sh
# Install the Warden CLI from a GitHub release: download the archive for this
# OS and CPU, check it against the release's SHA256SUMS, put `warden` on disk.
#
#   sh install.sh                         # the latest release
#   WARDEN_VERSION=0.2.0 sh install.sh    # a given release
#
# Installs to /usr/local/bin as root, else ~/.local/bin (WARDEN_INSTALL_DIR
# overrides). WARDEN_DOWNLOAD_URL points at a directory holding the release
# files instead of GitHub (a mirror, or CI testing unreleased builds).
set -eu

REPO="oceanwap/warden"

fail() {
    echo "install.sh: $*" >&2
    exit 1
}

say() {
    echo "install.sh: $*"
}

# ------------------------------------------------------------ platform

case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=macos ;;
    *) fail "no prebuilt Warden for $(uname -s): Linux and macOS only (WSL2 counts as Linux); build from source with cargo" ;;
esac

case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) fail "no prebuilt Warden for $(uname -m) CPUs: x86_64 and arm64 only; build from source with cargo" ;;
esac

# A shell running under Rosetta reports x86_64 on an Apple Silicon Mac.
if [ "$os" = macos ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    arch=arm64
fi

# The Linux binaries link glibc 2.28 or newer (RHEL 8, Debian 10, Ubuntu 18.10).
if [ "$os" = linux ]; then
    if ldd --version 2>&1 | grep -qi musl; then
        fail "this system uses musl libc (Alpine?); Warden's Linux binaries need glibc 2.28+. Build from source with cargo, or use a glibc-based image"
    fi
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || glibc=""
    if [ -n "$glibc" ]; then
        major=${glibc%%.*}
        minor=${glibc#*.}
        minor=${minor%%.*}
        if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 28 ]; }; then
            fail "glibc $glibc is too old: Warden's Linux binaries need glibc 2.28+ (RHEL 8, Debian 10, Ubuntu 18.10 or newer)"
        fi
    fi
fi

# ------------------------------------------------------------ tools

if command -v curl >/dev/null 2>&1; then
    fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
    fetch() { wget -q -O "$2" "$1"; }
else
    fail "needs curl or wget to download the release"
fi

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    sha256() { shasum -a 256 "$1" | awk '{print $1}'; }
elif command -v openssl >/dev/null 2>&1; then
    sha256() { openssl dgst -sha256 "$1" | awk '{print $NF}'; }
else
    fail "needs sha256sum, shasum or openssl to verify the download"
fi

# ------------------------------------------------------------ download

version=${WARDEN_VERSION:-}
version=${version#v}
if [ -n "${WARDEN_DOWNLOAD_URL:-}" ]; then
    base=${WARDEN_DOWNLOAD_URL%/}
elif [ -n "$version" ]; then
    base="https://github.com/$REPO/releases/download/v$version"
else
    base="https://github.com/$REPO/releases/latest/download"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t warden)
trap 'rm -rf "$tmp"' EXIT
trap 'exit 1' INT TERM

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
    fail "cannot download $base/SHA256SUMS${version:+ (is v$version a Warden release?)}"

# The CLI archive for this platform ("warden-gui-…" is the GUI bundle).
if [ -n "$version" ]; then
    want="warden-$version-$os-$arch.tar.gz"
    line=$(awk -v f="$want" '$2 == f || $2 == "*" f' "$tmp/SHA256SUMS")
else
    line=$(awk -v s="-$os-$arch.tar.gz" '$2 ~ /^\*?warden-[0-9]/ && substr($2, length($2) - length(s) + 1) == s' "$tmp/SHA256SUMS")
fi
[ -n "$line" ] || fail "no $os-$arch archive in $base/SHA256SUMS"
[ "$(echo "$line" | wc -l)" -eq 1 ] || fail "more than one $os-$arch archive in $base/SHA256SUMS"
expected=$(echo "$line" | awk '{print $1}')
archive=$(echo "$line" | awk '{print $2}')
archive=${archive#\*}
name=${archive%.tar.gz}
[ -n "$version" ] || version=$(echo "$name" | sed "s/^warden-//; s/-$os-$arch\$//")

say "downloading $archive"
fetch "$base/$archive" "$tmp/$archive" || fail "cannot download $base/$archive"
actual=$(sha256 "$tmp/$archive")
[ "$actual" = "$expected" ] ||
    fail "checksum mismatch for $archive: expected $expected, got $actual; nothing was installed (retry, or report it)"
say "checksum OK ($expected)"

tar -xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$name/warden" ] || fail "$archive has no $name/warden"

# ------------------------------------------------------------ install

if [ -n "${WARDEN_INSTALL_DIR:-}" ]; then
    dir=$WARDEN_INSTALL_DIR
elif [ "$(id -u)" -eq 0 ]; then
    dir=/usr/local/bin
else
    dir="$HOME/.local/bin"
fi
mkdir -p "$dir" || fail "cannot create $dir (set WARDEN_INSTALL_DIR, or run as root for /usr/local/bin)"

# Copy next to the target, then rename over it: a running warden keeps its
# old file, and nobody ever sees a half-written binary.
cp "$tmp/$name/warden" "$dir/.warden.new.$$" || fail "cannot write to $dir (run as root, or set WARDEN_INSTALL_DIR)"
chmod 0755 "$dir/.warden.new.$$"
mv -f "$dir/.warden.new.$$" "$dir/warden"

installed=$("$dir/warden" --version 2>/dev/null) || fail "$dir/warden was installed but does not run on this system"
say "installed $installed to $dir/warden"

case ":$PATH:" in
    *":$dir:"*) cmd=warden ;;
    *)
        cmd="$dir/warden"
        say "$dir is not on your PATH; add it, e.g.: export PATH=\"$dir:\$PATH\""
        ;;
esac

cat <<EOF

Next steps:
  $cmd doctor     check this machine (kernel, runtimes, limits)
  $cmd startup    start your saved apps at boot
  $cmd --help     everything else

The archive also has the systemd units, the sysctl file and an nginx example (contrib/):
  $base/$archive
EOF

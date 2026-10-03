#!/bin/sh
# Install Warden's GUI and the `warden` CLI it runs on, from a GitHub release:
#
#   curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install-gui.sh | sh
#   sh install-gui.sh --version v0.2.0 --dry-run
#
# macOS: Warden.app in /Applications (or ~/Applications) and `warden` in
# /usr/local/bin (as root) or ~/.local/bin. Linux: warden-gui and warden in the
# same folder, with a menu entry and an icon.
#
# This is install.sh with --gui: it downloads the release's install.sh, checks
# it against the release's SHA256SUMS, and runs it, passing every option on
# (`sh install-gui.sh --help` lists them). The manual is docs/install.md.
#
# Safe under `curl | sh`, like install.sh: nothing is read from standard input,
# and the last line runs it all, so a download cut short runs nothing.
set -eu

say() { printf 'install-gui.sh: %s\n' "$*"; }
fail() {
    printf 'install-gui.sh: %s\n' "$*" >&2
    exit 1
}

fetch() { # url dest
    case $1 in
        file://*) cp "$(printf '%s' "${1#file://}" | sed 's/%20/ /g; s/%25/%/g')" "$2" 2>/dev/null ;;
        *)
            if command -v curl >/dev/null 2>&1; then
                curl -fsSL --retry 3 -o "$2" "$1"
            elif command -v wget >/dev/null 2>&1; then
                wget -q -O "$2" "$1"
            else
                fail "needs curl or wget to download the release"
            fi
            ;;
    esac
}

sha256() { # file -> its hash
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        fail "needs sha256sum, shasum or openssl to check install.sh (or pass --no-verify, which is discouraged)"
    fi
}

main() {
    last=""
    for a in "$@"; do last=$a; done
    if [ "$last" != --end-of-install-script ]; then
        printf 'install-gui.sh: this script is incomplete (was the download cut short?): nothing was done. Download it again.\n' >&2
        exit 1
    fi
    n=$#
    i=0
    for a in "$@"; do
        i=$((i + 1))
        if [ "$i" -lt "$n" ]; then set -- "$@" "$a"; fi
    done
    shift "$n"

    REPO="oceanwap/warden"
    # Of install.sh's options, only what picks the release and the check
    # matters here; install.sh reads them all again.
    version=${WARDEN_VERSION:-}
    verify=1
    prev=""
    for a in "$@"; do
        if [ "$prev" = --version ]; then version=$a; fi
        case $a in
            --version=*) version=${a#--version=} ;;
            --no-verify) verify=0 ;;
            -h | --help)
                cat <<'EOF'
Install Warden's GUI and the warden CLI it needs: install.sh --gui, from the
release's own install.sh (checked against its SHA256SUMS first).

Usage: install-gui.sh [OPTIONS]    (every option of install.sh, below)

EOF
                ;;
        esac
        prev=$a
    done
    case $version in latest) version="" ;; esac
    version=${version#v}
    case $version in
        *[!0-9A-Za-z.+-]*) fail "'$version' is not a version: use 0.2.0 or v0.2.0" ;;
    esac

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t warden) || fail "cannot create a temporary folder (is TMPDIR writable?)"
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 1' INT TERM HUP

    pin=""
    if [ -n "${WARDEN_DOWNLOAD_URL:-}" ]; then
        base=${WARDEN_DOWNLOAD_URL%/}
    elif [ -n "$version" ]; then
        base="https://github.com/$REPO/releases/download/v$version"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi

    if [ "$verify" = 1 ]; then
        fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" ||
            fail "cannot download $base/SHA256SUMS${version:+ (is v$version a Warden release?)}. The releases are at https://github.com/$REPO/releases"
        # "latest" moves when a release is published: take install.sh from the
        # release this SHA256SUMS belongs to.
        if [ -z "$version" ] && [ -z "${WARDEN_DOWNLOAD_URL:-}" ]; then
            found=$(awk '{ f = $2; sub(/^\*/, "", f) }
                f ~ /^warden-[0-9].*-(linux|macos)-(x86_64|arm64)\.tar\.gz$/ {
                    sub(/^warden-/, "", f); sub(/-(linux|macos)-(x86_64|arm64)\.tar\.gz$/, "", f); print f; exit
                }' "$tmp/SHA256SUMS")
            if [ -n "$found" ]; then
                base="https://github.com/$REPO/releases/download/v$found"
                pin="--version=$found"
            fi
        fi
        want=$(awk '$2 == "install.sh" || $2 == "*install.sh" { print $1 }' "$tmp/SHA256SUMS")
        [ -n "$want" ] || fail "$base/SHA256SUMS does not list install.sh: this release has no installer to check"
    fi

    fetch "$base/install.sh" "$tmp/install.sh" || fail "cannot download $base/install.sh"
    if [ "$verify" = 1 ]; then
        got=$(sha256 "$tmp/install.sh")
        if [ "$got" != "$want" ]; then
            fail "checksum mismatch for install.sh:
      expected  $want   (from SHA256SUMS)
      got       $got
    Nothing was installed. Try again in a minute; if it keeps failing, report it at https://github.com/$REPO/issues"
        fi
    else
        say "install.sh was NOT checked against SHA256SUMS (--no-verify)"
    fi

    # With "latest" pinned above, install.sh installs that same release.
    sh "$tmp/install.sh" --gui ${pin:+"$pin"} "$@"
}

main "$@" --end-of-install-script

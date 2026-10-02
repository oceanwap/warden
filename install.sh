#!/bin/sh
# Install Warden from a GitHub release: download the archive for this OS and
# CPU, check it against the release's SHA256SUMS, put `warden` on disk.
#
#   curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/oceanwap/warden/main/install.sh | sh -s -- --gui
#   sh install.sh --version v0.2.0 --dry-run
#
# `sh install.sh --help` lists the options and the environment variables. The
# manual is docs/install.md.
#
# Safe under `curl | sh`: the script never reads from its standard input (it
# asks nothing), and every command lives in a function that the last line
# calls, so a download cut short runs nothing.
#
# NOTE: GitHub Releases are not published yet. Until the first release exists,
# this script will fail when it cannot download SHA256SUMS. Prefer building
# from source: `cargo build --release` (see docs/development.md).
set -eu

usage() {
    cat <<'EOF'
Install Warden, a supervisor for Bun and Node apps.

Usage: install.sh [OPTIONS]

  curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh
  curl -fsSL https://github.com/oceanwap/warden/releases/latest/download/install.sh | sh -s -- --gui

Options:
  --version VERSION   Install this release (v0.2.0 or 0.2.0), not the latest
  --gui               Also install the GUI. macOS: Warden.app in /Applications
                      (or ~/Applications). Linux: warden-gui, a menu entry and an
                      icon (~/.local/share, or /usr/local/share as root)
  --uninstall         Remove what this installer installs (with --gui: the GUI
                      too). Your apps, their configs and saved state stay
  --prefix DIR        Put the binaries in DIR (default: /usr/local/bin as root,
  --dir DIR           else ~/.local/bin); the same as WARDEN_INSTALL_DIR
  --app-dir DIR       macOS GUI: the folder for Warden.app; same as WARDEN_APP_DIR
  --modify-path       Add DIR to PATH in your shell's startup file (bash, zsh or
                      fish) when it is not there. Without it no file of yours is
                      edited: the line to add is printed
  --dry-run           Say what would happen; install nothing (SHA256SUMS is still
                      downloaded, to find the release's files)
  --no-verify         Do not check the download against SHA256SUMS. DISCOURAGED
  -h, --help          This help

Environment:
  WARDEN_VERSION       the release to install, as --version
  WARDEN_INSTALL_DIR   as --dir
  WARDEN_APP_DIR       as --app-dir
  WARDEN_DOWNLOAD_URL  a folder (https:// or file://) holding the release files
                       instead of GitHub: a mirror, or an unreleased build
  XDG_DATA_HOME        Linux GUI: where the menu entry and icon go

Run `warden update` after an upgrade: running apps keep the old version until
their supervisors restart. Manual: docs/install.md
EOF
}

say() { printf 'install.sh: %s\n' "$*"; }
warn() { printf 'install.sh: warning: %s\n' "$*" >&2; }
fail() {
    printf 'install.sh: %s\n' "$*" >&2
    exit 1
}
usage_error() {
    printf 'install.sh: %s\nTry: sh install.sh --help\n' "$*" >&2
    exit 2
}

cleanup() {
    if [ "$mounted" = 1 ] && [ -n "$mnt" ]; then
        hdiutil detach "$mnt" -quiet >/dev/null 2>&1 || hdiutil detach "$mnt" -force -quiet >/dev/null 2>&1 || true
        mounted=0
    fi
    if [ -n "$stage" ]; then rm -rf "$stage"; fi
    if [ -n "$stage2" ]; then rm -rf "$stage2"; fi
    if [ -n "$tmp" ]; then rm -rf "$tmp"; fi
}

# ------------------------------------------------------------ arguments

need_value() { # option value-or-empty
    if [ -z "${2:-}" ]; then usage_error "$1 needs a value"; fi
}

parse_args() {
    while [ $# -gt 0 ]; do
        case $1 in
            -h | --help)
                usage
                exit 0
                ;;
            --version)
                if [ $# -lt 2 ]; then usage_error "--version needs a release, e.g. --version v0.2.0"; fi
                version=$2
                shift
                ;;
            --version=*) version=${1#--version=} ;;
            --gui) gui=1 ;;
            --uninstall) uninstall=1 ;;
            --prefix | --dir)
                need_value "$1" "${2:-}"
                dir_opt=$2
                shift
                ;;
            --prefix=* | --dir=*)
                dir_opt=${1#*=}
                need_value "${1%%=*}" "$dir_opt"
                ;;
            --app-dir)
                need_value "$1" "${2:-}"
                app_dir_opt=$2
                shift
                ;;
            --app-dir=*)
                app_dir_opt=${1#--app-dir=}
                need_value --app-dir "$app_dir_opt"
                ;;
            --modify-path) modify_path=1 ;;
            --dry-run) dry=1 ;;
            --no-verify) verify=0 ;;
            *) usage_error "unknown option '$1'" ;;
        esac
        shift
    done

    case $version in latest) version="" ;; esac
    version=${version#v}
    if [ -n "$version" ] && ! printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.+-]+)?$'; then
        usage_error "'$version' is not a version: use 0.2.0 or v0.2.0 (the releases are at https://github.com/$REPO/releases)"
    fi
}

# ------------------------------------------------------------ platform

detect_platform() {
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

    libc=""
    glibc=""
    if [ "$os" = linux ]; then
        # glibc answers getconf; musl (Alpine) does not, and says so in ldd.
        glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | awk '{print $2}') || glibc=""
        if [ -n "$glibc" ]; then
            libc=glibc
        elif ldd --version 2>&1 | grep -qi musl; then
            libc=musl
        else
            libc=unknown
        fi
    fi
}

# Warden's Linux binaries link glibc 2.28 or newer (RHEL 8, Debian 10, Ubuntu
# 18.10); the GUI, built on Ubuntu 22.04, glibc 2.35 or newer. Nothing is built
# for musl, so there is no archive to fall back to: say what to do instead.
check_libc() {
    if [ "$os" != linux ]; then return 0; fi
    case $libc in
        musl)
            fail "this system uses musl libc (Alpine?), and Warden's Linux binaries are built for glibc 2.28+: there is no musl build.
    Use a glibc-based image or host (Debian, Ubuntu, Fedora, RHEL 8+), or build from source:
    git clone https://github.com/$REPO && cd warden && cargo build --release"
            ;;
        unknown)
            warn "cannot tell which C library this system uses; assuming glibc 2.28 or newer (the release binaries are built for it)"
            return 0
            ;;
    esac
    major=${glibc%%.*}
    minor=${glibc#*.}
    minor=${minor%%.*}
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 28 ]; }; then
        fail "glibc $glibc is too old: Warden's Linux binaries need glibc 2.28+ (RHEL 8, Debian 10, Ubuntu 18.10 or newer). Build from source with cargo instead"
    fi
    if [ "$gui" = 1 ] && { [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 35 ]; }; }; then
        fail "glibc $glibc is too old for the GUI, which needs glibc 2.35+ (Ubuntu 22.04, Debian 12, Fedora 36 or newer). The CLI alone works here: leave out --gui"
    fi
}

# ------------------------------------------------------------ where things go

# A folder as the user typed it, made absolute.
absolute() { # path -> prints it, ~ and relative paths resolved
    # shellcheck disable=SC2088 # a literal ~/ (as in --dir=~/bin), which the shell did not expand
    case $1 in
        "~") printf '%s\n' "${HOME:-/}" ;;
        "~/"*) printf '%s\n' "${HOME:-}/${1#"~/"}" ;;
        /*) printf '%s\n' "$1" ;;
        *) printf '%s\n' "$PWD/$1" ;;
    esac
}

need_home() {
    if [ -z "${HOME:-}" ]; then fail "HOME is not set: say where to install with --dir DIR"; fi
}

resolve_dirs() {
    is_root=0
    if [ "$(id -u)" -eq 0 ]; then is_root=1; fi

    dir=${dir_opt:-${WARDEN_INSTALL_DIR:-}}
    if [ -z "$dir" ]; then
        if [ "$is_root" = 1 ]; then
            dir=/usr/local/bin
        else
            need_home
            dir=$HOME/.local/bin
        fi
    fi
    dir=$(absolute "$dir")
    if [ "$dir" != / ]; then dir=${dir%/}; fi

    # GUI: the macOS app folder, the Linux menu entry and icon.
    data_dir=""
    app_dir=""
    app_dir2=""
    if [ "$os" = macos ]; then
        if [ -n "$app_dir_opt" ]; then
            app_dir=$(absolute "$app_dir_opt")
        elif [ "$is_root" = 1 ] || [ -w /Applications ]; then
            app_dir=/Applications
        else
            need_home
            app_dir=$HOME/Applications
        fi
        app_dir=${app_dir%/}
        # Where --uninstall also looks when no folder was given.
        if [ -z "$app_dir_opt" ] && [ "$app_dir" = /Applications ] && [ -n "${HOME:-}" ]; then
            app_dir2=$HOME/Applications
        fi
    else
        case ${XDG_DATA_HOME:-} in
            /*) data_dir=${XDG_DATA_HOME%/} ;;
            *)
                if [ "$is_root" = 1 ]; then
                    data_dir=/usr/local/share
                else
                    need_home
                    data_dir=$HOME/.local/share
                fi
                ;;
        esac
    fi
}

on_path() { # dir
    case ":$PATH:" in
        *":$1:"* | *":$1/:"*) return 0 ;;
    esac
    return 1
}

# ------------------------------------------------------------ PATH hints and edits

shell_name() {
    s=${SHELL:-}
    s=${s##*/}
    case $s in
        bash | zsh | fish) printf '%s\n' "$s" ;;
        *) printf 'sh\n' ;;
    esac
}

# The line that puts $dir on PATH, for the shell: $HOME stays literal.
path_line() {
    d=$dir
    if [ -n "${HOME:-}" ]; then
        case $d in
            "$HOME"/*) d="\$HOME/${d#"$HOME"/}" ;;
        esac
    fi
    case "$(shell_name)" in
        fish) printf 'fish_add_path "%s"\n' "$d" ;;
        *) printf 'export PATH="%s:%s"\n' "$d" "\$PATH" ;;
    esac
}

# The startup file for that line, and a shortened name for messages.
profile_file() {
    if [ -z "${HOME:-}" ]; then
        printf 'your shell startup file\n'
        return 0
    fi
    case "$(shell_name)" in
        zsh) printf '%s\n' "${ZDOTDIR:-$HOME}/.zshrc" ;;
        bash)
            if [ "$os" = macos ]; then printf '%s\n' "$HOME/.bash_profile"; else printf '%s\n' "$HOME/.bashrc"; fi
            ;;
        fish) printf '%s\n' "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/warden.fish" ;;
        *) printf '%s\n' "$HOME/.profile" ;;
    esac
}

tilde() { # path -> ~/path when under $HOME
    case $1 in
        "${HOME:-/nonexistent}"/*) printf '%s/%s\n' '~' "${1#"$HOME"/}" ;;
        *) printf '%s\n' "$1" ;;
    esac
}

path_hint() {
    if on_path "$dir"; then return 0; fi
    line=$(path_line)
    if [ "$modify_path" = 1 ]; then
        need_home
        pf=$(profile_file)
        if [ -f "$pf" ] && grep -qxF "$line" "$pf"; then
            say "$(tilde "$pf") already puts $dir on PATH"
        else
            mkdir -p "$(dirname "$pf")"
            if [ -s "$pf" ]; then printf '\n' >>"$pf"; fi
            printf '%s\n%s\n' "$MARK" "$line" >>"$pf"
            say "added $dir to PATH in $(tilde "$pf")"
        fi
        say "open a new terminal, or run this one now:  $line"
        return 0
    fi
    if [ "$(shell_name)" = fish ]; then
        cat <<EOF

$dir is not on your PATH, so "warden" is not found yet. In fish, once:

    $line

(install.sh --modify-path writes it to a file of fish's instead; nothing was changed in your files.)
EOF
        return 0
    fi
    pf=$(tilde "$(profile_file)")
    cat <<EOF

$dir is not on your PATH, so "warden" is not found yet. Add it for new shells
($(shell_name)) with:

    echo '$line' >> $pf

then open a new terminal; or in this one, run:

    $line

(install.sh --modify-path makes that edit for you; nothing was changed in your files.)
EOF
}

# Take our lines out of a startup file: the marker and the line after it.
unmodify_path() { # file
    f=$1
    if [ ! -f "$f" ] || ! grep -qxF "$MARK" "$f"; then return 0; fi
    # The marker, the line after it, and the blank line before it (ours too).
    awk -v m="$MARK" '
        {
            if (skip) { skip = 0; next }
            if (held) {
                if ($0 == m) { held = 0; skip = 1; next }
                print ""
                held = 0
            }
            if ($0 == "") { held = 1; next }
            if ($0 == m) { skip = 1; next }
            print
        }
        END { if (held) print "" }
    ' "$f" >"$tmp/profile.new"
    if [ "$dry" = 1 ]; then
        say "would remove the PATH line this installer added to $(tilde "$f")"
        return 0
    fi
    # cat, not mv: keeps the file's permissions, owner and any symlink.
    cat "$tmp/profile.new" >"$f"
    say "removed the PATH line this installer added to $(tilde "$f")"
    # fish: a file of our own, now empty (or only blank lines).
    if [ "$(basename "$f")" = warden.fish ] && ! grep -q '[^[:space:]]' "$f"; then rm -f "$f"; fi
}

# ------------------------------------------------------------ tools

find_tools() {
    for t in awk sed tar mktemp grep; do
        command -v "$t" >/dev/null 2>&1 || fail "needs '$t', which is not installed"
    done

    if command -v curl >/dev/null 2>&1; then
        dl=curl
    elif command -v wget >/dev/null 2>&1; then
        dl=wget
    else
        dl=none
    fi

    sumtool=""
    if command -v sha256sum >/dev/null 2>&1; then
        sumtool=sha256sum
    elif command -v shasum >/dev/null 2>&1; then
        sumtool=shasum
    elif command -v openssl >/dev/null 2>&1; then
        sumtool=openssl
    fi
    if [ -z "$sumtool" ] && [ "$verify" = 1 ]; then
        fail "needs sha256sum, shasum or openssl to check the download. Install one (macOS has shasum; Linux: coreutils), or pass --no-verify, which is discouraged"
    fi
}

fetch() { # url dest [big]
    case $1 in
        file://*)
            # A folder on this machine (a mirror, a test): the path may be %-encoded.
            fsrc=$(printf '%s' "${1#file://}" | sed 's/%20/ /g; s/%25/%/g')
            cp "$fsrc" "$2" 2>/dev/null
            ;;
        *)
            case $dl in
                curl)
                    if [ "${3:-}" = big ] && [ -t 2 ]; then
                        curl -fL# --retry 3 -o "$2" "$1"
                    else
                        curl -fsSL --retry 3 -o "$2" "$1"
                    fi
                    ;;
                wget) wget -q -O "$2" "$1" ;;
                *) fail "needs curl or wget to download the release (install one, or download the files yourself: docs/install.md)" ;;
            esac
            ;;
    esac
}

sha256() { # file -> its hash
    case $sumtool in
        sha256sum) sha256sum "$1" | awk '{print $1}' ;;
        shasum) shasum -a 256 "$1" | awk '{print $1}' ;;
        *) openssl dgst -sha256 "$1" | awk '{print $NF}' ;;
    esac
}

# ------------------------------------------------------------ the release

# The hash SHA256SUMS lists for a file, in want_hash. Returns 1 when the file
# is not listed. Without SHA256SUMS (--no-verify only) there is nothing to find.
sums_find() { # file
    want_hash=""
    if [ "$have_sums" = 0 ]; then return 0; fi
    want_hash=$(awk -v f="$1" '$2 == f || $2 == "*" f { print $1 }' "$tmp/SHA256SUMS")
    if [ -z "$want_hash" ]; then return 1; fi
    if [ "${#want_hash}" -ne 64 ] || printf '%s' "$want_hash" | grep -q '[^0-9a-fA-F]'; then
        fail "SHA256SUMS in $base lists $1 more than once, or is malformed"
    fi
}

# What the release has for this platform, for a message.
sums_listing() {
    if [ "$have_sums" = 0 ]; then return 0; fi
    awk '{ f = $2; sub(/^\*/, "", f); if (f ~ /^[Ww]arden/) print "      " f }' "$tmp/SHA256SUMS"
}

# Unpinned: the version is the one in the name of the CLI archive for this platform.
discover_version() {
    sfx="-$os-$arch.tar.gz"
    names=$(awk -v s="$sfx" '{ f = $2; sub(/^\*/, "", f) } f ~ /^warden-[0-9]/ && substr(f, length(f) - length(s) + 1) == s { print f }' "$tmp/SHA256SUMS")
    if [ -z "$names" ]; then
        fail "no $os-$arch archive in $base/SHA256SUMS. It lists:
$(sums_listing)
    Build from source instead: git clone https://github.com/$REPO && cd warden && cargo build --release"
    fi
    if [ "$(printf '%s\n' "$names" | wc -l | tr -d ' ')" -ne 1 ]; then
        fail "more than one $os-$arch archive in $base/SHA256SUMS: pick one with --version"
    fi
    version=${names#warden-}
    version=${version%"$sfx"}
}

bad_checksum() { # file expected actual
    fail "checksum mismatch for $1:
      expected  $2   (from $base/SHA256SUMS)
      got       $3
    Nothing was installed. The file was damaged on the way, or the server holds files
    that do not match its SHA256SUMS (a mirror that is out of date, a proxy cache).
    Try again in a minute. If it keeps failing, do not install it: report it at
    https://github.com/$REPO/issues. (--no-verify skips this check; that is discouraged.)"
}

# Download a file of the release into $tmp and check it (want_hash is its line).
get_file() { # file
    say "downloading $1"
    fetch "$base/$1" "$tmp/$1" big || fail "cannot download $base/$1"
    if [ "$verify" = 1 ]; then
        actual=$(sha256 "$tmp/$1")
        if [ "$actual" != "$want_hash" ]; then bad_checksum "$1" "$want_hash" "$actual"; fi
        say "checksum OK ($actual)"
    else
        warn "$1 was NOT checked against SHA256SUMS (--no-verify)"
    fi
}

no_verify_banner() {
    cat >&2 <<'EOF'

install.sh: !! WARNING: --no-verify !!
install.sh: The download is NOT checked against SHA256SUMS. If the file is damaged or
install.sh: was swapped on the way, it is installed and run all the same. Use this only
install.sh: for a mirror you trust that publishes no SHA256SUMS.

EOF
}

# ------------------------------------------------------------ installing files

# Copy a program next to its place, run it, then rename it over: a running
# warden keeps its old file, nobody sees a half-written binary, and a binary
# that cannot run here never replaces one that can.
install_program() { # source name
    stage="$dir/.$2.new.$$"
    if ! cp "$1" "$stage" 2>/dev/null; then
        fail "cannot write to $dir: run with sudo for a system-wide install, or choose a folder you own with --dir DIR"
    fi
    chmod 0755 "$stage"
    if ! "$stage" --version >/dev/null 2>&1 </dev/null; then
        fail "the $2 binary does not run on this system (it needs glibc 2.28+ on Linux, macOS 11+; or $dir is mounted noexec). Nothing was replaced"
    fi
    mv -f "$stage" "$dir/$2"
    stage=""
}

# A text file, replaced the same way.
install_data() { # source dest mode
    stage2="$(dirname "$2")/.$(basename "$2").new.$$"
    cp "$1" "$stage2" || fail "cannot write $2"
    chmod "$3" "$stage2"
    mv -f "$stage2" "$2"
    stage2=""
}

# An Exec= value that a desktop entry can hold.
desktop_exec() { # path
    case $1 in
        *[!A-Za-z0-9_./+@:=,-]*)
            case $1 in
                *[\"\`\$\\%]*) fail "the GUI's menu entry cannot name $1 (it has one of \" \` \$ \\ %): choose another --dir" ;;
            esac
            printf '"%s"\n' "$1"
            ;;
        *) printf '%s\n' "$1" ;;
    esac
}

install_gui_linux() {
    install_program "$gui_src/warden-gui" warden-gui
    say "installed $("$dir/warden-gui" --version 2>/dev/null </dev/null || echo warden-gui) to $dir/warden-gui"
    if [ ! -f "$gui_src/warden-gui.desktop" ] || [ ! -f "$gui_src/warden.png" ]; then
        warn "this release has no menu entry or icon for the GUI: start it with $dir/warden-gui"
        return 0
    fi
    apps_dir=$data_dir/applications
    icon_dir=$data_dir/icons/hicolor/256x256/apps
    mkdir -p "$apps_dir" "$icon_dir" || fail "cannot create $apps_dir and $icon_dir"
    # The menu starts programs with its own PATH, which may not have our folder:
    # name the binary in full.
    execline="Exec=$(desktop_exec "$dir/warden-gui")"
    EXECLINE=$execline awk '/^Exec=/ { print ENVIRON["EXECLINE"]; next } { print }' "$gui_src/warden-gui.desktop" >"$tmp/warden-gui.desktop"
    install_data "$tmp/warden-gui.desktop" "$apps_dir/warden-gui.desktop" 0644
    install_data "$gui_src/warden.png" "$icon_dir/warden.png" 0644
    refresh_desktop_caches
    say "added the menu entry $apps_dir/warden-gui.desktop and its icon"
}

# Menus find a new entry by themselves; these only make it quicker.
refresh_desktop_caches() {
    if command -v update-desktop-database >/dev/null 2>&1; then
        update-desktop-database "$data_dir/applications" >/dev/null 2>&1 || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -f "$data_dir/icons/hicolor/index.theme" ]; then
        gtk-update-icon-cache -q -t "$data_dir/icons/hicolor" >/dev/null 2>&1 || true
    fi
}

app_is_ours() { # path to Warden.app
    [ -f "$1/Contents/Info.plist" ] && grep -q "$APP_ID" "$1/Contents/Info.plist"
}

copy_app() { # source destination
    if command -v ditto >/dev/null 2>&1; then
        ditto "$1" "$2"
    else
        cp -R "$1" "$2"
    fi
}

# Unpack Warden.app from the downloaded zip or dmg into $tmp/app.
unpack_app() { # file
    mkdir -p "$tmp/app"
    case $1 in
        *.zip)
            if command -v ditto >/dev/null 2>&1; then
                ditto -x -k "$tmp/$1" "$tmp/app" || fail "cannot unpack $1"
            elif command -v unzip >/dev/null 2>&1; then
                unzip -q "$tmp/$1" -d "$tmp/app" || fail "cannot unpack $1"
            else
                fail "needs ditto or unzip to unpack $1"
            fi
            ;;
        *.dmg)
            command -v hdiutil >/dev/null 2>&1 || fail "needs hdiutil to open $1"
            mnt=$tmp/mnt
            mkdir -p "$mnt"
            hdiutil attach -nobrowse -readonly -noautoopen -quiet -mountpoint "$mnt" "$tmp/$1" >/dev/null </dev/null ||
                fail "cannot open $1 (hdiutil attach failed)"
            mounted=1
            if [ ! -d "$mnt/Warden.app" ]; then fail "$1 has no Warden.app"; fi
            copy_app "$mnt/Warden.app" "$tmp/app/Warden.app" || fail "cannot copy Warden.app out of $1"
            hdiutil detach "$mnt" -quiet >/dev/null 2>&1 || hdiutil detach "$mnt" -force -quiet >/dev/null 2>&1 || true
            mounted=0
            ;;
    esac
    gui_src=$tmp/app/Warden.app
    [ -d "$gui_src/Contents/MacOS" ] || fail "$1 has no Warden.app"
    # The ad-hoc signature must survive the unpacking: Apple silicon runs only signed code.
    if command -v codesign >/dev/null 2>&1 && ! codesign --verify --deep --strict "$gui_src" >/dev/null 2>&1; then
        fail "the Warden.app in $1 fails codesign --verify, so it would not start. Nothing was installed; try again, and report it if it keeps failing"
    fi
}

# What would make the GUI step fail, found before anything is downloaded or
# installed, so that a failure leaves the machine as it was.
preflight_gui() {
    if [ "$gui" != 1 ]; then return 0; fi
    if [ "$os" = linux ]; then
        execline=$(desktop_exec "$dir/warden-gui") || exit 1
    elif [ -e "$app_dir/Warden.app" ] && ! app_is_ours "$app_dir/Warden.app"; then
        fail "$app_dir/Warden.app exists and is not Warden's (its bundle id is not $APP_ID): not replacing it. Move it away, or choose another folder with --app-dir"
    fi
}

install_gui_macos() {
    dest=$app_dir/Warden.app
    mkdir -p "$app_dir" || fail "cannot create $app_dir: choose a folder you own with --app-dir DIR"
    stage="$app_dir/.Warden.app.new.$$"
    stage2="$app_dir/.Warden.app.old.$$"
    rm -rf "$stage" "$stage2"
    copy_app "$gui_src" "$stage" 2>/dev/null || fail "cannot write to $app_dir: run with sudo, or choose a folder you own with --app-dir DIR (~/Applications works)"
    if [ -e "$dest" ]; then mv "$dest" "$stage2"; fi
    if ! mv "$stage" "$dest"; then
        if [ -e "$stage2" ]; then mv "$stage2" "$dest"; fi
        fail "cannot put Warden.app in $app_dir"
    fi
    rm -rf "$stage2"
    stage=""
    stage2=""
    say "installed Warden.app to $dest (start it from Launchpad or: open -a Warden)"
}

# ------------------------------------------------------------ running Warden

# wardend (always on once an app starts) and every app's supervisor keep the
# code they started with: a new file on disk does not reach them.
note_running() {
    if ! "$dir/warden" daemon status >/dev/null 2>&1 </dev/null; then return 0; fi
    if "$dir/warden" --help 2>&1 </dev/null | grep -q '^ *update '; then
        restart="$cmd update"
    else
        # A release from before `warden update` (it does exactly this).
        restart="$cmd save && $cmd kill --yes && $cmd resurrect"
    fi
    cat <<EOF

wardend is running, and so are your apps' supervisors. Each keeps running the code it
started with, so they are still on the old version. To restart wardend and every
supervisor on the new one (the apps stop for a few seconds):

    $restart
EOF
}

# ------------------------------------------------------------ install

do_install() {
    check_libc
    find_tools
    if [ "$verify" = 0 ]; then no_verify_banner; fi

    if [ -n "${WARDEN_DOWNLOAD_URL:-}" ]; then
        base=${WARDEN_DOWNLOAD_URL%/}
    elif [ -n "$version" ]; then
        base="https://github.com/$REPO/releases/download/v$version"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi
    case $base in
        file://*) ;;
        *)
            if [ "$dl" = none ]; then
                fail "needs curl or wget to download the release. Install one (apt install curl, brew install curl), or download the files by hand: docs/install.md"
            fi
            ;;
    esac

    # SHA256SUMS is the one file used before it is checked, and only for the
    # names of the files and their hashes: nothing in it is ever run.
    have_sums=1
    if ! fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"; then
        if [ "$verify" = 0 ] && [ -n "$version" ]; then
            have_sums=0
            warn "cannot download $base/SHA256SUMS; carrying on without it (--no-verify)"
        else
            fail "cannot download $base/SHA256SUMS${version:+ (is v$version a Warden release?)}.
    No release published yet? Build from source: git clone https://github.com/$REPO && cd warden && cargo build --release (see docs/development.md).
    Behind a proxy or offline? Check that curl $base/SHA256SUMS works here."
        fi
    fi

    if [ -z "$version" ]; then
        discover_version
        # From here on that very release: a newer one published meanwhile must not
        # make the archive named in this SHA256SUMS vanish from "latest".
        if [ -z "${WARDEN_DOWNLOAD_URL:-}" ]; then base="https://github.com/$REPO/releases/download/v$version"; fi
    fi
    cli_archive="warden-$version-$os-$arch.tar.gz"
    cli_name="warden-$version-$os-$arch"
    if ! sums_find "$cli_archive"; then
        if [ "$verify" = 1 ]; then
            fail "$base/SHA256SUMS has no $cli_archive. It lists:
$(sums_listing)"
        fi
        warn "SHA256SUMS does not list $cli_archive; trying it anyway (--no-verify)"
    fi
    cli_hash=$want_hash

    gui_archive=""
    gui_hash=""
    if [ "$gui" = 1 ]; then
        if [ "$os" = linux ]; then
            gui_archive="warden-gui-$version-linux-$arch.tar.gz"
            if ! sums_find "$gui_archive"; then
                fail "release $version has no GUI for linux-$arch ($gui_archive is not in SHA256SUMS). It lists:
$(sums_listing)
    Leave out --gui for the CLI alone."
            fi
        else
            gui_archive="warden-gui-$version-macos-$arch.zip"
            if ! sums_find "$gui_archive"; then
                gui_archive="Warden-$version-macos-$arch.dmg"
                if ! sums_find "$gui_archive"; then
                    fail "release $version has no macOS GUI for $arch (neither warden-gui-$version-macos-$arch.zip nor $gui_archive is in SHA256SUMS). It lists:
$(sums_listing)
    Leave out --gui for the CLI alone."
                fi
            fi
        fi
        gui_hash=$want_hash
    fi

    preflight_gui

    old=""
    if [ -x "$dir/warden" ]; then old=$("$dir/warden" --version 2>/dev/null </dev/null | head -n 1) || old=""; fi

    if [ "$dry" = 1 ]; then
        say "dry run: nothing is downloaded or installed (only SHA256SUMS was fetched)"
        cat <<EOF
    platform   $os $arch${glibc:+ (glibc $glibc)}
    release    $version, from $base
    download   $cli_archive${cli_hash:+  sha256 $cli_hash}
    install    $dir/warden${old:+  (replaces: $old)}
EOF
        if [ -n "$gui_archive" ]; then
            printf '    download   %s%s\n' "$gui_archive" "${gui_hash:+  sha256 $gui_hash}"
            if [ "$os" = linux ]; then
                printf '    install    %s, %s, %s\n' "$dir/warden-gui" "$data_dir/applications/warden-gui.desktop" "$data_dir/icons/hicolor/256x256/apps/warden.png"
            else
                printf '    install    %s\n' "$app_dir/Warden.app"
            fi
        fi
        if ! on_path "$dir"; then
            if [ "$modify_path" = 1 ]; then
                printf '    PATH       would add %s to %s\n' "$dir" "$(tilde "$(profile_file)")"
            else
                printf '    PATH       %s is not on PATH (the line to add would be printed)\n' "$dir"
            fi
        fi
        return 0
    fi

    # Everything is downloaded and checked before anything is installed.
    sums_find "$cli_archive" || true
    get_file "$cli_archive"
    tar -xzf "$tmp/$cli_archive" -C "$tmp" || fail "cannot unpack $cli_archive"
    [ -f "$tmp/$cli_name/warden" ] || fail "$cli_archive has no $cli_name/warden"

    if [ -n "$gui_archive" ]; then
        sums_find "$gui_archive" || true
        get_file "$gui_archive"
        case $gui_archive in
            *.tar.gz)
                gui_name=${gui_archive%.tar.gz}
                tar -xzf "$tmp/$gui_archive" -C "$tmp" || fail "cannot unpack $gui_archive"
                gui_src=$tmp/$gui_name
                [ -f "$gui_src/warden-gui" ] || fail "$gui_archive has no $gui_name/warden-gui"
                ;;
            *) unpack_app "$gui_archive" ;;
        esac
    fi

    mkdir -p "$dir" || fail "cannot create $dir: run with sudo for /usr/local/bin, or choose a folder you own with --dir DIR"
    install_program "$tmp/$cli_name/warden" warden
    installed=$("$dir/warden" --version 2>/dev/null </dev/null | head -n 1) || installed="warden $version"
    if [ "$old" = "$installed" ]; then
        say "installed $installed to $dir/warden (it was this version already)"
    else
        say "installed $installed to $dir/warden${old:+ (was: $old)}"
    fi

    if [ -n "$gui_archive" ]; then
        if [ "$os" = linux ]; then install_gui_linux; else install_gui_macos; fi
    fi

    if on_path "$dir"; then
        cmd=warden
    else
        cmd="$dir/warden"
    fi
    # Another warden earlier on PATH would still be the one that runs.
    other=$(command -v warden 2>/dev/null | sed 's|//*|/|g' || true)
    if [ -n "$other" ] && [ "$other" != "$dir/warden" ] && on_path "$dir"; then
        warn "$other comes before $dir on your PATH, so a plain \`warden\` still runs that one ($("$other" --version 2>/dev/null </dev/null | head -n 1 || true)). Remove it, or put $dir first"
        cmd="$dir/warden"
    fi
    path_hint

    cat <<EOF

Next steps:
  $cmd doctor     check this machine (kernel, runtimes, limits)
  $cmd startup    start your saved apps at boot
  $cmd --help     everything else

The archive also has the systemd units, the sysctl file and an nginx example (contrib/):
  $base/$cli_archive
EOF
    note_running
}

# ------------------------------------------------------------ uninstall

# Remove a program only if it says it is Warden's.
remove_program() { # path expected-prefix-of---version
    p=$1
    if [ ! -e "$p" ] && [ ! -L "$p" ]; then return 1; fi
    v=$("$p" --version 2>/dev/null </dev/null | head -n 1) || v=""
    case $v in
        "$2 "*) ;;
        *)
            warn "not removing $p: it does not say it is $2 (remove it by hand if it is yours)"
            return 1
            ;;
    esac
    if [ "$dry" = 1 ]; then
        say "would remove $p ($v)"
    else
        rm -f "$p" || fail "cannot remove $p (permissions? try sudo)"
        say "removed $p ($v)"
    fi
    return 0
}

remove_file() { # path what
    if [ ! -e "$1" ]; then return 1; fi
    if [ "$dry" = 1 ]; then
        say "would remove $1"
    else
        rm -f "$1" || fail "cannot remove $1 (permissions? try sudo)"
        say "removed $1 ($2)"
    fi
    return 0
}

uninstall_gui() { # remove=1 removes it; 0 only looks (prints a hint)
    found=0
    if [ "$os" = linux ]; then
        entry=$data_dir/applications/warden-gui.desktop
        icon=$data_dir/icons/hicolor/256x256/apps/warden.png
        if [ "$1" = 1 ]; then
            if remove_program "$dir/warden-gui" warden-gui; then found=1; fi
            if [ -f "$entry" ] && grep -q '^Exec=.*warden-gui' "$entry"; then
                remove_file "$entry" "menu entry" || true
                remove_file "$icon" "icon" || true
                found=1
                if [ "$dry" = 0 ]; then refresh_desktop_caches; fi
            fi
        elif [ -x "$dir/warden-gui" ] || [ -f "$entry" ]; then
            found=1
        fi
    else
        for d in "$app_dir" "$app_dir2"; do
            if [ -z "$d" ]; then continue; fi
            if [ -d "$d/Warden.app" ] && app_is_ours "$d/Warden.app"; then
                found=1
                if [ "$1" = 1 ]; then
                    if [ "$dry" = 1 ]; then
                        say "would remove $d/Warden.app"
                    else
                        rm -rf "$d/Warden.app" || fail "cannot remove $d/Warden.app (permissions? try sudo)"
                        say "removed $d/Warden.app"
                    fi
                fi
            elif [ -e "$d/Warden.app" ] && [ "$1" = 1 ]; then
                warn "not removing $d/Warden.app: its bundle id is not $APP_ID"
            fi
        done
    fi
    if [ "$found" = 1 ] && [ "$1" = 0 ]; then
        say "the GUI is still installed: sh install.sh --uninstall --gui removes it too"
    fi
    return 0
}

do_uninstall() {
    removed=0
    # Said first: removing the program does not stop what runs.
    if [ -x "$dir/warden" ] && "$dir/warden" daemon status >/dev/null 2>&1 </dev/null; then
        warn "wardend and your apps' supervisors are running. This removes files only; they keep running (from files that are gone). Stop them first with: $dir/warden kill --yes"
    fi
    if remove_program "$dir/warden" warden; then removed=1; fi
    if [ "$gui" = 1 ]; then
        uninstall_gui 1
    else
        uninstall_gui 0
    fi

    # PATH lines this installer added (--modify-path), only ours.
    if [ -n "${HOME:-}" ]; then
        for f in "${ZDOTDIR:-$HOME}/.zshrc" "$HOME/.bashrc" "$HOME/.bash_profile" "$HOME/.profile" "${XDG_CONFIG_HOME:-$HOME/.config}/fish/conf.d/warden.fish"; do
            unmodify_path "$f"
        done
    fi

    if [ "$removed" = 0 ]; then
        say "no warden in $dir: nothing to remove. If you installed it elsewhere: sh install.sh --uninstall --dir DIR${other:+ (a warden is at $other)}"
    fi
    cat <<'EOF'

Not touched: your apps' configs and saved state (~/.config/warden, ~/.local/state/warden,
or /etc/warden and /var/lib/warden as root), systemd units or launchd jobs made by
`warden startup` (`warden unstartup` removes them), and the logs. Delete those by hand
if you want them gone.
EOF
}

# ------------------------------------------------------------ main

# The last line passes an end marker as the last argument. A script cut short
# anywhere, even right after `main`, lacks it, and nothing runs.
main() {
    last=""
    for a in "$@"; do last=$a; done
    if [ "$last" != --end-of-install-script ]; then
        printf 'install.sh: this script is incomplete (was the download cut short?): nothing was done. Download it again.\n' >&2
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
    APP_ID="io.github.oceanwap.warden"
    MARK="# added by the Warden installer (install.sh --modify-path)"

    version=${WARDEN_VERSION:-}
    dir_opt=""
    app_dir_opt=${WARDEN_APP_DIR:-}
    gui=0
    uninstall=0
    dry=0
    verify=1
    modify_path=0
    tmp=""
    stage=""
    stage2=""
    mnt=""
    mounted=0
    other=""

    parse_args "$@"
    detect_platform
    resolve_dirs

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t warden) || fail "cannot create a temporary folder (is TMPDIR writable?)"
    trap cleanup EXIT
    trap 'exit 1' INT TERM HUP

    if [ "$uninstall" = 1 ]; then
        other=$(command -v warden 2>/dev/null || true)
        do_uninstall
    else
        do_install
    fi
}

main "$@" --end-of-install-script

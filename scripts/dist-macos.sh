#!/usr/bin/env bash
# The macOS release archives, built on this Mac instead of on GitHub's macOS
# runners (a macOS minute costs ten Linux ones on a private repository).
#
#   scripts/dist-macos.sh [OPTIONS]     (also: cargo xtask dist-macos [OPTIONS])
#
# Builds, for arm64 and x86_64 (either Mac can build both):
#   warden-<version>-macos-<arch>.tar.gz       the CLI
#   warden-gui-<version>-macos-<arch>.zip      Warden.app (GUI + CLI), ad-hoc signed
#   Warden-<version>-macos-<arch>.dmg          a disk image (volume "Warden", UDZO) with
#                                              that Warden.app and a link to /Applications
# the archives with the same names and contents the Release workflow made on
# macOS runners before. It mounts each disk image to check what is inside, then
# checks the host's files with install.sh (checksum, tampered archive refused,
# Warden.app from the zip and from the image) and uploads everything to a DRAFT
# GitHub Release for the tag v<version> (created if needed; never published
# from here). The Release workflow (.github/workflows/release.yml) waits for
# these files, adds the Linux archives, SHA256SUMS and install.sh, and
# publishes. `macos-build-info.txt` goes up last: it names the commit these
# files were built from, and the workflow refuses any other commit.
#
# Options:
#   --no-upload        Build and check only: files stay in the output directory
#   --no-gui           CLI archives only (no zip, no disk image)
#   --no-dmg           No disk images
#   --arch LIST        arm64, x86_64 or arm64,x86_64 (default: both)
#   --branch NAME      The branch a new draft release points at (default: main)
#   --out DIR          Output directory, inside target/ (default: target/dist-macos;
#                      it is emptied first)
#   --allow-dirty      Build with uncommitted changes (the files are for testing,
#                      and the workflow will not accept them: the info file says dirty)
#   --check            Only check what a release needs (a Mac, the tools, gh logged
#                      in): builds and uploads nothing
#   -h, --help         This help
#
# --arch, --no-gui and --no-dmg build part of a release: they need --no-upload,
# since a release holds every file, built together from one commit.
#
# Needs: a Mac with Xcode's command line tools, rustup (or the right Rust),
# `cargo install cargo-about --locked --features cli` (third-party notices), and
# for the upload the GitHub CLI: `gh auth login` with a right to write releases.
# hdiutil (the disk images) comes with macOS.
# (Run by hand or by `cargo release` on a Mac; not meant to be portable to
# Linux: scripts/test-dist-macos.sh runs it there, against stubs.)
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
say() { printf '   %s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
die() { printf '\ndist-macos: %s\n' "$*" >&2; exit 1; }

upload=1
gui=1
dmg=1
archs="arm64,x86_64"
branch=main
out=""
allow_dirty=0
check=0
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --no-upload) upload=0 ;;
        --no-gui) gui=0 ;;
        --no-dmg) dmg=0 ;;
        --allow-dirty) allow_dirty=1 ;;
        --check) check=1 ;;
        --arch) [ $# -ge 2 ] || die "--arch needs arm64, x86_64 or arm64,x86_64"; archs=$2; shift ;;
        --branch) [ $# -ge 2 ] || die "--branch needs a branch name"; branch=$2; shift ;;
        --out) [ $# -ge 2 ] || die "--out needs a directory"; out=$2; shift ;;
        *) die "unknown option $1 (--help lists them)" ;;
    esac
    shift
done
case ",$archs," in
    ,arm64,|,x86_64,|,arm64,x86_64,|,x86_64,arm64,) ;;
    *) die "--arch $archs: use arm64, x86_64 or arm64,x86_64" ;;
esac
# A disk image holds Warden.app, so no GUI means no image.
[ "$gui" = 1 ] || dmg=0
full=1
[ "$gui" = 1 ] || full=0
[ "$dmg" = 1 ] || full=0
case ",$archs," in ,arm64,x86_64,|,x86_64,arm64,) ;; *) full=0 ;; esac
if [ "$full" = 0 ] && [ "$upload" = 1 ] && [ "$check" = 0 ]; then
    die "--arch, --no-gui and --no-dmg build part of a release, and a release needs every file (the CLI archives, the app zips and the disk images) from one build: add --no-upload"
fi

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
[ -n "$out" ] || out="$root/target/dist-macos"
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac
# The directory is emptied: only ever one inside target/.
case "$out" in
    "$root"/target/?*) ;;
    *) die "--out $out: it is emptied first, so it must be a directory inside $root/target" ;;
esac
case "$out" in *'#'*|*'?'*) die "--out $out: no # or ? in the path (they break asset uploads and URLs)" ;; esac

triple() { case "$1" in arm64) echo aarch64-apple-darwin ;; x86_64) echo x86_64-apple-darwin ;; esac; }

# ---------------------------------------------------------------- 1
step "1. Preconditions"
[ "$(uname -s)" = Darwin ] || die "this builds macOS binaries, so it needs a Mac (this is $(uname -s)). On another machine the Release workflow waits for the files: run this on a Mac."
command -v git >/dev/null || die "git is not installed"
if [ "$dmg" = 1 ]; then
    command -v hdiutil >/dev/null || die "hdiutil is missing (it makes the disk images; it is part of macOS: is this a Mac with a normal PATH?). --no-dmg builds without them"
fi
sha=$(git rev-parse HEAD)
version=""
tag=""
if [ "$check" = 0 ]; then
    if [ "$allow_dirty" = 0 ] && [ -n "$(git status --porcelain)" ]; then
        git status --short | head -10 >&2
        die "the working tree has uncommitted changes: the archives must be built from the commit they are released as. Commit or stash them (or --allow-dirty for a test build that can't be released)."
    fi
    say "commit $(git rev-parse --short HEAD)$([ "$allow_dirty" = 1 ] && echo ' (dirty tree allowed)')"

    manifest_version() {
        sed -n '/^\[package\]/,/^\[/{s/^version *= *"\(.*\)".*/\1/p;}' "$1" | head -1
    }
    version=$(manifest_version Cargo.toml)
    [ -n "$version" ] || die "no [package] version in Cargo.toml"
    for m in protocol/Cargo.toml gui/Cargo.toml; do
        [ "$(manifest_version $m)" = "$version" ] || die "$m is version $(manifest_version $m), Cargo.toml says $version: they are released together"
    done
    tag="v$version"
    say "version $version (tag $tag)"

    # The tag, here or on origin, must be on this very commit: the archives
    # are released as the commit the tag names.
    if tagged=$(git rev-parse -q --verify "refs/tags/$tag^{commit}" 2>/dev/null) && [ "$tagged" != "$sha" ]; then
        die "the tag $tag here points at $(echo "$tagged" | cut -c1-7), but HEAD is $(echo "$sha" | cut -c1-7): check out the commit the tag names (git checkout $tag), or the release will refuse these files"
    fi
    remote_tagged=$(git ls-remote --tags origin "refs/tags/$tag^{}" "refs/tags/$tag" 2>/dev/null | awk 'END { print $1 }') || remote_tagged=""
    if [ -n "$remote_tagged" ] && [ "$remote_tagged" != "$sha" ]; then
        die "the tag $tag on origin points at $(echo "$remote_tagged" | cut -c1-7), but HEAD is $(echo "$sha" | cut -c1-7): check out the commit the tag names (git fetch --tags && git checkout $tag)"
    fi
fi

# The toolchain and tool versions CI uses (release.yml), so the archives are
# built the way the Linux ones are.
workflow=.github/workflows/release.yml
toolchain=$(sed -n 's/^  RUST_TOOLCHAIN: "\(.*\)".*/\1/p' $workflow | head -1)
about_version=$(sed -n 's/^  CARGO_ABOUT_VERSION: "\(.*\)".*/\1/p' $workflow | head -1)
[ -n "$toolchain" ] || die "can't read RUST_TOOLCHAIN from $workflow"
command -v cargo >/dev/null || die "cargo is not installed (https://rustup.rs)"
if command -v rustup >/dev/null; then
    cargo_cmd() { cargo "+$toolchain" "$@"; }
    rustc_version() { rustc "+$toolchain" --version; }
    if [ "$check" = 1 ]; then
        say "Rust $toolchain (rustup installs it if it is missing)"
    else
        rustup toolchain install "$toolchain" --profile minimal >/dev/null 2>&1 || die "rustup can't install Rust $toolchain"
        say "Rust $toolchain (rustup)"
    fi
else
    cargo_cmd() { cargo "$@"; }
    rustc_version() { rustc --version; }
    say "no rustup: using $(rustc --version 2>/dev/null || echo 'no rustc!') (CI builds with $toolchain)"
fi
cargo about --version >/dev/null 2>&1 ||
    die "cargo-about is missing (it writes the third-party notices every archive carries). Install it once:
      cargo install cargo-about --locked --features cli --version ${about_version:-0.9.2}"
have_about=$(cargo about --version 2>/dev/null | awk '{ print $NF }')
if [ -n "$about_version" ] && [ "$have_about" != "$about_version" ]; then
    say "note: cargo-about $have_about (the Linux archives' notices use $about_version)"
fi

draft_state=none
if [ "$upload" = 1 ]; then
    command -v gh >/dev/null || die "the GitHub CLI (gh) is missing, and the files go to a draft release with it: brew install gh && gh auth login (or --no-upload to only build)"
    gh api user --silent >/dev/null 2>&1 || die "gh is not logged in (or can't reach GitHub): gh auth login (or --no-upload)"
    say "gh is logged in"
    if [ "$check" = 0 ]; then
        seen=$(gh release view "$tag" --json isDraft --jq .isDraft 2>&1) || true
        case "$seen" in
            true) draft_state=draft; say "the draft release $tag exists: its macOS files are replaced" ;;
            false) die "$tag is already published: a published release's files are never replaced. Release a new version instead." ;;
            *"not found"*) say "no release $tag yet: a draft is created" ;;
            *) die "can't look for the release $tag: $seen" ;;
        esac
    fi
fi
if [ "$check" = 1 ]; then
    echo
    echo "dist-macos: ready: everything a release needs here is in place."
    exit 0
fi

# ---------------------------------------------------------------- 2
step "2. Third-party notices"
rm -rf "$out"
mkdir -p "$out/stage" "$out/notices"
cargo_cmd about generate --locked --fail -m Cargo.toml about.hbs -o "$out/notices/THIRD-PARTY-LICENSES.txt"
if [ "$gui" = 1 ]; then
    cargo_cmd about generate --locked --fail -m gui/Cargo.toml about.hbs -o "$out/notices/THIRD-PARTY-LICENSES-GUI.txt"
fi
say "$(wc -l <"$out/notices/THIRD-PARTY-LICENSES.txt" | tr -d ' ') lines of notices"

# What the binary says when run: skipped (return 99) where this Mac can't run it.
host_arch=$(uname -m)
# A shell under Rosetta says x86_64 on an Apple silicon Mac (install.sh does this too).
if [ "$host_arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
    host_arch=arm64
fi
run_version() { # target binary
    case "$1" in
        aarch64-apple-darwin) if [ "$host_arch" = arm64 ]; then "$2" --version; else return 99; fi ;;
        x86_64-apple-darwin)
            if [ "$host_arch" = x86_64 ]; then "$2" --version
            elif arch -x86_64 /usr/bin/true 2>/dev/null; then arch -x86_64 "$2" --version
            else return 99; fi ;;
    esac
}
check_version() { # target binary expected
    local got rc=0
    got=$(run_version "$1" "$2") || rc=$?
    if [ "$rc" = 99 ]; then
        say "built, not run (this Mac can't run $1 binaries)"
    elif [ "$rc" != 0 ]; then
        die "$2 --version failed"
    elif [ "$got" != "$3" ]; then
        die "$2 --version says '$got', expected '$3'"
    else
        say "$got"
    fi
}

# The disk image: Warden.app next to a link to /Applications, to drag one onto
# the other. UDZO is zlib-compressed and read-only; HFS+ mounts on every macOS
# the app runs on. hdiutil is sometimes "resource busy" right after something
# else touched the folder (Spotlight): try again a few times.
make_dmg() { # folder dmg
    local try
    rm -f "$2"
    for try in 1 2 3; do
        if hdiutil create -volname Warden -srcfolder "$1" -fs HFS+ -format UDZO -ov "$2" >/dev/null; then return 0; fi
        say "hdiutil create failed (attempt $try of 3)"
        sleep "${DMG_RETRY_SLEEP:-3}"
    done
    die "hdiutil could not make $(basename "$2")"
}

# Where a disk image is mounted right now. Always detached, also when the
# script fails or is interrupted.
mounted=""
detach_dmg() {
    [ -n "$mounted" ] || return 0
    local m=$mounted
    mounted=""
    hdiutil detach "$m" >/dev/null 2>&1 || { sleep 2; hdiutil detach "$m" -force >/dev/null 2>&1; } || true
}
trap detach_dmg EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Mount a finished image the way a user's Mac does and look inside: the app,
# its two programs (they must run), its signature, the link to /Applications.
check_dmg() { # target dmg
    local t=$1 dmg=$2 f vol
    mounted="$out/stage/mnt-$t"
    mkdir -p "$mounted"
    hdiutil attach -nobrowse -readonly -mountpoint "$mounted" "$dmg" >/dev/null || die "cannot mount $(basename "$dmg")"
    for f in warden-gui warden; do
        [ -x "$mounted/Warden.app/Contents/MacOS/$f" ] || die "$(basename "$dmg"): Warden.app/Contents/MacOS/$f is missing or not executable"
    done
    grep -q "<string>$version</string>" "$mounted/Warden.app/Contents/Info.plist" || die "$(basename "$dmg"): Info.plist does not say version $version"
    check_version "$t" "$mounted/Warden.app/Contents/MacOS/warden" "warden $version"
    check_version "$t" "$mounted/Warden.app/Contents/MacOS/warden-gui" "warden-gui $version"
    codesign --verify --deep --strict "$mounted/Warden.app" || die "$(basename "$dmg"): the Warden.app inside fails codesign --verify"
    [ -L "$mounted/Applications" ] && [ "$(readlink "$mounted/Applications")" = /Applications ] ||
        die "$(basename "$dmg"): there is no Applications link to /Applications"
    # The volume name (best effort: diskutil may not say).
    vol=$(diskutil info "$mounted" 2>/dev/null | sed -n 's/^ *Volume Name: *//p' | head -1)
    if [ -n "$vol" ] && [ "$vol" != Warden ]; then die "$(basename "$dmg"): the volume is called '$vol', not Warden"; fi
    detach_dmg
}

# ---------------------------------------------------------------- 3
n=2
for arch in $(echo "$archs" | tr ',' ' '); do
    t=$(triple "$arch")
    n=$((n + 1))
    step "$n. macOS $arch ($t)"
    if command -v rustup >/dev/null; then
        rustup target add --toolchain "$toolchain" "$t" >/dev/null 2>&1 || die "rustup can't add the target $t"
    fi
    # CI's environment: no inherited flags, no deployment target override.
    export CARGO_TARGET_DIR="$root/target"
    # An empty RUSTFLAGS beats every `rustflags` in ~/.cargo/config.toml (a
    # local target-cpu=native must never reach a release), as CI's does.
    unset CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET MACOSX_DEPLOYMENT_TARGET
    export RUSTFLAGS=""
    say "building warden (release)"
    cargo_cmd build --release --locked --bin warden --target "$t"
    cli_bin="$root/target/$t/release/warden"
    check_version "$t" "$cli_bin" "warden $version"

    name="warden-$version-macos-$arch"
    stage="$out/stage/$name"
    mkdir -p "$stage/contrib"
    cp "$cli_bin" README.md LICENSE-MIT LICENSE-APACHE "$out/notices/THIRD-PARTY-LICENSES.txt" "$stage/"
    cp contrib/warden@.service contrib/wardend.service contrib/99-warden.conf contrib/nginx.conf "$stage/contrib/"
    # No AppleDouble (._*) files from macOS tar.
    COPYFILE_DISABLE=1 tar -C "$out/stage" -czf "$out/$name.tar.gz" "$name"
    say "$name.tar.gz"

    [ "$gui" = 1 ] || continue
    say "building warden-gui (release)"
    cargo_cmd build --release --locked -p warden-gui --bin warden-gui --target "$t"
    gui_bin="$root/target/$t/release/warden-gui"
    check_version "$t" "$gui_bin" "warden-gui $version"

    gui_name="warden-gui-$version-macos-$arch"
    app="$out/stage/$gui_name/Warden.app"
    mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
    cp "$gui_bin" "$app/Contents/MacOS/"
    cp "$cli_bin" "$app/Contents/MacOS/warden"
    mkdir -p "$app/Contents/Resources/contrib"
    cp README.md "$app/Contents/Resources/"
    cp contrib/warden@.service contrib/wardend.service contrib/99-warden.conf contrib/nginx.conf "$app/Contents/Resources/contrib/"
    cp gui/README.md "$app/Contents/Resources/README-GUI.md"
    cp gui/assets/fonts/LICENSES.txt "$app/Contents/Resources/FONT-LICENSES.txt"
    cp assets/icon/Warden.icns "$app/Contents/Resources/Warden.icns"
    cp LICENSE-MIT LICENSE-APACHE "$out/notices/THIRD-PARTY-LICENSES.txt" "$out/notices/THIRD-PARTY-LICENSES-GUI.txt" "$app/Contents/Resources/"
    cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Warden</string>
  <key>CFBundleDisplayName</key><string>Warden</string>
  <key>CFBundleIdentifier</key><string>io.github.oceanwap.warden</string>
  <key>CFBundleExecutable</key><string>warden-gui</string>
  <key>CFBundleIconFile</key><string>Warden</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
    plutil -lint "$app/Contents/Info.plist" >/dev/null
    # codesign refuses "detritus": Finder info and resource forks copied with the files.
    xattr -cr "$app"
    # Ad-hoc signature: a consistent bundle (Apple silicon runs only signed
    # code; the linker signs the binaries, not the bundle).
    codesign --force --deep --sign - "$app"
    codesign --verify --deep --strict "$app"
    # TODO(signing): not notarized yet, so Gatekeeper refuses the first open.
    # It needs the owner's Apple Developer ID, and building here makes that
    # simple (the certificate is already in this Mac's keychain, no CI secrets):
    #   codesign --force --deep --options runtime --timestamp --sign "Developer ID Application: <name> (<team>)" "$app"
    #   xcrun notarytool submit <zip> --keychain-profile <profile> --wait
    #   xcrun stapler staple "$app"   (and zip again after stapling)
    ditto -c -k --keepParent --norsrc "$app" "$out/$gui_name.zip"
    say "$gui_name.zip"

    [ "$dmg" = 1 ] || continue
    dmg_name="Warden-$version-macos-$arch"
    # Not named after $dmg_name: on a case-insensitive disk that is the CLI's staging folder.
    dmg_stage="$out/stage/dmg-$arch"
    mkdir -p "$dmg_stage"
    # ditto keeps the signature and the attributes the way codesign left them.
    ditto "$app" "$dmg_stage/Warden.app"
    ln -s /Applications "$dmg_stage/Applications"
    make_dmg "$dmg_stage" "$out/$dmg_name.dmg"
    # TODO(signing): the image has only an ad-hoc signature and is not notarized,
    # like the app (see above). With the owner's Developer ID certificate, sign the
    # app inside first (the line above), then the image, notarize and staple it:
    #   codesign --force --timestamp --sign "Developer ID Application: <name> (<team>)" "$out/$dmg_name.dmg"
    #   xcrun notarytool submit "$out/$dmg_name.dmg" --keychain-profile <profile> --wait
    #   xcrun stapler staple "$out/$dmg_name.dmg"
    codesign --force --sign - "$out/$dmg_name.dmg"
    codesign --verify "$out/$dmg_name.dmg"
    check_dmg "$t" "$out/$dmg_name.dmg"
    say "$dmg_name.dmg"
done

# ------------------------------------------------------------------ install.sh
n=$((n + 1))
step "$n. install.sh on this Mac (the archive for $host_arch)"
# file:// URLs: percent-encode what a path may hold.
url_out=$(printf '%s' "$out" | sed 's/%/%25/g; s/ /%20/g')
mkdir -p "$out/check/files" "$out/check/tampered"
cp "$out"/warden-"$version"-macos-*.tar.gz "$out/check/files/"
if [ "$gui" = 1 ]; then cp "$out"/warden-gui-"$version"-macos-*.zip "$out/check/files/"; fi
if [ "$dmg" = 1 ]; then cp "$out"/Warden-"$version"-macos-*.dmg "$out/check/files/"; fi
# SHA256SUMS lists every file in the folder, the way the release's does.
sums_of() { (cd "$1" && rm -f SHA256SUMS && shasum -a 256 -- * > SHA256SUMS); }
sums_of "$out/check/files"
cp "$out/check/files"/* "$out/check/tampered/"
for f in "$out"/check/tampered/*.tar.gz; do printf 'x' >> "$f"; done
if [ -f "$out/check/files/warden-$version-macos-$host_arch.tar.gz" ]; then
    if ! WARDEN_DOWNLOAD_URL="file://$url_out/check/files" WARDEN_VERSION="v$version" \
        WARDEN_INSTALL_DIR="$out/check/bin" sh install.sh >"$out/check/install.log" 2>&1; then
        cat "$out/check/install.log" >&2
        die "install.sh failed on the archives just built"
    fi
    got=$("$out/check/bin/warden" --version) || die "the installed warden doesn't run"
    [ "$got" = "warden $version" ] || die "install.sh installed '$got', expected 'warden $version'"
    say "installed: $got"
    if WARDEN_DOWNLOAD_URL="file://$url_out/check/tampered" WARDEN_VERSION="v$version" \
        WARDEN_INSTALL_DIR="$out/check/bin-tampered" sh install.sh >/dev/null 2>&1; then
        die "install.sh installed an archive whose checksum does not match"
    fi
    [ ! -e "$out/check/bin-tampered/warden" ] || die "install.sh left a binary behind after refusing a tampered archive"
    say "a tampered archive is refused"
    # Warden.app: from the zip, then (the zip taken out of the release) from the
    # disk image, then removed again.
    zip_file="$out/check/files/warden-gui-$version-macos-$host_arch.zip"
    if [ "$gui" = 1 ] && [ -f "$zip_file" ]; then
        gui_install() { # label: install --gui into check/Applications
            if ! WARDEN_DOWNLOAD_URL="file://$url_out/check/files" WARDEN_VERSION="v$version" \
                WARDEN_INSTALL_DIR="$out/check/bin" WARDEN_APP_DIR="$out/check/Applications" \
                sh install.sh --gui >"$out/check/install-gui.log" 2>&1; then
                cat "$out/check/install-gui.log" >&2
                die "install.sh --gui failed on the files just built ($1)"
            fi
            [ -x "$out/check/Applications/Warden.app/Contents/MacOS/warden-gui" ] || die "install.sh --gui ($1) left no Warden.app"
            codesign --verify --deep --strict "$out/check/Applications/Warden.app" || die "the Warden.app install.sh put there ($1) fails codesign --verify"
            say "installed: Warden.app ($1)"
        }
        gui_install "from the zip"
        if [ "$dmg" = 1 ] && [ -f "$out/check/files/Warden-$version-macos-$host_arch.dmg" ]; then
            rm -rf "$out/check/Applications" "$zip_file"
            sums_of "$out/check/files"
            gui_install "from the disk image"
        fi
        WARDEN_APP_DIR="$out/check/Applications" WARDEN_INSTALL_DIR="$out/check/bin" sh install.sh --uninstall --gui >/dev/null 2>&1 ||
            die "install.sh --uninstall --gui failed"
        [ ! -e "$out/check/Applications/Warden.app" ] || die "install.sh --uninstall --gui left Warden.app behind"
        [ ! -e "$out/check/bin/warden" ] || die "install.sh --uninstall left warden behind"
        say "uninstalled again"
    fi
else
    say "skipped: no $host_arch archive was built (--arch)"
fi
rm -rf "$out/check" "$out/stage"

# ------------------------------------------------------------------ files
n=$((n + 1))
step "$n. Build information and upload"
files=()
for f in "$out"/warden-"$version"-macos-*.tar.gz "$out"/warden-gui-"$version"-macos-*.zip "$out"/Warden-"$version"-macos-*.dmg; do
    if [ -f "$f" ]; then files+=("$f"); fi
done
[ "${#files[@]}" -gt 0 ] || die "no archive was built"

# The info file says which commit these files are, and their checksums: the
# workflow takes exactly these bytes, whatever else the draft may hold.
info="$out/macos-build-info.txt"
{
    echo "Warden $version, macOS archives"
    echo "commit: $sha"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host: macOS $(sw_vers -productVersion 2>/dev/null || echo '?') $host_arch"
    echo "rustc: $(rustc_version)"
    echo "dirty: $([ -n "$(git status --porcelain)" ] && echo yes || echo no)"
    for f in "${files[@]}"; do
        echo "sha256 $(shasum -a 256 "$f" | awk '{ print $1 }') $(basename "$f")"
    done
} > "$info"
sed 's/^/   | /' "$info"
say "${#files[@]} archive(s) in $out:"
for f in "${files[@]}"; do say "  $(basename "$f")  $(du -h "$f" | cut -f1)"; done

if [ "$upload" = 0 ]; then
    say "not uploaded (--no-upload)"
    exit 0
fi

# The build took a while: is it still what was checked, and still a draft?
[ "$(git rev-parse HEAD)" = "$sha" ] || die "HEAD moved during the build (it was $(echo "$sha" | cut -c1-7)): nothing was uploaded. Run this again."
seen=$(gh release view "$tag" --json isDraft --jq .isDraft 2>&1) || true
case "$seen" in
    true) draft_state=draft ;;
    false) die "$tag was published while this was building: nothing was uploaded (a published release's files are never replaced)" ;;
    *"not found"*) draft_state=none ;;
    *) die "can't look for the release $tag: $seen" ;;
esac

prerelease=()
case "$version" in *-*) prerelease=(--prerelease) ;; esac
if [ "$draft_state" = none ]; then
    say "creating the draft release $tag (target: $branch)"
    gh release create "$tag" --draft --target "$branch" --title "Warden $version" \
        --notes "Draft. The macOS archives were built on a Mac (macos-build-info.txt names the commit and the checksums). The Release workflow adds the Linux archives, SHA256SUMS and install.sh, then publishes." \
        ${prerelease[@]+"${prerelease[@]}"} "${files[@]}"
else
    # The old info file goes first: while the archives are replaced, no file
    # vouches for them, so a workflow waiting on the draft keeps waiting.
    say "replacing the macOS files of the draft $tag"
    gh release delete-asset "$tag" macos-build-info.txt --yes 2>/dev/null || true
    gh release upload "$tag" "${files[@]}" --clobber
fi
# Last: the workflow takes the files only when this names its commit and
# every archive matches its checksum.
gh release upload "$tag" "$info" --clobber
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || echo "")
say "uploaded${repo:+: https://github.com/$repo/releases}"
echo
echo "dist-macos: done. The Release workflow publishes $tag once its Linux builds pass."

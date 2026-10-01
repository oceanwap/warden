#!/usr/bin/env bash
# The macOS release archives, built on this Mac instead of on GitHub's macOS
# runners (a macOS minute costs ten Linux ones on a private repository).
#
#   scripts/dist-macos.sh [OPTIONS]     (also: cargo xtask dist-macos [OPTIONS])
#
# Builds, for arm64 and x86_64 (either Mac can build both):
#   warden-<version>-macos-<arch>.tar.gz       the CLI
#   warden-gui-<version>-macos-<arch>.zip      Warden.app (GUI + CLI), ad-hoc signed
# the same files, with the same names and contents, the Release workflow made
# on macOS runners before. Then it checks the host's archive with install.sh
# (checksum, tampered archive refused) and uploads everything to a DRAFT
# GitHub Release for the tag v<version> (created if needed; never published
# from here). The Release workflow (.github/workflows/release.yml) waits for
# these files, adds the Linux archives, SHA256SUMS and install.sh, and
# publishes. `macos-build-info.txt` goes up last: it names the commit these
# files were built from, and the workflow refuses any other commit.
#
# Options:
#   --no-upload        Build and check only: files stay in the output directory
#   --no-gui           CLI archives only
#   --arch LIST        arm64, x86_64 or arm64,x86_64 (default: both)
#   --branch NAME      The branch a new draft release points at (default: main)
#   --out DIR          Output directory (default: target/dist-macos)
#   --allow-dirty      Build with uncommitted changes (the files are for testing,
#                      and the workflow will not accept them: the commit differs)
#   -h, --help         This help
#
# Needs: a Mac with Xcode's command line tools, rustup (or the right Rust),
# `cargo install cargo-about --locked --features cli` (third-party notices), and
# for the upload the GitHub CLI: `gh auth login` with a right to write releases.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d; s/^# \{0,1\}//'; }
say() { printf '   %s\n' "$*"; }
step() { printf '\n== %s\n' "$*"; }
die() { printf '\ndist-macos: %s\n' "$*" >&2; exit 1; }

upload=1
gui=1
archs="arm64,x86_64"
branch=main
out=""
allow_dirty=0
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --no-upload) upload=0 ;;
        --no-gui) gui=0 ;;
        --allow-dirty) allow_dirty=1 ;;
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

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
[ -n "$out" ] || out="$root/target/dist-macos"
case "$out" in /*) ;; *) out="$PWD/$out" ;; esac

triple() { case "$1" in arm64) echo aarch64-apple-darwin ;; x86_64) echo x86_64-apple-darwin ;; esac; }

# ---------------------------------------------------------------- 1
step "1. Preconditions"
[ "$(uname -s)" = Darwin ] || die "this builds macOS binaries, so it needs a Mac (this is $(uname -s)). On another machine the Release workflow waits for the files: run this on a Mac."
command -v git >/dev/null || die "git is not installed"
sha=$(git rev-parse HEAD)
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
    rustup toolchain install "$toolchain" --profile minimal >/dev/null 2>&1 || die "rustup can't install Rust $toolchain"
    say "Rust $toolchain (rustup)"
else
    cargo_cmd() { cargo "$@"; }
    rustc_version() { rustc --version; }
    say "no rustup: using $(rustc --version 2>/dev/null || echo 'no rustc!') (CI builds with $toolchain)"
fi
cargo about --version >/dev/null 2>&1 ||
    die "cargo-about is missing (it writes the third-party notices every archive carries). Install it once:
      cargo install cargo-about --locked --features cli --version ${about_version:-0.9.2}"

draft_state=none
if [ "$upload" = 1 ]; then
    command -v gh >/dev/null || die "the GitHub CLI (gh) is missing, and the files go to a draft release with it: brew install gh && gh auth login (or --no-upload to only build)"
    gh auth status >/dev/null 2>&1 || die "gh is not logged in: gh auth login (or --no-upload)"
    seen=$(gh release view "$tag" --json isDraft --jq .isDraft 2>&1) || true
    case "$seen" in
        true) draft_state=draft; say "the draft release $tag exists: its macOS files are replaced" ;;
        false) die "$tag is already published: a published release's files are never replaced. Release a new version instead." ;;
        *"not found"*) say "no release $tag yet: a draft is created" ;;
        *) die "can't look for the release $tag: $seen" ;;
    esac
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
    unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CARGO_BUILD_TARGET MACOSX_DEPLOYMENT_TARGET
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
    ditto -c -k --keepParent "$app" "$out/$gui_name.zip"
    say "$gui_name.zip"
done

# ------------------------------------------------------------------ install.sh
n=$((n + 1))
step "$n. install.sh on this Mac (the archive for $host_arch)"
mkdir -p "$out/check/files" "$out/check/tampered"
cp "$out"/warden-"$version"-macos-*.tar.gz "$out/check/files/"
(cd "$out/check/files" && shasum -a 256 -- warden-*.tar.gz > SHA256SUMS)
cp "$out/check/files"/* "$out/check/tampered/"
for f in "$out"/check/tampered/*.tar.gz; do printf 'x' >> "$f"; done
if [ -f "$out/check/files/warden-$version-macos-$host_arch.tar.gz" ]; then
    if ! WARDEN_DOWNLOAD_URL="file://$out/check/files" WARDEN_VERSION="v$version" \
        WARDEN_INSTALL_DIR="$out/check/bin" sh install.sh >"$out/check/install.log" 2>&1; then
        cat "$out/check/install.log" >&2
        die "install.sh failed on the archives just built"
    fi
    got=$("$out/check/bin/warden" --version) || die "the installed warden doesn't run"
    [ "$got" = "warden $version" ] || die "install.sh installed '$got', expected 'warden $version'"
    say "installed: $got"
    if WARDEN_DOWNLOAD_URL="file://$out/check/tampered" WARDEN_VERSION="v$version" \
        WARDEN_INSTALL_DIR="$out/check/bin-tampered" sh install.sh >/dev/null 2>&1; then
        die "install.sh installed an archive whose checksum does not match"
    fi
    [ ! -e "$out/check/bin-tampered/warden" ] || die "install.sh left a binary behind after refusing a tampered archive"
    say "a tampered archive is refused"
else
    say "skipped: no $host_arch archive was built (--arch)"
fi
rm -rf "$out/check" "$out/stage"

# ------------------------------------------------------------------ files
n=$((n + 1))
step "$n. Build information and upload"
info="$out/macos-build-info.txt"
{
    echo "Warden $version, macOS archives"
    echo "commit: $sha"
    echo "built: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "host: macOS $(sw_vers -productVersion 2>/dev/null || echo '?') $host_arch"
    echo "rustc: $(rustc_version)"
    echo "dirty: $([ -n "$(git status --porcelain)" ] && echo yes || echo no)"
} > "$info"
sed 's/^/   | /' "$info"

files=()
for f in "$out"/warden-"$version"-macos-*.tar.gz "$out"/warden-gui-"$version"-macos-*.zip; do
    if [ -f "$f" ]; then files+=("$f"); fi
done
[ "${#files[@]}" -gt 0 ] || die "no archive was built"
say "${#files[@]} archive(s) in $out:"
for f in "${files[@]}"; do say "  $(basename "$f")  $(du -h "$f" | cut -f1)"; done

if [ "$upload" = 0 ]; then
    say "not uploaded (--no-upload)"
    exit 0
fi
prerelease=()
case "$version" in *-*) prerelease=(--prerelease) ;; esac
if [ "$draft_state" = none ]; then
    say "creating the draft release $tag (target: $branch)"
    gh release create "$tag" --draft --target "$branch" --title "Warden $version" \
        --notes "Draft. The macOS archives were built on a Mac (macos-build-info.txt names the commit). The Release workflow adds the Linux archives, SHA256SUMS and install.sh, then publishes." \
        ${prerelease[@]+"${prerelease[@]}"} "${files[@]}"
else
    say "replacing the macOS files of the draft $tag"
    gh release upload "$tag" "${files[@]}" --clobber
fi
# Last: the workflow takes the files only when this names its commit.
gh release upload "$tag" "$info" --clobber
repo=$(gh repo view --json nameWithOwner --jq .nameWithOwner 2>/dev/null || echo "")
say "uploaded${repo:+: https://github.com/$repo/releases}"
echo
echo "dist-macos: done. The Release workflow publishes $tag once its Linux builds pass."

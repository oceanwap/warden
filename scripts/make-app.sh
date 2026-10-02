#!/bin/sh
# Build Warden.app from a local build, so the Dock and Cmd-Tab show Warden's
# icon (macOS takes an app's icon from its bundle: a bare `warden-gui` binary
# is a generic "exec" tile).
#
#   cargo build --release -p warden -p warden-gui
#   scripts/make-app.sh                  # target/Warden.app
#   scripts/make-app.sh ~/Applications   # ~/Applications/Warden.app
#   open target/Warden.app
#
# The bundle holds copies of target/release/warden-gui and warden. It is
# signed ad hoc (Apple silicon runs only signed code); it is not notarized.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
dest=${1:-$root/target}
bin=${WARDEN_BIN_DIR:-$root/target/release}
app="$dest/Warden.app"

[ "$(uname -s)" = Darwin ] || {
    echo "make-app.sh: Warden.app is a macOS bundle; run this on a Mac" >&2
    exit 1
}
for f in warden-gui warden; do
    [ -x "$bin/$f" ] || {
        echo "make-app.sh: $bin/$f is missing: run \`cargo build --release -p warden -p warden-gui\` first" >&2
        exit 1
    }
done
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -n 1)
[ -n "$version" ] || {
    echo "make-app.sh: no version in Cargo.toml" >&2
    exit 1
}

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin/warden-gui" "$bin/warden" "$app/Contents/MacOS/"
cp "$root/assets/icon/Warden.icns" "$app/Contents/Resources/Warden.icns"
cat > "$app/Contents/Info.plist" <<PLIST
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
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null
xattr -cr "$app"
codesign --force --deep --sign - "$app"
# Finder and the Dock cache icons by path: nudge them to read this one.
touch "$app"
echo "make-app.sh: $app ($version); open it with: open \"$app\""

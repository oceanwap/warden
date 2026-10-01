#!/usr/bin/env bash
# Tests scripts/dist-macos.sh without a Mac: macOS-only tools (uname, codesign,
# plutil, ditto, arch, sw_vers), rustup, cargo and gh are stubs that record
# what they were asked to do. What this proves: the control flow, the file
# names and contents, the checks that stop a bad release, and the exact gh
# calls. What it cannot prove: that the real compilers and codesign work on a
# Mac, which only a run on a Mac does.
#
#   bash scripts/test-dist-macos.sh        (CI runs it on every code change)
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
trap 'rm -rf "$T"' EXIT
fails=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
check() { # description command...
    local d=$1
    shift
    if "$@" >/dev/null 2>&1; then pass "$d"; else fail "$d"; fi
}

# ------------------------------------------------------------- the stubs
bin=$T/bin
mkdir -p "$bin"
cat >"$bin/uname" <<'EOF'
#!/bin/sh
case "$1" in
  -s) echo "${FAKE_OS:-Darwin}" ;;
  -m) echo "${FAKE_ARCH:-arm64}" ;;
  *) exec /bin/uname "$@" ;;
esac
EOF
cat >"$bin/sw_vers" <<'EOF'
#!/bin/sh
echo 14.5
EOF
cat >"$bin/rustup" <<'EOF'
#!/bin/sh
echo "rustup $*" >>"$FAKE_LOG"
EOF
cat >"$bin/rustc" <<'EOF'
#!/bin/sh
echo "rustc 1.95.0 (stub)"
EOF
cat >"$bin/cargo" <<'EOF'
#!/bin/sh
# cargo [+toolchain] about --version | about generate … -o FILE | build … --bin NAME --target T
echo "cargo $*" >>"$FAKE_LOG"
case "$1" in +*) shift ;; esac
case "$1" in
  about)
    if [ "$2" = generate ]; then
      prev=
      for a in "$@"; do
        [ "$prev" = -o ] && echo "notices" >"$a"
        prev=$a
      done
    fi
    exit 0 ;;
  build)
    name=; target=
    while [ $# -gt 0 ]; do
      case "$1" in --bin) name=$2 ;; --target) target=$2 ;; esac
      shift
    done
    version=$(sed -n '/^\[package\]/,/^\[/{s/^version *= *"\(.*\)".*/\1/p;}' Cargo.toml | head -1)
    mkdir -p "$CARGO_TARGET_DIR/$target/release"
    printf '#!/bin/sh\necho "%s %s"\n' "$name" "$version" >"$CARGO_TARGET_DIR/$target/release/$name"
    chmod +x "$CARGO_TARGET_DIR/$target/release/$name"
    exit 0 ;;
esac
exit 0
EOF
cat >"$bin/arch" <<'EOF'
#!/bin/sh
shift
exec "$@"
EOF
cat >"$bin/codesign" <<'EOF'
#!/bin/sh
echo "codesign $*" >>"$FAKE_LOG"
EOF
cat >"$bin/plutil" <<'EOF'
#!/bin/sh
[ -f "$2" ] && grep -q '<plist' "$2"
EOF
cat >"$bin/ditto" <<'EOF'
#!/bin/sh
# ditto -c -k --keepParent SRC DST
src=$4; dst=$5
(cd "$(dirname "$src")" && zip -qr "$dst" "$(basename "$src")")
EOF
cat >"$bin/shasum" <<'EOF'
#!/bin/sh
[ "$1" = -a ] && shift 2
exec sha256sum "$@"
EOF
cat >"$bin/gh" <<'EOF'
#!/bin/sh
echo "gh $*" >>"$FAKE_LOG"
case "$1 $2" in
  "auth status") exit 0 ;;
  "repo view") echo oceanwap/warden; exit 0 ;;
  "release view")
    case "${FAKE_GH:-none}" in
      none) echo "release not found" >&2; exit 1 ;;
      draft) echo true ;;
      published) echo false ;;
    esac
    exit 0 ;;
esac
exit 0
EOF
chmod +x "$bin"/*

# --------------------------------------------- a clean copy of the working tree
mkrepo() { # dir
    mkdir -p "$1"
    (cd "$root" && git ls-files -co --exclude-standard -z | tar --null -cf - -T -) | tar -xf - -C "$1"
    (cd "$1" && git init -q . && git add -A && git -c user.email=t@t -c user.name=t commit -qm test)
}

run() { # repo log extra-env… -- args…: run dist-macos.sh in the copy with the stubs
    local repo=$1 log=$2
    shift 2
    : >"$log"
    local envs=()
    while [ $# -gt 0 ] && [ "$1" != -- ]; do envs+=("$1"); shift; done
    shift
    (cd "$repo" && env PATH="$bin:$PATH" FAKE_LOG="$log" ${envs[@]+"${envs[@]}"} \
        bash scripts/dist-macos.sh "$@")
}

version=$(sed -n '/^\[package\]/,/^\[/{s/^version *= *"\(.*\)".*/\1/p;}' "$root/Cargo.toml" | head -1)
echo "scripts/dist-macos.sh, stubbed (version $version)"

# ------------------------------------------------ 1. a build, no upload
R1=$T/r1
mkrepo "$R1"
sha=$(cd "$R1" && git rev-parse HEAD)
run "$R1" "$T/log1" -- --no-upload >"$T/out1" 2>&1
check "build without upload exits 0" test $? -eq 0
out=$R1/target/dist-macos
for f in warden-$version-macos-arm64.tar.gz warden-$version-macos-x86_64.tar.gz \
    warden-gui-$version-macos-arm64.zip warden-gui-$version-macos-x86_64.zip macos-build-info.txt; do
    check "writes $f" test -s "$out/$f"
done
check "the build info names the commit" grep -qx "commit: $sha" "$out/macos-build-info.txt"
check "the build info says the tree is clean" grep -qx "dirty: no" "$out/macos-build-info.txt"
tar -tzf "$out/warden-$version-macos-x86_64.tar.gz" >"$T/tar.lst" 2>&1
for f in warden README.md LICENSE-MIT LICENSE-APACHE THIRD-PARTY-LICENSES.txt contrib/nginx.conf contrib/wardend.service; do
    check "CLI archive holds $f" grep -qx "warden-$version-macos-x86_64/$f" "$T/tar.lst"
done
check "CLI archive has no AppleDouble files" sh -c "! grep -q '/\._' '$T/tar.lst'"
unzip -Z1 "$out/warden-gui-$version-macos-arm64.zip" >"$T/zip.lst" 2>&1
for f in Info.plist MacOS/warden-gui MacOS/warden Resources/README-GUI.md Resources/THIRD-PARTY-LICENSES-GUI.txt Resources/contrib/nginx.conf; do
    check "Warden.app holds $f" grep -qx "Warden.app/Contents/$f" "$T/zip.lst"
done
check "the app is signed and verified" sh -c "grep -q 'codesign --force --deep --sign - ' '$T/log1' && grep -q 'codesign --verify' '$T/log1'"
check "both targets were built, with the pinned toolchain" sh -c "
    grep -q 'cargo +[0-9.]* build --release --locked --bin warden --target aarch64-apple-darwin' '$T/log1' &&
    grep -q 'cargo +[0-9.]* build --release --locked --bin warden --target x86_64-apple-darwin' '$T/log1' &&
    grep -q 'cargo +[0-9.]* build --release --locked -p warden-gui --bin warden-gui --target x86_64-apple-darwin' '$T/log1'"
check "the targets were added with rustup" grep -q 'rustup target add --toolchain [0-9.]* x86_64-apple-darwin' "$T/log1"
check "install.sh verified the archive and refused a tampered one" sh -c "grep -q 'tampered archive is refused' '$T/out1' && grep -q 'installed: warden $version' '$T/out1'"
check "no gh call without upload" sh -c "! grep -q '^gh ' '$T/log1'"
check "scratch files are removed" test ! -e "$out/stage"

# ------------------------------------------------ 2. upload: no release yet
run "$R1" "$T/log2" -- >"$T/out2" 2>&1
check "upload (no release yet) exits 0" test $? -eq 0
check "a draft release is created at the branch, with the archives" sh -c "
    grep -q '^gh release create v$version --draft --target main --title Warden $version --notes .*macos-arm64.tar.gz' '$T/log2' &&
    grep '^gh release create' '$T/log2' | grep -q 'warden-gui-$version-macos-x86_64.zip'"
check "the build info is uploaded last" sh -c "grep '^gh release' '$T/log2' | tail -1 | grep -q '^gh release upload v$version .*/macos-build-info.txt --clobber\$'"
check "the build info is not part of the create call" sh -c "! grep '^gh release create' '$T/log2' | grep -q '/macos-build-info.txt'"

# ------------------------------------------------ 3. upload: a draft exists
run "$R1" "$T/log3" FAKE_GH=draft -- >"$T/out3" 2>&1
check "upload to an existing draft exits 0" test $? -eq 0
check "the draft's files are replaced, nothing created" sh -c "
    grep -q '^gh release upload v$version .*warden-$version-macos-arm64.tar.gz .* --clobber\$' '$T/log3' &&
    ! grep -q '^gh release create' '$T/log3'"

# ------------------------------------------------ 4. published: refuse, build nothing
run "$R1" "$T/log4" FAKE_GH=published -- >"$T/out4" 2>&1
check "a published release is refused" test $? -ne 0
check "…with the reason" grep -q "already published" "$T/out4"
check "…before building anything" sh -c "! grep -q ' build ' '$T/log4'"

# ------------------------------------------------ 5. not a Mac
run "$R1" "$T/log5" FAKE_OS=Linux -- --no-upload >"$T/out5" 2>&1
check "refuses to run off a Mac" test $? -ne 0
check "…and says why" grep -q "needs a Mac" "$T/out5"

# ------------------------------------------------ 6. uncommitted changes
echo stray >"$R1/stray.txt"
run "$R1" "$T/log6" -- --no-upload >"$T/out6" 2>&1
check "refuses a dirty tree" test $? -ne 0
check "…and says why" grep -q "uncommitted changes" "$T/out6"
run "$R1" "$T/log6b" -- --no-upload --allow-dirty >"$T/out6b" 2>&1
check "--allow-dirty builds anyway" test $? -eq 0
check "…and the build info says so" grep -qx "dirty: yes" "$R1/target/dist-macos/macos-build-info.txt"
rm -f "$R1/stray.txt"

# ------------------------------------------------ 7. one arch, CLI only
run "$R1" "$T/log7" FAKE_ARCH=x86_64 -- --no-upload --arch x86_64 --no-gui >"$T/out7" 2>&1
check "--arch x86_64 --no-gui exits 0" test $? -eq 0
check "…builds one CLI archive and nothing else" sh -c "
    [ \"\$(ls '$R1/target/dist-macos' | grep -c 'macos-.*\.\(tar.gz\|zip\)\$')\" = 1 ] &&
    test -s '$R1/target/dist-macos/warden-$version-macos-x86_64.tar.gz'"
check "…and installs it on an x86_64 Mac" grep -q "installed: warden $version" "$T/out7"

# ------------------------------------------------ 8. versions out of step
R8=$T/r8
mkrepo "$R8"
sed -i 's/^version = ".*"/version = "9.9.9"/' "$R8/gui/Cargo.toml"
(cd "$R8" && git -c user.email=t@t -c user.name=t commit -qam skew)
run "$R8" "$T/log8" -- --no-upload >"$T/out8" 2>&1
check "versions out of step are refused" test $? -ne 0
check "…and the manifest is named" grep -q "gui/Cargo.toml is version 9.9.9" "$T/out8"

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails check(s) failed. Output of the last run of each case: $T (kept)"
    trap - EXIT
    exit 1
fi
echo "all checks passed"

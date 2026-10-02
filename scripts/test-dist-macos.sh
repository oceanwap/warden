#!/usr/bin/env bash
# Tests scripts/dist-macos.sh without a Mac (it runs on Linux, with GNU tools): macOS-only tools (uname, codesign,
# plutil, ditto, hdiutil, diskutil, arch, sw_vers), rustup, cargo and gh are stubs that record
# what they were asked to do (the stub hdiutil makes a "disk image" out of a tar
# archive and mounts it by unpacking it). What this proves: the control flow, the
# file names and contents, the checks that stop a bad release (the disk image is
# mounted, looked into and always detached), and the exact gh calls. What it
# cannot prove: that the real compilers, codesign and hdiutil work on a Mac,
# which only a run on a Mac does.
#
#   bash scripts/test-dist-macos.sh        (CI runs it on every code change)
set -uo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
T=$(mktemp -d)
keep=0
mp=""
# Detaches the disk image this script mounted itself, whatever happens.
cleanup() {
    if [ -n "$mp" ]; then PATH="$T/bin:$PATH" hdiutil detach "$mp" >/dev/null 2>&1 || true; fi
    if [ "$keep" = 0 ]; then rm -rf "$T"; fi
}
trap cleanup EXIT
fails=0
pass() { printf '  ok    %s\n' "$1"; }
fail() { printf '  FAIL  %s\n' "$1"; fails=$((fails + 1)); }
# SHA-256 of a file: sha256sum on Linux, shasum on a Mac.
sha256() {
    if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

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
    [ -n "${FAKE_MOVE_HEAD:-}" ] && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m moved
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
# ditto -c -k --keepParent --norsrc SRC DST.zip | ditto -x -k ZIP DIR | ditto SRC DST
echo "ditto $*" >>"$FAKE_LOG"
case "$1" in
  -c) src=$5; dst=$6; (cd "$(dirname "$src")" && zip -qr "$dst" "$(basename "$src")") ;;
  -x) unzip -q "$3" -d "$4" ;;
  *) cp -R "$1" "$2" ;;
esac
EOF
cat >"$bin/hdiutil" <<'EOF'
#!/bin/sh
# create -volname V -srcfolder DIR -fs F -format UDZO -ov OUT | attach … -mountpoint DIR FILE | detach DIR
# The "image" is a tar archive. FAKE_HDIUTIL: create-fail, attach-fail, no-link (no Applications link inside).
echo "hdiutil $*" >>"$FAKE_LOG"
verb=$1
shift
case "$verb" in
  create)
    src=
    while [ $# -gt 1 ]; do
      case "$1" in -srcfolder) src=$2; shift ;; -volname|-fs|-format) shift ;; esac
      shift
    done
    [ "${FAKE_HDIUTIL:-}" = create-fail ] && exit 1
    ex=--exclude=./nothing
    [ "${FAKE_HDIUTIL:-}" = no-link ] && ex=--exclude=./Applications
    tar -C "$src" "$ex" -czf "$1" . ;;
  attach)
    [ "${FAKE_HDIUTIL:-}" = attach-fail ] && exit 1
    m=
    while [ $# -gt 1 ]; do
      case "$1" in -mountpoint) m=$2; shift ;; esac
      shift
    done
    tar -xzf "$1" -C "$m" ;;
esac
exit 0
EOF
cat >"$bin/diskutil" <<'EOF'
#!/bin/sh
echo "   Volume Name:               ${FAKE_VOLNAME:-Warden}"
EOF
cat >"$bin/xattr" <<'EOF'
#!/bin/sh
echo "xattr $*" >>"$FAKE_LOG"
EOF
cat >"$bin/sysctl" <<'EOF'
#!/bin/sh
echo "${FAKE_TRANSLATED:-0}"
EOF
cat >"$bin/shasum" <<'EOF'
#!/bin/sh
# Linux has sha256sum; a Mac has the real shasum (this one shadows it on PATH).
[ "$1" = -a ] && shift 2
if command -v sha256sum >/dev/null 2>&1; then exec sha256sum "$@"; fi
exec /usr/bin/shasum -a 256 "$@"
EOF
cat >"$bin/gh" <<'EOF'
#!/bin/sh
echo "gh $*" >>"$FAKE_LOG"
case "$1 $2" in
  "api user") [ "${FAKE_GH_LOGIN:-ok}" = ok ]; exit $? ;;
  "repo view") echo oceanwap/warden; exit 0 ;;
  "release view")
    n=$(cat "$FAKE_LOG.views" 2>/dev/null || echo 0); n=$((n + 1)); echo $n >"$FAKE_LOG.views"
    mode=${FAKE_GH:-none}
    # draft-then-published: published by someone while the build ran.
    [ "$mode" = draft-then-published ] && { if [ $n -le 1 ]; then mode=draft; else mode=published; fi; }
    case "$mode" in
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
    rm -f "$log.views"
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
    warden-gui-$version-macos-arm64.zip warden-gui-$version-macos-x86_64.zip \
    Warden-$version-macos-arm64.dmg Warden-$version-macos-x86_64.dmg macos-build-info.txt; do
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
for f in Info.plist MacOS/warden-gui MacOS/warden Resources/README-GUI.md Resources/FONT-LICENSES.txt Resources/THIRD-PARTY-LICENSES-GUI.txt Resources/contrib/nginx.conf; do
    check "Warden.app holds $f" grep -qx "Warden.app/Contents/$f" "$T/zip.lst"
done
check "the app is signed and verified" sh -c "grep -q 'codesign --force --deep --sign - ' '$T/log1' && grep -q 'codesign --verify' '$T/log1'"
check "both targets were built, with the pinned toolchain" sh -c "
    grep -q 'cargo +[0-9.]* build --release --locked --bin warden --target aarch64-apple-darwin' '$T/log1' &&
    grep -q 'cargo +[0-9.]* build --release --locked --bin warden --target x86_64-apple-darwin' '$T/log1' &&
    grep -q 'cargo +[0-9.]* build --release --locked -p warden-gui --bin warden-gui --target x86_64-apple-darwin' '$T/log1'"
check "the targets were added with rustup" grep -q 'rustup target add --toolchain [0-9.]* x86_64-apple-darwin' "$T/log1"
check "install.sh verified the archive and refused a tampered one" sh -c "grep -q 'tampered archive is refused' '$T/out1' && grep -q 'installed: warden $version' '$T/out1'"
check "the disk images are made with hdiutil: volume Warden, compressed (UDZO), from a staging folder" sh -c "
    [ \"\$(grep -c '^hdiutil create -volname Warden -srcfolder .*/stage/dmg-[a-z0-9_]* -fs HFS+ -format UDZO -ov .*/Warden-$version-macos-[a-z0-9_]*.dmg\$' '$T/log1')\" = 2 ]"
check "each disk image is signed ad hoc and verified" sh -c "
    grep -q 'codesign --force --sign - .*/Warden-$version-macos-arm64.dmg' '$T/log1' &&
    grep -q 'codesign --verify .*/Warden-$version-macos-x86_64.dmg' '$T/log1'"
check "each image is mounted read-only at a folder of our own, and detached" sh -c "
    [ \"\$(grep -c '^hdiutil attach -nobrowse -readonly -mountpoint .*/stage/mnt-' '$T/log1')\" = 2 ] &&
    [ \"\$(grep -c '^hdiutil detach .*/stage/mnt-' '$T/log1')\" = 2 ]"
check "Warden.app inside is verified on the mounted image" sh -c "grep -q 'codesign --verify --deep --strict .*/stage/mnt-' '$T/log1'"
check "the bundled CLI of the mounted app is run" grep -q "warden $version" "$T/out1"
check "install.sh put Warden.app in place from the zip, and from the disk image" sh -c "grep -q 'installed: Warden.app (from the zip)' '$T/out1' && grep -q 'installed: Warden.app (from the disk image)' '$T/out1' && grep -q 'uninstalled again' '$T/out1'"
check "no gh call without upload" sh -c "! grep -q '^gh ' '$T/log1'"
check "scratch files are removed" test ! -e "$out/stage"

# ------------------------------------------------ 2. upload: no release yet
run "$R1" "$T/log2" -- >"$T/out2" 2>&1
check "upload (no release yet) exits 0" test $? -eq 0
check "a draft release is created at the branch, with the archives" sh -c "
    grep -q '^gh release create v$version --draft --target main --title Warden $version --notes .*macos-arm64.tar.gz' '$T/log2' &&
    grep '^gh release create' '$T/log2' | grep -q 'warden-gui-$version-macos-x86_64.zip' &&
    grep '^gh release create' '$T/log2' | grep -q 'Warden-$version-macos-arm64.dmg' &&
    grep '^gh release create' '$T/log2' | grep -q 'Warden-$version-macos-x86_64.dmg'"
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
    [ \"\$(ls '$R1/target/dist-macos' | grep -Ec 'macos-.*\.(tar\.gz|zip|dmg)\$')\" = 1 ] &&
    test -s '$R1/target/dist-macos/warden-$version-macos-x86_64.tar.gz'"
check "…and installs it on an x86_64 Mac" grep -q "installed: warden $version" "$T/out7"

# ------------------------------------------------ 8. versions out of step
R8=$T/r8
mkrepo "$R8"
# No `sed -i`: its argument differs between GNU and BSD (macOS) sed.
sed 's/^version = ".*"/version = "9.9.9"/' "$R8/gui/Cargo.toml" >"$R8/gui/Cargo.toml.new" && mv "$R8/gui/Cargo.toml.new" "$R8/gui/Cargo.toml"
(cd "$R8" && git -c user.email=t@t -c user.name=t commit -qam skew)
run "$R8" "$T/log8" -- --no-upload >"$T/out8" 2>&1
check "versions out of step are refused" test $? -ne 0
check "…and the manifest is named" grep -q "gui/Cargo.toml is version 9.9.9" "$T/out8"

# ------------------------------------------------ 9. the info file vouches for each archive
run "$R1" "$T/log9" -- --no-upload >"$T/out9" 2>&1
for f in warden-$version-macos-arm64.tar.gz warden-$version-macos-x86_64.tar.gz warden-gui-$version-macos-arm64.zip warden-gui-$version-macos-x86_64.zip \
    Warden-$version-macos-arm64.dmg Warden-$version-macos-x86_64.dmg; do
    want=$(sha256 "$R1/target/dist-macos/$f")
    check "the build info lists $f with its checksum" grep -qx "sha256 $want $f" "$R1/target/dist-macos/macos-build-info.txt"
done
check "the app's attributes are cleared before signing" sh -c "grep -q '^xattr -cr ' '$T/log9' && grep -q '^ditto .*--norsrc' '$T/log9'"

# ------------------------------------------------ 10. part of a release is never uploaded
run "$R1" "$T/log10" -- --arch arm64 >"$T/out10" 2>&1
check "--arch without --no-upload is refused" test $? -ne 0
check "…before building anything" sh -c "! grep -q ' build ' '$T/log10'"
run "$R1" "$T/log10b" -- --no-gui >"$T/out10b" 2>&1
check "--no-gui without --no-upload is refused" test $? -ne 0
check "…and says why" grep -q "needs every file" "$T/out10b"
run "$R1" "$T/log10c" -- --no-dmg >"$T/out10c" 2>&1
check "--no-dmg without --no-upload is refused" test $? -ne 0
check "…before building anything" sh -c "! grep -q ' build ' '$T/log10c'"

# ------------------------------------------------ 11. --out never empties anything outside target/
run "$R1" "$T/log11" -- --no-upload --out src >"$T/out11" 2>&1
check "--out outside target/ is refused" test $? -ne 0
check "…and src/ is still there" test -d "$R1/src"
run "$R1" "$T/log11b" -- --no-upload --out target >"$T/out11b" 2>&1
check "--out target itself is refused" test $? -ne 0
run "$R1" "$T/log11c" -- --no-upload --out target/elsewhere >"$T/out11c" 2>&1
check "--out inside target/ works" test -s "$R1/target/elsewhere/macos-build-info.txt"

# ------------------------------------------------ 12. --check
run "$R1" "$T/log12" -- --check >"$T/out12" 2>&1
check "--check passes when everything is in place" test $? -eq 0
check "…and builds and uploads nothing" sh -c "! grep -q ' build ' '$T/log12' && ! grep -q '^gh release' '$T/log12'"
echo stray >"$R1/stray.txt"
run "$R1" "$T/log12b" -- --check >"$T/out12b" 2>&1
check "--check does not mind a dirty tree (a release is about to edit it)" test $? -eq 0
rm -f "$R1/stray.txt"
run "$R1" "$T/log12c" FAKE_GH_LOGIN=no -- --check >"$T/out12c" 2>&1
check "--check fails when gh is not logged in" test $? -ne 0
check "…and says how to log in" grep -q "gh auth login" "$T/out12c"
run "$R1" "$T/log12d" FAKE_OS=Linux -- --check >"$T/out12d" 2>&1
check "--check fails off a Mac" test $? -ne 0

# ------------------------------------------------ 13. replacing a draft: the old info file goes first
run "$R1" "$T/log13" FAKE_GH=draft -- >"$T/out13" 2>&1
d=$(grep -n "^gh release delete-asset v$version macos-build-info.txt" "$T/log13" | head -1 | cut -d: -f1)
u=$(grep -n "^gh release upload .*warden-$version-macos-arm64.tar.gz" "$T/log13" | head -1 | cut -d: -f1)
check "replacing a draft deletes the old info file before the archives" sh -c "[ -n '$d' ] && [ -n '$u' ] && [ '${d:-0}' -lt '${u:-0}' ]"

# ------------------------------------------------ 14. published, or HEAD moved, during the build
run "$R1" "$T/log14" FAKE_GH=draft-then-published -- >"$T/out14" 2>&1
check "a release published during the build is not touched" test $? -ne 0
check "…and says so" grep -q "was published while this was building" "$T/out14"
check "…nothing was uploaded" sh -c "! grep -Eq '^gh release (upload|create)' '$T/log14'"
run "$R1" "$T/log14b" FAKE_MOVE_HEAD=1 -- >"$T/out14b" 2>&1
check "a commit made during the build stops the upload" test $? -ne 0
check "…and says so" grep -q "HEAD moved during the build" "$T/out14b"
check "…nothing was uploaded" sh -c "! grep -Eq '^gh release (upload|create)' '$T/log14b'"

# ------------------------------------------------ 15. a tag on another commit
R15=$T/r15
mkrepo "$R15"
(cd "$R15" && git tag "v$version" && git -c user.email=t@t -c user.name=t commit -q --allow-empty -m later)
run "$R15" "$T/log15" -- >"$T/out15" 2>&1
check "an upload from a HEAD that is not the commit the tag names: refused" test $? -ne 0
check "…and says what to check out" grep -q "check out the commit the tag names" "$T/out15"
check "…before anything was built or uploaded" sh -c "! grep -q ' build ' '$T/log15' && ! grep -Eq '^gh release (upload|create)' '$T/log15'"
# The runner's way: a build that is not uploaded is tied to no release (a dry
# run on any branch has no tag at its commit, or the tag of an older release).
run "$R15" "$T/log15b" -- --no-upload >"$T/out15b" 2>&1
check "a build that is not uploaded ignores a tag on another commit" test $? -eq 0
check "…and still writes its build info for the commit it was built from" grep -qx "commit: $(cd "$R15" && git rev-parse HEAD)" "$R15/target/dist-macos/macos-build-info.txt"

# ------------------------------------------------ 16. a Rosetta shell is an arm64 Mac
run "$R1" "$T/log16" FAKE_ARCH=x86_64 FAKE_TRANSLATED=1 -- --no-upload >"$T/out16" 2>&1
check "under Rosetta the arm64 archive is the one installed" grep -q "install.sh on this Mac (the archive for arm64)" "$T/out16"

# ------------------------------------------------ 17. the disk images, mounted by this test too
run "$R1" "$T/log17" -- --no-upload >"$T/out17" 2>&1
check "a full build for the disk image checks exits 0" test $? -eq 0
for arch in arm64 x86_64; do
    dmg=$R1/target/dist-macos/Warden-$version-macos-$arch.dmg
    mp=$T/mnt-$arch
    mkdir -p "$mp"
    if PATH="$bin:$PATH" FAKE_LOG="$T/log17m" hdiutil attach -nobrowse -readonly -mountpoint "$mp" "$dmg" >/dev/null 2>&1; then
        pass "the $arch image mounts read-only"
    else
        fail "the $arch image mounts read-only"
    fi
    check "$arch image: Warden.app has its Info.plist" test -f "$mp/Warden.app/Contents/Info.plist"
    check "$arch image: the bundled CLI says warden $version" sh -c "[ \"\$('$mp/Warden.app/Contents/MacOS/warden' --version)\" = 'warden $version' ]"
    check "$arch image: the GUI is there and runs" sh -c "[ \"\$('$mp/Warden.app/Contents/MacOS/warden-gui' --version)\" = 'warden-gui $version' ]"
    check "$arch image: Warden.app passes codesign --verify" env PATH="$bin:$PATH" FAKE_LOG="$T/log17m" codesign --verify --deep --strict "$mp/Warden.app"
    check "$arch image: Applications is a link to /Applications" sh -c "[ -L '$mp/Applications' ] && [ \"\$(readlink '$mp/Applications')\" = /Applications ]"
    PATH="$bin:$PATH" FAKE_LOG="$T/log17m" hdiutil detach "$mp" >/dev/null 2>&1
    mp=""
done

# ------------------------------------------------ 18. a bad disk image stops the build, and is detached
count() { grep -c "$1" "$2" || true; }
run "$R1" "$T/log18a" FAKE_HDIUTIL=no-link -- --no-upload >"$T/out18a" 2>&1
check "an image without the Applications link is refused" test $? -ne 0
check "…and says so" grep -q "no Applications link" "$T/out18a"
check "…and it was detached" sh -c "[ \"\$(grep -c '^hdiutil attach' '$T/log18a')\" = 1 ] && [ \"\$(grep -c '^hdiutil detach' '$T/log18a')\" = 1 ]"
check "…and nothing was uploaded or released" sh -c "! grep -q '^gh release' '$T/log18a'"
run "$R1" "$T/log18b" FAKE_HDIUTIL=attach-fail -- --no-upload >"$T/out18b" 2>&1
check "an image that cannot be mounted is refused" test $? -ne 0
check "…and says so" grep -q "cannot mount Warden-$version-macos-" "$T/out18b"
check "…and a failed attach is still followed by a detach (it may be half mounted)" test "$(count '^hdiutil detach' "$T/log18b")" -ge 1
run "$R1" "$T/log18c" FAKE_HDIUTIL=create-fail DMG_RETRY_SLEEP=0 -- --no-upload >"$T/out18c" 2>&1
check "hdiutil create failing is refused after three tries" test $? -ne 0
check "…three tries" test "$(count '^hdiutil create' "$T/log18c")" = 3
check "…and says so" grep -q "hdiutil could not make Warden-$version-macos-" "$T/out18c"
run "$R1" "$T/log18d" FAKE_VOLNAME=Other -- --no-upload >"$T/out18d" 2>&1
check "an image whose volume is not called Warden is refused" test $? -ne 0
check "…and says so" grep -q "the volume is called 'Other'" "$T/out18d"
check "…and it was detached" sh -c "[ \"\$(grep -c '^hdiutil attach' '$T/log18d')\" = \"\$(grep -c '^hdiutil detach' '$T/log18d')\" ]"

# ------------------------------------------------ 19. --no-dmg
run "$R1" "$T/log19" -- --no-upload --no-dmg >"$T/out19" 2>&1
check "--no-dmg --no-upload exits 0" test $? -eq 0
check "…builds no disk image and runs no hdiutil" sh -c "! ls '$R1/target/dist-macos' | grep -q '\.dmg\$' && ! grep -q '^hdiutil' '$T/log19'"
check "…but still the zips" test -s "$R1/target/dist-macos/warden-gui-$version-macos-arm64.zip"
check "…and Warden.app still installs from the zip" grep -q "installed: Warden.app (from the zip)" "$T/out19"
run "$R1" "$T/log19b" -- --no-upload --no-gui >"$T/out19b" 2>&1
check "--no-gui builds no disk image either" sh -c "! ls '$R1/target/dist-macos' | grep -q '\.dmg\$' && ! grep -q '^hdiutil' '$T/log19b'"

# ------------------------------------------------ 20. the runner's way: --no-upload, no gh at all
run "$R1" "$T/log20" FAKE_GH_LOGIN=no FAKE_GH=published -- --no-upload >"$T/out20" 2>&1
check "--no-upload needs no gh login, and a published release of this version does not matter" test $? -eq 0
check "…gh was never called" sh -c "! grep -q '^gh ' '$T/log20'"
missing=""
for f in warden-$version-macos-arm64.tar.gz warden-$version-macos-x86_64.tar.gz warden-gui-$version-macos-arm64.zip warden-gui-$version-macos-x86_64.zip \
    Warden-$version-macos-arm64.dmg Warden-$version-macos-x86_64.dmg macos-build-info.txt; do
    [ -s "$R1/target/dist-macos/$f" ] || missing="$missing $f"
done
check "…and every file the workflow uploads as an artifact is there" test -z "$missing"
extra=""
for f in "$R1"/target/dist-macos/*; do
    case "$(basename "$f")" in
        warden-*.tar.gz|warden-gui-*.zip|Warden-*.dmg|macos-build-info.txt|notices) ;;
        *) extra="$extra $(basename "$f")" ;;
    esac
done
check "…and the output directory holds nothing else (but the notices folder)" test -z "$extra"

echo
if [ "$fails" -gt 0 ]; then
    echo "$fails check(s) failed. Output of the last run of each case: $T (kept)"
    keep=1
    exit 1
fi
echo "all checks passed"

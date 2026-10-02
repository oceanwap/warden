#!/usr/bin/env bash
# Linux packages from built binaries, with nfpm (https://nfpm.goreleaser.com):
#
#   warden_<version>-<rev>_<amd64|arm64>.deb              CLI (+ wardend): /usr/bin/warden
#   warden-gui_<version>-<rev>_<amd64|arm64>.deb          GUI: /usr/bin/warden-gui, menu entry, icons
#   warden-<version>-<rev>.<x86_64|aarch64>.rpm           the same two packages as rpm
#   warden-gui-<version>-<rev>.<x86_64|aarch64>.rpm
#   (--formats archlinux: warden[-gui]-<version>-<rev>-<arch>.pkg.tar.zst)
#
# plus their lines in SHA256SUMS. docs/packages.md says what is inside and why.
# The recipes are contrib/nfpm.yaml and contrib/nfpm-gui.yaml.
#
# Usage: scripts/dist-linux-packages.sh [OPTIONS] BIN_DIR VERSION ARCH
#
#   BIN_DIR    a directory with the release binary `warden` and, for the
#              warden-gui package, `warden-gui` (what release.yml builds)
#   VERSION    1.2.3 or 1.2.3-rc.1 (a leading v is dropped); the binaries must
#              say so in --version when they can run here
#   ARCH       x86_64 | arm64 (also amd64, aarch64): the binaries' architecture
#
# Options:
#   --notices DIR   the third-party notices from `cargo about` (release.yml's
#                   "licenses" job): THIRD-PARTY-LICENSES.txt, and
#                   THIRD-PARTY-LICENSES-GUI.txt for the GUI package
#                   (default: BIN_DIR)
#   --out DIR       where the packages go (default: dist)
#   --formats LIST  comma separated: deb, rpm, archlinux (default: deb,rpm).
#                   No apk: the binaries link glibc and Alpine is musl.
#   --release N     package revision, 1, 2, ... (default 1): raise it to
#                   repackage the same version
#   --no-gui        only the warden package
#   --no-sums       leave SHA256SUMS alone
#   -h, --help
#
# Environment:
#   NFPM                      the nfpm to run (default: nfpm on PATH); 2.47.0 was
#                             used, `go install github.com/goreleaser/nfpm/v2/cmd/nfpm@v2.47.0`
#   WARDEN_PKG_MAINTAINER     "Name <address>" in the packages (TODO(owner): the real one)
#   SOURCE_DATE_EPOCH         the time stamped on every file (default: the last
#                             commit's, else now), so the same inputs give the same package
#
# Writes only into --out (and reads the repository's LICENSE-*, README.md,
# warden.example.toml, contrib/ and assets/): never installs anything.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }
die() { echo "dist-linux-packages: $*" >&2; exit 1; }

notices="" out=dist formats=deb,rpm release=1 gui=1 sums=1
args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --notices) notices=${2:?--notices needs a directory}; shift 2 ;;
    --out) out=${2:?--out needs a directory}; shift 2 ;;
    --formats) formats=${2:?--formats needs a list}; shift 2 ;;
    --release) release=${2:?--release needs a number}; shift 2 ;;
    --no-gui) gui=0; shift ;;
    --no-sums) sums=0; shift ;;
    -h | --help) usage; exit 0 ;;
    -*) die "unknown option $1 (--help lists them)" ;;
    *) args+=("$1"); shift ;;
  esac
done
[ "${#args[@]}" -eq 3 ] || die "needs BIN_DIR VERSION ARCH (--help)"
bin_dir=${args[0]} version=${args[1]#v} arch_in=${args[2]}

[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]] ||
  die "version '$version' is not X.Y.Z or X.Y.Z-PRERELEASE"
[[ "$release" =~ ^[1-9][0-9]*$ ]] || die "--release '$release' is not a positive number"
case "$arch_in" in
  x86_64 | amd64) goarch=amd64 rpmarch=x86_64 elf_machine=3e00 ;;
  arm64 | aarch64) goarch=arm64 rpmarch=aarch64 elf_machine=b700 ;;
  *) die "architecture '$arch_in': x86_64 or arm64" ;;
esac
IFS=, read -r -a format_list <<<"$formats"
for f in "${format_list[@]}"; do
  case "$f" in
    deb | rpm | archlinux) ;;
    apk) die "no apk: the binaries link glibc, Alpine is musl (docs/packages.md)" ;;
    *) die "format '$f': deb, rpm or archlinux" ;;
  esac
done

root=$(cd "$(dirname "$0")/.." && pwd)
[ -d "$bin_dir" ] || die "$bin_dir is not a directory"
bin_dir=$(cd "$bin_dir" && pwd)
notices=$(cd "${notices:-$bin_dir}" 2>/dev/null && pwd) || die "--notices: no such directory"
mkdir -p "$out"
out=$(cd "$out" && pwd)
nfpm=${NFPM:-nfpm}
command -v "$nfpm" >/dev/null 2>&1 || die "nfpm not found: go install github.com/goreleaser/nfpm/v2/cmd/nfpm@v2.47.0, or set NFPM"

# The binaries must be what the name says: right architecture, right version.
host=$(uname -m)
check_binary() { # path expected-first-word
  local f=$1 name=$2 head machine got
  [ -f "$f" ] || die "$f is missing"
  head=$(od -An -tx1 -N4 "$f" | tr -d ' \n')
  [ "$head" = 7f454c46 ] || die "$f is not an ELF binary"
  machine=$(od -An -tx1 -j18 -N2 "$f" | tr -d ' \n')
  [ "$machine" = "$elf_machine" ] || die "$f is not built for $goarch (ELF machine $machine)"
  if { [ "$host" = x86_64 ] && [ "$goarch" = amd64 ]; } || { [ "$host" = aarch64 ] && [ "$goarch" = arm64 ]; }; then
    got=$("$f" --version) || die "$f --version failed"
    [ "$got" = "$name $version" ] || die "$f --version says '$got', not '$name $version'"
  fi
}
check_binary "$bin_dir/warden" warden
[ -f "$notices/THIRD-PARTY-LICENSES.txt" ] ||
  die "$notices/THIRD-PARTY-LICENSES.txt is missing: cargo about generate --locked --fail -m Cargo.toml about.hbs -o <dir>/THIRD-PARTY-LICENSES.txt (or --notices)"
if [ "$gui" = 1 ]; then
  check_binary "$bin_dir/warden-gui" warden-gui
  [ -f "$notices/THIRD-PARTY-LICENSES-GUI.txt" ] ||
    die "$notices/THIRD-PARTY-LICENSES-GUI.txt is missing (cargo about generate --locked --fail -m gui/Cargo.toml about.hbs, or --no-gui)"
fi

cd "$root"
epoch=${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct 2>/dev/null || true)}
epoch=${epoch:-$(date +%s)}
export SOURCE_DATE_EPOCH=$epoch   # nfpm stamps every file with it
maintainer=${WARDEN_PKG_MAINTAINER:-Warden authors <https://github.com/oceanwap/warden>}

# One package name per call: <recipe> <name>
packages=("contrib/nfpm.yaml warden")
[ "$gui" = 1 ] && packages+=("contrib/nfpm-gui.yaml warden-gui")

# Each format spells a pre-release its own way (1.2.3-rc.1): deb and rpm sort
# 1.2.3~rc.1 before 1.2.3, Arch has no hyphen in a version. The version inside
# the package is that; the file name keeps the plain 1.2.3-rc.1 (GitHub does not
# keep every character of an asset name, and the name is not what dpkg or rpm read).
made=()
for entry in "${packages[@]}"; do
  read -r recipe name <<<"$entry"
  for format in "${format_list[@]}"; do
    case "$format" in
      deb)
        v=${version/-/\~}
        file="${name}_${version}-${release}_${goarch}.deb"
        warden_dep="warden (= $v-$release)"
        ;;
      rpm)
        v=${version/-/\~}
        file="${name}-${version}-${release}.${rpmarch}.rpm"
        warden_dep="warden = $v-$release"
        ;;
      archlinux)
        v=${version//-/}
        file="${name}-${v}-${release}-${rpmarch}.pkg.tar.zst"
        warden_dep="warden=$v-$release"
        ;;
    esac
    rm -f "$out/$file"
    ARCH=$goarch VERSION=$v RELEASE=$release MAINTAINER=$maintainer \
      BIN_DIR=$bin_dir NOTICES_DIR=$notices WARDEN_DEP=$warden_dep \
      "$nfpm" package --config "$recipe" --packager "$format" --target "$out/$file" >&2
    [ -s "$out/$file" ] || die "nfpm did not write $out/$file"
    made+=("$file")
  done
done

# SHA256SUMS lines for what was just made, replacing earlier lines for the same
# files (the script runs once per architecture).
lines=$(cd "$out" && sha256sum -- "${made[@]}")
echo "$lines"
if [ "$sums" = 1 ]; then
  keep=$(mktemp)
  if [ -f "$out/SHA256SUMS" ]; then
    grep -vF -- "$(printf '  %s\n' "${made[@]}")" "$out/SHA256SUMS" >"$keep" || true
  fi
  printf '%s\n' "$lines" >>"$keep"
  sort -k2 "$keep" >"$out/SHA256SUMS"
  rm -f "$keep"
  echo "wrote ${#made[@]} packages and updated $out/SHA256SUMS" >&2
else
  echo "wrote ${#made[@]} packages in $out" >&2
fi

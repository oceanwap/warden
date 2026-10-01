#!/bin/bash
# Per-worker CPU pinning and SO_ATTACH_REUSEPORT_CBPF steering for `warden serve`
# (docs/benchmarks.md, "Tried and dropped"). Measured on 2 CPUs, where it
# could not show a win; re-run it on many-core hardware.
#
#   research/static-cpu-steering/run.sh            # from the repository root
#   ROUNDS=9 MODES=newconn research/static-cpu-steering/run.sh
#
# Builds two release binaries (as is, and with cbpf.patch: WARDEN_EXP_CBPF=<n>
# attaches `return cpu % n` to the listener's reuseport group), then per
# round and load (keep-alive; a new connection per request), interleaved:
#   unpinned       the workers wherever the scheduler puts them
#   pinned         worker i (all threads) on CPU i-1 (what `[workers] cpu_affinity` would do)
#   pinned+cbpf    and each new connection to the worker on the CPU it came in on
# with one worker per CPU (WORKERS, default: all CPUs) and the load generator
# unpinned, as a front proxy on the same host would be. Prints one line per
# measurement (bench/profile.ts: req/s, server CPU and context switches per
# request); compare medians.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
OUT=${OUT:-target/static-cpu-steering}
mkdir -p "$OUT"
W=${WORKERS:-$(nproc)}

cargo build --release --package warden
cp target/release/warden "$OUT/warden-plain"
git apply research/static-cpu-steering/cbpf.patch
trap 'git apply -R research/static-cpu-steering/cbpf.patch' EXIT
cargo build --release --package warden
cp target/release/warden "$OUT/warden-cbpf"
git apply -R research/static-cpu-steering/cbpf.patch
trap - EXIT
cargo build --release --package warden # back to the tree as it is

one() { # label, then bench/profile.ts arguments
  local label=$1
  shift
  local line
  line=$(bun bench/profile.ts --workers "$W" --rounds 1 "$@" 2>/dev/null |
    grep -E '^\| (req/s|server CPU|context)' | awk -F'|' '{gsub(/ +/, " "); printf "%s=%s ", $2, $3}')
  echo "$label $line"
}
for r in $(seq "${ROUNDS:-5}"); do
  for mode in ${MODES:-keepalive newconn}; do
    if [ "$mode" = keepalive ]; then A=(--requests 150000); else A=(--requests 40000 --new-connections); fi
    one "round $r $mode unpinned" --targets "warden@$OUT/warden-plain" "${A[@]}"
    one "round $r $mode pinned" --targets "warden@$OUT/warden-plain" --pin-servers "${A[@]}"
    WARDEN_EXP_CBPF=$W one "round $r $mode pinned+cbpf" --targets "warden@$OUT/warden-cbpf" --pin-servers "${A[@]}"
  done
done

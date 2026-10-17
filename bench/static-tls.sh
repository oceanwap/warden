#!/bin/bash
# Warden static-file benchmark: plain H1 vs TLS H1 vs TLS H2, small + big.
#
#   bench/static-tls.sh [warden-bin] [oha-bin] [--quick]
#
# Starts its own apps (ports 8451/8453) and stops ONLY those apps when done
# (never `warden kill`: that would take down the user's other apps). Needs
# oha >= 1.16 (`--insecure`), openssl, curl. The warden binary must already
# be built (release for real numbers). --quick runs fewer requests.
#
# Linux runs exercise kTLS where the kernel takes it; macOS runs the
# userspace TLS path. Same flags both sides, so columns stay comparable.
# --quick is a smoke test only (short runs never reach steady state).
set -u
WARDEN="${1:-./target/release/warden}"
OHA="${2:-oha}"
QUICK=0
[ "${3:-}" = "--quick" ] && QUICK=1

command -v "$OHA" >/dev/null || { echo "oha not found: $OHA"; exit 2; }
[ -x "$WARDEN" ] || { echo "warden binary not found: $WARDEN"; exit 2; }
if curl -ks -m 2 -o /dev/null http://127.0.0.1:8451/ 2>/dev/null || \
   curl -ks -m 2 -o /dev/null https://127.0.0.1:8453/ 2>/dev/null; then
  echo "ports 8451/8453 busy; stop those apps first"; exit 2
fi

if [ "$QUICK" = 1 ]; then SMALL_N=2000; SMALL_C=20; BIG_N=200; BIG_C=10;
else SMALL_N=20000; SMALL_C=50; BIG_N=2000; BIG_C=20; fi

DIR=$(mktemp -d "${TMPDIR:-/tmp}/static-tls-bench.XXXXXX")
cleanup() { "$WARDEN" stop bplain >/dev/null 2>&1; "$WARDEN" stop btls >/dev/null 2>&1; rm -rf "$DIR"; }
trap cleanup EXIT
mkdir -p "$DIR/site"
python3 -c "print('<h1>bench</h1>' * 100)" > "$DIR/site/small.html"
head -c 8388608 /dev/urandom > "$DIR/site/big.bin"
openssl req -x509 -newkey rsa:2048 -keyout "$DIR/key.pem" \
  -out "$DIR/cert.pem" -days 2 -nodes -subj "/CN=localhost" 2>/dev/null
printf '[app]\nname = "bplain"\nport = 8451\n\n[workers]\ncount = 1\n\n[static]\nroot = "%s"\n' "$DIR/site" > "$DIR/plain.toml"
printf '[app]\nname = "btls"\nport = 8453\n\n[workers]\ncount = 1\n\n[static]\nroot = "%s"\ncert = "%s"\nkey = "%s"\n' "$DIR/site" "$DIR/cert.pem" "$DIR/key.pem" > "$DIR/tls.toml"

wait_for() { # $1 = url
  for _ in $(seq 1 20); do curl -ks -m 2 -o /dev/null "$1" && return 0; sleep 1; done
  echo "server never answered: $1"; return 1
}
leg() { # $1 = label, rest = oha args + url
  label="$1"; shift
  rps=$("$OHA" --no-tui "$@" 2>&1 | grep -a "Requests/sec" | awk '{print $2}')
  printf '%-14s %12s req/s\n' "$label" "${rps:-FAILED}"
}

"$WARDEN" start "$DIR/plain.toml" >/dev/null || exit 1
wait_for http://127.0.0.1:8451/small.html
leg "plain-h1 small" -n "$SMALL_N" -c "$SMALL_C" http://127.0.0.1:8451/small.html
leg "plain-h1 big" -n "$BIG_N" -c "$BIG_C" http://127.0.0.1:8451/big.bin
"$WARDEN" stop bplain >/dev/null

"$WARDEN" start "$DIR/tls.toml" >/dev/null || exit 1
wait_for https://127.0.0.1:8453/small.html
leg "tls-h1 small" --insecure -n "$SMALL_N" -c "$SMALL_C" https://127.0.0.1:8453/small.html
leg "tls-h1 big" --insecure -n "$BIG_N" -c "$BIG_C" https://127.0.0.1:8453/big.bin
leg "tls-h2 small" --insecure --http2 -n "$SMALL_N" -c "$SMALL_C" https://127.0.0.1:8453/small.html
leg "tls-h2 big" --insecure --http2 -n "$BIG_N" -c "$BIG_C" https://127.0.0.1:8453/big.bin

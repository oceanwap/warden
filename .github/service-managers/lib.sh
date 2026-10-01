# Helpers for the service-manager checks (.github/workflows/service-managers.yml),
# sourced by linux.sh and macos.sh.
#
# Job logs need a GitHub login to read; annotations don't. So every failed
# check becomes one `::error` annotation carrying the command, its output and
# the platform's diagnostics (unit status, journal or launchd state, the
# supervisors' logs), and each step ends with one `::notice` that counts what
# passed. GitHub keeps at most 10 annotations of each kind per step: past
# MAX_ERRORS the failures are only in the log and the summary notice.

GROUP=${GROUP:-checks}
PASSED=0
FAILED=0
FAILED_NAMES=()
RESULTS=()
ERRORS_SHOWN=0
MAX_ERRORS=${MAX_ERRORS:-8}

# Workflow-command escaping: the message, and properties (also ':' and ',').
esc() { python3 -c 'import sys; s=sys.stdin.read(); sys.stdout.write(s.replace("%","%25").replace("\r","%0D").replace("\n","%0A"))'; }
esc_prop() { python3 -c 'import sys; s=sys.stdin.read(); sys.stdout.write(s.replace("%","%25").replace("\r","%0D").replace("\n","%0A").replace(":","%3A").replace(",","%2C"))'; }

# What the platform script adds to every failure (overridden there).
diag() { :; }

note() { echo "----- $*"; }

pass() {
  PASSED=$((PASSED + 1))
  RESULTS+=("PASS	$1")
  echo "PASS  [$GROUP] $1"
}

# fail NAME DETAIL
fail() {
  local name=$1 detail=${2:-} d body
  FAILED=$((FAILED + 1))
  FAILED_NAMES+=("$name")
  RESULTS+=("FAIL	$name")
  d=$(diag 2>&1 | head -c 2300)
  body=$(printf '%s\n%s\n--- diagnostics ---\n%s' "$name" "$(printf '%s' "$detail" | tail -c 1300)" "$d")
  echo "FAIL  [$GROUP] $name"
  printf '%s\n' "$detail" | sed 's/^/      /'
  echo "      --- diagnostics ---"
  printf '%s\n' "$d" | sed 's/^/      /'
  if [ "$ERRORS_SHOWN" -lt "$MAX_ERRORS" ]; then
    ERRORS_SHOWN=$((ERRORS_SHOWN + 1))
    echo "::error title=$(printf '%s' "$GROUP: $name" | esc_prop)::$(printf '%s' "$body" | head -c 3900 | esc)"
  fi
}

# check NAME CMD...: passes when CMD exits 0; its output is the failure's detail.
check() {
  local name=$1 out code
  shift
  out=$("$@" 2>&1)
  code=$?
  if [ "$code" -eq 0 ]; then
    pass "$name"
    return 0
  fi
  fail "$name" "$(printf '$ %s\nexit %s\n%s' "$*" "$code" "$out")"
  return 1
}

# check_not NAME CMD...: passes when CMD fails.
check_not() {
  local name=$1 out code
  shift
  out=$("$@" 2>&1)
  code=$?
  if [ "$code" -ne 0 ]; then
    pass "$name"
    return 0
  fi
  fail "$name" "$(printf 'expected this to fail:\n$ %s\nexit 0\n%s' "$*" "$out")"
  return 1
}

# wait_for NAME SECONDS CMD...: CMD every half second until it succeeds.
wait_for() {
  local name=$1 secs=$2 out code t0=$SECONDS
  shift 2
  while :; do
    out=$("$@" 2>&1)
    code=$?
    [ "$code" -eq 0 ] && break
    [ $((SECONDS - t0)) -ge "$secs" ] && break
    sleep 0.5
  done
  if [ "$code" -eq 0 ]; then
    pass "$name ($((SECONDS - t0)) s)"
    return 0
  fi
  fail "$name" "$(printf 'not within %s s:\n$ %s\nexit %s\n%s' "$secs" "$*" "$code" "$out")"
  return 1
}

# expect_eq NAME EXPECTED ACTUAL
expect_eq() {
  if [ "$2" = "$3" ]; then
    pass "$1"
  else
    fail "$1" "$(printf 'expected: %s\ngot:      %s' "$2" "$3")"
  fi
}

# Run CMD, show it and its output in the log, keep the output in $OUT and
# the exit code in $CODE. Where `timeout` exists (Linux), CMD gets
# RUN_TIMEOUT seconds (exit 124 after that): a `systemctl start` of a
# Type=notify unit that never gets ready would otherwise block for its
# TimeoutStartSec. CMD may be a function the script exported (`export -f`).
RUN_TIMEOUT=${RUN_TIMEOUT:-180}
run() {
  echo "\$ $*"
  if command -v timeout >/dev/null; then
    OUT=$(timeout -k 10 "$RUN_TIMEOUT" bash -c '"$@"' run "$@" 2>&1)
  else
    OUT=$("$@" 2>&1)
  fi
  CODE=$?
  [ "$CODE" -eq 124 ] && OUT="$OUT
(killed: still running after $RUN_TIMEOUT s)"
  printf '%s\n' "$OUT" | sed 's/^/  | /'
  echo "  (exit $CODE)"
}

# run_ok NAME CMD...: run, and pass when it exits 0 (its output is the detail).
run_ok() {
  local name=$1
  shift
  run "$@"
  if [ "$CODE" -eq 0 ]; then
    pass "$name"
    return 0
  fi
  fail "$name" "$(printf '$ %s\nexit %s\n%s' "$*" "$CODE" "$OUT")"
  return 1
}

# contains NAME TEXT NEEDLE: TEXT has NEEDLE in it.
contains() {
  case "$2" in
    *"$3"*) pass "$1" ;;
    *) fail "$1" "$(printf 'expected to find: %s\nin:\n%s' "$3" "$2")" ;;
  esac
}

# The app answers HTTP on 127.0.0.1:PORT.
http_ok() { curl -fsS --max-time 3 "http://127.0.0.1:$1/" >/dev/null; }

# Nothing listens on 127.0.0.1:PORT any more.
http_down() { ! curl -s --max-time 2 -o /dev/null "http://127.0.0.1:$1/"; }

# One notice with the counts, the step summary as a table; exit status.
finish() {
  local total=$((PASSED + FAILED)) msg
  msg="$PASSED of $total checks passed"
  if [ "$FAILED" -gt 0 ]; then
    msg="$msg. FAILED ($FAILED): $(printf '%s; ' "${FAILED_NAMES[@]}")"
  fi
  echo "::notice title=$(printf '%s' "$GROUP: summary" | esc_prop)::$(printf '%s' "$msg" | head -c 3900 | esc)"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    {
      echo "### $GROUP: $PASSED of $total checks passed"
      echo
      echo "| | check |"
      echo "|---|---|"
      for r in "${RESULTS[@]}"; do echo "| ${r%%	*} | ${r#*	} |"; done
      echo
    } >>"$GITHUB_STEP_SUMMARY"
  fi
  [ "$FAILED" -eq 0 ]
}

#!/usr/bin/env bash
# Does Warden work on THIS Mac? Builds it, runs the tests that exercise the
# macOS adapter, then drives a real supervisor and wardend and checks what the
# macOS adapter (src/platform/macos.rs) is there to provide: CPU and memory,
# the process owner, listening-port readiness, a process's environment, the
# host's numbers, the boot id, and how fast `warden serve` answers. It also
# kills a supervisor with kill -9, as macOS has no parent-death signal, and
# checks that the next start of the app stops the workers left running
# (src/platform/orphans.rs) instead of starting a second set next to them.
#
#   scripts/mac-check.sh [--quick] [--no-build]
#
#   --quick      skip the full unit-test run and clippy (still builds, runs
#                the platform tests and every smoke check)
#   --no-build   use target/debug/warden as it is
#   -h, --help   this help
#
# Nothing here touches your own Warden apps: the smoke checks run with their
# own WARDEN_HOME and runtime directory under /tmp and stop everything they
# start. It needs: Rust (cargo), python3 (Xcode's command line tools have it).
# Bun is not needed.
#
# The result: a summary on the terminal and the same, with the output of
# every failed step, in target/mac-check/report.txt (target/ is ignored by git;
# the logs of each step are next to it). Send that file when something fails.
#
# Plain POSIX tools only: macOS ships bash 3.2 and BSD sed/grep/ps.
set -u

here=$(cd "$(dirname "$0")/.." && pwd)
cd "$here" || exit 2
quick=0
build=1
while [ $# -gt 0 ]; do
    case "$1" in
        --quick) quick=1 ;;
        --no-build) build=0 ;;
        -h|--help) sed -n '2,/^set -u/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
        *) echo "mac-check: unknown option $1 (see --help)" >&2; exit 2 ;;
    esac
    shift
done

# MAC_CHECK_ALLOW_ANY_OS=1 is for developing this script on Linux (the `ps -E`
# check cannot pass there).
[ "$(uname -s)" = Darwin ] || [ -n "${MAC_CHECK_ALLOW_ANY_OS:-}" ] || { echo "mac-check: this checks macOS; you are on $(uname -s)" >&2; exit 2; }
command -v cargo >/dev/null 2>&1 || { echo "mac-check: cargo not found (install Rust: https://rustup.rs)" >&2; exit 2; }

out=$here/target/mac-check
rm -rf "$out"
mkdir -p "$out"
report=$out/report.txt
summary=$out/summary.txt
: >"$summary"
bin=${CARGO_TARGET_DIR:-$here/target}/debug/warden
failed=0

# A short path: a Unix socket's path is at most 104 bytes on macOS.
work=/tmp/wmc.$$
rm -rf "$work"
mkdir -p "$work/home" "$work/run"
cleanup() {
    if [ -x "$bin" ]; then
        WARDEN_HOME=$work/home WARDEN_RUNTIME_DIR=$work/run WARDEN_NO_DAEMON=1 "$bin" kill --yes >/dev/null 2>&1
    fi
    # The orphan check's workers, if it stopped half way.
    if [ -n "${tag:-}" ]; then pkill -f "^sleep $tag\$" >/dev/null 2>&1; fi
    rm -rf "$work"
}
trap cleanup EXIT

say() { printf '%s\n' "$*" | tee -a "$report"; }
result() { # PASS|FAIL|SKIP name [detail]
    printf '%-5s %s%s\n' "$1" "$2" "${3:+  ($3)}" | tee -a "$summary"
    if [ "$1" = FAIL ]; then failed=$((failed + 1)); fi
    return 0
}
# step <name> <logfile> <command...>: PASS when the command exits 0; the tail
# of its output joins the report when it does not.
step() {
    local name=$1 log=$2
    shift 2
    if "$@" >"$out/$log" 2>&1; then
        result PASS "$name"
    else
        result FAIL "$name" "see $log"
        { echo; echo "---- $name: last lines of $log"; tail -n 60 "$out/$log"; } >>"$report"
    fi
}
w() { WARDEN_HOME=$work/home WARDEN_RUNTIME_DIR=$work/run "$@"; }


# ----------------------------------------------------------- JSON helpers
# (python3 reads the JSON; written out here, not inline, because bash 3.2
# mis-parses a heredoc inside $( ).)
cat >"$work/doctor.py" <<'PY'
import json, sys
try:
    rows = json.load(open(sys.argv[1]))
except Exception as e:
    print("unreadable: %s" % e)
    sys.exit(0)
for r in rows:
    if r["check"] == "platform":
        print("ok" if r["level"] == "ok" else r["detail"])
        break
else:
    print("no platform row")
PY
# list.py <list.json> <user> <port of files> <port of wrap> <socket path>: one
# PASS/FAIL line per fact about the smoke apps.
cat >"$work/list.py" <<'PY'
import json, sys
apps, me = json.load(open(sys.argv[1])), sys.argv[2]
fport, wport, spath = int(sys.argv[3]), int(sys.argv[4]), sys.argv[5]
def pick(name):
    for a in apps:
        s = a.get("status") or {}
        if s.get("app") == name:
            return s
    return None
def show(ok, name, detail=""):
    print("%s\t%s\t%s" % ("PASS" if ok else "FAIL", name, detail))
burn, files = pick("burn"), pick("files")
if not burn or not files or not burn.get("workers") or not files.get("workers"):
    show(False, "both smoke apps are listed with workers", json.dumps(apps)[:300])
    sys.exit(0)
b, f = burn["workers"][0], files["workers"][0]
show(b.get("state") == "RUNNING", "the shell loop is RUNNING", b.get("state"))
show((b.get("rss_bytes") or 0) > 100000, "memory (RSS) of a running worker is read", b.get("rss_bytes"))
show((b.get("cpu_seconds") or 0) > 0.5, "CPU time of a busy worker is read", b.get("cpu_seconds"))
show((b.get("cpu_percent") or 0) > 20, "CPU percent of a busy worker is read", b.get("cpu_percent"))
show(burn.get("user") == me, "the app's user is the current user", burn.get("user"))
show(f.get("state") == "RUNNING", "the listener is RUNNING (ready by listening port)", f.get("state"))
show((f.get("rss_bytes") or 0) > 1000000, "memory of the listener is read", f.get("rss_bytes"))
fl = f.get("listening") or []
show(any(l.get("kind") == "tcp" and l.get("port") == fport and l.get("addr") == "127.0.0.1" for l in fl),
     "the listener's port and address are read (libproc)", json.dumps(fl))
wrap = pick("wrap")
if not wrap or not wrap.get("workers"):
    show(False, "the wrapper app is listed", json.dumps(apps)[:300])
else:
    wl = wrap["workers"][0].get("listening") or []
    show(any(l.get("kind") == "tcp" and l.get("port") == wport for l in wl),
         "a port in a child of the started process is found (sh -> python)", json.dumps(wl))
    show(any(l.get("kind") == "unix" and l.get("path", "").endswith("/s.sock") for l in wl),
         "a Unix socket of the child is found", json.dumps(wl))
PY
# pid.py <app> [not-this-pid]: the supervisor pid of an app from `list --json`.
cat >"$work/pid.py" <<'PY'
import json, sys
try:
    apps = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for a in apps:
    s = a.get("status") or {}
    if s.get("app") == sys.argv[1] and s.get("pid") and (len(sys.argv) < 3 or str(s["pid"]) != sys.argv[2]):
        print(s["pid"])
PY

# workers.py <app>: the pids of the app's workers once every one is RUNNING.
cat >"$work/workers.py" <<'PY'
import json, sys
try:
    apps = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for a in apps:
    s = a.get("status") or {}
    if s.get("app") == sys.argv[1]:
        ws = s.get("workers") or []
        if ws and all(w.get("state") == "RUNNING" for w in ws):
            print(" ".join(str(w["pid"]) for w in ws))
PY
# orphan-doctor.py <doctor.json> <app>: exit 0 when `warden doctor` warns of
# workers of that app that outlived their supervisor.
cat >"$work/orphan-doctor.py" <<'PY'
import json, sys
try:
    rows = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
sys.exit(0 if any(r["check"] == "orphan sweep" and r["level"] == "warn" and ("app %s:" % sys.argv[2]) in r["detail"] for r in rows) else 1)
PY

# ------------------------------------------------------------------ the Mac
{
    echo "warden mac-check $(date '+%Y-%m-%d %H:%M:%S')"
    echo "commit:  $(git rev-parse --short HEAD 2>/dev/null) $(git status --porcelain 2>/dev/null | head -1 | sed 's/.*/(uncommitted changes)/')"
    echo "macOS:   $(sw_vers -productVersion 2>/dev/null) ($(sw_vers -buildVersion 2>/dev/null)), $(uname -m)"
    echo "cpus:    $(sysctl -n hw.ncpu 2>/dev/null), memory $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1048576 )) MB"
    echo "rust:    $(rustc --version 2>/dev/null) / $(cargo --version 2>/dev/null)"
    echo "user:    $(id -un) (uid $(id -u))"
    echo "bun:     $(bun --version 2>/dev/null || echo none)   node: $(node --version 2>/dev/null || echo none)   python3: $(python3 --version 2>&1 | head -1)"
    echo
} >"$report"
cat "$report"

# ------------------------------------------------------------------- build
if [ $build = 1 ]; then
    step "cargo build" build.log cargo build --bin warden
fi
if [ ! -x "$bin" ]; then
    result FAIL "no $bin: nothing to check"
    say; say "report: $report"
    exit 1
fi

# ------------------------------------------------------------- unit tests
step "platform and sys::darwin unit tests" tests-platform.log \
    cargo test --bin warden -- platform:: sys::darwin:: doctor:: fleet::tests::origin
if [ $quick = 0 ]; then
    step "all unit tests" tests-all.log cargo test --bin warden
    if cargo clippy --version >/dev/null 2>&1; then
        step "clippy (warnings are errors)" clippy.log cargo clippy --all-targets -- -D warnings
    else
        result SKIP "clippy" "not installed: rustup component add clippy"
    fi
fi

# ------------------------------------------------------------------ doctor
w "$bin" doctor --json >"$out/doctor.json" 2>"$out/doctor.err"
if command -v python3 >/dev/null 2>&1; then
    verdict=$(python3 "$work/doctor.py" "$out/doctor.json")
    if [ "$verdict" = ok ]; then
        result PASS "warden doctor: the macOS adapter reads memory, CPU, owner, environment, ports, host, boot id"
    else
        result FAIL "warden doctor: platform self-check" "$verdict"
        { echo; echo "---- doctor"; "$bin" doctor 2>&1 | cut -c1-220; } >>"$report"
    fi
else
    result SKIP "doctor / smoke checks" "python3 not found (xcode-select --install)"
    say; say "report: $report"
    exit $((failed > 0))
fi

# --------------------------------------------------------- smoke: one app
# A CPU burner (a shell loop) and a listener (python's http.server on a port
# Warden is told about, so readiness is the listening-port check).
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'; }
port=$(free_port)
export WARDEN_NO_DAEMON=1
w "$bin" start /bin/sh --name burn --interpreter none -- -c 'while :; do :; done' >"$out/start-burn.log" 2>&1
w "$bin" start "python3 -m http.server $port --bind 127.0.0.1" --name files --port "$port" >"$out/start-files.log" 2>&1
# A shell that runs a python server with a TCP port and a Unix socket: what `npm run start` looks like.
wport=$(free_port)
cat >"$work/wrapped.py" <<'PY'
import os, socket, sys, time
t = socket.socket()
t.bind(("127.0.0.1", int(sys.argv[1])))
t.listen()
path = sys.argv[2]
if os.path.exists(path):
    os.unlink(path)
u = socket.socket(socket.AF_UNIX)
u.bind(path)
u.listen()
time.sleep(600)
PY
w "$bin" start /bin/sh --name wrap --interpreter none -- -c "python3 $work/wrapped.py $wport $work/s.sock; :" >"$out/start-wrap.log" 2>&1
sleep 3
w "$bin" list --json >"$out/list.json" 2>"$out/list.err"
w "$bin" list >"$out/list.txt" 2>&1
w "$bin" describe files >"$out/describe.txt" 2>&1
w "$bin" ports >"$out/ports.txt" 2>&1
unset WARDEN_NO_DAEMON
{ echo "---- warden list"; cat "$out/list.txt"; echo; } >>"$report"

me=$(id -un)
python3 "$work/list.py" "$out/list.json" "$me" "$port" "$wport" "$work/s.sock" >"$out/list-check.txt" 2>&1
while IFS=$'\t' read -r verdict name detail; do
    case "$verdict" in
        PASS) result PASS "$name" ;;
        FAIL) result FAIL "$name" "$detail" ;;
    esac
done <"$out/list-check.txt"
[ -s "$out/list-check.txt" ] || result FAIL "reading list --json" "see list-check.txt"
if grep -q "$me" "$out/list.txt"; then result PASS "warden list shows the user column"; else result FAIL "warden list shows the user column" "see list.txt"; fi
if grep -q "│ user *│ $me" "$out/describe.txt"; then result PASS "warden describe shows the user"; else result FAIL "warden describe shows the user" "see describe.txt"; fi
if grep -q "│ ports *│ $port (localhost only)" "$out/describe.txt"; then result PASS "warden describe shows the ports"; else result FAIL "warden describe shows the ports" "see describe.txt"; fi
if grep -q "http://localhost:$port" "$out/ports.txt" && grep -q "s.sock" "$out/ports.txt"; then result PASS "warden ports lists the port and the Unix socket"; else result FAIL "warden ports lists the port and the Unix socket" "see ports.txt"; fi
WARDEN_HOME=$work/home WARDEN_RUNTIME_DIR=$work/run WARDEN_NO_DAEMON=1 "$bin" stop all >/dev/null 2>&1
WARDEN_HOME=$work/home WARDEN_RUNTIME_DIR=$work/run WARDEN_NO_DAEMON=1 "$bin" delete all >/dev/null 2>&1

# ------------------------------------------------------ smoke: static serve
# `warden serve`: the bodies are right and the answers are fast. Keep-alive
# requests from python, so the time is the server's and the kernel's (no
# browser). A Mac opens files on a thread (openat with O_RESOLVE_BENEATH or
# O_NOFOLLOW_ANY, no openat2); this shows what that costs.
#
# The bytes are compared with the file, not just counted: files above 16 KB
# go out through the macOS-only path (one sendfile(2) with an `sf_hdtr` header,
# src/sys.rs `sendfile_head`), which assumes that the length sendfile reports
# back counts the header too. If it counted only the file, the head would be
# sent twice and the next response on the connection would start in the wrong
# place: a wrong body or a broken keep-alive connection, which comparing every
# response of a reused connection shows. multi.bin is 3 MB: three pieces of the
# server's 1 MiB `sendfile` chunks, so the same holds across calls.
cat >"$work/lat.py" <<'PY'
import http.client, sys, time
port, name, path, rounds = int(sys.argv[1]), sys.argv[2], sys.argv[3], int(sys.argv[4])
want = open(path, "rb").read()
c = http.client.HTTPConnection("127.0.0.1", port)
times = []
for i in range(rounds):
    t = time.perf_counter()
    c.request("GET", "/" + name)
    r = c.getresponse()
    body = r.read()
    times.append((time.perf_counter() - t) * 1000)
    if r.status != 200 or body != want:
        at = next((k for k, (x, y) in enumerate(zip(body, want)) if x != y), min(len(body), len(want)))
        print("FAIL\t%s\tresponse %d: status %s, %d bytes (want %d), first difference at byte %d"
              % (name, i + 1, r.status, len(body), len(want), at))
        sys.exit(0)
times = sorted(times[min(20, rounds // 10):])
med, p99 = times[len(times) // 2], times[int(len(times) * 0.99)]
print("%s\t%s\tbytes equal in %d responses, median %.2f ms, p99 %.2f ms" % ("PASS" if med < 25 else "FAIL", name, rounds, med, p99))
PY
mkdir -p "$work/site"
python3 -c '
import os
d = "'"$work"'/site/"
open(d + "small.html", "w").write("<p>" + os.urandom(750).hex() + "</p>")
open(d + "big.html", "w").write("<p>" + os.urandom(45000).hex() + "</p>")
open(d + "multi.bin", "wb").write(os.urandom(3000000))'
sport=$(free_port)
w env WARDEN_NO_DAEMON=1 "$bin" serve "$work/site" "$sport" --name site >"$out/start-site.log" 2>&1
sleep 2
for f in "small.html 300" "big.html 300" "multi.bin 30"; do
    # shellcheck disable=SC2086 # split on purpose: "name rounds"
    set -- $f
    line=$(python3 "$work/lat.py" "$sport" "$1" "$work/site/$1" "$2" 2>&1 | tail -n 1)
    verdict=$(printf '%s' "$line" | cut -f1)
    detail=$(printf '%s' "$line" | cut -f3-)
    case "$verdict" in
        PASS) result PASS "static serve $1 (keep-alive, bytes compared)" "$detail" ;;
        *) result FAIL "static serve $1 (keep-alive, bytes compared)" "$detail" ;;
    esac
done
w env WARDEN_NO_DAEMON=1 "$bin" delete all >/dev/null 2>&1

# ------------------------------------------- smoke: a supervisor kill -9'd
# macOS has no parent-death signal: kill -9 on a supervisor leaves its workers
# running, still holding the app's port. The next start of the app must stop
# them before it starts new ones (src/platform/orphans.rs); without that, two
# sets of workers would share the port. Two workers that are `sleep` with a
# number only this run uses, so pgrep counts exactly them.
tag=$((600000 + $$ % 100000))
sleepers() { pgrep -f "^sleep $tag\$" | sort -n | tr '\n' ' '; }
words() { printf '%s' "$1" | wc -w | tr -d ' '; }
nd() { w env WARDEN_NO_DAEMON=1 "$@"; }
nd "$bin" start /bin/sh --name orphan --interpreter none -i 2 -- -c "exec sleep $tag" >"$out/start-orphan.log" 2>&1
sleep 2
before=$(sleepers)
sup=$(nd "$bin" list --json 2>/dev/null | python3 "$work/pid.py" orphan)
if [ -z "$sup" ] || [ "$(words "$before")" != 2 ]; then
    result FAIL "orphan sweep: two workers of the app are running" "supervisor '${sup:-none}', workers: ${before:-none} (see start-orphan.log)"
else
    kill -9 "$sup"
    sleep 1
    left=$(sleepers)
    if [ -z "$left" ]; then
        result SKIP "orphan sweep" "this macOS stopped the workers with the killed supervisor: nothing to sweep"
    else
        result PASS "workers outlive a supervisor killed with kill -9 (no parent-death signal)" "$(words "$left") of 2 still running"
        nd "$bin" doctor --json >"$out/doctor-orphan.json" 2>"$out/doctor-orphan.err"
        if python3 "$work/orphan-doctor.py" "$out/doctor-orphan.json" orphan; then
            result PASS "warden doctor lists the workers that outlived their supervisor"
        else
            result FAIL "warden doctor lists the workers that outlived their supervisor" "see doctor-orphan.json"
        fi
        # The next start of the app: new workers, after the old ones are stopped.
        nd "$bin" start orphan >"$out/restart-orphan.log" 2>&1
        now=""
        i=0
        while [ $i -lt 40 ]; do
            now=$(nd "$bin" list --json 2>/dev/null | python3 "$work/workers.py" orphan)
            [ -n "$now" ] && break
            sleep 1
            i=$((i + 1))
        done
        after=$(sleepers)
        old_alive=""
        for p in $before; do
            if kill -0 "$p" 2>/dev/null; then old_alive="$old_alive $p"; fi
        done
        if [ -z "$now" ]; then
            result FAIL "the app starts again after its supervisor was killed" "no running workers within 40 s (see restart-orphan.log)"
        elif [ "$(words "$after")" = 2 ] && [ -z "$old_alive" ]; then
            result PASS "the next start stopped the old workers first: 2 workers, none of the old ones" "was $before, now $after"
        else
            result FAIL "the next start stopped the old workers first" "$(words "$after") workers running ($after), old ones still alive:${old_alive:- none}"
            { echo; echo "---- orphan check: log"; tail -n 40 "$work/home/state/logs/orphan.log" 2>/dev/null; } >>"$report"
        fi
        if grep -q "left behind by a previous supervisor" "$work/home/state/logs/orphan.log" 2>/dev/null; then
            result PASS "the log says which workers were stopped, and why"
        else
            result FAIL "the log says which workers were stopped, and why" "no 'left behind by a previous supervisor' line in the app's log"
        fi
    fi
fi
nd "$bin" delete all >/dev/null 2>&1
pkill -f "^sleep $tag\$" >/dev/null 2>&1

# -------------------------------------------------------- smoke: wardend
# wardend: host events, and a supervisor it restarts with the environment it
# was started with (read from the dead supervisor's process: sysctl, no /proc).
w env WMC_MARK=kept-from-start "$bin" start /bin/sh --name idle --interpreter none -- -c 'sleep 600' >"$out/start-idle.log" 2>&1
sleep 2
if w "$bin" wardend status >"$out/daemon-status.txt" 2>&1; then result PASS "wardend runs"; else result FAIL "wardend runs" "see daemon-status.txt"; fi

w "$bin" events --json >"$out/events.ndjson" 2>"$out/events.err" &
ev=$!
sleep 5
kill $ev >/dev/null 2>&1
wait $ev 2>/dev/null
host=$(grep '"type":"host"' "$out/events.ndjson" | head -1)
if [ -n "$host" ] && printf '%s' "$host" | grep -q '"mem_total_bytes":[1-9]' && printf '%s' "$host" | grep -q '"cpu_percent":[0-9]'; then
    result PASS "wardend sends host events (CPU, memory, load)"
else
    result FAIL "wardend sends host events" "no usable host event in events.ndjson"
fi

sup=$(w "$bin" list --json 2>/dev/null | python3 "$work/pid.py" idle)
if [ -n "$sup" ]; then
    kill -9 "$sup" 2>/dev/null
    new=""
    i=0
    while [ $i -lt 40 ]; do
        sleep 1
        new=$(w "$bin" list --json 2>/dev/null | python3 "$work/pid.py" idle "$sup")
        [ -n "$new" ] && break
        i=$((i + 1))
    done
    if [ -z "$new" ]; then
        result FAIL "wardend restarts a killed supervisor" "no new supervisor within 40 s (see daemon logs under $work: kept only until exit)"
        w "$bin" wardend status >>"$report" 2>&1
    else
        result PASS "wardend restarts a killed supervisor"
        # `ps -E` prints the environment of a process of ours (on Linux, /proc).
        if [ -r "/proc/$new/environ" ]; then
            new_env=$(tr '\0' '\n' <"/proc/$new/environ")
        else
            new_env=$(ps -Eww -p "$new" 2>/dev/null)
        fi
        if printf '%s' "$new_env" | grep -q 'WMC_MARK=kept-from-start'; then
            result PASS "…with the environment it was started with (read via sysctl)"
        else
            result FAIL "…with the environment it was started with" "WMC_MARK is missing from the new supervisor's environment"
        fi
    fi
else
    result FAIL "found the supervisor's pid to kill" "list --json had no idle app"
fi
w "$bin" delete all >/dev/null 2>&1

# ----------------------------------------------------------------- summary
passed=$(grep -c '^PASS' "$summary")
skipped=$(grep -c '^SKIP' "$summary")
total="== $passed passed, $failed failed, $skipped skipped"
{
    echo
    echo "---- summary"
    cat "$summary"
    echo "$total"
} >>"$report"
echo
echo "$total"
echo "report: $report"
[ $failed = 0 ]

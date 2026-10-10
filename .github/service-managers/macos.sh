#!/usr/bin/env bash
# `warden startup` / `unstartup` against the real launchd of a GitHub macOS
# runner. Run by .github/workflows/service-managers.yml; every failed check
# is an annotation.
#
#   macos.sh agent    warden startup as the runner user: a LaunchAgent (gui/<uid>)
#   macos.sh daemon   sudo warden startup: a LaunchDaemon (system domain)
#
# The job runs `warden wardend --resurrect` with KeepAlive SuccessfulExit=false.
# Checked: the plist installs and loads, wardend from the job adopts the apps
# already running (without starting them twice), restarts a SIGKILLed
# supervisor; launchd restarts a SIGKILLed wardend (and leaves its
# supervisors alone) but not one that exited cleanly (`warden kill`); the job
# loaded again as at login or boot brings the saved apps back; `warden
# unstartup` unloads and removes it while the apps keep running.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh
. "$HERE/lib.sh"

MODE=${1:?usage: macos.sh agent|daemon}
W=/usr/local/bin/warden
LABEL=io.github.oceanwap.warden.daemon

if [ "$MODE" = agent ]; then
  GROUP="launchd LaunchAgent (user)"
  SUDO=
  RUN_AS=$(id -un)
  DOMAIN=gui/$(id -u)
  PLIST=$HOME/Library/LaunchAgents/$LABEL.plist
  APPS=$HOME/warden-ci
  API=mapi SITE=msite PORT_API=3301 PORT_SITE=3302 NODE=mnode PORT_NODE=3303
else
  GROUP="launchd LaunchDaemon (root)"
  SUDO=sudo
  RUN_AS=root
  DOMAIN=system
  PLIST=/Library/LaunchDaemons/$LABEL.plist
  APPS=/opt/warden-ci
  API=rapi SITE=rsite PORT_API=3311 PORT_SITE=3312 NODE=rnode PORT_NODE=3313
fi
TARGET=$DOMAIN/$LABEL

w() { $SUDO "$W" "$@"; }
lc() { $SUDO launchctl "$@"; }
# The job's pid per launchd (empty when it is not running).
job_pid() { lc print "$TARGET" 2>/dev/null | awk '$1 == "pid" && $2 == "=" { print $3; exit }'; }
app_field() { w list --json 2>/dev/null | jq -r --arg a "$1" ".[] | select(.app == \$a) | $2"; }
app_ready() { [ "$(app_field "$1" '"\(.status.workers_ready)/\(.status.workers_configured)"')" = "$2/$2" ]; }
sup_pid() { app_field "$1" '.status.pid // empty'; }
wardend_pid() { w wardend status --json 2>/dev/null | jq -r '.hello.pid // empty'; }
wardend_sees() {
  w wardend status --json | jq -e --arg a "$1" '.apps[] | select(.name == $a and .state == "running")' >/dev/null
}
# wardend answers, and is the process launchd runs for the job.
wardend_is_job() {
  local j
  j=$(job_pid)
  [ -n "$j" ] && [ "$(wardend_pid)" = "$j" ]
}
# The job's process changed from OLD, and that wardend answers.
job_restarted() {
  local j
  j=$(job_pid)
  [ -n "$j" ] && [ "$j" != "$1" ] && [ "$(wardend_pid)" = "$j" ]
}
# APP's supervisor is a new process (not OLD) and the app answers on PORT.
sup_restarted() {
  local p
  p=$(sup_pid "$1")
  [ -n "$p" ] && [ "$p" != "$2" ] && http_ok "$3"
}
state_dir() { if [ "$MODE" = agent ]; then echo "${XDG_STATE_HOME:-$HOME/.local/state}/warden"; else echo /var/lib/warden; fi; }
runtime_dir() {
  if [ "$MODE" = agent ]; then
    if [ -n "${XDG_RUNTIME_DIR:-}" ]; then echo "$XDG_RUNTIME_DIR/warden"; else echo "/tmp/warden-$(id -u)"; fi
  else
    echo /var/run/warden
  fi
}
none_left() {
  local left
  left=$(ps -axo user,pid,command | awk -v u="$RUN_AS" '$1 == u' |
    grep -E " $W( |$)| [^ ]*bun .*$APPS/server\.ts| [^ ]*node .*$APPS/server\.mjs" | grep -v grep)
  if [ -n "$left" ]; then
    echo "still running:"
    echo "$left"
    return 1
  fi
  http_down "$PORT_API" && http_down "$PORT_SITE" && http_down "$PORT_NODE"
}

diag() {
  echo "--- launchctl print $TARGET"
  lc print "$TARGET" 2>&1 | grep -E '^[[:space:]]*(state|pid|last exit code|runs|path|program) =' | head -8
  echo "--- warden wardend status"
  w wardend status 2>&1 | head -6 | cut -c1-160
  echo "--- warden list"
  w list 2>&1 | head -6 | cut -c1-160
  echo "--- processes"
  ps -axo user,pid,ppid,command | grep -E "[w]arden|[s]erver\.ts" | cut -c1-150 | head -10
  echo "--- wardend.log"
  $SUDO tail -n 12 "$(state_dir)/logs/wardend.log" 2>&1 | cut -c1-230
  echo "--- $API.log"
  $SUDO tail -n 6 "$(state_dir)/logs/$API.log" 2>&1 | cut -c1-230
}

setup_apps() {
  $SUDO mkdir -p "$APPS/site"
  $SUDO tee "$APPS/server.ts" >/dev/null <<'EOF'
// A Bun app for the service-manager checks: who answered.
Bun.serve({
  port: Number(process.env.PORT),
  fetch(req) {
    if (new URL(req.url).pathname === "/health") return new Response("ok");
    return Response.json({ app: process.env.WARDEN_APP, worker: process.env.WARDEN_WORKER_ID, pid: process.pid });
  },
});
EOF
  $SUDO tee "$APPS/server.mjs" >/dev/null <<'EOF'
// A Node app (node:http) for the service-manager checks.
import http from "node:http";
http
  .createServer((req, res) => res.end(JSON.stringify({ app: process.env.WARDEN_APP, pid: process.pid })))
  .listen(Number(process.env.PORT));
EOF
  echo "<h1>warden ci</h1>" | $SUDO tee "$APPS/site/index.html" >/dev/null
  $SUDO chmod -R a+rX "$APPS"
}

note "environment: $(sw_vers -productVersion 2>/dev/null), uid $(id -u), node $(node --version), sudo PATH: $(sudo sh -c 'echo $PATH')"

# ------------------------------------------------------------------ the apps

note "start and save three apps"
setup_apps
# macOS does not spread connections across SO_REUSEPORT listeners: one worker each.
run_ok "warden start $API" w start "$APPS/server.ts" --name "$API" --port "$PORT_API"
check "$API answers" http_ok "$PORT_API"
run_ok "warden serve (the $SITE app)" w serve "$APPS/site" "$PORT_SITE" --name "$SITE"
check "$SITE answers" http_ok "$PORT_SITE"
# Node has no reusePort on macOS: the shim must not ask for it (listen() would fail with ENOTSUP).
run_ok "warden start $NODE (node:http, through the shim)" w start "$APPS/server.mjs" --name "$NODE" --port "$PORT_NODE"
check "$NODE answers" http_ok "$PORT_NODE"
check "$NODE's log says one Node worker can hold the port here" \
  $SUDO grep -q "Node cannot share a port on this OS" "$(state_dir)/logs/$NODE.log"
run_ok "warden save" w save
API_SUP=$(sup_pid "$API")
SITE_SUP=$(sup_pid "$SITE")

# ------------------------------------------------------------------- startup

note "warden startup"
run_ok "warden startup" w startup
STARTUP_OUT=$OUT
check "$PLIST written" test -f "$PLIST"
check "the plist is valid (plutil -lint)" plutil -lint "$PLIST"
check "the plist runs warden wardend --resurrect" grep -q "<string>--resurrect</string>" "$PLIST"
check "the plist runs it as Interactive (not a throttled background job)" grep -q "<string>Interactive</string>" "$PLIST"
contains "startup says the job is loaded" "$STARTUP_OUT" "$TARGET: loaded"
check "launchd has the job ($TARGET)" lc print "$TARGET"
wait_for "launchd runs wardend for the job" 20 wardend_is_job
# A background job runs at priority 20 and its children inherit that; Interactive keeps 31.
normal_priority() { [ "$(ps -o pri= -p "$(wardend_pid)" | tr -d ' ')" -ge 31 ]; }
check "wardend runs at normal priority ($(ps -o pri= -p "$(wardend_pid)" | tr -d ' '))" normal_priority
wait_for "wardend sees $API running" 20 wardend_sees "$API"
wait_for "wardend sees $SITE running" 20 wardend_sees "$SITE"
expect_eq "$API not started twice (--resurrect left the running supervisor)" "$API_SUP" "$(sup_pid "$API")"
expect_eq "$SITE not started twice" "$SITE_SUP" "$(sup_pid "$SITE")"

# ---------------------------------------------------- crash: a supervisor

note "kill -9 of a supervisor: wardend restarts it"
OLD=$(sup_pid "$API")
OLD_WORKER=$(app_field "$API" '.status.workers[0].pid // empty')
run $SUDO kill -9 "$OLD"
wait_for "wardend restarted $API's supervisor and the app answers" 40 sup_restarted "$API" "$OLD" "$PORT_API"
wait_for "$API 1/1 ready again" 30 app_ready "$API" 1
if [ -n "$OLD_WORKER" ] && $SUDO kill -0 "$OLD_WORKER" 2>/dev/null; then
  # Documented (docs/platforms.md): no parent-death signal on macOS, so the
  # dead supervisor's worker lives on. Stop it so the checks below see only
  # the new one.
  note "the killed supervisor's worker (pid $OLD_WORKER) outlived it (no PDEATHSIG on macOS); stopping it"
  $SUDO kill "$OLD_WORKER"
fi

# ------------------------------------------------------------ crash: wardend

note "kill -9 of wardend: launchd restarts it (KeepAlive), the apps stay"
API_SUP=$(sup_pid "$API")
SITE_SUP=$(sup_pid "$SITE")
OLD=$(job_pid)
run $SUDO kill -9 "$OLD"
# launchd throttles respawns to one per 10 s.
wait_for "launchd restarted wardend" 40 job_restarted "$OLD"
check "$API kept answering" http_ok "$PORT_API"
check "$SITE kept answering" http_ok "$PORT_SITE"
expect_eq "$API's supervisor survived wardend's death" "$API_SUP" "$(sup_pid "$API")"
expect_eq "$SITE's supervisor survived wardend's death" "$SITE_SUP" "$(sup_pid "$SITE")"
wait_for "the new wardend sees $API running" 20 wardend_sees "$API"

# ------------------------------------------------------------------- update

note "warden update: the apps move to this binary without a restart"
workers() { app_field "$1" '[.status.workers[].pid] | sort | map(tostring) | join(",")'; }
inner() { app_field "$1" '.status.supervisor_pid // empty'; }
API_KEEPER=$(sup_pid "$API")
API_INNER=$(inner "$API")
API_WORKERS=$(workers "$API")
NODE_WORKERS=$(workers "$NODE")
run_ok "warden update --yes" w update --yes
wait_for "$API 1/1 ready after the update" 30 app_ready "$API" 1
expect_eq "$API keeps its process (the keeper)" "$API_KEEPER" "$(sup_pid "$API")"
check "$API has a new supervisor (was $API_INNER)" test "$(inner "$API")" != "$API_INNER"
expect_eq "$API keeps its workers" "$API_WORKERS" "$(workers "$API")"
expect_eq "$NODE keeps its workers" "$NODE_WORKERS" "$(workers "$NODE")"
check "$API answers after the update" http_ok "$PORT_API"
check "$NODE answers after the update" http_ok "$PORT_NODE"
check "$SITE answers after the update" http_ok "$PORT_SITE"
wait_for "launchd runs the new wardend for the job" 20 wardend_is_job
wait_for "the new wardend sees $API running" 20 wardend_sees "$API"

# ------------------------------------------- clean stop, then login / boot

note "warden kill: a clean exit, which launchd leaves stopped"
run_ok "warden kill --yes" w kill --yes
wait_for "no supervisor, worker or wardend left" 40 none_left
sleep 12
check_not "launchd did not restart wardend after its clean exit" test -n "$(job_pid)"

note "the job loaded again, as at the next login or boot"
# A new boot has a new boot id (and an empty /var/run); the marker makes a
# wardend that launchd restarts in the same boot leave stopped apps alone.
$SUDO rm -f "$(runtime_dir)/resurrected"
run_ok "launchctl bootout $TARGET" lc bootout "$TARGET"
run_ok "launchctl bootstrap $DOMAIN $PLIST" lc bootstrap "$DOMAIN" "$PLIST"
wait_for "wardend runs for the job again" 30 wardend_is_job
wait_for "$API resurrected and answering" 40 http_ok "$PORT_API"
wait_for "$SITE resurrected and answering" 30 http_ok "$PORT_SITE"
wait_for "$NODE resurrected and answering" 30 http_ok "$PORT_NODE"
wait_for "$API 1/1 ready" 30 app_ready "$API" 1
check "wardend's log says it resurrected $API" $SUDO grep -q "resurrected a saved app app=$API" "$(state_dir)/logs/wardend.log"

# ----------------------------------------------------------------- unstartup

note "warden unstartup"
run_ok "warden unstartup" w unstartup
check_not "$PLIST removed" test -e "$PLIST"
check_not "launchd no longer has the job" lc print "$TARGET"
wait_for "wardend stopped with the job" 15 bash -c "! $SUDO $W wardend status >/dev/null 2>&1"
check "$API keeps running after unstartup" http_ok "$PORT_API"
run_ok "warden kill --yes (after unstartup)" w kill --yes
wait_for "nothing left after warden kill" 40 none_left

finish

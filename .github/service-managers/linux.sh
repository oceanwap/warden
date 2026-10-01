#!/usr/bin/env bash
# `warden startup` / `unstartup` against the real systemd of a GitHub Ubuntu
# runner (systemd is PID 1 there, sudo needs no password). Run by
# .github/workflows/service-managers.yml; every failed check is an annotation.
#
#   linux.sh system   sudo warden startup: system units, everything as root
#   linux.sh user     warden startup as the runner user: user units, lingering
#
# Both modes: two saved apps (a Bun app scaled to 3 workers, a static site)
# must come back from a cold start through the units' boot wiring, a
# SIGKILLed supervisor and a SIGKILLed wardend must be restarted by systemd,
# `systemctl reload` must run a safe-reload, and `warden unstartup` must
# remove what startup installed (the apps keep running until `warden kill`).
# User mode also restarts the user manager (user@UID.service): its units
# stop and start as at a reboot.
set -u
HERE=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=lib.sh
. "$HERE/lib.sh"

MODE=${1:?usage: linux.sh system|user}
W=/usr/local/bin/warden
ME=$(id -un)
MY_UID=$(id -u)

if [ "$MODE" = system ]; then
  GROUP="systemd root (system units)"
  SUDO=sudo
  SCTL="sudo systemctl"
  APPS=/srv/warden-ci
  API=api SITE=site PORT_API=3101 PORT_SITE=3102
  UNIT_DIR=/etc/systemd/system
  WANTS=multi-user.target.wants
  RUN_AS=root
else
  GROUP="systemd user units (lingering)"
  SUDO=
  SCTL="systemctl --user"
  APPS=$HOME/warden-ci
  API=uapi SITE=usite PORT_API=3201 PORT_SITE=3202
  UNIT_DIR=$HOME/.config/systemd/user
  WANTS=default.target.wants
  RUN_AS=$ME
fi

w() { $SUDO "$W" "$@"; }
# shellcheck disable=SC2086
sc() { $SCTL "$@"; }
mainpid() { sc show -p MainPID --value "$1"; }
# UNIT has a main process other than OLD, and is active.
new_main_pid() {
  local p
  p=$(mainpid "$1")
  [ "$p" != 0 ] && [ "$p" != "$2" ] && sc is-active --quiet "$1"
}
# wardend answers on its socket, and is the unit's main process.
wardend_is_unit() { [ "$(w daemon status --json | jq .hello.pid)" = "$(mainpid wardend.service)" ]; }
# No pid of the comma-separated list A is in B (and B is not empty).
disjoint() {
  local a
  for a in ${1//,/ }; do
    case ",$2," in *",$a,"*)
      echo "pid $a is in both $1 and $2"
      return 1
      ;;
    esac
  done
  [ -n "$2" ]
}
journal() {
  if [ "$MODE" = system ]; then
    sudo journalctl --no-pager -o short-monotonic -n "$1" -u "warden@$API.service" -u "warden@$SITE.service" -u wardend.service
  else
    sudo journalctl --no-pager -o short-monotonic -n "$1" "_SYSTEMD_USER_UNIT=warden@$API.service" \
      "_SYSTEMD_USER_UNIT=warden@$SITE.service" _SYSTEMD_USER_UNIT=wardend.service
  fi
}

diag() {
  for u in "warden@$API" "warden@$SITE" wardend; do
    sc status --no-pager -n 0 "$u.service" 2>&1 | sed -n '1,4p' | cut -c1-160
  done
  echo "--- warden list"
  w list 2>&1 | head -6 | cut -c1-160
  echo "--- processes"
  ps -eo user:10,pid,ppid,args | grep -E "[w]arden|[s]erver\.ts" | cut -c1-150 | head -10
  echo "--- journal"
  journal 14 2>&1 | cut -c1-230
}

# "<ready>/<configured>" of APP from `warden list --json`.
app_field() { w list --json 2>/dev/null | jq -r --arg a "$1" ".[] | select(.app == \$a) | $2"; }
app_ready() { [ "$(app_field "$1" '"\(.status.workers_ready)/\(.status.workers_configured)"')" = "$2/$2" ]; }
wardend_sees() {
  w daemon status --json | jq -e --arg a "$1" --arg s "$2" \
    '.apps[] | select(.name == $a and .state == "running" and .supervised_by == $s)' >/dev/null
}
worker_pids() { app_field "$1" '[.status.workers[].pid] | sort | map(tostring) | join(",")'; }
# No supervisor, wardend, static worker or app worker of this mode's user is
# left (system mode: of any user, in case a unit ran them as someone else).
none_left() {
  local left
  left=$(ps -eo user:20,pid,args | awk -v u="$RUN_AS" -v any="$SUDO" 'any != "" || $1 == u' |
    grep -E " $W( |$)| [^ ]*bun .*$APPS/server\.ts" | grep -v grep)
  if [ -n "$left" ]; then
    echo "still running:"
    echo "$left"
    return 1
  fi
  http_down "$PORT_API" && http_down "$PORT_SITE"
}

setup_apps() {
  $SUDO mkdir -p "$APPS/site"
  $SUDO tee "$APPS/server.ts" >/dev/null <<'EOF'
// A Bun app for the service-manager checks: who answered, and as whom.
Bun.serve({
  port: Number(process.env.PORT),
  fetch(req) {
    if (new URL(req.url).pathname === "/health") return new Response("ok");
    return Response.json({ app: process.env.WARDEN_APP, worker: process.env.WARDEN_WORKER_ID, pid: process.pid });
  },
});
EOF
  echo "<h1>warden ci</h1>" | $SUDO tee "$APPS/site/index.html" >/dev/null
  $SUDO chmod -R a+rX "$APPS"
}

# ---------------------------------------------------------------- user manager

if [ "$MODE" = user ]; then
  note "the user manager"
  # Without a login session or lingering there is no user manager to talk to:
  # startup must say so (and enable nothing).
  if env -u XDG_RUNTIME_DIR -u DBUS_SESSION_BUS_ADDRESS systemctl --user show-environment >/dev/null 2>&1; then
    note "a user manager already runs for $ME: the no-manager case is not checked"
  else
    run env -u XDG_RUNTIME_DIR -u DBUS_SESSION_BUS_ADDRESS "$W" startup --user
    expect_eq "startup without a user manager exits 1" 1 "$CODE"
    contains "startup without a user manager says why and what to do" "$OUT" "user manager did not answer"
    rm -f "$UNIT_DIR/warden@.service" "$UNIT_DIR/wardend.service"
  fi
  run_ok "sudo loginctl enable-linger $ME" sudo loginctl enable-linger "$ME"
  export XDG_RUNTIME_DIR=/run/user/$MY_UID
  wait_for "user manager user@$MY_UID.service running" 30 test -S "$XDG_RUNTIME_DIR/systemd/private"
  check "systemctl --user answers" systemctl --user show-environment
fi

# ------------------------------------------------------------------ the apps

note "start, scale and save two apps"
setup_apps
run_ok "warden start $API" w start "$APPS/server.ts" --name "$API" -i 2 --port "$PORT_API"
check "$API answers" http_ok "$PORT_API"
run_ok "warden serve (the $SITE app)" w serve "$APPS/site" "$PORT_SITE" --name "$SITE" -i 2
check "$SITE answers" http_ok "$PORT_SITE"
run_ok "warden scale $API 3" w scale "$API" 3
run_ok "warden save" w save

# ------------------------------------------------------------------- startup

note "warden startup"
run_ok "warden startup" w startup
STARTUP_OUT=$OUT
for f in warden@.service wardend.service; do
  check "$UNIT_DIR/$f written" test -f "$UNIT_DIR/$f"
done
check "warden@.service runs this binary" grep -q "^ExecStart=\"$W\" start --config " "$UNIT_DIR/warden@.service"
if [ "$MODE" = system ]; then
  check "/etc/sysctl.d/99-warden.conf written" test -f /etc/sysctl.d/99-warden.conf
  expect_eq "net.ipv4.tcp_migrate_req applied" 1 "$(sysctl -n net.ipv4.tcp_migrate_req 2>&1)"
else
  expect_eq "lingering on for $ME" yes "$(loginctl show-user "$ME" -p Linger --value 2>&1)"
  check "user units carry PATH (bun)" grep -q '^Environment="PATH=' "$UNIT_DIR/warden@.service"
fi
for u in "warden@$API" "warden@$SITE" wardend; do
  check "$u.service enabled" sc is-enabled --quiet "$u.service"
done
check "wardend.service active" sc is-active --quiet wardend.service
wait_for "wardend answers: the unit's main process" 15 wardend_is_unit
contains "startup says the running apps move under systemd at the next start" "$STARTUP_OUT" "running outside systemd now"
check "$API still answers after startup" http_ok "$PORT_API"

# ---------------------------------------------------------------- cold start

note "cold start: everything stopped, then the units started as at boot"
run_ok "warden kill --yes" w kill --yes
wait_for "no supervisor, worker or wardend left" 40 none_left
check_not "wardend.service stopped by warden kill" sc is-active --quiet wardend.service
UNITS=$(ls "$UNIT_DIR/$WANTS" 2>/dev/null | grep -E '^(warden@.+|wardend)\.service$' | LC_ALL=C sort | tr '\n' ' ' | sed 's/ $//')
expect_eq "$WANTS holds the apps' units and wardend" "warden@$API.service warden@$SITE.service wardend.service" "$UNITS"
if [ "$MODE" = system ]; then
  # What boot does: start what multi-user.target wants (only ours, so the
  # runner's own services are left alone).
  # shellcheck disable=SC2086
  run_ok "systemctl start (multi-user.target.wants)" sudo systemctl start $UNITS
else
  run_ok "systemctl --user start default.target (as at boot)" systemctl --user start default.target
fi
wait_for "warden@$API, warden@$SITE and wardend active" 90 \
  sc is-active --quiet "warden@$API.service" "warden@$SITE.service" wardend.service
wait_for "$API answers" 30 http_ok "$PORT_API"
check "$SITE answers" http_ok "$PORT_SITE"
wait_for "warden list: $API 3/3 ready (the saved worker count)" 30 app_ready "$API" 3
check "warden list: $SITE 2/2 ready" app_ready "$SITE" 2
expect_eq "$API's status names its unit" "warden@$API.service" "$(app_field "$API" .status.unit 2>&1)"
API_PID=$(mainpid "warden@$API.service")
expect_eq "$API's supervisor is the unit's main process" "$API_PID" "$(app_field "$API" .status.pid 2>&1)"
expect_eq "$API's supervisor runs as $RUN_AS (who saved it)" "$RUN_AS" "$(ps -o user= -p "$API_PID" 2>&1 | tr -d ' ')"
wait_for "wardend sees $API running, supervised by systemd" 20 wardend_sees "$API" systemd

# ---------------------------------------------------------- crash: supervisor

note "kill -9 of a supervisor: systemd restarts it"
OLD=$(mainpid "warden@$API.service")
N0=$(sc show -p NRestarts --value "warden@$API.service")
run sudo kill -9 "$OLD"
wait_for "systemd restarted warden@$API (new main pid, active)" 30 new_main_pid "warden@$API.service" "$OLD"
N1=$(sc show -p NRestarts --value "warden@$API.service")
check "NRestarts went up ($N0 -> $N1)" test "$N1" -gt "$N0"
wait_for "$API answers again" 30 http_ok "$PORT_API"
wait_for "$API 3/3 ready again" 30 app_ready "$API" 3

# ------------------------------------------------------------ crash: wardend

note "kill -9 of wardend: systemd restarts it"
OLD=$(mainpid wardend.service)
run sudo kill -9 "$OLD"
wait_for "systemd restarted wardend" 30 new_main_pid wardend.service "$OLD"
wait_for "the new wardend answers and sees $API" 20 wardend_sees "$API" systemd
check "$API kept answering" http_ok "$PORT_API"

# ------------------------------------------------------- reload through unit

note "systemctl reload: ExecReload runs warden safe-reload"
BEFORE=$(worker_pids "$API")
run_ok "systemctl reload warden@$API" sc reload "warden@$API.service"
AFTER=$(worker_pids "$API")
check "every worker replaced by the reload ($BEFORE -> $AFTER)" disjoint "$BEFORE" "$AFTER"
check "$API answers after the reload" http_ok "$PORT_API"

# ------------------------------------------------------- reboot (user mode)

if [ "$MODE" = user ]; then
  note "the user manager stopped and started (what a reboot does to it)"
  run_ok "sudo systemctl stop user@$MY_UID.service" sudo systemctl stop "user@$MY_UID.service"
  wait_for "the apps and wardend stopped with it" 40 none_left
  run_ok "sudo systemctl start user@$MY_UID.service" sudo systemctl start "user@$MY_UID.service"
  wait_for "user manager back" 30 test -S "$XDG_RUNTIME_DIR/systemd/private"
  wait_for "warden@$API, warden@$SITE and wardend active again" 90 \
    sc is-active --quiet "warden@$API.service" "warden@$SITE.service" wardend.service
  wait_for "$API answers after the user manager restart" 30 http_ok "$PORT_API"
  check "$SITE answers after the user manager restart" http_ok "$PORT_SITE"
  wait_for "$API 3/3 ready after the user manager restart" 30 app_ready "$API" 3
fi

# ----------------------------------------------------------------- unstartup

note "warden unstartup"
run_ok "warden unstartup" w unstartup
for f in warden@.service wardend.service; do
  check_not "$UNIT_DIR/$f removed" test -e "$UNIT_DIR/$f"
done
check_not "nothing of ours left in $WANTS" bash -c "ls '$UNIT_DIR/$WANTS' 2>/dev/null | grep -E '^(warden@|wardend)'"
check_not "wardend.service stopped" sc is-active --quiet wardend.service
if [ "$MODE" = system ]; then
  check_not "/etc/sysctl.d/99-warden.conf removed" test -e /etc/sysctl.d/99-warden.conf
else
  expect_eq "lingering left on (unstartup says how to turn it off)" yes "$(loginctl show-user "$ME" -p Linger --value 2>&1)"
fi
check "$API keeps running after unstartup" http_ok "$PORT_API"
run_ok "warden kill --yes (after unstartup)" w kill --yes
wait_for "nothing left after warden kill" 40 none_left

finish

# Control and event protocol

Two kinds of Unix sockets carry every conversation with Warden. Both are
mode 0600 in a directory only the service user can write (`ensure_private_dir`),
and both speak newline-delimited JSON: one request line, then one response
line, or a stream of event lines for `subscribe` and `logs --follow`.

| Socket | Who listens | Path |
|---|---|---|
| App control socket | each app's supervisor | `<runtime dir>/<app>/control.sock` |
| Daemon socket | `wardend` (`warden daemon`), optional | `<runtime dir>/wardend.sock` |

The runtime directory is `$WARDEN_RUNTIME_DIR`, else `/run/warden` for root
(`/var/run/warden` on macOS), else `$XDG_RUNTIME_DIR/warden`, else
`/tmp/warden-<uid>`.

The wire types live in `src/control.rs` (requests, `Status`) and
`src/events.rs` (events, daemon requests). `events::PROTOCOL` is the
version; it goes up only for incompatible changes. New fields are always
optional (`#[serde(default)]`) so an older CLI or GUI keeps working.

## Design rules

- **Apps never depend on the daemon.** Every app has its own supervisor;
  the CLI talks to it directly (`warden list` in ~3 ms). `wardend` adds a
  second level of supervision and one socket for live monitoring, nothing
  on the request path. Stopping or killing `wardend` stops no app, and
  supervisors are never started with a parent-death signal tied to it.
- **Push, not poll.** A subscriber gets a snapshot, then events as they
  happen, then a status every `interval_ms` (for CPU and memory figures).
- **Slow clients lose events, never slow Warden.** Events go through a
  bounded broadcast; a client that falls behind gets `lagged` with the
  number skipped, and a client that does not read for 5 s is disconnected.
- **Old peers keep working.** `wardend` falls back to polling `status` when
  a supervisor answers `subscribe` with an error (an older version during an
  upgrade).

## App control socket

Requests are `control::Request`, tagged by `cmd`: `status`, `stop`,
`shutdown`, `restart`, `reload`, `scale`, `start`, `reset`, `signal`,
`config`, `flush`, `logs`, `log-level`, and `subscribe`. Each answers one
`control::Response` line (`{"ok":true,"message":…,"status":…}`), except
`logs` (log lines) and `subscribe` (events).

### `subscribe`

```json
{"cmd":"subscribe","interval_ms":1000,"logs":false}
```

- `interval_ms`: how often a full `status` event is sent (default 1000,
  clamped to 250–60000).
- `logs`: also send every log line (worker output and Warden's own events)
  as `log` events.

The answer is a stream of events, one JSON object per line:

1. `hello`, then a `status` snapshot, at once;
2. `worker`, `rollout`, `rollout_done` (and `log`) events as they happen;
3. `status` every `interval_ms`;
4. `bye` just before the supervisor exits on purpose (`shutdown`, SIGTERM).
   EOF without `bye` means it died.

The supervisor subscribes to its event bus before it takes the snapshot, so
nothing is lost in between (an event may also show in the snapshot). After
the request line the client sends nothing (anything it sends is ignored);
closing the connection, or only its write side, ends the subscription at
once. If the supervisor is already exiting, the answer is one
`control::Response` error line instead.

`bye.reason` is `shutdown request`, `SIGTERM` or `SIGINT`. The supervisor
waits up to 200 ms for its subscribers to take `bye` before it exits, never
longer: a client that is not reading then sees EOF without `bye`.

A subscription counts against the socket's 64-connection limit.

## Events

Every event has a `type`. Times (`at_ms`) are Unix milliseconds.

| `type` | Fields | When |
|---|---|---|
| `hello` | `protocol`, `app` (absent from `wardend`), `pid`, `version` | first line of a stream |
| `status` | `app`, `status` (`control::Status`) | snapshot, then every interval |
| `worker` | `app`, `worker`, `event`, `pid`, `detail`, `at_ms` | a worker changed state |
| `rollout` | `app`, `rollout` (`control::RolloutStatus`) | a rollout started or changed phase |
| `rollout_done` | `app`, `outcome` (`control::RolloutOutcome`) | a rollout finished or rolled back |
| `log` | `app`, `line` | with `logs: true` |
| `lagged` | `app` (if one app's stream lagged), `dropped` | this client fell behind |
| `apps` | `apps` (list of `AppEntry`) | `wardend`: snapshot, and whenever the set or a state changes |
| `supervisor` | `app`, `event`, `pid`, `detail`, `at_ms` | `wardend`: something happened to a supervisor |
| `host` | `cpu_percent`, `mem_used_bytes`, `mem_total_bytes`, `load`, `at_ms` | `wardend`: every interval |
| `bye` | `app` (absent from `wardend`), `reason` | the sender is exiting on purpose |

`worker.event`: `starting`, `ready`, `unhealthy`, `hung`, `crashed`,
`exited` (clean exit), `restarting`, `failed` (too many restarts),
`stopping`, `stopped`. `detail` carries the reason in words
(`exit code 1`, `signal SIGKILL`, `startup_ms=120`, …).

What a supervisor sends, one event per transition (next to its log line):

| `event` | When | `detail` |
|---|---|---|
| `starting` | a process was spawned | `role=replacement` for a rollout's new process |
| `ready` | it listens (or reported ready) | `startup_ms=120`, plus ` role=replacement` |
| `unhealthy` | it failed `failure_threshold` health checks in a row | `failed 3 health checks: …` |
| `hung` | no heartbeat for `watchdog.timeout` | `no heartbeat for 10s` |
| `crashed` | it exited unasked (also a rollout's new process, a Worker thread, a failed spawn) | the exit reason: `signal 9 (SIGKILL)`, `exit code 1`, `not ready in time`, … |
| `exited` | it exited with one of `restart.stop_exit_codes` | `exit code 0` |
| `restarting` | it will be started again | `in_ms=50` (backoff), or why (`FAILED; reset on request`) |
| `failed` | restarts given up | `too many restarts (…)` |
| `stopping` | Warden asked it to stop | `SIGTERM grace_s=30`, `SIGKILL` |
| `stopped` | it exited after that, or stays down (`warden stop`, scaled down) | the exit reason |

`worker` is the worker number of `status.workers`. During a rollout two
processes share a number: `pid` tells them apart. In worker mode, `worker` 0
is the host process (`starting`, `ready`, `crashed`, `stopping`, …) and
1..=count its Worker threads (`ready` when one listens, `crashed`, `hung`).

`rollout` is sent when a rollout starts, before any worker is touched
(`done` 0, `phase` `starting` or `running preflight`), and whenever its
`phase` changes, except the soak countdown; `rollout_done` follows when it
ends (`status.last_rollout` from then on). A supervisor's `lagged` names its
app.

`supervisor.event` (from `wardend` only):

| event | Meaning |
|---|---|
| `found` | a running supervisor was found (at `wardend` start or later) and is now watched |
| `started` | `wardend` started it (`start` request, or a restart below) |
| `exited` | it exited on purpose (`bye` seen) |
| `died` | it exited without `bye`: crash, OOM kill, `kill -9` |
| `restarting` | `wardend` will start it again after `detail` (backoff) |
| `gave_up` | it died too often; `wardend` stopped restarting it |
| `unresponsive` | watched, but no event and no `status` answer for a while |
| `responsive` | answering again |

## Daemon socket (`wardend`)

Requests are `events::DaemonRequest`, tagged by `cmd`; replies are one
`events::DaemonReply` line, except `subscribe` (events).

| Request | Reply |
|---|---|
| `{"cmd":"hello"}` | `hello`: protocol, pid, version |
| `{"cmd":"apps"}` | `apps`: every known app with its last status |
| `{"cmd":"subscribe","interval_ms":1000,"logs":false,"apps":[]}` | a stream: `hello`, `apps`, one `status` per running app, then every app's events (tagged with `app`), `supervisor` and `host` events. `apps` limits it to some apps (empty: all) |
| `{"cmd":"start","app":"api"}` | starts that app's supervisor (config from the config directory) under `wardend`'s watch; clears a `gave_up` |
| `{"cmd":"app","app":"api","request":{"cmd":"reload","safe":true}}` | forwards any app request and returns its `control::Response` in `response` (`logs` streams lines) |
| `{"cmd":"shutdown"}` | `wardend` exits; every app keeps running |

On `wardend`'s `subscribe` stream:

- Supervisors' event lines are forwarded as they were sent, except their
  `hello` and `bye`: `wardend` reports those as `supervisor` events (`found`,
  `exited`), so the only `hello` and `bye` on this stream are its own (no
  `app`).
- `apps` filters the events about an app; `apps` and `host` events always
  go out.
- Supervisors report a status every second: a longer `interval_ms` thins
  `status` and `host` events out for that client, a shorter one does not
  make them more frequent.
- `lagged` without `app`: this client fell behind. With `app`: `wardend`
  fell behind that supervisor's stream.
- A supervisor that refuses `subscribe` is polled: its `status` events still
  come, but no `worker`, `rollout` or `log` events and no `bye`. Its exit
  then counts as on purpose when it removed its control socket (a crashed
  supervisor leaves the socket behind).

### `AppEntry`

`name`, `namespace`, `config`, `socket`, `state` (`running`, `stopped`,
`starting`, `unreachable`, `gave_up`, `not_started`), `supervised_by`
(`systemd`, `wardend`, `terminal`), `supervisor_pid`,
`supervisor_restarts`, `status` (the last `control::Status`), `problem`
(why it is not running, with the fix).

## Second-level supervision

`wardend` watches every supervisor it finds, and restarts the ones nobody
else would:

- **How it watches.** One subscription per app (or `status` polling for an
  older supervisor), plus a pidfd for the supervisor process (`pidfd_open`,
  Linux 5.3; `kill(pid, 0)` every second otherwise), so a death is seen at
  once even if the socket is gone.
- **Who restarts what.** `Status.launched` says how the supervisor was
  started: `systemd` (systemd restarts it: `wardend` only reports),
  `background` (`warden start` or `wardend` itself: `wardend` restarts it),
  `terminal` (someone runs it in a shell: report only).
- **Backoff.** A `background` supervisor that dies without `bye` is started
  again after 1 s, then 2, 4, 8 … up to 60 s. Five minutes up resets the
  delay. After 10 deaths in 10 minutes `wardend` gives up (`gave_up`, an
  error line with the last lines of the app's log and the fix);
  `warden start <app>` clears that.
- **Hung supervisors are reported, not killed.** Its workers are probably
  still serving (they own their listeners), and killing the supervisor
  stops them (parent-death signal). `wardend` logs `unresponsive` with the
  pid and how to get a stack (`gdb -p` / `cat /proc/<pid>/stack`).
- **`wardend` itself** runs under systemd (`contrib/wardend.service`,
  `Restart=always`, `KillMode=process`; `warden startup` installs it, and
  `warden kill` then stops the unit, not just the process), under launchd
  (`warden daemon --resurrect`, restarted unless it exited cleanly) or
  detached (`warden daemon --background`; `warden start` does this when it
  launches a supervisor in the background, unless `WARDEN_NO_DAEMON=1` or a
  service manager runs `wardend`, and `warden kill` stops it after the
  apps). `--resurrect` first starts every app `warden save` recorded that
  is not running; it is never used under systemd, where each app has its
  own unit. When it starts it finds every running supervisor
  (`found`); nothing is lost across its restarts because it holds no state
  an app needs. A supervisor it restarts is started as `warden start`
  started it: the same environment and working directory (read from
  `/proc/<pid>` when `wardend` found it), never `wardend`'s own, and without
  systemd's variables (`INVOCATION_ID`, ...).

## Clients

- `warden events [app] [--json] [--logs]`: live events, from `wardend` when
  it runs, else straight from the apps' sockets.
- The GUI (separate process, planned) connects to `wardend.sock` locally,
  or through an SSH tunnel (`ssh -L`) for a remote host: there is no TCP
  listener to secure.

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

The wire types live in the `warden-protocol` crate (`protocol/`):
`control.rs` (requests, `Status`), `events.rs` (events, daemon requests) and
`paths.rs` (where the sockets are). The `warden` binary re-exports them
from `src/control.rs` and `src/events.rs`, and the GUI builds against the
same crate, so the two cannot drift apart. `events::PROTOCOL` is the
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

Streams (`subscribe`, `logs -f`) have their own budget of 32 per socket
(`control::MAX_STREAMS`) and free their request slot, so however many
clients watch, the 64 slots for commands stay available. The 33rd stream
gets an error line (`too many live streams`); `wardend` then polls `status`.

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
(`exit code 1`, `killed by another process (SIGKILL)`, `startup_ms=120`, …).

What a supervisor sends, one event per transition (next to its log line):

| `event` | When | `detail` |
|---|---|---|
| `starting` | a process was spawned | `role=replacement` for a rollout's new process |
| `ready` | it listens (or reported ready) | `startup_ms=120`, plus ` role=replacement` |
| `unhealthy` | it failed `failure_threshold` health checks in a row | `failed 3 health checks: …` |
| `hung` | no heartbeat for `watchdog.timeout` | `no heartbeat for 10s` |
| `crashed` | it exited unasked (also a rollout's new process, a Worker thread, a failed spawn) | the exit reason: `exit code 1`, `killed by the kernel OOM killer (out of memory)`, `killed by another process (SIGKILL)`, `crashed: SIGSEGV (segmentation fault)`, `not ready in time`, … |
| `exited` | it exited with one of `restart.stop_exit_codes` | `exit code 0` |
| `restarting` | it will be started again | `in_ms=50` (backoff), or why (`FAILED; reset on request`) |
| `failed` | restarts given up | `too many restarts (…)` |
| `stopping` | Warden asked it to stop | `SIGTERM grace_s=30`, `SIGKILL` |
| `stopped` | it exited after that, or stays down (`warden stop`, scaled down) | the exit reason (`exit code 0 after Warden's SIGTERM`, `killed by Warden (SIGKILL)`) |

`worker` is the worker number of `status.workers`. During a rollout two
processes share a number: `pid` tells them apart. In worker mode, `worker` 0
is the host process (`starting`, `ready`, `crashed`, `stopping`, …) and
1..=count its Worker threads (`ready` when one listens, `crashed`, `hung`).

Hot standbys (process mode, `[workers] standby`) are listed in
`status.standbys` (`WorkerStatus` rows; the field is absent without
standbys, and older clients ignore it), never in `status.workers`: `id` is
the standby's number (1..=standby; `sN` in `warden status` and as `worker=sN`
on its log lines; a new standby takes the lowest number no live one has, so
one being replaced and its successor may share it for a moment: `pid`
tells them apart), `state` `WARMING` (starting,
or passing its health gates) then `STANDBY` (can take over), or `STOPPING`;
a missing one shows as `RESTARTING` (backoff), `FAILED` or `STOPPED`.
`restarts`, `crashes` and `last_exit` are the pool's. Their events are
`worker` 0 (process-mode workers are numbered from 1): `starting` (detail
`role=standby`), `ready` (`startup_ms=… role=standby`), `unhealthy`,
`hung`, `crashed` (`… (standby)`), `restarting`, `failed`, `stopping`,
`stopped`. A promotion is a story of the slot it fills: `crashed` (the dead
worker), `restarting`, then `starting` and `ready` with the standby's
`pid` and detail `promoted from standby` (`promote_ms=…` on `ready`).
Crash details keep the exit reason first (`killed by the kernel OOM killer
(out of memory) (standby)`), as alerts match it. These are additions:
clients that don't know them show a worker 0.

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
| `{"cmd":"history","app":"api","since_ms":1790000000000,"step_s":60}` | `history`: the resource history ([below](#resource-history)). Every field is optional |
| `{"cmd":"reload"}` | reads `wardend.toml` (the [alert rules](#alerts)) again, as SIGHUP does. `ok: false` with every problem in `message` when the file has errors; the rules in force then stay |

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

### Resource history

`wardend` keeps the last 24 hours of every app's and the host's resource
use in memory, from what it already receives: each app's `status` (every
second) and the host metrics (every second while a client subscribes,
else every 10 s). Nothing is polled for it. Every 10 s of wall-clock time
one sample per series is committed, covering `[t, t + 10 s)` with `t` a
multiple of 10 s. It is saved to disk every minute and read back when
`wardend` starts ([below](#on-disk)), so `wardend`'s own restarts keep it:

| Series | One sample (10 s) | A point of `step_s` |
|---|---|---|
| app `cpu_percent` | mean of the statuses' CPU (workers + worker-mode host; 100 = one core) | mean |
| app `rss_bytes` | max of the statuses' resident memory (workers + worker-mode host + supervisor) | max |
| app `workers_ready` | min | min |
| app `workers_configured` | max | max |
| app `restarts` | restarts seen: what the workers' (and worker-mode host's) `restarts` counters grew by between statuses of one supervisor (all of a new supervisor's), plus `wardend`'s restarts of the supervisor | sum |
| host `cpu_percent` | mean (100 = all CPUs) | mean |
| host `mem_used_bytes` | max | max |
| host `load1` | mean of the 1-minute load | mean |

The request: `{"cmd":"history","app":"api","since_ms":…,"step_s":60}`.

- `app`: that app (and the host); `""`: the host only; absent: every app.
  An app without a series (unknown, or no sample yet) is not an error: its
  entry is missing from `apps`.
- `since_ms`: from then (default, and at most, 24 h ago). The first point
  starts at `since_ms` rounded down to a multiple of `step_s`, the last one
  holds now (it is not committed yet, so it is `null` until the next 10 s).
- `step_s`: seconds per point (default 10), rounded up to a multiple of 10,
  at most 86400. A reply holds at most 1,000,000 numbers: with many apps at
  a small step, wardend doubles the step until it fits. The reply says the
  step it used.

The reply, in `history`:

```json
{"ok":true,"history":{"start_ms":1790000000000,"step_s":60,"points":1440,
  "host":{"cpu_percent":[12.5,null,…],"mem_used_bytes":[…],"mem_total_bytes":8220000000,"load1":[…]},
  "apps":[{"app":"api","cpu_percent":[…],"rss_bytes":[…],"workers_ready":[…],
           "workers_configured":[…],"restarts":[…]}]}}
```

Point `i` covers `[start_ms + i * step_s * 1000, + step_s)`; every array
has `points` entries; `null` where nothing was sampled (the app was not
watched, `wardend` was not running). CPU is rounded to 0.1, load to 0.01.

- **Memory bound.** A fixed ring of 8640 samples (24 h) per series, in
  16-byte records (f32 CPU, u32 KiB, u16 counts): at most 135 KiB per app,
  plus 169 KiB for the host and the sample times, for at most 128 apps.
  Rings grow an hour at a time, so a young `wardend` holds less. 10 apps:
  1.5 MB at most. A unit test checks the bound.
- **Lifetime.** An app's series is kept by name: it survives the app's
  restarts, `wardend` restarting its supervisor and `wardend`'s own
  restarts ([on disk](#on-disk)), and goes once it has
  had no sample for 24 h (or, past 128 apps, when a newer app needs room
  and it has had none for 10 minutes).
- **Gaps.** While `wardend` is not running nothing is sampled: those points
  are `null`, and the GUI shows them as gaps. Restarts while it was down are
  not counted (the first status after a start is the baseline).

#### On disk

`<state dir>/wardend-history.bin` (`/var/lib/warden` for root,
`$XDG_STATE_HOME/warden` or `~/.local/state/warden` for a user,
`$WARDEN_HOME/state`), mode 0600:

- **When.** Every minute if a sample was committed since the last write,
  and when `wardend` exits on purpose (SIGTERM, SIGINT, `shutdown`; at most
  5 s, then it exits anyway). A crash or `kill -9` loses at most the last
  minute; the 10 s being sampled at an exit are never saved.
- **Atomic.** Written to `.wardend-history.bin.tmp` (created new, 0600),
  fsynced, renamed over the file, and the directory fsynced: a crash
  leaves the previous snapshot or the new one, never a mix. The encoding
  runs on `wardend`'s thread (straight into the file through a 64 KiB
  buffer: about 3 ms for 10 apps with a full day, 1.1 MB, in a release
  build); the fsync and the rename run on a thread of their own.
- **Format.** Binary, little-endian, versioned: a header (magic
  `WDHIST\r\n`, format version 1, the 10 s step, the save time, counts),
  the sample times as runs, then the host's and each app's series as the
  16-byte records above, where a run of absent samples (an app not
  watched) takes 8 bytes, and a CRC-32 of everything. It is never bigger
  than 17,936,432 bytes (128 apps with a full day): `wardend` refuses a
  bigger file before reading it. The layout is in `src/daemon/history/disk.rs`.
- **Loading.** Samples older than 24 h are dropped (`wardend` was down that
  long), and so are samples from the future (the clock went back: a
  warning says so), then apps with none left. A file that is truncated,
  corrupt (bad checksum or structure), of another format version, too big,
  not a history file or not a regular file is not used: `wardend` logs a
  warning with the reason and the fix, moves it to
  `wardend-history.bin.bad` (replacing an older one) and starts with an
  empty history. A file it cannot read (permissions, an I/O error) is
  reported and left in place; the next save replaces it.

## Alerts

`wardend` sends alerts by the rules in `<config dir>/wardend.toml`
(`/etc/warden/wardend.toml` for root, `~/.config/warden/wardend.toml` for a
user, `$WARDEN_HOME/wardend.toml`). The file is optional; without it no
alert is sent. It is read at start, on SIGHUP and on the `reload` request
(`warden daemon reload`), and checked with `warden daemon check [-c FILE]`.
A file with errors is reported (every problem, with its line) and the rules
in force stay. App discovery ignores this file (it is not an app).

```toml
# Optional: what makes a crash loop (these are the defaults).
[crash_loop]
crashes = 3        # worker crashes of one app …
window = "5m"      # … within this long

[[alert]]
name = "ops"                                        # optional, in logs and alerts (default: "alert #N")
on = ["crash_loop", "gave_up", "rollout_failed", "unresponsive", "worker_failed", "oom"]   # or ["all"]
apps = ["api"]                                      # optional; default every app
command = ["/usr/local/bin/notify", "--channel", "ops"]   # the alert as JSON on stdin, and WARDEN_ALERT_*
min_interval = "5m"                                 # the default

[[alert]]
on = ["all"]
webhook = "https://hooks.slack.com/services/…"      # POSTed as JSON (with `text`, for Slack)
```

Durations: `500ms`, `30s`, `5m`, `1h30m`, `1d`, or a number of seconds.

### Kinds

| `kind` | When |
|---|---|
| `crash_loop` | `crashes` worker `crashed` events of one app within `window`. Once per loop: crashes meanwhile are counted (in `recovered`); the loop ends after `window` without a crash |
| `oom` | a worker `crashed` event whose `detail` holds the OOM marker (see below); it counts as a crash too |
| `worker_failed` | a worker `failed` event (too many restarts), or a worker already `FAILED` in the status `wardend` sees when it begins watching an app |
| `unhealthy` | a worker `unhealthy` or `hung` event |
| `rollout_failed` | `rollout_done` with `ok: false` |
| `recycled` | `rollout_done` of a `replace` whose message names `max_memory` or `max_lifetime` |
| `died` | `supervisor` `died` (no `bye`) |
| `gave_up` | `supervisor` `gave_up` |
| `unresponsive` | `supervisor` `unresponsive` |
| `recovered` | after any of the above but `rollout_failed` and `recycled`: every worker ready (and no rollout) for 1 minute |

**The OOM marker.** A worker the kernel's OOM killer killed (it died of
SIGKILL and its cgroup's `oom_kill` count rose) has the exit reason
`killed by the kernel OOM killer (out of memory)`: the `crashed` event's
`detail` and `WorkerStatus.last_exit` start with it, maybe followed by more
(`… (standby)`, `… (replacement, before taking over)`). `wardend` matches
exactly that text, anywhere in the detail; it is
`warden_protocol::events::OOM_KILLED`, shared by the supervisor and
`wardend`. A SIGKILL from anything else is `killed by another process
(SIGKILL)`; where Warden can't read the cgroup's OOM counter, an OOM kill
looks like that too, and no `oom` alert is sent (the log line says so).

### What a rule gets

```json
{"text":"web-1: api crash loop: 3 worker crashes within 5m; the last: worker 2 exit code 1",
 "kind":"crash_loop","app":"api","host":"web-1",
 "detail":"3 worker crashes within 5m; the last: worker 2 exit code 1",
 "at_ms":1790000000000,"count":1,"first_ms":1790000000000,"rule":"ops"}
```

- `command`: run with that JSON on stdin and `WARDEN_ALERT_KIND`, `_APP`,
  `_HOST`, `_TEXT`, `_DETAIL`, `_COUNT`, `_AT_MS`, `_RULE` in its
  environment (and `wardend`'s). It runs as `wardend`'s user (root for the
  system `wardend`), in a process group of its own. Its exit status alone
  decides: 0 is sent, anything else a failure (with the first line of its
  stderr, read for at most 200 ms after it exits). A process it leaves in
  the background is its own business and is not waited for; if it writes
  to stderr later, redirect that (`… 2>/dev/null &`), as wardend no longer
  reads it. The program is checked when the file is read: an absolute
  path, or a name on `wardend`'s PATH.
- `webhook`: `curl -fsS --max-time 10 --proto =http,https -K -`, which reads
  the URL, the `Content-Type: application/json` header and the body from
  its stdin, so neither the URL (often a secret) nor the alert ever appear
  in a process list. Warden carries no TLS stack of its own. An HTTP error
  status is a failure. Without curl, `wardend` logs one error with the fix.
- `min_interval` (per rule, app and kind): an alert goes out at once; the
  same alert within `min_interval` is held back and counted; when the
  interval has passed, the held ones go out as one alert with `count` (and
  `first_ms`, the first of them). `"0s"` sends every one.

### Delivery

Alerts never make `wardend` wait. They go into a queue of 64 (more are
dropped, counted and logged once a minute); at most 4 deliveries run at
once; each still running after 10 s is killed (the command's whole
process group); a
failed one is tried once more after 5 s, then logged as lost. Every failure
is logged with the rule, the kind, the app, the target (a command's program
only, a webhook's host only: never its path or token) and the fix.

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
  an app needs (and its resource history is [on disk](#on-disk)). A supervisor it restarts is started as `warden start`
  started it: the same environment and working directory (read from
  `/proc/<pid>` when `wardend` found it), never `wardend`'s own, and without
  systemd's variables (`INVOCATION_ID`, ...).

## Clients

- `warden events [app] [--json] [--logs]`: live events, from `wardend` when
  it runs, else straight from the apps' sockets.
- `warden-gui` ([`gui/`](../gui/README.md)), a separate process, connects to
  `wardend.sock` locally, or through an SSH tunnel (`ssh -N -L <local
  socket>:<remote wardend.sock> user@host`) for a remote host: there is no
  TCP listener to secure. It keeps one `subscribe` stream open (reconnecting
  with backoff), opens a second one with `logs: true` and `apps: [<app>]`
  only while its logs pane shows that app, and sends every action as a short
  `app` or `start` request. Its History tab asks `history` for the shown app
  and range (1 h at 10 s, 6 h at 1 min, 24 h at 4 min per point) when it
  opens, and its header for the host's last hour; then it adds the live
  `status` and `host` events to those series, the way `wardend` counts them
  (`events::Usage`, `events::restarts_since`).
- `warden daemon check` and `warden daemon reload`: the [alert rules](#alerts).

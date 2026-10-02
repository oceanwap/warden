# Troubleshooting

Every problem Warden reports is one log line with what happened, why, what
Warden did about it and a `hint=` with the fix. This page collects them by
symptom. Start with:

```sh
warden doctor               # the environment problems we know about, each with a fix
warden list                 # every app and worker; anything not online says why
warden describe <app>       # config, paths, restart policy, last exits, last rollout
warden logs <app> --events  # Warden's own decisions for that app
warden logs <app> --history --grep error --since 2h   # the log files, rotated and .gz included
```

## An app does not start

| You see | Why | Fix |
|---|---|---|
| `warden start` says `failed to start: all N workers crashed before one was ready (exit code 1)` and exits 1 | Every worker crashed twice (or was killed as `not ready in time`) before any was ready (one crash can be a transient the restart policy gets past, so it is not enough): a syntax error, a missing module or env variable, a port in use, a wrong command. The app's last error output is printed under it | Fix what it shows (`warden logs <app> --err` has the rest), then `warden start <app>`. The workers were stopped, so nothing restarts meanwhile; the app stays listed as `errored`. `warden resurrect` reports the same but leaves the restart policy at work (at boot a dependency may still be coming up) |
| `errored` in `warden list` (`State: errored: its last start failed`) | `warden start` stopped the workers after a start that failed (above); `start_failed` in `status --json` has the last crash's reason | `warden start <app>` tries again |
| `app cannot start: every worker crashed before it was ready` (Warden's log, once per start; `crashes_each=2`) | The same, seen by the supervisor, whoever started it (systemd, `resurrect`, `warden start`) | Its `reason=` and the app's output (`warden logs <app> --err`) say why. Warden goes on restarting with backoff (then FAILED, retried after `failed_cooldown`) unless `warden start` stopped it; `warden stop <app>` stops trying |
| `no worker ready after N s, and not every one has crashed yet` (`warden start`, exit 1) | No worker listened within `ready_timeout` + 15 s, but they have not all crashed either (a slow boot, a worker restarting) | The app keeps starting: `warden list` shows its progress. Raise `[workers] ready_timeout` for slow boots |
| `worker not ready in time; killing` | The app didn't listen on `PORT` within `workers.ready_timeout` (30 s), or, with `wait_ready`, never called `process.send('ready')` | Check that the app listens on `process.env.PORT`; raise `ready_timeout` for slow boots; `warden logs <app>` shows the app's own errors |
| `failed to start worker` | The command could not be executed | The error names the path: fix `command`/`args`/`working_directory`; `warden check -c <file>` validates the config first |
| `worker failed: too many restarts` | It crashed `max_restarts` times within `restart_window` | Fix the crash (the last exits are in `warden describe`); it is retried after `failed_cooldown`, or now with `warden reset <app>` |
| `EADDRINUSE` in the app's output | Workers can't share the port | Bun: the shim adds `reusePort` (don't set `shim = false`). Node: needs 22.12 or newer; older Node: `port_strategy = "offset"`. Another program on the port: `ss -ltnp 'sport = :3000'` |
| `Node cannot share a port on this OS: only one worker can listen on it` (macOS) | Node's `reusePort` exists only where the kernel spreads connections (Linux, FreeBSD); on macOS a second worker, or a reload's replacement next to the old worker, gets `EADDRINUSE` | `[workers] count = 1` and `warden restart --hard` to restart it, or `workers.port_strategy = "offset"`; Bun apps share the port on macOS too |
| The app is `stopped` after a clean exit | Its exit code is in `restart.stop_exit_codes` | Intended for one-shot jobs; `warden restart <app>` starts it again |
| An app without a port shows `starting` for a second | Port-less apps count as ready after `workers.min_uptime` (1000 ms) | Intended (PM2's `min_uptime`); lower it if you like |

## Restarts, reloads and deploys

| You see | Why | Fix |
|---|---|---|
| `safe-reload failed at worker 1 … Rolled back` | The canary failed a gate (health, `verify_command`, soak) | Nothing to undo: every worker still runs the previous version. The message says which gate; test the new release, then deploy again |
| `dependency outage suspected: holding worker replacements` | More than `health.outage_threshold` of the workers fail at once: probably a database or API is down, not the workers | Fix the dependency; Warden resumes replacements by itself. Use a `live_path` that checks the process only, and a `ready_path` for dependencies |
| `net.ipv4.tcp_migrate_req is 0` | Connections waiting in a closing worker's accept queue get reset during restarts | `sysctl -w net.ipv4.tcp_migrate_req=1` and `contrib/99-warden.conf` (what `warden startup` installs) |
| `worker did not exit within grace period; sending SIGKILL` | The app ignored the stop signal for `shutdown.grace_period` | Apps written for PM2 often listen for SIGINT: `shutdown.signal = "SIGINT"`. Otherwise close servers and DB pools on SIGTERM |
| `closed long-lived connections so their clients reconnect to new workers … websockets=N sse=M` | A worker being replaced still had WebSockets or SSE streams `shutdown.long_lived_timeout` (2 s) after its drain began: they got close 1001 / a clean end of stream | Nothing to fix; clients must reconnect (EventSource does by itself; WebSocket clients: reconnect on close, with a little jitter). Raise `long_lived_timeout` to let streams that do end (an answer streamed as SSE) finish first |
| A reload takes about 2 s more than without long-lived clients | Old workers with WebSocket or SSE clients wait `long_lived_timeout` for them to end by themselves before closing them. Up to `[reload] max_draining` (4) of them drain at once while the next workers are replaced, and the command returns once the last one has exited | Lower `[shutdown] long_lived_timeout` (fractions work: `0.5`); `0` leaves them open until `grace_period` |
| A reload takes about 2 s more *per worker* with long-lived clients | `[reload] max_draining = 1`, or more workers than `max_draining`: the next replacement waits until an old worker has finished draining (`waiting for old workers to finish draining before replacing the next ones`) | Raise `max_draining` if the memory allows: each old worker holds its memory until it exits, so a rollout runs at most max(`surge`, `max_draining`) extra processes |
| `old worker did not exit after its SIGKILL; the rollout stops waiting for it` | An old worker was still there 10 s after `grace_period` (when it got SIGKILL): the kernel holds it, in uninterruptible I/O or on a hung network filesystem | `cat /proc/<pid>/stack` shows where. Its replacement already serves; the rollout ends without it, and Warden reaps it (and `warden status` stops listing it as `DRAINING`) when it ends |
| `warden status` shows `2 (old)  DRAINING` rows; the rollout says `every worker replaced; 2 old workers draining` | Old workers a rollout replaced are closing their connections (long-lived ones after `long_lived_timeout`, requests in flight within `grace_period`); their replacements already serve | Nothing to do: they exit by themselves, or are SIGKILLed at `grace_period` (`worker did not exit within grace period`). The rollout, and the CLI waiting for it, end once they have exited |
| `shutdown.long_lived_timeout = X must be below shutdown.grace_period` (at start or `warden check`) | The worker needs time after closing them to finish and exit, or it is SIGKILLed and its clients see resets | Lower `long_lived_timeout` or raise `grace_period` |
| An SSE stream still holds a draining worker until `grace_period` | The shim didn't see it as SSE: a Bun response returned straight from `fetch()` (a proxied stream), a content type other than `text/event-stream`, an HTTPS `node:http` server | Bun: rebuild it, `const r = await fetch(url); return new Response(r.body, r)`. Other long streams (downloads, NDJSON) are left to finish on purpose: ending them would make a cut-off body look complete |
| `aborted: workers are being stopped` (the CLI of a reload or restart exits 1) | A `stop`, `restart --hard` or shutdown came in while the rollout ran, and took over | Run the rollout again once the workers are back, if it is still needed |
| `reload failed at worker 2 … Rolled back: the 2 new workers started together were stopped` | With `[reload] surge`, one worker of a batch failed a gate | Nothing to undo: the batch's new workers were stopped and the old ones keep serving. The message names the worker and the gate |
| `reload.surge = N starts new workers next to the ones they replace, but …` (at start or `warden check`) | Surge needs a worker and its replacement to run at once; with `port_strategy = "offset"` each worker owns its port, and apps without the shim can't share one | Remove `surge` (one worker at a time), or let the workers share the port (`port_strategy = "shared"`) |
| `reload.max_draining = N must be between 1 and 1024` (at start or `warden check`) | It counts old workers allowed to drain at once; 0 would never let a rollout start its next worker | `1` to replace one worker at a time and wait for each drain, more to let drains overlap (default 4) |
| Workers still run the old release after a deploy, or a restarted worker did | `[app] pin_release` (default): workers start in the release `current` pointed to at the last start / reload / safe-reload / restart. A crash restart stays on it on purpose, so versions don't mix | Reload after swapping the symlink: `warden safe-reload` (or `reload`). `warden status` shows the pinned release |
| `the pinned release directory is gone; starting the worker in the current release instead` | The pinned release was deleted (old releases cleaned up) before a reload moved the workers off it | Delete old releases only after the reload; then run `warden reload` so every worker runs the same release again |
| `cannot pin the release` (at start, `warden start` after a stop, `restart --hard`) | `[app] pin_release` (the default) resolves `working_directory` through its `current` symlink when workers start, and it doesn't resolve: missing, a dangling symlink, or not a directory (`error=` says which) | Fix `working_directory` or the `current` symlink. Until it resolves each worker's start fails (backoff, then FAILED); `warden reset <app>` once fixed. A reload with this problem stops before touching a worker |
| `the pinned release directory is gone, and working_directory can't be resolved either` | The pinned release was deleted, and `current` (or `working_directory`) now points nowhere, so there is no release to start a worker in | Restore the release or fix the `current` symlink; until then starting a worker fails (backoff, then FAILED: `warden reset <app>` once fixed) |
| `worker started by a rollout that failed has stopped; restarting it with the previous config` | That worker had already taken its slot when the rollout failed (its old process was gone, so there was nothing to roll back to); it was stopped with the rollout | Nothing to undo: Warden starts the slot again at once on the previous config, without counting a crash. Fix what the rollout's failure (the line before) says and deploy again |
| `config changes that need systemctl restart were not applied` | `reload` re-reads the config, but some keys only apply at start: `[workers]` (use `warden scale` for the count; `standby` too), `[metrics]`, `[control]`, `health.url/enabled/interval`, `logging.timestamps/worker_output` | The line lists them; `systemctl restart warden@<app>` (a full restart) applies them |

## Hot standbys (`[workers] standby`)

Standbys are `s1`, `s2`… in `warden status`, `worker=s1` on their log
lines and output, and `worker s1` in `warden events` and the GUI (the
pool's own events: `worker standby`): `warden logs <app> --worker s1`
follows one, `--worker standby` all of them with the pool's own lines
(`worker=standby`).

| You see | Why | Fix |
|---|---|---|
| `workers.standby … is for process mode` / `needs app.port` / `needs Warden's shim` / `needs workers.port_strategy = "shared"` / `does not work with … "direct"` | Config validation: a standby defers the app's listen on its port through the shim, and joins the shared port when promoted | Do what the message says, or remove `standby` |
| `standby not initialized in time; killing` | The standby never called `Bun.serve`/`listen` on `app.port` within `workers.ready_timeout`: that call is what makes it ready (it is held back, not made) | The app's errors are in `warden logs <app> --worker standby`; raise `ready_timeout` for slow boots |
| `standby crashed` … `standby restarting` … `standbys failed: too many standby crashes` | Standbys kept exiting before promotion (backoff like a worker, then FAILED until `failed_cooldown`) | `warden logs <app> --worker standby`; code that exits when idle, or that needs the port, fails here. `warden reset <app>` retries. Crashed workers restart the normal way meanwhile |
| `standby keeps failing its health checks before promotion` / `standby unhealthy; replacing it` / `standby failed reload.verify_command` | A standby must pass the rollout gates on its private socket before it can be promoted, and stay healthy while idle | The health path must answer once the app is initialized, before it listens. A `verify_command` that tests `$PORT` reaches the workers, not the standby: use `$WARDEN_WORKER_SOCKET` |
| `standby health check failed; it is not promoted while failing` | An idle standby (through its gates) failed a periodic liveness check on its private socket (`error=` has the status or error) | Nothing yet: it is skipped for promotion while failing, and replaced after `[health] failure_threshold` failures in a row (`standby unhealthy; replacing it`). `warden logs <app> --worker s1` shows its output |
| `failed to start a standby` | Its command could not be executed (`error=`: a missing file or release directory, a process or fd limit); standbys run the workers' command | Fix what `error=` names (`warden check -c <file>` for the config); standbys are retried with backoff, then FAILED (`warden reset <app>`). Crashed workers restart the normal way meanwhile |
| `could not reach a standby to promote it; starting a new worker instead` | The promote message on fd 3 failed (`error=`): the standby's end of its IPC socket is gone, e.g. it was exiting at that moment | Nothing to do: the slot gets a normal cold start and the standby is replaced (counted as a standby crash). If it repeats, report it with `warden logs <app> --worker standby` |
| `standbys disabled: a standby took the app's port before being promoted` | The app listens in a way the shim doesn't hold back (a raw TCP server on the port, a native HTTP server), so a standby would take traffic | Set `[workers] standby = 0`; the standbys were stopped and workers restart the normal way |
| `promoted standby did not listen in time; killing` | A promoted standby didn't report listening within 5 s (its listen failed, or it is stuck) | The slot restarts the normal way; `warden logs <app> --worker <n>`. If it repeats, set `standby = 0` and report it |
| A crash during `reload`/`safe-reload` restarts cold | Standbys run the previous version, so they are not promoted while a deploy runs; they are replaced when it succeeds and kept when it rolls back | Intended |
| `replacing standbys: a new worker takes their instance number` (after `warden scale`) | Each standby has its own instance number past the workers' (`instance_var`); a scale-up gives a new worker that number, and two processes would run as the same instance | Intended: the standby is replaced by one past the new count once the new workers are up (a crash meanwhile restarts cold) |
| A cron that runs on `NODE_APP_INSTANCE == 0` stops after worker 1 crashed | The standby started with a number past the workers' (so it doesn't run the job twice) and decided at startup | After promotion `process.env.NODE_APP_INSTANCE` is the slot's; start such jobs in `process.on("warden:promote", …)` as well |

## Why a worker died

`warden status` shows each worker's `last_exit`, and the log line of the death
carries the same reason with a `hint=`.

| You see | Why | Fix |
|---|---|---|
| `killed by the kernel OOM killer (out of memory)` | The worker's cgroup (Warden's: `MemoryMax=` of the unit, a container limit; or one of its own) ran out of memory and the kernel killed the worker; Warden saw the cgroup's `oom_kill` count go up in the 2 s before the worker died of SIGKILL, by a kill no other dying process could own (a rise seen earlier was something else in the cgroup, and a SIGKILL Warden sent is always `killed by Warden`) | Raise `memory.max` / `MemoryMax=`, or set `[limits] max_memory` below it so Warden replaces a growing worker gracefully before the kernel kills it |
| `probably killed by the kernel OOM killer (out of memory)` | As above, but other processes of Warden's in the same cgroup died of SIGKILL at the same moment, more than the kills counted: the count is per cgroup, not per process, so the kill may have been one of theirs | `journalctl -k \| grep -i 'killed process'` names the pid the kernel killed; if it was this one, as above. wardend's `oom` alert says `probably` too. When two processes die together on one kill, which of the two reads `probably` and which `killed by another process or…` is arbitrary: only the kernel log tells them apart |
| `killed by another process or the kernel OOM killer (SIGKILL)` | It died of SIGKILL next to another process of the same cgroup that took the cgroup's only OOM kill (that one reads `probably killed by the kernel OOM killer`): one of the two was the kernel's victim, the other killed by something else | `journalctl -k \| grep -i 'killed process'` says which; then see the rows above and below |
| `exit code 137` for an app run through a shell wrapper (`sh -c 'bun app.ts'`) | The shell reports its child's SIGKILL as 128 + 9. When the cgroup counted an OOM kill just before, it reads like the rows above; otherwise it stays an exit code | Run the app directly (`command = "bun"`, or `exec bun …` in the shell), so Warden sees the signal itself |
| `killed by another process (SIGKILL)` | A SIGKILL Warden didn't send: `kill -9` by a person or script, a container runtime. With the cgroup counter readable and no rise in the 2 s before, the OOM killer is unlikely | Find who sends it (`journalctl`, the audit log); `journalctl -k \| grep -i oom` settles whether the kernel killed it (the hint says when there is no readable counter at all, e.g. a worker in a cgroup of its own whose `memory.events` Warden can't read) |
| `killed by another process (SIGTERM)` | Something else asked the worker to stop, often systemd stopping the unit with `KillMode=control-group` | Use `KillMode=mixed` (what `warden startup` writes), so only Warden gets the stop signal and drains its workers |
| `crashed: SIGSEGV (segmentation fault)`, `SIGBUS`, `SIGILL` | A native crash in the runtime or a native module | The worker's last output (`warden logs <app>`) and core dumps (`coredumpctl list`) say where; try another Bun/Node version |
| `crashed: SIGABRT (aborted)` | The program aborted itself: `process.abort()`, a failed native assertion, a fatal runtime error | Its last output lines say why |
| `killed by Warden (SIGKILL)` | Warden killed it: it didn't exit within `shutdown.grace_period`, was hung (watchdog), or not ready in time | The line before it says which; see those entries |
| `exit code 0 after Warden's SIGTERM` | A normal stop: the worker exited after Warden's stop signal | Nothing to do |
| `worker crashed … reason="exit code N"` | The app exited by itself | Its last output lines say why (`warden logs <app> --worker N`); it is restarted with backoff, then FAILED after `max_restarts` |
| `worker thread error` then `worker thread crashed` (worker mode) | An uncaught error (in `message=`) or `process.exit()` in one Worker | The host keeps serving with its other Workers while Warden starts a replacement host; fix the error |

## Health

| You see | Why | Fix |
|---|---|---|
| `worker unhealthy` / `worker health check failed` | The worker's health path failed `failure_threshold` times in a row on its private socket | The line has the HTTP status or error; the worker is replaced gracefully (`on_failure = "replace"`) |
| `only 0 of 4 private health socket(s) reported` | A health path is set, but the app didn't listen through the shim | Keep `shim` on (Bun and Node), or use `health.url` for an app-level check |
| `worker hung: no heartbeat from its event loop` | The event loop was blocked for `watchdog.timeout` (60 s): an infinite loop, a sync call that never returns | The worker is killed and restarted; profile the app |
| `worker event loop delay is high` (`p99_ms=`, `for_s=`) | For 10 s in a row the worker's event loop ran timers at least `[watchdog] loop_delay_warn` (0.5 s) late at p99: requests wait that long before their handler starts. Synchronous work on the loop (large `JSON.parse`/`stringify`, sync fs or crypto, a CPU-heavy route) or a host short of CPU (`warden top`, the load). Logged at most every 10 min per worker | Profile the worker (`node --cpu-prof`, `bun --inspect`), move heavy work to a Worker thread or a queue, add workers if the host has idle CPU. `warden list` (Loop p99) and `warden_worker_event_loop_delay_p99_seconds` show it live; raise `loop_delay_warn` (or 0 = off) if this is expected |
| `Loop p99` shows `-` | No figure in the last 5 s: an app without Warden's shim (not bun or node, or `shim = false`), a worker still starting, or one whose heartbeats stopped (blocked: the watchdog handles it) | Nothing, unless it stays `-` for a running bun/node worker: then `warden logs <app> --worker N` |
| `event loop was blocked` (Warden's own line, `for_ms=`) | Warden itself did not run for that long: it was stopped (SIGSTOP, a debugger), the VM was paused, the host was starved, or its stdout blocked | Workers kept serving, and the watchdog does not count that time against them (a freeze used to get every worker killed as hung). If it repeats, check the host's load and what reads Warden's output |

## Logs

| You see | Why | Fix |
|---|---|---|
| `worker writes too much output; lines dropped` | A worker wrote more than `logging.max_lines_per_sec` (10,000) lines/s | Lower the app's log level; raise the budget; `max_lines_per_sec = 0` keeps every line (a flooding app is slowed down instead). Or bypass the budget: `worker_output = "inherit"` (straight to Warden's stdout, not in `warden logs`) or `"direct"` (into `out_file`), both in `warden.example.toml` |
| `log lines dropped because stdout could not keep up` | Warden's stdout (journald, a pipe) is slower than the apps' output | Check journald (`journalctl --disk-usage`, rate limits: `RateLimitBurst=`); write to files instead (`[logging] file`/`out_file`) |
| `cannot write the log file` | The directory doesn't exist or isn't writable, or the disk is full | Fix the path or permissions; lines still go to stdout and memory |
| `warden logs --history` finds nothing | No log files are configured and journald isn't in use | Set `[logging] out_file`/`err_file`/`file`, or run under systemd |
| `--history` lines start with `worker=2 stdout:` | They come from per-worker files (`out-2.log`), read next to other files; a file holds only what the app wrote, so its worker and stream are added in front (and its time, with `file_timestamps`). One file alone (`--out --worker 2`) prints as written | Intended. Order: Warden's own log first, then each worker's files, oldest first, workers in order (numbers, standbys, host). `--lines N` is the last N of each file |
| `out.log is shared by every worker and its lines don't say whose they are, so --worker N can't pick them out` | One shared out/err file (no `per_worker_files`) and no `[logging] file` to take worker lines from | Set `per_worker_files = true` (new lines only), or `[logging] file` in capture mode (every line names its worker); drop `--worker` to read the whole file |
| `stderr goes into out_file together with stdout (no err_file is set)` | Direct mode without `err_file` writes stderr into `out_file` (like `2>&1`), so `--err` can't separate it | Read both (drop `--err`), or set `[logging] err_file` (from the next restart) |
| `worker_output = "direct" with workers.count = N needs logging.per_worker_files = true` (at start or `warden check`) | Direct mode writes each worker's bytes unparsed; in one shared file a partial line of one worker could meet another's | Add `per_worker_files = true` under `[logging]` (files `out-1.log`, `out-2.log`…), or use `worker_output = "capture"` |
| `worker_output = "direct" writes worker output straight to files: set logging.out_file` / `needs logging.out_file for stdout` | Direct mode has nowhere else to put output | Set `out_file`; add `err_file` for a separate stderr file (without it stderr goes into `out_file`, like `2>&1`) |
| `logging.file_timestamps needs worker_output = "capture"` | Direct mode writes the app's bytes unchanged | Let the app's logger add timestamps, or use `"capture"` |
| `cannot splice into the worker output file; copying through Warden instead` | The file's filesystem has no splice support (some FUSE or network filesystems), or a seccomp profile blocks `splice` | Nothing is lost, it is only slower; put the files on ext4, xfs or tmpfs, or allow `splice` |
| `cannot open the worker output file; the worker's output is discarded until it can` | Direct mode: the directory is missing and can't be created, or isn't writable | Fix the path or permissions; Warden retries every second and `worker output file is writable again` reports what was lost |
| `cannot write the worker output file; its output is discarded until writes work again` | Direct mode: disk full, quota, I/O error. The worker keeps running (its output is read and dropped, never left to block it) | Free space (`df -h`); writing resumes by itself |
| `cannot rotate the worker output file; it keeps growing` | Warden can't rename or create files in the log directory | Fix the directory's permissions; retried every minute |
| `a worker wrote a line far longer than the log file limit; splitting it across rotated files` | Direct mode rotates at line ends; a line that doesn't end within `max_size` + max(`max_size`, 1 MiB) is cut | The line continues at the start of the next file. If the app really writes such lines, raise `[logging.rotate] max_size` |
| Direct mode: `warden logs` shows worker lines all with the same time | The files carry no timestamps; lines read from them are stamped with the file's last write | Intended. `warden logs -f` stamps new lines as they arrive; for per-line times let the app's logger write them |
| `worker output not shown: its file is not in memory right now` | Direct mode: `warden logs` reads the files' tails only from the page cache, so it never waits for a disk | Read the file itself (`tail`, `less`) |
| `warden flush` left `out.log.1`, `out-2.log.3.gz`… | Intended, as with `pm2 flush`: it empties the in-memory buffer and the current files (`[logging] file` or the background log, `out_file`/`err_file` and every worker's with `per_worker_files`), and keeps rotated ones | Rotated files go by `[logging.rotate] keep` and `max_age_days`; delete them yourself if you need the space now |
| `could not empty a log file on \`warden flush\`; it keeps its content` (and `warden flush` exits 1 naming the file) | Warden could not write the file: its owner or mode changed, or it is on a read-only filesystem (`error=`) | Fix the owner or permissions (`ls -l <file>`), then `warden flush` again. The other files were emptied |
| `log files are still being emptied after \`warden flush\`` | The thread that writes the files did not get to the request within 2 s: a slow disk, or a log consumer (journald, a pipe) that stopped reading holds it up | Nothing to do: the files are emptied as soon as it catches up. If it repeats, see `log lines dropped because stdout could not keep up` |

## Static files (`warden serve`)

| You see | Why | Fix |
|---|---|---|
| A symlinked file returns 404 | Its target is outside the served root | Put the target under the root, or serve a directory that contains it. Symlinks that stay inside the root, relative or absolute, are served |
| `openat2 is unavailable … checking paths with realpath instead` | Kernel before 5.6, or a seccomp profile blocks `openat2` (some container runtimes) | Nothing breaks, it is slower; allow `openat2` in the seccomp profile. `WARDEN_STATIC_OPEN=legacy` forces this mode for testing |
| Stale content after a deploy | Workers resolve a `current` symlink when they start | `warden restart <site>` (rolling, no downtime) |

## The CLI

| You see | Why | Fix |
|---|---|---|
| `not answering; see <log>` | The app's supervisor isn't running | `warden start <app>`; the log path in the message has the reason it stopped |
| `too many control connections; refusing new ones` | More than 64 clients at once (a monitoring loop without timeouts?) | Fix the client; each request has a 5 s timeout |
| `too many live streams; refusing a new one` | More than 32 `warden events` / `warden logs -f` sessions or GUI windows on one socket | Close some; commands (`status`, `stop`, `restart`) still get through, streams have their own budget |
| Permission denied on the control socket | It is 0600 and owned by the user Warden runs as | Run the CLI as that user (or `sudo -u <user>`) |

## Reboots, crashes and wardend

`warden startup` installs what brings the saved apps back (README:
"Surviving reboots and crashes"); `warden doctor` says whether anything does.

| You see | Why | Fix |
|---|---|---|
| `no service manager found: systemd is not running here` | A container or another init system: nothing to install into, so nothing was | Run `warden resurrect` from the entrypoint or init script, or make `warden wardend --resurrect` the entrypoint (under `docker run --init`) |
| `writing …/warden@.service: Permission denied; nothing was enabled` | System units need root | `sudo warden startup`, or `warden startup --user` for units of your own |
| `your systemd user manager did not answer, so nothing is enabled` (`Failed to connect to bus`) | No login session for that user (`su`, cron, a script) | Run `warden startup` from an ssh login as that user; if there is none, as root once `loginctl enable-linger <user>`, then log in and run it again |
| `could not turn on lingering for <user>` | `loginctl enable-linger` needs root (polkit) on this system | `sudo loginctl enable-linger <user>`. The units are installed; until then the apps start at login and stop at logout |
| `warden@.service reads <dir>/<app>.toml, but this app's config is …` | The unit reads configs only from the config directory | The message has the `ln -s` to run; then `warden startup` again |
| `warden list` shows an app `offline` while `systemctl status warden@<app>` says it runs | The unit runs Warden as another user (`User=`, e.g. the `www-data` of contrib/warden@.service, or a unit written by an older `warden startup`) without `WARDEN_RUNTIME_DIR`, so its control socket went to that user's default, `/tmp/warden-<uid>`, where root's CLI and wardend don't look | `sudo warden startup` again (its units run as root, with `WARDEN_RUNTIME_DIR=/run/warden`), or add `Environment=WARDEN_RUNTIME_DIR=/run/warden` to your unit (`systemctl edit warden@<app>`) and restart it |
| `warden@.service: kept, because <apps> still run under it` (`warden unstartup`) | Those apps run under their units; removing the unit file under a running unit leaves it half configured (systemd then killed it as hung every `WatchdogSec`) | Nothing starts at boot any more. `warden kill <app>` stops them when you want, then `warden unstartup` again removes the file |
| `… unloaded, but launchd still lists it 10 s later` (`warden unstartup`, macOS) | wardend did not exit after launchd's SIGTERM | `launchctl print <target>` shows its state and pid; `sudo kill -9` that pid |
| `launchctl bootstrap gui/<uid> … failed` (`Domain does not support specified action`) | macOS: no desktop login session (SSH only) | The plist is written and loads at the next desktop login; for boot without a login, `sudo warden startup` (a LaunchDaemon) |
| `wardend runs as the systemd unit wardend.service … stopping the unit failed` | `warden kill` as a user who cannot manage that unit; stopping only the process would get it restarted | `sudo systemctl stop wardend` |
| `wardend died without a clean exit; starting it again` (in an app's log) / `wardend is back` | wardend was killed or crashed (a clean exit, which is what `warden kill` does, removes its socket; a crash or `kill -9` leaves it) and a supervisor started it again | Nothing: it is always on. If it keeps dying, the end of `logs/wardend.log` in the state directory says why. `warden kill` stops it for good until the next `warden start` or `warden resurrect` |
| `wardend exited right after it was started again` / `could not start wardend again` | A supervisor tried and wardend failed to run (a full disk, a runtime directory it cannot use, a broken binary) | `logs/wardend.log`; `warden doctor`. The apps are not affected. The supervisor retries with growing pauses (up to 5 min) |
| `warden doctor` says `wardend: not running` | `warden kill` stopped it, or nothing was started yet | `warden resurrect` or `warden start <app>`: both start it. `WARDEN_NO_DAEMON=1` keeps it off |
| `warden list` shows `-` for cpu, mem, user or ports after upgrading warden | A supervisor keeps the code it started with: an upgrade or a rebuild replaces the file, not the process, so an old supervisor lacks what newer ones report (the list and the GUI say which) | `warden update`: it saves, stops every supervisor and wardend, and starts them again from the installed binary (the apps stop for a few seconds; stopped apps stay stopped). The GUI has the same under Settings, "Restart everything" |
| `wardend is already running (pid N)` repeating in wardend's log | A wardend started by hand holds the socket, and systemd or launchd keeps retrying theirs | `warden kill` (everything); the service manager's own then starts. `warden startup` does this hand-over itself |
| `supervisor died; restarting it` | A supervisor launched in the background died without saying `bye` (crash, OOM kill, kill -9) | `error=` says how; its log (in `hint=`) says why. wardend restarts it after 1, 2, 4 … 60 s |
| `supervisor keeps dying; wardend gave up restarting it` | 10 deaths within 10 minutes | Fix the error in `last_lines=`, then `warden start <app>` |
| `supervisor is unresponsive; not killing it` | No event and no `status` answer for 3 s + 5 s; its workers probably still serve | `cat /proc/<pid>/stack` or `gdb -p <pid>` shows where it is stuck; restart it yourself if it stays stuck |
| `supervisor died; not restarting it (it was not started in the background)` | It ran in a terminal (or under systemd, which restarts it itself) | `warden start <app>` runs it in the background, where wardend restarts it |
| `a saved app's config is gone; not starting it` | `warden wardend --resurrect` found a saved app whose config was deleted | `warden save` again to forget it |
| `wardend.toml has errors; keeping the alert rules in force` | The alert rules file did not parse or check (at start, when the file changed, or on SIGHUP); `errors=` lists every problem with its line | `warden check -c wardend.toml` shows the same list; fix the file and wardend reads it again by itself. Until then the previous rules (none at start) apply |
| `alert delivery failed; trying once more` / `… failed twice; this alert is lost` | The rule's command exited non-zero or ran over 10 s (it is killed, with its children), or the webhook answered an HTTP error or could not be reached | `error=` has the exit status and the command's first stderr line, or curl's error; `hint=` the next step. Run the command by hand with a JSON alert on stdin; for a webhook, check the URL (a 404 often means a revoked Slack hook) and that this host reaches it (DNS, firewall, `https_proxy` in wardend's environment) |
| `cannot send alerts to webhooks: curl is not installed` | Webhooks are POSTed by running `curl` (Warden has no TLS stack of its own); said once | `apt install curl` (`dnf`, `apk`, …), or use `command = [...]` |
| `alert queue is full; dropping alerts` | More than 64 alerts wait: deliveries are slow (each may take 10 s, one retry) and many alerts fire | Make the command or webhook answer fast; raise `min_interval`; narrow `on` / `apps` |
| `alert rules: a warning` | The file is valid, but: a plain `http://` webhook to another host (the token crosses the network unencrypted), or curl missing | Use `https://`; install curl |
| `resource history is full; new apps get none` | 128 apps have a series and none has been gone 10 minutes | Unusual; the history of an app gone for 10 minutes makes room |
| `cannot use the resource history on disk; starting with an empty one` (`reason=`, `moved_to=`) | At start, `<state dir>/wardend-history.bin` was truncated (a full disk, an incomplete copy), corrupt (checksum or structure), of another format version (a downgrade), bigger than any wardend writes, or not a history or not a regular file | Nothing to do: the file was moved to `wardend-history.bin.bad` and wardend writes a new one within a minute; the charts before this start are lost. If it repeats, check the disk (`df -h`, `dmesg`) and report it with the `.bad` file |
| `cannot read the resource history; starting with an empty one` | The file exists but reading it failed (`error=`: permissions, an I/O error); it is left in place | Check the file's owner and mode (wardend's user, 0600) and the disk; the next save (every minute) replaces it |
| `cannot save the resource history; it is kept in memory only until a save works` | Writing the snapshot failed (`error=`: the state directory is missing or not writable, the disk is full, fsync or rename failed); said once until a save works again (`resource history saved again`) | Make the state directory writable by wardend's user (`/var/lib/warden` for root, `~/.local/state/warden` for a user) and free space (`df -h`); wardend retries every minute and keeps the history in memory meanwhile |
| `the resource history on disk is newer than the clock; dropping the samples from the future` | The system clock is behind the newest saved sample (it went back, or was not set yet at boot) | Check the clock (`timedatectl`, NTP); the charts continue from now |
| `saving the resource history has not finished; no new snapshot until it does` (`running_s=`) | A snapshot's fsync or rename has been running for 5 minutes: a hung disk or network mount under the state directory | Check the disk or mount (`dmesg`); saves resume once it returns, and the history is kept in memory meanwhile |
| `the last save of the resource history is still running; not saving it again at exit` | At a clean exit the previous snapshot's fsync had not finished within 2 s: the disk is very slow or hung | The file on disk is the previous snapshot (up to a minute old); check the disk (`dmesg`) |

## Behind nginx or a load balancer

[`docs/proxies.md`](proxies.md) has the timeouts that must agree; the usual symptoms:

| You see | Why | Fix |
|---|---|---|
| A few 502s during a deploy (nginx: `recv() failed (104: Connection reset by peer) while reading response header from upstream`) | Connections queued on a closing worker's listener are reset (`net.ipv4.tcp_migrate_req` is 0; Warden warns at start), and nginx does not retry them | `contrib/99-warden.conf` (`sysctl -w net.ipv4.tcp_migrate_req=1`); use `contrib/nginx.conf`, whose backup entry lets nginx retry idempotent requests |
| Occasional 502s at any time, not only during deploys | The proxy or load balancer keeps idle connections to the app longer than the app does (Node 5 s, Bun 10 s), and sends a request on one the app is closing | nginx: upstream `keepalive_timeout` below the app's (4 s in `contrib/nginx.conf`); a load balancer straight to the app: raise the app's idle timeout above the load balancer's (ALB 60 s, GCP 600 s), or put nginx between |
| WebSocket clients see 1006 (or a reset) after a restart through nginx, instead of 1001 | nginx closed the client connection before reading the client's close reply | `lingering_close always;` in the WebSocket location (in `contrib/nginx.conf`) |
| SSE events arrive in bursts, or only when the stream ends | The proxy buffers the response | nginx: `proxy_buffering off` for the SSE location, or the app sends `X-Accel-Buffering: no` |
| WebSockets or SSE streams end after 60 s (ALB), 100 s (Cloudflare) or 30 s (GCP) | The load balancer's idle or backend timeout | Ping (or send an SSE comment) more often than the idle timeout; GCP: raise the backend service timeout |
| Requests fail for a while after `systemctl stop warden@api` on a host behind a load balancer | The load balancer still sends to the host until its health check fails | Deregister the host first, wait for the deregistration delay (≥ `grace_period`), then stop Warden. Deploys (`reload`, `safe-reload`) need no deregistration |

## The GUI over SSH (`warden-gui --ssh`)

The GUI shows ssh's own message with the fix; the usual ones:

| You see | Why | Fix |
|---|---|---|
| `Permission denied (publickey)` | The server refused every key the GUI's ssh offered (it never asks for a password) | `ssh-add` your key; `ssh user@host` in a terminal must work without a prompt. The GUI then tries again only every 10 minutes (Connection… → Connect tries at once) |
| `Host key verification failed` | The host's key is not in `known_hosts` yet | Connect once with `ssh user@host` in a terminal and check the fingerprint |
| `…'s host key has changed since you last connected` | The key differs from the one in `known_hosts`: a reinstalled host, or someone in between | Check the new fingerprint with the host's administrator, then run the `ssh-keygen -R` line shown; if the change is unexpected, do not connect |
| `nothing answers on <socket> on <host>` (`ssh: channel N: open failed: connect failed`) | wardend is not running there, runs as another user or with another socket, or that sshd does not forward Unix sockets | Start wardend (the button, or `warden resurrect` there); fix the remote socket (`/run/warden/wardend.sock` for root's, `/run/user/<uid>/warden/wardend.sock` for a user's); in the remote `sshd_config`, `AllowStreamLocalForwarding yes` and neither `AllowTcpForwarding no` nor `DisableForwarding yes` |
| `warden was not found on <host>` | The remote shell has no `warden` on its PATH (ssh runs commands without your login profile) | Install it there, or give its path: `--remote-warden ~/.local/bin/warden`, or in Connection… |
| `the SSH tunnel to <host> ended: its ssh process was killed` | Something killed the GUI's ssh | Nothing to do: the GUI opens a new tunnel |
| `Connection refused`, `timed out`, `No route to host`, `Could not resolve hostname` | The host or its sshd is down or unreachable, or the name is wrong | Check the host and its port (`~/.ssh/config`); the GUI keeps retrying |

## Containers

- Don't run Warden as PID 1: orphaned grandchildren are never reaped. Use
  `docker run --init` or `tini` (also for `warden wardend --resurrect`).
- `net.ipv4.tcp_migrate_req` is per network namespace: set it for the
  container (`docker run --sysctl net.ipv4.tcp_migrate_req=1`).
- Some seccomp profiles block `openat2`: see Static files above.

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
| `worker not ready in time; killing` | The app didn't listen on `PORT` within `workers.ready_timeout` (30 s), or, with `wait_ready`, never called `process.send('ready')` | Check that the app listens on `process.env.PORT`; raise `ready_timeout` for slow boots; `warden logs <app>` shows the app's own errors |
| `failed to start worker` | The command could not be executed | The error names the path: fix `command`/`args`/`working_directory`; `warden check -c <file>` validates the config first |
| `worker failed: too many restarts` | It crashed `max_restarts` times within `restart_window` | Fix the crash (the last exits are in `warden describe`); it is retried after `failed_cooldown`, or now with `warden reset <app>` |
| `EADDRINUSE` in the app's output | Workers can't share the port | Bun: the shim adds `reusePort` (don't set `shim = false`). Node: needs 22.12 or newer; older Node: `port_strategy = "offset"`. Another program on the port: `ss -ltnp 'sport = :3000'` |
| The app is `stopped` after a clean exit | Its exit code is in `restart.stop_exit_codes` | Intended for one-shot jobs; `warden restart <app>` starts it again |
| An app without a port shows `starting` for a second | Port-less apps count as ready after `workers.min_uptime` (1000 ms) | Intended (PM2's `min_uptime`); lower it if you like |

## Restarts, reloads and deploys

| You see | Why | Fix |
|---|---|---|
| `safe-reload failed at worker 1 … Rolled back` | The canary failed a gate (health, `verify_command`, soak) | Nothing to undo: every worker still runs the previous version. The message says which gate; test the new release, then deploy again |
| `dependency outage suspected: holding worker replacements` | More than `health.outage_threshold` of the workers fail at once: probably a database or API is down, not the workers | Fix the dependency; Warden resumes replacements by itself. Use a `live_path` that checks the process only, and a `ready_path` for dependencies |
| `net.ipv4.tcp_migrate_req is 0` | Connections waiting in a closing worker's accept queue get reset during restarts | `sysctl -w net.ipv4.tcp_migrate_req=1` and `contrib/99-warden.conf` (what `warden startup` installs) |
| `worker did not exit within grace period; sending SIGKILL` | The app ignored the stop signal for `shutdown.grace_period` | Apps written for PM2 often listen for SIGINT: `shutdown.signal = "SIGINT"`. Otherwise close servers and DB pools on SIGTERM |
| `config changes that need systemctl restart were not applied` | `reload` re-reads the config, but some keys only apply at start: `[workers]` (use `warden scale` for the count), `[metrics]`, `[control]`, `health.url/enabled/interval`, `logging.timestamps/worker_output` | The line lists them; `systemctl restart warden@<app>` (a full restart) applies them |

## Health

| You see | Why | Fix |
|---|---|---|
| `worker unhealthy` / `worker health check failed` | The worker's health path failed `failure_threshold` times in a row on its private socket | The line has the HTTP status or error; the worker is replaced gracefully (`on_failure = "replace"`) |
| `only 0 of 4 private health socket(s) reported` | A health path is set, but the app didn't listen through the shim | Keep `shim` on (Bun and Node), or use `health.url` for an app-level check |
| `worker hung: no heartbeat from its event loop` | The event loop was blocked for `watchdog.timeout` (60 s): an infinite loop, a sync call that never returns | The worker is killed and restarted; profile the app (`event loop was blocked` warnings show shorter stalls) |

## Logs

| You see | Why | Fix |
|---|---|---|
| `worker writes too much output; lines dropped` | A worker wrote more than `logging.max_lines_per_sec` (10,000) lines/s | Lower the app's log level; raise the budget; `max_lines_per_sec = 0` keeps every line (a flooding app is slowed down instead) |
| `log lines dropped because stdout could not keep up` | Warden's stdout (journald, a pipe) is slower than the apps' output | Check journald (`journalctl --disk-usage`, rate limits: `RateLimitBurst=`); write to files instead (`[logging] file`/`out_file`) |
| `cannot write the log file` | The directory doesn't exist or isn't writable, or the disk is full | Fix the path or permissions; lines still go to stdout and memory |
| `warden logs --history` finds nothing | No log files are configured and journald isn't in use | Set `[logging] out_file`/`err_file`/`file`, or run under systemd |
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
| `no service manager found: systemd is not running here` | A container or another init system: nothing to install into, so nothing was | Run `warden resurrect` from the entrypoint or init script, or make `warden daemon --resurrect` the entrypoint (under `docker run --init`) |
| `writing …/warden@.service: Permission denied; nothing was enabled` | System units need root | `sudo warden startup`, or `warden startup --user` for units of your own |
| `your systemd user manager did not answer, so nothing is enabled` (`Failed to connect to bus`) | No login session for that user (`su`, cron, a script) | Run `warden startup` from an ssh login as that user; if there is none, as root once `loginctl enable-linger <user>`, then log in and run it again |
| `could not turn on lingering for <user>` | `loginctl enable-linger` needs root (polkit) on this system | `sudo loginctl enable-linger <user>`. The units are installed; until then the apps start at login and stop at logout |
| `warden@.service reads <dir>/<app>.toml, but this app's config is …` | The unit reads configs only from the config directory | The message has the `ln -s` to run; then `warden startup` again |
| `launchctl bootstrap gui/<uid> … failed` (`Domain does not support specified action`) | macOS: no desktop login session (SSH only) | The plist is written and loads at the next desktop login; for boot without a login, `sudo warden startup` (a LaunchDaemon) |
| `wardend runs as the systemd unit wardend.service … stopping the unit failed` | `warden kill` / `warden daemon stop` as a user who cannot manage that unit; stopping only the process would get it restarted | `sudo systemctl stop wardend` |
| `wardend is already running (pid N)` repeating in wardend's log | A wardend started by hand holds the socket, and systemd or launchd keeps retrying theirs | `warden daemon stop`; the service manager's own then starts. `warden startup` does this hand-over itself |
| `supervisor died; restarting it` | A supervisor launched in the background died without saying `bye` (crash, OOM kill, kill -9) | `error=` says how; its log (in `hint=`) says why. wardend restarts it after 1, 2, 4 … 60 s |
| `supervisor keeps dying; wardend gave up restarting it` | 10 deaths within 10 minutes | Fix the error in `last_lines=`, then `warden start <app>` |
| `supervisor is unresponsive; not killing it` | No event and no `status` answer for 3 s + 5 s; its workers probably still serve | `cat /proc/<pid>/stack` or `gdb -p <pid>` shows where it is stuck; restart it yourself if it stays stuck |
| `supervisor died; not restarting it (it was not started in the background)` | It ran in a terminal (or under systemd, which restarts it itself) | `warden start <app>` runs it in the background, where wardend restarts it |
| `a saved app's config is gone; not starting it` | `warden daemon --resurrect` found a saved app whose config was deleted | `warden save` again to forget it |
| `wardend.toml has errors; keeping the alert rules in force` | The alert rules file did not parse or check (at start, SIGHUP or `warden daemon reload`); `errors=` lists every problem with its line | `warden daemon check` shows the same list; fix it, then `warden daemon reload`. Until then the previous rules (none at start) apply |
| `alert delivery failed; trying once more` / `… failed twice; this alert is lost` | The rule's command exited non-zero or ran over 10 s (it is killed, with its children), or the webhook answered an HTTP error or could not be reached | `error=` has the exit status and the command's first stderr line, or curl's error; `hint=` the next step. Run the command by hand with a JSON alert on stdin; for a webhook, check the URL (a 404 often means a revoked Slack hook) and that this host reaches it (DNS, firewall, `https_proxy` in wardend's environment) |
| `cannot send alerts to webhooks: curl is not installed` | Webhooks are POSTed by running `curl` (Warden has no TLS stack of its own); said once | `apt install curl` (`dnf`, `apk`, …), or use `command = [...]` |
| `alert queue is full; dropping alerts` | More than 64 alerts wait: deliveries are slow (each may take 10 s, one retry) and many alerts fire | Make the command or webhook answer fast; raise `min_interval`; narrow `on` / `apps` |
| `alert rules: a warning` | The file is valid, but: a plain `http://` webhook to another host (the token crosses the network unencrypted), or curl missing | Use `https://`; install curl |
| `resource history is full; new apps get none` | 128 apps have a series and none has been gone 10 minutes | Unusual; the history of an app gone for 10 minutes makes room |

## Containers

- Don't run Warden as PID 1: orphaned grandchildren are never reaped. Use
  `docker run --init` or `tini` (also for `warden daemon --resurrect`).
- `net.ipv4.tcp_migrate_req` is per network namespace: set it for the
  container (`docker run --sysctl net.ipv4.tcp_migrate_req=1`).
- Some seccomp profiles block `openat2`: see Static files above.

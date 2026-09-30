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
| Permission denied on the control socket | It is 0600 and owned by the user Warden runs as | Run the CLI as that user (or `sudo -u <user>`) |

## Containers

- Don't run Warden as PID 1: orphaned grandchildren are never reaped. Use
  `docker run --init` or `tini`.
- `net.ipv4.tcp_migrate_req` is per network namespace: set it for the
  container (`docker run --sysctl net.ipv4.tcp_migrate_req=1`).
- Some seccomp profiles block `openat2`: see Static files above.

# Staying up for weeks

What Warden does when a worker crashes, hangs, leaks or degrades, and where
to tune it.

| | What happens | Config |
|---|---|---|
| Crash | Restart with exponential backoff. After too many restarts in a window, the worker is FAILED and retried after a cooldown. | `[restart]` |
| Crash, without the startup time | A hot standby (started, app initialized, not listening) takes the dead worker's slot in a few milliseconds; a new standby starts in the background | `[workers] standby` |
| Unhealthy worker | Replaced gracefully (new worker ready first) after `failure_threshold` failed checks | `[health] on_failure = "replace"` |
| Hung event loop | The shim's heartbeat stops, and the worker is killed and restarted | `[watchdog] timeout` |
| Slow event loop | Each heartbeat carries the worker's event-loop delay over the last second (p50/p99/max, sampled natively every 100 ms: no cost per request). `warden list`/`top`/`describe` show p99 (`loop p99`), `status --json` all three (`loop_delay`), Prometheus `warden_worker_event_loop_delay_{p50,p99,max}_seconds`; a WARN when p99 stays high for 10 s | `[watchdog] loop_delay_warn` |
| Memory leak | Graceful replacement when RSS stays above the limit | `[limits] max_memory` |
| Slow degradation | Recycle every worker after a lifetime, ±10% jitter | `[limits] max_lifetime` |
| Stop / shutdown | SIGTERM to each process group, drain (WebSockets closed with 1001 and SSE streams ended after `long_lived_timeout`), SIGKILL after `grace_period` | `[shutdown]` |
| Supervisor killed | Linux: its workers get SIGTERM at once (parent-death signal), and wardend or systemd starts it again. macOS has no such signal: the workers keep running until the app starts again, and the new supervisor stops them first, so there are never two sets ([macOS](platforms.md#workers-a-killed-supervisor-leaves-behind-macos)) | |
| Why it died | `last_exit` and the log line say who ended a worker: a crash (`SIGSEGV`, `SIGABRT`), the kernel's OOM killer (from its cgroup's `oom_kill` count; `probably …` when other workers of that cgroup died of SIGKILL at the same moment), Warden, or another process, with a hint for the fix | |

Every problem Warden logs says what happened, why, what it did and how to fix
it; [`troubleshooting.md`](troubleshooting.md) collects them by symptom.

## Hot standbys: crash recovery in milliseconds

A crashed worker normally comes back after the runtime and the app have
started again: in the benchmark 54 ms for a small Bun app, 129 ms for
node:http, 687 ms for NestJS on Node. With standbys, that work is done
before the crash:

```toml
[workers]
count = 4
standby = 1      # one extra worker, started but not listening
```

Warden starts the standby once the workers are ready. The shim lets the app
initialize completely but holds back its listen on `app.port` (Bun.serve,
node:http, Express, NestJS; servers on other ports start as usual), so the
standby takes no traffic. When a worker dies, Warden tells the standby to
listen: it joins the shared port in about a millisecond and becomes that
worker (its number, `NODE_APP_INSTANCE`, its log label). A new standby then
starts in the background. Measured from `kill -9` to the port answering
again, with one worker: 4-15 ms (debug build, idle machine), whatever the
app's startup time; in the benchmark (4 workers, under load) 17-57 ms
against 54-687 ms without a standby and 166-1,182 ms under PM2.

- A promotion is the slot's restart: it is counted, backed off and ends in
  FAILED like any restart; the standby only saves the startup.
- A standby must pass the same health gates as a rollout's new worker before
  it can be promoted, and while idle gets the health checks and the
  watchdog; a failing one is replaced, never promoted. Standbys that keep
  crashing back off, then stop until `failed_cooldown` or `warden reset`.
- Deploys: during `reload`, `safe-reload` or `restart`, crashed workers
  restart the normal way; when the deploy succeeds the standbys (still on the
  previous version) are replaced, and when it rolls back they stay.
  Recycling (memory, lifetime, health) uses a standby as the replacement.
- Memory: a standby costs about one idle worker (RSS: 42 MB for a small Bun
  app, 57 MB on Node, 84 MB for NestJS on Bun, ~100 MB on Node), since the
  app is fully loaded; much of that is shared with the workers (one standby
  next to two Bun workers: +41 MB RSS, +11 MB PSS).
- Code that runs only on instance 0 and decides at startup (a cron) sees a
  number past the workers' in a standby; after promotion `NODE_APP_INSTANCE`
  is the slot's, and `process.on("warden:promote", ...)` runs late setup.

`warden status` lists standbys after the workers as `s1`, `s2`… (`WARMING`
then `STANDBY`; `standbys` in `--json`), `warden events` and the GUI show
their events as `worker s1`, and the GUI counts their memory.
A standby starts in the pinned release (`pin_release`) and is promoted only
while that is still the workers' release. Process mode only, with
`app.port`, a shared port and the shim (bun and node commands).

## See also

- [`configuration.md`](configuration.md): `[restart]`, `[health]`, `[watchdog]`, `[limits]`, `[workers] standby`.
- [`deploys.md`](deploys.md): rolling reloads and rollback.
- [`wardend.md`](wardend.md): alerts for crash loops, failed rollouts and OOM kills.
- [`troubleshooting.md`](troubleshooting.md#hot-standbys-workers-standby): standby problems.

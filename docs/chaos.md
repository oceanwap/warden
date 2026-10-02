# Chaos soak: faults under load, invariants checked

`cargo xtask chaos` runs a real fleet in a throwaway `WARDEN_HOME`, keeps
client load on it, injects random faults for a while, and checks after
each fault and at the end that nothing broke beyond what Warden documents.
It exits 1 on any violation, with the seed to reproduce the run and the
log lines around each violation. It complements the integration tests
(one scenario at a time) and the scenario matrix of
[`review-process.md`](review-process.md) §3 (by hand): here the faults land
on each other, on every feature at once, for minutes.

```sh
cargo xtask chaos                      # 10 minutes of faults, a random seed (printed)
cargo xtask chaos --minutes 3          # a short run while developing
cargo xtask chaos --seed 2 --minutes 3 # the same faults, in the same order, on the same targets
cargo xtask chaos --only stop-supervisor,kill-worker --minutes 2
cargo xtask chaos --release            # against a release build (what production runs)
cargo xtask chaos --help
```

It builds Warden in **debug** mode by default (correctness, not speed: the
CLI and supervisor latencies it reports are then a debug build's);
`--release` builds and runs a release build, and the report says which ran.
Needs `bun`, `node` ≥ 22.12, ~1.2 GB of RAM, and npm once (the NestJS app's
packages: `npm ci` in `bench/nest`, run for you when they are missing).
Linux runs everything; as root, it also gets the three things below. macOS
runs what it supports (see [On macOS](#on-macos)). Whatever a host lacks is
left out, printed at the start and listed in the report.

- **Its own pid and mount namespace** (`unshare`, with `bash` as the
  namespace's init). Background supervisors are orphans by design (`warden
  start` detaches them); whoever is pid 1 must reap them when they die, as
  a host's init or `docker run --init` does. The sandbox this was written
  in has a pid 1 that doesn't, so without the namespace every `kill -9` of
  a supervisor would leave a zombie that is not Warden's. `--no-namespace`
  runs without it (and without the tmpfs).
- **A 48 MB tmpfs for the apps' log files**, so the disk can really fill.
  It disappears with the namespace.
- **A memory cgroup** (400 MB, no swap) for the `oom` app: a child of the
  harness's own memory cgroup (v1 or v2), else of the cgroup v2 root (a
  service's own cgroup holds processes, so it cannot give children the
  memory controller). Warden limits no memory itself: it reads the OOM-kill
  counter of the cgroup it runs in, as under a unit's `MemoryMax=` or a
  container's limit. The app's supervisor is started inside it (its workers
  inherit it); the cgroup is removed at the end. Where none can be made, the
  app and the `oom-kill` fault are left out, with the reason.

## The fleet

| App | What it runs | Exercises |
|---|---|---|
| `api-bun` | `bench/chaos/app.ts` on Bun, 2 workers, `standby = 1`, `working_directory` behind a `current` symlink | hot standbys, release pinning, captured output |
| `api-node` | `tests/fixtures/longlived_node.mjs` on Node, 3 workers, `[reload] surge = 2` | surge rollouts, the Node shim, WebSocket + SSE on node:http |
| `ws-bun` | `tests/fixtures/longlived.ts`, 2 workers | Bun.serve WebSockets and every SSE body (stream, direct, generator) |
| `threads` | `bench/chaos/app.ts` in worker mode, 2 Workers | the host process, a Worker that throws |
| `site` | Warden's static server, 2 workers | the file server, its cache and drain |
| `direct` | `bench/chaos/app.ts`, 2 workers, `worker_output = "direct"` | spliced output files, their rotation, a full disk |
| `crashy` | `bench/chaos/app.ts`, 1 worker, `max_restarts = 3` | a crash loop to FAILED and `warden reset` |
| `memhog` | `bench/chaos/app.ts`, 2 workers, `[limits] max_memory = 200` (Linux: Warden reads RSS from /proc) | graceful memory recycling |
| `oom` | `bench/chaos/app.ts`, 2 workers, its supervisor in the memory cgroup (Linux, root) | the kernel's OOM killer and how Warden reports it |
| `nest` | `bench/nest/main.ts` (NestJS, Express) on Bun, 2 workers | `node:http` through the shim on a shared port, Nest's own shutdown hooks deferred to the drain |
| wardend | `warden wardend --background`, an alert rule writing to a file | supervisor restarts, alerts, its own death |

Every app runs with `[watchdog] timeout = 4`, `grace_period = 10` and
`long_lived_timeout = 1`, and all but `site` and `crashy` with health
checks and gates on `/health`. Load, all the time, without
retries: for each app but `crashy`, one keep-alive client (a request every
20 ms on one connection; a new one after `Connection: close` or a failure)
and one new-connection client (every 40 ms); two WebSocket clients and
three SSE clients on `ws-bun`, one of each on `api-node`, which reconnect
at once when their connection ends (a message every 5 s and pongs keep
the WebSockets from idling out).

## Faults

One at a time, in a shuffled deck (every kind comes up once per round, in
a random order), with 1–5 s between them. After each the harness waits for
the whole fleet to be ready again: every app with all its workers RUNNING
and ready, its standby available, no rollout running, and wardend seeing
every app running. **Recovery time** is measured from the end of the
injection (the kill, the `SIGCONT`, the CLI returning) to that point.

| Fault | What happens | What clients may see (allowances) |
|---|---|---|
| `kill-worker` | `kill -9` of a random worker of a random app (worker mode: the host) | requests in flight on it; connections queued on its listener; worker mode: the app down until the host restarts (documented: the threads go together) |
| `kill-standby` | `kill -9` of `api-bun`'s standby | nothing |
| `kill-supervisor` | `kill -9` of a random app's supervisor; wardend must restart it | the app down until then (documented: the workers drain and exit with it, PDEATHSIG) |
| `kill-wardend` | `kill -9` of wardend, started again 0.5–3 s later (as systemd would) | nothing |
| `stop-worker` | `SIGSTOP` of a worker for `watchdog.timeout` + 2.5–4.5 s; the watchdog must kill it | requests on it; worker mode: the app down |
| `stop-supervisor` | `SIGSTOP` of a supervisor for 2–9 s | nothing; its workers must survive |
| `stop-wardend` | `SIGSTOP` of wardend for 2–9 s | nothing; no supervisor may be restarted |
| `reload`, `safe-reload`, `restart` | the operation on a random app; must exit 0 | queued connections reset with `tcp_migrate_req = 0` |
| `restart-hard` | `warden restart <app> --hard` | the app down for a moment (documented) |
| `scale` | one worker more or less, then back | as `reload` |
| `overlap` | `reload`/`safe-reload`/`restart`, then during it another one, `restart --hard`, a scale, or `kill -9` of a worker; exit 0 or 1 (refused, aborted), never 2 or a hang | as the operations involved |
| `bad-config` | an unknown key, a missing script, or a release whose `/health` fails, with `reload` or `safe-reload` (must exit 1); a worker killed meanwhile (must restart on the config in effect); the good config reloaded | as `kill-worker` and `reload` |
| `release-swap` | `current` swapped to the other release, `kill -9` of a worker (it and the standby must stay on the pinned release, checked through `/proc/<pid>/cwd`), then `reload` (everything on the new one, `status.release` too) | as `kill-worker` and `reload` |
| `throw-thread` | an uncaught error in one Worker of `threads` | requests on that Worker |
| `log-flood` | 10–60 MB of output from one worker; `direct`'s files must stay within their rotation bound | nothing |
| `disk-full` | the log tmpfs filled, two apps flooding into it for 3–6 s, then freed | nothing |
| `crash-loop` | `crashy` made to exit at start and killed: it must reach FAILED, and come back after the cause is gone and `warden reset` | its own outage (no load on it) |
| `oom-kill` | a worker of `oom` allocates twice the cgroup's limit (`/grow`): the kernel must kill it, and Warden restart it and report it as an OOM kill: the slot's `last_exit`, the `worker crashed` log line with its `hint=`, wardend's `oom` alert | requests on that worker |
| `memory-recycle` | a worker of `memhog` grows 300 MB over `max_memory`: Warden must replace it gracefully within 60 s (new worker first, the old one drained), not count a crash, log `worker scheduled for replacement … max_memory` and send a `recycled` alert | as `reload` (nothing else may be lost) |

`kill-supervisor` never picks `oom`: wardend restarts a supervisor in its
own cgroup, so the app would leave the memory cgroup.

## Invariants

Checked continuously and at the end; any violation fails the run.

| Invariant | How |
|---|---|
| No request fails except within the allowances | Every failed request (and WebSocket/SSE connection attempt) is matched with the faults whose window (start − 0.5 s … recovered + 2 s) covers it and the allowances above; `who` (the pid in the body) ties a keep-alive failure to its worker. `tcp_migrate_req` is read and reported; its resets are allowed only with 0. Counted per app, client and fault kind |
| Every app recovers to all-ready within the bound | `--bound` (60 s) after each fault, and at the end |
| WebSocket/SSE clients see 1001 or a clean end | Every ended connection: a close frame with 1001, or the chunked stream's last chunk after a whole event, unless its worker was killed (then any end is allowed) |
| No zombies | `/proc` every 2 s: a zombie seen three scans in a row (4+ s) |
| No orphans | A process of the run whose parent is gone, still alive `grace_period` + 10 s later |
| No fd leak, no RSS growth | Open fds and RSS of every supervisor and wardend every 4 s while the fleet is quiet (recovered, no fault running); per process, the lowest of the first third against the lowest of the last third: growth above max(4 fds, 10 %) or max(4 MB, 25 %) fails. A process with under 2 minutes or 9 quiet samples (one that was killed and restarted late) is not judged |
| No panics | `panicked at` or `essential task failed` in any supervisor's or wardend's log |
| Every WARN/ERROR has a `hint=` | Every Warden line at WARN or ERROR in those logs (the static server's own lines included, the apps' output not) |
| `warden list` and `status` answer in 100 ms (p99) | `warden list`, `warden status <app>`, `warden list --json`, timed every 0.5 s. Samples taken while a supervisor was frozen on purpose are left out (a frozen app costs `list` its 1 s timeout by design), and so are those of an `oom-kill` (until the kernel kills the worker, everything in the full cgroup that allocates, its supervisor too, waits in memory reclaim). A violation names its slowest samples, the fault each fell in and the apps that did not answer |
| An OOM kill is reported as one | `oom-kill`'s checks: `last_exit`, the log line and its hint, the `oom` alert |
| Recycling over `max_memory` is graceful | `memory-recycle`'s checks: replaced in time, no crash counted, no request lost, the log line, the `recycled` alert |
| `warden kill --yes` leaves nothing | No process with the run's `WARDEN_HOME` left `grace_period` + 10 s after it; the memory cgroup removed |

Plus the checks of each fault (exit codes, release pinning, the watchdog
killing a stopped worker, a frozen supervisor or wardend not killing or
restarting anything).

## Output

A summary (faults with recovery p50/p99/max, requests per app and client
with the allowance each failure fell under, WebSocket/SSE endings, CLI
latency, fds and RSS per supervisor and wardend, the invariants), and the
same as JSON in `bench/results/chaos-<time>-seed<S>.json` (also
`bench/results/latest/chaos.json`), with every failure (up to 2000), every
fault and the quiet fd/RSS samples. On a violation: exit 1, the seed and
the command to reproduce, each violation with the supervisor's log lines
around it, and the run directory kept (its path is printed at the start).

The seed fixes the order of the faults, their targets and durations. What
the processes do in between (timing, which worker the kernel picks) is not
reproducible, so a rare race may need a few runs; `--only` narrows them.

## What it found

Each fix has its own commit and a regression test that fails without it.

| # | Found by | Bug | Fix |
|---|---|---|---|
| 1 | `stop-supervisor` | A supervisor frozen longer than `watchdog.timeout` (SIGSTOP, a paused VM, a starved host) killed **every** worker and standby as hung when it resumed: an outage of an app that had served all along. A heartbeat's time is when Warden reads it, and those sent during the freeze were still unread when the first tick judged them | A tick that comes over 1.5 s late moves every heartbeat forward by the stall, so the watchdog counts only silence Warden was awake to hear (`a_frozen_supervisor_does_not_kill_its_workers_as_hung`) |
| 2 | `reload` of `site` | A draining static worker closed its idle keep-alive connections at once: a client sending its next request on one lost it (EOF) | Idle connections stay open through the drain; what arrives gets `Connection: close` (`static_drain_answers_keep_alive_requests_instead_of_cutting_them`) |
| 3 | surge reloads of `api-node` | The same on Node, twice over: `http.Server#close()` closes idle connections at once since Node 19, and the shim swept served idle sockets every 20 ms from the start of the drain | `net.Server#close()` to stop accepting; the sweep starts after `drain_ms` (`node_drain_answers_keep_alive_requests_instead_of_cutting_them`) |
| 4 | the hint invariant | `worker crashed` and `replacement exited before taking over` had a hint only for some causes (not an app's own exit, a hang, not ready in time), and `worker thread crashed`, `worker thread error`, the rollout-restart line and the `tcp_migrate_req` warning (`fix=`) had none | A hint on every one, chosen by the cause (`every_warning_has_a_hint` on three tests' logs, `plain_hints_follow_the_cause`) |
| 5 | `overlap` (reload, then `restart --hard`) | No failed or aborted rollout's ERROR line had a hint | A hint saying what to do next (`an_aborted_rollout_says_what_to_do`) |
| 6 | the first macOS run (CI) | No Node app could listen on macOS: the shim forced `reusePort`, which Node (libuv) has only where the kernel spreads connections, so `listen()` failed with ENOTSUP and every worker crash-looped to FAILED | The shim adds `reusePort` only where Node has it, and Warden warns at start on macOS that one Node worker can hold the port (regression check: a Node app in the launchd checks of `service-managers.yml`, Node 22) |

Harness artifacts found and fixed on the way (not Warden bugs): the
supervisors' own lines went to the `[logging] file` on the tmpfs, which
vanishes with the namespace (now only the apps' output goes there); the
background log rotates with the app's `[logging.rotate]` settings, so a
log flood rotated Warden's lines out of reach of the scan (now 64 MB
before rotating, and a 2000 lines/s budget; the report says if a log
rotated anyway); recovery times included the harness's own pauses; alerts
wardend delivers at the same moment (a crash loop and an OOM kill of the
same death) run the alert rule's `cat >> file; echo >> file` at once, so two
objects can share a line of `alerts.jsonl`, which is now read as a JSON
stream rather than line by line.

The `oom-kill`, `memory-recycle` and NestJS additions found no Warden bug:
OOM kills were reported as such every time (status, log, alert), recycling
was graceful with no request lost. On this 2-CPU box, shared with other
builds at a load of 5, supervisors stalled 2–3 s while spawning workers
(Bun took 1–2.5 s to start instead of 35 ms), which the CLI latency
invariant flags; the CI runners, not shared with other jobs, are the
reference for that (`warden list` p99 4.7 ms there, release build).

## Last run

<!-- chaos:last-run -->
`cargo xtask chaos` (10 minutes), seed **601036295**, 2026-10-01, after the
fixes above: **PASS**, every invariant. 2 CPUs (Xeon @ 2.1 GHz) shared
with the load generator and another process, 8 GB RAM, Linux 6.18, Bun
1.3.13, Node 22.22.2, a debug build of Warden; `tcp_migrate_req = 0`. Raw
numbers: `bench/results/latest/chaos.json`.

82 faults, all recovered within the bound (60 s). Recovery, from the end
of the injection to the whole fleet ready (standbys warmed up included):

| Fault | n | p50 ms | p99 ms | | Fault | n | p50 ms | p99 ms |
|---|---|---|---|---|---|---|---|---|
| kill-worker | 4 | 187 | 191 | | reload | 4 | 17 | 351 |
| kill-standby | 4 | 525 | 535 | | safe-reload | 4 | 21 | 353 |
| kill-supervisor | 4 | 1374 | 1732 | | restart | 5 | 17 | 351 |
| kill-wardend | 4 | 18 | 18 | | restart-hard | 4 | 1661 | 1787 |
| stop-worker | 5 | 18 | 25 | | scale | 5 | 520 | 523 |
| stop-supervisor | 5 | 19 | 21 | | overlap | 4 | 353 | 1056 |
| stop-wardend | 5 | 18 | 19 | | bad-config | 4 | 352 | 353 |
| throw-thread | 4 | 1555 | 1560 | | release-swap | 4 | 354 | 358 |
| crash-loop (after `reset`) | 5 | 187 | 191 | | log-flood | 4 | 17 | 17 |
| disk-full | 4 | 17 | 23 | | | | | |

(After a rollout of `api-bun` the fleet is ready once its standby,
replaced on the new release, has passed its gates: ~350 ms. A killed
supervisor comes back after wardend's 1 s backoff, 2 s for a second death.)

- **Requests**: 274,919 answered; 348 lost, all within the allowances:
  332 while an app was down by design (4 supervisors killed: their workers
  exit with them until wardend restarts them; 4 `restart --hard`; worker-mode
  hosts killed), 15 in flight on or queued for a killed worker, 1 reset of a
  connection queued on a closing listener (`tcp_migrate_req = 0`). None
  during a reload, safe-reload, rolling restart, scale, release swap, bad
  config, log flood, full disk, or a frozen supervisor or wardend.
- **WebSocket/SSE**: 47 connections; 36 ended cleanly (1001 or the last
  event), 4 with their worker killed, 7 still open at the end; none
  broken on a planned restart.
- **CLI** (1146 samples each, 12 left out while a supervisor was frozen):
  `warden list` p50 6.8 ms, p99 34.8 ms; `warden status <app>` 4.0 / 6.6
  ms; `warden list --json` 6.3 / 12.0 ms.
- **Processes**: fds flat for every supervisor and wardend (27, 26, 23,
  22, 17 per app, 25 for wardend); RSS 14–16 MB (debug build), one step of
  ~1.3 MB early in each supervisor's life, then flat. No zombie lasted, no
  orphan, nothing left after `warden kill --yes`.
- **Logs**: 30,876 lines, 101 WARN/ERROR, each with a `hint=`; no panic.
  wardend delivered 32 alerts (crash_loop 7, worker_failed 5, died 4,
  unhealthy 5, rollout_failed 2, unresponsive 1, recovered 8).

The seeds that failed before the fixes pass after them, with the same
faults on the same targets: 2 (3 minutes; bugs 1–4), 3 (4 minutes; bugs
1, 3, 4) and 4 (5 minutes; bug 5).
<!-- /chaos:last-run -->

## On macOS

macOS has no `/proc`: the harness reads processes with `ps` (one call for
the whole table) and `lsof` (fds, a worker's working directory). It leaves
out, and lists at the start and in the report: the pid namespace and the
tmpfs (`disk-full`), memory cgroups (`oom` app, `oom-kill`), `memhog` and
`memory-recycle` (Warden reads workers' RSS from `/proc`), and
`kill-supervisor` (no parent-death signal: the workers of a SIGKILLed
supervisor keep running next to the new ones, as the README's Platforms
section says), and `api-node` (Node has no `reusePort` on macOS, so its 3
workers cannot share the port). Connections are not spread across
`SO_REUSEPORT` listeners there, which changes which worker answers, not
what may fail.

## In CI

`.github/workflows/chaos.yml` runs `cargo xtask chaos --release --minutes 3
--seed 20261001` on pushes to `main` and once a day (not on every branch:
macOS minutes count ten times against the account's spending limit): on
Ubuntu as root (pid namespace, tmpfs, memory cgroup: everything), and on
macOS. The fixed seed means the same faults in the same order on the same
targets, so a new failure points at a change in Warden. The verdict,
recovery per fault and the request counts are notice annotations, each
violation an error annotation (job logs need a login to read; annotations
don't), and the JSON report is an artifact. "Run workflow" takes other
minutes and seeds.

The first CI runs, 2026-10-01, on 2-vCPU runners, release build:

- **Linux**: PASS, every invariant. 24 faults, every kind (`oom-kill` and
  `memory-recycle` included: the memory cgroup could be made under the
  cgroup v2 root), all recovered; 134,679 requests answered, 76 lost within
  the allowances, none outside them; 26 WebSocket/SSE sessions, none
  broken; `warden list` p50 2.8 ms, p99 4.7 ms; alerts: `oom` 1,
  `recycled` 1, and the rest as expected.
- **macOS**: 25 faults of the 17 kinds macOS runs, all recovered; 17,581
  requests answered, 16 lost within the allowances, none outside them;
  `warden list` p99 18.7 ms. It found the shim bug (#6 above), and on the
  way two harness artifacts: the run directory in macOS's long `$TMPDIR`
  (socket paths over the limit) and `/tmp` vs `/private/tmp` in the
  release-pinning check (the harness now uses real paths).

## Not covered

- Latency under real load: this is a correctness soak; the load is steady,
  not a benchmark (`cargo xtask bench` measures that).
- `tcp_migrate_req = 1`: the harness does not change host settings; with 1
  the allowance for queued resets is off.
- systemd: supervisors run in the background under wardend, not as units;
  `Type=notify`, `WatchdogSec=` and `KillMode=mixed` are exercised by
  `.github/workflows/service-managers.yml` instead (`warden startup` on real
  systemd and launchd), not under faults.
- `max_lifetime` recycling, health-based replacement on purpose.
- A wardend killed *while* a supervisor is dead: faults run one at a time,
  so wardend is always back before the next supervisor dies.
- An app's memory cgroup after wardend restarts its supervisor: the new
  supervisor lands in wardend's cgroup (so `kill-supervisor` skips `oom`).

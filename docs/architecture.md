# Warden architecture

Warden is a small Rust supervisor that runs N copies of a Bun (or Node) HTTP
application on one machine. systemd supervises Warden; Warden supervises the
application workers; the Linux kernel spreads connections across workers with
`SO_REUSEPORT`. Warden never sits on the request path.

```
Cloudflare → Nginx → 127.0.0.1:3000 ──kernel SO_REUSEPORT──► worker 1..N
                                                              ▲
systemd → warden (spawn, watch, restart, drain, report) ──────┘
```

This document records what was verified experimentally before the design was
fixed (PRD §25), the Bun limitations found, and the resulting design.

## 1. Experimental findings

Environment: Bun 1.3.13, Node 22.22, NestJS 12.1, Linux 6.18 x86_64,
2 vCPU / 8 GB container. Scripts are in [`research/`](../research); every claim
below comes from running them, not from documentation.

| # | Question | Result |
|---|---|---|
| F1 | Can N Bun **processes** share a port with `Bun.serve({ reusePort: true })`? | **Yes.** 4 processes on :3100; 400 fresh connections landed 99/111/93/97. Without `reusePort` the 2nd bind fails with `EADDRINUSE`. |
| F2 | What happens to traffic when one of 4 processes dies under load? | A non‑retrying keep‑alive client (Node `http.Agent`) saw **15 `ECONNRESET` / 53k** on SIGKILL and **14 / 50k** on Bun's default SIGTERM (Bun exits immediately). Bun's own `fetch` silently retried and hid the failures. |
| F3 | Does a graceful drain fix F2? | `server.stop()` + immediate exit still gave **11 / 50k** (keep‑alive race). Stop the listener, answer with `Connection: close` and keep serving for ~200 ms before exit → **0 / 49k**. |
| F4 | Can N Bun **Workers** in one process each run `Bun.serve` on the same port? | **Yes.** 4 Workers, 4 listeners owned by one pid, fair distribution (57/39/57/47 of 200). |
| F5 | Do Workers give real CPU parallelism? | **Yes.** `/cpu` (20 ms busy loop): 1 Worker 49 rps, 4 Workers 131 rps, 4 processes 132 rps. |
| F6 | Memory, hello‑world server | 4 processes **146 MB** RSS total vs 1 process × 4 Workers **57 MB**. NestJS: 2 Workers in one process 121 MB. (Real apps differ; see [benchmarks](benchmarks.md).) |
| F7 | What happens when a Worker dies (uncaught throw, `process.exit`)? | The Worker closes (`close` event, code 1 / exit code) **but Bun does not close its listening socket.** The socket stays in the reuseport group and nothing accepts on it: **26 % of new connections timed out** after one dead Worker, 44 % after two. `worker.terminate()` from the host leaks the same way (24 %). |
| F8 | Can the leak be avoided? | **Yes, from inside the Worker.** If the Worker calls `server.stop()` before exiting — explicitly, or from a `process.on("exit")` hook, which Bun *does* run on an uncaught‑error death — the listener closes and timeouts drop to **0**. |
| F9 | Does Bun's `node:http` honour `listen({ reusePort: true })`? | **No.** Bun 1.3.13 hard‑codes `reusePort = false` unless the process is a `node:cluster` worker (`NODE_UNIQUE_ID` set) — see `src/js/node/_http_server.ts`. The 2nd process fails with `EADDRINUSE`. Node 22.22 honours it. |
| F10 | Can `node:http` / NestJS share a port under Bun anyway? | **Yes, with a shim.** Bun's `node:http` calls the global `Bun.serve` at listen time; wrapping `Bun.serve` in a `--preload` to force `reusePort: true` made 2 plain `node:http` processes, 2 NestJS processes, and 2 NestJS **Workers** in one process all share the port. Without it NestJS fails with `EADDRINUSE`. |
| F11 | Signals | Bun's default on SIGTERM is immediate exit (143). `process.on("SIGTERM")` works on the main thread only; **Workers never receive process signals**, so shutdown must be relayed to them by message. |
| F12 | Inherited fds / Worker env | A Bun process can write JSON lines to an inherited fd 3. `new Worker(url, { env })` gives each Worker its own `process.env`. |
| F13 | Can one Bun process serve the same app on a second, private Unix socket? | **Yes**, for `Bun.serve` fetch apps, plain `node:http` and NestJS: calling the original `Bun.serve` with the app's own options object plus `unix: <path>` gives a second listener with the same handler. This is how Warden health-checks one specific worker. |
| F14 | Worker‑mode reload under load | 9–15 resets per ~100 k requests with `tcp_migrate_req = 0`, **0** with `1` (3 runs). Same for fresh-connection clients in process mode (~1 reset per replaced worker → 0). |
| F15 | Can an app start completely but listen later (hot standby)? | **Yes, from the shim.** `Bun.serve`: start the app's options on a private Unix socket or an ephemeral 127.0.0.1 port (a stand-in) and hand the app a Proxy whose target becomes the real server later; WebSocket `publish`, `subscriberCount`, `stop()` through the app's reference reach the real server. Bun's `node:http` **rejects a Proxy** ("The "server" argument must be of type bun.Server"): it gets the stand-in itself, keeps it under `Symbol(::bunternal::)` and emits `listening` through `EventEmitter.prototype.emit`, where the shim finds it; swapping the symbol to the real server makes `address()` and `close()` act on it. Node: `listen()` is recorded and replayed. Measured promote → port answering: Bun.serve 0.7–1.5 ms, node:http on Bun 1.4 ms, NestJS on Bun 2 ms, node:http on Node **9–13 ms** (first `listen` compiles and loads its path) → **2–3 ms** with a warm-up listen on an ephemeral port during standby, NestJS on Node 2–3 ms. `fs.read` on fd 3 (a blocking socket) runs on the thread pool in both runtimes; the event loop keeps running (49 timer ticks during a 500 ms read). But Node joins its pool's threads at exit, so a standby's `process.exit()` waited for that read until SIGKILL: Node reads fd 3 through a `net.Socket` (the event loop) instead; Bun keeps `fs.read` (its `net.Socket` can't take an fd, and its exit doesn't wait). Warden also shuts down its end when it stops a standby (EOF). |

Note: the sandbox these probes first ran in exported `BUN_OPTIONS=--smol`;
F5/F6 were re-measured without it (numbers above). The benchmark harness
strips it explicitly.

Kernel note: `net.ipv4.tcp_migrate_req` was `0`. With it set to `1`
(Linux ≥ 5.14), connections still in a closing listener's accept queue are
migrated to another listener in the group instead of being reset. Warden's
drain makes this rarely matter, but it is a cheap production hardening.

## 2. Bun limitations that shape the design

1. **`node:http` ignores `reusePort`** (F9). Any Express/Fastify/NestJS app under
   Bun needs the Warden shim (or per‑worker ports) to run more than one copy.
2. **A dead Worker leaks its listener** (F7). In worker mode an unplanned Worker
   death silently black‑holes 1/N of new connections unless the listener is
   closed from inside the Worker (F8). Warden's shim installs that hook, and
   any Worker death makes Warden replace the whole host process (new host
   ready first, then the old one drained), so a leak can't outlive the host.
3. **Bun exits immediately on SIGTERM** (F11). Without a handler, in‑flight
   requests are cut. The shim installs a drain handler.
4. **Workers don't see signals** (F11). The worker‑mode host relays shutdown.
5. `Bun.serve` cannot adopt an inherited listening fd, so Warden cannot own the
   socket and hand it down (the systemd socket‑activation model). Each worker
   binds its own `SO_REUSEPORT` socket; Warden only configures the port.

## 3. Watt, and what we borrow

Studied `@platformatic/runtime` 3.71.0 (details in `research/watt-findings.md`).

| Watt | Warden |
|---|---|
| App instances in `worker_threads` of one Node process | `process` mode (default) and Bun `worker` mode; both explicit, never conflated |
| Injects `reusePort` into every `listen()` via `diagnostics_channel` | Injects `reusePort` by wrapping `Bun.serve` in a preload shim (same idea, Bun mechanism) |
| No reuseport (macOS) → entrypoint forced to 1 worker, or `port + index` | `port_strategy = "offset"`: worker *i* gets `PORT = port + i - 1` |
| Health = ELU / heap polled from thread handles | Per-worker HTTP check over a private socket + event-loop heartbeat (watchdog). Bun exposes no per‑Worker ELU/heap to the host |
| Unhealthy worker is replaced (new first, then old stopped) | Same, through the rollout gates |
| Crash restart: 5 s delay (immediate in prod), no backoff, no cap | Exponential backoff + restart cap per window → `FAILED`, retried after `failed_cooldown` (PRD §10) |
| Graceful stop: `Connection: close` + server close, 10 s per worker, 30 s total | Same pattern (F3), in the shim; supervisor enforces `grace_period` then SIGKILL |
| Rolling restart on SIGUSR2: new worker alongside old, then stop old | Same, plus health gates, canary soak, preflight and rollback (`warden safe-reload`) |
| Prometheus on :9090, merged from workers via ITC | Optional Prometheus endpoint with supervisor‑side metrics from `/proc` |
| Mesh, ITC, shared cache, scheduler, autoscaler | **Not borrowed** (non‑goals) |

## 4. Design

### 4.1 Processes and responsibilities

- **systemd**: boot, top‑level restart, resource limits, user/group.
- **warden** (one process, single‑threaded tokio runtime): spawns workers,
  tracks state, restarts with backoff, drains on shutdown, answers the CLI over
  a Unix socket, optionally serves `/metrics`. Never proxies requests.
- **Workers**: the application. Each binds the shared port itself.
- **wardend** (`warden daemon`, optional, one per host and user): one socket
  for every app's live events and commands (the CLI's `warden events`,
  `warden-gui`), and a second level of supervision: it restarts supervisors
  that `warden start` launched in the background when they die (backoff, give
  up after 10 deaths in 10 min), and reports hung ones without killing them
  (their workers die with them). It holds no workers and no state an app
  needs: killing it stops nothing. Under systemd every app is its own unit and
  wardend only watches; under launchd, in containers and for background apps
  it also resurrects the saved apps (once per boot). Protocol:
  [`protocol.md`](protocol.md).

### 4.2 Modes

**process** (default) — N OS processes, each running `command args…`.
If worker 2 crashes, workers 1, 3, 4 keep serving; only worker 2 is restarted.

**worker** (Bun only) — one Bun process runs Warden's host script, which starts
N `Worker`s, each importing the app `entry`. Lower memory (F6), but:
if the Bun process itself crashes (native crash, OOM, `process.abort`), **all N
workers die together** and Warden restarts the whole process. When a single
Worker dies (uncaught error, `process.exit`), the shim's exit hook closes its
listener (F8) and the host keeps serving with N−1 Workers while Warden brings
up a replacement host (with backoff), waits until all its Workers listen and
pass the gates, then drains the old host.

### 4.3 The shim (`shim/warden-shim.mjs`, embedded in the binary)

Loaded with `bun --preload` in process mode, and imported first by every Worker
in worker mode. Enabled by default when `command` is `bun`. It:

1. wraps `Bun.serve` to force `reusePort: true` for TCP servers (fixes F9/F10 for
   `node:http`, Express, NestJS);
2. serves the same handler on a **private Unix socket** for that worker only
   (F13), so health checks reach exactly one worker;
3. reports `listening` (+ the private socket) and a 1 s **heartbeat** from the
   event loop to Warden (fd 3 in process mode, `postMessage` → host → fd 3 in
   worker mode) — readiness and watchdog signals. The heartbeat carries the
   interval's **event-loop delay** (`"loop":{"p50":…,"p99":…,"max":…}`, ms):
   `perf_hooks.monitorEventLoopDelay` (Node and Bun) at a 100 ms resolution,
   a native timer, read and reset once per heartbeat — no work per request,
   no measurable idle CPU in Node, ~0.05 % in Bun. Node's samples include
   the resolution and are corrected; where the histogram is missing or stays
   empty the heartbeat's own lateness stands in. A block of 100 ms or more
   always shows; shorter ones when a sample falls in them. The heartbeat is
   sent whether or not the watchdog is on (`WARDEN_HEARTBEAT_MS` is always
   set); `warden list` (Loop p99), `status --json` (`loop_delay`), the GUI
   and `warden_worker_event_loop_delay_*_seconds` show it, and
   `[watchdog] loop_delay_warn` warns when p99 stays high for 10 s;
4. on SIGTERM (process mode) or a `shutdown` message (worker mode) drains:
   stop listeners, add `Connection: close` (Bun `fetch` handlers and
   `node:http` responses), wait `drain_ms` and for in‑flight requests, exit 0.
   The app's own SIGTERM handlers (NestJS `enableShutdownHooks`) are deferred
   until the drain is done — otherwise Nest closes every connection at once
   (39 resets per reload measured) — and still run afterwards;
5. in Workers, closes all listeners from a `process.on("exit")` hook (F8);
6. in a hot standby (`WARDEN_STANDBY=1`, §4.5a), holds back the app's listen
   on its port until Warden sends `promote` on fd 3 (F15).

fd 3 is a Unix socketpair (`sys::socketpair_cloexec`): the worker writes its
reports as JSON lines; Warden writes to it only to promote a standby, and
only a standby's shim reads it.

Apps that are not Bun (Node) run without the shim: they must pass
`reusePort: true` themselves (Node ≥ 22.12) or use `port_strategy = "offset"`.

### 4.4 Worker lifecycle

```
STARTING ─ready─► RUNNING ─stop─► STOPPING ─► STOPPED
   │                 │
   └──exit──► CRASHED ─► RESTARTING (backoff) ─► STARTING
                 └── too many restarts in window ─► FAILED
```

`READY` is the transition event into `RUNNING`, logged as `worker=N ready`.
Readiness sources, first one wins: the shim's `listening` message; on Linux, the
worker pid owning a `LISTEN` socket on the configured port (`/proc/<pid>/fd` ×
`/proc/net/tcp{,6}`); without a port, the spawn itself. A worker that is not
ready within `ready_timeout` is killed and counted as a crash. FAILED workers
are retried after `failed_cooldown` (default 300 s).

Every death gets a reason (`last_exit`, the `crashed` event's detail, and the
log line, with a `hint=`), not just the wait status. The task that owns each
worker records the signals it delivered, so a SIGKILL is either *Warden's*
(grace period over, hung, not ready in time) or *another process's*; an exit
after Warden's stop signal is a normal stop. A SIGKILL Warden did not send
is the kernel's *OOM killer* when the `oom_kill` counter of Warden's cgroup
(cgroup v2 `memory.events`, or v1 `memory.oom_control`, found through
`/proc/self/cgroup` and `/proc/self/mountinfo`) rose in the 2 s before the
death: Warden reads it every second (the tick) and at each death, stamping
each rise with when it was seen. Everything in the cgroup shares the
counter, so each SIGKILL death takes at most one recent rise; an older one
was something else's (a helper process, a `verify_command`) and is
forgotten. A SIGKILL Warden sent stays Warden's even if the counter moved
meanwhile. SIGSEGV, SIGBUS, SIGILL, SIGFPE, SIGABRT… are *crashes*, named.
Without a readable counter (macOS, no memory controller) an OOM kill reads as
another process's SIGKILL, and the hint says it may be either.

### 4.5 Restart protection

Per worker: every unexpected exit is a crash. The first crash after a healthy
run restarts at once (a one-off crash costs only the app's startup time, as in
PM2); crash n ≥ 2 in a row waits
`min(backoff_initial × 2^(n−2), backoff_max)`. The consecutive counter
resets after a worker stays up for `restart_window` seconds. More than
`max_restarts` restarts inside `restart_window` → `FAILED`; retried after
`failed_cooldown` (if restarts are enabled), or cleared by `warden restart <id>`
(process mode) / `warden reload` (worker mode).

### 4.5a Hot standbys (`[workers] standby = N`, `src/supervisor/standby.rs`)

A restart costs the runtime's and the app's startup (Bun 47 ms, node:http
113 ms, NestJS on Node ~800 ms). A standby pays it before the crash:

```
spawn (WARDEN_STANDBY=1) ─► standby_ready ─► gates ─► STANDBY ─promote─► listening ─► RUNNING in slot k
  (WARMING: app initialized,   (health_passes,                  (fd 3)    (~1-3 ms)
   listen held back, F15)       verify_command)
```

- **Pool.** N instances outside the slots (`Role::Standby`, slot 0), started
  once no worker is starting (they would compete for the CPU). `status`
  lists them in `standbys` (`WARMING`, `STANDBY`; additive, so older
  clients just don't show them), the GUI counts their memory and CPU, and
  Prometheus gets `warden_standbys_*`. Standbys are numbered 1..=N (a new
  one takes the lowest number no live one has): `s1`, `s2`… in `warden
  status`, `worker=s1` on their log lines and output, so `warden logs
  --worker s1` follows one and `--worker standby` all of them with the
  pool's own lines (`worker=standby`). A Bun standby's stand-in server
  serves on its private health socket when there is one, else on an
  ephemeral 127.0.0.1 port: no socket path of its own, so no length limit.
- **Gates.** Promotable only after `standby_ready` (initialized: the app
  called `Bun.serve`/`listen` on its port) and what a rollout's new worker
  must pass before taking traffic: `reload.health_passes` checks on its
  private socket (the stand-in serves the app there) and
  `reload.verify_command`. Idle standbys get the liveness checks and the
  watchdog; a failing one is replaced, never promoted. A standby found
  listening on the port before promotion (an app that listens in a way the
  shim does not hold back) turns standbys off with an error.
- **Promotion.** On a crash, the slot goes through `on_slot_crash` as
  always (counted, backed off, FAILED after `max_restarts`); when its
  restart is due, an available standby is told
  `{"cmd":"promote","worker":k,"count":n}` instead of spawning a process. The
  shim listens for real and takes worker id k (`WARDEN_WORKER_ID`,
  `NODE_APP_INSTANCE`, `process.emit("warden:promote")`), Warden relabels its
  output `worker=k` and gives it `PROMOTE_TIMEOUT` (5 s) to listen. Events:
  `starting` and `ready` of worker k with detail `promoted from standby`; log
  `worker promoted from standby worker=k pid=… standby=sN promote_ms=…`. A new standby
  starts once the promoted one listens.
- **Crash loops.** Standby exits (and failed gates) share one restart
  tracker with the slots' policy: backoff, then FAILED (a `FAILED` row) until
  `failed_cooldown` or `warden reset`; crashed workers restart cold meanwhile.
- **Deploys.** A standby runs the code it started with. While a reload,
  safe-reload or restart runs, standbys are neither promoted nor started
  (a crash restarts cold, on the new code like before). When it succeeds,
  every standby is stopped and fresh ones go through the gates; when it fails
  or rolls back they stay, matching the workers that kept the previous
  version. Recycling (`Kind::Replace`: memory, lifetime, health, hang) keeps
  the code, so an available standby is the replacement: it listens next to
  the old worker, passes the remaining gates, then the old one drains.
  A standby's instance number (`instance_var`) is past the workers' and no
  other standby's; `scale` up replaces a standby whose number a new worker
  takes (it would run instance-specific code twice until promoted), and
  otherwise changes only the slots. With `[reload] surge` a batch of new
  workers starts next to the old ones; the standbys stay out of it (they
  are paused, and kept if the batch rolls back), so a surge's peak memory
  is 2 × batch + standbys.
- **Release pinning.** A standby starts in the release workers start in
  (`[app] pin_release`: pinned command, arguments and directory) and
  records it; it is promoted only while that is still the workers' release.
  A deploy re-pins at its start (standbys paused), so after a successful one
  they are replaced in the new release, and after a rollback the pin and the
  standbys are the previous ones again. If the pin moves otherwise (the
  pinned directory was deleted), standbys on the old one are replaced.
- **Memory.** A standby is a fully initialized app without a listener: about
  one idle worker (measured idle RSS, worker vs standby: Bun fixture 42.9 /
  42.4 MB, Node fixture 57.6 / 56.5 MB, NestJS on Bun 85.3 / 84.0 MB, NestJS
  on Node 99.9 / 102.8 MB), not less. Much of it is the runtime's pages,
  shared with the workers: in the benchmark harness (Bun.serve app, 2
  workers) one standby added 41 MB RSS but 11 MB PSS. It does not grow with
  traffic as workers do. Measured there too: crash recovery 56 ms cold, 17 ms
  with the standby (debug build of Warden), with no extra failed requests.
- Process mode only, with `app.port`, `port_strategy = "shared"`, the shim
  and captured output (a `direct` output file could not follow the standby
  into its slot); config validation says so.

### 4.6 Shutdown (SIGTERM / SIGINT from systemd)

1. Mark shutting down; stop restarting; notify systemd `STOPPING=1`.
2. SIGTERM every worker's process group (worker mode: host relays `shutdown`
   to each Worker). The shim drains (F3).
3. Wait up to `grace_period`; SIGKILL the process group of whatever is left;
   exit 0. (Signalling the group, like supervisord's `stopasgroup`, keeps
   `bun run <script>` wrappers and app-spawned helpers from being orphaned;
   when a worker's leader exits, whatever is left in its group is SIGKILLed.)

A second SIGTERM/SIGINT skips the wait. Recommended unit uses `KillMode=mixed`
so only Warden gets the first SIGTERM and orchestrates the drain.

### 4.7 Rollouts: reload, safe-reload, restart N, recycling

Replacing a running worker falls out of `SO_REUSEPORT`: start the new process
next to the old one, and only drain the old one once the new one has proven
itself. Every replacement goes through the same gates
(`src/supervisor/rollout.rs`), borrowing Kubernetes' readiness probe +
`minReadySeconds` + `progressDeadlineSeconds`:

```
new process ─► listening ─► N health passes on ─► verify_command ─► soak ─► promote:
 (old keeps      (/proc or     its private socket    exits 0         (min_ready,   old worker
  serving)        shim)                                               canary_soak)  drains
```

Because the old worker serves until *promote*, a failure at any gate (crash,
timeout, failing check, failing command) is a free **rollback**: the new
process is stopped and nothing else changes. `reload.timeout` bounds each
worker.

- `warden reload` (also SIGHUP): every worker, one at a time, through the gates.
- `warden safe-reload` — the production deploy command:
  1. **preflight** (like `nginx -t`): re-read and validate the config (applying
     `[app]`, `[reload]`, `[limits]`, … and reporting sections that need a
     Warden restart), check the working directory, command and entry script
     exist, run the optional `reload.preflight` command. Any failure: nothing
     is touched;
  2. refuse to start if any worker is not RUNNING or is failing health checks
     (don't deploy onto a broken fleet);
  3. **canary**: the first replacement soaks `canary_soak` seconds next to the
     worker it replaces, taking 1/(N+1) of new connections while its health is
     watched. Failure → rollback, every worker still on the previous version;
  4. the rest one at a time with the same gates and an optional `pause`;
     **halt on the first failure** (reporting how many workers were replaced).
- `warden restart N`: one worker through the gates.
- Recycling (health, memory, lifetime, hang) queues one-worker rollouts.

The CLI blocks until the rollout finishes, prints its phases, and exits 1 on
failure — usable as systemd `ExecReload=` or in a deploy script. With
`port_strategy = "offset"` workers can't overlap, so each is stopped and then
started. In worker mode the unit is the whole host process.

**Surge** (`[reload] surge = N | "all"`, like Kubernetes' `maxSurge`; default
1): reload, safe-reload and restart replace workers in batches of N. Each worker
of a batch starts next to the one it replaces and goes through the gates on
its own; when all have passed, the batch's old workers drain together, and the
next batch starts once they have exited. A failure in any of them stops every
new worker of the batch (a free rollback: the old ones still serve), so a
batch is all-or-nothing. safe-reload's canary is still a batch of one with its
soak, then the rest go N at a time. A slot that isn't serving (starting,
stopping, down) is a batch of its own, since it has no old worker to fall back
on. Memory: surge N runs up to N extra workers while a batch overlaps (`"all"`:
twice the fleet for a few seconds). It requires workers that can overlap, so
config validation refuses it with `port_strategy = "offset"`. Status shows the
batch as `workers 1, 2: health checks 1/3`, from its slowest worker; `done`
counts workers whose old process has exited.

**Release pinning** (`[app] pin_release`, default on;
`src/supervisor/release.rs`): deploys swap a `current` symlink, then reload.
Warden resolves `working_directory` to its real path when it starts (also
`start` after `stop`, `restart --hard`) and when a reload, safe-reload or
rolling restart of every worker begins, and starts workers there; `command`,
`args`, `entry` and `static.root` that go through the same symlink (the
deepest one on the `working_directory` path) are rewritten to match. Crash
restarts, recycling (health, memory, lifetime) and `restart N` of one worker
reuse the pin, so a crash between the swap and the reload doesn't bring up the
new release next to the old one. A failed rollout restores the previous pin
with the previous config. If the pinned directory has been deleted, the next
start re-resolves `current` and logs a warning (the worker may then run another
release than the rest until a reload). `status` (`release`), `describe` and
each rollout's start line show the pin.

With a health path configured, gates are mandatory: a worker that reports no
private socket fails them instead of skipping them (and a config whose socket
paths would exceed the Unix limit is rejected at start). If the old worker dies
while its replacement is being verified, the replacement takes over the slot;
if it then fails, it is stopped and the slot restarted through crash handling.
On any failure the config in effect before the rollout is restored.

What a canary cannot undo: once workers are promoted, their old processes are
gone. A failure later in the rollout leaves a mixed fleet (reported); roll
back by redeploying the previous release and running `safe-reload` again.

### 4.8 Health checks

`GET health.path` from Warden, never proxying traffic, over each worker's
**private Unix socket** (F13) — so the answer comes from that worker, not from
whichever worker the kernel picks on the shared port. Every `interval` seconds;
after `failure_threshold` consecutive failures the worker is marked unhealthy
and, with `on_failure = "replace"` (default), replaced through the gates like
a Kubernetes liveness probe (new one first, zero downtime).

`health.url` adds an optional **app-level** check through the shared port,
exported as `warden_app_healthy`; with `on_failure = "reload"` a failing app
check triggers a rolling reload. Apps without the shim (Node) only get the
app-level check; their rollout gates fall back to it too.

### 4.8a Staying up for weeks (`src/supervisor/upkeep.rs`)

| Mechanism | Inspired by | Behaviour |
|---|---|---|
| Watchdog | systemd `WatchdogSec`, gunicorn `timeout` | The shim's heartbeat comes from the event loop; no heartbeat for `watchdog.timeout` s = hung. Process mode: SIGKILL, restarted by crash handling. Worker mode: replacement host first, then SIGKILL of the old one (a hung Worker's listener would black-hole connections). |
| Per-worker liveness | Kubernetes liveness probe | See 4.8. |
| `limits.max_memory` | PM2 `max_memory_restart` | 3 samples (15 s) over the limit → graceful replacement. |
| `limits.max_lifetime` | gunicorn `max_requests` + jitter | Recycle each worker after its lifetime ±10 %, so workers started together don't recycle together. |
| `restart.failed_cooldown` | Kubernetes CrashLoopBackOff | A FAILED worker is retried after the cooldown instead of needing a human after a transient outage. |
| Restart intensity | Erlang/OTP supervisor intensity/period | `max_restarts` in `restart_window` with exponential backoff. |
| Readiness on fd 3 | s6 `notification-fd` | Plus `/proc` listener detection for apps without the shim. |

### 4.9 Observability

- Logs: one line per event on stdout, `ts LEVEL message key=value…`; worker
  stdout/stderr are prefixed `worker=N`. Timestamps are dropped under journald
  (`JOURNAL_STREAM` set), which adds its own. The last 1000 lines are kept in
  memory for `warden logs`. Worker output is read in 64 KB batches (one
  timestamp, one queue message, one lock per read), limited by
  `max_lines_per_sec`, and written to rotated files. With `worker_output =
  "direct"` the bytes go from the worker's pipe into its log file with
  `splice(2)`: no copy through Warden, no parsing, rotation at a line
  boundary (supervisor CPU per 200 MB: 0.72 s captured, 0.14 s direct).
  `warden flush` (like `pm2 flush`) empties the in-memory rings and every
  current log file, each on the thread that writes it, between two writes:
  the writer thread's files are `O_APPEND` and hold whole lines only, so a
  message in its queue truncates them through its own descriptors and the
  next line lands at offset 0 (lines queued before the flush go too); the
  output thread resets each direct file's write offset, keeping the start of
  a line a worker is in the middle of, so both kinds of file begin with a
  whole line. Files no running worker writes (a scaled-down worker's, an
  earlier run's `out-3.log`) are emptied by name; rotated ones are kept.
- History (`warden logs --history`, `warden search`, `src/logview.rs`):
  the CLI reads the files itself, so it works while the app is stopped.
  `logview::sources` picks them: in capture mode `[logging] file` has
  every line tagged with its worker and stream and answers alone
  (`--out`/`--err` read the out/err files); in direct mode Warden's file
  (or journald) has the events and the out/err files the output, so both
  are read. Per-worker files are found on disk by name (`out-<label>.log`
  for a worker number, `sN` or `host`, current or rotated), so gone
  workers' are read too; each file comes with its rotated and gzipped
  ones, oldest first, workers in order. Lines of several files are
  labelled `worker=N stream:`, and `--lines N` is per file (as `pm2 logs`).
- Live events: every worker state change, rollout phase and (on request) log
  line is pushed to `subscribe` clients as it happens, with a full status
  every interval; with no subscriber an event costs one atomic load.
- Metrics (`warden status`, optional Prometheus endpoint): workers configured /
  running, restarts, crashes, uptime, RSS and CPU per worker from `/proc`,
  health status.
- systemd: `READY=1` once all workers are ready (for `Type=notify`),
  `STOPPING=1`, `RELOADING=1`.

### 4.10 Control plane

Each supervisor answers on its own Unix socket (mode 0600, in a private
runtime directory), one JSON request and response per line: `status`,
`stop`, `shutdown`, `start`, `restart`, `reload`, `scale`, `reset`,
`signal`, `config`, `flush`, `logs [-f]`, `log-level`, and `subscribe`
(a stream of events). The CLI talks to each app directly (`warden list`
~3 ms for 10 apps), so it never depends on wardend; `wardend.sock` adds
one place to watch and drive every app. Both are specified in
[`protocol.md`](protocol.md), with the wire types in the `warden-protocol`
crate (`protocol/`, re-exported from `src/control.rs` and `src/events.rs`).

### 4.11 Security

Warden needs no privileges and performs none: workers inherit Warden's user,
group, environment and working directory; `env_file`, then `env`, then
Warden's own variables go on top, each winning over the ones before (one
function, `worker_env`, builds them for spawning and for `warden env`; the
list is in the README, "Environment variables"). Run it as
the service user from systemd. The runtime directory (control socket, the shim
every worker preloads, per-worker health sockets) must be owned by Warden's
user, not a symlink, and not group/world-writable, or Warden refuses to start;
the JS files are written with `O_EXCL` and renamed into place. The config file
is trusted input (`verify_command` / `preflight` run via `sh -c`).

## 5. Scope as built

Phase 1–2 (process mode) is the production path: spawn, readiness, restart with
backoff, drain, gated rollouts (reload / safe-reload), self-healing, CLI,
metrics, health. Phase 3 (worker mode) ships as experimental: it depends on the
shim to paper over F7/F9 and on the host process never crashing natively.
[Benchmarks](benchmarks.md): worker mode halves memory with equal throughput,
but costs fault isolation and (for NestJS) p99 latency.

## 6. Open questions / next steps

- NestJS inside Bun Workers works for a minimal app (F10); the real app's
  dependencies (Prisma/pg drivers, native modules) must be tested in Workers.
- `surge = "all"` is blue/green for one batch, but a deploy is still not
  reversible once a batch has been promoted: keeping the old workers stopped
  but restartable (or SIGSTOPped) until the whole rollout passed would be.
- Error-rate gates need request metrics Warden doesn't see (it isn't a proxy);
  `verify_command` is the hook for app-specific smoke tests today.
- The native GUI ([`gui/`](../gui/README.md), iced) runs as a separate process
  on `wardend.sock`, locally or through an SSH tunnel, and shares the
  `warden-protocol` crate with the daemon. Next: a resource history (charts),
  alerts, several hosts in one window.
- wardend features beyond this round: alerts (webhook/command), in-memory
  resource history, port registry, start order, fleet deploys with release
  pinning, audit log (peer uid), self-upgrade by re-exec.

# How Warden works

A small Rust supervisor that runs N copies of a Bun (or Node) HTTP app on one
machine, with zero-downtime deploys. systemd supervises Warden, Warden
supervises the workers, and the Linux kernel spreads connections across them
with `SO_REUSEPORT`. **Warden never sits on the request path.**

```
Cloudflare → Nginx → 127.0.0.1:3000 ──kernel SO_REUSEPORT──► worker 1..N  (Bun / NestJS)
                                                              ▲
systemd → warden  (spawn · watch · restart · drain · gate · report)
```

It replaces "4 systemd units / PM2 + an nginx upstream list" with one service,
one config file and one port. It is inspired by [Platformatic Watt](https://github.com/platformatic/platformatic)'s
worker model, but it is a single binary of about 5 MB that embeds no JavaScript runtime.

- **Process mode** (default, production): N Bun processes. One crash affects one worker.
- **Worker mode** (opt-in, Linux, Bun 1.4+): one Bun process running N `Worker` threads, 14-22 % less memory
  and as fast as processes for NestJS on Bun 1.4. If that process crashes, all workers go down together;
  on macOS the threads don't share the load, and before Bun 1.4 they ran 20-50 % slower (Warden warns).

This page is the short version. The long one, with the experiments behind
every decision, is [`architecture.md`](architecture.md).

## Why these design choices

Everything below was checked experimentally before it was designed. The details
are in [`architecture.md`](architecture.md) (findings F1–F14) and
[`research/`](../research).

- **Bun's `node:http` ignores `listen({ reusePort: true })`**, so NestJS/Express
  on Bun can't share a port. Warden's embedded shim wraps `Bun.serve` and fixes
  this without app changes.
- **Bun exits instantly on SIGTERM.** In testing, restarts dropped about 15 of
  every 50k requests. The shim drains first (closes the listener, sends
  `Connection: close`, waits), and reloads then dropped **0**.
- **A crashed Bun Worker leaks its listening socket.** About 1 in 4 new
  connections then silently hang. The shim closes the socket from inside the
  dying Worker, and Warden replaces the host.
- **Health checks through a shared port hit a random worker.** The shim gives
  each worker a private Unix socket, so Warden can check *that* worker.
- **Each worker makes its own TLS session-ticket key**, so a returning
  visitor's ticket only resumes on the worker that issued it: 1 in N with N
  workers, the rest paying for a full handshake (about a third more server
  CPU under Node, a quarter under Bun). Warden gives all of an app's workers
  one key, made when its supervisor starts and handed to each worker on fd 3
  before it runs (never in its environment), and the shim sets it on the
  app's Node `https`/`http2` servers. The key moves forward every 12 hours
  by a one-way step, so no worker holds an earlier period's key. An app that
  sets its own `ticketKeys` keeps them. Bun.serve ignores `ticketKeys` (Bun
  1.4), so Bun apps keep one key per worker for now.

## Limitations

- Linux is authoritative. macOS works for development; see
  [platforms.md](platforms.md) for what it lacks.
- Worker mode needs Bun and the shim.
- Node apps share the port through Warden's shim (`--import`, Node ≥ 22.12
  for `reusePort`); older Node needs `port_strategy = "offset"`.
- Outside a drain the shim adds no work to a request: a `Bun.serve` app's
  own fetch handler answers it, and on Node it keeps one entry per open
  connection, nothing per request. When a drain starts, each `Bun.serve`
  server gets handlers that add `Connection: close` through
  `server.reload()`: `fetch`, every function in `routes` (a static
  `Response` there becomes a function answering with a copy of it) and
  `error`; the shim intercepts `reload()` on Bun's server prototype, so an
  app's own `server.reload()` still works (and a drain never brings back a
  handler the app replaced). A Node `https` server is tracked on its TLS
  connections. Known gap: `http2` servers are not tracked.
- The shim's reports never wait for Warden. Its end of fd 3 is
  non-blocking, with room for hours of heartbeats (4 MB where the system
  allows it). If a supervisor stops reading (SIGSTOPped, hung, a frozen VM),
  only the latest heartbeat is kept and other reports wait in order until
  there is room, so the worker's event loop never blocks on Warden. The
  app's own stdout and stderr are ordinary pipes: an app that logs more than
  a pipe holds while its supervisor is stopped waits on its next write, as
  it would under any process manager. `[watchdog]` on the supervisor itself
  (systemd's `WatchdogSec=`) is what restarts a hung supervisor. A dead
  supervisor is different: see the keeper point below.
- On Bun, the shim wraps `Response` and `ReadableStream`, so it can end SSE
  bodies in a drain (one call frame per `new Response`, which JSC inlines:
  no measurable difference, `bench/shim-cost.ts`). The wrappers pass for
  Bun's own: `instanceof` (also for `fetch()` responses), `constructor`,
  `name`, `length`, the statics, subclassing, the error without `new`, and
  the source text (`Function.prototype.toString` is wrapped for that: one
  lookup per call). What still differs: they are other function objects,
  `Bun.inspect(Response)` shows `[Function: Response]` rather than
  `[class Response]`, and their own property names list `prototype` before
  the statics. `[shutdown] long_lived_timeout = 0` leaves both untouched.
- An app runs as two Warden processes: the keeper (the process
  `warden start` started, the one wardend and systemd see) and its child,
  the supervisor, which does everything else. The keeper holds a copy of
  each worker's pipes and fd 3 and, on macOS, of the listening socket the
  supervisor hands connections from. If the supervisor dies (a crash, a
  panic, an OOM kill, `kill -9`), the workers keep serving: on Linux they
  become the keeper's children (`PR_SET_CHILD_SUBREAPER`), the keeper reads
  their output into 1 MB per stream, starts the supervisor again at once
  (then with backoff; after 6 deaths in a minute it stops the workers and
  exits), and hands it the workers, their descriptors and the output it
  kept. The new supervisor supervises them again, with their uptime and
  their pids; a worker that died meanwhile is started again, and a hot
  standby or a worker caught mid-rollout is stopped and replaced (it was
  not serving). `status`
  says `pid` (the keeper) and `supervisor_pid`. `[restart]
  keep_workers_on_crash = false` (or `WARDEN_KEEPER=0`) runs the
  supervisor alone, as earlier releases did.
- If the supervisor and the keeper die together (one `kill -9` of the
  group, wardend gone with them or not), workers under the shim on Linux
  keep serving and come back under Warden by themselves. Each holds a
  copy of Warden's ends of its own pipes and fd 3, so nothing it prints
  fails (a Node `console.log` to a closed pipe kills the app); its output
  waits in the pipe (1 MB as root, 64 KB otherwise). Once a second the shim looks whether its
  supervisor or keeper still runs; when neither does, it starts `warden
  recover-worker`, which starts wardend again if it died, and hands the
  descriptors to the app's next supervisor. wardend, when it starts,
  starts every app whose recorded workers run with no Warden process.
  That supervisor waits up to 5 s for them and supervises those that
  answer exactly as after a supervisor crash: same pids, their output,
  health checks, restarts, `warden stop`. Measured: 0 failed requests,
  about 3 s from the kill to supervised again, with wardend killed too.
- If the keeper is SIGKILLed, the supervisor tells each worker to stop
  (the stop signal, to its process group) and exits without `bye`, so
  wardend or systemd starts the app again. Without a keeper, workers are
  signalled via `PR_SET_PDEATHSIG` (direct children only). Under systemd the
  cgroup takes care of the rest. macOS has no parent-death signal: the next
  start of the app stops the workers a SIGKILLed supervisor left behind
  (docs/platforms.md).
- Don't run Warden as PID 1 in a container; use `tini`, or `docker run --init`, to reap orphans.
- A log consumer that can't keep up (a stuck journald, a full disk) costs
  log lines, never supervision: lines past the queue bounds are dropped,
  reported in the log and counted (`log_lines_dropped` in `status --json`,
  `warden_log_lines_dropped_total` in the metrics). With
  `[logging] max_lines_per_sec = 0` Warden keeps every line by slowing a
  flooding app down instead (for at most a second per read).

## See also

- [`architecture.md`](architecture.md): the design in full.
- [`deploys.md`](deploys.md): rolling reloads, canaries and rollback.
- [`reliability.md`](reliability.md): crash, hang and leak handling, hot standbys.
- [`configuration.md`](configuration.md): every config key.

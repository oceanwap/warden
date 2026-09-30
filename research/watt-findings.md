# Platformatic Watt: how multi-worker execution works (source study)

**Version studied:** `@platformatic/runtime`, `wattpm`, `@platformatic/basic`, `@platformatic/node`, `@platformatic/itc`, `@platformatic/foundation` **3.71.0** (published 2026-09-29), plus `undici-thread-interceptor` 1.5.0 (runtime depends on `^1.3.1`) and `close-with-grace` 2.5.0. Cross-checked against a shallow clone of `platformatic/platformatic` at `c0831a0` (2026-09-29). Paths below are relative to each npm package root (`packages/<name>/` in the monorepo).

## 1. Worker model
- **Default: `worker_threads`, all in one process.** `runtime/lib/runtime.js` `#setupWorker()` calls `new Worker('lib/worker/main.js', {workerData, resourceLimits, env, execArgv, stdout:true, stderr:true})`. Worker id is `"<appId>:<index>"`. A replacement gets a new, higher index (`#getNextWorkerIndex`).
- **Child processes only in "command" mode.** If an app defines `application.commands.{production,development}` (e.g. Next.js in dev), the worker thread spawns a child with `child_process.spawn` (`basic/lib/capability.js` `startWithCommand()`/`spawn()`). It adds `--import child-process.js` to `NODE_OPTIONS`, and the child talks back over a WebSocket on a Unix socket, `$TMPDIR/platformatic/runtimes/<pid>-<rand>.socket` (`basic/lib/worker/child-manager.js`). If the child exits, the thread exits with the same code (`process.exit(code)`).
- **What the main thread owns:** the config, the worker map (`RoundRobinMap`), the mesh *coordinator*, health polling, restarts and scaling, signal handling, the main pino logger, and the management API (a Unix socket). It also runs the Prometheus/health server (default `0.0.0.0:9090`), the cron scheduler and the shared HTTP-cache store.
- **What each worker owns:** one app instance, its own undici global dispatcher and a prom-client registry.
- **`workers` setting** (`foundation/lib/schema.js`, `runtime/lib/config.js` `parseWorkers()`):
  - Accepts a number, an env-placeholder string, or an object `{static, dynamic, minimum, maximum, total, maxMemory, cooldown, gracePeriod, scaleUpELU, scaleDownELU}`.
  - The runtime-level default is `{static:1, dynamic:false}`, and each app can override it.
  - Optional ELU-driven autoscaler (`lib/worker-scaler.js`) defaults: total = `availableParallelism()`, scale up above ELU 0.8, scale down below 0.2, cooldown 20 s, grace 30 s, check every 60 s, memory cap 90% of RAM.
  - The docs say workers are forced to 1 in dev mode. I found no code in 3.71.0 that does this; only `watch` is tied to dev. Not verified at runtime.

## 2. Networking / port sharing
- **SO_REUSEPORT per worker thread.** Each thread's own HTTP server binds the public port with `reusePort: true`, and the kernel balances connections. How it is injected (`basic/lib/capability.js` `_start()`): the capability subscribes to `diagnostics_channel.tracingChannel('net.server.listen')` and in `asyncStart` sets `options.reusePort = true`. That covers *any* `listen()` in the thread. Child processes do the same (`child-process.js` `#setupTcpPortsHandling`).
- **Controlled by `reuseTcpPorts`** (default `true`, runtime- or app-level) combined with the platform check `features.node.reusePort` (`foundation/lib/node.js`):

  ```js
  reusePort: satisfies(process.version, '^22.12.0 || ^23.1.0 || >=24.0.0') && !['win32','darwin'].includes(platform())
  ```
  The minimum Node version is 22.19.0.
- **Fallback without reusePort (macOS, Windows): there is no main-thread routing.** In `runtime.js` `addApplications()`, an entrypoint with more than one worker logs a warning and is **forced to 1 worker**. The dynamic scaler pins it too (`worker-scaler.js` `add()`). The only alternative is `server.portAssignment: "perWorkerIncrement"`: each worker binds `port + index`, and you bring your own load balancer. Non-entrypoint apps can still run several workers on any OS, because they are reached via the mesh.
- **Linux path: public HTTP traffic never goes through the main thread.** The main thread is on the data path only for its own traffic:
  - `runtime.inject()` (management API / `wattpm inject`) and scheduler jobs, which dispatch into workers via the main thread's own mesh interceptor.
  - ITC round-trips from workers when `httpCache` is enabled, because the cache store lives in main (`worker/http-cache.js` `RemoteCacheStore`).
- **Internal mesh (`*.plt.local`), from `undici-thread-interceptor`:**
  - The main thread's `createThreadInterceptor().route(appId, worker)` acts as coordinator. It creates a direct `MessageChannel` between every pair of workers (`lib/coordinator.js` `wire()`), subject to `policies.deny`.
  - A worker's undici dispatcher round-robins across the target app's ports (`lib/interceptor.js`).
  - The receiving worker serves the request in memory with `light-my-request` against the app's request handler (`lib/wire.js`), so there is no TCP.
  - Bodies stream over transferred MessagePorts.
  - If a target advertises an address (`useHttp`, command/child mode), the request goes over TCP to that address.

## 3. Lifecycle and health
- **Worker states:** `boot` (thread created) → `init` (worker notified ITC `init`) → `starting` → `started` → `stopping` → `stopped`, plus `exited`. Runtime states: `init / starting / started / stopping / stopped / closing / closed / errored`.
- **Readiness:** a worker becomes `started` only after two things finish:
  1. The ITC `start` call returns: app started, `controller.listen()` for the entrypoint, and `dispatcher.replaceServer(...)` (`worker/itc.js`).
  2. The mesh `route()` promise resolves (the worker is wired as ready).

  That sequence is bounded by `startTimeout` (30 s), after which the worker is terminated (`#startWorker`). The `/ready` endpoint is true when every app has at least one `started` worker and all custom readiness checks pass. `/status` (liveness) is readiness plus custom health checks (`prom-server.js`).
- **Health checks** (`#startHealthMetricsCollection`, `#setupHealthCheck`):
  - Every 1 s the main thread reads `worker.performance.eventLoopUtilization()` straight from the thread handle, with no ITC. It reads `worker.getHeapStatistics()` about every 60 s.
  - A per-worker timer evaluates the latest sample every `interval`. The worker is unhealthy if `elu > maxELU`, `heapUsed/maxHeapTotal > maxHeapUsed`, or optional event-loop-delay limits are exceeded.
  - Defaults (`foundation/lib/schema.js` `health`):

    | Setting | Default |
    |---|---|
    | `enabled` | `true` |
    | `interval` | 30000 (min 1000) |
    | `gracePeriod` | 30000 |
    | `maxUnhealthyChecks` | 10 (consecutive) |
    | `maxELU` | 0.99 |
    | `maxHeapUsed` | 0.99 |
    | `maxHeapTotal` | 4 GiB |
    | `maxYoungGeneration` | 128 MiB |

    `maxHeapTotal` also sets V8 `resourceLimits`, so an OOM kills the thread.
  - Checks run only if `restartOnError > 0`.
- **Unhealthy worker:** it is replaced via `#replaceWorker()` (start new, then stop old). If replacing fails, it gets `worker.terminate()`.
- **Crash / restart** (`exit` handler, then `#restartCrashedWorker`):
  - `restartOnError` defaults to `true` = 5000 ms delay. **In production, `config.js` forces the runtime-level value to 2**, which means an immediate `process.nextTick` restart. An app-level value overrides this.
  - There is no exponential backoff and no cap on restarts after a worker has started once.
  - Start failures retry up to `MAX_BOOTSTRAP_ATTEMPTS = 5`.
  - An uncaught exception in a worker exits it after 100 ms (`exitOnUnhandledErrors`, default `true` = 100).

## 4. Graceful shutdown
- **Signals** (`runtime/index.js` `handleSignal()`): `close-with-grace` handles SIGINT, SIGTERM, SIGHUP, SIGQUIT and similar, plus `uncaughtException`, `unhandledRejection` and `beforeExit`.
  - Timeout is `gracefulShutdown.runtime`, default 30 s, then exit(1).
  - A second signal exits immediately.
  - **SIGUSR2 triggers a rolling restart, not a shutdown.**
- **Order** (`Runtime.stop()`): scheduler, then the scaler, then **the entrypoint's workers first** (stop accepting traffic), then extensions, then the other apps in parallel (each waits for its dependents), then close the mesh.
- **Per worker** (`#stopWorker`):
  1. ITC `stop` with a `gracefulShutdown.application` timeout (default 10 s). The capability's `setClosing()` adds `Connection: close` to responses and sends HTTP/2 GOAWAY (`closeConnections`, default `true`). The app then closes (`fastify.close()` / `server.close()` / the exported `close`).
  2. Wait for the thread to exit, with the same timeout.
  3. `worker.terminate()` if it has not exited.
- **Child processes:** a WS `close` message, then SIGINT, then SIGKILL, with a timeout between each step.

## 5. Zero-downtime reload
- **Triggers:** `runtime.restart()` from SIGUSR2, `wattpm restart` or the management API. Apps restart in parallel; within an app, workers are replaced **one at a time**, with optional `workersRestartDelay` (default 0).
- **With reusePort:** the new worker is set up and started on the same port *alongside* the old one. Then the old one gets `removeFromMesh` (its interceptor closes), then it is stopped.
- **Without reusePort, or with `reuseTcpPorts:false`, for the entrypoint:** stop-then-start, with the port pinned. This means brief downtime.
- **Dev file-watch reload:** stop-then-start of the whole app, not rolling.
- **Not handled in the code I read:** connections still queued in the old socket's accept backlog when it closes under SO_REUSEPORT (a general Linux caveat, not something the source addresses).

## 6. ITC, shared state, and what is Node-specific
- **ITC** (`@platformatic/itc`): request/response/notification messages (`PLT_ITC_REQUEST` and so on, with a UUID `reqId`) over the Worker `MessagePort`. Commands include `start`, `stop`, `getHealth`, `getMetrics`, `removeFromMesh` and `inject`.
- **Shared state and messaging:**
  - `sharedContext`, kept in main and pushed to workers.
  - A `BroadcastChannel` with the list of workers.
  - App-to-app messaging (`platformatic.messaging.send`) over dedicated `MessageChannel`s set up by main.
  - The shared HTTP cache in main.
- **Node-only pieces that would not carry over to a Bun or multi-process design:**
  - `worker_threads` `resourceLimits`, `worker.performance.eventLoopUtilization()` and `worker.getHeapStatistics()` on the thread handle.
  - Transferring `MessagePort`s.
  - `diagnostics_channel` `tracingChannel('net.server.listen')` to inject `reusePort`, and `http.server.request.start` for basePath stripping.
  - The undici global-dispatcher symbols and interceptors for the mesh.
  - `light-my-request` in-memory injection.
  - `NODE_OPTIONS --import` preload for child processes.
  - `module.enableCompileCache`.
- **What does carry over as a pattern:** SO_REUSEPORT per worker, and polling ELU/heap from the supervisor.

## 7. Metrics and logging
- **Metrics:** Prometheus text or JSON at `:9090/metrics` when `metrics` is configured.
  - On scrape, main asks every started worker via ITC `getMetrics` (with a timeout) and merges the results with main-thread process metrics and `platformatic_application_restarts_total`.
  - Worker metrics include prom-client defaults, `http_request_all_duration_seconds`, `nodejs_eventloop_utilization` and `thread_cpu_*`.
  - OTLP export is optional.
- **Logging:** each worker uses its own pino, with direct write disabled, writing JSON to the thread's stdout. `console.error` lines are tagged with a marker character.
  - Main reads `worker.stdout` and `worker.stderr`. It writes pino JSON lines through **unchanged** to the main pino destination, and wraps anything else as `{caller:'STDOUT'|'STDERR'}` (`#handleWorkerStandardStreams`, `#forwardThreadLog`).
  - Base bindings are `name` (the app id), `worker` (index) and `pid`.

## 8. Bun
- There are **no Bun references** in the 3.71.0 packages or the monorepo source; the only hits are transitive entries in `pnpm-lock.yaml`.
- The runtime refuses to run on Node below 22.19.
- Platformatic's Bun blog post only benchmarks Bun standalone, not inside Watt.
- **Unverified:** whether a `commands.production: "bun ..."` app would work. The child integration relies on Node's `--import` via `NODE_OPTIONS`.

## Could not verify / caveats
- I read the code but ran nothing, so runtime behavior on Linux vs. macOS was not tested.
- `multithread-architecture.md` in the repo disagrees with the code in places: its health defaults, "unhealthy workers excluded from rotation" (the code replaces them instead) and "dev = 1 worker". I treated the code as ground truth.

Sources: npm tarballs above; https://github.com/platformatic/platformatic (`docs/reference/runtime/multithread-architecture.md`, `_shared-configuration.md`); https://blog.platformatic.dev/93-faster-nextjs-in-your-kubernetes ; https://blog.platformatic.dev/bun-is-fast-until-latency-matters-for-nextjs-workloads

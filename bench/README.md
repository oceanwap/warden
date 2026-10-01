# Benchmarks

Everything here compares Warden with the tools people use today, on the same
machine and the same apps: PM2, Platformatic Watt (`wattpm`), nginx and the
`serve` package. The results in the main README come from one command:

```sh
cargo xtask bench              # every suite (~20 min on 2 CPUs); updates README.md
cargo bench-all                # the same, shorter to type
cargo xtask bench --quick      # 3 s per measurement: a smoke test, not results to publish
cargo xtask bench --only static,logs --no-readme
cargo xtask bench --only longlived --no-readme   # WebSockets and SSE through a rolling restart
```

`cargo xtask` is a cargo alias (`.cargo/config.toml`) for the `xtask` crate
in this repository; it is not part of `cargo build`. It checks the tools
below, installs the npm packages on first run (`npm ci` here and in
`bench/nest`), builds Warden in release mode, runs every suite, writes
`bench/results/latest.md` and `bench/results/latest/<suite>.json`, and
replaces the tables between `<!-- bench:start -->` and `<!-- bench:end -->` in
the README.

## Needs

- Linux (the scripts read `/proc`; SO_REUSEPORT balancing is Linux behaviour)
- [Bun](https://bun.sh) (runs the scripts and the Bun apps)
- Node.js 22.12 or newer (`reusePort` in `node:http`)
- [oha](https://github.com/hatoo/oha), the load generator: `cargo install oha`
- npm (installs the pinned versions in `package.json`: pm2 6.0.14, wattpm 3.71.0, serve 14.2.6)
- nginx, optional: without it the static-files table has no nginx column
- Recommended: `sysctl -w net.ipv4.tcp_migrate_req=1` (what `warden startup`
  sets). Without it, a rolling restart can reset connections that were queued
  on a closing listener, under every manager.

## Suites

Each script runs on its own too, prints a Markdown table on stdout and writes
the raw numbers to `bench/results/<date>-<suite>.json` (git-ignored).

| Script | What it measures |
|---|---|
| `run.ts --app node-http` | A `node:http` app under bare / PM2 (cluster) / Watt (4 worker threads) / Warden (4 processes) |
| `run.ts --app nest-node` | A minimal NestJS app on Node, same scenarios |
| `run.ts --app bun-http` | A `Bun.serve` app under bare / PM2 (fork mode: it has no Bun cluster mode) / Warden processes / Warden worker threads |
| `run.ts --app nest-bun` | The NestJS app on Bun, same scenarios |
| `static.ts` | `warden serve` vs nginx vs `pm2 serve` vs `serve`: a 1.5 KB page, a 48 KB script and a 1 MB file, keep-alive and a new connection per request |
| `logs.ts` | Capturing worker output: steady logging (CPU, completeness) and a flood (throughput, CPU per GB) |
| `fleet.ts` | 10 apps on one host: manager memory, idle CPU, and how fast `list`, `describe` and `logs` answer |
| `longlived.ts` | WebSocket and SSE clients held through a rolling restart: PM2 (cluster mode for Node, fork mode for Bun) vs Warden (processes; Bun also worker mode) |

Common options: `--duration S` (seconds per load test, default 10),
`--connections N` (64), `--workers N` (4), `--scenarios a,b` (a subset).
`logs.ts` takes `--rate`, `--seconds` and `--mb`; `fleet.ts` takes `--apps`;
`longlived.ts` takes `--clients N` (50 of each kind), `--apps node,bun` and
`--warden <path>` (a debug build for a smoke run; default
`target/release/warden`), and has an extra scenario, `node-warden-off`
(`long_lived_timeout = 0`, `grace_period = 5`: the behaviour before Warden
closed these connections).

### What each number means

- **total RAM, RSS / PSS**: every process of the scenario (manager and
  workers). RSS counts pages shared between processes (the same `node`
  binary in four workers) once per process; PSS splits them, so it is the
  memory the scenario really uses. Both are shown.
- **manager RAM / idle CPU**: the process manager alone (PM2's daemon, the
  Watt runtime, Warden's supervisor), idle CPU over 5 s after the load.
- **startup**: from launching the manager until N different workers have
  answered a request.
- **crash recovery**: a request to `/crash` makes one worker exit; the time
  until a *new* worker (a pid:thread not seen before) answers.
- **requests failed during a crash**: 8 clients, a new connection per request,
  while that happens. A request in flight on the crashing worker fails under
  every manager.
- **rolling restart under load**: each manager's zero-downtime restart
  (`pm2 reload`, `wattpm restart`, `warden restart`) with 8 clients running;
  done when the command has returned and N new workers have answered.
  `warden-surge` is `warden-process` with `[reload] surge = "all"`: every new
  worker starts at once next to the old ones, which drain once all have passed
  the gates (briefly twice the workers). Only this row, startup and idle memory
  are measured for it; its other cells are `-`.
- **status command**: the median of 10 runs of the manager's status command
  (`pm2 jlist`, `wattpm ps`, `warden status --json`).
- **req/s, p50/p99**: oha with 64 keep-alive connections for `--duration`
  seconds after a 2 s warm-up; `/plaintext` (13 bytes), `/json` (~1.2 KB) and
  `/cpu` (fib(27), CPU-bound).
- **long-lived connections** (`longlived.ts`): N WebSocket and N SSE clients
  (raw sockets, so close codes are exact; Bun's own WebSocket client reports a
  1001 as 1000) connect, then the manager's rolling restart runs. A client
  reconnects at once whenever its connection ends; counted from the restart
  on:
  - *WebSocket closes, clean / abnormal*: a close frame (its code is listed:
    1001 Going Away from Warden) / the connection cut without one, which a
    browser reports as 1006, or a reset;
  - *SSE ends, clean / abnormal*: the chunked stream's last chunk (an
    EventSource then reconnects quietly) / cut without it, or a reset;
  - *failed reconnects*: connections refused or closed before the first
    message, e.g. while no worker listens;
  - *rolling restart*: until the command has returned and N new workers have
    answered; *until every client is back*: until every client's connection
    is on a worker that wasn't there before the restart.

  The apps are the integration tests' fixtures
  (`tests/fixtures/longlived.ts`, `tests/fixtures/longlived_node.mjs`).
  Warden runs with its default `[shutdown]` settings, so a worker holding
  such clients drains in about `long_lived_timeout` (2 s): its restart takes
  longer than an abrupt one, by design.

## Fairness rules

- The same app file runs under every manager. Node apps listen with
  `reusePort` (Node 22.12+) where they share the port themselves; under PM2's
  cluster mode the daemon owns the port instead, as PM2 users run it.
- Every manager is timed from the outside with the same probe (which worker
  answered), never with its own CLI: `pm2 jlist` alone takes ~170 ms, and
  polling it would time PM2's CLI instead of its recovery.
- Requests time out after 2 s and count as failed. PM2's cluster mode can
  hang a connection it handed to a worker that is exiting. The "which worker
  answered" probe gives up after 250 ms and asks again, so one such hang
  doesn't inflate a recovery time.
- `BUN_OPTIONS` and `NODE_OPTIONS` from your shell are removed, so they can't
  change what is measured.
- Every static server must return the exact bytes of every file, or its run
  is rejected.
- The load generator shares the CPUs with the apps. On a 2-CPU machine the
  req/s columns are low and move ~10 % between runs; compare columns within a
  run, and don't read differences under 10 % as real.

Methodology notes and history (what changed and why): `docs/benchmarks.md`.

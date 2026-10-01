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
cargo xtask bench --app-cpus 0-3 --loadgen-cpus 4-7 --loadgen wrk   # apps and load generator apart
cargo xtask bench --only static --rounds 3 --no-readme   # servers interleaved 3 times, medians
cargo xtask profile --targets warden,nginx --path /assets/app.3f9a2c1b.js --strace   # see "Profiling"
```

`cargo xtask` is a cargo alias (`.cargo/config.toml`) for the `xtask` crate
in this repository; it is not part of `cargo build`. (The same crate releases
Warden: `cargo xtask release`, alias `cargo release`; see
[`docs/releasing.md`](../docs/releasing.md).) It checks the tools
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
  (or [wrk](https://github.com/wg/wrk) with `--loadgen wrk`)
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
| `run.ts --app node-http` | A `node:http` app under bare / the shim alone / PM2 (cluster) / Watt (4 worker threads) / Warden (4 processes) |
| `run.ts --app nest-node` | A minimal NestJS app on Node, same scenarios |
| `run.ts --app bun-http` | A `Bun.serve` app under bare / the shim alone / PM2 (fork mode: it has no Bun cluster mode) / Warden processes / Warden worker threads |
| `run.ts --app nest-bun` | The NestJS app on Bun, same scenarios |
| `static.ts` | `warden serve` vs nginx vs `pm2 serve` vs `serve`: a 1.5 KB page, a 48 KB script and a 1 MB file, keep-alive and a new connection per request; `warden-nocache` is Warden with the response cache off (`cache_size = 0`), to show what the cache buys |
| `logs.ts` | Capturing worker output: steady logging (CPU, completeness) and a flood (throughput, CPU per GB); `warden-direct` is `worker_output = "direct"` (bytes spliced into the file, unparsed) |
| `fleet.ts` | 10 apps on one host: manager memory, idle CPU, and how fast `list`, `describe` and `logs` answer |
| `longlived.ts` | WebSocket and SSE clients held through a rolling restart: PM2 (cluster mode for Node, fork mode for Bun) vs Warden (processes; Bun also worker mode) |
| `profile.ts` | One server at a time, a fixed number of requests: CPU time, context switches and syscalls per request, and where the time goes (perf). See [Profiling](#profiling) |
| `shim-cost.ts` | What Warden's shim adds to one request, in-process (ns), bare vs with the shim |

Every `run.ts` app also runs `warden-standby`: Warden's processes plus one
hot standby (`[workers] standby = 1`), so its crash recovery is a promotion
instead of a cold start. The standby counts in the RAM rows (that is its
cost); it answers no request until promoted.

The Bun and Node apps also run `shim`: the same bare processes with Warden's
shim preloaded as Warden does it (drain hooks on, heartbeat, `reusePort`),
so the column next to `bare` is what the shim costs per request, alone. (The
NestJS apps' `bare` needs the shim already, to share the port, with its drain
off.) That cost is tens of nanoseconds against ~10 µs of TCP per request, so
the req/s columns can't show it; `bench/shim-cost.ts` measures it in-process:

```sh
bun bench/shim-cost.ts --rounds 3                                    # bare vs the shim
git show HEAD~1:shim/warden-shim.mjs > /tmp/old.mjs
bun bench/shim-cost.ts --rounds 5 --shim-b /tmp/old.mjs              # and against an older shim
```

Common options: `--duration S` (seconds per load test, default 10),
`--connections N` (64), `--workers N` (4), `--scenarios a,b` (a subset).
Every suite also takes:

- `--app-cpus L` and `--loadgen-cpus L` (taskset lists, `0-3,6`): pin the
  apps with their managers, and the load generator, to separate CPUs, so
  they don't take each other's (children inherit the pinning). Unpinned, all
  share every CPU, as in the published 2-CPU run.
- `--loadgen oha|wrk`: oha (default) or wrk, which uses less CPU per request
  and so leaves more to the apps on a small machine (its latency percentiles
  come from a `done()` hook, exact).
- `BENCH_APP_CPUS`, `BENCH_LOADGEN_CPUS`, `BENCH_LOADGEN` in the environment
  do the same.

`static.ts` takes `--rounds N`: every server N times, interleaved (A B A B
…), the table showing the median of each number: an A/B comparison that a
busy machine moves less (`cargo xtask bench --rounds N` passes it on).
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
- **CPU per request (µs)**: the CPU time of every process of the scenario
  (apps and manager) during that load, divided by the requests answered,
  from `/proc/<pid>/task/*/schedstat` (nanoseconds, not 10 ms ticks). It
  moves much less than req/s when the machine is busy; it includes the
  kernel's TCP work done in the server's time (loopback: part of the
  client's too), so compare it between columns, not with other machines.
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

## Profiling

`bench/profile.ts` (or `cargo xtask profile`, which first builds Warden
with symbols, `--profile profiling`, and adds `--perf`) measures one server
at a time with a fixed number of requests, in numbers a busy machine moves
little:

```sh
cargo xtask profile --targets warden,nginx --path /assets/app.3f9a2c1b.js --strace
bun bench/profile.ts --targets bun,bun-shim,node,node-shim --rounds 5 --app-cpus 0 --loadgen-cpus 1
bun bench/profile.ts --targets warden,warden:cache_max_file=16384 --path /assets/app.3f9a2c1b.js --rounds 3
bun bench/profile.ts --targets warden,warden@/tmp/warden-before --rounds 5   # two builds
```

Targets: `warden`, `warden-nocache`, `nginx` (the static site of
`static.ts`), `bun`, `node` (the bench apps, bare) and `bun-shim`,
`node-shim` (with the shim). `warden@PATH` / `bun-shim@PATH` use another
binary or shim; `warden:key=value,…` sets `[static]` keys. Per target:

- **req/s, p50 / p99** of `--requests N` (200,000) on `--connections C` (64)
  keep-alive connections (`--new-connections`: one per request), after a
  warm-up; `--workers N` server processes (1 by default: the cleanest
  per-request numbers);
- **server CPU per request** (µs, from schedstat) and **context switches
  per request** (how many requests one wake-up serves);
- **instructions and cycles per request**, where the machine has hardware
  counters (`perf stat`; VMs often have none);
- with `--strace`: **system calls per request**, each kind listed (a
  separate pass of N/10 requests: strace slows the server);
- with `--perf`: the top symbols of the server processes (`perf record -e
  cpu-clock`, a separate pass; `--call-graph` adds call chains, `--top N`).

`--file-size N` adds an N-byte file to the static site and requests it
(where the cache's memfd starts paying was found this way);
`--pin-servers` pins server process i to CPU i mod n (`taskset -a -p`),
what a per-worker CPU affinity would do, for any target
(`research/static-cpu-steering/run.sh` uses it).

`--rounds N` interleaves the targets N times and reports medians. Ubuntu's
`/usr/bin/perf` is a wrapper that refuses to run without the kernel's own
linux-tools package; the script then uses any `/usr/lib/linux-tools*/perf`
(or `--perf-bin`, `$PERF`).

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
- Unless pinned (`--app-cpus`, `--loadgen-cpus`), the load generator shares
  the CPUs with the apps. On a 2-CPU machine the req/s columns are low and
  move ~10 % between runs; compare columns within a run, and don't read
  differences under 10 % as real (the CPU-per-request rows move less).

Methodology notes and history (what changed and why): `docs/benchmarks.md`.

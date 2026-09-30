# Benchmarks

PRD §19: compare the same app run four ways, and don't claim an optimisation
the data doesn't show.

| | Scenario | What it models |
|---|---|---|
| **A** | bare | 4 × `bun app` started directly, same port with `reusePort` — what 4 systemd units do |
| **B** | pm2 | PM2 6.0, 4 fork-mode instances with the `bun` interpreter |
| **C** | warden process | `mode = "process"`, 4 workers |
| **D** | warden worker | `mode = "worker"`, 1 Bun process × 4 Workers |

Reproduce: `cargo build --release && (cd bench && npm i) && (cd bench/nest && npm i)`,
then `bun bench/run.ts` and `bun bench/run.ts --app nest`. Needs [`oha`](https://github.com/hatoo/oha)
(`cargo install oha`). Raw results are written to `bench/results/*.json`.

## Environment and caveats

- Bun 1.3.13, Linux 6.18 x86_64, **2 vCPU** container, 8 GB. `oha` runs on the
  same 2 vCPUs as the 4 workers, so absolute req/s are low and noisy; compare
  scenarios with each other, not with other machines. Two runs differ by up to
  ~10 % on req/s; differences smaller than that are noise.
- Loopback only, no nginx in front. 64 keep-alive connections, 10 s per endpoint
  after a 2 s warm-up.
- `net.ipv4.tcp_migrate_req = 0` (the default).
- NestJS 12.1 on Express, minimal app with the same endpoints (`bench/nest`).
  In A and B it is started with Warden's shim preloaded — without it the 2nd
  copy fails with `EADDRINUSE` (Bun's `node:http` ignores `reusePort`, see
  architecture F9), so every scenario runs identical code.
- The production target is ARM64 (Ampere) with more cores; re-run there before
  deciding anything.

## Results: Bun.serve app (`bench/app`)

| | bare | pm2 | warden process | warden worker |
|---|---|---|---|---|
| startup to 4 listeners (ms) | 84 | 712 | 97 | 107 |
| app RSS idle (MB) | 147.4 | 211.9 | 163.6 | **65.3** |
| app RSS after load (MB) | 173.7 | 225.0 | 189.2 | **71.8** |
| supervisor RSS (MB) | – | 65.4 | 3.9 | 3.8 |
| supervisor CPU during 10 s of load (s) | – | 0.01–0.04 | ≤ 0.01 | 0 |
| connections per worker (400 fresh) | 109/106/94/91 | 115/99/99/87 | 105/104/103/88 | 107/103/98/92 |
| restart after SIGKILL of one worker (ms) | never | 101 | 142 | 187 **(all 4 down)** |
| /plaintext req/s | 92 146 | 83 584 | 94 779 | 89 076 |
| /plaintext p50 / p95 / p99 (ms) | 0.43 / 2.30 / 3.99 | 0.61 / 2.35 / 4.02 | 0.36 / 2.38 / 4.10 | 0.47 / 2.29 / 4.30 |
| /json req/s | 85 382 | 71 763 | 84 588 | 80 239 |
| /json p50 / p95 / p99 (ms) | 0.60 / 1.92 / 3.77 | 0.78 / 2.09 / 3.18 | 0.63 / 1.91 / 3.25 | 0.60 / 2.08 / 4.59 |
| /cpu req/s | 1 225 | 1 224 | 1 205 | 1 195 |
| /cpu p50 / p95 / p99 (ms) | 49.8 / 82.1 / 103.6 | 54.8 / 68.6 / 108.2 | 54.9 / 74.7 / 115.4 | 55.8 / 73.6 / 113.3 |
| errors (all endpoints) | 0 | 0 | 0 | 0 |

**Respawn update (2026-09-30):** the first crash after a healthy run now
restarts with no backoff. Release build, 4 × `bench/app`, SIGKILL of one
worker to `RUNNING` again as seen over the control socket: 31–45 ms (was
142 ms; PM2 101 ms). A second crash of the same worker inside
`restart_window` still waits `backoff_initial` (135–150 ms). The control
socket round trip for `status` was 0.3–0.5 ms.

An earlier run of the same code minus the per-worker health socket gave
plaintext 97 k / 84 k / 98 k / 99 k req/s and the same memory figures —
the throughput ordering between A, C and D is within noise.

## Results: NestJS app (`bench/nest`)

| | bare | pm2 | warden process | warden worker |
|---|---|---|---|---|
| startup to 4 listeners (ms) | 806 | 1 573 | 777 | 765 |
| app RSS idle (MB) | 334.2 | 340.3 | 337.9 | **173.8** |
| app RSS after load (MB) | 439.9 | 439.1 | 446.2 | **360.8** |
| supervisor RSS (MB) | – | 66.4 | 4.0 | 3.9 |
| restart after SIGKILL of one worker (ms) | never | 375 | 433 | 906 **(all 4 down)** |
| /plaintext req/s | 26 978 | 28 720 | 29 047 | 26 889 |
| /plaintext p50 / p95 / p99 (ms) | 1.48 / 6.84 / 12.3 | 1.47 / 5.92 / 13.4 | 1.40 / 6.24 / 11.6 | 1.06 / 9.93 / **18.2** |
| /json req/s | 27 983 | 26 704 | 28 700 | 26 900 |
| /json p50 / p95 / p99 (ms) | 1.72 / 5.74 / 9.67 | 1.74 / 5.92 / 12.2 | 1.52 / 5.95 / 10.6 | 1.37 / 7.51 / **16.0** |
| /cpu req/s | 1 130 | 1 141 | 1 134 | 1 113 |
| errors (all endpoints) | 0 | 0 | 0 | 0 |

Earlier run: worker mode idle 172 MB vs 334 MB, after load 295 MB vs 458 MB,
plaintext p99 18.2 ms vs 10.9 ms — the tail-latency penalty reproduced.

## Zero-downtime measurements

Separate from the harness (scripts in `research/`), with a non-retrying
keep-alive client (Node `http.Agent`), 32–64 connections:

| Event under load | Failed requests |
|---|---|
| SIGKILL of 1 of 4 workers (no drain) | 15–22 `ECONNRESET` per ~50–80 k |
| 2 × `warden reload`, process mode, Bun app | **0** / 124 k |
| `warden reload`, process mode, NestJS (after the shim defers Nest's shutdown hooks) | **0** / 67 k and 0 / 87 k |
| `warden reload`, worker mode, `tcp_migrate_req = 0` | 9–15 / ~80–100 k |
| `warden reload`, worker mode, `tcp_migrate_req = 1` | **0** / ~80 k (3 runs) |
| reload with a fresh connection per request, `tcp_migrate_req = 0` | ~1 per replaced worker |
| same, `tcp_migrate_req = 1` | **0** (3 runs) |

## What the data says (PRD §27)

- **Memory: worker mode wins clearly.** 55 % less RSS idle for a Bun.serve app
  (65 vs 147 MB), 48 % less idle and 18–36 % less after load for NestJS. This
  is the one dimension where the Watt-style architecture shows a measurable
  advantage over 4 independent Bun processes.
- **Throughput and CPU: no measurable difference** between bare, warden
  process and warden worker (within run-to-run noise). Warden adds no
  request-path overhead because it isn't on the request path.
- **Tail latency: worker mode regresses for NestJS** (p99 ~16–18 ms vs
  ~10–12 ms, reproduced in both runs); for the Bun.serve app it's within noise.
  Likely GC / shared-process scheduling across 4 heavy JS heaps in one process;
  worth re-checking on more cores.
- **Fault isolation: process mode wins.** Killing one process in worker mode
  takes all 4 workers down (187 ms Bun, 906 ms NestJS until all are back).
- **Supervisor cost:** Warden ~4 MB RSS, ~0 CPU, vs PM2's 65 MB daemon. PM2
  also added ~10 % app CPU and ~30 % app memory in fork mode with Bun (its
  in-process wrapper), and ~10–15 % lower req/s on the Bun.serve app.
- **Startup:** comparable to bare Bun for both Warden modes; PM2 is 0.7–1.6 s.

**Recommendation:** run production in **process mode** (same throughput,
full fault isolation, zero-downtime reloads verified). Consider **worker mode**
only if memory is the binding constraint, after testing the real API
(database drivers, native modules) inside Bun Workers and checking p99 on the
production ARM64 hardware.

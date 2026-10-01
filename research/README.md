# Research experiments

The scripts behind the findings (F1–F12) in [`docs/architecture.md`](../docs/architecture.md).
They are throwaway probes, kept so every claim can be re-run on new Bun
versions or on the production hardware (e.g. ARM64 Ampere).

Setup: `cd research/bun && npm i autocannon` (the scripts use `bun`, `node`,
`curl`, and `/proc`). NestJS probes: `cd research/nest && npm i @nestjs/core
@nestjs/common @nestjs/platform-express reflect-metadata rxjs`.

| Finding | Run |
|---|---|
| F1 reusePort across processes | `bun srv.ts` ×4 (with `WID=1..4`), then `bun count.ts 400` |
| F2 kill under load | `./run_kill2.sh KILL ""` (Bun client, hides errors); non-retrying client: `nload.mjs` (see `t_*` scripts) |
| F3 graceful drain | `srv3.ts` with `DRAIN_MS=200` + `node nload.mjs 100 5000` while sending SIGTERM to one |
| F4/F7 Workers share port, leak on death | `./t_exit.sh` |
| F5/F6 CPU parallelism, memory | `./t_mem.sh` |
| F7/F8 terminate() vs stop-before-exit | `bun host2.ts terminate` / `bun host2.ts stop` + `bun count2.ts 200 3201` |
| F8/F12 exit hook, fd 3, Worker env | `./t3.sh` |
| F9 node:http reusePort | `./t_nh.sh` |
| F10 preload shim | `./t_pre.sh`, and `research/nest/t.sh` for NestJS |
| F11 signals and Workers | `bun sigh.ts`, then `kill -TERM <pid>` |

`static-cpu-steering/run.sh` (from the repository root) re-runs the
per-worker CPU pinning and `SO_ATTACH_REUSEPORT_CBPF` experiment for
`warden serve` (docs/benchmarks.md, "Tried and dropped"): it builds Warden
with and without `cbpf.patch` and measures unpinned / pinned / pinned with
steering, interleaved. Dropped on 2 CPUs for lack of a clear win; meant for
many-core runners.

`listeners.sh <pid> <port>` counts the LISTEN sockets a pid owns (the same
check Warden uses for readiness and leak detection).

`watt-findings.md` is the source study of Platformatic Watt 3.71.0.

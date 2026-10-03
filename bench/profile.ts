#!/usr/bin/env bun
// What one request costs a server, in numbers a noisy machine moves little:
// server CPU time per request (exact, from schedstat), context switches per
// request, system calls per request (strace -c) and where the CPU time goes
// (perf record, top symbols). Each target runs alone, with a fixed number of
// requests; with --rounds N the targets run interleaved (A B A B ...) and the
// table shows medians. Also `cargo xtask profile` (which builds Warden with
// symbols first). See bench/README.md.
//
//   bun bench/profile.ts --targets warden,nginx --path /assets/app.3f9a2c1b.js --strace --perf
//   bun bench/profile.ts --targets bun,bun-shim,node,node-shim --rounds 5
//   bun bench/profile.ts --targets warden-cache,warden-cache:cache_max_file=16384 --path /assets/app.3f9a2c1b.js
//   bun bench/profile.ts --targets warden,warden@/tmp/warden-before --app-cpus 0 --loadgen-cpus 1
//
// Targets:
//   warden, warden-cache, nginx   the static site of bench/static.ts (1.5 KB page, 48 KB script, 1 MB file)
//   bun, node                       the bench apps (bench/app/server.ts, bench/node/server.mjs), started bare
//   bun-shim, node-shim             the same with Warden's shim preloaded, as Warden runs them
// A target can carry overrides:
//   warden@/path/to/warden          another Warden binary (an A/B of two builds)
//   bun-shim@/path/to/shim.mjs      another shim
//   warden:key=value,key=value      [static] settings (values as TOML: numbers, true/false, "strings")
//
// Options:
//   --targets a,b        what to measure (default warden)
//   --path P             the URL path (default /index.html for static targets, /plaintext for apps)
//   --requests N         requests per measurement (default 200000); --connections C (default 64)
//   --workers N          server processes (default 1: one process, the cleanest per-request numbers)
//   --rounds N           interleaved repetitions; medians are reported (default 1)
//   --new-connections    a new connection per request (default: keep-alive)
//   --pin-servers        pin server process i (all its threads) to CPU i mod n of the app CPUs
//                        (taskset -a -p): what a per-worker CPU affinity would do, for any target
//   --strace             also count system calls per request (a separate pass of requests/10: strace slows the server)
//   --perf               also sample where the CPU time goes (perf record -e cpu-clock; a separate pass), top
//                        symbols per target; --top N lines (default 25), --call-graph (with -g)
//   --perf-bin PATH      the perf binary (default: $PERF, else `perf` if it works, else /usr/lib/linux-tools*/perf)
//   --warden PATH        the Warden binary for warden targets (default target/release/warden, which is
//                        stripped: `cargo xtask profile` builds target/profiling/warden, with symbols)
//   --app-cpus, --loadgen-cpus, --loadgen   as for every suite (bench/lib.ts)
//
// Hardware counters (instructions, cycles) are used when the machine has them (perf stat); VMs often don't.

import { spawn, spawnSync, type Subprocess } from "bun";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  APP_CPUS, ROOT, SHIM, TMP, baseEnv, cpuNs, listenersOnPort, machine, median, oha, onAppCpus, parseArgs, sleep, sum, table,
  uniq, waitFor, type LoadResult,
} from "./lib.ts";
import type { Running } from "./static.ts";

// One server process unless told otherwise (bench/static.ts reads --workers when imported).
if (!process.argv.includes("--workers")) process.argv.push("--workers", "1");
const args = parseArgs();
const st = await import("./static.ts");

const TARGETS = (args.targets ?? "warden").split(",").filter(Boolean);
const REQUESTS = Number(args.requests ?? 200_000);
const CONNECTIONS = Number(args.connections ?? 64);
const WORKERS = Number(args.workers);
const ROUNDS = Math.max(1, Number(args.rounds ?? 1));
const KEEPALIVE = !process.argv.includes("--new-connections");
const PIN_SERVERS = process.argv.includes("--pin-servers");
const STRACE = process.argv.includes("--strace");
const PERF = process.argv.includes("--perf");
const CALL_GRAPH = process.argv.includes("--call-graph");
const TOP = Number(args.top ?? 25);
const APP_PORT = 3920;
// --file-size N: static targets also serve /blob-N.bin (N bytes), the default path.
const FILE_SIZE = Number(args["file-size"] ?? 0);
const STATIC_PATH = args.path ?? (FILE_SIZE ? `/blob-${FILE_SIZE}.bin` : "/index.html");

// ------------------------------------------------------------------ targets

interface Started {
  /** Every process of the target (manager included): CPU time is summed over these. */
  pids: number[];
  /** The processes that serve (strace and perf attach to these). */
  servers: number[];
  stop: () => Promise<void>;
}

interface Target {
  spec: string;
  port: number;
  path: string;
  start: () => Promise<Started>;
}

/** `name[:k=v,...][@path]` */
function parseTarget(spec: string): Target {
  const [nameAndKeys, override] = spec.split("@");
  const [name, keys] = nameAndKeys.split(/:(.*)/s);
  const staticExtra = (keys ?? "")
    .split(",")
    .filter(Boolean)
    .map((kv) => {
      const [k, v] = kv.split("=");
      if (!k || v === undefined) throw new Error(`target ${spec}: settings are key=value`);
      return `${k.trim()} = ${v.trim()}\n`;
    })
    .join("");
  const fromStatic = (run: () => Promise<Running>) => async (): Promise<Started> => {
    const r = await run();
    await waitFor(async () => (await get(st.PORT, "/index.html")) !== null, 30_000);
    await r.ready?.();
    const managers = new Set(r.managerPids());
    const pids = uniq(r.pids());
    return { pids, servers: pids.filter((p) => !managers.has(p)), stop: r.stop };
  };
  switch (name) {
    case "warden":
    case "warden-cache":
      return {
        spec,
        port: st.PORT,
        path: STATIC_PATH,
        start: fromStatic(() => st.startWarden({ cache: name === "warden-cache", bin: override ?? args.warden, staticExtra })),
      };
    case "nginx":
      if (override || keys) throw new Error("nginx takes no overrides");
      return { spec, port: st.PORT, path: STATIC_PATH, start: fromStatic(st.startNginx) };
    case "bun":
    case "bun-shim":
    case "node":
    case "node-shim":
      if (keys) throw new Error(`${name} takes no settings`);
      return {
        spec,
        port: APP_PORT,
        path: args.path ?? "/plaintext",
        start: () => startApp(name.startsWith("bun") ? "bun" : "node", name.endsWith("-shim") ? (override ?? SHIM) : null),
      };
  }
  throw new Error(`unknown target ${name}: warden, warden-cache, nginx, bun, bun-shim, node, node-shim`);
}

/** The bench app, N processes sharing the port; with `shim`, preloaded as Warden does
 *  (drain hooks on, heartbeat, reusePort), its reports going nowhere (no fd 3). */
async function startApp(runtime: "bun" | "node", shim: string | null): Promise<Started> {
  if (shim && !existsSync(shim)) throw new Error(`${shim} does not exist`);
  const entry = runtime === "bun" ? join(ROOT, "bench/app/server.ts") : join(ROOT, "bench/node/server.mjs");
  const procs: Subprocess[] = [];
  for (let i = 0; i < WORKERS; i++) {
    const env: Record<string, string> = { ...baseEnv, PORT: String(APP_PORT) };
    if (shim) Object.assign(env, { WARDEN_REUSE_PORT: "1", WARDEN_WORKER_ID: String(i + 1), WARDEN_HEARTBEAT_MS: "1000", WARDEN_DRAIN_MS: "500", WARDEN_APP: "bench" });
    const pre = shim ? [runtime === "bun" ? `--preload=${shim}` : `--import=${shim}`] : [];
    procs.push(spawn(onAppCpus([runtime, ...pre, entry]), { env, stdout: "ignore", stderr: "inherit" }));
  }
  await waitFor(() => listenersOnPort(APP_PORT) >= WORKERS, 30_000);
  const pids = procs.map((p) => p.pid);
  return {
    pids,
    servers: pids,
    stop: async () => {
      procs.forEach((p) => p.kill("SIGKILL"));
      await Promise.all(procs.map((p) => p.exited));
    },
  };
}

async function get(port: number, path: string): Promise<Uint8Array | null> {
  try {
    const r = await fetch(`http://127.0.0.1:${port}${path}`, { signal: AbortSignal.timeout(2000) });
    const b = new Uint8Array(await r.arrayBuffer());
    return r.status === 200 ? b : null;
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------- measuring

/** Context switches of every thread of `pid`. */
function ctxSwitches(pid: number): number {
  let n = 0;
  try {
    for (const tid of readdirSync(`/proc/${pid}/task`)) {
      try {
        const s = readFileSync(`/proc/${pid}/task/${tid}/status`, "utf8");
        for (const m of s.matchAll(/^(?:non)?voluntary_ctxt_switches:\s+(\d+)/gm)) n += Number(m[1]);
      } catch {}
    }
  } catch {}
  return n;
}

/** A perf binary that runs on this kernel (Ubuntu's /usr/bin/perf is a wrapper that
 *  refuses when the kernel's own linux-tools package is missing). */
function findPerf(): string | null {
  const works = (p: string) => {
    const r = spawnSync([p, "version"], { stdout: "pipe", stderr: "pipe" });
    return r.exitCode === 0 && /perf version/.test(r.stdout.toString());
  };
  const candidates = [args["perf-bin"], process.env.PERF, "perf"].filter(Boolean) as string[];
  for (const base of ["/usr/lib"]) {
    try {
      for (const d of readdirSync(base).filter((d) => d.startsWith("linux-tools"))) {
        candidates.push(join(base, d, "perf"));
        try {
          for (const sub of readdirSync(join(base, d))) candidates.push(join(base, d, sub, "perf"));
        } catch {}
      }
    } catch {}
  }
  return candidates.find((p) => (p.includes("/") ? existsSync(p) : true) && works(p)) ?? null;
}

interface Measure {
  rps: number;
  p50_ms: number;
  p99_ms: number;
  cpu_us: number;
  ctxsw: number;
  errors: number;
  /** syscall name -> calls per request (with --strace) */
  syscalls?: Record<string, number>;
  /** per request, from perf stat, when the machine has hardware counters */
  instructions?: number;
  cycles?: number;
  /** perf report lines (with --perf) */
  top?: string[];
}

function load(t: Target, requests: number): LoadResult {
  const r = oha(`http://127.0.0.1:${t.port}${t.path}`, 10, CONNECTIONS, { requests, keepalive: KEEPALIVE });
  if (r.errors > 0 || r.ok === 0) throw new Error(`${t.spec}: ${r.errors} errors, ${r.ok} ok`);
  return r;
}

/** Runs `tool` (attached to the servers) around a load of `requests`. */
async function attached(cmd: string[], t: Target, requests: number): Promise<LoadResult> {
  const p = spawn(cmd, { stdout: "ignore", stderr: "pipe" });
  await sleep(700); // attach
  const r = load(t, requests);
  p.kill("SIGINT");
  await p.exited;
  return r;
}

let perfBin: string | null | undefined;
let hwCounters: boolean | undefined;

/** CPUs in a taskset list like "0-3,6" (all of this machine's without one). */
function cpuList(list: string | undefined): number[] {
  if (!list) return Array.from({ length: navigator.hardwareConcurrency }, (_, i) => i);
  return list.split(",").flatMap((part) => {
    const [a, b] = part.split("-").map(Number);
    return b === undefined ? [a] : Array.from({ length: b - a + 1 }, (_, i) => a + i);
  });
}

async function measure(t: Target, withProfile: boolean): Promise<Measure> {
  const run = await t.start();
  if (PIN_SERVERS) {
    const cpus = cpuList(APP_CPUS);
    run.servers.forEach((pid, i) => {
      const r = spawnSync(["taskset", "-a", "-p", "-c", String(cpus[i % cpus.length]), String(pid)], { stdout: "ignore", stderr: "pipe" });
      if (r.exitCode !== 0) throw new Error(`taskset -p ${pid}: ${r.stderr.toString()}`);
    });
  }
  try {
    load(t, Math.max(2000, Math.round(REQUESTS / 5))); // warm-up
    const c0 = sum(run.pids, cpuNs);
    const s0 = sum(run.pids, ctxSwitches);
    const r = load(t, REQUESTS);
    const m: Measure = {
      rps: r.rps,
      p50_ms: r.p50_ms,
      p99_ms: r.p99_ms,
      cpu_us: +((sum(run.pids, cpuNs) - c0) / 1000 / r.ok).toFixed(3),
      ctxsw: +((sum(run.pids, ctxSwitches) - s0) / r.ok).toFixed(3),
      errors: r.errors,
    };
    const servers = run.servers.map(String);
    if (perfBin === undefined && (PERF || hwCounters === undefined)) perfBin = findPerf();
    if (perfBin && hwCounters !== false) {
      // Instructions and cycles per request, where the machine counts them.
      const out = join(TMP, "perf-stat.txt");
      const r2 = await attached([perfBin, "stat", "-x", ",", "-o", out, "-e", "instructions,cycles", "-p", servers.join(",")], t, REQUESTS);
      const text = existsSync(out) ? readFileSync(out, "utf8") : "";
      const count = (ev: string) => Number(text.split("\n").find((l) => l.includes(`,${ev}`))?.split(",")[0]);
      hwCounters = Number.isFinite(count("instructions"));
      if (hwCounters) {
        m.instructions = Math.round(count("instructions") / r2.ok);
        m.cycles = Math.round(count("cycles") / r2.ok);
      }
    }
    if (STRACE) {
      const out = join(TMP, "strace.txt");
      rmSync(out, { force: true });
      const n = Math.max(2000, Math.round(REQUESTS / 10));
      const r2 = await attached(["strace", "-c", "-f", "-q", "-o", out, ...servers.flatMap((p) => ["-p", p])], t, n);
      m.syscalls = parseStrace(readFileSync(out, "utf8"), r2.ok);
    }
    if (PERF && withProfile) {
      if (!perfBin) throw new Error("--perf: no working perf binary (install linux-tools, or pass --perf-bin PATH)");
      const data = join(TMP, "perf.data");
      rmSync(data, { force: true });
      await attached(
        [perfBin, "record", "-q", "-e", "cpu-clock", "-F", "1999", ...(CALL_GRAPH ? ["-g"] : []), "-o", data, "-p", servers.join(",")],
        t,
        REQUESTS,
      );
      const rep = spawnSync(
        [perfBin, "report", "-i", data, "--stdio", "--no-children", "-F", "overhead,dso,sym", "--percent-limit", "0.2"],
        { stdout: "pipe", stderr: "pipe" },
      );
      m.top = rep.stdout
        .toString()
        .split("\n")
        .filter((l) => l.trim() && !l.startsWith("#"))
        .map((l) => l.replace(/\s+$/, "").replace(/\s{3,}/g, "  "))
        .map((l) => (l.length > 140 ? l.slice(0, 137) + "..." : l))
        .slice(0, CALL_GRAPH ? TOP * 6 : TOP);
      rmSync(data, { force: true });
    }
    return m;
  } finally {
    await run.stop();
    await waitFor(() => listenersOnPort(t.port) === 0, 15_000).catch(() => {});
  }
}

/** strace -c's table -> calls per request by syscall (plus "total"). */
function parseStrace(text: string, requests: number): Record<string, number> {
  const out: Record<string, number> = {};
  for (const line of text.split("\n")) {
    const f = line.trim().split(/\s+/);
    // % time, seconds, usecs/call, calls, [errors], syscall
    if (f.length < 5 || !/^\d/.test(f[0])) continue;
    const name = f[f.length - 1];
    const calls = Number(f[3]);
    if (Number.isFinite(calls)) out[name] = +(calls / requests).toFixed(3);
  }
  const total = text.split("\n").find((l) => /\btotal$/.test(l.trim()));
  if (total) out.total = +(Number(total.trim().split(/\s+/)[3]) / requests).toFixed(3);
  return out;
}

// --------------------------------------------------------------------- main

mkdirSync(TMP, { recursive: true });
const targets = TARGETS.map(parseTarget);
if (targets.some((t) => t.port === st.PORT)) {
  st.makeSite();
  if (FILE_SIZE) writeFileSync(join(st.SITE, `blob-${FILE_SIZE}.bin`), new Uint8Array(FILE_SIZE).fill(97));
  // Warden's cache leaves a file changed in the last 2 s uncached (by its
  // ctime, which a new file can't set back): let the site age first.
  await sleep(2500);
}
const meta = machine();
console.error(
  JSON.stringify({ ...meta, targets: TARGETS, requests: REQUESTS, connections: CONNECTIONS, workers: WORKERS, rounds: ROUNDS, keepalive: KEEPALIVE }),
);
const runs = new Map<string, Measure[]>(targets.map((t) => [t.spec, []]));
for (let round = 1; round <= ROUNDS; round++) {
  for (const t of targets) {
    if (listenersOnPort(t.port) !== 0) throw new Error(`port ${t.port} is busy`);
    const m = await measure(t, round === ROUNDS);
    console.error(`round ${round}/${ROUNDS} ${t.spec}: ${JSON.stringify({ ...m, top: undefined })}`);
    runs.get(t.spec)!.push(m);
  }
}

const med = (spec: string, f: (m: Measure) => number | undefined) => {
  const v = median(runs.get(spec)!.map(f).filter((x): x is number => x !== undefined));
  return Number.isFinite(v) ? +v.toFixed(3) : "-";
};
const rows: [string, (spec: string) => string | number][] = [
  ["req/s", (s) => med(s, (m) => m.rps)],
  ["p50 / p99 (ms)", (s) => `${med(s, (m) => m.p50_ms)} / ${med(s, (m) => m.p99_ms)}`],
  ["server CPU per request (µs)", (s) => med(s, (m) => m.cpu_us)],
  ["context switches per request", (s) => med(s, (m) => m.ctxsw)],
];
if (hwCounters) {
  rows.push(["instructions per request", (s) => med(s, (m) => m.instructions)]);
  rows.push(["cycles per request", (s) => med(s, (m) => m.cycles)]);
}
if (STRACE) {
  // Every syscall that any target makes at least once per 100 requests.
  const names = new Set<string>();
  for (const ms of runs.values()) for (const m of ms) for (const [k, v] of Object.entries(m.syscalls ?? {})) if (k !== "total" && v >= 0.01) names.add(k);
  rows.push(["syscalls per request", (s) => med(s, (m) => m.syscalls?.total)]);
  for (const n of [...names].sort()) rows.push([`  ${n}`, (s) => med(s, (m) => m.syscalls?.[n] ?? 0)]);
}
console.log(
  `${REQUESTS} requests to ${[...new Set(targets.map((t) => t.path))].join(", ")}, ${CONNECTIONS} ${KEEPALIVE ? "keep-alive" : "new"} connections, ` +
    `${WORKERS} server process(es), ${meta.loadgen}; ${ROUNDS > 1 ? `median of ${ROUNDS} interleaved rounds` : "one round"}; ` +
    `app CPUs ${meta.app_cpus}${PIN_SERVERS ? " (servers pinned one per CPU)" : ""}, load generator CPUs ${meta.loadgen_cpus}`,
);
console.log("");
console.log(table(TARGETS, (s) => s, rows));
if (PERF) {
  for (const t of TARGETS) {
    const top = runs.get(t)!.at(-1)?.top ?? [];
    console.log(`\n${t}: where the servers' CPU time goes (perf record -e cpu-clock, last round)\n`);
    console.log("```\n" + top.join("\n") + "\n```");
  }
}
if (hwCounters === false) console.error("note: no hardware counters here (instructions/cycles): a VM? CPU time per request stands in.");

// Shared helpers for the benchmark scripts (run.ts, static.ts, logs.ts,
// fleet.ts): /proc sampling, the load generator, run metadata and output.
import { spawnSync } from "bun";
import { existsSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";

export const ROOT = resolve(import.meta.dir, "..");
export const TMP = join(ROOT, "bench/.run");
export const BIN = join(ROOT, "bench/node_modules/.bin");
export const WARDEN = join(ROOT, "target/release/warden");
export const SHIM = join(ROOT, "shim/warden-shim.mjs");
const HZ = Number(spawnSync(["getconf", "CLK_TCK"]).stdout.toString().trim() || 100);
const PAGE = Number(spawnSync(["getconf", "PAGESIZE"]).stdout.toString().trim() || 4096);

/** `--key value` pairs from argv. */
export function parseArgs(): Record<string, string> {
  return Object.fromEntries(
    process.argv.slice(2).reduce<[string, string][]>((acc, a, i, all) => {
      if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
      return acc;
    }, []),
  );
}

/** The environment without anything that changes what we measure. */
export const baseEnv: Record<string, string> = Object.fromEntries(
  Object.entries(process.env).filter(([k]) => k !== "BUN_OPTIONS" && k !== "NODE_OPTIONS") as [string, string][],
);

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function waitFor(cond: () => boolean | Promise<boolean>, timeoutMs: number): Promise<number> {
  const t0 = performance.now();
  while (performance.now() - t0 < timeoutMs) {
    if (await cond()) return performance.now() - t0;
    await sleep(5);
  }
  throw new Error("timeout");
}

// ---------------------------------------------------------------- /proc helpers

export function listenersOnPort(port: number): number {
  const hex = port.toString(16).toUpperCase().padStart(4, "0");
  let n = 0;
  for (const t of ["/proc/net/tcp", "/proc/net/tcp6"]) {
    if (!existsSync(t)) continue;
    for (const line of readFileSync(t, "utf8").split("\n").slice(1)) {
      const f = line.trim().split(/\s+/);
      if (f.length > 3 && f[3] === "0A" && f[1].endsWith(":" + hex)) n++;
    }
  }
  return n;
}

export function rss(pid: number): number {
  try {
    return Number(readFileSync(`/proc/${pid}/statm`, "utf8").split(" ")[1]) * PAGE;
  } catch {
    return 0;
  }
}

/** Proportional set size: shared pages (the same binary or libraries in
 *  several processes) split between the processes that map them. The fair
 *  "memory used" for a group of processes; RSS counts shared pages in each. */
export function pss(pid: number): number {
  try {
    const m = readFileSync(`/proc/${pid}/smaps_rollup`, "utf8").match(/^Pss:\s+(\d+) kB/m);
    return m ? Number(m[1]) * 1024 : 0;
  } catch {
    return 0;
  }
}

export function cpuSeconds(pid: number): number {
  try {
    const s = readFileSync(`/proc/${pid}/stat`, "utf8");
    const f = s.slice(s.lastIndexOf(")") + 2).split(" ");
    return (Number(f[11]) + Number(f[12])) / HZ;
  } catch {
    return 0;
  }
}

/** CPU time of every thread of `pid` in nanoseconds (/proc/<pid>/task/<tid>/schedstat:
 *  exact, where /proc/<pid>/stat counts 10 ms ticks). Falls back to ticks. */
export function cpuNs(pid: number): number {
  try {
    let ns = 0;
    for (const tid of readdirSync(`/proc/${pid}/task`)) {
      try {
        ns += Number(readFileSync(`/proc/${pid}/task/${tid}/schedstat`, "utf8").split(" ")[0]);
      } catch {} // the thread just exited
    }
    return ns;
  } catch {
    return cpuSeconds(pid) * 1e9;
  }
}

export const median = (xs: number[]) => {
  const s = xs.filter((x) => Number.isFinite(x)).sort((a, b) => a - b);
  if (!s.length) return NaN;
  return s.length % 2 ? s[(s.length - 1) / 2] : (s[s.length / 2 - 1] + s[s.length / 2]) / 2;
};

/** Several runs of the same measurement as one: the median of every
 *  number (field by field, nested objects too), the first run's value for
 *  anything else. For `--rounds N` (interleaved A/B runs). */
export function medianOf<T>(runs: T[]): T {
  const first = runs[0] as any;
  if (runs.every((r) => typeof r === "number")) {
    const m = median(runs as number[]);
    return (runs.every((r) => Number.isInteger(r)) ? Math.round(m) : +m.toFixed(2)) as T;
  }
  if (first && typeof first === "object" && !Array.isArray(first)) {
    const out: any = {};
    for (const k of Object.keys(first)) out[k] = medianOf(runs.map((r: any) => r?.[k]));
    return out;
  }
  return first;
}

/** Direct children of `pid`. */
export function childrenOf(pid: number): number[] {
  const out: number[] = [];
  for (const d of spawnSync(["ls", "/proc"]).stdout.toString().split("\n")) {
    if (!/^\d+$/.test(d)) continue;
    try {
      const s = readFileSync(`/proc/${d}/stat`, "utf8");
      if (Number(s.slice(s.lastIndexOf(")") + 2).split(" ")[1]) === pid) out.push(Number(d));
    } catch {}
  }
  return out;
}

export const uniq = (pids: number[]) => [...new Set(pids.filter((p) => p > 0))];
export const sum = (pids: number[], f: (p: number) => number) => uniq(pids).reduce((a, p) => a + f(p), 0);
export const mb = (b: number) => Math.round((b / 1048576) * 10) / 10;

// ---------------------------------------------------------------------- load

export interface LoadResult {
  rps: number;
  p50_ms: number;
  p95_ms: number;
  p99_ms: number;
  mb_per_s: number;
  ok: number;
  errors: number;
}

// Where things run and what generates the load, for every suite:
//   --app-cpus 0-3        pin the apps and their manager to these CPUs (taskset)
//   --loadgen-cpus 4-7    pin the load generator to these CPUs
//   --loadgen oha|wrk     oha (default) or wrk (lighter: more headroom on small machines)
// (or BENCH_APP_CPUS, BENCH_LOADGEN_CPUS, BENCH_LOADGEN in the environment).
const globalArgs = parseArgs();
export const APP_CPUS: string | undefined = globalArgs["app-cpus"] ?? process.env.BENCH_APP_CPUS;
export const LOADGEN_CPUS: string | undefined = globalArgs["loadgen-cpus"] ?? process.env.BENCH_LOADGEN_CPUS;
export const LOADGEN: string = globalArgs["loadgen"] ?? process.env.BENCH_LOADGEN ?? "oha";
if (LOADGEN !== "oha" && LOADGEN !== "wrk") throw new Error(`--loadgen ${LOADGEN}: oha or wrk`);

/** `cmd` pinned to the app CPUs (children inherit the affinity). */
export const onAppCpus = (cmd: string[]) => (APP_CPUS ? ["taskset", "-c", APP_CPUS, ...cmd] : cmd);
const onLoadgenCpus = (cmd: string[]) => (LOADGEN_CPUS ? ["taskset", "-c", LOADGEN_CPUS, ...cmd] : cmd);

/** CPUs in a taskset list like "0-3,6". */
function cpuCount(list: string | undefined): number {
  if (!list) return navigator.hardwareConcurrency;
  return list.split(",").reduce((n, part) => {
    const [a, b] = part.split("-").map(Number);
    return n + (b === undefined ? 1 : b - a + 1);
  }, 0);
}

export interface LoadOptions {
  keepalive?: boolean;
  headers?: string[];
  /** A fixed number of requests instead of `seconds` (oha only; wrk runs for `seconds`):
   *  per-request costs then divide by an exact count. */
  requests?: number;
}

/** A load test against `url` for `seconds` with the configured generator. */
export function oha(url: string, seconds: number, connections: number, opts: LoadOptions = {}): LoadResult {
  return LOADGEN === "wrk" ? wrk(url, seconds, connections, opts) : ohaRun(url, seconds, connections, opts);
}

function ohaRun(url: string, seconds: number, connections: number, opts: LoadOptions): LoadResult {
  const amount = opts.requests ? ["-n", String(opts.requests)] : ["-z", `${seconds}s`];
  const cmd = ["oha", ...amount, "-c", String(connections), "--no-tui", "--output-format", "json"];
  if (opts.keepalive === false) cmd.push("--disable-keepalive");
  for (const h of opts.headers ?? []) cmd.push("-H", h);
  cmd.push(url);
  const r = spawnSync(onLoadgenCpus(cmd), { stdout: "pipe", stderr: "pipe" });
  const j = JSON.parse(r.stdout.toString());
  const ok = j.statusCodeDistribution?.["200"] ?? 0;
  // oha cancels the requests still in flight when -z expires; those are not errors.
  const errors = Object.entries<number>(j.errorDistribution ?? {})
    .filter(([k]) => k !== "aborted due to deadline")
    .reduce((a, [, n]) => a + n, 0);
  const lat = j.latencyPercentiles;
  return {
    rps: Math.round(j.summary.requestsPerSec),
    p50_ms: +(lat.p50 * 1000).toFixed(2),
    p95_ms: +(lat.p95 * 1000).toFixed(2),
    p99_ms: +(lat.p99 * 1000).toFixed(2),
    mb_per_s: +((j.summary.sizePerSec ?? 0) / 1048576).toFixed(1),
    ok,
    errors,
  };
}

// wrk prints its summary as text; this script prints the exact numbers.
const WRK_REPORT = `done = function(s, lat, req)
  local e = s.errors
  io.write(string.format('WRKJSON {"requests":%d,"bytes":%d,"duration_us":%d,"errors":%d,"non2xx":%d,"p50":%d,"p95":%d,"p99":%d}\\n',
    s.requests, s.bytes, s.duration, e.connect + e.read + e.write + e.timeout, e.status,
    lat:percentile(50), lat:percentile(95), lat:percentile(99)))
end
`;

function wrk(url: string, seconds: number, connections: number, opts: LoadOptions): LoadResult {
  const script = join(TMP, "wrk-report.lua");
  mkdirSync(TMP, { recursive: true });
  writeFileSync(script, WRK_REPORT);
  const threads = Math.max(1, Math.min(cpuCount(LOADGEN_CPUS), connections));
  const cmd = ["wrk", "-t", String(threads), "-c", String(connections), "-d", `${seconds}s`, "--timeout", "2s", "-s", script];
  if (opts.keepalive === false) cmd.push("-H", "Connection: close");
  for (const h of opts.headers ?? []) cmd.push("-H", h);
  cmd.push(url);
  const r = spawnSync(onLoadgenCpus(cmd), { stdout: "pipe", stderr: "pipe" });
  const line = r.stdout.toString().split("\n").find((l) => l.startsWith("WRKJSON "));
  if (!line) throw new Error(`wrk failed: ${r.stderr.toString() || r.stdout.toString()}`);
  const j = JSON.parse(line.slice("WRKJSON ".length));
  const secs = j.duration_us / 1e6;
  return {
    rps: Math.round(j.requests / secs),
    p50_ms: +(j.p50 / 1000).toFixed(2),
    p95_ms: +(j.p95 / 1000).toFixed(2),
    p99_ms: +(j.p99 / 1000).toFixed(2),
    mb_per_s: +(j.bytes / secs / 1048576).toFixed(1),
    ok: j.requests - j.non2xx,
    errors: j.errors + j.non2xx,
  };
}

// ------------------------------------------------------------------ metadata

export const version = (cmd: string[]) => {
  try {
    return spawnSync(cmd).stdout.toString().trim().split("\n")[0];
  } catch {
    return "not installed";
  }
};

/** A package's version from bench/node_modules (running `pm2 --version` would start a daemon). */
export function pkgVersion(name: string): string {
  try {
    return JSON.parse(readFileSync(join(ROOT, "bench/node_modules", name, "package.json"), "utf8")).version;
  } catch {
    return "not installed";
  }
}

export function machine() {
  return {
    date: new Date().toISOString(),
    kernel: version(["uname", "-rm"]),
    cpus: navigator.hardwareConcurrency,
    cpu_model: (readFileSync("/proc/cpuinfo", "utf8").match(/model name\s*:\s*(.*)/) ?? [])[1] ?? "unknown",
    warden: version([WARDEN, "version"]),
    loadgen: LOADGEN === "wrk" ? version(["wrk", "--version"]).split(" [")[0] : version(["oha", "--version"]),
    app_cpus: APP_CPUS ?? "all",
    loadgen_cpus: LOADGEN_CPUS ?? "all",
    tcp_migrate_req: existsSync("/proc/sys/net/ipv4/tcp_migrate_req")
      ? readFileSync("/proc/sys/net/ipv4/tcp_migrate_req", "utf8").trim()
      : "n/a",
  };
}

/** Writes bench/results/<date>-<name>.json; returns the path. */
export function saveResults(name: string, data: { meta: { date: string } } & Record<string, unknown>): string {
  const out = join(ROOT, "bench/results", `${data.meta.date.slice(0, 19).replace(/:/g, "")}-${name}.json`);
  mkdirSync(join(ROOT, "bench/results"), { recursive: true });
  writeFileSync(out, JSON.stringify(data, null, 2));
  return out;
}

/** A Markdown table: one column per result. */
export function table<T>(results: T[], name: (r: T) => string, rows: [string, (r: T) => string | number][]): string {
  return [
    `| | ${results.map(name).join(" | ")} |`,
    `|---|${results.map(() => "---").join("|")}|`,
    ...rows.map(([label, f]) => `| ${label} | ${results.map((r) => f(r)).join(" | ")} |`),
  ].join("\n");
}

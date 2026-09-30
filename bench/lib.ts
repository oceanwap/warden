// Shared helpers for the benchmark scripts (run.ts, static.ts, logs.ts,
// fleet.ts): /proc sampling, the load generator, run metadata and output.
import { spawnSync } from "bun";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
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

export function cpuSeconds(pid: number): number {
  try {
    const s = readFileSync(`/proc/${pid}/stat`, "utf8");
    const f = s.slice(s.lastIndexOf(")") + 2).split(" ");
    return (Number(f[11]) + Number(f[12])) / HZ;
  } catch {
    return 0;
  }
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

/** oha against `url` for `seconds`. */
export function oha(url: string, seconds: number, connections: number, opts: { keepalive?: boolean; headers?: string[] } = {}): LoadResult {
  const cmd = ["oha", "-z", `${seconds}s`, "-c", String(connections), "--no-tui", "--output-format", "json"];
  if (opts.keepalive === false) cmd.push("--disable-keepalive");
  for (const h of opts.headers ?? []) cmd.push("-H", h);
  cmd.push(url);
  const r = spawnSync(cmd, { stdout: "pipe", stderr: "pipe" });
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

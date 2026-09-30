#!/usr/bin/env bun
// Benchmark harness (PRD §19). Runs the same app four ways on one port and
// measures startup, memory, CPU, throughput, latency, errors, restart time
// and connection distribution.
//
//   A  bare     4 × `bun app` started directly (what 4 systemd units do)
//   B  pm2      PM2, 4 fork-mode instances with the bun interpreter
//   C  process  warden, mode = "process", 4 workers
//   D  worker   warden, mode = "worker", 4 Bun Workers in one process
//
// Usage: bun bench/run.ts [--app bench|nest] [--duration 10] [--connections 64]
//                         [--scenarios A,B,C,D] [--workers 4]
// Needs: oha (cargo install oha), target/release/warden, bench/node_modules (pm2).
// Linux only (reads /proc).

import { spawn, spawnSync, type Subprocess } from "bun";
import { existsSync, mkdirSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const args = Object.fromEntries(
  process.argv.slice(2).reduce<[string, string][]>((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
    return acc;
  }, []),
);
const APP = args.app ?? "bench";
const DURATION = Number(args.duration ?? 10);
const CONNECTIONS = Number(args.connections ?? 64);
const WORKERS = Number(args.workers ?? 4);
const SCENARIOS = (args.scenarios ?? "A,B,C,D").split(",");
const PORT = 3900;
const WARDEN = join(ROOT, "target/release/warden");
const SHIM = join(ROOT, "shim/warden-shim.mjs");
const TMP = join(ROOT, "bench/.run");
const HZ = Number(spawnSync(["getconf", "CLK_TCK"]).stdout.toString().trim() || 100);

const apps: Record<string, { entry: string; cwd: string; paths: string[]; needsShim: boolean }> = {
  bench: { entry: join(ROOT, "bench/app/server.ts"), cwd: join(ROOT, "bench/app"), paths: ["/plaintext", "/json", "/cpu"], needsShim: false },
  nest: { entry: join(ROOT, "bench/nest/main.ts"), cwd: join(ROOT, "bench/nest"), paths: ["/plaintext", "/json", "/cpu"], needsShim: true },
};
const app = apps[APP];
if (!app) throw new Error(`unknown app ${APP}`);

// The sandbox this was developed in exported BUN_OPTIONS=--smol; never let it leak in.
const baseEnv: Record<string, string> = Object.fromEntries(
  Object.entries(process.env).filter(([k]) => k !== "BUN_OPTIONS") as [string, string][],
);

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

// ---------------------------------------------------------------- /proc helpers

function listenersOnPort(port: number): number {
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

function rss(pid: number): number {
  try {
    return Number(readFileSync(`/proc/${pid}/statm`, "utf8").split(" ")[1]) * 4096;
  } catch {
    return 0;
  }
}

function cpuSeconds(pid: number): number {
  try {
    const s = readFileSync(`/proc/${pid}/stat`, "utf8");
    const f = s.slice(s.lastIndexOf(")") + 2).split(" ");
    return (Number(f[11]) + Number(f[12])) / HZ;
  } catch {
    return 0;
  }
}

const sum = (pids: number[], f: (p: number) => number) => pids.reduce((a, p) => a + f(p), 0);
const mb = (b: number) => Math.round((b / 1048576) * 10) / 10;

async function waitFor(cond: () => boolean, timeoutMs: number): Promise<number> {
  const t0 = performance.now();
  while (performance.now() - t0 < timeoutMs) {
    if (cond()) return performance.now() - t0;
    await sleep(5);
  }
  throw new Error("timeout");
}

// ------------------------------------------------------------------- scenarios

interface Running {
  appPids: () => Promise<number[]>;
  supervisorPids: () => Promise<number[]>;
  /** A pid to SIGKILL for the restart test, or null if nothing would restart it. */
  victim: () => Promise<number | null>;
  stop: () => Promise<void>;
}

function appArgs(): string[] {
  return app.needsShim ? [`--preload=${SHIM}`, app.entry] : [app.entry];
}
const appEnv = { ...baseEnv, PORT: String(PORT), WARDEN_REUSE_PORT: "1", WARDEN_DRAIN_MS: "0" };

async function startBare(): Promise<Running> {
  const procs: Subprocess[] = [];
  for (let i = 0; i < WORKERS; i++) {
    procs.push(spawn(["bun", ...appArgs()], { cwd: app.cwd, env: appEnv, stdout: "ignore", stderr: "ignore" }));
  }
  return {
    appPids: async () => procs.filter((p) => !p.killed && p.exitCode === null).map((p) => p.pid),
    supervisorPids: async () => [],
    victim: async () => null,
    stop: async () => {
      procs.forEach((p) => p.kill("SIGKILL"));
      await Promise.all(procs.map((p) => p.exited));
    },
  };
}

async function startPm2(): Promise<Running> {
  const pm2 = join(ROOT, "bench/node_modules/.bin/pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(TMP, "pm2");
  rmSync(home, { recursive: true, force: true });
  mkdirSync(home, { recursive: true });
  const env = { ...appEnv, PM2_HOME: home };
  const eco = join(TMP, "ecosystem.config.cjs");
  writeFileSync(
    eco,
    `module.exports = { apps: [{ name: "bench", script: ${JSON.stringify(app.entry)}, interpreter: "bun",
      interpreter_args: ${JSON.stringify(app.needsShim ? `--preload=${SHIM}` : "")}, cwd: ${JSON.stringify(app.cwd)},
      exec_mode: "fork", instances: ${WORKERS}, autorestart: true, restart_delay: 0,
      env: ${JSON.stringify({ PORT: String(PORT), WARDEN_REUSE_PORT: "1", WARDEN_DRAIN_MS: "0" })} }] };`,
  );
  const r = spawnSync([pm2, "start", eco], { env, stdout: "ignore", stderr: "pipe" });
  if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  const jlist = () => JSON.parse(spawnSync([pm2, "jlist"], { env }).stdout.toString() || "[]") as any[];
  return {
    appPids: async () => jlist().map((p) => p.pid).filter((p: number) => p > 0),
    supervisorPids: async () => {
      try {
        return [Number(readFileSync(join(home, "pm2.pid"), "utf8"))];
      } catch {
        return [];
      }
    },
    victim: async () => jlist()[1]?.pid ?? null,
    stop: async () => {
      spawnSync([pm2, "kill"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

async function startWarden(mode: "process" | "worker"): Promise<Running> {
  if (!existsSync(WARDEN)) throw new Error("build first: cargo build --release");
  const sock = join(TMP, `warden-${mode}.sock`);
  const cfg = join(TMP, `warden-${mode}.toml`);
  const appCfg =
    mode === "process"
      ? `args = ${JSON.stringify([app.entry])}`
      : `entry = ${JSON.stringify(app.entry)}`;
  writeFileSync(
    cfg,
    `[app]\nname = "bench-${mode}"\n${appCfg}\nworking_directory = ${JSON.stringify(app.cwd)}\nport = ${PORT}\n` +
      `[workers]\ncount = ${WORKERS}\nmode = "${mode}"\n[restart]\nbackoff_initial = 100\n` +
      `[shutdown]\ngrace_period = 10\ndrain_ms = 0\n[logging]\nlevel = "warn"\n[control]\nsocket = ${JSON.stringify(sock)}\n`,
  );
  const w = spawn([WARDEN, "start", "-c", cfg], { env: baseEnv, stdout: "ignore", stderr: "ignore" });
  const status = async () => {
    const r = spawnSync([WARDEN, "status", "--json", "--socket", sock]);
    return r.exitCode === 0 ? JSON.parse(r.stdout.toString()) : null;
  };
  const pids = async () => {
    const s = await status();
    if (!s) return [];
    return [...new Set<number>(s.workers.map((x: any) => x.pid).filter(Boolean))];
  };
  return {
    appPids: pids,
    supervisorPids: async () => [w.pid],
    victim: async () => (await pids())[mode === "process" ? 1 : 0] ?? null,
    stop: async () => {
      w.kill("SIGTERM");
      await w.exited;
    },
  };
}

// --------------------------------------------------------------------- measure

function oha(path: string, seconds: number) {
  const r = spawnSync(
    ["oha", "-z", `${seconds}s`, "-c", String(CONNECTIONS), "--no-tui", "--output-format", "json", `http://127.0.0.1:${PORT}${path}`],
    { stdout: "pipe", stderr: "pipe" },
  );
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
    ok,
    errors,
    error_rate: ok + errors ? +(errors / (ok + errors)).toFixed(5) : 0,
  };
}

async function distribution(n = 400): Promise<Record<string, number>> {
  const counts: Record<string, number> = {};
  for (let i = 0; i < n; i++) {
    try {
      const r = await fetch(`http://127.0.0.1:${PORT}/whoami`, { headers: { connection: "close" }, keepalive: false });
      const t = await r.text();
      counts[t] = (counts[t] ?? 0) + 1;
    } catch {
      counts.error = (counts.error ?? 0) + 1;
    }
  }
  return counts;
}

async function runScenario(key: string) {
  const name = { A: "bare", B: "pm2", C: "warden-process", D: "warden-worker" }[key]!;
  if (listenersOnPort(PORT) !== 0) throw new Error(`port ${PORT} busy`);
  const t0 = performance.now();
  const run =
    key === "A" ? await startBare() : key === "B" ? await startPm2() : key === "C" ? await startWarden("process") : await startWarden("worker");
  await waitFor(() => listenersOnPort(PORT) >= WORKERS, 60_000);
  const startup_ms = Math.round(performance.now() - t0);
  await sleep(1500);
  const appPids = await run.appPids();
  const supPids = await run.supervisorPids();
  const idle = { app_rss_mb: mb(sum(appPids, rss)), supervisor_rss_mb: mb(sum(supPids, rss)) };
  const dist = await distribution();

  const endpoints: Record<string, any> = {};
  for (const path of app.paths) {
    oha(path, 2); // warm-up
    const a0 = sum(appPids, cpuSeconds), s0 = sum(supPids, cpuSeconds);
    const r = oha(path, DURATION);
    endpoints[path] = {
      ...r,
      app_cpu_s: +(sum(appPids, cpuSeconds) - a0).toFixed(2),
      supervisor_cpu_s: +(sum(supPids, cpuSeconds) - s0).toFixed(3),
    };
  }
  const loaded = { app_rss_mb: mb(sum(appPids, rss)), supervisor_rss_mb: mb(sum(supPids, rss)) };

  // Supervisor idle CPU over 5 s.
  const s1 = sum(supPids, cpuSeconds);
  await sleep(5000);
  const supervisor_idle_cpu_pct = +(((sum(supPids, cpuSeconds) - s1) / 5) * 100).toFixed(2);

  // Restart: SIGKILL one worker process; time until all listeners are back.
  let restart: any = "n/a (nothing restarts it)";
  const victim = await run.victim();
  if (victim) {
    process.kill(victim, "SIGKILL");
    const tk = performance.now();
    await waitFor(() => listenersOnPort(PORT) < WORKERS, 2000).catch(() => 0);
    const lostAll = listenersOnPort(PORT) === 0;
    try {
      await waitFor(() => listenersOnPort(PORT) >= WORKERS, 30_000);
      restart = { ms: Math.round(performance.now() - tk), all_workers_down: lostAll };
    } catch {
      restart = "did not recover within 30 s";
    }
  }
  await run.stop();
  await waitFor(() => listenersOnPort(PORT) === 0, 15_000).catch(() => {});
  return { scenario: key, name, startup_ms, idle, loaded, supervisor_idle_cpu_pct, distribution: Object.values(dist).sort((a, b) => b - a), endpoints, restart };
}

// ------------------------------------------------------------------------ main

mkdirSync(TMP, { recursive: true });
const meta = {
  date: new Date().toISOString(),
  app: APP,
  workers: WORKERS,
  duration_s: DURATION,
  connections: CONNECTIONS,
  bun: spawnSync(["bun", "--version"]).stdout.toString().trim(),
  kernel: spawnSync(["uname", "-rm"]).stdout.toString().trim(),
  cpus: navigator.hardwareConcurrency,
  tcp_migrate_req: existsSync("/proc/sys/net/ipv4/tcp_migrate_req") ? readFileSync("/proc/sys/net/ipv4/tcp_migrate_req", "utf8").trim() : "n/a",
};
console.error(JSON.stringify(meta));
const results = [];
for (const s of SCENARIOS) {
  console.error(`--- scenario ${s}`);
  const r = await runScenario(s);
  console.error(JSON.stringify(r));
  results.push(r);
}
const out = join(ROOT, "bench/results", `${meta.date.slice(0, 19).replace(/:/g, "")}-${APP}.json`);
mkdirSync(join(ROOT, "bench/results"), { recursive: true });
writeFileSync(out, JSON.stringify({ meta, results }, null, 2));

// Markdown summary.
const lines = [
  `| | ${results.map((r) => r.name).join(" | ")} |`,
  `|---|${results.map(() => "---").join("|")}|`,
  `| startup to ${WORKERS} listeners (ms) | ${results.map((r) => r.startup_ms).join(" | ")} |`,
  `| app RSS idle (MB) | ${results.map((r) => r.idle.app_rss_mb).join(" | ")} |`,
  `| app RSS after load (MB) | ${results.map((r) => r.loaded.app_rss_mb).join(" | ")} |`,
  `| supervisor RSS (MB) | ${results.map((r) => r.loaded.supervisor_rss_mb).join(" | ")} |`,
  `| supervisor idle CPU (%) | ${results.map((r) => r.supervisor_idle_cpu_pct).join(" | ")} |`,
  `| connections per worker (400 fresh) | ${results.map((r) => r.distribution.join("/")).join(" | ")} |`,
  `| restart after SIGKILL of one worker (ms) | ${results.map((r) => (typeof r.restart === "string" ? r.restart : `${r.restart.ms}${r.restart.all_workers_down ? " (all down)" : ""}`)).join(" | ")} |`,
];
for (const p of app.paths) {
  lines.push(`| ${p} req/s | ${results.map((r) => r.endpoints[p].rps).join(" | ")} |`);
  lines.push(`| ${p} p50 / p95 / p99 (ms) | ${results.map((r) => `${r.endpoints[p].p50_ms} / ${r.endpoints[p].p95_ms} / ${r.endpoints[p].p99_ms}`).join(" | ")} |`);
  lines.push(`| ${p} errors | ${results.map((r) => r.endpoints[p].errors).join(" | ")} |`);
  lines.push(`| ${p} app CPU s / supervisor CPU s | ${results.map((r) => `${r.endpoints[p].app_cpu_s} / ${r.endpoints[p].supervisor_cpu_s}`).join(" | ")} |`);
}
console.log(lines.join("\n"));
console.error(`results: ${out}`);

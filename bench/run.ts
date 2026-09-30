#!/usr/bin/env bun
// Benchmark harness: the same app under several process managers, on one
// port, measuring memory, CPU, startup, crash recovery, throughput, latency
// and errors. Results go to bench/results/<date>-<app>.json plus a Markdown
// table on stdout. See bench/README.md.
//
//   bun bench/run.ts --app bun-http   (Bun.serve app)     scenarios: bare,pm2,warden-process,warden-worker
//   bun bench/run.ts --app node-http  (node:http app)     scenarios: bare,pm2,watt,warden-process
//   bun bench/run.ts --app nest-bun   (NestJS on Bun)     scenarios: bare,pm2,warden-process,warden-worker
//   bun bench/run.ts --app nest-node  (NestJS on Node)    scenarios: bare,pm2,watt,warden-process
//
// Options: --duration 10  --connections 64  --workers 4  --scenarios a,b,...
//
// Scenarios:
//   bare            N copies started directly (what N systemd units would do)
//   pm2             PM2 the usual way: cluster mode for Node, fork mode for Bun
//   watt            Platformatic Watt (wattpm), N worker threads (Node only)
//   warden-process  Warden, N worker processes
//   warden-worker   Warden, N worker threads in one Bun process
//
// Needs: Linux (/proc), bun, node >= 22.12, oha (cargo install oha),
// target/release/warden (cargo build --release), `npm install` in bench/
// (pm2, wattpm) and bench/nest.

import { spawn, spawnSync, type Subprocess } from "bun";
import { existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  BIN, ROOT, SHIM, TMP, WARDEN, baseEnv, cpuSeconds, listenersOnPort, machine, mb, oha as ohaRaw,
  parseArgs, pkgVersion, pss, rss, saveResults, sleep, sum, table, uniq, version, waitFor,
} from "./lib.ts";

const args = parseArgs();
const APP = args.app ?? "bun-http";
const DURATION = Number(args.duration ?? 10);
const CONNECTIONS = Number(args.connections ?? 64);
const WORKERS = Number(args.workers ?? 4);
const PORT = 3900;

interface AppDef {
  runtime: "bun" | "node";
  entry: string;
  cwd: string;
  /** Needs Warden's shim to share the port (node:http-style listen without reusePort). */
  needsShim: boolean;
  build?: () => void;
  scenarios: string[];
}

const apps: Record<string, AppDef> = {
  "bun-http": {
    runtime: "bun",
    entry: join(ROOT, "bench/app/server.ts"),
    cwd: join(ROOT, "bench/app"),
    needsShim: false,
    scenarios: ["bare", "pm2", "warden-process", "warden-worker"],
  },
  "node-http": {
    runtime: "node",
    entry: join(ROOT, "bench/node/server.mjs"),
    cwd: join(ROOT, "bench/node"),
    needsShim: false,
    scenarios: ["bare", "pm2", "watt", "warden-process"],
  },
  "nest-bun": {
    runtime: "bun",
    entry: join(ROOT, "bench/nest/main.ts"),
    cwd: join(ROOT, "bench/nest"),
    needsShim: true,
    scenarios: ["bare", "pm2", "warden-process", "warden-worker"],
  },
  "nest-node": {
    runtime: "node",
    entry: join(ROOT, "bench/nest/dist/main.mjs"),
    cwd: join(ROOT, "bench/nest"),
    needsShim: true,
    build: () => {
      const r = spawnSync(["bun", "build", "main.ts", "--target=node", "--packages=external", "--outfile=dist/main.mjs"], {
        cwd: join(ROOT, "bench/nest"),
      });
      if (r.exitCode !== 0) throw new Error("building the NestJS app for Node failed: " + r.stderr.toString());
    },
    scenarios: ["bare", "pm2", "watt", "warden-process"],
  },
};
const app = apps[APP];
if (!app) throw new Error(`unknown app ${APP}; one of ${Object.keys(apps).join(", ")}`);
const SCENARIOS = (args.scenarios ?? app.scenarios.join(",")).split(",");
const PATHS = ["/plaintext", "/json", "/cpu"];

/** A request that takes longer than this counts as failed (a hang). */
const REQUEST_TIMEOUT_MS = 2000;
async function httpOk(path = "/health"): Promise<boolean> {
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}${path}`, {
      headers: { connection: "close" },
      keepalive: false,
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    await r.text();
    return r.ok;
  } catch {
    return false;
  }
}

/** Which worker answered a fresh connection ("pid:threadId"), or null. */
async function whoami(): Promise<string | null> {
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}/whoami`, {
      headers: { connection: "close" },
      keepalive: false,
      signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
    });
    const t = await r.text();
    return r.ok ? t : null;
  } catch {
    return null;
  }
}

// Every manager is timed the same way, from the outside: a worker counts as
// ready when it has answered a request. Asking each manager for its own view
// (pm2 jlist takes ~180 ms per call) would time the manager's CLI instead.

/** Waits until `n` workers not in `old` have answered; returns the ms taken. */
async function newWorkersServing(n: number, old: Set<string>, timeoutMs: number): Promise<number> {
  const t0 = performance.now();
  const seen = new Set<string>();
  while (performance.now() - t0 < timeoutMs) {
    const w = await whoami();
    if (w === null) await sleep(2);
    else if (!old.has(w)) {
      seen.add(w);
      if (seen.size >= n) return performance.now() - t0;
    }
  }
  throw new Error(`timeout: ${seen.size} of ${n} new workers answered`);
}

/** The workers answering now (polls until `n` distinct ones have answered). */
async function currentWorkers(n: number): Promise<Set<string>> {
  const seen = new Set<string>();
  const t0 = performance.now();
  while (seen.size < n && performance.now() - t0 < 30_000) {
    const w = await whoami();
    if (w) seen.add(w);
    else await sleep(2);
  }
  return seen;
}

// ------------------------------------------------------------------- scenarios

interface Running {
  appPids: () => Promise<number[]>;
  managerPids: () => Promise<number[]>;
  /** Does anything restart a crashed worker? */
  restarts: boolean;
  /** The manager's zero-downtime restart of every worker; resolves when done. */
  rollingRestart?: () => Promise<void>;
  /** Its status command, timed for CLI latency. */
  statusCmd?: string[];
  stop: () => Promise<void>;
}

const runtimeBin = app.runtime === "bun" ? "bun" : "node";
function appArgs(): string[] {
  if (!app.needsShim) return [app.entry];
  return app.runtime === "bun" ? [`--preload=${SHIM}`, app.entry] : [`--import=${SHIM}`, app.entry];
}
const appEnv = { ...baseEnv, PORT: String(PORT), WARDEN_REUSE_PORT: "1", WARDEN_DRAIN_MS: "0" };

async function startBare(): Promise<Running> {
  const procs: Subprocess[] = [];
  for (let i = 0; i < WORKERS; i++) {
    procs.push(spawn([runtimeBin, ...appArgs()], { cwd: app.cwd, env: appEnv, stdout: "ignore", stderr: "ignore" }));
  }
  return {
    appPids: async () => procs.filter((p) => p.exitCode === null).map((p) => p.pid),
    managerPids: async () => [],
    restarts: false,
    stop: async () => {
      procs.forEach((p) => p.kill("SIGKILL"));
      await Promise.all(procs.map((p) => p.exited));
    },
  };
}

async function startPm2(): Promise<Running> {
  const pm2 = join(BIN, "pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(TMP, "pm2");
  rmSync(home, { recursive: true, force: true });
  mkdirSync(home, { recursive: true });
  const env = { ...appEnv, PM2_HOME: home };
  const eco = join(TMP, "ecosystem.config.cjs");
  // Node: cluster mode (the master owns the port, workers get connections
  // from it). Bun: fork mode (PM2 has no cluster mode for Bun), sharing the
  // port with reusePort (the shim adds it where the app doesn't).
  const cluster = app.runtime === "node";
  const interpreterArgs = app.needsShim && !cluster ? appArgs().slice(0, -1).join(" ") : "";
  writeFileSync(
    eco,
    `module.exports = { apps: [{ name: "bench", script: ${JSON.stringify(app.entry)}, interpreter: ${JSON.stringify(runtimeBin)},
      interpreter_args: ${JSON.stringify(interpreterArgs)}, cwd: ${JSON.stringify(app.cwd)},
      exec_mode: ${JSON.stringify(cluster ? "cluster" : "fork")}, instances: ${WORKERS}, autorestart: true, restart_delay: 0,
      env: ${JSON.stringify({ PORT: String(PORT), WARDEN_REUSE_PORT: "1", WARDEN_DRAIN_MS: "0", ...(cluster ? { BENCH_NO_REUSEPORT: "1" } : {}) })} }] };`,
  );
  const r = spawnSync([pm2, "start", eco], { env, stdout: "ignore", stderr: "pipe" });
  if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  const jlist = () => JSON.parse(spawnSync([pm2, "jlist"], { env }).stdout.toString() || "[]") as any[];
  const daemon = () => {
    try {
      return [Number(readFileSync(join(home, "pm2.pid"), "utf8"))];
    } catch {
      return [];
    }
  };
  return {
    appPids: async () => jlist().map((p) => p.pid),
    managerPids: async () => daemon(),
    restarts: true,
    // `pm2 reload`: graceful in cluster mode (one worker at a time); in fork
    // mode it is a restart.
    rollingRestart: async () => {
      spawnSync([pm2, "reload", "bench"], { env, stdout: "ignore", stderr: "ignore" });
    },
    statusCmd: [pm2, "jlist"],
    stop: async () => {
      spawnSync([pm2, "kill"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

async function startWatt(): Promise<Running> {
  const wattpm = join(BIN, "wattpm");
  if (!existsSync(wattpm)) throw new Error("run `cd bench && npm install` first (wattpm)");
  const dir = join(TMP, "watt");
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(join(dir, "app"), { recursive: true });
  // A Watt project with one Node application pointing at the same entry file.
  writeFileSync(join(dir, "package.json"), JSON.stringify({ name: "watt-bench", private: true, type: "module" }));
  writeFileSync(
    join(dir, "watt.json"),
    JSON.stringify({
      $schema: "https://schemas.platformatic.dev/@platformatic/runtime/3.71.0.json",
      entrypoint: "app",
      server: { hostname: "127.0.0.1", port: PORT },
      workers: WORKERS,
      logger: { level: "warn" },
      applications: [{ id: "app", path: "./app" }],
    }),
  );
  writeFileSync(join(dir, "app/package.json"), JSON.stringify({ name: "app", type: "module", main: "entry.mjs" }));
  writeFileSync(join(dir, "app/watt.json"), JSON.stringify({ $schema: "https://schemas.platformatic.dev/@platformatic/node/3.71.0.json" }));
  writeFileSync(join(dir, "app/entry.mjs"), `import ${JSON.stringify(app.entry)};\n`);
  symlinkSync(join(ROOT, "bench/node_modules"), join(dir, "node_modules"));
  const w = spawn([wattpm, "start", dir], { cwd: dir, env: { ...baseEnv, PORT: String(PORT) }, stdout: "ignore", stderr: "ignore" });
  return {
    // One process: the runtime and its worker threads.
    appPids: async () => [w.pid],
    managerPids: async () => [w.pid],
    restarts: true,
    rollingRestart: async () => {
      spawnSync([wattpm, "restart", String(w.pid), "app"], { cwd: dir, env: baseEnv, stdout: "ignore", stderr: "ignore" });
    },
    statusCmd: [wattpm, "ps"],
    stop: async () => {
      w.kill("SIGTERM");
      await Promise.race([w.exited, sleep(10_000)]);
      w.kill("SIGKILL");
    },
  };
}

async function startWarden(mode: "process" | "worker"): Promise<Running> {
  if (!existsSync(WARDEN)) throw new Error("build first: cargo build --release");
  const sock = join(TMP, `warden-${mode}.sock`);
  const cfg = join(TMP, `warden-${mode}.toml`);
  const appCfg =
    mode === "process"
      ? `command = ${JSON.stringify(runtimeBin)}\nargs = ${JSON.stringify([app.entry])}`
      : `entry = ${JSON.stringify(app.entry)}`;
  writeFileSync(
    cfg,
    `[app]\nname = "bench-${mode}"\n${appCfg}\nworking_directory = ${JSON.stringify(app.cwd)}\nport = ${PORT}\n` +
      `[workers]\ncount = ${WORKERS}\nmode = "${mode}"\n` +
      `[shutdown]\ngrace_period = 10\ndrain_ms = 0\n[logging]\nlevel = "warn"\n[control]\nsocket = ${JSON.stringify(sock)}\n`,
  );
  const w = spawn([WARDEN, "start", "-c", cfg], { env: baseEnv, stdout: "ignore", stderr: "ignore" });
  const status = () => {
    const r = spawnSync([WARDEN, "status", "--json", "--socket", sock]);
    return r.exitCode === 0 ? JSON.parse(r.stdout.toString()) : null;
  };
  return {
    appPids: async () => uniq((status()?.workers ?? []).map((x: any) => x.pid)),
    managerPids: async () => [w.pid],
    restarts: true,
    rollingRestart: async () => {
      spawnSync([WARDEN, "restart", "--socket", sock], { stdout: "ignore", stderr: "ignore" });
    },
    statusCmd: [WARDEN, "status", "--json", "--socket", sock],
    stop: async () => {
      w.kill("SIGTERM");
      await w.exited;
    },
  };
}

function start(name: string): Promise<Running> {
  switch (name) {
    case "bare":
      return startBare();
    case "pm2":
      return startPm2();
    case "watt":
      return startWatt();
    case "warden-process":
      return startWarden("process");
    case "warden-worker":
      return startWarden("worker");
  }
  throw new Error(`unknown scenario ${name}`);
}

// --------------------------------------------------------------------- measure

const oha = (path: string, seconds: number) => ohaRaw(`http://127.0.0.1:${PORT}${path}`, seconds, CONNECTIONS);

async function runScenario(name: string) {
  if (listenersOnPort(PORT) !== 0) throw new Error(`port ${PORT} busy`);
  const t0 = performance.now();
  const run = await start(name);
  await newWorkersServing(WORKERS, new Set(), 120_000);
  const startup_ms = Math.round(performance.now() - t0);
  await sleep(1500);
  const appPids = await run.appPids();
  const mgrPids = await run.managerPids();
  const all = uniq([...appPids, ...mgrPids]);
  const idle = { total_rss_mb: mb(sum(all, rss)), total_pss_mb: mb(sum(all, pss)), manager_rss_mb: mb(sum(mgrPids, rss)) };

  const endpoints: Record<string, any> = {};
  for (const path of PATHS) {
    oha(path, 2); // warm-up
    const a0 = sum(all, cpuSeconds);
    const r = oha(path, DURATION);
    endpoints[path] = { ...r, cpu_s: +(sum(all, cpuSeconds) - a0).toFixed(2) };
  }
  const loaded = { total_rss_mb: mb(sum(all, rss)), total_pss_mb: mb(sum(all, pss)), manager_rss_mb: mb(sum(mgrPids, rss)) };

  // Manager idle CPU over 5 s (for Watt the manager is the whole process).
  const m1 = sum(mgrPids, cpuSeconds);
  await sleep(5000);
  const manager_idle_cpu_pct = +(((sum(mgrPids, cpuSeconds) - m1) / 5) * 100).toFixed(2);

  // Load for the disruption tests: 8 clients, a fresh connection per request.
  const underLoad = async (disrupt: () => Promise<void>) => {
    let ok = 0,
      failed = 0,
      stopClients = false;
    const client = async () => {
      while (!stopClients) {
        if (await httpOk("/plaintext")) ok++;
        else failed++;
      }
    };
    const clients = Array.from({ length: 8 }, client);
    await sleep(300);
    await disrupt();
    await sleep(300);
    stopClients = true;
    await Promise.all(clients);
    return { ok, failed };
  };

  // Rolling restart under load. Done when the command has returned and N
  // replacement workers answered.
  let rolling: any = "n/a";
  if (run.rollingRestart) {
    const old = await currentWorkers(WORKERS);
    let took = 0,
      replaced = false;
    const counts = await underLoad(async () => {
      const tr = performance.now();
      await run.rollingRestart!();
      replaced = await newWorkersServing(WORKERS, old, 60_000).then(
        () => true,
        () => false,
      );
      took = Math.round(performance.now() - tr);
    });
    rolling = { ...counts, ms: took, replaced_all: replaced };
  }

  // CLI latency: the manager's status command, median of 10.
  let cli_ms: any = "n/a";
  if (run.statusCmd) {
    const times: number[] = [];
    for (let i = 0; i < 10; i++) {
      const tc = performance.now();
      spawnSync(run.statusCmd, { stdout: "ignore", stderr: "ignore" });
      times.push(performance.now() - tc);
    }
    times.sort((a, b) => a - b);
    cli_ms = +times[5].toFixed(1);
  }

  // Crash recovery under load: one worker exits (GET /crash); time until a
  // replacement worker answers, and requests that failed meanwhile.
  let recovery: any = "n/a (nothing restarts it)";
  let crash: any = "n/a";
  if (run.restarts) {
    const old = await currentWorkers(WORKERS);
    crash = await underLoad(async () => {
      const tk = performance.now();
      await fetch(`http://127.0.0.1:${PORT}/crash`, {
        headers: { connection: "close" },
        signal: AbortSignal.timeout(REQUEST_TIMEOUT_MS),
      }).catch(() => {});
      try {
        await newWorkersServing(1, old, 60_000);
        recovery = Math.round(performance.now() - tk);
      } catch {
        recovery = "did not recover within 60 s";
      }
    });
  }
  await run.stop();
  await waitFor(() => listenersOnPort(PORT) === 0, 15_000).catch(() => {});
  return { name, startup_ms, idle, loaded, manager_idle_cpu_pct, endpoints, recovery_ms: recovery, crash, rolling, cli_ms };
}

// ------------------------------------------------------------------------ main

mkdirSync(TMP, { recursive: true });
app.build?.();
const meta = {
  ...machine(),
  app: APP,
  workers: WORKERS,
  duration_s: DURATION,
  connections: CONNECTIONS,
  bun: version(["bun", "--version"]),
  node: version(["node", "--version"]),
  pm2: pkgVersion("pm2"),
  wattpm: pkgVersion("wattpm"),
};
console.error(JSON.stringify(meta));
const results: any[] = [];
for (const s of SCENARIOS) {
  console.error(`--- ${s}`);
  const r = await runScenario(s);
  console.error(JSON.stringify(r));
  results.push(r);
}
const out = saveResults(APP, { meta, results });

const lines = [
  `${WORKERS} workers · bun ${meta.bun} · node ${meta.node} · pm2 ${meta.pm2} · wattpm ${meta.wattpm} · ${meta.warden}`,
  "",
  table(results, (r) => r.name, [
    ["total RAM idle: RSS / PSS (MB)", (r) => `${r.idle.total_rss_mb} / ${r.idle.total_pss_mb}`],
    ["total RAM after load: RSS / PSS (MB)", (r) => `${r.loaded.total_rss_mb} / ${r.loaded.total_pss_mb}`],
    ["manager RAM (MB)", (r) => (r.name === "watt" ? "(in total)" : r.loaded.manager_rss_mb)],
    ["manager idle CPU (%)", (r) => r.manager_idle_cpu_pct],
    [`startup to ${WORKERS} serving (ms)`, (r) => r.startup_ms],
    ["crash recovery (ms)", (r) => r.recovery_ms],
    ["requests failed during a crash", (r) => (typeof r.crash === "string" ? r.crash : `${r.crash.failed} of ${r.crash.ok + r.crash.failed}`)],
    [
      "rolling restart under load: failed",
      (r) =>
        typeof r.rolling === "string"
          ? r.rolling
          : `${r.rolling.failed} of ${r.rolling.ok + r.rolling.failed} (${r.rolling.ms} ms${r.rolling.replaced_all ? "" : ", not all replaced"})`,
    ],
    ["status command (ms)", (r) => r.cli_ms],
    ...PATHS.flatMap((p): [string, (r: any) => string | number][] => [
      [`${p} req/s`, (r) => r.endpoints[p].rps],
      [`${p} p50 / p99 (ms)`, (r) => `${r.endpoints[p].p50_ms} / ${r.endpoints[p].p99_ms}`],
      [`${p} errors`, (r) => r.endpoints[p].errors],
    ]),
  ]),
];
console.log(lines.join("\n"));
console.error(`results: ${out}`);

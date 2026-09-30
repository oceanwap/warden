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
import { existsSync, mkdirSync, readFileSync, writeFileSync, rmSync, symlinkSync } from "node:fs";
import { join, resolve } from "node:path";

const ROOT = resolve(import.meta.dir, "..");
const args = Object.fromEntries(
  process.argv.slice(2).reduce<[string, string][]>((acc, a, i, all) => {
    if (a.startsWith("--")) acc.push([a.slice(2), all[i + 1]]);
    return acc;
  }, []),
);
const APP = args.app ?? "bun-http";
const DURATION = Number(args.duration ?? 10);
const CONNECTIONS = Number(args.connections ?? 64);
const WORKERS = Number(args.workers ?? 4);
const PORT = 3900;
const WARDEN = join(ROOT, "target/release/warden");
const SHIM = join(ROOT, "shim/warden-shim.mjs");
const TMP = join(ROOT, "bench/.run");
const BIN = join(ROOT, "bench/node_modules/.bin");
const HZ = Number(spawnSync(["getconf", "CLK_TCK"]).stdout.toString().trim() || 100);
const PAGE = Number(spawnSync(["getconf", "PAGESIZE"]).stdout.toString().trim() || 4096);

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

// Never let a shell's BUN_OPTIONS / NODE_OPTIONS change what we measure.
const baseEnv: Record<string, string> = Object.fromEntries(
  Object.entries(process.env).filter(([k]) => k !== "BUN_OPTIONS" && k !== "NODE_OPTIONS") as [string, string][],
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
    return Number(readFileSync(`/proc/${pid}/statm`, "utf8").split(" ")[1]) * PAGE;
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

const uniq = (pids: number[]) => [...new Set(pids.filter((p) => p > 0))];
const sum = (pids: number[], f: (p: number) => number) => uniq(pids).reduce((a, p) => a + f(p), 0);
const mb = (b: number) => Math.round((b / 1048576) * 10) / 10;

async function waitFor(cond: () => boolean | Promise<boolean>, timeoutMs: number): Promise<number> {
  const t0 = performance.now();
  while (performance.now() - t0 < timeoutMs) {
    if (await cond()) return performance.now() - t0;
    await sleep(5);
  }
  throw new Error("timeout");
}

async function httpOk(path = "/health"): Promise<boolean> {
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}${path}`, { headers: { connection: "close" }, keepalive: false });
    await r.text();
    return r.ok;
  } catch {
    return false;
  }
}

// ------------------------------------------------------------------- scenarios

interface Running {
  appPids: () => Promise<number[]>;
  managerPids: () => Promise<number[]>;
  /** Workers ready to serve (for startup and crash-recovery timing). */
  ready: () => Promise<number>;
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
const listenerReady = async () => listenersOnPort(PORT);

async function startBare(): Promise<Running> {
  const procs: Subprocess[] = [];
  for (let i = 0; i < WORKERS; i++) {
    procs.push(spawn([runtimeBin, ...appArgs()], { cwd: app.cwd, env: appEnv, stdout: "ignore", stderr: "ignore" }));
  }
  return {
    appPids: async () => procs.filter((p) => p.exitCode === null).map((p) => p.pid),
    managerPids: async () => [],
    ready: listenerReady,
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
    // Cluster workers own no listener (the daemon does): count online ones.
    ready: async () =>
      cluster
        ? (await httpOk()) ? jlist().filter((p) => p.pm2_env?.status === "online" && p.pid > 0).length : 0
        : listenersOnPort(PORT),
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
    ready: listenerReady,
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
    ready: listenerReady,
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
  };
}

async function runScenario(name: string) {
  if (listenersOnPort(PORT) !== 0) throw new Error(`port ${PORT} busy`);
  const t0 = performance.now();
  const run = await start(name);
  await waitFor(async () => (await run.ready()) >= WORKERS && (await httpOk()), 120_000);
  const startup_ms = Math.round(performance.now() - t0);
  await sleep(1500);
  const appPids = await run.appPids();
  const mgrPids = await run.managerPids();
  const all = uniq([...appPids, ...mgrPids]);
  const idle = { total_rss_mb: mb(sum(all, rss)), manager_rss_mb: mb(sum(mgrPids, rss)) };

  const endpoints: Record<string, any> = {};
  for (const path of PATHS) {
    oha(path, 2); // warm-up
    const a0 = sum(all, cpuSeconds);
    const r = oha(path, DURATION);
    endpoints[path] = { ...r, cpu_s: +(sum(all, cpuSeconds) - a0).toFixed(2) };
  }
  const loaded = { total_rss_mb: mb(sum(all, rss)), manager_rss_mb: mb(sum(mgrPids, rss)) };

  // Manager idle CPU over 5 s (for Watt the manager is the whole process).
  const m1 = sum(mgrPids, cpuSeconds);
  await sleep(5000);
  const manager_idle_cpu_pct = +(((sum(mgrPids, cpuSeconds) - m1) / 5) * 100).toFixed(2);

  // Rolling restart under load: 8 clients, a fresh connection per request.
  let rolling: any = "n/a";
  if (run.rollingRestart) {
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
    const tr = performance.now();
    await run.rollingRestart();
    await waitFor(async () => (await run.ready()) >= WORKERS && (await httpOk()), 60_000).catch(() => {});
    const took = Math.round(performance.now() - tr);
    await sleep(300);
    stopClients = true;
    await Promise.all(clients);
    rolling = { ok, failed, ms: took };
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

  // Crash recovery: one worker exits (GET /crash); time until N are ready again.
  let recovery: any = "n/a (nothing restarts it)";
  if (run.restarts) {
    const tk = performance.now();
    await fetch(`http://127.0.0.1:${PORT}/crash`, { headers: { connection: "close" } }).catch(() => {});
    await waitFor(async () => (await run.ready()) < WORKERS, 3000).catch(() => 0);
    try {
      await waitFor(async () => (await run.ready()) >= WORKERS && (await httpOk()), 60_000);
      recovery = Math.round(performance.now() - tk);
    } catch {
      recovery = "did not recover within 60 s";
    }
  }
  await run.stop();
  await waitFor(() => listenersOnPort(PORT) === 0, 15_000).catch(() => {});
  return { name, startup_ms, idle, loaded, manager_idle_cpu_pct, endpoints, recovery_ms: recovery, rolling, cli_ms };
}

// ------------------------------------------------------------------------ main

mkdirSync(TMP, { recursive: true });
app.build?.();
const version = (cmd: string[]) => spawnSync(cmd).stdout.toString().trim().split("\n")[0];
const pkgVersion = (name: string) => {
  try {
    return JSON.parse(readFileSync(join(ROOT, "bench/node_modules", name, "package.json"), "utf8")).version;
  } catch {
    return "not installed";
  }
};
const meta = {
  date: new Date().toISOString(),
  app: APP,
  workers: WORKERS,
  duration_s: DURATION,
  connections: CONNECTIONS,
  bun: version(["bun", "--version"]),
  node: version(["node", "--version"]),
  // `pm2 --version` would start a daemon: read package versions instead.
  pm2: pkgVersion("pm2"),
  wattpm: pkgVersion("wattpm"),
  warden: version([WARDEN, "version"]),
  kernel: version(["uname", "-rm"]),
  cpus: navigator.hardwareConcurrency,
  cpu_model: (readFileSync("/proc/cpuinfo", "utf8").match(/model name\s*:\s*(.*)/) ?? [])[1] ?? "unknown",
  tcp_migrate_req: existsSync("/proc/sys/net/ipv4/tcp_migrate_req")
    ? readFileSync("/proc/sys/net/ipv4/tcp_migrate_req", "utf8").trim()
    : "n/a",
};
console.error(JSON.stringify(meta));
const results = [];
for (const s of SCENARIOS) {
  console.error(`--- ${s}`);
  const r = await runScenario(s);
  console.error(JSON.stringify(r));
  results.push(r);
}
const out = join(ROOT, "bench/results", `${meta.date.slice(0, 19).replace(/:/g, "")}-${APP}.json`);
mkdirSync(join(ROOT, "bench/results"), { recursive: true });
writeFileSync(out, JSON.stringify({ meta, results }, null, 2));

const row = (label: string, f: (r: any) => string | number) => `| ${label} | ${results.map((r) => f(r)).join(" | ")} |`;
const lines = [
  `**${APP}**, ${WORKERS} workers, ${meta.cpus} CPUs (${meta.cpu_model}), bun ${meta.bun}, node ${meta.node}, pm2 ${meta.pm2}, wattpm ${meta.wattpm}, ${meta.warden}`,
  "",
  `| | ${results.map((r) => r.name).join(" | ")} |`,
  `|---|${results.map(() => "---").join("|")}|`,
  row("total RAM idle (MB)", (r) => r.idle.total_rss_mb),
  row("total RAM after load (MB)", (r) => r.loaded.total_rss_mb),
  row("manager RAM (MB)", (r) => (r.name === "watt" ? "(in total)" : r.loaded.manager_rss_mb)),
  row("manager idle CPU (%)", (r) => r.manager_idle_cpu_pct),
  row(`startup to ${WORKERS} ready (ms)`, (r) => r.startup_ms),
  row("crash recovery (ms)", (r) => r.recovery_ms),
  row("rolling restart under load: failed / total", (r) =>
    typeof r.rolling === "string" ? r.rolling : `${r.rolling.failed} / ${r.rolling.ok + r.rolling.failed} (${r.rolling.ms} ms)`,
  ),
  row("status command (ms)", (r) => r.cli_ms),
];
for (const p of PATHS) {
  lines.push(row(`${p} req/s`, (r) => r.endpoints[p].rps));
  lines.push(row(`${p} p50 / p99 (ms)`, (r) => `${r.endpoints[p].p50_ms} / ${r.endpoints[p].p99_ms}`));
  lines.push(row(`${p} errors`, (r) => r.endpoints[p].errors));
}
console.log(lines.join("\n"));
console.error(`results: ${out}`);

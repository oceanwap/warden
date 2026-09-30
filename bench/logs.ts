#!/usr/bin/env bun
// Log-heavy apps: what capturing worker output costs the process manager.
// Results go to bench/results/<date>-logs.json plus a Markdown table.
//
//   bun bench/logs.ts [--scenarios warden,pm2] [--workers 4] [--rate 5000] [--seconds 10] [--mb 200]
//
// Two loads, each under Warden (out_file, like PM2's out log) and PM2 (its
// default per-app log files):
//   steady  N workers each logging --rate lines/s (~100 bytes) for --seconds:
//           manager CPU, and whether every line reached the log file
//   flood   one worker (sh: yes | head -c) writing --mb MB to stdout as fast
//           as the manager reads it: throughput and manager CPU per GB. By
//           default Warden keeps at most 10,000 lines/s per worker and counts
//           the rest as dropped (so one runaway worker can't eat the host);
//           PM2 writes everything. "warden (keep all)" is the flood with
//           max_lines_per_sec = 0, writing everything too.

import { spawn, spawnSync } from "bun";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { BIN, ROOT, TMP, WARDEN, baseEnv, cpuSeconds, machine, mb, parseArgs, pkgVersion, rss, saveResults, sleep, table, version } from "./lib.ts";

const args = parseArgs();
const WORKERS = Number(args.workers ?? 4);
const RATE = Number(args.rate ?? 5000);
const SECONDS = Number(args.seconds ?? 10);
const FLOOD_MB = Number(args.mb ?? 200);
const SCENARIOS = (args.scenarios ?? "warden,pm2").split(",");
const APP = join(ROOT, "bench/logs/app.mjs");
const FLOOD = join(ROOT, "bench/logs/flood.sh");
const DIR = join(TMP, "logs");

interface Load {
  mode: "steady" | "flood";
  command: string[];
  workers: number;
  env: Record<string, string>;
  /** Lines the workers write in total. */
  lines: number;
}

const loads: Load[] = [
  { mode: "steady", command: ["node", APP], workers: WORKERS, env: { LOG_MODE: "steady", LOG_RATE: String(RATE), LOG_SECONDS: String(SECONDS) }, lines: WORKERS * RATE * SECONDS },
  { mode: "flood", command: ["/bin/sh", FLOOD], workers: 1, env: { LOG_MODE: "flood", LOG_MB: String(FLOOD_MB) }, lines: 0 },
];

interface Running {
  manager: () => number[];
  logFiles: () => string[];
  stop: () => Promise<void>;
}

async function startWarden(load: Load, done: string, keepAll = false): Promise<Running> {
  if (!existsSync(WARDEN)) throw new Error("build first: cargo build --release");
  const out = join(DIR, "warden-out.log");
  const cfg = join(DIR, "warden.toml");
  const env = Object.entries({ ...load.env, LOG_DONE_DIR: done })
    .map(([k, v]) => `${k} = ${JSON.stringify(v)}`)
    .join(", ");
  writeFileSync(
    cfg,
    `[app]\nname = "logbench"\ncommand = ${JSON.stringify(load.command[0])}\nargs = ${JSON.stringify(load.command.slice(1))}\nenv = { ${env} }\n` +
      `[workers]\ncount = ${load.workers}\n` +
      `[logging]\nlevel = "warn"\nout_file = ${JSON.stringify(out)}\nrotate = { max_size = "10G" }\n` +
      (keepAll ? "max_lines_per_sec = 0\n" : "") +
      `[control]\nsocket = ${JSON.stringify(join(DIR, "w.sock"))}\n`,
  );
  const w = spawn([WARDEN, "start", "-c", cfg], { env: baseEnv, stdout: "ignore", stderr: "ignore" });
  return {
    manager: () => [w.pid],
    logFiles: () => [out],
    stop: async () => {
      w.kill("SIGTERM");
      await w.exited;
    },
  };
}

async function startPm2(load: Load, done: string): Promise<Running> {
  const pm2 = join(BIN, "pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(DIR, "pm2");
  mkdirSync(home, { recursive: true });
  const env = { ...baseEnv, PM2_HOME: home };
  const eco = join(home, "logs.config.cjs");
  writeFileSync(
    eco,
    `module.exports = { apps: [{ name: "logbench", script: ${JSON.stringify(load.command[1])}, interpreter: ${JSON.stringify(load.command[0])},
      exec_mode: "fork", instances: ${load.workers},
      env: ${JSON.stringify({ ...load.env, LOG_DONE_DIR: done })} }] };`,
  );
  const r = spawnSync([pm2, "start", eco], { env, stdout: "ignore", stderr: "pipe" });
  if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  const daemon = () => {
    try {
      return [Number(readFileSync(join(home, "pm2.pid"), "utf8"))];
    } catch {
      return [];
    }
  };
  return {
    manager: daemon,
    logFiles: () =>
      existsSync(join(home, "logs"))
        ? readdirSync(join(home, "logs"))
            .filter((f) => f.includes("-out"))
            .map((f) => join(home, "logs", f))
        : [],
    stop: async () => {
      spawnSync([pm2, "kill"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

function countLines(files: string[], marker: string): number {
  let n = 0;
  for (const f of files) {
    const r = spawnSync(["grep", "-c", marker, f]);
    n += Number(r.stdout.toString().trim() || 0);
  }
  return n;
}

async function run(name: string, load: Load) {
  rmSync(DIR, { recursive: true, force: true });
  const done = join(DIR, "done");
  mkdirSync(done, { recursive: true });
  const t0 = performance.now();
  const r = name === "pm2" ? await startPm2(load, done) : await startWarden(load, done, name === "warden-keep-all");
  let peak = 0;
  // Wait for every worker's "done" marker, sampling the manager's memory.
  while (readdirSync(done).length < load.workers) {
    if (performance.now() - t0 > 600_000) throw new Error(`${name} ${load.mode}: workers never finished`);
    peak = Math.max(peak, r.manager().reduce((a, p) => a + rss(p), 0));
    await sleep(20);
  }
  const wall_ms = Math.round(performance.now() - t0);
  await sleep(1500); // let the manager drain its buffers to disk
  const cpu = r.manager().reduce((a, p) => a + cpuSeconds(p), 0);
  const markers = readdirSync(done).map((f) => JSON.parse(readFileSync(join(done, f), "utf8")));
  const written = markers.reduce((a, m) => a + m.lines, 0);
  const worker_ms = Math.max(...markers.map((m) => m.ms));
  const kept = countLines(r.logFiles(), load.mode === "flood" ? " flood" : " line ");
  await r.stop();
  const bytes = markers.reduce((a, m) => a + m.bytes, 0);
  return {
    name,
    mode: load.mode,
    workers: load.workers,
    lines_written: written,
    lines_in_log: kept,
    worker_ms,
    wall_ms,
    manager_cpu_s: +cpu.toFixed(2),
    manager_peak_rss_mb: mb(peak),
    mb_per_s: +(bytes / 1048576 / (worker_ms / 1000)).toFixed(1),
    cpu_s_per_gb: +((cpu / (bytes / 1073741824)) || 0).toFixed(2),
  };
}

mkdirSync(TMP, { recursive: true });
const meta = { ...machine(), workers: WORKERS, rate: RATE, seconds: SECONDS, flood_mb: FLOOD_MB, node: version(["node", "--version"]), pm2: pkgVersion("pm2") };
console.error(JSON.stringify(meta));
const results: any[] = [];
for (const load of loads) {
  const names = load.mode === "flood" && SCENARIOS.includes("warden") ? [...SCENARIOS, "warden-keep-all"] : SCENARIOS;
  for (const s of names) {
    console.error(`--- ${s} ${load.mode}`);
    const r = await run(s, load);
    console.error(JSON.stringify(r));
    results.push(r);
  }
}
const out = saveResults("logs", { meta, results });
const steady = results.filter((r) => r.mode === "steady");
const flood = results.filter((r) => r.mode === "flood");
console.log(
  [
    `**log capture**: ${meta.cpus} CPUs (${meta.cpu_model}), node ${meta.node}, pm2 ${meta.pm2}, ${meta.warden}`,
    "",
    `Steady: ${WORKERS} workers × ${RATE} lines/s × ${SECONDS} s`,
    "",
    table(steady, (r) => r.name, [
      ["lines written / in the log file", (r) => `${r.lines_written} / ${r.lines_in_log}`],
      ["manager CPU (s)", (r) => r.manager_cpu_s],
      ["manager peak RAM (MB)", (r) => r.manager_peak_rss_mb],
    ]),
    "",
    `Flood: 1 worker writing ${FLOOD_MB} MB to stdout as fast as it is read`,
    "",
    table(flood, (r) => (r.name === "warden-keep-all" ? "warden (keep all)" : r.name), [
      ["time to write it (ms)", (r) => r.worker_ms],
      ["throughput (MB/s)", (r) => r.mb_per_s],
      ["manager CPU (s)", (r) => r.manager_cpu_s],
      ["manager CPU per GB (s)", (r) => r.cpu_s_per_gb],
      ["manager peak RAM (MB)", (r) => r.manager_peak_rss_mb],
      ["lines in the log file", (r) => r.lines_in_log],
    ]),
  ].join("\n"),
);
console.error(`results: ${out}`);

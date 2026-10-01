#!/usr/bin/env bun
// Many apps on one host: what the process manager costs with --apps apps
// running (one idle Node process each), and how fast its everyday commands
// answer. Warden runs one small supervisor per app (no daemon); PM2 runs one
// daemon for all. Results go to bench/results/<date>-fleet.json plus a
// Markdown table.
//
//   bun bench/fleet.ts [--apps 10] [--scenarios warden,pm2] [--wait 1]
//
// "start N apps" is until N app processes exist. `warden start` normally
// waits until an app is up (for a port-less app: min_uptime, 1 s) and says
// if it failed; the benchmark passes --no-wait, which returns like
// `pm2 start` does (--wait 1 measures the waiting mode).

import { spawnSync } from "bun";
import { existsSync, mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { BIN, TMP, WARDEN, baseEnv, childrenOf, cpuSeconds, machine, mb, parseArgs, pkgVersion, pss, rss, saveResults, sleep, table, version, waitFor } from "./lib.ts";

const args = parseArgs();
const APPS = Number(args.apps ?? 10);
const SCENARIOS = (args.scenarios ?? "warden,pm2").split(",");
/** `--wait 1`: time `warden start` in its default, waiting mode instead. */
const WAIT = args.wait === "1";
const DIR = join(TMP, "fleet");
const IDLE = join(DIR, "idle.mjs");

/** Median wall time of `cmd` over `n` runs, in ms. */
function timed(cmd: string[], env: Record<string, string>, n = 10): number {
  const t: number[] = [];
  for (let i = 0; i < n; i++) {
    const t0 = performance.now();
    const r = spawnSync(cmd, { env, stdout: "ignore", stderr: "ignore" });
    t.push(performance.now() - t0);
    if (r.exitCode !== 0) throw new Error(`${cmd.join(" ")} exited ${r.exitCode}`);
  }
  t.sort((a, b) => a - b);
  return +t[Math.floor(n / 2)].toFixed(1);
}

interface Fleet {
  env: Record<string, string>;
  managers: () => number[];
  apps: () => number[];
  commands: Record<string, string[]>;
  stop: () => void;
}

async function startWarden(): Promise<Fleet> {
  if (!existsSync(WARDEN)) throw new Error("build first: cargo build --release");
  const env = { ...baseEnv, WARDEN_HOME: join(DIR, "warden"), WARDEN_RUNTIME_DIR: join(DIR, "warden/run") };
  for (let i = 0; i < APPS; i++) {
    // --no-wait: like `pm2 start`, return without waiting for the app to be
    // up (by default `warden start` waits and reports whether it came up).
    const r = spawnSync([WARDEN, "start", IDLE, "--name", `app${i}`, ...(WAIT ? [] : ["--no-wait"])], {
      env,
      stdout: "ignore",
      stderr: "pipe",
    });
    if (r.exitCode !== 0) throw new Error("warden start failed: " + r.stderr.toString());
  }
  // One supervisor per app: the processes whose command is `warden run`.
  const supervisors = () =>
    readdirSync("/proc")
      .filter((d) => /^\d+$/.test(d))
      .map(Number)
      .filter((p) => {
        try {
          const cmd = readFileSync(`/proc/${p}/cmdline`, "utf8");
          return cmd.startsWith(WARDEN) && cmd.includes(join(DIR, "warden"));
        } catch {
          return false;
        }
      });
  // wardend, which `warden start` starts too (like PM2's daemon): counted
  // with the managers. Found by its WARDEN_HOME, as its command line has none.
  const wardend = () =>
    readdirSync("/proc")
      .filter((d) => /^\d+$/.test(d))
      .map(Number)
      .filter((p) => {
        try {
          const cmd = readFileSync(`/proc/${p}/cmdline`, "utf8").split("\0");
          const environ = readFileSync(`/proc/${p}/environ`, "utf8").split("\0");
          return cmd[0] === WARDEN && cmd[1] === "daemon" && environ.includes(`WARDEN_HOME=${join(DIR, "warden")}`);
        } catch {
          return false;
        }
      });
  return {
    env,
    managers: () => [...supervisors(), ...wardend()],
    apps: () => supervisors().flatMap(childrenOf),
    commands: {
      "list": [WARDEN, "list"],
      "list --json": [WARDEN, "list", "--json"],
      "describe app3": [WARDEN, "describe", "app3"],
      "logs app3 --nostream": [WARDEN, "logs", "app3", "--nostream", "--lines", "20"],
    },
    stop: () => {
      spawnSync([WARDEN, "delete", "all"], { env, stdout: "ignore", stderr: "ignore" });
      // Everything else, wardend included (`delete` leaves it running).
      spawnSync([WARDEN, "kill", "--yes"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

async function startPm2(): Promise<Fleet> {
  const pm2 = join(BIN, "pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(DIR, "pm2");
  mkdirSync(home, { recursive: true });
  const env = { ...baseEnv, PM2_HOME: home };
  for (let i = 0; i < APPS; i++) {
    const r = spawnSync([pm2, "start", IDLE, "--name", `app${i}`], { env, stdout: "ignore", stderr: "pipe" });
    if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  }
  const daemon = () => {
    try {
      return [Number(readFileSync(join(home, "pm2.pid"), "utf8"))];
    } catch {
      return [];
    }
  };
  return {
    env,
    managers: daemon,
    apps: () => daemon().flatMap(childrenOf),
    commands: {
      "list": [pm2, "list"],
      "list --json": [pm2, "jlist"],
      "describe app3": [pm2, "describe", "app3"],
      "logs app3 --nostream": [pm2, "logs", "app3", "--nostream", "--lines", "20"],
    },
    stop: () => {
      spawnSync([pm2, "kill"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

async function run(name: string) {
  const t0 = performance.now();
  const f = await (name === "warden" ? startWarden : startPm2)();
  try {
    await waitFor(() => f.apps().length >= APPS, 60_000);
    const start_all_ms = Math.round(performance.now() - t0);
    await sleep(2000);
    const managers = f.managers();
    const manager_rss_mb = mb(managers.reduce((a, p) => a + rss(p), 0));
    const manager_pss_mb = mb(managers.reduce((a, p) => a + pss(p), 0));
    const c0 = managers.reduce((a, p) => a + cpuSeconds(p), 0);
    await sleep(10_000);
    const manager_idle_cpu_pct = +(((managers.reduce((a, p) => a + cpuSeconds(p), 0) - c0) / 10) * 100).toFixed(2);
    const cmd_ms: Record<string, number> = {};
    for (const [label, cmd] of Object.entries(f.commands)) cmd_ms[label] = timed(cmd, f.env);
    return { name, apps: APPS, manager_processes: managers.length, start_all_ms, manager_rss_mb, manager_pss_mb, manager_idle_cpu_pct, cmd_ms };
  } finally {
    f.stop();
  }
}

rmSync(DIR, { recursive: true, force: true });
mkdirSync(DIR, { recursive: true });
writeFileSync(IDLE, "setInterval(() => {}, 1 << 30);\n");
const meta = { ...machine(), apps: APPS, node: version(["node", "--version"]), pm2: pkgVersion("pm2") };
console.error(JSON.stringify(meta));
const results: any[] = [];
for (const s of SCENARIOS) {
  console.error(`--- ${s}`);
  const r = await run(s);
  console.error(JSON.stringify(r));
  results.push(r);
  await sleep(1000);
}
const out = saveResults("fleet", { meta, results });
const labels = Object.keys(results[0].cmd_ms);
console.log(
  [
    `${APPS} apps, one idle Node process each · node ${meta.node} · pm2 ${meta.pm2} · ${meta.warden}`,
    "",
    table(results, (r) => r.name, [
      ["manager processes", (r) => r.manager_processes],
      ["manager RAM, all apps: RSS (MB)", (r) => r.manager_rss_mb],
      ["manager RAM, all apps: PSS (MB)", (r) => r.manager_pss_mb],
      ["manager idle CPU (%)", (r) => r.manager_idle_cpu_pct],
      [`start ${APPS} apps (ms)`, (r) => r.start_all_ms],
      ...labels.map((l): [string, (r: any) => number] => [`\`${l}\` (ms)`, (r) => r.cmd_ms[l]]),
    ]),
  ].join("\n"),
);
console.error(`results: ${out}`);

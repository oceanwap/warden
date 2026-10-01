#!/usr/bin/env bun
// Static file serving: `warden serve` against nginx, PM2's `pm2 serve` and
// the `serve` npm package, on the same files and port. Measures memory,
// startup, throughput, latency and transfer rate for a small HTML page, a
// 48 KB script and a 1 MB file, with keep-alive and with a fresh connection
// per request. Results go to bench/results/<date>-static.json plus a
// Markdown table on stdout. See bench/README.md.
//
//   bun bench/static.ts [--duration 10] [--connections 64] [--workers 4] [--scenarios warden,nginx,pm2-serve,serve]
//                       [--io epoll|uring]
//
// Scenarios:
//   warden        warden serve: N worker processes sharing the port (SO_REUSEPORT), small files from its
//                 response cache, sendfile above; `--io uring` runs it with [static] io = "uring"
//   warden-uring  the same with io_uring (not in the default set: `--scenarios warden,warden-uring,nginx`
//                 compares the two I/O paths side by side); fails if io_uring is unavailable here
//   nginx         nginx with N worker processes, sendfile, tcp_nopush, access log off (skipped if not installed)
//   pm2-serve  PM2's static server (`pm2 serve`), N instances in cluster mode
//   serve      the `serve` npm package (one process: it has no cluster mode)

import { spawn, spawnSync, type Subprocess } from "bun";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import {
  BIN, TMP, WARDEN, baseEnv, childrenOf, listenersOnPort, machine, mb, oha, parseArgs, pkgVersion,
  pss, rss, saveResults, sleep, sum, table, uniq, version, waitFor, type LoadResult,
  onAppCpus,
} from "./lib.ts";

const args = parseArgs();
const DURATION = Number(args.duration ?? 10);
const CONNECTIONS = Number(args.connections ?? 64);
const WORKERS = Number(args.workers ?? 4);
const PORT = 3910;
const SITE = join(TMP, "site");
const ALL = ["warden", "nginx", "pm2-serve", "serve"];
const SCENARIOS = (args.scenarios ?? ALL.join(",")).split(",");
const IO = args.io ?? "epoll";
if (IO !== "epoll" && IO !== "uring") throw new Error(`--io ${IO}: expected epoll or uring`);

// ----------------------------------------------------------------------- site

const FILES = {
  "/index.html": 1_500,
  "/assets/app.3f9a2c1b.js": 48 * 1024,
  "/media/video.bin": 1024 * 1024,
};

function makeSite() {
  rmSync(SITE, { recursive: true, force: true });
  mkdirSync(join(SITE, "assets"), { recursive: true });
  mkdirSync(join(SITE, "media"), { recursive: true });
  let seed = 42;
  const bytes = (n: number) => {
    const b = new Uint8Array(n);
    for (let i = 0; i < n; i++) {
      seed = (seed * 1103515245 + 12345) & 0x7fffffff;
      b[i] = 32 + (seed % 90); // printable, so no server compresses on the fly differently
    }
    return b;
  };
  for (const [path, size] of Object.entries(FILES)) writeFileSync(join(SITE, path), bytes(size));
}

// ------------------------------------------------------------------ scenarios

interface Running {
  pids: () => number[];
  managerPids: () => number[];
  /** Optional check once the server answers. */
  ready?: () => Promise<void>;
  stop: () => Promise<void>;
}

async function startWarden(io: string): Promise<Running> {
  if (!existsSync(WARDEN)) throw new Error("build first: cargo build --release");
  const cfg = join(TMP, "warden-static.toml");
  const log = join(TMP, "warden-static.log");
  writeFileSync(
    cfg,
    `[app]\nname = "bench-static"\nport = ${PORT}\n[workers]\ncount = ${WORKERS}\n[static]\nroot = ${JSON.stringify(SITE)}\n` +
      `io = "${io}"\n[logging]\nlevel = "warn"\n[control]\nsocket = ${JSON.stringify(join(TMP, "warden-static.sock"))}\n`,
  );
  writeFileSync(log, "");
  const w = spawn(onAppCpus([WARDEN, "start", "-c", cfg]), {
    env: { ...baseEnv, WARDEN_STATIC_IO: io },
    stdout: Bun.file(log),
    stderr: "ignore",
  });
  return {
    pids: () => [w.pid, ...childrenOf(w.pid)],
    managerPids: () => [w.pid],
    ready: async () => {
      // Each worker says how it serves; an io_uring run that fell back to
      // epoll would publish epoll's numbers under the wrong name.
      await waitFor(() => (readFileSync(log, "utf8").match(/ via (epoll|io_uring)/g) ?? []).length >= WORKERS, 10_000);
      const text = readFileSync(log, "utf8");
      if (io === "uring" && !text.includes("via io_uring")) throw new Error(`io_uring is unavailable here:\n${text}`);
    },
    stop: async () => {
      w.kill("SIGTERM");
      await w.exited;
    },
  };
}

async function startNginx(): Promise<Running> {
  const nginx = ["/usr/sbin/nginx", "/usr/local/sbin/nginx", "/usr/bin/nginx"].find(existsSync);
  if (!nginx) throw new Error("skip: nginx is not installed");
  const dir = join(TMP, "nginx");
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const conf = join(dir, "nginx.conf");
  writeFileSync(
    conf,
    `daemon off;
master_process on;
worker_processes ${WORKERS};
pid ${dir}/nginx.pid;
error_log ${dir}/error.log warn;
events { worker_connections 4096; }
http {
  client_body_temp_path ${dir}/body; proxy_temp_path ${dir}/proxy; fastcgi_temp_path ${dir}/fcgi;
  uwsgi_temp_path ${dir}/uwsgi; scgi_temp_path ${dir}/scgi;
  types { text/html html; text/javascript js; application/octet-stream bin; }
  access_log off;
  sendfile on; tcp_nopush on; tcp_nodelay on;
  keepalive_requests 1000000; keepalive_timeout 30s;
  server { listen 127.0.0.1:${PORT} reuseport; root ${SITE}; }
}
`,
  );
  const n = spawn(onAppCpus([nginx, "-c", conf, "-p", dir]), { env: baseEnv, stdout: "ignore", stderr: "ignore" });
  return {
    pids: () => [n.pid, ...childrenOf(n.pid)],
    managerPids: () => [n.pid],
    stop: async () => {
      n.kill("SIGQUIT");
      await Promise.race([n.exited, sleep(10_000)]);
      n.kill("SIGKILL");
    },
  };
}

async function startPm2Serve(): Promise<Running> {
  const pm2 = join(BIN, "pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(TMP, "pm2-static");
  rmSync(home, { recursive: true, force: true });
  mkdirSync(home, { recursive: true });
  const env = { ...baseEnv, PM2_HOME: home };
  // `pm2 serve <dir> <port>` is this app, written as an ecosystem file so it
  // can run N instances like the others.
  const eco = join(home, "static.config.cjs");
  writeFileSync(
    eco,
    `module.exports = { apps: [{ name: "static", script: "serve", exec_mode: "cluster", instances: ${WORKERS},
      env: { PM2_SERVE_PATH: ${JSON.stringify(SITE)}, PM2_SERVE_PORT: ${PORT} } }] };`,
  );
  const r = spawnSync(onAppCpus([pm2, "start", eco]), { env, stdout: "ignore", stderr: "pipe" });
  if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  const daemon = () => {
    try {
      return [Number(readFileSync(join(home, "pm2.pid"), "utf8"))];
    } catch {
      return [];
    }
  };
  return {
    pids: () => uniq([...daemon(), ...daemon().flatMap(childrenOf)]),
    managerPids: daemon,
    stop: async () => {
      spawnSync([pm2, "kill"], { env, stdout: "ignore", stderr: "ignore" });
    },
  };
}

async function startServe(): Promise<Running> {
  const main = join(TMP, "..", "node_modules/serve/build/main.js");
  if (!existsSync(main)) throw new Error("run `cd bench && npm install` first (serve)");
  const s = spawn(onAppCpus(["node", main, "-l", `tcp://127.0.0.1:${PORT}`, "--no-clipboard", "--no-request-logging", SITE]), {
    env: baseEnv,
    stdout: "ignore",
    stderr: "ignore",
  });
  return {
    pids: () => [s.pid, ...childrenOf(s.pid)],
    managerPids: () => [],
    stop: async () => {
      s.kill("SIGTERM");
      await Promise.race([s.exited, sleep(5000)]);
      s.kill("SIGKILL");
    },
  };
}

const starters: Record<string, () => Promise<Running>> = {
  warden: () => startWarden(IO),
  "warden-uring": () => startWarden("uring"),
  nginx: startNginx,
  "pm2-serve": startPm2Serve,
  serve: startServe,
};

// -------------------------------------------------------------------- measure

async function get(path: string): Promise<Uint8Array | null> {
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}${path}`, { signal: AbortSignal.timeout(2000) });
    const b = new Uint8Array(await r.arrayBuffer());
    return r.status === 200 ? b : null;
  } catch {
    return null;
  }
}

async function runScenario(name: string) {
  if (listenersOnPort(PORT) !== 0) throw new Error(`port ${PORT} busy`);
  const t0 = performance.now();
  const run = await starters[name]();
  try {
    await waitFor(async () => (await get("/index.html")) !== null, 60_000);
    const startup_ms = Math.round(performance.now() - t0);
    await run.ready?.();
    // Every server must return the exact bytes, or its numbers mean nothing.
    for (const path of Object.keys(FILES)) {
      const body = await get(path);
      const want = readFileSync(join(SITE, path));
      if (!body || Buffer.compare(Buffer.from(body), want) !== 0) throw new Error(`${name} served wrong bytes for ${path}`);
    }
    await sleep(1000);
    const idle_mb = mb(sum(run.pids(), rss));
    const idle_pss_mb = mb(sum(run.pids(), pss));
    const manager_mb = mb(sum(run.managerPids(), rss));
    const loads: Record<string, LoadResult> = {};
    for (const path of Object.keys(FILES)) {
      const url = `http://127.0.0.1:${PORT}${path}`;
      oha(url, 2, CONNECTIONS); // warm-up
      loads[path] = oha(url, DURATION, CONNECTIONS);
    }
    loads["/index.html (new connection each)"] = oha(`http://127.0.0.1:${PORT}/index.html`, DURATION, CONNECTIONS, { keepalive: false });
    const loaded_mb = mb(sum(run.pids(), rss));
    const loaded_pss_mb = mb(sum(run.pids(), pss));
    return { name, processes: uniq(run.pids()).length, startup_ms, idle_mb, idle_pss_mb, loaded_mb, loaded_pss_mb, manager_mb, loads };
  } finally {
    await run.stop();
    await waitFor(() => listenersOnPort(PORT) === 0, 15_000).catch(() => {});
  }
}

// ----------------------------------------------------------------------- main

mkdirSync(TMP, { recursive: true });
makeSite();
const nginxBin = ["/usr/sbin/nginx", "/usr/local/sbin/nginx", "/usr/bin/nginx"].find(existsSync);
const meta = {
  ...machine(),
  workers: WORKERS,
  duration_s: DURATION,
  connections: CONNECTIONS,
  node: version(["node", "--version"]),
  pm2: pkgVersion("pm2"),
  serve: pkgVersion("serve"),
  nginx: nginxBin ? spawnSync([nginxBin, "-v"]).stderr.toString().trim().replace("nginx version: ", "") : "not installed",
};
console.error(JSON.stringify(meta));
const results: any[] = [];
for (const s of SCENARIOS) {
  console.error(`--- ${s}`);
  try {
    const r = await runScenario(s);
    console.error(JSON.stringify(r));
    results.push(r);
  } catch (e: any) {
    if (String(e.message).startsWith("skip:")) console.error(e.message);
    else throw e;
  }
}
const out = saveResults("static", { meta, results });
const paths = Object.keys(results[0]?.loads ?? {});
console.log(
  [
    `${WORKERS} workers · ${meta.nginx} · pm2 ${meta.pm2} · serve ${meta.serve} · node ${meta.node} · ${meta.warden}`,
    "",
    table(results, (r) => (r.name === "serve" ? "serve (1 process)" : r.name === "warden" && IO === "uring" ? "warden (io_uring)" : r.name), [
      ["processes", (r) => r.processes],
      ["total RAM idle: RSS / PSS (MB)", (r) => `${r.idle_mb} / ${r.idle_pss_mb}`],
      ["total RAM after load: RSS / PSS (MB)", (r) => `${r.loaded_mb} / ${r.loaded_pss_mb}`],
      ["startup (ms)", (r) => r.startup_ms],
      ...paths.flatMap((p): [string, (r: any) => string | number][] => [
        [`${p} req/s`, (r) => r.loads[p].rps],
        [`${p} p50 / p99 (ms)`, (r) => `${r.loads[p].p50_ms} / ${r.loads[p].p99_ms}`],
        ...(p.endsWith(".bin") ? [[`${p} MB/s`, (r: any) => r.loads[p].mb_per_s] as [string, (r: any) => number]] : []),
        [`${p} errors`, (r) => r.loads[p].errors],
      ]),
    ]),
  ].join("\n"),
);
console.error(`results: ${out}`);

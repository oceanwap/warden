#!/usr/bin/env bun
// Long-lived connections through a rolling restart: N WebSocket and N SSE
// clients stay connected while every worker is replaced, under PM2 (cluster
// mode for Node, fork mode for Bun, as run.ts runs it) and under Warden.
// Each client reconnects at once whenever its connection ends. Reported per
// scenario: how the connections ended (clean: a WebSocket close frame, the
// SSE chunked stream's last chunk; abnormal: EOF without one, a reset),
// failed reconnects, how long the restart took, and how long until every
// client was back on a new worker. Results go to
// bench/results/<date>-longlived.json plus a Markdown table on stdout.
//
//   bun bench/longlived.ts [--clients 50] [--workers 4] [--apps node,bun]
//                          [--scenarios a,b] [--warden target/debug/warden]
//
// Scenarios (default: all but warden-off):
//   node-pm2       the Node app, `pm2 reload` in cluster mode
//   node-warden    the Node app, `warden restart` (process mode)
//   node-warden-off  the same with shutdown.long_lived_timeout = 0 and
//                  grace_period = 5: the behaviour before Warden closed them
//   bun-pm2        the Bun app, `pm2 reload` in fork mode (PM2 has no Bun cluster mode)
//   bun-warden     the Bun app, `warden restart` (process mode)
//   bun-warden-worker  the Bun app, Warden worker mode (one host process)
//
// The apps are the integration tests' fixtures: tests/fixtures/longlived.ts
// (Bun.serve) and tests/fixtures/longlived_node.mjs (node:http, WebSocket
// handshake by hand). Both answer /whoami, /ws and /sse.
//
// Needs: Linux, bun, node >= 22.12, target/release/warden (or --warden),
// `npm install` in bench/ (pm2).

import { spawn, spawnSync } from "bun";
import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { join } from "node:path";
import {
  BIN, ROOT, TMP, WARDEN, baseEnv, listenersOnPort, machine, onAppCpus, parseArgs, pkgVersion, saveResults,
  sleep, table, version, waitFor,
} from "./lib.ts";

const args = parseArgs();
const CLIENTS = Number(args.clients ?? 50);
const WORKERS = Number(args.workers ?? 4);
const WARDEN_BIN = args.warden ? join(ROOT, args.warden) : WARDEN;
const PORT = 3910;
const ALL = ["node-pm2", "node-warden", "node-warden-off", "bun-pm2", "bun-warden", "bun-warden-worker"];
const DEFAULT = ALL.filter((s) => s !== "node-warden-off");
const APPS = (args.apps ?? "node,bun").split(",");
const SCENARIOS = (args.scenarios ?? DEFAULT.join(",")).split(",").filter((s) => APPS.some((a) => s.startsWith(a + "-")));
for (const s of SCENARIOS) if (!ALL.includes(s)) throw new Error(`unknown scenario ${s}; one of ${ALL.join(", ")}`);

const APP = {
  node: { runtime: "node", entry: join(ROOT, "tests/fixtures/longlived_node.mjs") },
  bun: { runtime: "bun", entry: join(ROOT, "tests/fixtures/longlived.ts") },
};

/** A client's connection gives up when nothing happens for this long. */
const BACK_TIMEOUT_MS = 90_000;

// -------------------------------------------------------------------- clients

const WS_REQUEST =
  "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
  "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
const SSE_REQUEST = "GET /sse HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n";

/** A masked client close frame echoing `code`. */
function clientClose(code: number): Buffer {
  const mask = [0x37, 0xfa, 0x21, 0x3d];
  return Buffer.from([0x88, 0x82, ...mask, (code >> 8) ^ mask[0], (code & 255) ^ mask[1]]);
}

interface Tally {
  clean: number;
  abnormal: number;
  /** "1001", "1006 (no close frame)", "reset", "end of stream", "cut" … */
  how: Record<string, number>;
  connectErrors: number;
}

/** One WebSocket or SSE client that reconnects at once when its connection ends. */
class Client {
  who = ""; // the worker its current connection is on ("" while connecting)
  backAt = 0; // when it first reached a worker not in `old` after the restart began
  stopped = false;
  private sock: net.Socket | null = null;
  constructor(
    readonly kind: "ws" | "sse",
    private tally: Tally,
    private onGreet: (c: Client) => void,
  ) {}

  connect() {
    if (this.stopped) return;
    this.who = "";
    const s = net.connect(PORT, "127.0.0.1");
    this.sock = s;
    let buf = Buffer.alloc(0);
    let head = true;
    let greeted = false;
    let closeCode = 0; // WebSocket close frame received
    let streamEnded = false; // SSE: the last chunk received
    let body = "";
    let over = false;
    const end = (how: string) => {
      if (over) return;
      over = true;
      s.destroy();
      if (this.stopped) return; // the benchmark is done with it
      if (!greeted) this.tally.connectErrors++;
      else if (this.kind === "ws") this.count(closeCode ? String(closeCode) : how === "eof" ? "1006 (no close frame)" : how, closeCode > 0);
      else this.count(streamEnded ? "end of stream" : how === "eof" ? "cut (no last chunk)" : how, streamEnded);
      if (!this.stopped) setTimeout(() => this.connect(), greeted ? 0 : 20);
    };
    s.on("connect", () => s.write(this.kind === "ws" ? WS_REQUEST : SSE_REQUEST));
    s.on("error", (e: any) => end(e.code === "ECONNRESET" ? "reset" : String(e.code || e.message)));
    s.on("close", () => end("eof"));
    s.on("data", (d: Buffer) => {
      buf = buf.length ? Buffer.concat([buf, d]) : d;
      if (head) {
        const i = buf.indexOf("\r\n\r\n");
        if (i < 0) return;
        const status = buf.subarray(0, 12).toString();
        if (!status.startsWith(this.kind === "ws" ? "HTTP/1.1 101" : "HTTP/1.1 200")) return end(`status ${status}`);
        buf = buf.subarray(i + 4);
        head = false;
      }
      if (this.kind === "ws") {
        // Server frames: unmasked, short payloads.
        while (buf.length >= 2) {
          const op = buf[0] & 15;
          let len = buf[1] & 127;
          let off = 2;
          if (len === 126) {
            if (buf.length < 4) return;
            len = buf.readUInt16BE(2);
            off = 4;
          }
          if (buf.length < off + len) return;
          const payload = buf.subarray(off, off + len);
          buf = buf.subarray(off + len);
          if (op === 1 && !greeted) {
            greeted = true;
            this.greet(payload.toString());
          } else if (op === 8 && !closeCode) {
            closeCode = len >= 2 ? payload.readUInt16BE(0) : 1005;
            s.write(clientClose(closeCode)); // answer it, as a browser does
          }
        }
      } else {
        // Chunked body: "<hex>\r\n<data>\r\n" … "0\r\n\r\n".
        for (;;) {
          const i = buf.indexOf("\r\n");
          if (i < 0) return;
          const size = parseInt(buf.subarray(0, i).toString(), 16);
          if (Number.isNaN(size)) return end("bad chunk");
          if (buf.length < i + 2 + size + 2) return;
          if (size === 0) {
            streamEnded = true;
            return end("eof");
          }
          body += buf.subarray(i + 2, i + 2 + size).toString();
          buf = buf.subarray(i + 2 + size + 2);
          if (!greeted) {
            const m = /^data: (\S+)/m.exec(body);
            if (m) {
              greeted = true;
              this.greet(m[1]);
            }
          }
          if (body.length > 4096) body = body.slice(-1024);
        }
      }
    });
  }

  private greet(who: string) {
    this.who = who;
    this.onGreet(this);
  }

  private count(how: string, clean: boolean) {
    if (clean) this.tally.clean++;
    else this.tally.abnormal++;
    this.tally.how[how] = (this.tally.how[how] ?? 0) + 1;
  }

  stop() {
    this.stopped = true;
    this.sock?.destroy();
  }
}

// ------------------------------------------------------------------- managers

interface Running {
  /** The manager's rolling restart of every worker; resolves when its command returns. */
  restart: () => Promise<void>;
  stop: () => Promise<void>;
}

/** Runs a command without blocking the event loop: the clients keep reading
 *  and reconnecting while the manager restarts its workers. */
async function command(cmd: string[], env: Record<string, string> = baseEnv): Promise<void> {
  await spawn(cmd, { env, stdout: "ignore", stderr: "ignore" }).exited;
}

async function whoami(): Promise<string | null> {
  try {
    const r = await fetch(`http://127.0.0.1:${PORT}/whoami`, {
      headers: { connection: "close" },
      keepalive: false,
      signal: AbortSignal.timeout(250),
    });
    const t = await r.text();
    return r.ok ? t : null;
  } catch {
    return null;
  }
}

/** Polls until `n` distinct workers not in `old` have answered; returns them. */
async function workersAnswering(n: number, old: Set<string>, timeoutMs: number): Promise<Set<string>> {
  const seen = new Set<string>();
  const t0 = performance.now();
  while (seen.size < n && performance.now() - t0 < timeoutMs) {
    const w = await whoami();
    if (w === null) await sleep(5);
    else if (!old.has(w)) seen.add(w);
  }
  if (seen.size < n) throw new Error(`only ${seen.size} of ${n} workers answered`);
  return seen;
}

async function startPm2(app: "node" | "bun"): Promise<Running> {
  const pm2 = join(BIN, "pm2");
  if (!existsSync(pm2)) throw new Error("run `cd bench && npm install` first (pm2)");
  const home = join(TMP, "pm2-longlived");
  rmSync(home, { recursive: true, force: true });
  mkdirSync(home, { recursive: true });
  const env = { ...baseEnv, PM2_HOME: home };
  const eco = join(TMP, "longlived.config.cjs");
  // Node: cluster mode (the daemon owns the port). Bun: fork mode, the app
  // shares the port itself (reusePort).
  const cluster = app === "node";
  writeFileSync(
    eco,
    `module.exports = { apps: [{ name: "ll", script: ${JSON.stringify(APP[app].entry)}, interpreter: ${JSON.stringify(APP[app].runtime)},
      exec_mode: ${JSON.stringify(cluster ? "cluster" : "fork")}, instances: ${WORKERS}, autorestart: true,
      env: ${JSON.stringify({ PORT: String(PORT) })} }] };`,
  );
  const r = spawnSync(onAppCpus([pm2, "start", eco]), { env, stdout: "ignore", stderr: "pipe" });
  if (r.exitCode !== 0) throw new Error("pm2 start failed: " + r.stderr.toString());
  return {
    // `pm2 reload`: graceful in cluster mode (one worker at a time); in fork
    // mode it is a restart.
    restart: () => command([pm2, "reload", "ll"], env),
    stop: () => command([pm2, "kill"], env),
  };
}

async function startWarden(app: "node" | "bun", mode: "process" | "worker", off = false): Promise<Running> {
  if (!existsSync(WARDEN_BIN)) throw new Error(`no ${WARDEN_BIN}: cargo build --release (or pass --warden)`);
  const sock = join(TMP, "warden-longlived.sock");
  const cfg = join(TMP, "warden-longlived.toml");
  const appCfg =
    mode === "process"
      ? `command = ${JSON.stringify(APP[app].runtime)}\nargs = ${JSON.stringify([APP[app].entry])}`
      : `entry = ${JSON.stringify(APP[app].entry)}`;
  // Default [shutdown] settings, except for the "before" scenario.
  const shutdown = off ? "[shutdown]\ngrace_period = 5\nlong_lived_timeout = 0\n" : "";
  writeFileSync(
    cfg,
    `[app]\nname = "longlived"\n${appCfg}\nport = ${PORT}\n[workers]\ncount = ${WORKERS}\nmode = "${mode}"\n` +
      `${shutdown}[logging]\nlevel = "warn"\n[control]\nsocket = ${JSON.stringify(sock)}\n`,
  );
  const w = spawn(onAppCpus([WARDEN_BIN, "start", "-c", cfg]), { env: baseEnv, stdout: "ignore", stderr: "ignore" });
  return {
    restart: () => command([WARDEN_BIN, "restart", "--socket", sock]),
    stop: async () => {
      w.kill("SIGTERM");
      await w.exited;
    },
  };
}

function start(name: string): Promise<Running> {
  const app = name.startsWith("node-") ? "node" : "bun";
  switch (name.slice(app.length + 1)) {
    case "pm2":
      return startPm2(app);
    case "warden":
      return startWarden(app, "process");
    case "warden-off":
      return startWarden(app, "process", true);
    case "warden-worker":
      return startWarden(app, "worker");
  }
  throw new Error(`unknown scenario ${name}`);
}

// -------------------------------------------------------------------- measure

async function runScenario(name: string) {
  if (listenersOnPort(PORT) !== 0) throw new Error(`port ${PORT} busy`);
  const run = await start(name);
  const tallies = { ws: newTally(), sse: newTally() };
  const clients: Client[] = [];
  try {
    const old = await workersAnswering(WORKERS, new Set(), 120_000);
    let t0 = 0;
    const onGreet = (c: Client) => {
      if (t0 && !c.backAt && !old.has(c.who)) c.backAt = performance.now();
    };
    for (let i = 0; i < CLIENTS; i++) {
      clients.push(new Client("ws", tallies.ws, onGreet), new Client("sse", tallies.sse, onGreet));
    }
    clients.forEach((c) => c.connect());
    await waitFor(() => clients.every((c) => c.who !== ""), 30_000).catch(() => {
      throw new Error(`only ${clients.filter((c) => c.who).length} of ${clients.length} clients connected`);
    });
    // Connections made before the restart count from here.
    for (const t of [tallies.ws, tallies.sse]) Object.assign(t, newTally());
    t0 = performance.now();
    await run.restart();
    const commandMs = performance.now() - t0;
    await workersAnswering(WORKERS, old, 120_000);
    const restartMs = performance.now() - t0;
    let allBack = true;
    await waitFor(() => clients.every((c) => c.backAt > 0), BACK_TIMEOUT_MS).catch(() => (allBack = false));
    const backMs = allBack ? Math.max(...clients.map((c) => c.backAt - t0)) : null;
    return {
      name,
      restart_ms: Math.round(restartMs),
      command_ms: Math.round(commandMs),
      back_ms: backMs === null ? `not all within ${BACK_TIMEOUT_MS / 1000} s (${clients.filter((c) => c.backAt).length}/${clients.length})` : Math.round(backMs),
      ws: structuredClone(tallies.ws),
      sse: structuredClone(tallies.sse),
    };
  } finally {
    clients.forEach((c) => c.stop());
    await run.stop();
    await waitFor(() => listenersOnPort(PORT) === 0, 15_000).catch(() => {});
  }
}

function newTally(): Tally {
  return { clean: 0, abnormal: 0, how: {}, connectErrors: 0 };
}

// ----------------------------------------------------------------------- main

mkdirSync(TMP, { recursive: true });
const meta = {
  ...machine(),
  warden: version([WARDEN_BIN, "version"]),
  clients: CLIENTS,
  workers: WORKERS,
  bun: version(["bun", "--version"]),
  node: version(["node", "--version"]),
  pm2: pkgVersion("pm2"),
};
console.error(JSON.stringify(meta));
const results: any[] = [];
for (const s of SCENARIOS) {
  console.error(`--- ${s}`);
  const r = await runScenario(s);
  console.error(JSON.stringify(r));
  results.push(r);
}
const out = saveResults("longlived", { meta, results });

const how = (t: Tally) =>
  Object.entries(t.how)
    .sort((a, b) => b[1] - a[1])
    .map(([k, n]) => `${n}× ${k}`)
    .join(", ") || "-";
const label = (r: any) =>
  ({
    "node-pm2": "Node: pm2 (cluster)",
    "node-warden": "Node: warden",
    "node-warden-off": "Node: warden, long_lived_timeout = 0",
    "bun-pm2": "Bun: pm2 (fork)",
    "bun-warden": "Bun: warden",
    "bun-warden-worker": "Bun: warden (worker mode)",
  })[r.name as string] ?? r.name;
console.log(
  [
    `${WORKERS} workers · ${CLIENTS} WebSocket + ${CLIENTS} SSE clients · bun ${meta.bun} · node ${meta.node} · pm2 ${meta.pm2} · ${meta.warden}`,
    "",
    table(results, label, [
      ["rolling restart, until all new workers answer (ms)", (r) => r.restart_ms],
      ["until every client is back on a new worker (ms)", (r) => r.back_ms],
      ["WebSocket closes: clean / abnormal", (r) => `${r.ws.clean} / ${r.ws.abnormal}`],
      ["WebSocket close codes", (r) => how(r.ws)],
      ["SSE ends: clean / abnormal", (r) => `${r.sse.clean} / ${r.sse.abnormal}`],
      ["SSE ends", (r) => how(r.sse)],
      ["failed reconnects (WebSocket + SSE)", (r) => r.ws.connectErrors + r.sse.connectErrors],
    ]),
  ].join("\n"),
);
console.error(`results: ${out}`);

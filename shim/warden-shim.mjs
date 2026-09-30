// Warden shim for Bun. Injected by Warden; apps don't import it.
//
// Process mode: loaded with `bun --preload <this file>`.
// Worker mode:  the entry module of every Worker; it installs the hooks and
//               then imports the app (WARDEN_ENTRY).
//
// What it does (see docs/architecture.md, findings F3, F7-F11):
//  1. Wraps Bun.serve to force `reusePort: true`. Bun's node:http (Express,
//     NestJS, Fastify on node:http) calls Bun.serve internally, so this also
//     fixes node:http ignoring `listen({ reusePort })` under Bun.
//  2. Serves the same app on a private Unix socket for this worker only, so
//     Warden can health-check *this* worker (the shared port reaches a random one).
//  3. Reports listening servers (readiness) and a heartbeat from the event
//     loop (watchdog) to Warden.
//  4. Drains on SIGTERM (process mode) or on a shutdown message (worker mode):
//     closes listeners, answers remaining requests with `Connection: close`,
//     waits WARDEN_DRAIN_MS and for in-flight requests, then exits.
//  5. In Workers, closes listeners from an exit hook: Bun does not close the
//     listening socket of a Worker that dies, which would black-hole 1/N of
//     new connections.

import fs from "node:fs";
import { isMainThread } from "node:worker_threads";

const env = process.env;
const inWorker = !isMainThread;
const workerId = Number(env.WARDEN_WORKER_ID || 0);
const ipcFd = env.WARDEN_IPC_FD ? Number(env.WARDEN_IPC_FD) : null;
const drainMs = Number(env.WARDEN_DRAIN_MS ?? 500);
const forceReusePort = env.WARDEN_REUSE_PORT === "1";
const healthDir = env.WARDEN_HEALTH_DIR || "";
const instance = env.WARDEN_INSTANCE || String(process.pid);
const heartbeatMs = Number(env.WARDEN_HEARTBEAT_MS || 0);
// The app's own port. Other servers the app starts (metrics, admin) are
// neither readiness signals nor health-check targets.
const appPort = env.PORT ? Number(env.PORT) : null;

const servers = new Set();
let privateServer = null;
let privatePath = null;
let draining = false;
let drainStarted = false;
let drainDone = false;
let drainFinished;
const drainPromise = new Promise((r) => (drainFinished = r));
// Promises of app SIGTERM handlers we deferred until after the drain, and
// whether any of them was callback-style (returned no promise).
const appHandlers = new Set();
let appHandlerNoPromise = false;

function report(msg) {
  msg.worker = workerId;
  if (inWorker) {
    try {
      postMessage({ __warden: msg });
    } catch {}
    return;
  }
  if (ipcFd == null) return;
  try {
    fs.writeSync(ipcFd, JSON.stringify(msg) + "\n");
  } catch {}
}

const originalServe = Bun.serve;

function wardenServe(options, ...rest) {
  let opts = options;
  if (opts && typeof opts === "object") {
    // Object.create keeps the caller's object (and its prototype methods)
    // intact while overriding just what we need.
    const o = Object.create(opts);
    if (forceReusePort && !opts.unix) o.reusePort = true;
    if (typeof opts.fetch === "function") {
      const fetch = opts.fetch;
      o.fetch = function (req, server) {
        const res = fetch.call(this, req, server);
        if (!draining) return res;
        return Promise.resolve(res).then((r) => {
          if (r instanceof Response) {
            try {
              r.headers.set("connection", "close");
            } catch {}
          }
          return r;
        });
      };
    }
    opts = o;
  }
  const server = originalServe.call(this, opts, ...rest);
  servers.add(server);
  const isApp = server && server.port && (appPort == null || server.port === appPort);
  if (isApp) {
    const socket = privateServer ? null : openPrivateSocket(options);
    report(socket ? { ev: "listening", port: server.port, socket } : { ev: "listening", port: server.port });
  }
  return server;
}

// Same handler (Bun fetch or node:http), private Unix socket, this worker only.
function openPrivateSocket(options) {
  if (!healthDir || !options || typeof options !== "object") return null;
  const path = `${healthDir}/${env.WARDEN_APP || "app"}.h${instance}-${workerId}.sock`;
  if (path.length > 100) return null; // sun_path limit
  try {
    fs.rmSync(path, { force: true });
    const p = Object.create(options);
    p.unix = path;
    p.port = undefined;
    p.hostname = undefined;
    p.reusePort = false;
    privateServer = originalServe.call(Bun, p);
    privatePath = path;
    return path;
  } catch {
    return null;
  }
}

function closePrivateSocket() {
  try {
    if (privateServer) privateServer.stop(true);
  } catch {}
  try {
    if (privatePath) fs.rmSync(privatePath, { force: true });
  } catch {}
  privateServer = null;
}

if (heartbeatMs > 0) {
  // Stops arriving when the event loop is blocked: Warden's watchdog notices.
  const t = setInterval(() => report({ ev: "heartbeat" }), heartbeatMs);
  if (t && typeof t.unref === "function") t.unref();
}

Bun.serve = wardenServe;

function pending() {
  let n = 0;
  for (const s of servers) n += s.pendingRequests || 0;
  return n;
}

async function markNodeResponsesClose() {
  // node:http responses don't go through a fetch handler; add the header there.
  try {
    const http = await import("node:http");
    const proto = http.ServerResponse && http.ServerResponse.prototype;
    if (!proto || proto.__wardenPatched) return;
    const set = (res) => {
      try {
        if (!res.headersSent) res.setHeader("connection", "close");
      } catch {}
    };
    const writeHead = proto.writeHead;
    proto.writeHead = function (...a) {
      set(this);
      return writeHead.apply(this, a);
    };
    const end = proto.end;
    proto.end = function (...a) {
      set(this);
      return end.apply(this, a);
    };
    proto.__wardenPatched = true;
  } catch {}
}

async function drain() {
  if (drainStarted) return;
  drainStarted = true;
  draining = true;
  report({ ev: "draining" });
  await markNodeResponsesClose();
  for (const s of servers) {
    try {
      s.stop(false); // stop accepting; keep serving open connections
    } catch {}
  }
  const t0 = Date.now();
  while (Date.now() - t0 < drainMs || pending() > 0) {
    await Bun.sleep(20);
  }
  for (const s of servers) {
    try {
      s.stop(true);
    } catch {}
  }
  closePrivateSocket();
  drainDone = true;
  drainFinished();
}

// The app's own SIGTERM handlers (e.g. NestJS enableShutdownHooks) would close
// every connection at once and cut the drain short. Keep them registered, but
// run them only after the drain: they still get to close DB pools etc.
function deferAppSigtermHandlers(ours) {
  const wrapped = new WeakMap();
  const wrap = (fn) => {
    let w = wrapped.get(fn);
    if (!w) {
      w = function (...a) {
        const p = drainPromise.then(() => {
          const r = fn.apply(this, a);
          if (!(r && typeof r.then === "function")) appHandlerNoPromise = true;
          return r;
        });
        appHandlers.add(p);
        return p;
      };
      wrapped.set(fn, w);
    }
    return w;
  };
  for (const m of ["on", "addListener", "once", "prependListener", "prependOnceListener"]) {
    const orig = process[m];
    if (typeof orig !== "function") continue;
    process[m] = function (ev, fn, ...rest) {
      if (ev === "SIGTERM" && typeof fn === "function" && fn !== ours) fn = wrap(fn);
      return orig.call(this, ev, fn, ...rest);
    };
  }
  for (const m of ["off", "removeListener"]) {
    const orig = process[m];
    if (typeof orig !== "function") continue;
    process[m] = function (ev, fn, ...rest) {
      if (ev === "SIGTERM" && wrapped.has(fn)) fn = wrapped.get(fn);
      return orig.call(this, ev, fn, ...rest);
    };
  }
}

if (inWorker) {
  // Close our listeners however this Worker ends (uncaught error, process.exit).
  process.on("exit", () => {
    for (const s of servers) {
      try {
        s.stop(true);
      } catch {}
    }
    closePrivateSocket();
  });
  self.addEventListener("message", async (e) => {
    if (e.data && e.data.__warden === "shutdown") {
      await drain();
      process.exit(0);
    }
  });
} else if (drainMs > 0) {
  const onSigterm = async () => {
    if (drainStarted) {
      // The app re-raised SIGTERM after its own cleanup (NestJS does this
      // once it has removed its handler): nothing left to wait for.
      if (drainDone) process.exit(0);
      return;
    }
    await drain();
    // App handlers (deferred above, including `once` ones) run their cleanup
    // now. Async ones: wait for them, then exit. Callback-style ones (e.g.
    // `pool.end(cb)` then `process.exit`) decide when to exit themselves; the
    // process also exits on its own once nothing keeps it alive. Warden's
    // grace period is the upper bound either way.
    await Promise.allSettled([...appHandlers]);
    if (!appHandlerNoPromise) process.exit(0);
  };
  process.on("SIGTERM", onSigterm);
  deferAppSigtermHandlers(onSigterm);
}

if (!inWorker) {
  process.on("exit", closePrivateSocket);
}

if (inWorker && env.WARDEN_ENTRY) {
  await import(env.WARDEN_ENTRY);
}

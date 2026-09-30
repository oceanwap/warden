// Warden shim for Bun and Node. Injected by Warden; apps don't import it.
//
// Process mode: loaded with `bun --preload <this file>` or
//               `node --import <this file>`.
// Worker mode:  (Bun) the entry module of every Worker; it installs the hooks
//               and then imports the app (WARDEN_ENTRY).
//
// What it does (see docs/architecture.md, findings F3, F7-F11):
//  1. Makes every server on the app's port join the SO_REUSEPORT group, so N
//     workers share it: Bun.serve gets `reusePort: true` (Bun's node:http goes
//     through Bun.serve); under Node, net.Server#listen gets `reusePort: true`
//     (Node 22.12+ / 23.1+).
//  2. Serves the same app on a private Unix socket for this worker only, so
//     Warden can health-check *this* worker (the shared port reaches a random one).
//  3. Reports listening servers (readiness), `process.send('ready')` (PM2's
//     wait_ready) and a heartbeat from the event loop (watchdog) to Warden.
//  4. Drains on the stop signal (WARDEN_STOP_SIGNAL, SIGTERM by default; SIGINT
//     for apps written for PM2) or on a shutdown message (worker mode): closes
//     listeners, answers remaining requests with `Connection: close`, waits
//     WARDEN_DRAIN_MS and for in-flight requests, then exits.
//  5. In Workers, closes listeners from an exit hook: Bun does not close the
//     listening socket of a Worker that dies, which would black-hole 1/N of
//     new connections.

import fs from "node:fs";
import { createRequire } from "node:module";
import { isMainThread } from "node:worker_threads";

// node:http and node:net are loaded only when the app uses them: in Bun,
// importing node:http costs ~6 MB per worker, wasted on a Bun.serve app.
// By the time the shim needs them, the app has loaded them (free).
const require = createRequire(import.meta.url);
let httpModule = null;
const http = () => (httpModule ??= require("node:http"));

const env = process.env;
const isBun = typeof Bun !== "undefined";
const inWorker = !isMainThread;
const workerId = Number(env.WARDEN_WORKER_ID || 0);
const ipcFd = env.WARDEN_IPC_FD ? Number(env.WARDEN_IPC_FD) : null;
const drainMs = Number(env.WARDEN_DRAIN_MS ?? 500);
const forceReusePort = env.WARDEN_REUSE_PORT === "1";
const healthDir = env.WARDEN_HEALTH_DIR || "";
const instance = env.WARDEN_INSTANCE || String(process.pid);
const heartbeatMs = Number(env.WARDEN_HEARTBEAT_MS || 0);
const stopSignal = env.WARDEN_STOP_SIGNAL || "SIGTERM";
// The app's own port. Other servers the app starts (metrics, admin) are
// neither readiness signals nor health-check targets.
const appPort = env.PORT ? Number(env.PORT) : null;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Worker mode: each Worker gets its instance index (PM2's NODE_APP_INSTANCE).
if (inWorker && env.WARDEN_INSTANCE_VAR) {
  process.env[env.WARDEN_INSTANCE_VAR] = String(Math.max(0, workerId - 1));
}

const servers = new Set();
let privateServer = null;
let privatePath = null;
let draining = false;
let drainStarted = false;
let drainDone = false;
let drainFinished;
const drainPromise = new Promise((r) => (drainFinished = r));
// Promises of app stop-signal handlers we deferred until after the drain,
// and whether any of them was callback-style (returned no promise).
const appHandlers = new Set();
let appHandlerNoPromise = false;
// Node: requests in flight, and every open connection with its state.
let nodeInflight = 0;
const nodeConns = new Set();
// The app has a node:http server (Bun: one went through Bun.serve), so
// node:http is loaded and its responses need `Connection: close` in a drain.
let appUsesNodeHttp = false;

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

function privateSocketPath() {
  if (!healthDir) return null;
  const path = `${healthDir}/${env.WARDEN_APP || "app"}.h${instance}-${workerId}.sock`;
  return path.length > 100 ? null : path; // sun_path limit
}

// ------------------------------------------------------------------- Bun

let originalServe = null;

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
  if (options && typeof options === "object" && typeof options.onNodeHTTPRequest === "function") appUsesNodeHttp = true;
  const server = originalServe.call(this, opts, ...rest);
  servers.add(server);
  const isApp = server && server.port && (appPort == null || server.port === appPort);
  if (isApp) {
    const socket = privateServer ? null : openPrivateBun(options);
    report(socket ? { ev: "listening", port: server.port, socket } : { ev: "listening", port: server.port });
  }
  return server;
}

// Same handler (Bun fetch or node:http), private Unix socket, this worker only.
function openPrivateBun(options) {
  const path = privateSocketPath();
  if (!path || !options || typeof options !== "object") return null;
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

if (isBun) {
  originalServe = Bun.serve;
  Bun.serve = wardenServe;
}

// ------------------------------------------------------------------ Node

// listen(port[, host][, backlog][, cb]) and listen({ port, ... }[, cb]) with
// reusePort added; pipes, handles and fds are left alone.
function withReusePort(args) {
  const a0 = args[0];
  if (a0 && typeof a0 === "object" && !Array.isArray(a0)) {
    if (a0.path !== undefined || a0.fd !== undefined || a0._handle || a0.handle) return args;
    return [{ ...a0, reusePort: true }, ...args.slice(1)];
  }
  if (typeof a0 === "number" || (typeof a0 === "string" && /^\d+$/.test(a0))) {
    const opts = { port: Number(a0), reusePort: true };
    let i = 1;
    if (typeof args[i] === "string") opts.host = args[i++];
    if (typeof args[i] === "number") opts.backlog = args[i++];
    const cb = typeof args[i] === "function" ? args[i] : undefined;
    return cb ? [opts, cb] : [opts];
  }
  if (a0 === undefined || typeof a0 === "function") {
    // listen() / listen(cb): a random port; nothing to share.
    return args;
  }
  return args;
}

// An http(s).Server, told apart without loading node:http for a plain TCP
// server: every http.Server has `maxHeadersCount` (null by default).
const isHttpServer = (server) => "maxHeadersCount" in server && typeof server.setTimeout === "function";

function trackNodeServer(server) {
  if (servers.has(server)) return;
  servers.add(server);
  if (isHttpServer(server)) {
    appUsesNodeHttp = true;
    server.on("connection", (sock) => {
      sock.__warden = { served: false, busy: false };
      nodeConns.add(sock);
      sock.once("close", () => nodeConns.delete(sock));
    });
    server.on("request", (req, res) => {
      nodeInflight++;
      const st = req.socket && req.socket.__warden;
      if (st) st.busy = true;
      res.once("close", () => {
        nodeInflight--;
        if (st) {
          st.busy = false;
          st.served = true;
        }
      });
      if (draining) {
        try {
          if (!res.headersSent) res.setHeader("connection", "close");
        } catch {}
      }
    });
  }
  server.once("listening", () => {
    const addr = server.address();
    if (!addr || typeof addr !== "object") return; // a Unix socket
    const isApp = appPort == null || addr.port === appPort;
    if (!isApp) return;
    if (!privateServer && isHttpServer(server)) {
      openPrivateNode(server, (socket) =>
        report(socket ? { ev: "listening", port: addr.port, socket } : { ev: "listening", port: addr.port }),
      );
    } else {
      report({ ev: "listening", port: addr.port });
    }
  });
}

function openPrivateNode(appServer, done) {
  const path = privateSocketPath();
  if (!path) return done(null);
  try {
    fs.rmSync(path, { force: true });
    const p = http().createServer((req, res) => appServer.emit("request", req, res));
    privateServer = p;
    p.once("error", () => done(null));
    p.listen(path, () => {
      privatePath = path;
      done(path);
    });
  } catch {
    done(null);
  }
}

if (!isBun) {
  const net = require("node:net");
  const origListen = net.Server.prototype.listen;
  net.Server.prototype.listen = function (...args) {
    if (this !== privateServer) {
      trackNodeServer(this);
      if (forceReusePort) args = withReusePort(args);
    }
    return origListen.apply(this, args);
  };
}

// ---------------------------------------------------------------- common

function closePrivateSocket() {
  try {
    if (privateServer) {
      if (isBun) privateServer.stop(true);
      else privateServer.close();
    }
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

// PM2 apps call process.send('ready') (wait_ready) and some call process.send
// unguarded. Without an IPC channel it would be undefined and throw; here it
// reports readiness to Warden instead.
if (!inWorker && typeof process.send !== "function") {
  process.send = function (msg, ...rest) {
    if (msg === "ready" || (msg && typeof msg === "object" && msg.type === "ready")) report({ ev: "ready" });
    const cb = rest.find((x) => typeof x === "function");
    if (cb) queueMicrotask(() => cb(null));
    return true;
  };
}

function pending() {
  if (!isBun) return nodeInflight;
  let n = 0;
  for (const s of servers) n += s.pendingRequests || 0;
  return n;
}

async function markNodeResponsesClose() {
  // node:http responses (Bun's node:http included) don't go through a fetch
  // handler; add the header there. Nothing to do if the app has no
  // node:http server.
  if (!appUsesNodeHttp) return;
  try {
    const mod = http();
    const proto = mod.ServerResponse && mod.ServerResponse.prototype;
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

function stopAccepting(s) {
  try {
    if (isBun) s.stop(false); // stop accepting; keep serving open connections
    else s.close();
  } catch {}
}

// Node: close keep-alive connections that already served a request and sit
// idle. Not Node's closeIdleConnections(): it also closes connections just
// accepted whose first request hasn't been parsed yet, and those clients
// would see an empty reply. New connections get their request answered
// (with Connection: close) and close after it.
function closeServedIdle() {
  for (const sock of nodeConns) {
    const st = sock.__warden;
    if (st && st.served && !st.busy) {
      try {
        sock.destroy();
      } catch {}
    }
  }
}

function stopAll(s) {
  try {
    if (isBun) s.stop(true);
    else if (typeof s.closeAllConnections === "function") s.closeAllConnections();
  } catch {}
}

async function drain() {
  if (drainStarted) return;
  drainStarted = true;
  draining = true;
  report({ ev: "draining" });
  await markNodeResponsesClose();
  for (const s of servers) stopAccepting(s);
  const t0 = Date.now();
  while (Date.now() - t0 < drainMs || pending() > 0) {
    if (!isBun) closeServedIdle();
    await sleep(20);
  }
  for (const s of servers) stopAll(s);
  closePrivateSocket();
  drainDone = true;
  drainFinished();
}

// The app's own stop-signal handlers (e.g. NestJS enableShutdownHooks) would
// close every connection at once and cut the drain short. Keep them
// registered, but run them only after the drain: they still get to close DB
// pools etc.
function deferAppStopHandlers(ours) {
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
      if (ev === stopSignal && typeof fn === "function" && fn !== ours) fn = wrap(fn);
      return orig.call(this, ev, fn, ...rest);
    };
  }
  for (const m of ["off", "removeListener"]) {
    const orig = process[m];
    if (typeof orig !== "function") continue;
    process[m] = function (ev, fn, ...rest) {
      if (ev === stopSignal && wrapped.has(fn)) fn = wrapped.get(fn);
      return orig.call(this, ev, fn, ...rest);
    };
  }
}

if (inWorker) {
  // Close our listeners however this Worker ends (uncaught error, process.exit).
  process.on("exit", () => {
    for (const s of servers) stopAll(s);
    closePrivateSocket();
  });
  self.addEventListener("message", async (e) => {
    if (e.data && e.data.__warden === "shutdown") {
      await drain();
      process.exit(0);
    }
  });
} else if (drainMs > 0) {
  const onStop = async () => {
    if (drainStarted) {
      // The app re-raised the signal after its own cleanup (NestJS does this
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
  process.on(stopSignal, onStop);
  deferAppStopHandlers(onStop);
}

if (!inWorker) {
  process.on("exit", closePrivateSocket);
}

if (inWorker && env.WARDEN_ENTRY) {
  await import(env.WARDEN_ENTRY);
}

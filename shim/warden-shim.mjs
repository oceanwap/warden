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
//  6. Hot standby (WARDEN_STANDBY=1, `[workers] standby`): the app starts
//     completely but its listen on the app's port is deferred until Warden
//     sends `{"cmd":"promote"}` on fd 3 (see the "standby" section).

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
// `let`: a promoted standby takes the worker id of the slot it fills.
let workerId = Number(env.WARDEN_WORKER_ID || 0);
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
// Hot standby (process mode with a port only): listens on the app's port
// deferred until promoted. See the "standby" section.
const standby = env.WARDEN_STANDBY === "1" && !inWorker && ipcFd != null && appPort != null;
let promoted = false;

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
  if (standby && !promoted && deferrableBun(options)) return deferBunServe(options, rest);
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

let nodeOrigListen = null; // for the standby's warm-up listen

if (!isBun) {
  const net = require("node:net");
  const origListen = net.Server.prototype.listen;
  nodeOrigListen = origListen;
  net.Server.prototype.listen = function (...args) {
    if (this !== privateServer) {
      trackNodeServer(this);
      if (standby && !promoted && deferNodeListen(this, args)) return this;
      if (forceReusePort) args = withReusePort(args);
    }
    return origListen.apply(this, args);
  };
}

// --------------------------------------------------------------- standby
//
// `[workers] standby = N`: Warden keeps N extra workers started but not
// receiving traffic. The app initializes completely; only its listen on the
// app's port is deferred (servers on other ports start as usual), and the
// worker reports `standby_ready`. On `{"cmd":"promote","worker":W}` (fd 3 is
// read only by standbys) it listens for real, takes worker id W (and its
// instance variable), and reports `listening` as any worker does.
//
// - Bun.serve: the app gets a Proxy over a stand-in server (its own options
//   on a private Unix socket: every method works, nothing reaches it from the
//   network); at promotion the real server takes the stand-in's place.
// - node:http under Bun: Bun's node:http insists on a real Bun server, so it
//   gets the stand-in; the node:http server is found when it emits
//   `listening`, and at promotion its internal server is swapped for the
//   real one (address(), close() then act on the real listener).
// - node:http / net under Node: listen() is recorded and called at promotion
//   (its callback and `listening` come then).
// The first stand-in doubles as the private health socket when Warden asked
// for one, so Warden can health-check the standby before promoting it.

// Bun: { options, rest, standIn, current, real, stopped, node, path, health }
// Node: { server, args }
const deferredListens = [];
let standbyReported = false;
let emitHook = null;

// The port Bun.serve listens on for these options (Bun's own defaults).
function bunPort(o) {
  return Number(o.port ?? env.BUN_PORT ?? env.PORT ?? env.NODE_PORT ?? 3000);
}

function deferrableBun(o) {
  return o && typeof o === "object" && !o.unix && bunPort(o) === appPort;
}

function standbyReady(socket) {
  if (standbyReported) return;
  standbyReported = true;
  report(socket ? { ev: "standby_ready", port: appPort, socket } : { ev: "standby_ready", port: appPort });
}

// Listening for real would take traffic before promotion: refuse instead.
function standbyCannotDefer(why) {
  try {
    process.stderr.write(`warden: standby cannot defer the app's listen on port ${appPort}: ${why}\n`);
  } catch {}
  process.exit(70);
}

function standInPath() {
  if (!privateServer) {
    const path = privateSocketPath();
    if (path) return { path, health: true };
  }
  const dir = env.WARDEN_STANDBY_DIR || healthDir;
  const path = dir ? `${dir}/${env.WARDEN_APP || "app"}.s${instance}-${deferredListens.length}.sock` : null;
  return path && path.length <= 100 ? { path, health: false } : null;
}

function deferBunServe(options, rest) {
  const at = standInPath();
  if (!at) return standbyCannotDefer("no directory for its stand-in socket (WARDEN_STANDBY_DIR)");
  let standIn;
  try {
    fs.rmSync(at.path, { force: true });
    const p = Object.create(options);
    p.unix = at.path;
    p.port = undefined;
    p.hostname = undefined;
    p.reusePort = false;
    standIn = originalServe.call(Bun, p);
  } catch (e) {
    return standbyCannotDefer(`its stand-in server failed: ${e && e.message}`);
  }
  if (at.health) {
    privateServer = standIn;
    privatePath = at.path;
  }
  const rec = { options, rest, standIn, current: standIn, real: null, stopped: false, node: null, ...at };
  deferredListens.push(rec);
  standbyReady(at.health ? at.path : null);
  if (typeof options.onNodeHTTPRequest === "function") {
    appUsesNodeHttp = true;
    hookListeningEmit();
    return standIn;
  }
  return standbyProxy(rec);
}

// What the app sees until promotion: the stand-in, except where it would
// tell (its port and address are the app's).
function standbyProxy(rec) {
  const host = rec.options.hostname;
  const before = {
    port: () => appPort,
    hostname: () => host || "localhost",
    url: () => new URL(`http://${host || "localhost"}:${appPort}/`),
    address: () => ({ address: host || "0.0.0.0", family: host && host.includes(":") ? "IPv6" : "IPv4", port: appPort }),
    stop: () =>
      function (...a) {
        rec.stopped = true; // the app stopped it: don't listen at promotion either
        return rec.standIn.stop(...a);
      },
  };
  const bound = new Map();
  return new Proxy(rec.standIn, {
    get(_, prop) {
      if (!rec.real && Object.hasOwn(before, prop)) return before[prop]();
      const cur = rec.current;
      const v = Reflect.get(cur, prop, cur);
      if (typeof v !== "function" || prop === "constructor") return v;
      let b = bound.get(prop);
      if (!b || b.on !== cur || b.fn !== v) {
        b = { on: cur, fn: v, call: v.bind(cur) };
        bound.set(prop, b);
      }
      return b.call;
    },
    set(_, prop, value) {
      return Reflect.set(rec.current, prop, value, rec.current);
    },
    has(_, prop) {
      return prop in rec.current;
    },
  });
}

// Bun's node:http keeps the Bun server it got under a symbol and emits
// `listening` right after: find it there (only while a standby).
function hookListeningEmit() {
  if (emitHook) return;
  const EE = require("node:events");
  const orig = EE.prototype.emit;
  const hook = function (ev) {
    if (ev === "listening" && !promoted) {
      for (const r of deferredListens) {
        if (!r.standIn || r.node) continue;
        const key = Object.getOwnPropertySymbols(this).find((s) => this[s] === r.standIn);
        if (key) {
          r.node = { server: this, key };
          const host = r.options.hostname;
          const family = host && !host.includes(":") ? "IPv4" : "IPv6";
          // Until promotion, address() is the app's port, not the stand-in's path.
          Object.defineProperty(this, "address", {
            configurable: true,
            writable: true,
            value: () => ({ address: host || "::", family, port: appPort }),
          });
        }
      }
    }
    return orig.apply(this, arguments);
  };
  EE.prototype.emit = hook;
  emitHook = { EE, orig, hook };
}

function unhookListeningEmit() {
  // Only if nobody wrapped it since (then our hook stays, idle once promoted).
  if (emitHook && emitHook.EE.prototype.emit === emitHook.hook) emitHook.EE.prototype.emit = emitHook.orig;
}

function promoteBun(rec) {
  if (rec.stopped) return;
  // `promoted` is set: this is the app's own Bun.serve call, made now
  // (reusePort, drain wrapper, `listening` report).
  const real = wardenServe.call(Bun, rec.options, ...rec.rest);
  rec.real = real;
  rec.current = real;
  if (rec.node) {
    rec.node.server[rec.node.key] = real;
    delete rec.node.server.address;
  } else if (typeof rec.options.onNodeHTTPRequest === "function") {
    try {
      process.stderr.write("warden: promoted, but the node:http server was not found; its address()/close() act on the stand-in\n");
    } catch {}
    return; // keep the stand-in it holds alive
  }
  if (!rec.health) {
    try {
      rec.standIn.stop(true);
    } catch {}
    try {
      fs.rmSync(rec.path, { force: true });
    } catch {}
  }
}

// node:http / net under Node: `listen(port, …)` or `listen({ port }, …)` on
// the app's port.
function nodeListenPort(args) {
  const a0 = args[0];
  if (a0 && typeof a0 === "object" && !Array.isArray(a0)) {
    if (a0.path !== undefined || a0.fd !== undefined || a0._handle || a0.handle) return null;
    return Number(a0.port);
  }
  if (typeof a0 === "number" || (typeof a0 === "string" && /^\d+$/.test(a0))) return Number(a0);
  return null;
}

function deferNodeListen(server, args) {
  if (nodeListenPort(args) !== appPort) return false;
  if (!deferredListens.length) warmNodeListen(args);
  deferredListens.push({ server, args });
  if (healthDir && !privateServer && isHttpServer(server)) {
    openPrivateNode(server, (socket) => standbyReady(socket));
  } else if (!privateServer || privatePath) {
    standbyReady(privatePath);
  }
  return true;
}

// Node compiles and loads its listen path (dns, cluster, TCP binding) on
// first use: ~10 ms that would land on the promotion. Run it once now, on
// an ephemeral port nobody connects to (measured: promotion 9-13 ms → 3 ms).
function warmNodeListen(args) {
  if (!nodeOrigListen) return;
  try {
    const a0 = args[0];
    const host = a0 && typeof a0 === "object" ? a0.host : typeof args[1] === "string" ? args[1] : undefined;
    const net = require("node:net");
    const w = new net.Server();
    w.on("error", () => {});
    nodeOrigListen.call(w, { port: 0, host, reusePort: forceReusePort }, () => w.close());
    w.unref();
  } catch {}
}

function promote(msg) {
  if (promoted) return;
  promoted = true;
  if (Number.isInteger(msg.worker) && msg.worker > 0) {
    workerId = msg.worker;
    env.WARDEN_WORKER_ID = String(msg.worker);
    if (env.WARDEN_INSTANCE_VAR) env[env.WARDEN_INSTANCE_VAR] = String(msg.worker - 1);
  }
  if (Number.isInteger(msg.count)) env.WARDEN_WORKER_COUNT = String(msg.count);
  delete env.WARDEN_STANDBY;
  unhookListeningEmit();
  // A listen that throws (EADDRINUSE…) crashes the worker, as it would have
  // at startup: Warden restarts the slot.
  for (const d of deferredListens) {
    if (d.server) d.server.listen(...d.args);
    else promoteBun(d);
  }
  // For apps that start instance-specific work (cron on instance 0) late.
  process.emit("warden:promote", { worker: workerId });
}

// Commands from Warden on fd 3 (a socket). Reads run on the runtime's
// thread pool, so the event loop never waits; the pending read also keeps a
// Node standby alive while nothing listens. No reads after promotion.
function readWardenCommands() {
  const buf = Buffer.alloc(4096);
  let acc = "";
  const next = () =>
    fs.read(ipcFd, buf, 0, buf.length, null, (err, n) => {
      if (err) {
        if (err.code === "EAGAIN" || err.code === "EINTR") setTimeout(next, 10);
        return; // fd 3 is gone: nothing can promote this worker
      }
      if (!n) return; // Warden closed its end (it is exiting)
      acc += buf.toString("utf8", 0, n);
      for (let i = acc.indexOf("\n"); i >= 0; i = acc.indexOf("\n")) {
        const line = acc.slice(0, i);
        acc = acc.slice(i + 1);
        let msg = null;
        try {
          msg = JSON.parse(line);
        } catch {}
        if (msg && msg.cmd === "promote") promote(msg);
      }
      if (acc.length > 65536) acc = "";
      if (!promoted) next();
    });
  next();
}

if (standby) {
  readWardenCommands();
  process.on("exit", () => {
    for (const d of deferredListens) {
      if (d.path && !d.health) {
        try {
          fs.rmSync(d.path, { force: true });
        } catch {}
      }
    }
  });
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

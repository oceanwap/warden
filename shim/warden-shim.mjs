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
//     WARDEN_DRAIN_MS and for in-flight requests, then exits. Until then it
//     adds no work to a request: Bun.serve runs the app's own fetch handler
//     until the drain swaps in one that adds the header (server.reload), and
//     node:http's requests in flight are read off the open connections when
//     the drain needs them (one listener per connection, none per request).
//  5. In Workers, closes listeners from an exit hook: Bun does not close the
//     listening socket of a Worker that dies, which would black-hole 1/N of
//     new connections.
//  6. Ends long-lived connections during a drain (WARDEN_LONG_LIVED_MS after
//     it starts): WebSockets get a 1001 close frame, SSE responses a clean end
//     of the chunked stream, so clients reconnect to the new workers instead of
//     holding the old one until SIGKILL. See "long-lived connections" below.
//  7. Hot standby (WARDEN_STANDBY=1, `[workers] standby`): the app starts
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
// Node's `reusePort` (libuv's UV_TCP_REUSEPORT) exists only where the kernel
// spreads connections across the listeners: Linux, FreeBSD 12+, DragonFly,
// Solaris 11.4, AIX 7.3. Elsewhere (macOS) listen() fails with ENOTSUP, so
// a Node app could not listen at all; there it listens as it asked (one
// worker per port: docs/platforms.md). Bun sets SO_REUSEPORT on macOS too.
const nodeReusePort = forceReusePort && ["linux", "freebsd", "dragonfly", "sunos", "aix"].includes(process.platform);
// Warden accepts on the app's port and hands each connection to a worker
// over Node's IPC channel (src/handoff.rs): where the kernel does not spread
// a shared port (macOS) and the app has several workers. A node:http server
// on the app's port then takes those connections instead of listening.
// Bun takes a handed-over socket from 1.4.0 on (1.3.x: "TODO case
// net.Socket", and the connection is lost): older ones listen as before.
const bunTakesSockets = !isBun || (([maj, min]) => maj > 1 || (maj === 1 && min >= 4))(Bun.version.split(".").map(Number));
const handoff = env.WARDEN_HANDOFF === "1" && !inWorker && typeof process.send === "function" && bunTakesSockets;
// `[app] address` on Linux: the workers still listen on the app's port
// themselves (the kernel spreads it), and only the hostname router's
// connections come over the IPC channel.
const handoffListens = handoff && env.WARDEN_HANDOFF_LISTEN === "1";
// Say so, so Warden can suggest a newer Bun (after the module has loaded:
// `report` needs state declared below).
if (env.WARDEN_HANDOFF === "1" && !inWorker && !bunTakesSockets) {
  queueMicrotask(() => report({ ev: "handoff_unsupported", message: Bun.version }));
}
const healthDir = env.WARDEN_HEALTH_DIR || "";
const instance = env.WARDEN_INSTANCE || String(process.pid);
const heartbeatMs = Number(env.WARDEN_HEARTBEAT_MS || 0);
const stopSignal = env.WARDEN_STOP_SIGNAL || "SIGTERM";
// How long a drain lets WebSockets and SSE streams end by themselves before
// closing them (0 = leave them alone). Only meaningful when there is a drain.
const longLivedMs = Number(env.WARDEN_LONG_LIVED_MS ?? 2000);
// Whether this worker drains at all: a Worker on the host's message, a
// process on the stop signal unless WARDEN_DRAIN_MS is 0.
const drains = inWorker || drainMs > 0;
const longLived = longLivedMs > 0 && drains;
// The app's own port. Other servers the app starts (metrics, admin) are
// neither readiness signals nor health-check targets.
const appPort = env.PORT ? Number(env.PORT) : null;
// `[app] address`: the app's own IP on this server. Its listen on the app's
// port uses that address, whatever host the app asked for, so several apps
// can each have the same port (443) on their own address.
const appAddress = env.WARDEN_ADDRESS || null;
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
// Node: every open connection of the app's servers, and of the private
// health socket (requests in flight are read off them in a drain).
const nodeConns = new Set();
const privateConns = new Set();
// The app has a node:http server (Bun: one went through Bun.serve), so
// node:http is loaded and its responses need `Connection: close` in a drain.
let appUsesNodeHttp = false;
// Hot standby (process mode with a port only): listens on the app's port
// deferred until promoted. See the "standby" section.
const standby = env.WARDEN_STANDBY === "1" && !inWorker && ipcFd != null && appPort != null;
let promoted = false;
// A standby's reader of Warden's commands (Node: a net.Socket owning fd 3),
// and whether fd 3 is still open (that socket closes it when it ends).
let ipcReader = null;
let ipcOpen = true;

function report(msg) {
  msg.worker = workerId;
  if (inWorker) {
    try {
      postMessage({ __warden: msg });
    } catch {}
    return;
  }
  if (ipcFd == null || !ipcOpen) return;
  ipcSend(JSON.stringify(msg) + "\n", msg.ev === "heartbeat");
}

// Reports never wait for Warden: fd 3 is non-blocking (Warden sets it up so,
// with room for hours of heartbeats), so a stopped Warden (SIGSTOP, a frozen
// VM, a bug) can't freeze the event loop on a full socket. When it is full,
// a heartbeat waits as the latest one only (the next replaces it), and every
// other line waits whole and in order, sent once there is room (a timer
// retries, without keeping the process alive).
let ipcPending = null; // bytes that go first (a line cut short, lines kept)
let ipcBeat = null; // the latest heartbeat not sent yet
let ipcRetry = null;
let ipcRetryMs = 10;
const IPC_BACKLOG_MAX = 1 << 20; // lines kept beyond this are dropped (an app spamming `ready`)
function ipcSend(line, droppable) {
  if (droppable) ipcBeat = line;
  else if (!ipcPending) ipcPending = Buffer.from(line);
  else if (ipcPending.length < IPC_BACKLOG_MAX) ipcPending = Buffer.concat([ipcPending, Buffer.from(line)]);
  ipcFlush();
}
function ipcFlush() {
  while (ipcOpen && (ipcPending || ipcBeat)) {
    const beat = !ipcPending;
    const buf = ipcPending || Buffer.from(ipcBeat);
    let n;
    try {
      n = fs.writeSync(ipcFd, buf);
    } catch (e) {
      if (e && e.code === "EINTR") continue;
      if (e && (e.code === "EAGAIN" || e.code === "EWOULDBLOCK")) return ipcRetryLater();
      ipcPending = ipcBeat = null; // Warden is gone: no one to wait for
      return;
    }
    if (!(n > 0)) return ipcRetryLater();
    if (beat) ipcBeat = null;
    ipcPending = n < buf.length ? buf.subarray(n) : null;
  }
  ipcRetryMs = 10;
}
function ipcRetryLater() {
  if (ipcRetry) return;
  ipcRetry = setTimeout(() => {
    ipcRetry = null;
    ipcFlush();
  }, ipcRetryMs);
  if (typeof ipcRetry.unref === "function") ipcRetry.unref();
  ipcRetryMs = Math.min(ipcRetryMs * 2, 1000);
}

function privateSocketPath() {
  if (!healthDir) return null;
  const path = `${healthDir}/${env.WARDEN_APP || "app"}.h${instance}-${workerId}.sock`;
  return path.length > 100 ? null : path; // sun_path limit
}

// ------------------------------------------------------------------- Bun

let originalServe = null;

// Bun.serve servers that run the app's own fetch handler, and the options
// they run with. Requests go straight to that handler: the shim adds nothing
// per request. When a drain starts, server.reload() gives each one a handler
// that adds `Connection: close` (requests in flight finish with the old
// one), so keep-alive clients move to the new workers.
const bunApps = new Map(); // server -> { opts, reload: the native reload }

function wardenServe(options, ...rest) {
  if (appAddress && deferrableBun(options)) {
    options = Object.create(options);
    options.hostname = appAddress;
  }
  if (standby && !promoted && deferrableBun(options)) return deferBunServe(options, rest);
  const nodeHttp = !!options && typeof options === "object" && typeof options.onNodeHTTPRequest === "function";
  let opts = options;
  if (opts && typeof opts === "object") {
    // Object.create keeps the caller's object (and its prototype methods)
    // intact while overriding just what we need.
    opts = withShimOptions(opts);
    if (forceReusePort && !options.unix) opts.reusePort = true;
  }
  if (nodeHttp) {
    appUsesNodeHttp = true;
    if (longLived) watchBunNodeServer();
  }
  const own = drains && opts !== options && !nodeHttp && runsAppHandler(options);
  // The handoff: on a private port if this Bun can adopt descriptors (below).
  const adopting = handoff && bunAdopts !== false && !nodeHttp && deferrableBun(options);
  if (adopting && !handoffListens) opts = onPrivatePort(opts);
  // A server started while draining (rare) drains from its first request.
  let server = originalServe.call(this, own && draining ? drainingOptions(opts) : opts, ...rest);
  if (adopting) {
    if (server && typeof server.adopt === "function") {
      bunAdopts = true;
      servers.add(server);
      if (own) trackBunApp(server, opts);
      takeAdoptions(server, options);
      return server;
    }
    // This Bun has no adopt(): the app listens on its port, as before.
    bunAdopts = false;
    if (!handoffListens) {
      try {
        server.stop(true);
      } catch {}
      opts = Object.getPrototypeOf(opts);
      server = originalServe.call(this, own && draining ? drainingOptions(opts) : opts, ...rest);
    }
  }
  servers.add(server);
  if (own) trackBunApp(server, opts);
  const isApp = server && server.port && (appPort == null || server.port === appPort);
  if (isApp) {
    const socket = privateServer ? null : openPrivateBun(options);
    // Bun's node:http server on the port, taking the router's connections too (takeHandoff).
    const handed = handoffListens && nodeHttp && handoffServers.size > 0 ? { handoff: true } : {};
    report({ ev: "listening", port: server.port, ...handed, ...(socket ? { socket } : {}) });
  }
  return server;
}

// ------------------------------------------------- handoff (Bun.serve)
//
// Bun.serve has no "connection" event to hand a socket to, but a Bun with
// `server.adopt(fd)` serves an already-connected descriptor as if its own
// listener had accepted it (TLS included: the ClientHello is still in the
// kernel buffer). So in handoff mode the app's Bun.serve on its port listens
// on an ephemeral 127.0.0.1 port instead (everything else as the app asked:
// fetch, routes, tls, websocket...), the worker reports `adopt: true`, and
// Warden sends its connections as `"type":"fd"`: the bare descriptor, which
// Bun's IPC hands over as a number without reading from it.
//
// Whether this Bun can adopt is only known from a server: the first one
// tells (null: not asked yet). Without adopt() (Bun up to 1.4.x) that server
// is stopped and the app listens on its port as it always did.

let bunAdopts = null;
// The app's latest Bun.serve on its port: one that stopped and served again
// (server.stop(), then Bun.serve) takes the connections from then on.
let adoptServer = null;

function onPrivatePort(opts) {
  const o = Object.create(opts);
  o.port = 0;
  o.hostname = "127.0.0.1";
  o.reusePort = false;
  return o;
}

function takeAdoptions(server, options) {
  if (!adoptServer) {
    process.on("message", (msg, fd) => {
      if (msg !== "warden:connection" || typeof fd !== "number") return;
      // adopt() closes the descriptor when it cannot take it (a stopped
      // server).
      adoptServer.adopt(fd);
    });
  }
  adoptServer = server;
  const socket = privateServer ? privatePath : openPrivateBun(options);
  const host = options.hostname ?? null;
  report({ ev: "listening", port: appPort, handoff: true, adopt: true, host, ...(socket ? { socket } : {}) });
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
    // Warden checks this socket over plain HTTP: an app that serves TLS
    // itself must not make the private socket TLS too, nor HTTP/3 (which
    // Bun refuses without TLS).
    p.tls = undefined;
    p.http3 = undefined;
    privateServer = originalServe.call(Bun, p);
    privatePath = path;
    return path;
  } catch {
    return null;
  }
}

// The app's options plus what the shim adds to every server: WebSocket
// open/close tracking (long-lived connections, below).
function withShimOptions(options) {
  const o = Object.create(options);
  if (longLived && options.websocket && typeof options.websocket === "object") o.websocket = trackBunWebSockets(options.websocket);
  return o;
}

// Does this Bun.serve() call run the app's own handlers: a `fetch` function
// and/or `routes`?
const runsAppHandler = (o) => typeof o.fetch === "function" || (!!o.routes && typeof o.routes === "object");

// `o` with handlers that add `Connection: close` to every response: what a
// server runs once its drain has started. That is `fetch`, every function in
// `routes` (a static Response there becomes a function answering with a copy)
// and `error`. A result that is not a promise stays synchronous.
function drainingOptions(o) {
  const d = Object.create(o);
  if (typeof o.fetch === "function") d.fetch = closing(o.fetch);
  if (o.routes && typeof o.routes === "object") d.routes = closingRoutes(o.routes);
  if (typeof o.error === "function") d.error = closing(o.error);
  return d;
}

function closing(handler) {
  return function (...args) {
    const res = handler.apply(this, args);
    if (res instanceof Response) return connectionClose(res);
    if (res && typeof res.then === "function") return res.then(connectionClose);
    return res;
  };
}

const HTTP_METHODS = new Set(["GET", "HEAD", "POST", "PUT", "DELETE", "PATCH", "OPTIONS"]);

// Bun's `routes`: a handler, a static Response, or an object of handlers by
// HTTP method. Anything else (an HTML import, `false`) is left as it is.
function closingRoute(route) {
  if (typeof route === "function") return closing(route);
  if (route instanceof Response) {
    return () => {
      try {
        return connectionClose(route.clone());
      } catch {
        return route;
      }
    };
  }
  if (route && typeof route === "object" && Object.keys(route).length > 0 && Object.keys(route).every((k) => HTTP_METHODS.has(k))) {
    const byMethod = {};
    for (const k of Object.keys(route)) byMethod[k] = closingRoute(route[k]);
    return byMethod;
  }
  return route;
}

function closingRoutes(routes) {
  const out = {};
  for (const k of Object.keys(routes)) out[k] = closingRoute(routes[k]);
  return out;
}

function connectionClose(r) {
  if (r instanceof Response) {
    try {
      r.headers.set("connection", "close");
    } catch {}
  }
  return r;
}

// server prototype (one per kind of server: plain, TLS, development) -> its
// own reload()
const nativeReloads = new WeakMap();

function trackBunApp(server, opts) {
  const proto = Object.getPrototypeOf(server);
  if (!proto) return;
  let native = nativeReloads.get(proto);
  if (!native) {
    native = proto.reload;
    if (typeof native !== "function") return; // no reload(): see drainBunApps
    nativeReloads.set(proto, native);
    interceptReload(proto, native);
  }
  bunApps.set(server, { opts, reload: native });
}

// The app's own server.reload(newOptions) still works as before, through
// the shim: its WebSockets stay tracked, a drain swaps in the handler the app
// has now (never an older one), and a reload during a drain keeps draining.
// Other servers' reload() (Bun's node:http) passes straight through.
function interceptReload(proto, native) {
  const desc = Object.getOwnPropertyDescriptor(proto, "reload");
  if (desc && !desc.configurable && !desc.writable) return;
  const reload = {
    reload(options, ...rest) {
      const app = bunApps.get(this);
      if (!app || !options || typeof options !== "object" || typeof options.onNodeHTTPRequest === "function") {
        return Reflect.apply(native, this, [options, ...rest]);
      }
      const o = withShimOptions(options);
      // Bun keeps the handler it has when the new options bring none.
      if (typeof options.fetch !== "function" && typeof app.opts.fetch === "function") o.fetch = app.opts.fetch;
      app.opts = o;
      return Reflect.apply(native, this, [draining && runsAppHandler(o) ? drainingOptions(o) : o, ...rest]);
    },
  }.reload;
  nativeSource.set(reload, native);
  try {
    Object.defineProperty(proto, "reload", { ...(desc ?? { enumerable: false, configurable: true, writable: true }), value: reload });
  } catch {}
}

// The drain: from now on every response of the app's Bun.serve servers says
// `Connection: close`. Requests in flight finish with the handler they
// started with.
function drainBunApps() {
  for (const [server, app] of bunApps) {
    if (!runsAppHandler(app.opts)) continue;
    try {
      Reflect.apply(app.reload, server, [drainingOptions(app.opts)]);
    } catch (e) {
      try {
        process.stderr.write(
          `warden: drain: server.reload() failed (${(e && e.message) || e}), so this worker's responses go out ` +
            `without "Connection: close"; the drain goes on, and a keep-alive client whose connection closes as the ` +
            `worker exits reconnects to another worker. hint: report this with \`bun --version\` ` +
            `(https://github.com/oceanwap/warden/issues)\n`,
        );
      } catch {}
    }
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

// A listen on the app's port, on the app's own address (`appAddress`):
// listen({ port, host }), listen(port[, host][, backlog][, cb]).
function withAddress(args) {
  if (nodeListenPort(args) !== appPort) return args;
  const a0 = args[0];
  if (a0 && typeof a0 === "object") return [{ ...a0, host: appAddress }, ...args.slice(1)];
  const rest = args.slice(1);
  if (typeof rest[0] === "string") rest.shift();
  return [Number(a0), appAddress, ...rest];
}

// An http(s).Server, told apart without loading node:http for a plain TCP
// server: every http.Server has `maxHeadersCount` (null by default).
const isHttpServer = (server) => "maxHeadersCount" in server && typeof server.setTimeout === "function";

// A node:http2 server (createServer or createSecureServer): it gets a private
// health socket too, answered by the same "request" handler.
const isHttp2Server = (server) => typeof server.updateSettings === "function" && typeof server.setTimeout === "function";

// The open connections of the app's node:http servers (and, separately, of
// the private health socket's), per connection: nothing is added per
// request. A drain reads what it needs off the sockets (nodeBusy, below).
function trackConn(sock) {
  nodeConns.add(sock);
  sock.on("close", untrackConn);
}
function untrackConn() {
  nodeConns.delete(this);
}
function trackPrivateConn(sock) {
  privateConns.add(sock);
  sock.on("close", untrackPrivateConn);
}
function untrackPrivateConn() {
  privateConns.delete(this);
}

// A response is in progress on this connection: node:http sets
// `socket._httpMessage` when it hands a request to the app and clears it
// once the response is finished (a pipelined request's response waits
// behind it). An upgraded socket (WebSocket) has none; long-lived handling
// deals with those.
const nodeBusy = (sock) => !!sock._httpMessage;

// An https server's "connection" event hands over the raw TCP socket; the
// HTTP parser and `_httpMessage` live on the TLSSocket that "secureConnection"
// carries, so that is the one to track (on the raw socket nothing is ever
// in flight and a drain would cut every slow request).
const isTlsServer = (server) => typeof server.setSecureContext === "function";

function trackNodeServer(server) {
  if (servers.has(server)) return;
  servers.add(server);
  if (isHttpServer(server)) {
    appUsesNodeHttp = true;
    if (longLived) trackNodeUpgrades(server);
    if (drains) server.on(isTlsServer(server) ? "secureConnection" : "connection", trackConn);
  }
  server.once("listening", () => {
    const addr = server.address();
    if (!addr || typeof addr !== "object") return; // a Unix socket
    const isApp = appPort == null || addr.port === appPort;
    if (!isApp) return;
    if (isHttpServer(server)) appHttpServers.add(server);
    if (!privateServer && (isHttpServer(server) || isHttp2Server(server))) {
      // Taking the hostname router's connections too (takeHandoff).
      const handed = handoffServers.has(server) ? { handoff: true } : {};
      openPrivateNode(server, (socket) => report({ ev: "listening", port: addr.port, ...handed, ...(socket ? { socket } : {}) }));
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
    // Health checks in flight count in a drain like the app's requests.
    if (drains) p.on("connection", trackPrivateConn);
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
      if (appAddress) args = withAddress(args);
      trackNodeServer(this);
      if (standby && !promoted && deferNodeListen(this, args)) return this;
      if (handoff && isHttpServer(this) && takeHandoff(this, args, origListen)) return this;
      if (nodeReusePort) args = withReusePort(args);
    }
    return origListen.apply(this, args);
  };
}

// ---------------------------------------------------------------- handoff
//
// The app's server does not listen on its port: it listens on an ephemeral
// 127.0.0.1 port (so `listening`, address() and the app's callback behave)
// and serves the connections Warden hands over ("warden:connection" with the
// socket attached, Node's own handle passing; Bun speaks it too).

const handoffServers = new Set();
const handedOver = new WeakSet();

function takeHandoff(server, args, origListen) {
  if (appPort == null || nodeListenPort(args) !== appPort) return false;
  const a0 = args[0];
  const host = a0 && typeof a0 === "object" ? a0.host : typeof args[1] === "string" ? args[1] : undefined;
  const cb = args.find((x) => typeof x === "function");
  if (!handoffServers.size) {
    process.on("message", (msg, socket) => {
      // A number is a bare descriptor, for Bun.serve (takeAdoptions).
      if (msg !== "warden:connection" || !socket || typeof socket !== "object") return;
      const target = [...handoffServers].find((s) => s.listening !== false) ?? [...handoffServers][0];
      if (!target) return socket.destroy();
      handedOver.add(socket);
      target.emit("connection", socket);
      socket.resume?.();
    });
  }
  handoffServers.add(server);
  // Bun answers "Connection: close" on a handed-over socket (to an HTTP/1.0
  // request always, keep-alive or not) but keeps it open: end it once the
  // response is out. Node closes it itself.
  if (isBun) {
    server.on("request", (req, res) => {
      if (!handedOver.has(req.socket)) return;
      const close = req.httpVersion === "1.0" || String(req.headers.connection || "").toLowerCase() === "close";
      if (close) res.once("finish", () => req.socket.end());
    });
  }
  // Listening on the port as well: the usual `listening` report says so.
  if (handoffListens) return false;
  server.once("listening", () => {
    appHttpServers.add(server);
    // Its private health socket, as for a server on the port.
    const done = (socket) =>
      report({ ev: "listening", port: appPort, handoff: true, host: host ?? null, ...(socket ? { socket } : {}) });
    if (!privateServer) openPrivateNode(server, done);
    else done(privatePath);
  });
  origListen.call(server, { port: 0, host: "127.0.0.1" }, cb);
  return true;
}

if (isBun && handoff) {
  // Bun's node:http Server has its own listen (it ends in Bun.serve).
  const Server = http().Server;
  const origListen = Server.prototype.listen;
  Server.prototype.listen = function (...args) {
    if (this !== privateServer && takeHandoff(this, args, origListen)) return this;
    return origListen.apply(this, args);
  };
}

if (isBun && appAddress) {
  // Bun's node:http Server has its own listen (it ends in Bun.serve).
  const Server = http().Server;
  const origListen = Server.prototype.listen;
  Server.prototype.listen = function (...args) {
    return origListen.apply(this, this === privateServer ? args : withAddress(args));
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
//   on the private health socket, or else on an ephemeral 127.0.0.1 port
//   nobody is told about: every method works, no traffic reaches it); at
//   promotion the real server takes the stand-in's place.
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

// Where the stand-in serves: the private health socket when Warden asked for
// one (so it can check the standby), else an ephemeral port on 127.0.0.1
// that nobody is told about (no socket path, so no length limit).
function standInAt() {
  if (!privateServer) {
    const path = privateSocketPath();
    if (path) return { path, health: true };
  }
  return { path: null, health: false };
}

function deferBunServe(options, rest) {
  const at = standInAt();
  let standIn;
  try {
    const p = Object.create(options);
    if (at.path) {
      fs.rmSync(at.path, { force: true });
      p.unix = at.path;
      p.port = undefined;
      p.hostname = undefined;
    } else {
      p.port = 0;
      p.hostname = "127.0.0.1";
    }
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
          // Long-lived connections (below) track node:http connections from
          // this `listening`, which comes only once (for the stand-in): its
          // connections then include the real server's after promotion.
          if (longLived) this.on("connection", trackConn);
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
    nodeOrigListen.call(w, { port: 0, host, reusePort: nodeReusePort }, () => w.close());
    w.unref();
  } catch {}
}

function promote(msg) {
  if (promoted) return;
  promoted = true;
  stopReadingWardenCommands();
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

// Commands from Warden on fd 3 (a socket), read without the event loop
// waiting. The pending read keeps a standby alive while nothing listens.
// No commands after promotion.
// - Node: through its event loop (a net.Socket on fd 3). Not fs.read: that
//   blocks a thread-pool thread in read(2), and Node joins those threads when
//   it exits, so a standby's process.exit() (its drain on SIGTERM) waited
//   until Warden's SIGKILL. The socket owns fd 3 from then on and closes it
//   when it ends (Warden shut down or closed its end): `ipcOpen` turns false
//   first, so reports never go to a reused fd 3.
// - Bun: a stream of Bun.file(fd) (its net.Socket can't take an fd), which
//   waits for data in Bun's event loop (fd 3 is non-blocking: fs.read would
//   only get EAGAIN) and leaves fd 3 open when it ends or is cancelled.
//   Cancelled once promoted or draining, so it doesn't hold up the exit.
//   Without Bun.file: fs.read, retried on EAGAIN.
// Warden also shuts down its end when it stops a standby: a pending read
// gets EOF whatever it runs on.
function readWardenCommands() {
  let acc = "";
  const take = (text) => {
    if (promoted) return;
    acc += text;
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
  };
  if (!isBun) {
    try {
      const net = require("node:net");
      const sock = new net.Socket({ fd: ipcFd, readable: true, writable: false });
      // Every way it closes fd 3 (end, error, destroy) goes through destroy().
      const destroy = sock.destroy;
      sock.destroy = function (...a) {
        ipcOpen = false;
        return destroy.apply(this, a);
      };
      sock.setEncoding("utf8");
      sock.on("data", take);
      sock.on("error", () => {});
      ipcReader = sock;
      return;
    } catch {} // not a socket Node can watch: read it the Bun way
  }
  if (isBun && typeof Bun !== "undefined" && typeof Bun.file === "function") {
    try {
      const reader = Bun.file(ipcFd).stream().getReader();
      const decoder = new TextDecoder();
      const stop = () => reader.cancel().catch(() => {});
      ipcReader = { unref: stop };
      (async () => {
        for (;;) {
          const r = await reader.read();
          if (r.done) return; // Warden shut down its end (stopping this standby, or exiting)
          take(decoder.decode(r.value, { stream: true }));
          if (promoted) return stop();
        }
      })().catch(() => {}); // fd 3 is gone: nothing can promote this worker
      return;
    } catch {}
  }
  const buf = Buffer.alloc(4096);
  const next = () =>
    fs.read(ipcFd, buf, 0, buf.length, null, (err, n) => {
      if (err) {
        if (err.code === "EAGAIN" || err.code === "EINTR") setTimeout(next, 10);
        return; // fd 3 is gone: nothing can promote this worker
      }
      if (!n) return; // Warden shut down its end (stopping this standby, or exiting)
      take(buf.toString("utf8", 0, n));
      if (!promoted) next();
    });
  next();
}

// No more commands: the reader no longer keeps the process alive (a
// promoted worker lives as long as its servers; a draining standby exits
// once its drain and the app's handlers are done).
function stopReadingWardenCommands() {
  if (ipcReader) ipcReader.unref();
}

if (standby) readWardenCommands();

// ------------------------------------------------- long-lived connections
//
// WebSockets and SSE streams never finish by themselves: a drain that waits
// for in-flight requests would hold the old worker until Warden's SIGKILL,
// and the client would see a reset (WebSocket 1006, a broken EventSource).
// They may end by themselves for WARDEN_LONG_LIVED_MS after the drain
// starts; then the shim ends the rest the way a server going away should:
//   - WebSockets get a close frame with 1001 (Going Away). Bun.serve: ws.close()
//     on each ServerWebSocket, tracked by wrapping the app's open/close
//     handlers (Bun's node:http `ws` goes through them too). Node: the frame is
//     written to each socket taken over through the server's 'upgrade' event
//     (the `ws` library uses it), and the socket ended after a short grace.
//   - SSE responses (text/event-stream) end after their last complete chunk
//     (the chunked stream terminates; EventSource reconnects by itself).
//     Bun.serve: the body goes through a stream the shim can close, set up by
//     a Response constructor wrapper (see installBunStreamHooks). node:http,
//     on Node and Bun: res.end() on each connection's current response.
// Other streamed responses (downloads, proxied bodies) are left alone: a
// clean end would make a truncated file look complete. They are in-flight
// requests like any other, bounded by grace_period.
// Then up to WS_CLOSE_WAIT_MS for clients to answer the close frames, so the
// sockets end with FIN rather than RST.
//
// Cost: per request, nothing on Node (a Set entry per connection) and one
// call frame per `new Response()` in Bun (JSC inlines it: no measurable
// difference, bench/README.md "shim"); a Set entry per open WebSocket / SSE
// stream, removed when it ends.

const CLOSE_REASON = "server restarting";
const WS_CLOSE_WAIT_MS = 1000;
const WS_END_GRACE_MS = 200;
const noop = () => {};
const bunWs = new Set(); // Bun ServerWebSockets we have not closed
const nodeWs = new Set(); // Node sockets upgraded to WebSocket we have not closed
const wsClosing = new Set(); // closed by us, waiting for the closing handshake
const sseOpen = new Set(); // Bun SSE bodies being served: { end() }
const longLivedClosed = { ws: 0, sse: 0 };
let longLivedAt = 0; // when the deadline passed (0: not yet)

// Server close frame (unmasked): FIN + opcode 8, status 1001, reason.
const WS_CLOSE_FRAME = (() => {
  const reason = Buffer.from(CLOSE_REASON);
  return Buffer.concat([Buffer.from([0x88, 2 + reason.length, 0x03, 0xe9]), reason]);
})();

function settle(fn, arg) {
  try {
    const r = fn(arg);
    if (r && typeof r.catch === "function") r.catch(noop);
  } catch {}
}

// Bun.serve's websocket handlers, with the open/close of every socket seen.
function trackBunWebSockets(handlers) {
  const w = Object.create(handlers);
  const { open, close } = handlers;
  w.open = function (ws) {
    bunWs.add(ws);
    if (typeof open === "function") return open.apply(this, arguments);
  };
  w.close = function (ws) {
    bunWs.delete(ws);
    wsClosing.delete(ws);
    if (typeof close === "function") return close.apply(this, arguments);
  };
  return w;
}

// Node: sockets the app takes over as WebSockets. Our listener is there only
// while the app has one of its own: with none, Node answers an upgrade
// request as a normal request, and an extra listener would change that.
function trackNodeUpgrades(server) {
  let hooked = false;
  const onUpgrade = (req, sock) => {
    if (!/^websocket$/i.test(String((req.headers && req.headers.upgrade) || ""))) return;
    nodeWs.add(sock);
    sock.once("close", () => {
      nodeWs.delete(sock);
      wsClosing.delete(sock);
    });
  };
  const sync = () => {
    const apps = server.listenerCount("upgrade") - (hooked ? 1 : 0);
    if (apps > 0 && !hooked) {
      hooked = true;
      server.prependListener("upgrade", onUpgrade);
    } else if (apps === 0 && hooked) {
      hooked = false;
      server.removeListener("upgrade", onUpgrade);
    }
  };
  server.on("newListener", (ev) => ev === "upgrade" && process.nextTick(sync));
  server.on("removeListener", (ev) => ev === "upgrade" && process.nextTick(sync));
  sync();
}

// Bun's node:http server calls Bun.serve from http.Server#listen and emits
// 'listening' right after. One hooked emit catches that server (to see its
// connections, hence their responses); then emit is put back, so requests
// never pass through the hook. The timer puts it back if listen fails.
let bunNodeServersDue = 0;
function watchBunNodeServer() {
  let proto;
  try {
    proto = http().Server.prototype;
  } catch {
    return;
  }
  if (bunNodeServersDue++ > 0) return;
  const own = Object.prototype.hasOwnProperty.call(proto, "emit");
  const emit = proto.emit;
  const restore = () => {
    bunNodeServersDue = 0;
    if (own) proto.emit = emit;
    else delete proto.emit;
  };
  const t = setTimeout(restore, 2000);
  if (t && typeof t.unref === "function") t.unref();
  proto.emit = function (ev) {
    if (ev === "listening" && bunNodeServersDue > 0) {
      this.on("connection", trackConn);
      if (--bunNodeServersDue === 0) {
        clearTimeout(t);
        restore();
      }
    }
    return emit.apply(this, arguments);
  };
}

// `writeHead(200, { "content-type": … })` alone leaves getHeader() empty in
// Node; the header block it sent (`_header`) has it.
function isEventStreamResponse(res) {
  let type;
  try {
    type = typeof res.getHeader === "function" ? res.getHeader("content-type") : undefined;
  } catch {}
  if (type == null && typeof res._header === "string") {
    const m = /\r\ncontent-type:[ \t]*([^\r\n]*)/i.exec(res._header);
    type = m ? m[1] : undefined;
  }
  return typeof type === "string" && /^\s*text\/event-stream/i.test(type);
}

function isEventStreamInit(init) {
  const h = init != null && typeof init === "object" ? init.headers : undefined;
  if (h == null || typeof h !== "object") return false;
  let type;
  try {
    type = (h instanceof Headers ? h : new Headers(h)).get("content-type");
  } catch {
    return false;
  }
  return typeof type === "string" && /^\s*text\/event-stream/i.test(type);
}

// Closes every long-lived connection still open (each once). Called every
// drain round after the deadline, which also catches ones opened late on a
// kept-alive connection.
function closeLongLived() {
  for (const ws of bunWs) {
    bunWs.delete(ws);
    wsClosing.add(ws); // before close(): its handler may run at once
    try {
      ws.close(1001, CLOSE_REASON);
      longLivedClosed.ws++;
    } catch {
      wsClosing.delete(ws);
    }
  }
  for (const sock of nodeWs) {
    if (!sock.bytesWritten) continue; // the app hasn't answered the handshake yet
    nodeWs.delete(sock);
    if (sock.destroyed || !sock.writable) continue;
    wsClosing.add(sock);
    longLivedClosed.ws++;
    try {
      sock.write(WS_CLOSE_FRAME);
    } catch {}
    // The app's WebSocket code answers the client's close frame and ends the
    // socket itself; if nothing does, end it.
    const t = setTimeout(() => {
      try {
        sock.end();
      } catch {}
    }, WS_END_GRACE_MS);
    if (t && typeof t.unref === "function") t.unref();
  }
  for (const entry of sseOpen) if (entry.end()) longLivedClosed.sse++;
  for (const sock of nodeConns) {
    const res = sock._httpMessage;
    if (!res || res.writableEnded || !isEventStreamResponse(res)) continue;
    try {
      // A handler that writes once more must not crash the worker.
      res.on("error", noop);
      res.end();
      longLivedClosed.sse++;
    } catch {}
  }
}

// One drain round. Returns whether the drain must keep going for them.
function longLivedStep(elapsed) {
  // Before the deadline they may end by themselves. SSE responses are in
  // pending(); WebSockets are not.
  if (elapsed < longLivedMs) return bunWs.size + nodeWs.size > 0;
  if (!longLivedAt) longLivedAt = Date.now();
  closeLongLived();
  // Bun keeps the connection of an ended SSE response alive: close it once
  // idle, so the client's reconnect opens a new one (to a new worker). Node:
  // closeServedIdle() does it.
  if (isBun && longLivedClosed.sse > 0) {
    for (const s of servers) {
      try {
        if (typeof s.closeIdleConnections === "function") s.closeIdleConnections();
      } catch {}
    }
  }
  return wsClosing.size > 0 && Date.now() - longLivedAt < WS_CLOSE_WAIT_MS;
}

function reportLongLived() {
  const { ws, sse } = longLivedClosed;
  if (ws + sse > 0) report({ ev: "long_lived_closed", ws, sse });
}

// Bun.serve SSE bodies. Recognising one costs nothing per request only where
// the body is handed over: in `new Response(body, init)` (reading `.body` of
// a returned Response would change a string body's content-type; reading its
// headers costs ~0.4 µs per request). So Response is wrapped: a stream or
// async-iterable body with `content-type: text/event-stream` goes through a
// stream the shim can end. ReadableStream is wrapped too, only to recognise
// `type: "direct"` streams: reading one through a reader breaks it (Bun 1.3
// calls its pull() again for every read), so those are ended through the
// controller Bun gives their pull() instead. Everything else is constructed
// natively, and both wrappers pass for the native constructors
// (passAsNative), so instanceof, subclasses and statics behave as before.
//
// A function, not a Proxy or a class: a Proxy's construct trap costs ~300 ns
// per `new Response` in Bun 1.3 (this one frame is inlined by JSC: `new
// Response(text)` measured 202-238 ns through it, 199-252 ns native, Bun
// 1.3.13, interleaved runs), and a class's `prototype` can't be the native one.

// wrapper -> the native function whose source text it shows.
const nativeSource = new WeakMap();

// `wrapper` passes for `native`: the same prototype (so instanceof works,
// for fetch()'s responses too, and the instances' `constructor` names the
// wrapper), name, length, statics, a read-only `prototype`, and the native
// source text, also through Function.prototype.toString.call (lodash's
// isNative, core-js). What still tells them apart: it is another function
// object, Bun.inspect shows `[Function: Response]` instead of
// `[class Response]`, and `prototype` comes before the statics in its own
// property names (docs/how-it-works.md, "Limitations").
function passAsNative(wrapper, native) {
  for (const key of Reflect.ownKeys(native)) {
    if (key !== "prototype") Object.defineProperty(wrapper, key, Object.getOwnPropertyDescriptor(native, key));
  }
  Object.defineProperty(wrapper, "prototype", {
    value: native.prototype,
    writable: false,
    enumerable: false,
    configurable: false,
  });
  const ctor = Object.getOwnPropertyDescriptor(native.prototype, "constructor");
  if (ctor && ctor.configurable) Object.defineProperty(native.prototype, "constructor", { ...ctor, value: wrapper });
  if (nativeSource.has(Function.prototype.toString) === false) maskSources();
  nativeSource.set(wrapper, native);
}

// Function.prototype.toString shows a wrapper's native source text, and its
// own. One WeakMap lookup per call; nothing else changes.
function maskSources() {
  const d = Object.getOwnPropertyDescriptor(Function.prototype, "toString");
  if (!d || !d.configurable || typeof d.value !== "function") return;
  const original = d.value;
  // A method: like the native one, it has no `prototype` and can't be `new`ed.
  const toString = {
    toString() {
      return Reflect.apply(original, nativeSource.get(this) ?? this, []);
    },
  }.toString;
  nativeSource.set(toString, original);
  Object.defineProperty(Function.prototype, "toString", { ...d, value: toString });
}

function installBunStreamHooks() {
  const NativeResponse = globalThis.Response;
  const NativeStream = globalThis.ReadableStream;
  if (typeof NativeResponse !== "function" || typeof NativeStream !== "function") return;
  const direct = new WeakMap(); // direct stream -> { sse }
  const ours = new WeakSet(); // streams made by closableBody

  // The app's body behind a stream the shim can end. Chunks pass through one
  // at a time, as Bun asks for them (highWaterMark 0: nothing is buffered).
  // Ending it cancels the app's source, as a client disconnecting would.
  function closableBody(body) {
    if (ours.has(body)) return body;
    const d = direct.get(body);
    if (d) {
      d.sse = true;
      return body;
    }
    let next, cancel;
    if (body instanceof NativeStream) {
      if (body.locked) return body;
      const reader = body.getReader();
      next = () => reader.read();
      cancel = (why) => reader.cancel(why);
    } else {
      const it = body[Symbol.asyncIterator]();
      next = () => it.next();
      cancel = (why) => (typeof it.return === "function" ? it.return(why) : undefined);
    }
    let ctl = null;
    let state = 0; // 0: not served yet, 1: being served (in sseOpen), 2: over
    const over = () => {
      if (state === 1) sseOpen.delete(entry);
      state = 2;
    };
    const entry = {
      end() {
        if (state !== 1) return false;
        over();
        try {
          ctl.close();
        } catch {}
        settle(cancel, CLOSE_REASON);
        return true;
      },
    };
    const out = new NativeStream(
      {
        start(c) {
          ctl = c;
        },
        pull(c) {
          if (state === 0) {
            state = 1;
            sseOpen.add(entry);
          }
          if (state === 2) return;
          let p;
          try {
            p = Promise.resolve(next());
          } catch (e) {
            over();
            c.error(e);
            return;
          }
          return p.then(
            (r) => {
              if (state === 2) return;
              if (!r || r.done) {
                over();
                c.close();
              } else c.enqueue(r.value);
            },
            (e) => {
              if (state === 2) return;
              over();
              c.error(e);
            },
          );
        },
        cancel(why) {
          over();
          settle(cancel, why);
        },
      },
      { highWaterMark: 0 },
    );
    ours.add(out);
    return out;
  }

  function Response(body, init) {
    // Throws the native TypeError.
    if (new.target === undefined) return NativeResponse(body, init);
    if (
      body !== null &&
      typeof body === "object" &&
      (body instanceof NativeStream || typeof body[Symbol.asyncIterator] === "function") &&
      isEventStreamInit(init)
    ) {
      body = closableBody(body);
    }
    return new.target === Response
      ? new NativeResponse(body, init)
      : Reflect.construct(NativeResponse, [body, init], new.target);
  }
  passAsNative(Response, NativeResponse);

  function ReadableStream(source, strategy) {
    // Throws the native TypeError.
    if (new.target === undefined) return NativeStream(source, strategy);
    const target = new.target === ReadableStream ? NativeStream : new.target;
    if (source == null || typeof source !== "object" || source.type !== "direct" || typeof source.pull !== "function") {
      return target === NativeStream ? new NativeStream(source, strategy) : Reflect.construct(NativeStream, [source, strategy], target);
    }
    const info = { sse: false, canceled: false };
    const src = Object.create(source);
    src.pull = function (c) {
      const r = source.pull.apply(source, arguments);
      // A direct stream ends when its (async) pull() settles. Until then it
      // stays here even if the client left (Bun cancels it, but writes then
      // return 0 and the request stays pending while the app's loop runs):
      // closing it at the deadline makes such a loop's next write throw.
      if (info.sse && r && typeof r.then === "function") {
        const entry = {
          end() {
            if (!sseOpen.delete(entry)) return false;
            const open = !info.canceled; // close() makes Bun call cancel()
            try {
              c.close();
            } catch {}
            return open;
          },
        };
        sseOpen.add(entry);
        const off = () => sseOpen.delete(entry);
        r.then(off, off);
      }
      return r;
    };
    src.cancel = function () {
      info.canceled = true;
      if (typeof source.cancel === "function") return source.cancel.apply(source, arguments);
    };
    const s = Reflect.construct(NativeStream, [src, strategy], target);
    direct.set(s, info);
    return s;
  }
  passAsNative(ReadableStream, NativeStream);

  globalThis.Response = Response;
  globalThis.ReadableStream = ReadableStream;
}

if (isBun && longLived) installBunStreamHooks();

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

// ------------------------------------------------------------- responses
//
// The responses of the app's node:http servers on its port, by status, sent
// with each heartbeat as `req` (counts since the worker started) for `warden
// list`, the GUI and the metrics. Node publishes each response's end on its
// own diagnostics channel, `http.server.response.finish`: subscribing wraps
// nothing of the app or of http. Measured on a hello-world server under Node
// 22: 25.89 µs of CPU per request against 25.80 without (within the noise);
// wrapping `emit` instead cost 3.7 %. Not under Bun, whose node:http does not
// publish it (and Bun.serve has no such hook), and not with WARDEN_REQUESTS=0
// (`[metrics] requests = false`). Health checks on the private socket go to
// its own server, which is not counted. No figure is sent before the first
// response: a Node without the channel sends none, rather than zeros.
const appHttpServers = new WeakSet();
const responses = { "2xx": 0, "3xx": 0, "4xx": 0, "404": 0, "5xx": 0 };
let responsesSeen = false;

function onResponse(m) {
  if (!m || !appHttpServers.has(m.server)) return;
  const s = m.response ? m.response.statusCode : 0;
  responsesSeen = true;
  if (s >= 500) responses["5xx"]++;
  else if (s >= 400) {
    responses["4xx"]++;
    if (s === 404) responses["404"]++;
  } else if (s >= 300) responses["3xx"]++;
  else if (s >= 200) responses["2xx"]++;
}

if (!isBun && heartbeatMs > 0 && env.WARDEN_REQUESTS !== "0") {
  try {
    require("node:diagnostics_channel").subscribe("http.server.response.finish", onResponse);
  } catch {}
}

if (heartbeatMs > 0) {
  // Stops arriving when the event loop is blocked: Warden's watchdog notices.
  // Carries the event-loop delay of the interval (next section), and the
  // responses since the worker started (above).
  const t = setInterval(
    () => report(responsesSeen ? { ev: "heartbeat", loop: loopDelay(), req: responses } : { ev: "heartbeat", loop: loopDelay() }),
    heartbeatMs,
  );
  if (t && typeof t.unref === "function") t.unref();
}

// ------------------------------------------------------- event-loop delay
//
// How late the event loop runs: what a request waits before its handler
// starts. Reported with each heartbeat as `loop: {p50, p99, max}` (ms, over
// the heartbeat interval), shown by `warden list` and in the metrics.
//
// perf_hooks.monitorEventLoopDelay is a histogram filled by a native timer
// every LOOP_RESOLUTION_MS (no JavaScript runs per sample, nothing per
// request); the heartbeat reads and resets it once a second. Measured idle
// cost: none in Node, ~0.05% of a core in Bun; 100 ms gives ten samples per
// heartbeat, enough for p50/p99/max (a 20 ms resolution cost 0.2%). A block
// of 100 ms or more always shows (a sample falls due during it); a shorter
// one shows when a sample does, which under steady load is most of them. Node
// records each sample as the timer's whole period, the resolution included;
// Bun records the delay alone. Where the histogram is missing or never
// records (an older Bun), the fallback is the heartbeat timer's own lateness
// (one sample per interval, at no cost at all).
const LOOP_RESOLUTION_MS = 100;
let loopHist = null;
let loopEmpty = 0; // heartbeats in a row that found the histogram empty
let loopLast = 0;

if (heartbeatMs > 0) {
  try {
    const { monitorEventLoopDelay } = require("node:perf_hooks");
    loopHist = monitorEventLoopDelay({ resolution: LOOP_RESOLUTION_MS });
    loopHist.enable();
  } catch {
    loopHist = null;
  }
  loopLast = performance.now();
}

// The event-loop delay since the last heartbeat: { p50, p99, max } in ms.
function loopDelay() {
  const now = performance.now();
  // The heartbeat runs every heartbeatMs: how much later than that it ran.
  const drift = Math.max(0, now - loopLast - heartbeatMs);
  loopLast = now;
  const ms = (ns) => Math.round(Math.max(0, ns - (isBun ? 0 : LOOP_RESOLUTION_MS * 1e6)) / 1e4) / 100;
  if (loopHist) {
    try {
      const h = loopHist;
      // `count` is Node 17+ and Bun; before that, an empty histogram's max is 0.
      if ((typeof h.count === "number" ? h.count : h.max) > 0) {
        loopEmpty = 0;
        // A block longer than the interval delays the heartbeat more than
        // any timer the histogram saw end in it.
        const max = Math.max(ms(h.max), Math.round(drift * 100) / 100);
        const out = { p50: ms(h.percentile(50)), p99: Math.min(ms(h.percentile(99)), max), max };
        h.reset();
        return out;
      }
      // Ten samples were due: three empty intervals in a row mean it doesn't work here.
      if (++loopEmpty >= 3) {
        h.disable();
        loopHist = null;
      }
    } catch {
      loopHist = null;
    }
  }
  const d = Math.round(drift * 100) / 100;
  return { p50: d, p99: d, max: d };
}

// PM2 apps call process.send('ready') (wait_ready) and some call process.send
// unguarded. Without an IPC channel it would be undefined and throw; here it
// reports readiness to Warden instead.
// With the handoff, process.send is Node's IPC channel to Warden's
// dispatcher, which reads no app messages: readiness still goes to fd 3.
if (!inWorker && (typeof process.send !== "function" || handoff)) {
  process.send = function (msg, ...rest) {
    if (msg === "ready" || (msg && typeof msg === "object" && msg.type === "ready")) report({ ev: "ready" });
    const cb = rest.find((x) => typeof x === "function");
    if (cb) queueMicrotask(() => cb(null));
    return true;
  };
}

function pending() {
  if (!isBun) {
    let n = 0;
    for (const s of nodeConns) if (nodeBusy(s)) n++;
    for (const s of privateConns) if (nodeBusy(s)) n++;
    return n;
  }
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
    // Node >= 19: http.Server#close() also closes idle keep-alive
    // connections at once, cutting a request a client is sending on one
    // right then. net.Server#close() only stops accepting.
    else if (isHttpServer(s)) netServerClose(s);
    else s.close();
  } catch {}
}

function netServerClose(s) {
  const net = require("node:net");
  net.Server.prototype.close.call(s);
}

// Node: close keep-alive connections that already served a request and sit
// idle, once the drain window is over (until then a client's next request
// is answered with Connection: close; closing them earlier races with a
// request being sent, which then fails). Not Node's closeIdleConnections():
// it also closes connections just accepted whose first request hasn't been
// parsed yet, and those clients would see an empty reply. New connections
// get their request answered (with Connection: close) and close after it.
// Served: response bytes went out on it; idle: none in progress; still
// HTTP: an upgraded socket (WebSocket) has no parser.
function closeServedIdle() {
  for (const sock of nodeConns) {
    if (sock.bytesWritten > 0 && !nodeBusy(sock) && sock.parser) {
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
  stopReadingWardenCommands();
  report({ ev: "draining" });
  drainBunApps();
  await markNodeResponsesClose();
  for (const s of servers) stopAccepting(s);
  const t0 = Date.now();
  for (;;) {
    const elapsed = Date.now() - t0;
    // Every round: WebSockets aren't in pending(), and at the long-lived
    // deadline this closes WebSockets and ends SSE responses.
    const longLivedBusy = longLived && longLivedStep(elapsed);
    if (elapsed >= drainMs && pending() === 0 && !longLivedBusy) break;
    // After the window: idle keep-alive connections (ended SSE responses'
    // included, so EventSource reconnects to a new worker) go; requests in
    // flight and long-lived ones may still hold the drain.
    if (!isBun && elapsed >= drainMs) closeServedIdle();
    await sleep(20);
  }
  if (longLived) reportLongLived();
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
  // A last try for lines still waiting for room in fd 3 (Warden stopped).
  process.on("exit", () => ipcFlush());
}

if (inWorker && env.WARDEN_ENTRY) {
  await import(env.WARDEN_ENTRY);
}

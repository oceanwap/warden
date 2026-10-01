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
//  6. Ends long-lived connections during a drain (WARDEN_LONG_LIVED_MS after
//     it starts): WebSockets get a 1001 close frame, SSE responses a clean end
//     of the chunked stream, so clients reconnect to the new workers instead of
//     holding the old one until SIGKILL. See "long-lived connections" below.

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
// How long a drain lets WebSockets and SSE streams end by themselves before
// closing them (0 = leave them alone). Only meaningful when there is a drain.
const longLivedMs = Number(env.WARDEN_LONG_LIVED_MS ?? 2000);
const longLived = longLivedMs > 0 && (inWorker || drainMs > 0);
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
    if (longLived && opts.websocket && typeof opts.websocket === "object") o.websocket = trackBunWebSockets(opts.websocket);
    opts = o;
  }
  if (options && typeof options === "object" && typeof options.onNodeHTTPRequest === "function") {
    appUsesNodeHttp = true;
    if (longLived) watchBunNodeServer();
  }
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
    if (longLived) trackNodeUpgrades(server);
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
// Cost: per request, nothing on Node and one call frame per `new Response()`
// in Bun; a Set entry per open WebSocket / SSE stream, removed when it ends.

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
      this.on("connection", (sock) => {
        nodeConns.add(sock);
        sock.once("close", () => nodeConns.delete(sock));
      });
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
// natively, so instanceof, subclasses and statics behave as before.
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
    if (new.target === undefined) throw new TypeError("Class constructor Response cannot be invoked without 'new'");
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
  Response.prototype = NativeResponse.prototype;
  Object.setPrototypeOf(Response, NativeResponse);

  function ReadableStream(source, strategy) {
    if (new.target === undefined) {
      throw new TypeError("Class constructor ReadableStream cannot be invoked without 'new'");
    }
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
  ReadableStream.prototype = NativeStream.prototype;
  Object.setPrototypeOf(ReadableStream, NativeStream);

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
  for (;;) {
    const elapsed = Date.now() - t0;
    // Every round: WebSockets aren't in pending(), and at the long-lived
    // deadline this closes WebSockets and ends SSE responses.
    const longLivedBusy = longLived && longLivedStep(elapsed);
    if (elapsed >= drainMs && pending() === 0 && !longLivedBusy) break;
    if (!isBun) closeServedIdle();
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
}

if (inWorker && env.WARDEN_ENTRY) {
  await import(env.WARDEN_ENTRY);
}

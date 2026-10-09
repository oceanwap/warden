// Test fixture for the integration tests. Behaviour is driven by env vars:
//   FIXTURE_EXIT=<code>        exit immediately with <code> (crash loop)
//   FIXTURE_EXIT_AFTER=<ms>    exit 1 after <ms> (dies during a canary soak)
//   FIXTURE_IGNORE_TERM=1      ignore SIGTERM (forces Warden's SIGKILL path)
//   FIXTURE_NO_LISTEN=1        never listen (readiness timeout)
//   FIXTURE_HEALTH_FAIL=1      /health answers 503 (a bad deploy)
//   FIXTURE_THROW_AFTER=<ms>   uncaught error after <ms> (kills one Worker in worker mode)
//   FIXTURE_EXTRA_SERVER=1     start an unrelated server (random port, 404 everywhere)
//                              first, and the app server 400 ms later
//   FIXTURE_SPAM=1             write stdout lines as fast as possible (stalled-log tests)
//   FIXTURE_IPC_FLOOD=1        write messages and junk to fd 3 as fast as possible
//   FIXTURE_STANDBY_EXIT=<ms>  exit 4 after <ms> if still a hot standby (standby crash loop)
//   FIXTURE_BLOCK_MS=<ms>      block the event loop for <ms> every 400 ms (event-loop delay);
//                              FIXTURE_BLOCK_WORKER=<n>: only in worker <n>, or Worker <n> in worker mode
// Endpoints:
//   /whoami  pid:thread      /health  200 or 503
//   /throw   uncaught error  /exit    exit 3
//   /hang    block the event loop forever (watchdog)
//   /sick    this worker's /health starts answering 503
//   /leak    allocate ~200 MB and keep it (max_memory)
//   /reload-v2  the app swaps its own handler (server.reload): later answers
//               start with "v2 "
//   /unlisten   close the app's server and stay alive (port_lost)
//   /relisten?ms=N  close it, and listen again after N ms (a server restarting itself)
//   /fill-ipc   fill fd 3 with heartbeats until it is full (hours of them, Warden
//               stopped); answers the bytes written
import { threadId } from "node:worker_threads";

if (process.env.FIXTURE_EXIT) process.exit(Number(process.env.FIXTURE_EXIT));
if (process.env.FIXTURE_EXIT_AFTER) setTimeout(() => process.exit(1), Number(process.env.FIXTURE_EXIT_AFTER));
if (process.env.FIXTURE_THROW_AFTER) {
  setTimeout(() => { throw new Error("fixture: delayed crash"); }, Number(process.env.FIXTURE_THROW_AFTER));
}
if (process.env.FIXTURE_IGNORE_TERM) process.on("SIGTERM", () => console.log("ignoring SIGTERM"));
if (process.env.FIXTURE_STANDBY_EXIT) {
  setTimeout(() => {
    if (process.env.WARDEN_STANDBY !== "1") return;
    console.log("fixture: standby exiting");
    process.exit(4);
  }, Number(process.env.FIXTURE_STANDBY_EXIT));
}

const who = `${process.pid}:${threadId}`;

const blockHere = !process.env.FIXTURE_BLOCK_WORKER || process.env.FIXTURE_BLOCK_WORKER === process.env.WARDEN_WORKER_ID;
if (process.env.FIXTURE_BLOCK_MS && blockHere) {
  const ms = Number(process.env.FIXTURE_BLOCK_MS);
  setInterval(() => {
    const t = Date.now();
    while (Date.now() - t < ms);
  }, 400);
}

if (process.env.FIXTURE_SPAM) {
  const line = `spam ${"x".repeat(200)}\n`.repeat(100);
  const spam = () => { process.stdout.write(line); setImmediate(spam); };
  spam();
}
if (process.env.FIXTURE_IPC_FLOOD) {
  const fs = require("node:fs");
  const msgs = '{"ev":"heartbeat"}\n'.repeat(200) + "not json\n" + "z".repeat(100_000);
  const flood = () => {
    try { fs.writeSync(3, msgs); } catch {}
    setImmediate(flood);
  };
  setTimeout(flood, 200);
}
// What hours of heartbeats do to fd 3 while Warden reads nothing, at once.
// Stops at the first write that does not go through (a full socket).
function fillIpc() {
  const fs = require("node:fs");
  const line = JSON.stringify({ ev: "heartbeat", worker: Number(process.env.WARDEN_WORKER_ID || 0) }) + "\n";
  let n = 0;
  try {
    while (n < 256 << 20) n += fs.writeSync(3, line);
  } catch {}
  return n;
}
let sick = process.env.FIXTURE_HEALTH_FAIL === "1";
const hoard: Uint8Array[] = [];

let server: ReturnType<typeof Bun.serve>;

function handler(tag: string) {
  return function fetch(req: Request) {
    const path = new URL(req.url).pathname;
    switch (path) {
      case "/reload-v2":
        server.reload({ fetch: handler("v2 ") });
        break;
      case "/health":
        return new Response(sick ? "sick" : "ok", { status: sick ? 503 : 200 });
      case "/fill-ipc":
        return new Response(String(fillIpc()));
      case "/throw":
        setTimeout(() => { throw new Error(`fixture crash ${who}`); });
        break;
      case "/exit":
        setTimeout(() => process.exit(3));
        break;
      case "/hang":
        setTimeout(() => { for (;;) {} });
        break;
      case "/sick":
        sick = true;
        break;
      case "/say":
        console.log(`fixture says ${new URL(req.url).searchParams.get("w")} ${who}`);
        break;
      case "/leak":
        for (let i = 0; i < 20; i++) hoard.push(new Uint8Array(10 * 1024 * 1024).fill(1));
        break;
      case "/unlisten":
      case "/relisten": {
        const back = path === "/relisten" ? Number(new URL(req.url).searchParams.get("ms") ?? 0) : 0;
        setInterval(() => {}, 1 << 30);
        setTimeout(() => {
          server.stop(true);
          console.log(`fixture: stopped listening ${who}`);
          if (back > 0) {
            setTimeout(() => {
              serveApp();
              console.log(`fixture: listening again ${who}`);
            }, back);
          }
        }, 50);
        break;
      }
    }
    return new Response(tag + who);
  };
}

function serveApp() {
  server = Bun.serve({
    port: Number(process.env.PORT),
    reusePort: true,
    fetch: handler(""),
  });
}

if (process.env.FIXTURE_NO_LISTEN) {
  setInterval(() => {}, 1 << 30);
} else if (process.env.FIXTURE_EXTRA_SERVER) {
  Bun.serve({ port: 0, fetch: () => new Response("not the app", { status: 404 }) });
  setTimeout(serveApp, 400);
} else {
  serveApp();
}

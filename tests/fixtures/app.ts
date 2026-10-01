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
// Endpoints:
//   /whoami  pid:thread      /health  200 or 503
//   /throw   uncaught error  /exit    exit 3
//   /hang    block the event loop forever (watchdog)
//   /sick    this worker's /health starts answering 503
//   /leak    allocate ~200 MB and keep it (max_memory)
//   /reload-v2  the app swaps its own handler (server.reload): later answers
//               start with "v2 "
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
      case "/leak":
        for (let i = 0; i < 20; i++) hoard.push(new Uint8Array(10 * 1024 * 1024).fill(1));
        break;
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

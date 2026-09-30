// Test fixture for the integration tests. Behaviour is driven by env vars:
//   FIXTURE_EXIT=<code>        exit immediately with <code> (crash loop)
//   FIXTURE_EXIT_AFTER=<ms>    exit 1 after <ms> (dies during a canary soak)
//   FIXTURE_IGNORE_TERM=1      ignore SIGTERM (forces Warden's SIGKILL path)
//   FIXTURE_NO_LISTEN=1        never listen (readiness timeout)
//   FIXTURE_HEALTH_FAIL=1      /health answers 503 (a bad deploy)
//   FIXTURE_THROW_AFTER=<ms>   uncaught error after <ms> (kills one Worker in worker mode)
//   FIXTURE_EXTRA_SERVER=1     start an unrelated server (random port, 404 everywhere)
//                              first, and the app server 400 ms later
// Endpoints:
//   /whoami  pid:thread      /health  200 or 503
//   /throw   uncaught error  /exit    exit 3
//   /hang    block the event loop forever (watchdog)
//   /sick    this worker's /health starts answering 503
//   /leak    allocate ~200 MB and keep it (max_memory)
import { threadId } from "node:worker_threads";

if (process.env.FIXTURE_EXIT) process.exit(Number(process.env.FIXTURE_EXIT));
if (process.env.FIXTURE_EXIT_AFTER) setTimeout(() => process.exit(1), Number(process.env.FIXTURE_EXIT_AFTER));
if (process.env.FIXTURE_THROW_AFTER) {
  setTimeout(() => { throw new Error("fixture: delayed crash"); }, Number(process.env.FIXTURE_THROW_AFTER));
}
if (process.env.FIXTURE_IGNORE_TERM) process.on("SIGTERM", () => console.log("ignoring SIGTERM"));

const who = `${process.pid}:${threadId}`;
let sick = process.env.FIXTURE_HEALTH_FAIL === "1";
const hoard: Uint8Array[] = [];

function serveApp() {
  Bun.serve({
    port: Number(process.env.PORT),
    reusePort: true,
    fetch(req) {
      const path = new URL(req.url).pathname;
      switch (path) {
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
      return new Response(who);
    },
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

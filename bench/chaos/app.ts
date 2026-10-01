// The Bun app of the chaos soak (`cargo xtask chaos`, docs/chaos.md), in
// process and worker mode.
//   /whoami      "<pid>:<threadId>"
//   /health      200; 503 with CHAOS_HEALTH_FAIL=1 (a release that fails its gates)
//   /flood?mb=N  writes N MB of log lines to stdout in the background, in
//                64 KB slices so the event loop (and its heartbeat) keeps running
//   /throw       an uncaught error (in worker mode: that Worker dies)
// CHAOS_CRASH_FILE=<path>: exit 1 at start while that file exists (a crash
// loop on demand: the harness creates the file, kills the worker, removes it).
import { existsSync } from "node:fs";
import { threadId } from "node:worker_threads";

const crashFile = process.env.CHAOS_CRASH_FILE;
if (crashFile && existsSync(crashFile)) {
  console.error(`chaos: ${crashFile} exists, exiting 1`);
  process.exit(1);
}

const who = `${process.pid}:${threadId}`;
const sick = process.env.CHAOS_HEALTH_FAIL === "1";
const line = `flood ${who} ${"x".repeat(80)}\n`;
const slice = line.repeat(Math.floor(65536 / line.length));
let flooding = false;

async function flood(bytes: number) {
  if (flooding) return;
  flooding = true;
  try {
    for (let sent = 0; sent < bytes; sent += slice.length) {
      if (!process.stdout.write(slice)) {
        await new Promise((r) => process.stdout.once("drain", r));
      } else if ((sent / slice.length) % 8 === 7) {
        await new Promise((r) => setTimeout(r, 0));
      }
    }
  } finally {
    flooding = false;
  }
}

Bun.serve({
  port: Number(process.env.PORT),
  reusePort: true,
  fetch(req) {
    const url = new URL(req.url);
    switch (url.pathname) {
      case "/health":
        return new Response(sick ? "sick" : "ok", { status: sick ? 503 : 200 });
      case "/flood":
        flood(Number(url.searchParams.get("mb") || 10) * 1024 * 1024);
        break;
      case "/throw":
        setTimeout(() => {
          throw new Error(`chaos: uncaught error in ${who}`);
        });
        break;
    }
    return new Response(who);
  },
});

import { threadId, isMainThread } from "node:worker_threads";
function busy(ms: number) { const e = performance.now() + ms; let x = 0; while (performance.now() < e) x++; return x; }
const server = Bun.serve({
  port: Number(process.env.PORT ?? 3200), reusePort: true,
  fetch(req) {
    const u = new URL(req.url);
    if (u.pathname === "/cpu") busy(20);
    if (u.pathname === "/throw") setTimeout(() => { throw new Error("boom in worker " + threadId); }, 0);
    if (u.pathname === "/exit") setTimeout(() => process.exit(3), 0);
    return new Response("t" + threadId);
  },
});
console.log(`thread=${threadId} main=${isMainThread} pid=${process.pid} listening`);

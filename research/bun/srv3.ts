const id = process.env.WID!;
let draining = false;
const server = Bun.serve({
  port: 3100, reusePort: true,
  async fetch(req) {
    await Bun.sleep(50);
    return new Response(id, draining ? { headers: { connection: "close" } } : undefined);
  },
});
process.on("SIGTERM", async () => {
  draining = true;
  const t = Date.now();
  server.stop(false);                       // close listener; keep existing connections
  while (server.pendingRequests > 0 || Date.now() - t < Number(process.env.DRAIN_MS ?? 1000)) await Bun.sleep(20);
  console.error(`wid=${id} drained pending=${server.pendingRequests} after ${Date.now() - t}ms`);
  process.exit(0);
});

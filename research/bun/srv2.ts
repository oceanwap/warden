const id = process.env.WID ?? String(process.pid);
const server = Bun.serve({
  port: Number(process.env.PORT ?? 3100), reusePort: true,
  async fetch(req) { if (process.env.SLOW) await Bun.sleep(Number(process.env.SLOW)); return new Response(id); },
});
if (process.env.GRACEFUL) {
  process.on("SIGTERM", async () => {
    const t = Date.now();
    await server.stop(false); // stop accepting, wait for in-flight
    console.error(`wid=${id} drained in ${Date.now()-t}ms, exiting`);
    process.exit(0);
  });
}

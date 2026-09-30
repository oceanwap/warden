const id = process.env.WID ?? String(process.pid);
const server = Bun.serve({
  port: Number(process.env.PORT ?? 3100),
  reusePort: process.env.REUSE !== "0",
  fetch(req) { return new Response(id); },
});
console.log(`wid=${id} pid=${process.pid} listening ${server.port}`);

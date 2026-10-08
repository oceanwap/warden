// The app of bench/routing.ts: Bun.serve with its own TLS and HTTP/2
// (`tls`), or plain HTTP behind nginx's proxy (`plain`).
//   GET /     12 bytes
//   GET /big  64 KB
const small = "hello world\n";
const big = "x".repeat(65536);
const tls = process.argv[2] !== "plain";
Bun.serve({
  port: Number(process.env.PORT),
  reusePort: true,
  ...(tls ? { http2: true, tls: { cert: Bun.file(process.env.BENCH_CERT!), key: Bun.file(process.env.BENCH_KEY!) } } : {}),
  fetch: (req) => new Response(new URL(req.url).pathname === "/big" ? big : small),
});

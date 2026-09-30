// Minimal benchmark app (PRD §20). Bun.serve with reusePort, so it runs
// unchanged as N processes or as N Workers.
//   GET /plaintext  HTTP overhead
//   GET /json       serialization
//   GET /cpu        fixed CPU work (worker parallelism)
//   GET /health     supervision
//   GET /whoami     which worker answered (pid:thread)
import { threadId } from "node:worker_threads";

const who = `${process.pid}:${threadId}`;
const payload = { message: "Hello, World!", items: Array.from({ length: 20 }, (_, i) => ({ id: i, name: `item-${i}`, tags: ["a", "b", "c"] })) };

function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

const server = Bun.serve({
  port: Number(process.env.PORT ?? 3000),
  reusePort: true,
  fetch(req) {
    const path = new URL(req.url).pathname;
    switch (path) {
      case "/plaintext":
        return new Response("Hello, World!");
      case "/json":
        return Response.json(payload);
      case "/cpu":
        return new Response(String(fib(27)));
      case "/health":
        return new Response("ok");
      case "/whoami":
        return new Response(who);
      default:
        return new Response("not found", { status: 404 });
    }
  },
});

console.log(`bench app listening on :${server.port} (${who})`);

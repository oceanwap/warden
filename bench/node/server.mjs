// Benchmark app for Node (node:http), same endpoints as bench/app/server.ts.
// Listens with reusePort (Node 22.12+) so N copies share the port when run
// bare; under PM2 cluster mode the master owns the port instead.
//   GET /plaintext /json /cpu /health /whoami, and /crash (exits this
//   worker: process in process mode, thread in thread mode)
import http from "node:http";
import { threadId } from "node:worker_threads";

const who = `${process.pid}:${threadId}`;
const payload = JSON.stringify({
  message: "Hello, World!",
  items: Array.from({ length: 20 }, (_, i) => ({ id: i, name: `item-${i}`, tags: ["a", "b", "c"] })),
});

function fib(n) {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

const server = http.createServer((req, res) => {
  switch (req.url) {
    case "/plaintext":
      res.writeHead(200, { "content-type": "text/plain" });
      return res.end("Hello, World!");
    case "/json":
      res.writeHead(200, { "content-type": "application/json" });
      return res.end(payload);
    case "/cpu":
      return res.end(String(fib(27)));
    case "/health":
      return res.end("ok");
    case "/whoami":
      return res.end(who);
    case "/crash":
      res.end("bye");
      setTimeout(() => process.exit(1), 10);
      return;
    default:
      res.writeHead(404);
      return res.end("not found");
  }
});

const port = Number(process.env.PORT ?? 3000);
const opts = process.env.BENCH_NO_REUSEPORT ? { port } : { port, reusePort: true };
server.listen(opts, () => console.log(`node bench app listening on :${port} (${who})`));

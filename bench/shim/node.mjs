// What Warden's shim adds to one node:http request, in-process (run by
// bench/shim-cost.ts with and without `--import <shim>`). A real http.Server
// whose listen() goes through the shim when it is preloaded, and the events
// node:http produces per request: the server's 'request', then the
// response's 'close'. Prints the median ns per request of 7 runs.
import http from "node:http";
import net from "node:net";

let sink = 0;
const server = http.createServer((req, res) => {
  sink ^= req.url.length;
});
await new Promise((r) => server.listen({ port: 0, host: "127.0.0.1" }, r));
// One real connection, so req.socket is a socket the shim has seen.
const client = net.connect(server.address().port, "127.0.0.1");
const conn = await new Promise((r) => server.once("connection", r));
await new Promise((r) => setTimeout(r, 50));

function request() {
  const req = new http.IncomingMessage(conn);
  req.url = "/plaintext";
  const res = new http.ServerResponse(req);
  server.emit("request", req, res);
  res.emit("close");
}

const N = Number(process.env.N ?? 1_000_000);
for (let i = 0; i < 200_000; i++) request(); // JIT warm-up
const runs = [];
for (let k = 0; k < 7; k++) {
  const t0 = process.hrtime.bigint();
  for (let i = 0; i < N; i++) request();
  runs.push(Number(process.hrtime.bigint() - t0) / N);
}
runs.sort((a, b) => a - b);
console.log(JSON.stringify({ request_ns: +runs[3].toFixed(1) }));
client.destroy();
process.exit(sink === -1 ? 1 : 0);

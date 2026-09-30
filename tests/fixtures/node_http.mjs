// node:http server that does NOT ask for reusePort (like Express/NestJS).
// Under Bun, a second copy fails with EADDRINUSE unless Warden's shim is loaded.
import http from "node:http";

http
  .createServer((req, res) => res.end(String(process.pid)))
  .listen({ port: Number(process.env.PORT), host: "127.0.0.1" });

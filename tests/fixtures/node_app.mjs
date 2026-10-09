// Node (not Bun) test app written the way PM2 apps are.
//   FIXTURE_WAIT_READY=<ms>   call process.send('ready') after <ms> (PM2 wait_ready)
//   FIXTURE_SIGINT_ONLY=1     graceful stop on SIGINT only (PM2's default signal);
//                             SIGTERM is ignored
//   FIXTURE_BLOCK_MS=<ms>     block the event loop for <ms> every 400 ms (event-loop delay)
//   FIXTURE_TLS_DIR=<dir>     serve https with <dir>/key.pem and <dir>/cert.pem
// Endpoints: /whoami -> "<pid>:<NODE_APP_INSTANCE>", /slow?ms=N -> answers after N ms,
//            /say?w=X -> prints "fixture says X <pid>" to stdout, answers like /whoami,
//            /status?code=N -> answers with status N,
//            /fill-ipc -> fills fd 3 with heartbeats until it is full (hours of them,
//                         Warden stopped); answers the bytes written
import fs from "node:fs";
import http from "node:http";
import https from "node:https";

const who = () => `${process.pid}:${process.env.NODE_APP_INSTANCE}`;
const handler = (req, res) => {
  const url = new URL(req.url, "http://x");
  if (url.pathname === "/status") {
    res.statusCode = Number(url.searchParams.get("code") || 200);
    res.end(who());
    return;
  }
  if (url.pathname === "/fill-ipc") {
    const line = JSON.stringify({ ev: "heartbeat", worker: Number(process.env.WARDEN_WORKER_ID || 0) }) + "\n";
    let n = 0;
    try {
      while (n < 256 << 20) n += fs.writeSync(3, line);
    } catch {}
    res.end(String(n));
    return;
  }
  if (url.pathname === "/say") {
    console.log(`fixture says ${url.searchParams.get("w")} ${process.pid}`);
    res.end(who());
    return;
  }
  if (url.pathname === "/slow") {
    setTimeout(() => res.end(who()), Number(url.searchParams.get("ms") || 500));
    return;
  }
  res.end(who());
};
const dir = process.env.FIXTURE_TLS_DIR;
const server = dir
  ? https.createServer({ key: fs.readFileSync(`${dir}/key.pem`), cert: fs.readFileSync(`${dir}/cert.pem`) }, handler)
  : http.createServer(handler);
server.listen(Number(process.env.PORT), "127.0.0.1");

if (process.env.FIXTURE_WAIT_READY) {
  setTimeout(() => process.send("ready"), Number(process.env.FIXTURE_WAIT_READY));
}
if (process.env.FIXTURE_BLOCK_MS) {
  const ms = Number(process.env.FIXTURE_BLOCK_MS);
  setInterval(() => {
    const t = Date.now();
    while (Date.now() - t < ms);
  }, 400);
}
if (process.env.FIXTURE_SIGINT_ONLY) {
  process.on("SIGTERM", () => console.log("ignoring SIGTERM"));
  process.on("SIGINT", () => {
    console.log("got SIGINT, closing");
    server.close(() => process.exit(0));
  });
}

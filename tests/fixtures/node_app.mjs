// Node (not Bun) test app written the way PM2 apps are.
//   FIXTURE_WAIT_READY=<ms>   call process.send('ready') after <ms> (PM2 wait_ready)
//   FIXTURE_SIGINT_ONLY=1     graceful stop on SIGINT only (PM2's default signal);
//                             SIGTERM is ignored
//   FIXTURE_BLOCK_MS=<ms>     block the event loop for <ms> every 400 ms (event-loop delay)
// Endpoints: /whoami -> "<pid>:<NODE_APP_INSTANCE>", /slow?ms=N -> answers after N ms
import http from "node:http";

const who = () => `${process.pid}:${process.env.NODE_APP_INSTANCE}`;
const server = http.createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  if (url.pathname === "/slow") {
    setTimeout(() => res.end(who()), Number(url.searchParams.get("ms") || 500));
    return;
  }
  res.end(who());
});
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

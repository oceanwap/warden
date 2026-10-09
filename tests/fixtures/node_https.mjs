// Test fixture: a node:https app that serves TLS itself, with the
// self-signed test certificate in fixtures/tls (localhost, test use only).
//   FIXTURE_TICKET_KEYS=1   the app sets its own `ticketKeys`
import fs from "node:fs";
import https from "node:https";

const dir = new URL("./tls/", import.meta.url).pathname;
const tls = { cert: fs.readFileSync(dir + "cert.pem"), key: fs.readFileSync(dir + "key.pem") };
if (process.env.FIXTURE_TICKET_KEYS === "1") tls.ticketKeys = Buffer.alloc(48, 7);
https
  .createServer(tls, (req, res) => {
    res.end(req.url === "/health" ? "ok\n" : `${process.pid}\n`);
  })
  .listen(Number(process.env.PORT));

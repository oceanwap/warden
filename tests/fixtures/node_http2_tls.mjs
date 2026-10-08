// Test fixture: a node:http2 app that serves TLS itself (createSecureServer),
// with the self-signed test certificate in fixtures/tls (localhost, test use only).
import fs from "node:fs";
import http2 from "node:http2";

const dir = new URL("./tls/", import.meta.url).pathname;
const tls = { cert: fs.readFileSync(dir + "cert.pem"), key: fs.readFileSync(dir + "key.pem") };
http2
  .createSecureServer(tls, (req, res) => {
    res.end(req.url === "/health" ? "ok\n" : `${process.pid}\n`);
  })
  .listen(Number(process.env.PORT));

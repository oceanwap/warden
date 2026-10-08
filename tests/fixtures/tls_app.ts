// Test fixture: an app that serves TLS itself (Bun.serve `tls`),
// with the self-signed test certificate in fixtures/tls (localhost, test use only).
const dir = new URL("./tls/", import.meta.url).pathname;
Bun.serve({
  port: Number(process.env.PORT),
  tls: { cert: Bun.file(dir + "cert.pem"), key: Bun.file(dir + "key.pem") },
  fetch(req) {
    const path = new URL(req.url).pathname;
    if (path === "/health") return new Response("ok\n");
    return new Response(`${process.pid}\n`);
  },
});

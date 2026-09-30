// Node client: keepalive agent, no automatic retries. C loops for D ms.
import http from "node:http";
const [C, D, port] = [Number(process.argv[2]), Number(process.argv[3]), Number(process.argv[4] ?? 3100)];
const agent = new http.Agent({ keepAlive: true, maxSockets: C });
const end = Date.now() + D; let ok = 0, fail = 0; const errs = {};
const get = () => new Promise((res) => {
  const req = http.get({ host: "127.0.0.1", port, path: "/", agent }, (r) => { r.resume(); r.on("end", () => { ok++; res(); }); r.on("error", (e) => { fail++; errs[e.code]=(errs[e.code]??0)+1; res(); }); });
  req.on("error", (e) => { fail++; errs[e.code] = (errs[e.code] ?? 0) + 1; res(); });
});
await Promise.all(Array.from({ length: C }, async () => { while (Date.now() < end) await get(); }));
console.log(JSON.stringify({ ok, fail, errs }));

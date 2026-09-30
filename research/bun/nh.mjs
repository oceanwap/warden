import http from "node:http";
import { threadId } from "node:worker_threads";
const opts = { port: Number(process.env.PORT), host: "0.0.0.0" };
if (process.env.REUSE === "1") opts.reusePort = true;
const s = http.createServer((q, r) => r.end(`${process.pid}/t${threadId}`));
s.on("error", (e) => { console.log(`listen error ${e.code}`); process.exit(1); });
s.listen(opts, () => console.log(`listening pid=${process.pid} t=${threadId}`));

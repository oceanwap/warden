import { threadId } from "node:worker_threads";
const servers: any[] = [];
const s = Bun.serve({ port: 3210, reusePort: true, fetch(req) {
  if (new URL(req.url).pathname === "/throw") setTimeout(() => { throw new Error("boom"); });
  return new Response(`t${threadId}:${process.env.WARDEN_WORKER_ID}`); } });
servers.push(s);
process.on("exit", (c) => { for (const x of servers) x.stop(true); });

import { threadId } from "node:worker_threads";
const server = Bun.serve({ port: 3201, reusePort: true, fetch() { return new Response("t" + threadId); } });
self.onmessage = async (e: MessageEvent) => {
  if (e.data === "stop-then-exit") { await server.stop(true); process.exit(0); }
};

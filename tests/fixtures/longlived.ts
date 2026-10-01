// Bun test app with long-lived connections (Bun.serve; process or worker mode).
// Endpoints:
//   /whoami          "<pid>:<threadId>"
//   /slow?ms=N       answers after N ms (a normal request in flight)
//   /download?ms=N   a streamed text/plain body that ends by itself after N ms
//                    (10 lines, then "end"): not long-lived, never cut short
//   /sse             text/event-stream, a ReadableStream: "id: n\ndata: <who> n\n\n" every 100 ms
//   /sse-direct      the same from a Bun `type: "direct"` stream
//   /sse-gen         the same from an async generator
//   /ws              WebSocket: sends "<who>" on open, echoes text messages
import { threadId } from "node:worker_threads";

const who = `${process.pid}:${threadId}`;
const event = (n: number) => `id: ${n}\ndata: ${who} ${n}\n\n`;
const sseHeaders = { "content-type": "text/event-stream", "cache-control": "no-store" };

function sse(kind: string): Response {
  let n = 0;
  if (kind === "direct") {
    let gone = false;
    const body = new ReadableStream({
      type: "direct",
      async pull(c: any) {
        try {
          while (!gone) {
            c.write(event(n++));
            await c.flush();
            await Bun.sleep(100);
          }
        } catch {
          // the server ended the response
        }
      },
      cancel() {
        gone = true; // the client left
      },
    } as any);
    return new Response(body, { headers: sseHeaders });
  }
  if (kind === "gen") {
    const gen = (async function* () {
      for (;;) {
        yield event(n++);
        await Bun.sleep(100);
      }
    })();
    return new Response(gen as any, { headers: new Headers(sseHeaders) });
  }
  let timer: ReturnType<typeof setInterval>;
  const body = new ReadableStream({
    start(c) {
      c.enqueue(event(n++));
      timer = setInterval(() => c.enqueue(event(n++)), 100);
    },
    cancel() {
      clearInterval(timer);
    },
  });
  return new Response(body, { headers: sseHeaders });
}

Bun.serve({
  port: Number(process.env.PORT),
  reusePort: true,
  async fetch(req, server) {
    const url = new URL(req.url);
    switch (url.pathname) {
      case "/ws":
        if (server.upgrade(req)) return;
        return new Response("expected a WebSocket upgrade", { status: 400 });
      case "/sse":
        return sse("default");
      case "/sse-direct":
        return sse("direct");
      case "/sse-gen":
        return sse("gen");
      case "/slow":
        await Bun.sleep(Number(url.searchParams.get("ms") || 500));
        return new Response(who);
      case "/download": {
        const ms = Number(url.searchParams.get("ms") || 1000);
        let i = 0;
        const body = new ReadableStream({
          async pull(c) {
            if (i === 10) {
              c.enqueue("end\n");
              c.close();
              return;
            }
            await Bun.sleep(ms / 10);
            c.enqueue(`line ${i++}\n`);
          },
        });
        return new Response(body, { headers: { "content-type": "text/plain" } });
      }
      default:
        return new Response(who);
    }
  },
  websocket: {
    open(ws) {
      ws.send(who);
    },
    message(ws, msg) {
      ws.send(msg);
    },
  },
});

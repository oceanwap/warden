// A Bun app that uses `routes` (functions, a static Response, per-method
// handlers) and an `error` handler instead of one `fetch`: during a drain
// every one of them must answer with `Connection: close` too.
Bun.serve({
  port: Number(process.env.PORT),
  routes: {
    "/r": () => new Response("route"),
    "/api/:id": (req: Request & { params: { id: string } }) => new Response(`id ${req.params.id}`),
    "/static": new Response("static"),
    "/m": { GET: () => new Response("get"), POST: () => new Response("post") },
    "/async": async () => {
      await Bun.sleep(5);
      return new Response("async");
    },
    "/throw": () => {
      throw new Error("boom");
    },
  },
  fetch: () => new Response("fallback", { status: 404 }),
  error: () => new Response("handled", { status: 500 }),
});

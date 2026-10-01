// Release-pinning fixture: the tests copy it into each release directory
// (releases/v1, releases/v2…) behind a `current` symlink.
//   /where   "<cwd>|<this file's path>": which release this worker runs
//   /health  200
Bun.serve({
  port: Number(process.env.PORT),
  reusePort: true,
  fetch(req) {
    const path = new URL(req.url).pathname;
    if (path === "/health") return new Response("ok");
    return new Response(`${process.cwd()}|${import.meta.path}`);
  },
});

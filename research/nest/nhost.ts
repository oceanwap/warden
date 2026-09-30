// host: N workers, each patches Bun.serve then imports the Nest app
const N = Number(process.env.N ?? 2);
for (let i = 0; i < N; i++) {
  const w = new Worker(new URL("./nworker.ts", import.meta.url).href);
  w.addEventListener("error", (e) => console.log("worker error", e.message));
}
setInterval(() => {}, 1 << 30);

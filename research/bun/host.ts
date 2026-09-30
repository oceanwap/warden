const N = Number(process.env.N ?? 4);
for (let i = 0; i < N; i++) {
  const w = new Worker(new URL("./wsrv.ts", import.meta.url).href);
  w.addEventListener("error", (e) => console.log(`host: worker ${i} error event: ${e.message}`));
  w.addEventListener("close", (e: any) => console.log(`host: worker ${i} close code=${e.code}`));
  w.addEventListener("open", () => console.log(`host: worker ${i} open`));
}
setInterval(() => {}, 1 << 30);

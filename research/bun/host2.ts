const ws = Array.from({ length: 4 }, () => new Worker(new URL("./wsrv2.ts", import.meta.url).href));
await Bun.sleep(800);
const mode = process.argv[2];
if (mode === "terminate") ws[0].terminate();
if (mode === "stop") ws[0].postMessage("stop-then-exit");
await Bun.sleep(500);
console.log("READY");
setInterval(() => {}, 1 << 30);

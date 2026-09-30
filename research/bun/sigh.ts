const w = new Worker(new URL("./sigw.ts", import.meta.url).href);
w.onmessage = () => console.log("worker ready");
process.on("SIGTERM", () => { console.log("main got SIGTERM"); setTimeout(() => process.exit(0), 300); });
setInterval(() => {}, 1000);

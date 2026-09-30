import fs from "node:fs";
try { fs.writeSync(3, JSON.stringify({ event: "hello-from-fd3" }) + "\n"); } catch (e) { console.log("fd3 write failed", e.message); }
for (let i = 1; i <= 2; i++) {
  const w = new Worker(new URL("./w3.ts", import.meta.url).href, { env: { ...process.env, WARDEN_WORKER_ID: String(i) } });
  w.addEventListener("close", (e: any) => console.log(`worker ${i} closed code=${e.code}`));
}
setInterval(() => {}, 1 << 30);

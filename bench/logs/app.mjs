// Log-heavy worker for bench/logs.ts. Writes to stdout (a pipe to the
// process manager; Node writes to pipes synchronously on Linux, so a slow
// reader slows this loop down), then records when it finished and idles.
//   LOG_MODE=steady  LOG_RATE lines/s for LOG_SECONDS
//   LOG_MODE=flood   LOG_MB megabytes as fast as the reader takes them
import { writeFileSync } from "node:fs";

const id = process.env.NODE_APP_INSTANCE ?? process.env.WARDEN_WORKER_ID ?? "0";
const done = process.env.LOG_DONE_DIR;
const pad = "x".repeat(80);
const t0 = Date.now();
const finish = (lines, bytes) => {
  if (done) writeFileSync(`${done}/done-${id}-${process.pid}`, JSON.stringify({ lines, bytes, ms: Date.now() - t0 }));
  setInterval(() => {}, 1 << 30); // stay up: a manager would restart an exit
};

if (process.env.LOG_MODE === "flood") {
  const line = `w${id} ${pad} flood\n`;
  const chunk = line.repeat(Math.floor(65536 / line.length));
  const total = Number(process.env.LOG_MB ?? 200) * 1048576;
  let written = 0;
  while (written < total) {
    process.stdout.write(chunk);
    written += chunk.length;
  }
  finish(Math.round(written / line.length), written);
} else {
  const rate = Number(process.env.LOG_RATE ?? 5000);
  const seconds = Number(process.env.LOG_SECONDS ?? 10);
  let n = 0;
  let bytes = 0;
  const perTick = rate / 100; // every 10 ms
  const tick = setInterval(() => {
    let out = "";
    for (let i = 0; i < perTick; i++) out += `w${id} line ${++n} ${pad}\n`;
    process.stdout.write(out);
    bytes += out.length;
    if (n >= rate * seconds) {
      clearInterval(tick);
      finish(n, bytes);
    }
  }, 10);
}

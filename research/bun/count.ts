// N requests, each on a fresh TCP connection
const n = Number(process.argv[2] ?? 400);
const port = Number(process.argv[3] ?? 3100);
const counts: Record<string, number> = {};
let errors = 0;
for (let i = 0; i < n; i++) {
  try {
    const r = await fetch(`http://127.0.0.1:${port}/`, { headers: { connection: "close" }, keepalive: false });
    const t = await r.text(); counts[t] = (counts[t] ?? 0) + 1;
  } catch (e) { errors++; }
}
console.log(JSON.stringify({ counts, errors }));

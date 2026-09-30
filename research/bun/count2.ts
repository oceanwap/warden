const n = Number(process.argv[2] ?? 100), port = Number(process.argv[3] ?? 3100);
const counts: Record<string, number> = {}; let timeouts = 0, errors = 0;
for (let i = 0; i < n; i++) {
  try { const r = await fetch(`http://127.0.0.1:${port}/`, { headers: { connection: "close" }, signal: AbortSignal.timeout(300) });
    const t = await r.text(); counts[t] = (counts[t] ?? 0) + 1; }
  catch (e: any) { if (e.name === "TimeoutError") timeouts++; else errors++; }
}
console.log(JSON.stringify({ counts, timeouts, errors }));

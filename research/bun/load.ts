// C concurrent loops for D ms; keepalive (default fetch pool). Counts failures precisely.
const C = Number(process.argv[2] ?? 50), D = Number(process.argv[3] ?? 5000), port = Number(process.argv[4] ?? 3100);
const end = Date.now() + D; let ok = 0, fail = 0; const errs: Record<string, number> = {}; const by: Record<string, number> = {};
await Promise.all(Array.from({ length: C }, async () => {
  while (Date.now() < end) {
    try { const r = await fetch(`http://127.0.0.1:${port}/`); const t = await r.text(); ok++; by[t] = (by[t] ?? 0) + 1; }
    catch (e: any) { fail++; const k = e.code ?? e.name; errs[k] = (errs[k] ?? 0) + 1; }
  }
}));
console.log(JSON.stringify({ ok, fail, errs, by }));

// What Warden's shim adds to one Bun.serve request, in-process (run by
// bench/shim-cost.ts with and without `--preload <shim>`, after capture.ts).
//   fetch_call_ns     calling the handler Bun.serve was given (the shim's wrapper,
//                     or the app's own handler, which returns a prebuilt Response)
//                     from native code, as Bun does for each request: here the
//                     C++ JSON.parse reviver loop, so the JIT can't inline the call
//                     into a JS loop (it would hide a wrapper)
//   new_response_ns   `new Response(text)` with the global Response (the shim's
//                     wrapper, or the native one)
// Medians of 9 runs.
const prebuilt = new Response("Hello, World!");
const appFetch = function (this: unknown, req: Request) {
  return prebuilt;
};
const server = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: appFetch });
const handler = (globalThis as any).__servedFetch as (req: unknown, s: unknown) => unknown;
if (!handler) throw new Error("preload bench/shim/capture.ts first");
let sink: unknown;

function time(n: number, step: () => void): number {
  for (let w = 0; w < 2; w++) step();
  const runs: number[] = [];
  for (let k = 0; k < 9; k++) {
    const t0 = Bun.nanoseconds();
    step();
    runs.push((Bun.nanoseconds() - t0) / n);
  }
  runs.sort((a, b) => a - b);
  return +runs[4].toFixed(2);
}

const CALLS = 2_000_000;
const json = `[${"0,".repeat(CALLS - 1)}0]`;
// JSON.parse calls its reviver from C++ for every element: the handler itself
// is the reviver (it ignores its arguments), against a trivial one. The two
// alternate, and the median difference is the cost of one call.
const identity = (k: string, v: unknown) => v;
const diffs: number[] = [];
for (let k = 0; k < 11; k++) {
  let t0 = Bun.nanoseconds();
  sink = JSON.parse(json, handler as any);
  const withHandler = Bun.nanoseconds() - t0;
  t0 = Bun.nanoseconds();
  sink = JSON.parse(json, identity);
  if (k >= 2) diffs.push((withHandler - (Bun.nanoseconds() - t0)) / CALLS);
}
diffs.sort((a, b) => a - b);
const RESPONSES = 3_000_000;
const out = {
  fetch_wrapped: handler !== appFetch,
  fetch_call_ns: +diffs[Math.floor(diffs.length / 2)].toFixed(2),
  new_response_ns: time(RESPONSES, () => {
    for (let i = 0; i < RESPONSES; i++) sink = new Response("Hello, World!");
  }),
};
console.log(JSON.stringify(out));
server.stop(true);
process.exit(sink === undefined ? 1 : 0);

// Preloaded before Warden's shim by bench/shim-cost.ts: records the fetch
// handler that reaches the native Bun.serve (the shim's wrapper, if it
// installs one, or the app's own), so bun.ts calls exactly what Bun calls.
const native = Bun.serve;
(Bun as any).serve = function (this: unknown, opts: any, ...rest: any[]) {
  if (opts && !opts.unix) (globalThis as any).__servedFetch = opts.fetch;
  return native.call(this, opts, ...rest);
};

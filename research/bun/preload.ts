const orig = Bun.serve;
let patched = 0;
// @ts-ignore
Bun.serve = function (opts: any) { patched++; return orig.call(Bun, { ...opts, reusePort: true }); };
(globalThis as any).__wardenPatched = () => patched;

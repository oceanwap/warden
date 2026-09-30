const orig = Bun.serve;
// @ts-ignore
Bun.serve = (o: any) => orig.call(Bun, { ...o, reusePort: true });
await import("./main.ts");

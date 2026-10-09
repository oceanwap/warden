// Run with the shim preloaded: are its Response and ReadableStream wrappers
// observable? Prints one JSON object of checks, all true when they are not;
// `hooked`: the wrappers are there at all (an SSE body goes through the
// shim's own stream, so the shim can end it in a drain).
const checks: Record<string, unknown> = {};
const sse = new ReadableStream({ pull: (c) => c.close() });
checks.hooked = new Response(sse, { headers: { "content-type": "text/event-stream" } }).body !== sse;
const nativeText = (name: string, s: string) => s.includes(name) && /\{\s*\[native code\]\s*\}$/.test(s);
const throwsNative = (f: () => unknown, messages: readonly string[]) => {
  try {
    f();
    return false;
  } catch (e) {
    return e instanceof TypeError && messages.includes(e.message);
  }
};

for (const [name, C, make, statics, noNew] of [
  ["Response", Response, () => new Response("x"), ["error", "json", "redirect"], ["Response constructor cannot be invoked without 'new'"]],
  // Bun 1.3, then Bun 1.4.
  ["ReadableStream", ReadableStream, () => new ReadableStream(), [], ["Constructor called as a function", "Use `new ReadableStream(...)` instead of `ReadableStream(...)`"]],
] as const) {
  const x = make();
  class Sub extends (C as any) {}
  const s = new Sub();
  const proto = Object.getOwnPropertyDescriptor(C, "prototype")!;
  Object.assign(checks, {
    [`${name}.constructor`]: x.constructor === C && Object.getPrototypeOf(x).constructor === C,
    [`${name}.toString`]: nativeText(name, Function.prototype.toString.call(C)) && nativeText(name, String(C)) && nativeText(name, `${C}`),
    [`${name}.name`]: C.name === name,
    [`${name}.length`]: C.length === 0,
    [`${name}.hasInstance`]: C[Symbol.hasInstance](x) && x instanceof C && !(C[Symbol.hasInstance]({})),
    [`${name}.subclass`]: s instanceof Sub && s instanceof C && s.constructor === Sub,
    [`${name}.statics`]: statics.every((k) => Object.hasOwn(C, k) && typeof (C as any)[k] === "function"),
    [`${name}.__proto__`]: Object.getPrototypeOf(C) === Function.prototype,
    [`${name}.prototype`]: !proto.writable && !proto.enumerable && !proto.configurable,
    [`${name}()`]: throwsNative(() => (C as any)(), noNew),
  });
}
checks["Response.json"] = Response.json({ a: 1 }) instanceof Response && Response.error() instanceof Response;
const t = Function.prototype.toString;
checks["Function.prototype.toString"] =
  nativeText("toString", t.call(t)) && t.name === "toString" && t.length === 0 && !("prototype" in t);
let notAFunction = false;
try {
  t.call({});
} catch (e) {
  notAFunction = e instanceof TypeError;
}
checks["toString of others"] = t.call(function f() {}) === "function f() {}" && notAFunction;
console.log(JSON.stringify(checks));

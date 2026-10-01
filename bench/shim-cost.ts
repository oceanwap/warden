#!/usr/bin/env bun
// What Warden's shim adds to each request, measured in-process: the shim's
// hooks on the path of one request, without the network, so tens of
// nanoseconds show (end to end they drown in ~10 µs of TCP; see the `shim`
// scenario of bench/run.ts and bench/profile.ts for those numbers).
//
//   bun bench/shim-cost.ts [--rounds 3] [--shim-b /path/to/other-shim.mjs]
//
// Runs bench/shim/node.mjs and bench/shim/bun.ts bare, with the shim
// (shim/warden-shim.mjs, preloaded as Warden does it: drain on, reusePort),
// and with --shim-b (e.g. the previous version: `git show HEAD~1:shim/warden-shim.mjs
// > /tmp/old.mjs`), interleaved, and prints the median of each over the rounds.
import { spawnSync } from "bun";
import { join } from "node:path";
import { ROOT, SHIM, baseEnv, median, parseArgs, table } from "./lib.ts";

const args = parseArgs();
const ROUNDS = Math.max(1, Number(args.rounds ?? 3));
const variants: [string, string | null][] = [
  ["bare", null],
  ["shim", SHIM],
  ...(args["shim-b"] ? [["shim-b", args["shim-b"]] as [string, string]] : []),
];
const env = { ...baseEnv, WARDEN_DRAIN_MS: "500", WARDEN_REUSE_PORT: "1" };
const dir = join(ROOT, "bench/shim");

function run(cmd: string[], withShim: boolean): Record<string, number> {
  const r = spawnSync(cmd, { env: withShim ? env : baseEnv, stdout: "pipe", stderr: "pipe" });
  const line = r.stdout.toString().trim().split("\n").at(-1) ?? "";
  if (r.exitCode !== 0 || !line.startsWith("{")) throw new Error(`${cmd.join(" ")}: ${r.stderr.toString()}`);
  return JSON.parse(line);
}

const results: Record<string, Record<string, number[]>> = {};
for (let round = 1; round <= ROUNDS; round++) {
  for (const [name, shim] of variants) {
    const node = run(["node", ...(shim ? [`--import=${shim}`] : []), join(dir, "node.mjs")], !!shim);
    const bun = run(["bun", `--preload=${join(dir, "capture.ts")}`, ...(shim ? [`--preload=${shim}`] : []), join(dir, "bun.ts")], !!shim);
    const all = { node_request_ns: node.request_ns, bun_fetch_call_ns: bun.fetch_call_ns, bun_new_response_ns: bun.new_response_ns };
    console.error(`round ${round}/${ROUNDS} ${name}: ${JSON.stringify({ ...all, bun_fetch_wrapped: bun.fetch_wrapped })}`);
    for (const [k, v] of Object.entries(all)) ((results[name] ??= {})[k] ??= []).push(v);
  }
}
const names = variants.map(([n]) => n);
const med = (n: string, k: string) => +median(results[n][k]).toFixed(1);
console.log(`ns per request, median of ${ROUNDS} interleaved rounds (each the median of 7-9 runs)\n`);
console.log(
  table(names, (n) => n, [
    ["node:http: 'request' + response 'close'", (n) => med(n, "node_request_ns")],
    ["Bun.serve: the fetch handler, called from native code", (n) => med(n, "bun_fetch_call_ns")],
    ["Bun: new Response(text)", (n) => med(n, "bun_new_response_ns")],
  ]),
);

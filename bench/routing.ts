#!/usr/bin/env bun
// Several apps on one IP and port 443: what each way of putting a front in
// costs, against the app serving visitors itself. One Bun.serve app with its
// own TLS and HTTP/2, N workers under Warden, loaded with h2load over a real
// hostname (SNI `app.test`). Prints a Markdown table on stdout and writes
// bench/results/<date>-routing.json. See bench/README.md.
//
//   bun bench/routing.ts [--duration 10] [--rounds 2] [--workers 2] [--bun PATH] [--nginx PATH]
//                        [--scenarios direct,route,route-copy,nginx-stream,nginx-proxy]
//
// Scenarios:
//   direct        the app's workers on their port, nothing in front (the floor to beat)
//   route         Warden's [route] router: reads the hostname, hands the socket to the app's
//                 worker when its Bun has server.adopt(fd) (oven-sh/bun#44768), else copies
//   route-copy    the same router with WARDEN_HANDOFF=0: always copies bytes both ways
//   nginx-stream  nginx `stream` + ssl_preread: the same pass-through, nginx's way
//   nginx-proxy   nginx terminating TLS and HTTP/2, proxying plain HTTP/1.1 to the app
//                 (keep-alive pool): the usual nginx-in-front setup
//
// --bun PATH runs the app with another Bun (a build with server.adopt, to see the hand-off);
// --nginx PATH picks an nginx built with the stream and ssl_preread modules (nginx-stream is
// skipped without them). On a machine with 4 CPUs or more the servers run on the first half
// and h2load on the other (override with --app-cpus / --loadgen-cpus); the CPU column is the
// busy time of the server CPUs per request, everything on them included.
//
// Rounds are interleaved (A B C A B C ...) and the table shows the median of each number.
// client_ip is off: the router's own cost is the same, and it needs no root.

import { spawnSync } from "bun";
import { existsSync, mkdirSync, readFileSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { ROOT, TMP, WARDEN, baseEnv, listenersOnPort, machine, median, parseArgs, saveResults, sleep, version, waitFor } from "./lib.ts";

const args = parseArgs();
const DURATION = Number(args.duration ?? 10);
const ROUNDS = Math.max(1, Number(args.rounds ?? 2));
const WORKERS = Number(args.workers ?? 2);
const BUN_ARG = args.bun ?? "bun";
const ALL = ["direct", "route", "route-copy", "nginx-stream", "nginx-proxy"];
const SCENARIOS = (args.scenarios ?? ALL.join(",")).split(",");

const half = Math.floor(navigator.hardwareConcurrency / 2);
const range = (a: number, b: number) => (a === b ? `${a}` : `${a}-${b}`);
const APP_CPUS = args["app-cpus"] ?? (half >= 2 ? range(0, half - 1) : undefined);
const LOADGEN_CPUS = args["loadgen-cpus"] ?? (half >= 2 ? range(half, 2 * half - 1) : undefined);
const pin = (cpus: string | undefined, cmd: string[]) => (cpus ? ["taskset", "-c", cpus, ...cmd] : cmd);

const PORT = { app: 3940, plain: 3941, route: 3942, stream: 3943, proxy: 3944 };
const DIR = join(TMP, "routing");
const HOME = join(DIR, "home");
const HOST = "app.test";
const BUN = BUN_ARG === "bun" ? "bun" : join(DIR, "bin/bun");

const LOADS = [
  { name: "HTTP/2, 12 B", path: "/", h2: ["-c32", "-m10"] },
  { name: "HTTP/2, 64 KB", path: "/big", h2: ["-c32", "-m10"] },
  { name: "HTTP/1.1, 12 B", path: "/", h2: ["--h1", "-c64"] },
];

// ---------------------------------------------------------------------- setup

function setup() {
  rmSync(DIR, { recursive: true, force: true });
  mkdirSync(HOME, { recursive: true });
  const cert = join(DIR, "cert.pem");
  const key = join(DIR, "key.pem");
  const ssl = spawnSync(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-nodes",
    "-days", "2", "-subj", `/CN=${HOST}`, "-addext", `subjectAltName=DNS:${HOST}`, "-keyout", key, "-out", cert]);
  if (ssl.exitCode !== 0) throw new Error(`openssl: ${ssl.stderr}`);
  // Warden knows a Bun app by its command's name: a build under another name goes in as `bun`.
  if (BUN !== "bun") {
    mkdirSync(join(DIR, "bin"));
    symlinkSync(resolve(BUN_ARG), BUN);
  }
  const app = join(ROOT, "bench/routing/app.ts");
  const env = `env = { BENCH_CERT = "${cert}", BENCH_KEY = "${key}" }`;
  const conf = (name: string, port: number, mode: string) =>
    `[app]\nname = "${name}"\ncommand = "${BUN}"\nargs = ["${app}", "${mode}"]\nport = ${port}\n${env}\n[workers]\ncount = ${WORKERS}\n`;
  writeFileSync(join(HOME, "app.toml"), conf("app", PORT.app, "tls"));
  writeFileSync(join(HOME, "plain.toml"), conf("plain", PORT.plain, "plain"));
  writeFileSync(join(HOME, "edge.toml"),
    `[app]\nname = "edge"\nport = ${PORT.route}\n[workers]\ncount = ${WORKERS}\n[route]\nclient_ip = false\nhosts = { "${HOST}" = "app" }\n`);
  return { cert, key };
}

const wenv = (extra: Record<string, string> = {}) => ({ ...baseEnv, WARDEN_HOME: HOME, ...extra });
function warden(cmd: string[], extra: Record<string, string> = {}) {
  const r = spawnSync(pin(APP_CPUS, [WARDEN, ...cmd]), { env: wenv(extra) });
  if (r.exitCode !== 0) throw new Error(`warden ${cmd.join(" ")}: ${r.stderr}`);
}
async function wardenStart(name: string, port: number, extra: Record<string, string> = {}) {
  warden(["start", name], extra);
  await waitFor(() => listenersOnPort(port) > 0, 15_000);
}
const wardenKill = () => spawnSync([WARDEN, "kill"], { env: wenv() });

/** nginx with the stream modules, if one is around: --nginx, then the usual places. */
function findNginx(): { bin?: string; stream: boolean; version: string } {
  const bin = [args.nginx, "/usr/local/nginx/sbin/nginx", "/usr/sbin/nginx", "/usr/local/sbin/nginx", "/usr/bin/nginx"]
    .filter(Boolean).find((p) => existsSync(p!));
  if (!bin) return { stream: false, version: "not installed" };
  const v = spawnSync([bin, "-V"]).stderr.toString();
  // A dynamic stream module counts only if its .so is there to load.
  const builtIn = /--with-stream(\s|$)/.test(v) && /ssl_preread/.test(v);
  return { bin, stream: builtIn, version: (v.match(/nginx version: (\S+)/) ?? [])[1] ?? "?" };
}

async function nginx(bin: string, kind: "stream" | "proxy", tls: { cert: string; key: string }) {
  const dir = join(DIR, `nginx-${kind}`);
  mkdirSync(join(dir, "logs"), { recursive: true });
  const body = kind === "stream"
    ? `stream {
  server { listen ${PORT.stream} reuseport; ssl_preread on; proxy_pass 127.0.0.1:${PORT.app}; }
}`
    : `http {
  access_log off;
  keepalive_requests 10000000;
  upstream app { server 127.0.0.1:${PORT.plain}; keepalive 1024; keepalive_requests 10000000; }
  server {
    listen ${PORT.proxy} ssl reuseport;
    http2 on;
    ssl_certificate ${tls.cert}; ssl_certificate_key ${tls.key}; ssl_protocols TLSv1.3;
    location / { proxy_pass http://app; proxy_http_version 1.1; proxy_set_header Connection ""; proxy_set_header Host $host; }
  }
}`;
  // As many nginx workers as the app has: the same processes either side.
  writeFileSync(join(dir, "nginx.conf"),
    `worker_processes ${WORKERS};\npid ${dir}/nginx.pid;\nerror_log ${dir}/error.log;\nevents { worker_connections 4096; }\n${body}\n`);
  const r = spawnSync(pin(APP_CPUS, [bin, "-c", join(dir, "nginx.conf"), "-p", dir]), { env: baseEnv });
  if (r.exitCode !== 0) throw new Error(`nginx: ${r.stderr}`);
  const port = kind === "stream" ? PORT.stream : PORT.proxy;
  await waitFor(() => listenersOnPort(port) > 0, 5_000);
  return () => {
    spawnSync([bin, "-c", join(dir, "nginx.conf"), "-p", dir, "-s", "stop"]);
  };
}

// ----------------------------------------------------------------------- load

/** Busy jiffies of the server CPUs (all CPUs when unpinned). */
function busy(): number {
  const want = cpuList(APP_CPUS);
  let t = 0;
  for (const l of readFileSync("/proc/stat", "utf8").split("\n")) {
    const m = l.match(/^cpu(\d+) (.*)/);
    if (!m || (want && !want.has(Number(m[1])))) continue;
    const f = m[2].split(" ").map(Number); // user nice system idle iowait irq softirq steal
    t += f[0] + f[1] + f[2] + f[5] + f[6] + f[7];
  }
  return t;
}
function cpuList(s?: string): Set<number> | undefined {
  if (!s) return undefined;
  const out = new Set<number>();
  for (const part of s.split(",")) {
    const [a, b] = part.split("-").map(Number);
    for (let i = a; i <= (b ?? a); i++) out.add(i);
  }
  return out;
}
const HZ = Number(spawnSync(["getconf", "CLK_TCK"]).stdout.toString().trim() || 100);

interface Run { rps: number; cpu_us: number; ok: number; failed: number }

function h2load(port: number, load: (typeof LOADS)[number]): Run {
  const b0 = busy();
  const r = spawnSync(pin(LOADGEN_CPUS, ["h2load", "-t2", ...load.h2, `-D${DURATION}`, "--warm-up-time=1",
    `--connect-to=127.0.0.1:${port}`, `https://${HOST}:${port}${load.path}`]), { env: baseEnv });
  const b1 = busy();
  const out = r.stdout.toString();
  const ok = Number((out.match(/(\d+) succeeded/) ?? [])[1] ?? 0);
  const failed = Number((out.match(/(\d+) failed/) ?? [])[1] ?? 0);
  const rps = Number((out.match(/finished in [^,]+, ([\d.]+) req\/s/) ?? [])[1] ?? 0);
  // Warm-up included in the CPU, so per request it is a slight over-count, the same for all.
  const cpu_us = ok ? ((b1 - b0) / HZ) * 1e6 / (ok * (DURATION + 1) / DURATION) : 0;
  return { rps: Math.round(rps), cpu_us: Math.round(cpu_us * 10) / 10, ok, failed };
}

// ----------------------------------------------------------------------- main

const tls = setup();
const ng = findNginx();
const adopt = spawnSync([BUN, "-e", "const s = Bun.serve({ port: 0, fetch: () => new Response() }); console.log(typeof s.adopt); s.stop(true)"],
  { env: baseEnv }).stdout.toString().trim() === "function";
const active = SCENARIOS.filter((s) => {
  if (s.startsWith("nginx") && !ng.bin) return console.error(`skip ${s}: nginx is not installed`), false;
  if (s === "nginx-stream" && !ng.stream) return console.error(`skip ${s}: ${ng.bin} has no built-in stream + ssl_preread (try --nginx)`), false;
  return true;
});
const results: Record<string, Run[][]> = Object.fromEntries(active.map((s) => [s, LOADS.map(() => [])]));

async function measure(s: string, port: number) {
  if (!active.includes(s)) return;
  LOADS.forEach((load, i) => {
    const r = h2load(port, load);
    if (!r.ok) throw new Error(`${s} ${load.name}: no request succeeded`);
    results[s][i].push(r);
    console.error(`${s.padEnd(13)} ${load.name.padEnd(15)} ${String(r.rps).padStart(7)} req/s  ${r.cpu_us} µs/req`);
  });
}

try {
  wardenKill();
  for (let round = 1; round <= ROUNDS; round++) {
    console.error(`round ${round}`);
    // Hand-off: the router's config first, so the app's supervisor knows it is routed.
    if (active.includes("route")) {
      await wardenStart("edge", PORT.route);
      await wardenStart("app", PORT.app);
      await sleep(500);
      await measure("route", PORT.route);
      wardenKill();
      await sleep(500);
    }
    // Everything else with the app listening on its port as usual.
    await wardenStart("edge", PORT.route);
    await wardenStart("app", PORT.app, { WARDEN_HANDOFF: "0" });
    await sleep(500);
    await measure("direct", PORT.app);
    await measure("route-copy", PORT.route);
    if (active.includes("nginx-stream")) {
      const stop = await nginx(ng.bin!, "stream", tls);
      await measure("nginx-stream", PORT.stream);
      stop();
    }
    if (active.includes("nginx-proxy")) {
      await wardenStart("plain", PORT.plain);
      const stop = await nginx(ng.bin!, "proxy", tls);
      await measure("nginx-proxy", PORT.proxy);
      stop();
    }
    wardenKill();
    await sleep(500);
  }
} finally {
  wardenKill();
}

const med = (s: string, i: number, k: "rps" | "cpu_us") => median(results[s][i].map((r) => r[k]));
const label = (s: string) => (s === "route" ? (adopt ? "route (hand-off)" : "route (copies: Bun has no adopt)") : s);
const meta = {
  ...machine(), suite: "routing", rounds: ROUNDS, duration_s: DURATION, workers: WORKERS,
  bun: version([BUN, "--version"]), bun_adopt: adopt, nginx: ng.version, h2load: version(["h2load", "--version"]),
  server_cpus: APP_CPUS ?? "all", client_cpus: LOADGEN_CPUS ?? "all",
};
const lines = [
  `### Several apps on one IP: req/s (server CPU µs per request), median of ${ROUNDS}`,
  "",
  `${WORKERS} workers · Bun ${meta.bun}${adopt ? " with server.adopt" : ""} · nginx ${ng.version} · ${meta.h2load} · ` +
    `server CPUs ${meta.server_cpus}, client ${meta.client_cpus} · ${meta.cpus} CPUs ${meta.cpu_model}`,
  "",
  `| | ${LOADS.map((l) => l.name).join(" | ")} |`,
  `|---|${LOADS.map(() => "---").join("|")}|`,
  ...active.map((s) => `| ${label(s)} | ${LOADS.map((_, i) => `${med(s, i, "rps").toLocaleString("en")} (${med(s, i, "cpu_us")})`).join(" | ")} |`),
];
console.log(lines.join("\n"));
console.error(`raw: ${saveResults("routing", { meta, results })}`);

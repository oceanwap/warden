// Warden worker-mode host: one Bun process running WARDEN_WORKERS Workers.
// Each Worker runs the shim, which imports the app (WARDEN_ENTRY).
//
// The host is deliberately dumb: it starts the Workers, relays their events
// to Warden on fd 3, and relays shutdown to them (Workers never receive
// process signals). Restart policy lives in Warden: when a Worker dies,
// Warden replaces this whole process (start new, wait ready, drain old).

import fs from "node:fs";

const env = process.env;
const count = Number(env.WARDEN_WORKERS || 1);
const shim = env.WARDEN_SHIM;
const ipcFd = env.WARDEN_IPC_FD ? Number(env.WARDEN_IPC_FD) : null;

if (!shim || !env.WARDEN_ENTRY) {
  console.error("warden-host: WARDEN_SHIM and WARDEN_ENTRY must be set");
  process.exit(78);
}

function report(msg) {
  if (ipcFd == null) return;
  try {
    fs.writeSync(ipcFd, JSON.stringify(msg) + "\n");
  } catch {}
}

const workers = new Map();
let shuttingDown = false;

function start(id) {
  const w = new Worker(shim, {
    env: { ...env, WARDEN_WORKER_ID: String(id), WARDEN_IPC_FD: "" },
  });
  workers.set(id, w);
  w.addEventListener("message", (e) => {
    const m = e.data && e.data.__warden;
    if (m && typeof m === "object") report({ ...m, worker: id });
  });
  w.addEventListener("error", (e) => {
    report({ ev: "error", worker: id, message: String((e && e.message) || e) });
  });
  w.addEventListener("close", (e) => {
    workers.delete(id);
    report({ ev: "exit", worker: id, code: e && typeof e.code === "number" ? e.code : null, expected: shuttingDown });
    if (shuttingDown && workers.size === 0) process.exit(0);
  });
}

function shutdown() {
  if (shuttingDown) return;
  shuttingDown = true;
  if (workers.size === 0) process.exit(0);
  for (const w of workers.values()) w.postMessage({ __warden: "shutdown" });
}

process.on("SIGTERM", shutdown);
process.on("SIGINT", shutdown);

for (let i = 1; i <= count; i++) start(i);

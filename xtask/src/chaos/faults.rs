//! The faults the chaos soak injects. Each one runs to its end (the fleet
//! recovered, or the recovery bound passed) and returns a [`Fault`]: when it
//! ran, which app it hit, what it allows clients to see (requests in flight
//! on a killed worker, resets of queued connections while workers stop on
//! purpose with `tcp_migrate_req = 0`, an app down by design), how long the
//! fleet took to recover, and anything that went wrong on the way.

use super::fleet::{AppSpec, Fleet, WATCHDOG_S};
use super::{Rng, Shared, procfs};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const KINDS: &[&str] = &[
    "kill-worker",
    "kill-standby",
    "kill-supervisor",
    "kill-wardend",
    "stop-worker",
    "stop-supervisor",
    "stop-wardend",
    "reload",
    "safe-reload",
    "restart",
    "restart-hard",
    "scale",
    "overlap",
    "bad-config",
    "release-swap",
    "throw-thread",
    "log-flood",
    "disk-full",
    "crash-loop",
];

/// What a fault lets clients of its app see without it being a violation.
#[derive(Debug, Clone, Default)]
pub struct Allow {
    /// A worker was killed (or hung and killed by the watchdog): requests
    /// in flight on it, and connections queued on its listener, may fail.
    pub killed: bool,
    /// Workers stop on purpose (reload, scale down…): with
    /// `tcp_migrate_req = 0`, connections queued on a closing listener are reset.
    pub planned: bool,
    /// The app is down for a moment by design: its supervisor was killed
    /// (workers exit with it, wardend restarts it), a worker-mode host was
    /// killed (its threads go together), `restart --hard`.
    pub down: bool,
}

#[derive(Debug, Clone)]
pub struct Fault {
    pub n: usize,
    pub kind: &'static str,
    pub app: Option<String>,
    pub detail: String,
    pub start: f64,
    pub end: f64,
    /// Seconds from the end of the injection to the whole fleet ready.
    pub recovery: Option<f64>,
    /// How long the CLI operation took, for operations.
    pub op_ms: Option<f64>,
    pub allow: Allow,
    pub problems: Vec<String>,
    pub skipped: Option<String>,
}

pub struct Ctx<'a> {
    pub sh: &'a Arc<Shared>,
    pub fleet: &'a Fleet,
    pub rng: &'a mut Rng,
    pub bound: Duration,
}

const OP_TIMEOUT: Duration = Duration::from_secs(180);

pub fn run(kind: &'static str, n: usize, cx: &mut Ctx) -> Fault {
    let mut f = Fault {
        n,
        kind,
        app: None,
        detail: String::new(),
        start: cx.sh.now(),
        end: 0.0,
        recovery: None,
        op_ms: None,
        allow: Allow::default(),
        problems: Vec::new(),
        skipped: None,
    };
    match kind {
        "kill-worker" => kill_worker(cx, &mut f),
        "kill-standby" => kill_standby(cx, &mut f),
        "kill-supervisor" => kill_supervisor(cx, &mut f),
        "kill-wardend" => kill_wardend(cx, &mut f),
        "stop-worker" => stop_worker(cx, &mut f),
        "stop-supervisor" => stop_supervisor(cx, &mut f),
        "stop-wardend" => stop_wardend(cx, &mut f),
        "reload" | "safe-reload" | "restart" | "restart-hard" => operation(cx, &mut f, kind),
        "scale" => scale(cx, &mut f),
        "overlap" => overlap(cx, &mut f),
        "bad-config" => bad_config(cx, &mut f),
        "release-swap" => release_swap(cx, &mut f),
        "throw-thread" => throw_thread(cx, &mut f),
        "log-flood" => log_flood(cx, &mut f),
        "disk-full" => disk_full(cx, &mut f),
        "crash-loop" => crash_loop(cx, &mut f),
        _ => f.skipped = Some(format!("unknown fault {kind}")),
    }
    if f.end == 0.0 {
        f.end = cx.sh.now();
    }
    f
}

// ----------------------------------------------------------------- helpers

fn pick<'a>(cx: &mut Ctx, apps: &'a [AppSpec], ok: impl Fn(&AppSpec) -> bool) -> Option<&'a AppSpec> {
    let v: Vec<&AppSpec> = apps.iter().filter(|a| ok(a)).collect();
    if v.is_empty() { None } else { Some(v[cx.rng.below(v.len())]) }
}

fn status(cx: &Ctx, app: &str) -> Option<Value> {
    cx.fleet.status(app)
}

fn pid_of(v: &Value) -> Option<u32> {
    v["pid"].as_u64().map(|p| p as u32)
}

/// A random RUNNING worker: (worker id, pid).
fn running_worker(cx: &mut Ctx, st: &Value) -> Option<(u64, u32)> {
    let ws: Vec<(u64, u32)> = st["workers"]
        .as_array()?
        .iter()
        .filter(|w| w["state"] == "RUNNING")
        .filter_map(|w| Some((w["id"].as_u64()?, pid_of(w)?)))
        .collect();
    if ws.is_empty() { None } else { Some(ws[cx.rng.below(ws.len())]) }
}

/// Every process the supervisor in `st` runs: workers, standbys, the host.
fn all_worker_pids(st: &Value) -> Vec<u32> {
    let mut v: Vec<u32> = st["workers"].as_array().map(|a| a.iter().filter_map(pid_of).collect()).unwrap_or_default();
    v.extend(st["standbys"].as_array().map(|a| a.iter().filter_map(pid_of).collect::<Vec<_>>()).unwrap_or_default());
    v.extend(pid_of(&st["host"]));
    // Worker mode: every worker row has the host's pid.
    v.sort_unstable();
    v.dedup();
    v
}

fn start_of(pid: u32) -> Option<u64> {
    procfs::stat(pid).map(|s| s.start)
}

/// SIGKILL `pid` (if it is still the process we looked at), marking it as
/// killed on purpose first.
fn kill9(cx: &Ctx, pid: u32) -> bool {
    let Some(start) = start_of(pid) else { return false };
    cx.sh.doom_pid(pid);
    procfs::signal(pid, start, "KILL")
}

/// Wait for the whole fleet to be ready again; record the time from `from`.
fn recover(cx: &Ctx, f: &mut Fault, from: Instant) {
    match cx.fleet.wait_ready(cx.bound) {
        Ok(_) => f.recovery = Some(from.elapsed().as_secs_f64()),
        Err(why) => {
            f.problems.push(format!("the fleet did not recover within {}s: {}", cx.bound.as_secs(), why.join("; ")))
        }
    }
    f.end = cx.sh.now();
}

fn sleep_s(s: f64) {
    std::thread::sleep(Duration::from_secs_f64(s.max(0.0)));
}

/// GET `path` on a new connection; the body on 200.
fn get(port: u16, path: &str) -> Option<String> {
    use std::io::{Read, Write};
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    if !buf.starts_with("HTTP/1.1 200") {
        return None;
    }
    buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

// ------------------------------------------------------------------ kills

fn kill_worker(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy) else { return };
    f.app = Some(spec.name.into());
    let Some(st) = status(cx, spec.name) else {
        f.skipped = Some(format!("{} is not answering", spec.name));
        return;
    };
    f.allow.killed = true;
    let pid = if spec.worker_mode {
        f.allow.down = true;
        let Some(p) = pid_of(&st["host"]) else { return };
        f.detail = format!("kill -9 of the worker-mode host pid {p} (its threads go together)");
        p
    } else {
        let Some((id, p)) = running_worker(cx, &st) else { return };
        f.detail = format!("kill -9 of worker {id} pid {p}");
        p
    };
    let from = Instant::now();
    if !kill9(cx, pid) {
        f.skipped = Some(format!("pid {pid} was gone before the kill"));
        return;
    }
    recover(cx, f, from);
}

fn kill_standby(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| a.standby > 0) else { return };
    f.app = Some(spec.name.into());
    let Some(st) = status(cx, spec.name) else { return };
    let Some(pid) = st["standbys"].as_array().and_then(|s| s.iter().find(|x| x["state"] == "STANDBY")).and_then(pid_of)
    else {
        f.skipped = Some("no available standby".into());
        return;
    };
    f.detail = format!("kill -9 of the standby pid {pid}");
    let from = Instant::now();
    kill9(cx, pid);
    recover(cx, f, from);
}

fn kill_supervisor(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |_| true) else { return };
    f.app = Some(spec.name.into());
    let Some(st) = status(cx, spec.name) else { return };
    let Some(pid) = pid_of(&st) else { return };
    f.allow = Allow { killed: true, planned: true, down: true };
    for w in all_worker_pids(&st) {
        cx.sh.doom_pid(w);
    }
    f.detail = format!("kill -9 of the supervisor pid {pid}; wardend restarts it");
    let from = Instant::now();
    kill9(cx, pid);
    recover(cx, f, from);
    if let Some(new) = status(cx, spec.name).and_then(|s| pid_of(&s)) {
        if new == pid {
            f.problems.push(format!("the supervisor pid is still {pid} after kill -9"));
        }
        f.detail += &format!(" (new pid {new})");
    }
}

fn kill_wardend(cx: &mut Ctx, f: &mut Fault) {
    let Some(pid) = cx.fleet.wardend_pid() else {
        f.problems.push("wardend was not running before the fault".into());
        return;
    };
    let down = cx.rng.range(0.5, 3.0);
    f.detail = format!("kill -9 of wardend pid {pid}; started again after {down:.1}s (as systemd would)");
    kill9(cx, pid);
    sleep_s(down);
    let r = cx.fleet.cli(&["daemon", "--background"], Duration::from_secs(30));
    if !r.ok() {
        f.problems.push(format!("warden daemon --background: {}", r.brief()));
    }
    // From wardend's start: until then nothing was meant to restart it.
    recover(cx, f, Instant::now());
}

// -------------------------------------------------------------- SIGSTOP

fn stop_worker(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy) else { return };
    f.app = Some(spec.name.into());
    let Some(st) = status(cx, spec.name) else { return };
    f.allow.killed = true;
    let (what, pid) = if spec.worker_mode {
        f.allow.down = true;
        let Some(p) = pid_of(&st["host"]) else { return };
        ("the worker-mode host", p)
    } else {
        let Some((id, p)) = running_worker(cx, &st) else { return };
        let _ = id;
        ("a worker", p)
    };
    let Some(start) = start_of(pid) else { return };
    let hold = WATCHDOG_S as f64 + cx.rng.range(2.5, 4.5);
    f.detail = format!("SIGSTOP of {what} pid {pid} for {hold:.1}s (watchdog.timeout = {WATCHDOG_S}s)");
    cx.sh.doom_pid(pid);
    procfs::signal(pid, start, "STOP");
    sleep_s(hold);
    if procfs::same(pid, start) {
        f.problems.push(format!(
            "the watchdog did not kill hung pid {pid} within {hold:.1}s (watchdog.timeout = {WATCHDOG_S}s)"
        ));
        procfs::signal(pid, start, "CONT");
    }
    recover(cx, f, Instant::now());
}

fn stop_supervisor(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |_| true) else { return };
    f.app = Some(spec.name.into());
    let Some(st) = status(cx, spec.name) else { return };
    let Some(pid) = pid_of(&st) else { return };
    let Some(start) = start_of(pid) else { return };
    let workers = all_worker_pids(&st);
    let hold = cx.rng.range(2.0, 9.0);
    f.detail = format!("SIGSTOP of the supervisor pid {pid} for {hold:.1}s; its workers keep serving");
    let t = cx.sh.now();
    cx.sh.frozen(t, f64::INFINITY);
    procfs::signal(pid, start, "STOP");
    sleep_s(hold);
    procfs::signal(pid, start, "CONT");
    let resumed = Instant::now();
    cx.sh.frozen(t, cx.sh.now());
    recover(cx, f, resumed);
    // Give it a few more ticks to act on what it found.
    sleep_s(2.0);
    let gone: Vec<u32> = workers.iter().copied().filter(|p| !procfs::alive(*p)).collect();
    if !gone.is_empty() {
        f.problems.push(format!(
            "after a {hold:.1}s freeze the supervisor killed or lost healthy workers {gone:?} (they were serving all along)"
        ));
    }
}

fn stop_wardend(cx: &mut Ctx, f: &mut Fault) {
    let Some(pid) = cx.fleet.wardend_pid() else {
        f.problems.push("wardend was not running before the fault".into());
        return;
    };
    let Some(start) = start_of(pid) else { return };
    let before: Vec<(String, Option<u64>)> = cx
        .fleet
        .list()
        .unwrap_or_default()
        .iter()
        .map(|a| (a["app"].as_str().unwrap_or("?").to_string(), a["status"]["pid"].as_u64()))
        .collect();
    let hold = cx.rng.range(2.0, 9.0);
    f.detail = format!("SIGSTOP of wardend pid {pid} for {hold:.1}s");
    procfs::signal(pid, start, "STOP");
    sleep_s(hold);
    procfs::signal(pid, start, "CONT");
    recover(cx, f, Instant::now());
    // Give it time to act on what it found (it must not restart a live supervisor).
    sleep_s(3.0);
    let after = cx.fleet.list().unwrap_or_default();
    for (app, pid) in before {
        let now = after.iter().find(|a| a["app"] == app.as_str()).and_then(|a| a["status"]["pid"].as_u64());
        if pid.is_some() && now != pid {
            f.problems.push(format!("{app}: supervisor pid changed {pid:?} -> {now:?} after wardend was frozen"));
        }
    }
}

// ------------------------------------------------------------ operations

fn op_args(op: &str, app: &str) -> Vec<String> {
    match op {
        "restart-hard" => vec!["restart".into(), app.into(), "--hard".into()],
        other => vec![other.into(), app.into()],
    }
}

fn run_op(cx: &Ctx, op: &str, app: &str) -> super::fleet::CliOut {
    let args = op_args(op, app);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    cx.fleet.cli(&refs, OP_TIMEOUT)
}

fn operation(cx: &mut Ctx, f: &mut Fault, op: &'static str) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy) else { return };
    f.app = Some(spec.name.into());
    f.allow.planned = true;
    f.allow.down = op == "restart-hard";
    f.detail = format!("warden {}", op_args(op, spec.name).join(" "));
    let r = run_op(cx, op, spec.name);
    f.op_ms = Some(r.ms);
    if !r.ok() {
        f.problems.push(format!("{} failed: {}", f.detail, r.brief()));
    }
    recover(cx, f, Instant::now());
}

fn scale_to(cx: &Ctx, f: &mut Fault, app: &str, n: usize) -> bool {
    let r = cx.fleet.cli(&["scale", app, &n.to_string()], OP_TIMEOUT);
    if r.ok() {
        cx.fleet.set_expected(app, n);
        true
    } else {
        f.problems.push(format!("warden scale {app} {n}: {}", r.brief()));
        false
    }
}

fn scale(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    f.allow.planned = true;
    let base = spec.count;
    let first = if base >= 2 && cx.rng.below(2) == 0 { base - 1 } else { base + 1 };
    f.detail = format!("warden scale {app} {first}, then back to {base}");
    let t0 = Instant::now();
    if scale_to(cx, f, app, first) {
        if let Err(why) = cx.fleet.wait_ready(cx.bound) {
            f.problems.push(format!("not ready at {first} workers: {}", why.join("; ")));
        }
        sleep_s(cx.rng.range(0.5, 3.0));
    }
    if !scale_to(cx, f, app, base) {
        cx.fleet.set_expected(app, base);
    }
    f.op_ms = Some(t0.elapsed().as_secs_f64() * 1000.0);
    recover(cx, f, Instant::now());
}

/// A rollout, and while it runs a second operation on the same app.
fn overlap(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy && !a.static_site) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    f.allow.planned = true;
    let first = ["reload", "safe-reload", "restart"][cx.rng.below(3)];
    let second =
        ["reload", "safe-reload", "restart", "restart-hard", "scale-up", "scale-down", "kill-worker"][cx.rng.below(7)];
    let delay = cx.rng.range(0.2, 2.0);
    f.detail = format!("warden {first} {app}, then {second} after {delay:.1}s");
    let first_cmd = cx.fleet.command(&op_args(first, app).iter().map(String::as_str).collect::<Vec<_>>());
    let h = std::thread::spawn(move || super::fleet::run_cmd(first_cmd, OP_TIMEOUT));
    sleep_s(delay);
    let base = spec.count;
    let second_out = match second {
        "scale-up" | "scale-down" => {
            let n = if second == "scale-up" { base + 1 } else { base.saturating_sub(1).max(1) };
            let r = cx.fleet.cli(&["scale", app, &n.to_string()], OP_TIMEOUT);
            if r.ok() {
                cx.fleet.set_expected(app, n);
            }
            Some(r)
        }
        "kill-worker" => {
            f.allow.killed = true;
            if let Some(st) = status(cx, app) {
                let pid = if spec.worker_mode {
                    f.allow.down = true;
                    pid_of(&st["host"])
                } else {
                    running_worker(cx, &st).map(|w| w.1)
                };
                if let Some(p) = pid {
                    kill9(cx, p);
                    f.detail += &format!(" (pid {p})");
                }
            }
            None
        }
        op => {
            f.allow.down |= op == "restart-hard";
            Some(run_op(cx, op, app))
        }
    };
    let first_out = h.join().unwrap_or_else(|_| super::fleet::CliOut { code: None, out: "panicked".into(), ms: 0.0 });
    f.op_ms = Some(first_out.ms);
    // Overlapping operations may be refused (exit 1, "in progress") or
    // fail (a rollout whose worker was killed): never hang or exit 2.
    for (what, r) in [(first, Some(&first_out)), (second, second_out.as_ref())] {
        let Some(r) = r else { continue };
        if !matches!(r.code, Some(0 | 1)) {
            f.problems.push(format!("{what}: {}", r.brief()));
        }
        if r.code == Some(1) {
            f.detail += &format!("; {what} exit 1: {}", r.out.trim().lines().last().unwrap_or(""));
        }
    }
    if cx.fleet.expected(app) != base {
        if let Err(why) = cx.fleet.wait_ready(cx.bound) {
            f.problems.push(format!("not ready before scaling back: {}", why.join("; ")));
        }
        if !scale_to(cx, f, app, base) {
            cx.fleet.set_expected(app, base);
        }
    }
    recover(cx, f, Instant::now());
}

fn bad_config(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| !a.crashy && !a.static_site && !a.worker_mode) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    f.allow.planned = true;
    f.allow.killed = true;
    let Some(good) = cx.fleet.good_config.get(app).cloned() else { return };
    let mut variants = vec!["unknown-key", "missing-script"];
    if spec.chaos_app {
        variants.push("unhealthy");
    }
    let variant = variants[cx.rng.below(variants.len())];
    let bad = match variant {
        "unknown-key" => good.replacen("[app]\n", "[app]\nchaos_tpyo = true\n", 1),
        "missing-script" => {
            let mut out = String::new();
            for l in good.lines() {
                if l.starts_with("args = ") {
                    out += "args = [\"/nonexistent/chaos-missing.ts\"]\n";
                } else {
                    out += l;
                    out += "\n";
                }
            }
            out
        }
        _ => good.replacen("[app]\n", "[app]\nenv = { CHAOS_HEALTH_FAIL = \"1\" }\n", 1),
    };
    let op = if cx.rng.below(2) == 0 { "reload" } else { "safe-reload" };
    f.detail =
        format!("{variant} config deployed with warden {op} {app}, a worker killed, then the good config reloaded");
    let path = cx.fleet.config_path(app);
    if let Err(e) = std::fs::write(&path, &bad) {
        f.problems.push(format!("writing {}: {e}", path.display()));
        return;
    }
    let r = run_op(cx, op, app);
    f.op_ms = Some(r.ms);
    if r.code != Some(1) {
        f.problems.push(format!("warden {op} {app} with a {variant} config should fail with exit 1: {}", r.brief()));
    }
    // A crash now must restart on the config in effect, not the rejected one.
    if let Some((_, pid)) = status(cx, app).and_then(|st| running_worker(cx, &st)) {
        let from = Instant::now();
        kill9(cx, pid);
        if let Err(why) = cx.fleet.wait_ready(cx.bound) {
            f.problems.push(format!(
                "after the rejected config, a killed worker did not come back ({:.1}s): {}",
                from.elapsed().as_secs_f64(),
                why.join("; ")
            ));
        }
    }
    if let Err(e) = std::fs::write(&path, &good) {
        f.problems.push(format!("restoring {}: {e}", path.display()));
    }
    let r = run_op(cx, "reload", app);
    if !r.ok() {
        f.problems.push(format!("warden reload {app} with the good config again: {}", r.brief()));
    }
    recover(cx, f, Instant::now());
}

/// Where each worker and standby of `app` runs (its cwd), by pid.
fn cwds(st: &Value) -> Vec<(u32, String)> {
    let mut v: Vec<u32> = st["workers"].as_array().map(|a| a.iter().filter_map(pid_of).collect()).unwrap_or_default();
    v.extend(
        st["standbys"]
            .as_array()
            .map(|a| a.iter().filter(|s| s["state"] == "STANDBY").filter_map(pid_of).collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    v.into_iter().map(|p| (p, procfs::cwd(p).map(|c| c.display().to_string()).unwrap_or_default())).collect()
}

fn release_swap(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| a.releases) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    f.allow.planned = true;
    f.allow.killed = true;
    let rel = cx.fleet.home.join("releases");
    let current = rel.join("current");
    let old = std::fs::read_link(&current).map(|p| p.display().to_string()).unwrap_or_default();
    let new = if old == "v1" { "v2" } else { "v1" };
    let (old_real, new_real) = (rel.join(&old).display().to_string(), rel.join(new).display().to_string());
    f.detail = format!("current: {old} -> {new}, kill -9 of a worker (stays on {old}), then warden reload {app}");
    let tmp = rel.join("current.tmp");
    let _ = std::fs::remove_file(&tmp);
    if let Err(e) = std::os::unix::fs::symlink(new, &tmp).and_then(|_| std::fs::rename(&tmp, &current)) {
        f.problems.push(format!("swapping the current symlink: {e}"));
        return;
    }
    if let Some((_, pid)) = status(cx, app).and_then(|st| running_worker(cx, &st)) {
        kill9(cx, pid);
    }
    if let Err(why) = cx.fleet.wait_ready(cx.bound) {
        f.problems.push(format!("not ready after the crash: {}", why.join("; ")));
    }
    if let Some(st) = status(cx, app) {
        let wrong: Vec<_> = cwds(&st).into_iter().filter(|(_, c)| *c != old_real).collect();
        if !wrong.is_empty() {
            f.problems.push(format!(
                "release pinning: after the symlink swap and a crash, these run elsewhere than the pinned {old_real}: {wrong:?}"
            ));
        }
    }
    let r = run_op(cx, "reload", app);
    f.op_ms = Some(r.ms);
    if !r.ok() {
        f.problems.push(format!("warden reload {app}: {}", r.brief()));
    }
    recover(cx, f, Instant::now());
    if let Some(st) = status(cx, app) {
        let wrong: Vec<_> = cwds(&st).into_iter().filter(|(_, c)| *c != new_real).collect();
        if !wrong.is_empty() {
            f.problems.push(format!("after warden reload these still run outside {new_real}: {wrong:?}"));
        }
        if st["release"].as_str() != Some(new_real.as_str()) {
            f.problems.push(format!("status release is {} after the reload, expected {new_real}", st["release"]));
        }
    }
}

fn throw_thread(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| a.worker_mode) else { return };
    f.app = Some(spec.name.into());
    f.allow.killed = true;
    f.allow.planned = true;
    let from = Instant::now();
    match get(spec.port, "/throw") {
        Some(who) => {
            let who = who.trim().to_string();
            cx.sh.doom_thread(&who);
            f.detail = format!("an uncaught error in Worker {who}: the host is replaced");
        }
        None => f.problems.push("GET /throw got no answer".into()),
    }
    recover(cx, f, from);
}

// ----------------------------------------------------------------- logs

fn direct_bytes(logs: &Path) -> u64 {
    std::fs::read_dir(logs)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("direct-out"))
                .filter_map(|e| e.metadata().ok().map(|m| m.len()))
                .sum()
        })
        .unwrap_or(0)
}

fn log_flood(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| a.chaos_app && !a.crashy) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    let mb = 10 + cx.rng.below(50);
    let who = get(spec.port, &format!("/flood?mb={mb}"));
    f.detail = format!("{mb} MB of log lines from one worker ({})", who.as_deref().unwrap_or("no answer").trim());
    // The flood runs in the background; recovery counts from its expected end.
    sleep_s(cx.rng.range(3.0, 6.0));
    recover(cx, f, Instant::now());
    if spec.direct {
        // 2 workers, keep = 1: the file and one rotated copy each, at most
        // max_size + max(max_size, 1 MiB) long.
        let limit = 2 * 2 * (256 * 1024 + 1024 * 1024);
        let used = direct_bytes(&cx.fleet.logs);
        if used > limit {
            f.problems.push(format!("direct output files hold {used} bytes, above the rotation bound {limit}"));
        }
    }
    if let Some(st) = status(cx, app) {
        f.detail += &format!("; log_lines_dropped={}", st["log_lines_dropped"]);
    }
}

fn disk_full(cx: &mut Ctx, f: &mut Fault) {
    if !cx.fleet.tmpfs {
        f.skipped = Some("no tmpfs for the log directory (needs root and a mount namespace)".into());
        return;
    }
    use std::io::Write;
    let filler = cx.fleet.logs.join("filler");
    let chunk = vec![b'x'; 1 << 20];
    let mut written = 0u64;
    match std::fs::File::create(&filler) {
        Ok(mut file) => {
            while file.write_all(&chunk).is_ok() {
                written += chunk.len() as u64;
            }
            // Fill the last partial MiB too.
            while file.write_all(&chunk[..4096]).is_ok() {
                written += 4096;
            }
        }
        Err(e) => f.problems.push(format!("creating {}: {e}", filler.display())),
    }
    let hold = cx.rng.range(3.0, 6.0);
    f.detail =
        format!("log directory full ({} MB filler) for {hold:.1}s while api-bun and direct flood", written >> 20);
    let mut targets = Vec::new();
    for a in cx.fleet.apps.iter().filter(|a| a.chaos_app && !a.crashy && !a.worker_mode) {
        targets.push(a.port);
    }
    for p in targets {
        get(p, "/flood?mb=5");
    }
    sleep_s(hold);
    let list = cx.fleet.list();
    if let Err(e) = list {
        f.problems.push(format!("while the disk was full: {e}"));
    }
    let _ = std::fs::remove_file(&filler);
    recover(cx, f, Instant::now());
}

fn crash_loop(cx: &mut Ctx, f: &mut Fault) {
    let apps = cx.fleet.apps.clone();
    let Some(spec) = pick(cx, &apps, |a| a.crashy) else { return };
    let app = spec.name;
    f.app = Some(app.into());
    f.allow = Allow { killed: true, planned: true, down: true };
    let flag = cx.fleet.crash_flag.clone();
    if let Err(e) = std::fs::write(&flag, "crash") {
        f.problems.push(format!("creating {}: {e}", flag.display()));
        return;
    }
    f.detail = "the app exits 1 at start: kill -9, crash loop to FAILED, then warden reset".into();
    if let Some((_, pid)) = status(cx, app).and_then(|st| running_worker(cx, &st)) {
        kill9(cx, pid);
    }
    let t0 = Instant::now();
    let mut failed = false;
    while t0.elapsed() < Duration::from_secs(30) {
        if status(cx, app).is_some_and(|st| st["workers"][0]["state"] == "FAILED") {
            failed = true;
            break;
        }
        sleep_s(0.2);
    }
    if !failed {
        f.problems.push(format!("{app} did not reach FAILED within 30s of crash-looping"));
    }
    let _ = std::fs::remove_file(&flag);
    let r = cx.fleet.cli(&["reset", app], Duration::from_secs(30));
    if !r.ok() {
        f.problems.push(format!("warden reset {app}: {}", r.brief()));
    }
    recover(cx, f, Instant::now());
}

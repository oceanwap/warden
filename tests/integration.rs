//! End-to-end tests: run the real `warden` binary against real Bun processes.
//! Skipped (with a message) when `bun` is not on PATH.

use serde_json::Value;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_warden");

fn fixture(name: &str) -> String {
    format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn have_bun() -> bool {
    let ok = Command::new("bun").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: bun not found on PATH");
    }
    ok
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// GET over a fresh connection; body on 2xx.
fn get(port: u16, path: &str) -> Option<String> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
    let mut buf = String::new();
    s.read_to_string(&mut buf).ok()?;
    if !buf.starts_with("HTTP/1.1 2") {
        return None;
    }
    buf.split_once("\r\n\r\n").map(|(_, b)| b.to_string())
}

struct Warden {
    child: Child,
    cfg: PathBuf,
    dir: PathBuf,
    port: u16,
}

impl Warden {
    fn start(name: &str, port: u16, toml: &str) -> Warden {
        Self::start_env(name, port, toml, &[])
    }

    fn start_env(name: &str, port: u16, toml: &str, env: &[(&str, &str)]) -> Warden {
        Self::start_opts(name, port, toml, env, false)
    }

    /// `stall_stdout`: Warden's stdout is a pipe nobody reads (a stuck log
    /// consumer); stderr still goes to the log file.
    fn start_opts(name: &str, port: u16, toml: &str, env: &[(&str, &str)], stall_stdout: bool) -> Warden {
        let dir = std::env::temp_dir().join(format!("warden-it-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("warden.toml");
        let text = format!("{toml}\n[control]\nsocket = \"{}\"\n", dir.join("w.sock").display());
        std::fs::write(&cfg, text).unwrap();
        let log = std::fs::File::create(dir.join("warden.log")).unwrap();
        let child = Command::new(BIN)
            .args(["start", "-c"])
            .arg(&cfg)
            .envs(env.iter().copied())
            .stdout(if stall_stdout { Stdio::piped() } else { Stdio::from(log.try_clone().unwrap()) })
            .stderr(log)
            .spawn()
            .unwrap();
        Warden { child, cfg, dir, port }
    }

    fn cli(&self, args: &[&str]) -> (i32, String) {
        let out = Command::new(BIN).args(args).arg("-c").arg(&self.cfg).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    }

    fn status(&self) -> Option<Value> {
        let (code, out) = self.cli(&["status", "--json"]);
        if code != 0 {
            return None;
        }
        serde_json::from_str(&out).ok()
    }

    fn wait_for(&self, what: &str, timeout: Duration, f: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        let mut last = None;
        while start.elapsed() < timeout {
            if let Some(s) = self.status() {
                if f(&s) {
                    return s;
                }
                last = Some(s);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}; last status: {last:#?}\nlog:\n{}", self.log());
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.join("warden.log")).unwrap_or_default()
    }

    /// Wait until the log file contains `needle` (the writer thread is async).
    fn wait_log(&self, needle: &str, timeout: Duration) -> String {
        let t0 = Instant::now();
        loop {
            let log = self.log();
            if log.contains(needle) {
                return log;
            }
            if t0.elapsed() > timeout {
                panic!("log never contained {needle:?} (waited {timeout:?}):\n{log}");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn socket(&self) -> PathBuf {
        self.dir.join("w.sock")
    }

    /// One request straight over the control socket (no CLI process), timed.
    fn request(&self, req: &str) -> (Duration, String) {
        let t0 = Instant::now();
        let mut s = std::os::unix::net::UnixStream::connect(self.socket()).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // A server that refuses the connection may close it before the
        // request is written (EPIPE); its answer is still there to read.
        let _ = writeln!(s, "{req}");
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        (t0.elapsed(), out)
    }

    fn rss_kb(&self) -> u64 {
        let st = std::fs::read_to_string(format!("/proc/{}/status", self.child.id())).unwrap_or_default();
        st.lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next()?.parse().ok())
            .unwrap_or(0)
    }

    fn pids(s: &Value) -> Vec<u64> {
        s["workers"].as_array().unwrap().iter().filter_map(|w| w["pid"].as_u64()).collect()
    }

    fn signal(&self, sig: i32) {
        unsafe { libc::kill(self.child.id() as i32, sig) };
    }

    /// SIGTERM and wait; returns exit code and how long it took.
    fn terminate(&mut self, timeout: Duration) -> (Option<i32>, Duration) {
        let t0 = Instant::now();
        self.signal(libc::SIGTERM);
        while t0.elapsed() < timeout {
            if let Some(st) = self.child.try_wait().unwrap() {
                return (st.code(), t0.elapsed());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("warden did not exit within {timeout:?}\n{}", self.log());
    }
}

impl Drop for Warden {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            self.signal(libc::SIGTERM);
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn alive(pid: u64) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn ready(n: u64) -> impl Fn(&Value) -> bool {
    move |s| s["workers_ready"] == n
}

const T: Duration = Duration::from_secs(20);

/// Requests a rolling restart may lose: none with net.ipv4.tcp_migrate_req=1
/// (queued connections move to another worker when one closes its
/// listener); without it the kernel resets the few queued at that instant.
fn allowed_resets() -> usize {
    let on = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_migrate_req").is_ok_and(|v| v.trim() == "1");
    if on {
        0
    } else {
        eprintln!("note: net.ipv4.tcp_migrate_req=0, allowing up to 3 reset connections");
        3
    }
}

#[test]
fn process_mode_lifecycle() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start(
        "lifecycle",
        port,
        &format!(
            "[app]\nname = \"lifecycle\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 3\n[restart]\nbackoff_initial = 50\n[shutdown]\ngrace_period = 5\ndrain_ms = 100\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("3 ready workers", T, ready(3));
    let pids: HashSet<u64> = Warden::pids(&s).into_iter().collect();
    assert_eq!(pids.len(), 3);
    // No health path: workers open no private health socket (it would cost
    // each worker a second server).
    let socks: Vec<String> = std::fs::read_dir(&w.dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|n| n.contains(".h") && n.ends_with(".sock"))
        .collect();
    assert!(socks.is_empty(), "unexpected health sockets: {socks:?}");

    // The kernel spreads fresh connections over every worker.
    let mut seen = HashSet::new();
    for _ in 0..120 {
        let who = get(w.port, "/whoami").expect("request failed");
        seen.insert(who.split(':').next().unwrap().parse::<u64>().unwrap());
    }
    assert_eq!(seen, pids, "requests should reach all workers");

    // Crash recovery: kill -9 worker 2, it comes back with a new pid.
    let victim = s["workers"][1]["pid"].as_u64().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let s = w.wait_for("worker 2 restarted", T, |s| {
        let w2 = &s["workers"][1];
        w2["state"] == "RUNNING" && w2["pid"].as_u64() != Some(victim) && w2["crashes"] == 1
    });
    assert_eq!(s["workers"][1]["restarts"], 1);
    assert!(s["workers"][1]["last_exit"].as_str().unwrap().contains("SIGKILL"));
    assert_eq!(s["workers"][0]["restarts"], 0, "other workers untouched");

    // Rolling reload while a client keeps sending requests.
    let before: HashSet<u64> = Warden::pids(&s).into_iter().collect();
    let stop = Arc::new(AtomicBool::new(false));
    let (ok, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let client = {
        let (stop, ok, fail) = (stop.clone(), ok.clone(), fail.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match get(port, "/whoami") {
                    Some(_) => ok.fetch_add(1, Ordering::Relaxed),
                    None => fail.fetch_add(1, Ordering::Relaxed),
                };
            }
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    w.wait_for("reload done", T, |s| {
        s["reloading"] == false && s["workers_ready"] == 3 && Warden::pids(s).iter().all(|p| !before.contains(p))
    });
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    let (ok, fail) = (ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed));
    eprintln!("reload under load: {ok} ok, {fail} failed");
    assert!(ok > 50);
    // Fresh connection per request: a SYN can still land in a closing
    // listener's accept queue unless net.ipv4.tcp_migrate_req=1.
    assert!(fail * 100 <= ok, "too many failures during reload: {fail}/{ok}");
    for p in &before {
        assert!(!alive(*p), "old worker {p} still running after reload");
    }

    // Scale down and up.
    assert_eq!(w.cli(&["scale", "2"]).0, 0);
    w.wait_for("2 workers", T, |s| s["workers"].as_array().unwrap().len() == 2 && s["workers_ready"] == 2);
    assert_eq!(w.cli(&["scale", "3"]).0, 0);
    let s = w.wait_for("3 workers again", T, ready(3));

    // Graceful shutdown: exit 0, no workers left behind.
    let last: Vec<u64> = Warden::pids(&s);
    let (code, took) = w.terminate(Duration::from_secs(8));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(4), "shutdown took {took:?}");
    std::thread::sleep(Duration::from_millis(100));
    for p in last {
        assert!(!alive(p), "worker {p} survived shutdown");
    }
    assert!(!w.dir.join("w.sock").exists(), "control socket not cleaned up");
}

#[test]
fn crash_loop_ends_in_failed() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "crashloop",
        port,
        &format!(
            "[app]\nname = \"crashloop\"\nargs = [\"{}\"]\nport = {port}\nenv = {{ FIXTURE_EXIT = \"1\" }}\n[restart]\nmax_restarts = 3\nbackoff_initial = 20\nbackoff_max = 100\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("FAILED", T, |s| s["workers"][0]["state"] == "FAILED");
    assert_eq!(s["workers"][0]["restarts"], 3);
    assert_eq!(s["workers"][0]["crashes"], 4);
    assert_eq!(s["workers"][0]["last_exit"], "exit code 1");
    assert!(w.log().contains("too many restarts"));
    // The supervisor stays up; a manual restart is attempted, waited for, and
    // reported as failed (exit 1) because the app still crashes.
    let (code, out) = w.cli(&["restart", "1"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("exited before listening"), "{out}");
    w.wait_for("FAILED again", T, |s| s["workers"][0]["state"] == "FAILED" && s["workers"][0]["crashes"] == 8);
}

#[test]
fn not_ready_in_time_is_a_crash() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "notready",
        port,
        &format!(
            "[app]\nname = \"notready\"\nargs = [\"{}\"]\nport = {port}\nenv = {{ FIXTURE_NO_LISTEN = \"1\" }}\n[workers]\nready_timeout = 1\n[restart]\nmax_restarts = 1\nbackoff_initial = 20\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("FAILED", T, |s| s["workers"][0]["state"] == "FAILED");
    assert_eq!(s["workers"][0]["last_exit"], "not ready in time");
    assert!(w.log().contains("worker not ready in time"));
}

#[test]
fn stuck_worker_is_killed_after_grace_period() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start(
        "stuck",
        port,
        &format!(
            "[app]\nname = \"stuck\"\nargs = [\"{}\"]\nport = {port}\nshim = false\nenv = {{ FIXTURE_IGNORE_TERM = \"1\" }}\n[shutdown]\ngrace_period = 1\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("ready", T, ready(1));
    let pid = Warden::pids(&s)[0];
    let (code, took) = w.terminate(Duration::from_secs(6));
    assert_eq!(code, Some(0));
    assert!(took >= Duration::from_millis(900), "exited before the grace period: {took:?}");
    assert!(took < Duration::from_secs(4), "took {took:?}");
    assert!(!alive(pid));
    assert!(w.log().contains("SIGKILL"));
}

#[test]
fn shim_lets_node_http_share_the_port() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "nodehttp",
        port,
        &format!(
            "[app]\nname = \"nodehttp\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n",
            fixture("node_http.mjs")
        ),
    );
    let s = w.wait_for("2 ready", T, ready(2));
    let pids: HashSet<u64> = Warden::pids(&s).into_iter().collect();
    let mut seen = HashSet::new();
    for _ in 0..80 {
        seen.insert(get(port, "/").expect("request failed").parse::<u64>().unwrap());
    }
    assert_eq!(seen, pids, "node:http servers must share the port via the shim");
}

#[test]
fn offset_ports() {
    if !have_bun() {
        return;
    }
    // Two consecutive free ports.
    let port = loop {
        let p = free_port();
        if p < 65000 && std::net::TcpListener::bind(("127.0.0.1", p + 1)).is_ok() {
            break p;
        }
    };
    let w = Warden::start(
        "offset",
        port,
        &format!(
            "[app]\nname = \"offset\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\nport_strategy = \"offset\"\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("2 ready", T, ready(2));
    let pids = Warden::pids(&s);
    let a = get(port, "/whoami").unwrap();
    let b = get(port + 1, "/whoami").unwrap();
    assert!(a.starts_with(&format!("{}:", pids[0])), "{a}");
    assert!(b.starts_with(&format!("{}:", pids[1])), "{b}");
    // Reload with offset ports is stop-then-start per worker.
    assert_eq!(w.cli(&["reload"]).0, 0);
    w.wait_for("reloaded", T, |s| {
        s["reloading"] == false && s["workers_ready"] == 2 && Warden::pids(s).iter().all(|p| !pids.contains(p))
    });
}

#[test]
fn worker_mode_threads_and_recovery() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "threads",
        port,
        &format!(
            "[app]\nname = \"threads\"\nentry = \"{}\"\nport = {port}\n[workers]\ncount = 3\nmode = \"worker\"\n[restart]\nbackoff_initial = 50\n[shutdown]\ndrain_ms = 100\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("3 ready threads", T, ready(3));
    let host = s["host"]["pid"].as_u64().unwrap();
    assert!(Warden::pids(&s).iter().all(|p| *p == host), "all workers live in one process");

    let mut threads = HashSet::new();
    for _ in 0..120 {
        let who = get(port, "/whoami").expect("request failed");
        let (pid, tid) = who.split_once(':').unwrap();
        assert_eq!(pid.parse::<u64>().unwrap(), host);
        threads.insert(tid.to_string());
    }
    assert_eq!(threads.len(), 3, "requests should reach all Workers: {threads:?}");

    // One Worker throws: Warden replaces the whole host process.
    let _ = get(port, "/throw");
    let s = w.wait_for("host replaced", T, |s| {
        s["host"]["pid"].as_u64().is_some_and(|p| p != host) && s["workers_ready"] == 3
    });
    assert_eq!(s["host"]["restarts"], 1);
    assert!(w.log().contains("worker thread crashed"));
    std::thread::sleep(Duration::from_millis(500));
    assert!(!alive(host), "old host should have been drained and stopped");
    // No black-holed connections afterwards.
    for _ in 0..60 {
        assert!(get(port, "/whoami").is_some());
    }
}

#[test]
fn cli_errors() {
    let dir = std::env::temp_dir().join(format!("warden-it-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("bad.toml");
    std::fs::write(&cfg, "[app]\nname = \"x\"\n[workers]\ncount = 0\n").unwrap();
    let out = Command::new(BIN).args(["check", "-c"]).arg(&cfg).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("workers.count"));

    let out = Command::new(BIN)
        .args(["status", "--socket"])
        .arg(dir.join("nobody.sock"))
        .stdout(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Is it running?"));

    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("reload"));
}

// ------------------------------------------------------------ safe rollouts

fn gated(name: &str, port: u16, count: usize, extra: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nargs = [\"{}\"]\nport = {port}\n{extra}\n[workers]\ncount = {count}\n\
         [health]\npath = \"/health\"\n[reload]\nhealth_passes = 2\nhealth_interval_ms = 100\ncanary_soak = 1\n\
         [restart]\nbackoff_initial = 50\n[shutdown]\ndrain_ms = 100\n",
        fixture("app.ts")
    )
}

fn pid_set(s: &Value) -> HashSet<u64> {
    Warden::pids(s).into_iter().collect()
}

#[test]
fn safe_reload_replaces_every_worker_through_the_gates() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("safe-ok", port, &gated("safe-ok", port, 3, ""));
    let before = pid_set(&w.wait_for("ready", T, ready(3)));
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("(canary)"), "{out}");
    assert!(out.contains("safe-reload complete: 3 worker(s) replaced"), "{out}");
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before), "every worker must be new");
    assert_eq!(s["last_rollout"]["ok"], true);
    assert_eq!(s["workers_ready"], 3);
}

#[test]
fn safe_reload_rolls_back_a_bad_canary() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("safe-bad", port, &gated("safe-bad", port, 3, ""));
    let before = pid_set(&w.wait_for("ready", T, ready(3)));
    let good = std::fs::read_to_string(&w.cfg).unwrap();

    // Deploy a version whose /health answers 503: the canary never passes.
    std::fs::write(&w.cfg, good.replace("[workers]", "env = { FIXTURE_HEALTH_FAIL = \"1\" }\n[workers]")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Rolled back"), "{out}");
    let s = w.status().unwrap();
    assert_eq!(pid_set(&s), before, "old workers must be untouched");
    assert_eq!(s["workers_ready"], 3);
    assert_eq!(s["last_rollout"]["ok"], false);

    // A version that dies during the canary soak is rolled back too.
    std::fs::write(
        &w.cfg,
        good.replace("[workers]", "env = { FIXTURE_EXIT_AFTER = \"600\" }\n[workers]")
            .replace("canary_soak = 1", "canary_soak = 3"),
    )
    .unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("canary exited"), "{out}");
    assert_eq!(pid_set(&w.status().unwrap()), before);

    // Broken config and missing files are refused before anything is touched.
    std::fs::write(&w.cfg, format!("{good}\n[workers")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1);
    assert!(out.contains("config error, nothing was changed"), "{out}");
    std::fs::write(&w.cfg, good.replace("app.ts", "missing.ts")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1);
    assert!(out.contains("not found"), "{out}");

    // A failing verify_command blocks the rollout too.
    std::fs::write(
        &w.cfg,
        good.replace("[reload]", "[reload]\nverify_command = \"test -S \\\"$WARDEN_WORKER_SOCKET\\\" && exit 7\""),
    )
    .unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("verify_command failed: exit 7"), "{out}");
    assert_eq!(pid_set(&w.status().unwrap()), before);

    // And the good version still deploys afterwards.
    std::fs::write(&w.cfg, &good).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 0, "{out}");
    assert!(pid_set(&w.status().unwrap()).is_disjoint(&before));
}

#[test]
fn unhealthy_worker_is_replaced_gracefully() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("sick", port, 1, "")
        .replace("[health]\n", "[health]\nenabled = true\ninterval = 1\nfailure_threshold = 2\n");
    let w = Warden::start("sick", port, &cfg);
    let before = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let _ = get(port, "/sick");
    let s = w.wait_for("replaced", T, |s| {
        s["workers"][0]["pid"].as_u64().is_some_and(|p| p != before) && s["rollout"].is_null()
    });
    assert_eq!(s["workers"][0]["crashes"], 0, "a graceful replacement is not a crash");
    assert_eq!(s["last_rollout"]["kind"], "replace");
    assert!(w.log().contains("worker unhealthy"));
    assert!(get(port, "/whoami").is_some());
}

#[test]
fn watchdog_kills_a_hung_worker() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("hung", port, 1, "").replace("[restart]", "[watchdog]\ntimeout = 2\n[restart]");
    let w = Warden::start("hung", port, &cfg);
    let before = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    std::thread::sleep(Duration::from_millis(1500)); // let heartbeats arm the watchdog
    let _ = get(port, "/hang");
    let s = w.wait_for("hung worker restarted", T, |s| {
        s["workers"][0]["pid"].as_u64().is_some_and(|p| p != before) && s["workers"][0]["state"] == "RUNNING"
    });
    assert!(s["workers"][0]["last_exit"].as_str().unwrap().contains("hung"), "{s:#?}");
    assert!(!alive(before));
}

#[test]
fn memory_and_lifetime_recycling() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("recycle", port, 1, "").replace("[restart]", "[limits]\nmax_memory = 150\n[restart]");
    let w = Warden::start("recycle", port, &cfg);
    let before = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let _ = get(port, "/leak");
    let s = w.wait_for("leaky worker recycled", Duration::from_secs(40), |s| {
        s["workers"][0]["pid"].as_u64().is_some_and(|p| p != before) && s["rollout"].is_null()
    });
    assert_eq!(s["workers"][0]["crashes"], 0);
    assert!(s["last_rollout"]["message"].as_str().unwrap().contains("max_memory"), "{s:#?}");

    let port = free_port();
    let cfg = gated("lifetime", port, 2, "").replace("[restart]", "[limits]\nmax_lifetime = 3\n[restart]");
    let w = Warden::start("lifetime", port, &cfg);
    let first = pid_set(&w.wait_for("ready", T, ready(2)));
    let s = w.wait_for("both recycled", T, |s| pid_set(s).is_disjoint(&first) && s["workers_ready"] == 2);
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["crashes"] == 0));
}

#[test]
fn failed_worker_is_retried_after_cooldown() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "cooldown",
        port,
        &format!(
            "[app]\nname = \"cooldown\"\nargs = [\"{}\"]\nport = {port}\nenv = {{ FIXTURE_EXIT = \"1\" }}\n[restart]\nmax_restarts = 1\nbackoff_initial = 20\nfailed_cooldown = 2\n",
            fixture("app.ts")
        ),
    );
    w.wait_for("FAILED", T, |s| s["workers"][0]["state"] == "FAILED" && s["workers"][0]["crashes"] == 2);
    w.wait_for("retried after cooldown", T, |s| s["workers"][0]["crashes"].as_u64() >= Some(4));
    assert!(w.log().contains("retrying failed worker after cooldown"));
}

// ------------------------------------------------- review regression tests

/// LISTEN sockets on `port`, system-wide.
fn listeners(port: u16) -> usize {
    let hex = format!(":{port:04X}");
    ["/proc/net/tcp", "/proc/net/tcp6"]
        .iter()
        .filter_map(|t| std::fs::read_to_string(t).ok())
        .flat_map(|t| t.lines().skip(1).map(str::to_string).collect::<Vec<_>>())
        .filter(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            f.len() > 3 && f[3] == "0A" && f[1].ends_with(&hex)
        })
        .count()
}

#[test]
fn old_worker_dying_mid_rollout_never_leaves_the_slot_empty() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("adopt", port, 1, "").replace("health_passes = 2", "health_passes = 50");
    let w = Warden::start("adopt", port, &cfg);
    let old = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let good = std::fs::read_to_string(&w.cfg).unwrap();
    // A bad release (503), and the old worker dies while the new one is being checked.
    std::fs::write(&w.cfg, good.replace("[workers]", "env = { FIXTURE_HEALTH_FAIL = \"1\" }\n[workers]")).unwrap();
    assert_eq!(w.cli(&["reload", "--no-wait"]).0, 0);
    w.wait_for("verifying", T, |s| s["rollout"]["phase"].as_str().is_some_and(|p| p.contains("health checks")));
    unsafe { libc::kill(old as i32, libc::SIGKILL) };
    let s = w.wait_for("rollout failed", T, |s| s["last_rollout"]["ok"] == false);
    assert!(s["last_rollout"]["message"].as_str().unwrap().contains("being restarted"), "{s:#?}");
    // The slot is restarted with the previous (restored) config: healthy again,
    // and exactly one process serves the port.
    w.wait_for("running again", T, |s| s["workers"][0]["state"] == "RUNNING");
    std::thread::sleep(Duration::from_millis(300));
    for _ in 0..20 {
        assert_eq!(get(port, "/health").as_deref(), Some("ok"));
    }
    assert_eq!(listeners(port), 1, "no orphan worker left serving");
}

#[test]
fn rejected_config_is_not_used_by_later_restarts() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("restore", port, &gated("restore", port, 1, ""));
    let old = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let good = std::fs::read_to_string(&w.cfg).unwrap();
    std::fs::write(&w.cfg, good.replace("app.ts", "missing.ts")).unwrap();
    assert_eq!(w.cli(&["safe-reload"]).0, 1);
    // A crash restart afterwards must use the config that was in effect.
    unsafe { libc::kill(old as i32, libc::SIGKILL) };
    w.wait_for("restarted on the old config", T, |s| {
        s["workers"][0]["state"] == "RUNNING" && s["workers"][0]["pid"].as_u64() != Some(old)
    });
}

#[test]
fn stop_after_restart_all_still_shuts_down() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("restartall", port, &gated("restartall", port, 2, ""));
    w.wait_for("ready", T, ready(2));
    assert_eq!(w.cli(&["restart", "--hard"]).0, 0);
    let (code, took) = w.terminate(Duration::from_secs(10));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(!w.log().contains("INFO  starting all workers"), "must not restart during shutdown");

    // `restart` then `stop`: stays stopped.
    let port = free_port();
    let w = Warden::start("restartstop", port, &gated("restartstop", port, 2, ""));
    w.wait_for("ready", T, ready(2));
    assert_eq!(w.cli(&["restart", "--hard"]).0, 0);
    assert_eq!(w.cli(&["stop"]).0, 0);
    std::thread::sleep(Duration::from_millis(1500));
    let s = w.status().unwrap();
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["state"] == "STOPPED"), "{s:#?}");
}

#[test]
fn reload_right_after_scale_down_spawns_nothing_extra() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    // Workers ignore SIGTERM, so worker 3 is still draining when the reload starts.
    let cfg = gated("scalereload", port, 3, "shim = false\nenv = { FIXTURE_IGNORE_TERM = \"1\" }")
        .replace("[shutdown]\n", "[shutdown]\ngrace_period = 2\n")
        // No shim: per-worker sockets don't exist, so gate on the app-level URL.
        .replace("path = \"/health\"", &format!("url = \"http://127.0.0.1:{port}/health\""));
    let w = Warden::start("scalereload", port, &cfg);
    w.wait_for("ready", T, ready(3));
    assert_eq!(w.cli(&["scale", "2"]).0, 0);
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.wait_for("2 workers", T, |s| s["workers"].as_array().unwrap().len() == 2 && s["workers_ready"] == 2);
    std::thread::sleep(Duration::from_secs(3)); // old workers' grace period
    assert_eq!(listeners(port), 2, "only the 2 supervised workers may listen: {s:#?}");
}

#[test]
fn worker_mode_canary_with_a_crashing_worker_is_rolled_back() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "wcanary",
        port,
        &format!(
            "[app]\nname = \"wcanary\"\nentry = \"{}\"\nport = {port}\n[workers]\ncount = 2\nmode = \"worker\"\n\
             [health]\npath = \"/health\"\n[reload]\nhealth_passes = 2\nhealth_interval_ms = 100\ncanary_soak = 3\n\
             [shutdown]\ndrain_ms = 100\n",
            fixture("app.ts")
        ),
    );
    let host = w.wait_for("ready", T, ready(2))["host"]["pid"].as_u64().unwrap();
    let good = std::fs::read_to_string(&w.cfg).unwrap();
    std::fs::write(&w.cfg, good.replace("[workers]", "env = { FIXTURE_THROW_AFTER = \"1200\" }\n[workers]")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("of the new host crashed"), "{out}");
    let s = w.status().unwrap();
    assert_eq!(s["host"]["pid"].as_u64(), Some(host), "old host keeps serving");
    assert_eq!(s["workers_ready"], 2);
}

#[test]
fn other_servers_in_the_app_are_not_mistaken_for_it() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("extra", port, &gated("extra", port, 1, "env = { FIXTURE_EXTRA_SERVER = \"1\" }"));
    let s = w.wait_for("ready", T, ready(1));
    // Readiness waited for the app's own port...
    assert!(get(port, "/whoami").is_some(), "{s:#?}");
    // ...and the private health socket belongs to the app server, so the gates pass.
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 0, "{out}");
}

// ------------------------------------------------------ v0.2 crash protection

#[test]
fn systemd_ready_and_watchdog_pings_stop_when_frozen() {
    if !have_bun() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("warden-it-notify-sock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("notify.sock");
    let sock = std::os::unix::net::UnixDatagram::bind(&path).unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    let recv_for = |d: Duration| {
        let (t0, mut msgs) = (Instant::now(), Vec::new());
        let mut buf = [0u8; 512];
        while t0.elapsed() < d {
            if let Ok(n) = sock.recv(&mut buf) {
                msgs.push(String::from_utf8_lossy(&buf[..n]).to_string());
            }
        }
        msgs
    };
    let port = free_port();
    let w = Warden::start_env(
        "notify",
        port,
        &gated("notify", port, 2, ""),
        &[("NOTIFY_SOCKET", path.to_str().unwrap()), ("WATCHDOG_USEC", "10000000")],
    );
    let first = recv_for(Duration::from_secs(4));
    assert!(first.iter().any(|m| m.starts_with("READY=1")), "{first:?}");
    let pings = first.iter().filter(|m| *m == "WATCHDOG=1").count();
    assert!(pings >= 2, "expected a ping per second: {first:?}");

    w.signal(libc::SIGSTOP);
    let frozen = recv_for(Duration::from_millis(3500));
    w.signal(libc::SIGCONT);
    assert!(!frozen.iter().any(|m| m == "WATCHDOG=1"), "no pings while frozen: {frozen:?}");
    let after = recv_for(Duration::from_secs(3));
    assert!(after.iter().filter(|m| *m == "WATCHDOG=1").count() >= 1, "pings resume: {after:?}");
    assert!(w.log().contains("event loop was blocked"));
}

fn fault_warden(name: &str, fault: &str, count: usize) -> (Warden, u16) {
    let port = free_port();
    let w = Warden::start_env(name, port, &gated(name, port, count, ""), &[("WARDEN_FAULT", fault)]);
    (w, port)
}

#[test]
fn a_panicking_waiter_task_restarts_its_worker() {
    if !have_bun() {
        return;
    }
    let (w, port) = fault_warden("fault-waiter", "waiter:1", 2);
    let s = w.wait_for("both ready after the injected fault", T, |s| {
        s["workers_ready"] == 2 && s["workers"].as_array().unwrap().iter().any(|x| x["crashes"] == 1)
    });
    let crashed = s["workers"].as_array().unwrap().iter().find(|x| x["crashes"] == 1).unwrap();
    assert!(crashed["last_exit"].as_str().unwrap().contains("internal error"), "{crashed:#?}");
    assert!(w.log().contains("Warden lost track of a worker"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listeners(port), 2, "exactly the two supervised workers listen");
}

#[test]
fn a_panicking_output_reader_restarts_its_worker() {
    if !have_bun() {
        return;
    }
    let (w, port) = fault_warden("fault-stdout", "stdout:1", 2);
    w.wait_for("both ready after the injected fault", T, |s| {
        s["workers_ready"] == 2 && s["workers"].as_array().unwrap().iter().any(|x| x["crashes"] == 1)
    });
    assert!(w.log().contains("output reader failed"));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listeners(port), 2);
}

#[test]
fn a_panicking_control_request_only_drops_that_request() {
    if !have_bun() {
        return;
    }
    let (w, _port) = fault_warden("fault-control", "control:1", 1);
    // The first request hits the fault; retry until the supervisor answers.
    let t0 = Instant::now();
    let mut first_failed = false;
    loop {
        let (code, _) = w.cli(&["status", "--json"]);
        if code == 0 {
            break;
        }
        first_failed = true;
        assert!(t0.elapsed() < T, "control socket never recovered");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(first_failed, "the injected fault should have failed one request");
    assert!(w.log().contains("request handler failed"));
    w.wait_for("still supervising", T, ready(1));
}

#[test]
fn a_panic_in_the_event_loop_exits_and_takes_workers_down() {
    if !have_bun() {
        return;
    }
    let (mut w, port) = fault_warden("fault-tick", "tick:4", 2);
    let t0 = Instant::now();
    let code = loop {
        if let Some(st) = w.child.try_wait().unwrap() {
            break st.code();
        }
        assert!(t0.elapsed() < T, "warden should have exited");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(code, Some(0));
    assert!(w.log().contains("warden panicked at"), "{}", w.log());
    // Workers drain and exit via the parent-death signal.
    let t1 = Instant::now();
    while listeners(port) > 0 {
        assert!(t1.elapsed() < Duration::from_secs(5), "workers outlived Warden");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn simple(name: &str, port: u16, count: usize, extra: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = {count}\n[restart]\nbackoff_initial = 50\n[shutdown]\ngrace_period = 5\ndrain_ms = 100\n{extra}",
        fixture("app.ts")
    )
}

/// CP5 / S12: the log consumer stops reading. Supervision, the control socket
/// and memory must be unaffected; dropped lines are counted.
#[test]
fn stalled_stdout_does_not_block_supervision() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start_opts("stalled", port, &simple("stalled", port, 2, ""), &[("FIXTURE_SPAM", "1")], true);
    w.wait_for("2 ready workers", T, ready(2));
    let s = w.wait_for("dropped log lines", T, |s| s["log_lines_dropped"].as_u64().unwrap_or(0) > 0);

    let mut lat: Vec<Duration> = (0..20)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            let (d, out) = w.request(r#"{"cmd":"status"}"#);
            assert!(out.contains("\"ok\":true"), "{out}");
            d
        })
        .collect();
    lat.sort();
    eprintln!("status latency with a stalled stdout: p50 {:?}, p95 {:?}, max {:?}", lat[10], lat[18], lat[19]);
    assert!(lat[10] < Duration::from_millis(100), "p50 {:?}", lat[10]);
    // p95, not max: a shared CI runner can stall any process once for most
    // of a second (seen: one 825 ms sample in 20, p50 1 ms); blocking by the
    // log path would slow most samples.
    assert!(lat[18] < Duration::from_millis(500), "p95 {:?}, max {:?}", lat[18], lat[19]);

    // Crash handling still works.
    let victim = s["workers"][0]["pid"].as_u64().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    w.wait_for("worker 1 restarted", T, |s| {
        s["workers"][0]["state"] == "RUNNING" && s["workers"][0]["pid"].as_u64() != Some(victim)
    });

    // Memory stays bounded while output keeps being dropped.
    std::thread::sleep(Duration::from_secs(3));
    let rss = w.rss_kb();
    eprintln!("warden RSS with a stalled stdout: {rss} kB");
    assert!(rss > 0 && rss < 40 * 1024, "RSS {rss} kB");

    // Events survive in memory even though worker output floods the log.
    let (code, out) = w.cli(&["logs", "--events", "-n", "200"]);
    assert_eq!(code, 0);
    assert!(out.contains("worker crashed") || out.contains("worker exited"), "{out}");
    assert!(!out.contains(" OUT "), "--events must not show worker output");
    let (_, out) = w.cli(&["logs", "--worker", "2", "-n", "5"]);
    assert!(out.lines().all(|l| l.contains("worker=2")), "{out}");
}

/// CP6: a worker that floods fd 3 with messages and junk is rate-limited,
/// summarised in a bounded number of lines, and Warden's memory stays flat.
#[test]
fn ipc_flood_is_bounded() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start_env("ipcflood", port, &simple("ipcflood", port, 1, ""), &[("FIXTURE_IPC_FLOOD", "1")]);
    w.wait_for("1 ready worker", T, ready(1));
    std::thread::sleep(Duration::from_secs(4));
    let rss = w.rss_kb();
    eprintln!("warden RSS under an IPC flood: {rss} kB");
    assert!(rss > 0 && rss < 20 * 1024, "RSS {rss} kB");
    let (d, _) = w.request(r#"{"cmd":"status"}"#);
    assert!(d < Duration::from_millis(500), "status took {d:?}");
    let log = w.log();
    assert!(log.contains("worker floods Warden's IPC pipe"), "{log}");
    assert!(log.contains("worker sent invalid IPC messages"), "{log}");
    let summaries = log.matches("worker sent invalid IPC messages").count();
    assert!(summaries <= 2, "invalid IPC input must be summarised, not logged per line ({summaries} lines)");
    let s = w.status().unwrap();
    assert_eq!(s["workers"][0]["state"], "RUNNING");
}

/// CP6: idle and excess control connections can't wedge the control socket.
#[test]
fn control_connections_are_bounded() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("ctlcap", port, &simple("ctlcap", port, 1, ""));
    w.wait_for("1 ready worker", T, ready(1));

    // An idle client gets an answer after the request timeout.
    let t0 = Instant::now();
    let mut idle = std::os::unix::net::UnixStream::connect(w.socket()).unwrap();
    idle.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut out = String::new();
    let _ = idle.read_to_string(&mut out);
    assert!(out.contains("no request received"), "{out}");
    assert!(t0.elapsed() >= Duration::from_secs(4) && t0.elapsed() < Duration::from_secs(8), "{:?}", t0.elapsed());

    // 64 idle connections fill the slots: the 65th is refused, with a reason.
    let held: Vec<_> = (0..64).map(|_| std::os::unix::net::UnixStream::connect(w.socket()).unwrap()).collect();
    std::thread::sleep(Duration::from_millis(200));
    let (_, out) = w.request(r#"{"cmd":"status"}"#);
    assert!(out.contains("too many control connections"), "{out}");
    drop(held);
    // Slots free up as soon as those clients go away.
    w.wait_for("status works again", T, |_| true);
    w.wait_log("too many control connections; refusing new ones", Duration::from_secs(3));

    // Streams have their own budget (review R3): 32 subscriptions and
    // `logs -f` sessions, then the next is refused, and commands still
    // get through however many are open.
    let mut streams: Vec<Events> = (0..32).map(|_| Events::open(&w, r#"{"cmd":"subscribe"}"#)).collect();
    for e in &mut streams {
        assert_eq!(e.next()["type"], "hello");
    }
    let mut extra = Events::open(&w, r#"{"cmd":"subscribe"}"#);
    let refused = extra.next();
    assert!(refused["message"].as_str().unwrap_or("").contains("too many live streams"), "{refused}");
    let (_, out) = w.request(r#"{"cmd":"logs","lines":1,"follow":true}"#);
    assert!(out.contains("too many live streams"), "{out}");
    let (_, out) = w.request(r#"{"cmd":"status"}"#);
    assert!(out.contains(r#""ok":true"#), "status with 32 streams open: {out}");
    w.wait_log("too many live streams; refusing a new one", Duration::from_secs(3));
    drop(streams);
    let mut again = Events::open(&w, r#"{"cmd":"subscribe"}"#);
    assert_eq!(again.next()["type"], "hello", "a stream slot is free again");
}

/// A6: the log level can be changed at runtime and filters apply to `logs`.
#[test]
fn runtime_log_level() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("loglevel", port, &gated("loglevel", port, 1, ""));
    w.wait_for("1 ready worker", T, ready(1));
    let (code, out) = w.cli(&["log-level"]);
    assert_eq!(code, 0);
    assert!(out.contains("log level: info"), "{out}");
    let (code, out) = w.cli(&["log-level", "debug"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("log level: debug (was info"), "{out}");
    // A restart runs the gates, which log each check at DEBUG.
    let (code, out) = w.cli(&["restart", "1"]);
    assert_eq!(code, 0, "{out}");
    let (_, out) = w.cli(&["logs", "--events", "-n", "300"]);
    assert!(out.contains("DEBUG gate health check passed"), "{out}");
    assert!(out.contains("log level changed from=info to=debug"), "{out}");
    let (code, _) = w.cli(&["log-level", "loud"]);
    assert_eq!(code, 2);
    w.cli(&["log-level", "warn"]);
    let (code, out) = w.cli(&["restart", "1"]);
    assert_eq!(code, 0, "{out}");
    let (_, out) = w.cli(&["logs", "--events", "-n", "300"]);
    let after = out.rsplit("log level changed").next().unwrap_or("");
    assert!(!after.contains("DEBUG") && !after.contains("INFO "), "nothing below WARN after the change:\n{after}");
}

/// A PM2-style host: several apps, each with its own background supervisor,
/// driven only through `warden <command> <target>` (no -c).
struct Fleet {
    home: PathBuf,
}

impl Fleet {
    fn new(name: &str) -> Fleet {
        let home = std::env::temp_dir().join(format!("wf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Fleet { home }
    }

    fn cli(&self, args: &[&str]) -> (i32, String) {
        self.cli_env(args, &[])
    }

    fn cli_env(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        let out = Command::new(BIN)
            .args(args)
            .env("WARDEN_HOME", &self.home)
            .env("WARDEN_RUNTIME_DIR", self.home.join("run"))
            .env_remove("WARDEN_CONFIG")
            // No test leaves a wardend behind unless it asks for one.
            .env("WARDEN_NO_DAEMON", "1")
            .envs(env.iter().copied())
            .current_dir(&self.home)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    }

    fn ok(&self, args: &[&str]) -> String {
        let (code, out) = self.cli(args);
        assert_eq!(code, 0, "warden {} failed:\n{out}", args.join(" "));
        out
    }

    fn list(&self) -> Vec<Value> {
        let out = self.ok(&["list", "--json"]);
        serde_json::from_str::<Value>(&out).unwrap().as_array().unwrap().clone()
    }

    fn app(&self, name: &str) -> Value {
        self.list().into_iter().find(|a| a["app"] == name).unwrap_or(Value::Null)
    }

    fn pids(&self, name: &str) -> Vec<u64> {
        let a = self.app(name);
        a["status"]["workers"]
            .as_array()
            .map(|w| w.iter().filter_map(|x| x["pid"].as_u64()).collect())
            .unwrap_or_default()
    }

    fn wait(&self, what: &str, f: impl Fn(&Fleet) -> bool) {
        let t0 = Instant::now();
        while t0.elapsed() < T {
            if f(self) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("timed out waiting for {what}:\n{}", self.cli(&["list"]).1);
    }
}

impl Drop for Fleet {
    fn drop(&mut self) {
        let _ = self.cli(&["kill", "--yes"]);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
fn pm2_style_fleet_workflow() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("pm2");
    let (p1, p2) = (free_port(), free_port());
    let app = fixture("app.ts");

    // `pm2 start app.ts -i 2 --name api` habits: a config is written, the app runs in the background.
    let out = f.ok(&[
        "start",
        &app,
        "--name",
        "api",
        "-i",
        "2",
        "--port",
        &p1.to_string(),
        "--namespace",
        "backend",
        "--env",
        "API_TOKEN=s3cret",
        "--env",
        "NODE_ENV=production",
    ]);
    assert!(out.contains("api: online (2/2 workers ready)"), "{out}");
    assert!(f.home.join("api.toml").is_file());
    f.ok(&["start", &app, "--name", "web", "--port", &p2.to_string(), "--namespace", "backend"]);
    let (code, out) = f.cli(&["start", &app, "--name", "api"]);
    assert_eq!(code, 1, "a second app with the same name is refused: {out}");
    assert!(out.contains("already exists"), "{out}");
    assert!(get(p1, "/whoami").is_some() && get(p2, "/whoami").is_some());

    // list: one row per worker, both apps.
    let out = f.ok(&["list"]);
    assert_eq!(out.lines().filter(|l| l.starts_with("api ") && l.contains("RUNNING")).count(), 2, "{out}");
    assert_eq!(out.lines().filter(|l| l.starts_with("web ") && l.contains("RUNNING")).count(), 1, "{out}");
    assert_eq!(f.list().len(), 2);

    // PM2 numeric ids are explained, not guessed.
    let (code, out) = f.cli(&["restart", "0"]);
    assert_eq!(code, 2);
    assert!(out.contains("names apps, not numeric ids"), "{out}");
    let (code, out) = f.cli(&["restart"]);
    assert_eq!(code, 2);
    assert!(out.contains("which app?"), "{out}");

    // restart = rolling and gated: every api pid changes, web is untouched.
    let (api0, web0) = (f.pids("api"), f.pids("web"));
    f.ok(&["restart", "api"]);
    let api1 = f.pids("api");
    assert!(api1.iter().all(|p| !api0.contains(p)) && api1.len() == 2, "{api0:?} -> {api1:?}");
    assert_eq!(f.pids("web"), web0);
    // One worker.
    f.ok(&["restart", "api:2"]);
    let api2 = f.pids("api");
    assert_eq!(api2[0], api1[0]);
    assert_ne!(api2[1], api1[1]);

    // Namespaces as targets: stop both, start one.
    f.ok(&["stop", "backend"]);
    f.wait("both stopped", |f| {
        f.list().iter().all(|a| {
            a["status"]["stopped"] == true
                && a["status"]["workers"].as_array().unwrap().iter().all(|w| w["state"] == "STOPPED")
        })
    });
    assert!(f.ok(&["list"]).contains("stopped"));
    let out = f.ok(&["start", "api"]);
    assert!(out.contains("online (2/2"), "{out}");

    // Scale relative to now, save, kill everything, resurrect.
    f.ok(&["scale", "api", "+1"]);
    f.wait("3 api workers", |f| f.app("api")["status"]["workers_ready"] == 3);
    let out = f.ok(&["save"]);
    assert!(out.contains("api: saved (3 workers)") && out.contains("web: saved (1 workers, stopped)"), "{out}");
    f.ok(&["kill", "--yes"]);
    f.wait("all offline", |f| f.list().iter().all(|a| a["status"].is_null()));
    assert!(f.ok(&["list"]).contains("offline"));
    let out = f.ok(&["resurrect"]);
    assert!(out.contains("api: online (3/3 workers ready)"), "{out}");
    assert!(out.contains("web: stopped"), "{out}");

    // describe / env: secrets hidden unless asked.
    let out = f.ok(&["describe", "api"]);
    assert!(out.contains("namespace backend") && out.contains("restart") && out.contains("Worker"), "{out}");
    assert!(!out.contains("s3cret") && out.contains("NODE_ENV=production"), "{out}");
    let out = f.ok(&["env", "api"]);
    assert!(out.contains("API_TOKEN=(hidden") && !out.contains("s3cret"), "{out}");
    assert!(f.ok(&["env", "api", "--show-secrets"]).contains("API_TOKEN=s3cret"));

    // logs, signal, reset.
    let out = f.ok(&["logs", "api", "--nostream", "--events", "--lines", "50"]);
    assert!(out.contains("worker ready"), "{out}");
    f.ok(&["start", "web"]);
    let out = f.ok(&["logs", "--nostream", "--events"]);
    assert!(out.lines().any(|l| l.starts_with("api ")) && out.lines().any(|l| l.starts_with("web ")), "{out}");
    let out = f.ok(&["signal", "SIGCONT", "api:1"]);
    assert!(out.contains("sent SIGCONT to pid"), "{out}");
    assert!(f.ok(&["reset", "api"]).contains("reset 3 worker(s)"));

    // The background supervisor writes its own rotated log file.
    let log = std::fs::read_to_string(f.home.join("state/logs/api.log")).unwrap_or_default();
    assert!(log.contains("starting application app=api"), "{log}");

    // delete: gone from the list, config kept aside.
    f.ok(&["delete", "web"]);
    assert!(f.home.join("deleted/web.toml").is_file());
    assert!(f.app("web").is_null());
    let (code, _) = f.cli(&["start", "web"]);
    assert_eq!(code, 2, "a deleted app is unknown");
}

fn have_node() -> bool {
    Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Node apps get the shim through `--import`: several workers share the
/// port (SO_REUSEPORT), each knows its NODE_APP_INSTANCE, and a rolling
/// restart under load drops nothing.
#[test]
fn node_workers_share_a_port_through_the_shim() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"nodeapp\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n\
         [health]\npath = \"/whoami\"\n[reload]\nhealth_passes = 1\nhealth_interval_ms = 100\n[shutdown]\ndrain_ms = 200\n",
        fixture("node_app.mjs")
    );
    let w = Warden::start("nodeapp", port, &cfg);
    w.wait_for("2 ready", T, ready(2));
    let mut seen = HashSet::new();
    for _ in 0..60 {
        seen.insert(get(port, "/whoami").expect("request failed"));
    }
    let instances: HashSet<String> = seen.iter().map(|s| s.split(':').nth(1).unwrap().to_string()).collect();
    assert_eq!(instances, HashSet::from(["0".to_string(), "1".to_string()]), "{seen:?}");

    let stop = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let client = {
        let (stop, fail, ok) = (stop.clone(), fail.clone(), ok.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if get(port, "/whoami").is_some() {
                    ok.fetch_add(1, Ordering::Relaxed);
                } else {
                    fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let before = pid_set(&w.status().unwrap());
    let (code, out) = w.cli(&["restart"]);
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    assert_eq!(code, 0, "{out}\n{}", w.log());
    let after = pid_set(&w.status().unwrap());
    assert!(before.is_disjoint(&after), "every worker replaced");
    eprintln!("node rolling restart: {} ok, {} failed", ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed));
    assert!(ok.load(Ordering::Relaxed) > 50);
    assert!(fail.load(Ordering::Relaxed) <= allowed_resets(), "requests failed during the rolling restart");
}

/// Apps written for PM2: readiness from process.send('ready'), graceful stop
/// on SIGINT.
#[test]
fn pm2_style_wait_ready_and_sigint() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"pm2app\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n\
         env = {{ FIXTURE_WAIT_READY = \"1500\", FIXTURE_SIGINT_ONLY = \"1\" }}\n\
         [workers]\ncount = 1\nwait_ready = true\n[shutdown]\nsignal = \"SIGINT\"\ngrace_period = 10\n",
        fixture("node_app.mjs")
    );
    let t0 = Instant::now();
    let mut w = Warden::start("pm2app", port, &cfg);
    // Listening early is not enough: ready only after process.send('ready').
    w.wait_for("listening", T, |_| get(port, "/whoami").is_some());
    assert_eq!(w.status().unwrap()["workers_ready"], 0, "not ready before process.send('ready')");
    w.wait_for("ready", T, ready(1));
    assert!(t0.elapsed() >= Duration::from_millis(1400), "{:?}", t0.elapsed());
    let (code, took) = w.terminate(Duration::from_secs(8));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(5), "stopped by SIGINT, not by the SIGKILL after grace: {took:?}");
    let log = w.log();
    assert!(log.contains("got SIGINT"), "{log}");
    assert!(!log.contains("ignoring SIGTERM"), "{log}");
}

/// Any long-running command, not just HTTP apps: no port means ready after
/// min_uptime; exit codes in stop_exit_codes mean "done".
#[test]
fn plain_processes_min_uptime_and_stop_exit_codes() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = "[app]\nname = \"plain\"\ncommand = \"sh\"\nargs = [\"-c\", \"echo started; exec sleep 300\"]\n\
               [workers]\ncount = 2\nmin_uptime = 700\n";
    let w = Warden::start("plain", port, cfg);
    let t0 = Instant::now();
    w.wait_for("2 ready", T, ready(2));
    assert!(t0.elapsed() >= Duration::from_millis(600), "ready only after min_uptime: {:?}", t0.elapsed());
    let before = pid_set(&w.status().unwrap());
    let (code, out) = w.cli(&["restart"]);
    assert_eq!(code, 0, "{out}");
    assert!(before.is_disjoint(&pid_set(&w.status().unwrap())));
    assert!(w.log().contains("OUT   worker=1 stdout: started"));

    let port = free_port();
    let cfg = "[app]\nname = \"oneshot\"\ncommand = \"sh\"\nargs = [\"-c\", \"sleep 0.3; exit 0\"]\n\
               [workers]\nmin_uptime = 100\n[restart]\nstop_exit_codes = [0]\n";
    let w = Warden::start("oneshot", port, cfg);
    let s = w.wait_for("done", T, |s| s["workers"][0]["state"] == "STOPPED");
    assert!(s["workers"][0]["last_exit"].as_str().unwrap().contains("(done)"), "{s:#?}");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(w.status().unwrap()["workers"][0]["restarts"], 0, "not restarted");
}

/// Worker output under a flood: by default Warden keeps its line budget and
/// counts the rest as dropped; with `max_lines_per_sec = 0` it keeps every
/// line, in order, by slowing the writer down instead (backpressure).
#[test]
fn output_flood_budget_and_keep_all() {
    const N: u32 = 300_000;
    for keep_all in [false, true] {
        let port = free_port();
        let name = if keep_all { "flood-all" } else { "flood-budget" };
        let dir = std::env::temp_dir().join(format!("warden-it-{name}-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let out = dir.join("out.log");
        let cfg = format!(
            "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"seq 1 {N}; exec sleep 300\"]\n\
             [workers]\nmin_uptime = 100\n[logging]\nout_file = \"{}\"\n{}",
            out.display(),
            if keep_all { "max_lines_per_sec = 0\n" } else { "" }
        );
        let w = Warden::start(name, port, &cfg);
        let lines = || std::fs::read_to_string(&out).map(|t| t.lines().count()).unwrap_or(0);
        let t0 = Instant::now();
        let mut last = (usize::MAX, Instant::now());
        // Wait until the file stops growing.
        while t0.elapsed() < T {
            let n = lines();
            if n == last.0 && last.1.elapsed() > Duration::from_millis(700) && n > 0 {
                break;
            }
            if n != last.0 {
                last = (n, Instant::now());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = std::fs::read_to_string(&out).unwrap();
        let got: Vec<u32> = text.lines().map(|l| l.parse().unwrap()).collect();
        if keep_all {
            assert_eq!(got.len(), N as usize, "every line kept");
            assert!(got.iter().enumerate().all(|(i, v)| *v == i as u32 + 1), "in order");
        } else {
            // At most the 10,000-line budget (less if the queue to the
            // writer filled first); the rest counted as dropped.
            assert!(got.len() > 1000 && got.len() <= 10_000, "budget applied: {} lines", got.len());
            assert!(got.windows(2).all(|p| p[0] < p[1]), "kept lines stay in order");
            w.wait_log("lines dropped", T);
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `warden start` runs anything PM2 would: a command line, a program on
/// PATH, a script by extension, an executable.
#[test]
fn start_runs_any_command() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("anycmd");
    let port = free_port();
    // A command line (PM2: pm2 start "python3 -m http.server 8000" --name files).
    let out = f.ok(&["start", "sleep 300", "--name", "sleeper"]);
    assert!(out.contains("sleeper: online (1/1"), "{out}");
    // A program on PATH with args after --.
    let out = f.ok(&[
        "start",
        "python3",
        "--name",
        "files",
        "--port",
        &port.to_string(),
        "--",
        "-m",
        "http.server",
        &port.to_string(),
        "--bind",
        "127.0.0.1",
    ]);
    assert!(out.contains("files: online (1/1"), "{out}");
    let mut conn = TcpStream::connect(("127.0.0.1", port)).expect("python's http.server listens");
    write!(conn, "GET / HTTP/1.0\r\n\r\n").unwrap();
    let mut resp = String::new();
    let _ = conn.read_to_string(&mut resp);
    assert!(resp.starts_with("HTTP/1.0 200"), "{resp}");
    // A script picked by extension, and an executable with a shebang.
    let py = f.home.join("tick.py");
    std::fs::write(&py, "import time\nprint('tick', flush=True)\ntime.sleep(300)\n").unwrap();
    f.ok(&["start", py.to_str().unwrap(), "--name", "ticker", "--kill-signal", "SIGINT"]);
    let sh = f.home.join("loop.sh");
    std::fs::write(&sh, "#!/bin/sh\necho looping\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&sh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    f.ok(&["start", sh.to_str().unwrap(), "--name", "looper", "-o", f.home.join("looper-out.log").to_str().unwrap()]);
    let list = f.ok(&["list"]);
    for app in ["sleeper", "files", "ticker", "looper"] {
        assert!(list.lines().any(|l| l.starts_with(app) && l.contains("RUNNING")), "{app}:\n{list}");
    }
    let cfg = std::fs::read_to_string(f.home.join("ticker.toml")).unwrap();
    assert!(cfg.contains("command = \"python3\"") && cfg.contains("signal = \"SIGINT\""), "{cfg}");
    f.wait("out file", |f| std::fs::read_to_string(f.home.join("looper-out.log")).is_ok_and(|t| t == "looping\n"));
    // Rolling restart of a plain process: a new pid, still one worker.
    let before = f.pids("ticker");
    f.ok(&["restart", "ticker"]);
    let after = f.pids("ticker");
    assert!(after.len() == 1 && after != before, "{before:?} -> {after:?}");
    // Nothing to run.
    let (code, out) = f.cli(&["start", "no-such-program-xyz"]);
    assert_eq!(code, 2);
    assert!(out.contains("quote it"), "{out}");
}

/// Old logs: rotated + gzipped files read back in order, searched, filtered,
/// as JSON, and safe to pipe into `head`.
#[test]
fn log_history_search_and_pipes() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("logs");
    let log = f.home.join("logs/chatty.log");
    // keep = 10 retains all ~40 KB, so `worker ready` (logged after
    // min_uptime, which on a loaded machine can fall inside the output) is
    // never rotated out before the `--level info` check below.
    let cfg = format!(
        "[app]\nname = \"chatty\"\ncommand = \"sh\"\n\
         args = [\"-c\", \"for i in $(seq 1 400); do echo \\\"line $i padding padding padding\\\"; done; echo oops >&2; exec sleep 300\"]\n\
         [workers]\nmin_uptime = 300\n[logging]\nfile = \"{}\"\n[logging.rotate]\nmax_size = \"8K\"\nkeep = 10\ncompress = true\n",
        log.display()
    );
    std::fs::write(f.home.join("chatty.toml"), cfg).unwrap();
    f.ok(&["start", "chatty"]);
    f.wait("line 400 in the log", |f| {
        f.cli(&["logs", "chatty", "--nostream", "--lines", "5"]).1.contains("line 400 padding")
    });
    f.wait("rotated and gzipped", |f| f.home.join("logs/chatty.log.1.gz").exists());

    // History: every retained line, oldest first, across .gz and current.
    let out = f.ok(&["logs", "chatty", "--history"]);
    let nums: Vec<u32> =
        out.lines().filter_map(|l| l.split("stdout: line ").nth(1)?.split(' ').next()?.parse().ok()).collect();
    assert!(nums.len() > 50, "{} lines", nums.len());
    assert!(nums.windows(2).all(|w| w[1] == w[0] + 1), "in order, no gaps");
    assert_eq!(nums.last(), Some(&400));

    // search = history + grep; filters combine.
    let out = f.ok(&["search", "line 399 ", "chatty"]);
    assert_eq!(out.lines().count(), 1, "{out}");
    let out = f.ok(&["logs", "chatty", "--history", "--grep", "OOPS", "--ignore-case", "--err"]);
    assert!(out.contains("stderr: oops") && out.lines().count() == 1, "{out}");
    let out = f.ok(&["logs", "chatty", "--history", "--level", "info"]);
    assert!(out.contains("INFO  worker ready") && !out.contains(" OUT "), "{out}");
    let out = f.ok(&["logs", "chatty", "--history", "--since", "1h", "--lines", "3"]);
    assert_eq!(out.lines().count(), 3);
    let out = f.ok(&["logs", "chatty", "--history", "--until", "2000-01-01"]);
    assert!(out.is_empty(), "{out}");

    // JSON lines for jq.
    let out = f.ok(&["logs", "chatty", "--history", "--json", "--grep", "worker ready"]);
    let v: Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(
        (v["app"].as_str(), v["kind"].as_str(), v["level"].as_str()),
        (Some("chatty"), Some("event"), Some("info"))
    );
    assert_eq!(v["fields"]["worker"], "1");

    // Piping into `head`: stops quietly.
    let mut child = Command::new(BIN)
        .args(["logs", "chatty", "--history"])
        .env("WARDEN_HOME", &f.home)
        .env("WARDEN_RUNTIME_DIR", f.home.join("run"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut first = [0u8; 16];
    child.stdout.as_mut().unwrap().read_exact(&mut first).unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    let mut err = String::new();
    child.stderr.take().unwrap().read_to_string(&mut err).unwrap();
    assert!(status.success(), "exit {status:?}: {err}");
    assert!(!err.contains("panicked"), "{err}");

    // Works while the app is stopped too (reads the config).
    f.ok(&["kill", "--yes"]);
    assert!(f.ok(&["search", "line 400 ", "chatty"]).contains("line 400 padding"));
}

/// One raw HTTP/1.1 exchange: (status, headers lowercased, body).
fn http_raw(port: u16, req: &str) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n").expect("no header end");
    let head = String::from_utf8_lossy(&buf[..split]).to_string();
    let body = buf[split + 4..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    (status, headers, body)
}

fn get_close(port: u16, path: &str, extra: &str) -> (u16, std::collections::HashMap<String, String>, Vec<u8>) {
    http_raw(port, &format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n{extra}\r\n"))
}

/// The static server opens files three ways (openat2 with RESOLVE_CACHED,
/// openat2, realpath check on old kernels); each must keep the same root
/// closed and serve the same bytes.
#[test]
fn static_open_modes_agree_and_keep_the_root_closed() {
    let dir = std::env::temp_dir().join(format!("warden-it-open-modes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let site = dir.join("site");
    std::fs::create_dir_all(site.join("sub")).unwrap();
    std::fs::write(site.join("index.html"), "home").unwrap();
    std::fs::write(site.join("sub/index.html"), "sub home").unwrap();
    std::fs::write(site.join("a.css"), "a{}").unwrap();
    std::fs::write(dir.join("outside.txt"), "secret").unwrap();
    std::os::unix::fs::symlink(dir.join("outside.txt"), site.join("escape.txt")).unwrap();
    std::os::unix::fs::symlink("../outside.txt", site.join("up.txt")).unwrap();
    std::os::unix::fs::symlink(&dir, site.join("outdir")).unwrap();
    std::os::unix::fs::symlink("a.css", site.join("alias.css")).unwrap();
    std::os::unix::fs::symlink(site.join("a.css"), site.join("abs.css")).unwrap();
    std::os::unix::fs::symlink(site.join("sub"), site.join("subabs")).unwrap();
    let fifo = std::ffi::CString::new(site.join("pipe.txt").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let mid: Vec<u8> = (0..12_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("mid.bin"), &mid).unwrap();
    let big: Vec<u8> = (0..300_000u32).map(|i| (i % 241) as u8).collect();
    std::fs::write(site.join("big.bin"), &big).unwrap();

    for mode in ["cached", "beneath", "legacy"] {
        let port = free_port();
        let toml = format!(
            "[app]\nname = \"open-{mode}\"\nport = {port}\n[workers]\ncount = 1\n[static]\nroot = \"{}\"\n",
            site.display()
        );
        let w = Warden::start_env(&format!("open-{mode}"), port, &toml, &[("WARDEN_STATIC_OPEN", mode)]);
        w.wait_for("static worker ready", T, ready(1));
        let how = match mode {
            "cached" => "(files opened with openat2, cache-first)",
            "beneath" => "(files opened with openat2)",
            _ => "(files opened with realpath check)",
        };
        w.wait_log(how, T);
        let get = |p: &str| get_close(port, p, "");
        assert_eq!(get("/").2, b"home", "{mode}");
        assert_eq!(get("/sub/").2, b"sub home", "{mode}");
        assert_eq!(get("/alias.css").2, b"a{}", "{mode}: relative symlink inside");
        assert_eq!(get("/abs.css").2, b"a{}", "{mode}: absolute symlink inside");
        assert_eq!(get("/subabs/").2, b"sub home", "{mode}: absolute directory symlink inside");
        for bad in ["/escape.txt", "/up.txt", "/outdir/outside.txt", "/pipe.txt", "/missing.txt"] {
            assert_eq!(get(bad).0, 404, "{mode}: {bad}");
        }
        assert_eq!(get("/%2e%2e/outside.txt").0, 403, "{mode}");
        assert!(get("/mid.bin").2 == mid, "{mode}: small body");
        assert!(get("/big.bin").2 == big, "{mode}: sendfile body");
        // Keep-alive after a refused path: the connection still works.
        let (st, _, body) = http_raw(
            port,
            "GET /up.txt HTTP/1.1\r\nHost: x\r\n\r\nGET /a.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        );
        assert_eq!(st, 404);
        assert!(String::from_utf8_lossy(&body).ends_with("a{}"), "{mode}: {}", String::from_utf8_lossy(&body));
        drop(w);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `warden pm2-migrate` against a real PM2 (bench/node_modules): configs and
/// 0600 env files from `pm2 jlist` with the inherited shell left out, a
/// same-port cutover, a failed cutover rolled back to PM2, and --finalize.
#[test]
fn pm2_migrate_imports_cuts_over_and_rolls_back() {
    let pm2 = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/node_modules/.bin/pm2");
    if !pm2.exists() || !have_node() {
        eprintln!("skipping: needs node and `npm ci` in bench/ (pm2)");
        return;
    }
    let f = Fleet::new("migrate");
    let pm2_home = f.home.join("pm2home");
    let env = [("PM2_HOME", pm2_home.to_str().unwrap()), ("WARDEN_PM2", pm2.to_str().unwrap())];
    let pm2_run = |args: &[&str]| {
        let out = Command::new(&pm2).args(args).env("PM2_HOME", &pm2_home).output().unwrap();
        assert!(out.status.success(), "pm2 {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).to_string()
    };
    let (p_api, p_bad) = (free_port(), free_port());
    let app = fixture("node_app.mjs");
    // `bad` works under PM2 but crashes under Warden (it sees WARDEN_APP).
    let bad = f.home.join("bad.mjs");
    std::fs::write(&bad, format!("if (process.env.WARDEN_APP) process.exit(3);\nawait import({:?});\n", app)).unwrap();
    let idle = f.home.join("queue.mjs");
    std::fs::write(&idle, "setInterval(() => {}, 1 << 30);\n").unwrap();
    std::fs::write(
        f.home.join("eco.config.cjs"),
        format!(
            "module.exports = {{ apps: [\n\
             {{ name: 'api', script: {app:?}, env: {{ PORT: '{p_api}', API_SECRET: 's3cr3t value' }}, kill_timeout: 3000 }},\n\
             {{ name: 'queue', script: {idle:?}, cron_restart: '0 3 * * *' }},\n\
             {{ name: 'bad', script: {bad:?}, env: {{ PORT: '{p_bad}' }}, max_restarts: 1 }} ] }};\n"
        ),
    )
    .unwrap();
    pm2_run(&["start", f.home.join("eco.config.cjs").to_str().unwrap()]);
    let pm2_pid = |name: &str| -> u64 {
        let list: Value = serde_json::from_str(&pm2_run(&["jlist"])).unwrap();
        list.as_array().unwrap().iter().find(|p| p["name"] == name).map(|p| p["pid"].as_u64().unwrap()).unwrap_or(0)
    };
    let t0 = Instant::now();
    while get(p_api, "/").is_none() && t0.elapsed() < T {
        std::thread::sleep(Duration::from_millis(50));
    }

    // Dry run: nothing written, no secret printed.
    let (code, out) = f.cli_env(&["pm2-migrate", "--dry-run"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("name = \"api\"") && out.contains("name = \"queue\""), "{out}");
    assert!(!out.contains("s3cr3t"), "dry run must not print env values: {out}");
    assert!(!f.home.join("api.toml").exists());

    // The real thing: configs, env files, report.
    let (code, out) = f.cli_env(&["pm2-migrate"], &env);
    assert_eq!(code, 0, "{out}");
    let toml = std::fs::read_to_string(f.home.join("api.toml")).unwrap();
    assert!(toml.contains(&format!("port = {p_api}")) && toml.contains("env_file = \"api.env\""), "{toml}");
    assert!(toml.contains("signal = \"SIGINT\"") && toml.contains("grace_period = 3"), "{toml}");
    assert!(!toml.contains("s3cr3t"));
    let env_file = f.home.join("api.env");
    let env_text = std::fs::read_to_string(&env_file).unwrap();
    assert!(env_text.contains("API_SECRET=\"s3cr3t value\""), "{env_text}");
    assert!(!env_text.contains("CARGO_MANIFEST_DIR"), "the shell that ran `pm2 start` is not copied: {env_text}");
    let mode = std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&env_file).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    assert!(std::fs::read_to_string(f.home.join("queue.toml")).unwrap().contains("schedule = \"0 3 * * *\""));
    let report = std::fs::read_to_string(f.home.join("MIGRATION.md")).unwrap();
    assert!(report.contains("## api") && report.contains("Environment left out"), "{report}");
    for app in ["api", "queue", "bad"] {
        let (code, out) = f.cli(&["check", "-c", f.home.join(format!("{app}.toml")).to_str().unwrap()]);
        assert_eq!(code, 0, "{out}");
    }
    // A second run doesn't overwrite.
    let (code, out) = f.cli_env(&["pm2-migrate", "--apps", "api"], &env);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("--overwrite"), "{out}");

    // Same-port cutover: PM2 stops it, Warden serves it.
    let before = pm2_pid("api");
    let (code, out) = f.cli_env(&["pm2-migrate", "--apps", "api", "--overwrite", "--cutover", "same-port"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("now served by Warden"), "{out}");
    let who = get(p_api, "/").expect("api answers after the cutover");
    let pid: u64 = who.split(':').next().unwrap().parse().unwrap();
    assert!(pid != before && f.pids("api").contains(&pid), "served by a Warden worker: {who}");
    assert_eq!(pm2_pid("api"), 0, "PM2's copy is stopped");
    assert!(f.cli(&["env", "api", "--show-secrets"]).1.contains("API_SECRET=s3cr3t value"));

    // A failed cutover rolls back: PM2 serves `bad` again.
    let (code, out) = f.cli_env(&["pm2-migrate", "--apps", "bad", "--overwrite", "--cutover", "same-port"], &env);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("rolled back"), "{out}");
    let t0 = Instant::now();
    while get(p_bad, "/").is_none() && t0.elapsed() < T {
        std::thread::sleep(Duration::from_millis(50));
    }
    let who = get(p_bad, "/").expect("bad answers again under PM2");
    assert_eq!(who.split(':').next().unwrap().parse::<u64>().unwrap(), pm2_pid("bad"));

    // Finalize: only the app that runs under Warden leaves PM2.
    let (code, out) = f.cli_env(&["pm2-migrate", "--finalize"], &env);
    assert_eq!(code, 0, "{out}");
    let names: Vec<String> = serde_json::from_str::<Value>(&pm2_run(&["jlist"]))
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap().to_string())
        .collect();
    assert!(!names.contains(&"api".to_string()) && names.contains(&"bad".to_string()), "{names:?}");
    pm2_run(&["kill"]);
}

/// `warden doctor` names each problem with a fix, and fails only on real ones.
#[test]
fn doctor_reports_problems_with_fixes() {
    let f = Fleet::new("doctor");
    // Nothing configured: warnings at most, exit 0.
    let (code, out) = f.cli(&["doctor"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("kernel") && out.contains("runtime dir"), "{out}");
    // A config that doesn't parse, and a stopped app whose port is taken.
    std::fs::write(f.home.join("broken.toml"), "[app\nname = ").unwrap();
    let taken = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    std::fs::write(f.home.join("api.toml"), format!("[app]\nname = \"api\"\ncommand = \"sleep\"\nport = {port}\n"))
        .unwrap();
    let (code, out) = f.cli(&["doctor"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("FAIL  app broken"), "{out}");
    assert!(out.contains(&format!("port {port} is taken")) && out.contains(&format!("sport = :{port}")), "{out}");
    let (_, json) = f.cli(&["doctor", "--json"]);
    let v: Value = serde_json::from_str(&json).unwrap();
    let fails: Vec<&Value> = v.as_array().unwrap().iter().filter(|x| x["level"] == "fail").collect();
    assert_eq!(fails.len(), 2, "{json}");
    assert!(fails.iter().all(|x| x["fix"].is_string()), "every failure has a fix: {json}");
    drop(taken);
}

/// `warden serve`: Warden's own static server, supervised like any app.
#[test]
fn serve_static_files() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("serve");
    let site = f.home.join("site");
    std::fs::create_dir_all(site.join("assets")).unwrap();
    std::fs::create_dir_all(site.join("docs")).unwrap();
    std::fs::write(site.join("index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(site.join("404.html"), "custom missing").unwrap();
    std::fs::write(site.join("assets/app.3f9a2c1b.js"), "console.log(1)").unwrap();
    std::fs::write(site.join("style.css"), "body{}").unwrap();
    std::fs::write(site.join(".env"), "SECRET=1").unwrap();
    let big: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("big.bin"), &big).unwrap();
    // A precompressed sibling.
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gz, b"body{}").unwrap();
    std::fs::write(site.join("style.css.gz"), gz.finish().unwrap()).unwrap();
    // A symlink pointing outside the root must not be served.
    std::fs::write(f.home.join("outside.txt"), "nope").unwrap();
    std::os::unix::fs::symlink(f.home.join("outside.txt"), site.join("escape.txt")).unwrap();
    std::os::unix::fs::symlink("../outside.txt", site.join("up.txt")).unwrap();
    std::os::unix::fs::symlink(&f.home, site.join("outdir")).unwrap();
    // Symlinks that stay inside are served, relative or absolute.
    std::os::unix::fs::symlink("style.css", site.join("alias.css")).unwrap();
    std::os::unix::fs::symlink(site.join("style.css"), site.join("abs.css")).unwrap();
    // A FIFO must not hang a worker.
    let fifo = std::ffi::CString::new(site.join("pipe").to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    // Just under the single-write limit: read into the response buffer.
    let mid: Vec<u8> = (0..15_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(site.join("assets/mid.bin"), &mid).unwrap();

    let port = free_port();
    let out = f.ok(&["serve", site.to_str().unwrap(), &port.to_string(), "--name", "site", "-i", "2", "--spa"]);
    assert!(out.contains("site: online (2/2"), "{out}");

    let (st, h, body) = get_close(port, "/", "");
    assert_eq!((st, body.as_slice()), (200, b"<h1>home</h1>".as_slice()));
    assert_eq!(h["content-type"], "text/html; charset=utf-8");
    assert_eq!(h["cache-control"], "no-cache");
    let (st, h, _) = get_close(port, "/assets/app.3f9a2c1b.js", "");
    assert_eq!(st, 200);
    assert!(h["cache-control"].contains("immutable"), "{h:?}");
    // Revalidation.
    let etag = h["etag"].clone();
    let (st, _, body) = get_close(port, "/assets/app.3f9a2c1b.js", &format!("If-None-Match: {etag}\r\n"));
    assert_eq!((st, body.len()), (304, 0));
    // Ranges on a large file (sent with sendfile) and the whole file.
    let (st, h, body) = get_close(port, "/big.bin", "Range: bytes=1000-1999\r\n");
    assert_eq!(st, 206);
    assert_eq!(h["content-range"], format!("bytes 1000-1999/{}", big.len()));
    assert_eq!(body, big[1000..2000]);
    let (st, _, body) = get_close(port, "/big.bin", "Range: bytes=-10\r\n");
    assert_eq!((st, body.as_slice()), (206, &big[big.len() - 10..]));
    let (st, _, _) = get_close(port, "/big.bin", "Range: bytes=99999999-\r\n");
    assert_eq!(st, 416);
    let (st, h, body) = get_close(port, "/big.bin", "");
    assert_eq!((st, body.len()), (200, big.len()));
    assert!(body == big, "large body intact");
    assert_eq!(h["content-length"], big.len().to_string());
    // Precompressed.
    let (st, h, body) = get_close(port, "/style.css", "Accept-Encoding: gzip, deflate\r\n");
    assert_eq!((st, h.get("content-encoding").map(String::as_str)), (200, Some("gzip")));
    let mut plain = String::new();
    std::io::Read::read_to_string(&mut flate2::read::GzDecoder::new(&body[..]), &mut plain).unwrap();
    assert_eq!(plain, "body{}");
    let (_, h, body) = get_close(port, "/style.css", "");
    assert!(!h.contains_key("content-encoding") && body == b"body{}");
    // SPA fallback, 404 page, hidden and escaping paths.
    let (st, _, body) = get_close(port, "/app/settings", "Accept: text/html\r\n");
    assert_eq!((st, body.as_slice()), (200, b"<h1>home</h1>".as_slice()));
    let (st, _, body) = get_close(port, "/missing.png", "");
    assert_eq!((st, body.as_slice()), (404, b"custom missing".as_slice()));
    assert_eq!(get_close(port, "/.env", "").0, 404);
    assert_eq!(get_close(port, "/escape.txt", "").0, 404);
    assert_eq!(get_close(port, "/up.txt", "").0, 404);
    assert_eq!(get_close(port, "/outdir/outside.txt", "").0, 404);
    assert_eq!(get_close(port, "/alias.css", "").2, b"body{}");
    assert_eq!(get_close(port, "/abs.css", "").2, b"body{}");
    let t0 = std::time::Instant::now();
    assert_eq!(get_close(port, "/pipe", "Accept: application/json\r\n").0, 404);
    assert!(t0.elapsed() < Duration::from_secs(2), "a FIFO is refused at once");
    let (st, _, body) = get_close(port, "/assets/mid.bin", "");
    assert_eq!(st, 200);
    assert!(body == mid, "15 KB body intact");
    let (st, _, body) = get_close(port, "/assets/mid.bin", "Range: bytes=100-10099\r\n");
    assert_eq!(st, 206);
    assert!(body == mid[100..10_100], "small-file range intact");
    assert_eq!(get_close(port, "/../../etc/passwd", "").0, 403);
    assert_eq!(get_close(port, "/%2e%2e/%2e%2e/etc/passwd", "").0, 403);
    assert_eq!(get_close(port, "/docs", "").0, 301);
    // Methods and HEAD.
    assert_eq!(http_raw(port, "POST / HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").0, 405);
    let (st, h, body) = http_raw(port, "HEAD /big.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!((st, body.len(), h["content-length"].clone()), (200, 0, big.len().to_string()));
    // Keep-alive: two requests on one connection.
    let (st, _, body) = http_raw(
        port,
        "GET /style.css HTTP/1.1\r\nHost: x\r\n\r\nGET /style.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(st, 200);
    assert!(String::from_utf8_lossy(&body).contains("HTTP/1.1 200"), "second response on the same connection");
    // Oversized head.
    let huge = format!("GET / HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(20_000));
    assert_eq!(http_raw(port, &huge).0, 431);

    // Rolling restart under load: no failed request.
    let stop = Arc::new(AtomicBool::new(false));
    let (ok, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let client = {
        let (stop, ok, fail) = (stop.clone(), ok.clone(), fail.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match std::panic::catch_unwind(|| get_close(port, "/style.css", "").0) {
                    Ok(200) => ok.fetch_add(1, Ordering::Relaxed),
                    _ => fail.fetch_add(1, Ordering::Relaxed),
                };
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    f.ok(&["restart", "site"]);
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    eprintln!("static rolling restart: {} ok, {} failed", ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed));
    assert!(ok.load(Ordering::Relaxed) > 100);
    assert!(fail.load(Ordering::Relaxed) <= allowed_resets(), "failed: {}", fail.load(Ordering::Relaxed));

    // Basic auth.
    let p2 = free_port();
    std::fs::create_dir_all(f.home.join("private")).unwrap();
    std::fs::write(f.home.join("private/index.html"), "secret page").unwrap();
    f.ok(&["serve", f.home.join("private").to_str().unwrap(), &p2.to_string(), "--basic-auth", "u:p"]);
    let (st, h, _) = get_close(p2, "/", "");
    assert_eq!(st, 401);
    assert!(h["www-authenticate"].starts_with("Basic"));
    assert_eq!(get_close(p2, "/", "Authorization: Basic dTpw\r\n").0, 200);
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(f.home.join("private.toml")).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600, "config with credentials is owner-only");
}

// ---- wardend

/// A wardend of one Fleet, run in the foreground as our child (stdout and
/// stderr to `wardend.out` in the fleet's home). Dropping it stops it:
/// SIGTERM, then SIGKILL.
struct Wardend {
    child: Child,
    out: PathBuf,
    sock: PathBuf,
}

impl Wardend {
    fn start(f: &Fleet, env: &[(&str, &str)]) -> Wardend {
        Self::start_args(f, &["daemon"], env)
    }

    fn start_args(f: &Fleet, args: &[&str], env: &[(&str, &str)]) -> Wardend {
        let out = f.home.join("wardend.out");
        let file = std::fs::File::create(&out).unwrap();
        let child = Command::new(BIN)
            .args(args)
            .env("WARDEN_HOME", &f.home)
            .env("WARDEN_RUNTIME_DIR", f.home.join("run"))
            .env_remove("WARDEN_CONFIG")
            .envs(env.iter().copied())
            .current_dir(&f.home)
            .stdout(Stdio::from(file.try_clone().unwrap()))
            .stderr(file)
            .spawn()
            .unwrap();
        let w = Wardend { child, out, sock: f.home.join("run/wardend.sock") };
        let t0 = Instant::now();
        while w.try_request(r#"{"cmd":"hello"}"#).is_none() {
            assert!(t0.elapsed() < T, "wardend did not answer:\n{}", w.log());
            std::thread::sleep(Duration::from_millis(50));
        }
        w
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    fn connect(&self, req: &str) -> Option<std::io::BufReader<std::os::unix::net::UnixStream>> {
        let mut s = std::os::unix::net::UnixStream::connect(&self.sock).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(10))).ok()?;
        writeln!(s, "{req}").ok()?;
        Some(std::io::BufReader::new(s))
    }

    fn try_request(&self, req: &str) -> Option<Value> {
        use std::io::BufRead;
        let mut line = String::new();
        self.connect(req)?.read_line(&mut line).ok()?;
        serde_json::from_str(&line).ok()
    }

    fn request(&self, req: &str) -> Value {
        self.try_request(req).unwrap_or_else(|| panic!("no answer to {req}:\n{}", self.log()))
    }

    fn subscribe(&self, req: &str) -> EventStream {
        let r = self.connect(req).unwrap_or_else(|| panic!("cannot subscribe:\n{}", self.log()));
        EventStream { r, line: String::new(), seen: Vec::new() }
    }

    /// The entry of `app` in `apps`.
    fn app(&self, app: &str) -> Value {
        let v = self.request(r#"{"cmd":"apps"}"#);
        v["apps"].as_array().unwrap().iter().find(|a| a["name"] == app).cloned().unwrap_or(Value::Null)
    }

    fn wait_app(&self, what: &str, app: &str, f: impl Fn(&Value) -> bool) -> Value {
        let t0 = Instant::now();
        loop {
            let a = self.app(app);
            if f(&a) {
                return a;
            }
            assert!(t0.elapsed() < T, "timed out waiting for {what}; last: {a:#}\n{}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

impl Drop for Wardend {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) {
                if self.child.try_wait().ok().flatten().is_some() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// A `subscribe` stream from wardend.
struct EventStream {
    r: std::io::BufReader<std::os::unix::net::UnixStream>,
    line: String,
    seen: Vec<Value>,
}

impl EventStream {
    fn next(&mut self) -> Option<Value> {
        use std::io::BufRead;
        // A read timeout may leave half a line in `line`: keep it for the next call.
        match self.r.read_line(&mut self.line) {
            Ok(n) if n > 0 && self.line.ends_with('\n') => {
                let v: Value = serde_json::from_str(&self.line).unwrap();
                self.line.clear();
                self.seen.push(v.clone());
                Some(v)
            }
            _ => None,
        }
    }

    /// The next event matching `f` (within `T`).
    fn wait(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let t0 = Instant::now();
        while t0.elapsed() < T {
            if let Some(v) = self.next() {
                if f(&v) {
                    return v;
                }
            }
        }
        let seen: Vec<String> = self.seen.iter().map(|v| v.to_string().chars().take(200).collect()).collect();
        panic!("no event {what} within {T:?}; seen:\n{}", seen.join("\n"));
    }
}

fn sup_event(app: &str, event: &str) -> impl Fn(&Value) -> bool {
    let (app, event) = (app.to_string(), event.to_string());
    move |v| v["type"] == "supervisor" && v["app"] == app.as_str() && v["event"] == event.as_str()
}

fn supervisor_pid(f: &Fleet, app: &str) -> u64 {
    f.app(app)["status"]["pid"].as_u64().unwrap_or_else(|| panic!("{app} is not running:\n{}", f.cli(&["list"]).1))
}

/// A `warden` child whose stdout lines arrive on a channel.
fn spawn_lines(f: &Fleet, args: &[&str]) -> (Child, std::sync::mpsc::Receiver<String>) {
    let mut child = Command::new(BIN)
        .args(args)
        .env("WARDEN_HOME", &f.home)
        .env("WARDEN_RUNTIME_DIR", f.home.join("run"))
        .env_remove("WARDEN_CONFIG")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    (child, rx)
}

fn stop_child(mut c: Child) {
    unsafe { libc::kill(c.id() as i32, libc::SIGINT) };
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(st)) = c.try_wait() {
            assert!(st.success(), "Ctrl-C ends it cleanly: {st:?}");
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = c.kill();
    let _ = c.wait();
    panic!("did not stop on SIGINT");
}

/// A wardend we did not spawn ourselves (`--background`, or started by
/// `warden start`): SIGTERM, then SIGKILL, on drop, whatever happened.
struct DetachedWardend(Option<i32>);

impl DetachedWardend {
    /// From output that says "wardend started in the background (pid N)".
    fn from_output(out: &str) -> DetachedWardend {
        let pid = out.split("wardend started in the background (pid ").nth(1).and_then(|s| s.split(')').next());
        DetachedWardend(pid.and_then(|p| p.parse().ok()))
    }
}

impl Drop for DetachedWardend {
    fn drop(&mut self) {
        let Some(pid) = self.0 else { return };
        // Only if that pid is still our `warden daemon` (not a reused pid).
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if cmdline != format!("{BIN}\0daemon\0").as_bytes() {
            return;
        }
        unsafe { libc::kill(pid, libc::SIGTERM) };
        let t0 = Instant::now();
        while alive(pid as u64) && t0.elapsed() < Duration::from_secs(3) {
            std::thread::sleep(Duration::from_millis(20));
        }
        if alive(pid as u64) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

#[test]
fn wardend_runs_reports_and_stops() {
    let f = Fleet::new("wd-life");
    let (code, out) = f.cli(&["daemon", "status"]);
    assert_eq!(code, 1, "not running yet: {out}");
    assert!(out.contains("not running"), "{out}");

    // --background detaches it; status answers; a second one is refused.
    let out = f.ok(&["daemon", "--background"]);
    let bg = DetachedWardend::from_output(&out);
    assert!(bg.0.is_some(), "{out}");
    let out = f.ok(&["daemon", "status"]);
    assert!(out.contains("wardend: pid") && out.contains("protocol 1"), "{out}");
    let (code, out) = f.cli(&["daemon"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("already running"), "{out}");
    let out = f.ok(&["daemon", "--background"]);
    assert!(out.contains("already running"), "{out}");
    let out = f.ok(&["daemon", "stop"]);
    assert!(out.contains("wardend stopped"), "{out}");
    let (code, _) = f.cli(&["daemon", "status"]);
    assert_eq!(code, 1);
    assert!(!f.home.join("run/wardend.sock").exists(), "socket removed");
    assert!(f.ok(&["daemon", "stop"]).contains("was not running"));

    // Like PM2: `warden start` starts wardend too (without WARDEN_NO_DAEMON),
    // and `warden kill` stops it after the apps.
    if !have_bun() {
        return;
    }
    let port = free_port();
    let (code, out) = f.cli_env(
        &["start", &fixture("app.ts"), "--name", "api", "--port", &port.to_string()],
        &[("WARDEN_NO_DAEMON", "0")],
    );
    let autostarted = DetachedWardend::from_output(&out);
    assert_eq!(code, 0, "{out}");
    assert!(autostarted.0.is_some() && out.contains("api: online"), "{out}");
    let out = f.ok(&["daemon", "status"]);
    assert!(out.lines().any(|l| l.starts_with("api ") && l.contains("running") && l.contains("wardend")), "{out}");
    let out = f.ok(&["kill", "--yes"]);
    assert!(out.contains("api: stopped") && out.contains("wardend: stopped"), "{out}");
    assert_eq!(f.cli(&["daemon", "status"]).0, 1);
}

#[test]
fn wardend_watches_apps_streams_events_and_forwards_requests() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("wd-watch");
    let (p1, p2) = (free_port(), free_port());
    f.ok(&["start", &fixture("app.ts"), "--name", "api", "--port", &p1.to_string()]);
    let d = Wardend::start(&f, &[]);

    // A running app: watched, `supervised_by` wardend (started in the background).
    let a = d.wait_app("api running", "api", |a| a["state"] == "running");
    assert_eq!(a["supervised_by"], "wardend", "{a:#}");
    assert_eq!(a["supervisor_pid"].as_u64(), Some(supervisor_pid(&f, "api")));
    assert_eq!(a["status"]["workers_ready"], 1);
    let hello = d.request(r#"{"cmd":"hello"}"#);
    assert_eq!((hello["ok"].as_bool(), hello["hello"]["protocol"].as_u64()), (Some(true), Some(1)), "{hello}");

    // subscribe: hello, apps, a status per running app, then events.
    let mut ev = d.subscribe(r#"{"cmd":"subscribe","interval_ms":500}"#);
    let first = ev.next().unwrap();
    assert!(first["type"] == "hello" && first.get("app").is_none(), "{first}");
    let apps = ev.next().unwrap();
    assert_eq!(apps["type"], "apps", "{apps}");
    let st = ev.next().unwrap();
    assert!(st["type"] == "status" && st["app"] == "api", "{st}");
    ev.wait("a host event", |v| v["type"] == "host" && v["mem_total_bytes"].as_u64() > Some(0));

    // An app started later is found.
    f.ok(&["start", &fixture("app.ts"), "--name", "web", "--port", &p2.to_string()]);
    let found = ev.wait("web found", sup_event("web", "found"));
    assert_eq!(found["pid"].as_u64(), Some(supervisor_pid(&f, "web")));

    // Requests forwarded to an app: a reload (its rollout is followed with status).
    let r = d.request(r#"{"cmd":"app","app":"api","request":{"cmd":"reload"}}"#);
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["response"]["seq"].as_u64().is_some(), "{r}");
    f.wait("reload finished", |f| f.app("api")["status"]["last_rollout"]["ok"] == true);
    // logs stream through.
    let mut logs =
        d.connect(r#"{"cmd":"app","app":"api","request":{"cmd":"logs","lines":50,"follow":false}}"#).unwrap();
    let mut text = String::new();
    logs.read_to_string(&mut text).unwrap();
    assert!(text.contains("worker ready"), "{text}");
    let r = d.request(r#"{"cmd":"app","app":"nope","request":{"cmd":"status"}}"#);
    assert_eq!(r["ok"], false, "{r}");

    // `warden events --json`: NDJSON from wardend; plain text without --json.
    let (child, lines) = spawn_lines(&f, &["events", "--json"]);
    let got: Vec<Value> =
        (0..3).map(|_| serde_json::from_str(&lines.recv_timeout(T).expect("an event line")).unwrap()).collect();
    assert_eq!((got[0]["type"].as_str(), got[1]["type"].as_str()), (Some("hello"), Some("apps")), "{got:?}");
    assert_eq!(got[2]["type"], "status", "{got:?}");
    stop_child(child);
    let (child, lines) = spawn_lines(&f, &["events", "api"]);
    let first = lines.recv_timeout(T).unwrap();
    assert!(first.contains("wardend pid="), "{first}");
    let mut text = String::new();
    while let Ok(l) = lines.recv_timeout(Duration::from_secs(3)) {
        text += &l;
        text += "\n";
        if l.contains("workers ready") {
            break;
        }
    }
    assert!(text.contains("api running (supervised by wardend"), "{text}");
    assert!(text.contains("api 1/1 workers ready"), "{text}");
    stop_child(child);

    // `warden delete`: the supervisor exits on purpose; not restarted; forgotten.
    f.ok(&["delete", "web"]);
    ev.wait("web exited", sup_event("web", "exited"));
    d.wait_app("web forgotten", "web", |a| a.is_null());
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!ev.seen.iter().any(|v| sup_event("web", "restarting")(v) || sup_event("web", "died")(v)));
    assert!(get(p2, "/whoami").is_none(), "web stays down");
}

#[test]
fn wardend_restarts_a_killed_supervisor_and_apps_outlive_it() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("wd-restart");
    let port = free_port();
    // A variable only the supervisor's environment has (not wardend's).
    let (code, out) = f.cli_env(
        &["start", &fixture("app.ts"), "--name", "api", "--port", &port.to_string()],
        &[("WD_ORIGIN_MARK", "kept")],
    );
    assert_eq!(code, 0, "{out}");
    let mut d = Wardend::start(&f, &[]);
    let mut ev = d.subscribe(r#"{"cmd":"subscribe"}"#);
    ev.wait("api status", |v| v["type"] == "status" && v["app"] == "api");

    // kill -9: died, restarting (after 1 s), started; the app serves again.
    let pid = supervisor_pid(&f, "api");
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let died = ev.wait("died", sup_event("api", "died"));
    assert_eq!(died["pid"].as_u64(), Some(pid));
    let restarting = ev.wait("restarting", sup_event("api", "restarting"));
    assert_eq!(restarting["detail"], "after 1000 ms", "{restarting}");
    let started = ev.wait("started", sup_event("api", "started"));
    let new_pid = started["pid"].as_u64().unwrap();
    assert_ne!(new_pid, pid);
    f.wait("api serving again", |_| get(port, "/whoami").is_some());
    let a = d.wait_app("api running again", "api", |a| a["state"] == "running");
    assert_eq!(a["supervisor_restarts"], 1, "{a:#}");
    assert_eq!(a["supervisor_pid"].as_u64(), Some(new_pid));
    assert_eq!(supervisor_pid(&f, "api"), new_pid);
    assert!(d.log().contains("supervisor died; restarting it app=api"), "{}", d.log());
    // Restarted with the environment it was started with, not wardend's.
    let env = std::fs::read(format!("/proc/{new_pid}/environ")).unwrap();
    assert!(env.split(|b| *b == 0).any(|kv| kv == b"WD_ORIGIN_MARK=kept"));
    assert!(env.split(|b| *b == 0).any(|kv| kv == b"WARDEN_LAUNCH=background"));

    // A hung supervisor is reported, never killed: its workers keep serving.
    unsafe { libc::kill(new_pid as i32, libc::SIGSTOP) };
    let unresponsive = ev.wait("unresponsive", sup_event("api", "unresponsive"));
    assert_eq!(unresponsive["pid"].as_u64(), Some(new_pid));
    let a = d.app("api");
    assert_eq!(a["state"], "unreachable", "{a:#}");
    assert!(a["problem"].as_str().unwrap().contains(&format!("gdb -p {new_pid}")), "{a:#}");
    assert!(get(port, "/whoami").is_some(), "workers still serve");
    assert!(alive(new_pid));
    unsafe { libc::kill(new_pid as i32, libc::SIGCONT) };
    ev.wait("responsive", sup_event("api", "responsive"));
    assert!(d.log().contains("supervisor is unresponsive; not killing it"), "{}", d.log());

    // Shut down on request: exited, and not restarted.
    f.ok(&["shutdown", "api"]);
    ev.wait("exited", sup_event("api", "exited"));
    d.wait_app("api stopped", "api", |a| a["state"] == "stopped");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(d.app("api")["state"], "stopped", "not restarted");
    assert!(get(port, "/whoami").is_none());

    // wardend's own `start`.
    let r = d.request(r#"{"cmd":"start","app":"api"}"#);
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["message"].as_str().unwrap().contains("started in the background"), "{r}");
    f.wait("api serving", |_| get(port, "/whoami").is_some());
    let r = d.request(r#"{"cmd":"start","app":"api"}"#);
    assert!(r["message"].as_str().unwrap().starts_with("already"), "{r}");

    // SIGKILL wardend: every app keeps serving.
    unsafe { libc::kill(d.child.id() as i32, libc::SIGKILL) };
    d.child.wait().unwrap();
    std::thread::sleep(Duration::from_millis(500));
    assert!(get(port, "/whoami").is_some(), "the app outlives wardend");
    assert!(f.app("api")["status"]["pid"].is_u64());
}

#[test]
fn wardend_gives_up_on_a_supervisor_that_keeps_dying() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("wd-giveup");
    let port = free_port();
    f.ok(&["start", &fixture("app.ts"), "--name", "api", "--port", &port.to_string()]);
    let policy = "initial_ms=100,max_ms=200,deaths=3,window_ms=60000";
    let d = Wardend::start(&f, &[("WARDEN_DAEMON_POLICY", policy)]);
    let mut ev = d.subscribe(r#"{"cmd":"subscribe"}"#);
    ev.wait("api status", |v| v["type"] == "status" && v["app"] == "api");

    // Three deaths within the window: restarted twice, then given up.
    let mut pid = supervisor_pid(&f, "api");
    for death in 1..=3 {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        let died = ev.wait("died", sup_event("api", "died"));
        assert_eq!(died["pid"].as_u64(), Some(pid));
        if death < 3 {
            pid = ev.wait("started", sup_event("api", "started"))["pid"].as_u64().unwrap();
        }
    }
    let gave_up = ev.wait("gave up", sup_event("api", "gave_up"));
    assert_eq!(gave_up["detail"], "3 deaths in 60 s", "{gave_up}");
    let a = d.wait_app("api gave up", "api", |a| a["state"] == "gave_up");
    assert!(a["problem"].as_str().unwrap().contains("warden start api"), "{a:#}");
    assert_eq!(a["supervisor_restarts"], 2);
    let log = d.log();
    assert!(log.contains("ERROR wardend gave up restarting it") || log.contains("gave up restarting it"), "{log}");
    assert!(log.contains("last_lines=") && log.contains("then run `warden start api`"), "{log}");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(f.app("api")["status"].is_null(), "not restarted after giving up");

    // `warden start api` clears it.
    f.ok(&["start", "api"]);
    let a = d.wait_app("api running", "api", |a| a["state"] == "running");
    assert!(a["problem"].is_null(), "{a:#}");
}

// ---- subscribe (event stream)

use std::io::BufRead;

/// A raw `subscribe` connection speaking NDJSON, as `wardend` or a GUI would.
struct Events {
    r: std::io::BufReader<std::os::unix::net::UnixStream>,
    /// A line cut short by a read timeout, completed by the next read.
    partial: String,
    seen: Vec<Value>,
}

impl Events {
    fn open(w: &Warden, req: &str) -> Events {
        let mut s = std::os::unix::net::UnixStream::connect(w.socket()).unwrap();
        writeln!(s, "{req}").unwrap();
        Events { r: std::io::BufReader::new(s), partial: String::new(), seen: Vec::new() }
    }

    /// `Ok(None)` at EOF, `Err(())` when nothing arrived within `timeout`.
    fn read(&mut self, timeout: Duration) -> Result<Option<Value>, ()> {
        self.r.get_ref().set_read_timeout(Some(timeout.max(Duration::from_millis(1)))).unwrap();
        match self.r.read_line(&mut self.partial) {
            Ok(_) if self.partial.ends_with('\n') => {
                let v: Value = serde_json::from_str(&self.partial)
                    .unwrap_or_else(|e| panic!("not an event line ({e}): {:?}", self.partial));
                self.partial.clear();
                self.seen.push(v.clone());
                Ok(Some(v))
            }
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => Err(()),
            Err(e) => panic!("reading events: {e}"),
        }
    }

    fn next(&mut self) -> Value {
        match self.read(T) {
            Ok(Some(v)) => v,
            Ok(None) => panic!("EOF; events so far: {:#?}", self.seen),
            Err(()) => panic!("no event within {T:?}; events so far: {:#?}", self.seen),
        }
    }

    /// Events up to and including the first one matching `f`.
    fn until(&mut self, what: &str, f: impl Fn(&Value) -> bool) -> Vec<Value> {
        let t0 = Instant::now();
        let mut out = Vec::new();
        loop {
            let left = T.checked_sub(t0.elapsed()).unwrap_or_default();
            match self.read(left) {
                Ok(Some(v)) => {
                    let done = f(&v);
                    out.push(v);
                    if done {
                        return out;
                    }
                }
                Ok(None) => panic!("EOF while waiting for {what}: {out:#?}"),
                Err(()) => panic!("timed out waiting for {what}: {out:#?}"),
            }
        }
    }

    /// Every event until EOF.
    fn rest(&mut self) -> Vec<Value> {
        let t0 = Instant::now();
        let mut out = Vec::new();
        loop {
            let left = T.checked_sub(t0.elapsed()).unwrap_or_default();
            match self.read(left) {
                Ok(Some(v)) => out.push(v),
                Ok(None) => return out,
                Err(()) => panic!("no EOF within {T:?}: {out:#?}"),
            }
        }
    }
}

fn is_worker(e: &Value, worker: u64, event: &str) -> bool {
    e["type"] == "worker" && e["worker"] == worker && e["event"] == event
}

/// The `event` names of one worker's events, in order.
fn worker_story(events: &[Value], worker: u64) -> Vec<String> {
    events
        .iter()
        .filter(|e| e["type"] == "worker" && e["worker"] == worker)
        .map(|e| e["event"].as_str().unwrap().to_string())
        .collect()
}

fn wait_exit(w: &mut Warden) -> Option<i32> {
    let t0 = Instant::now();
    loop {
        if let Some(st) = w.child.try_wait().unwrap() {
            return st.code();
        }
        assert!(t0.elapsed() < T, "warden did not exit\n{}", w.log());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn subscribe_streams_worker_rollout_log_and_bye_events() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("sub-events", port, &simple("sub-events", port, 2, ""));
    let s = w.wait_for("2 ready workers", T, ready(2));

    // hello, then a status snapshot, at once.
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe","interval_ms":60000}"#);
    let mut logs = Events::open(&w, r#"{"cmd":"subscribe","interval_ms":60000,"logs":true}"#);
    let hello = ev.next();
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["app"], "sub-events");
    assert_eq!(hello["pid"], w.child.id());
    assert!(!hello["version"].as_str().unwrap().is_empty());
    let snap = ev.next();
    assert_eq!((&snap["type"], &snap["app"]), (&"status".into(), &"sub-events".into()), "{snap}");
    assert_eq!(snap["status"]["workers_ready"], 2);
    assert_eq!(logs.next()["type"], "hello");
    assert_eq!(logs.next()["type"], "status");

    // kill -9 worker 2: crashed, restarting, starting, ready, for worker 2 only.
    let victim = s["workers"][1]["pid"].as_u64().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let got = ev.until("worker 2 ready again", |e| is_worker(e, 2, "ready"));
    assert_eq!(worker_story(&got, 2), ["crashed", "restarting", "starting", "ready"], "{got:#?}");
    assert!(worker_story(&got, 1).is_empty(), "{got:#?}");
    let crashed = got.iter().find(|e| is_worker(e, 2, "crashed")).unwrap();
    assert_eq!(crashed["pid"], victim);
    assert!(crashed["detail"].as_str().unwrap().contains("SIGKILL"), "{crashed}");
    assert!(crashed["at_ms"].as_u64().unwrap() > 1_600_000_000_000, "{crashed}");
    let restarting = got.iter().find(|e| is_worker(e, 2, "restarting")).unwrap();
    assert!(restarting["detail"].as_str().unwrap().starts_with("in_ms="), "{restarting}");
    let back = got.last().unwrap();
    assert!(back["pid"].as_u64().is_some_and(|p| p != victim), "{back}");
    assert!(back["detail"].as_str().unwrap().starts_with("startup_ms="), "{back}");
    assert!(!got.iter().any(|e| e["type"] == "log"), "no log events without `logs`");

    // `logs: true` also streams the log lines.
    let line = logs.until("the crash log line", |e| {
        e["type"] == "log" && e["line"].as_str().is_some_and(|l| l.contains("worker crashed"))
    });
    assert_eq!(line.last().unwrap()["app"], "sub-events");

    // A reload: `rollout` when it starts (before any worker is touched) and
    // as its phase changes, then `rollout_done`.
    let before: Vec<u64> = Warden::pids(&w.status().unwrap());
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let got = ev.until("rollout_done", |e| e["type"] == "rollout_done");
    let done = got.last().unwrap();
    assert_eq!(done["app"], "sub-events");
    assert_eq!(done["outcome"]["ok"], true, "{done}");
    assert_eq!(done["outcome"]["kind"], "reload");
    let rollouts: Vec<&Value> = got.iter().filter(|e| e["type"] == "rollout").collect();
    assert!(rollouts.len() >= 2, "a start and phase changes: {got:#?}");
    assert!(
        rollouts.iter().all(|r| r["rollout"]["seq"] == done["outcome"]["seq"] && r["rollout"]["kind"] == "reload"),
        "{got:#?}"
    );
    assert_eq!(got[0]["type"], "rollout", "the rollout is announced first: {got:#?}");
    assert_eq!(got[0]["rollout"]["phase"], "starting");
    assert_eq!((&got[0]["rollout"]["done"], &got[0]["rollout"]["total"]), (&0.into(), &2.into()));
    let phases: HashSet<&str> = rollouts.iter().map(|r| r["rollout"]["phase"].as_str().unwrap()).collect();
    assert!(phases.iter().any(|p| p.contains("draining old process")), "{phases:?}");
    for old in &before {
        let of_old: Vec<&str> = got.iter().filter(|e| e["pid"] == *old).map(|e| e["event"].as_str().unwrap()).collect();
        assert_eq!(of_old, ["stopping", "stopped"], "old worker {old}: {got:#?}");
    }
    for id in [1, 2] {
        assert_eq!(worker_story(&got, id), ["starting", "ready", "stopping", "stopped"], "worker {id}: {got:#?}");
    }

    // `shutdown`: every worker stops, then `bye`, then EOF.
    let (_, out) = w.request(r#"{"cmd":"shutdown"}"#);
    assert!(out.contains("\"ok\":true"), "{out}");
    let rest = ev.rest();
    let bye = rest.last().unwrap_or_else(|| panic!("no events before EOF"));
    assert_eq!(bye["type"], "bye", "{rest:#?}");
    assert_eq!(bye["app"], "sub-events");
    assert_eq!(bye["reason"], "shutdown request");
    for id in [1, 2] {
        assert_eq!(worker_story(&rest, id), ["stopping", "stopped"], "worker {id}: {rest:#?}");
    }
    // The log stream ends the same way, after Warden's last log line.
    let rest = logs.rest();
    assert_eq!(rest.last().unwrap()["type"], "bye", "{rest:#?}");
    assert!(
        rest.iter().any(|e| e["type"] == "log" && e["line"].as_str().unwrap().contains("INFO  stopped")),
        "{rest:#?}"
    );
    assert_eq!(wait_exit(&mut w), Some(0));
}

#[test]
fn subscribe_sends_status_every_interval_and_bye_on_sigterm() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("sub-interval", port, &simple("sub-interval", port, 1, ""));
    w.wait_for("1 ready worker", T, ready(1));
    let t0 = Instant::now();
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe","interval_ms":500}"#);
    assert_eq!(ev.next()["type"], "hello");
    assert_eq!(ev.next()["type"], "status");
    let mut periodic = 0;
    while let Some(left) = Duration::from_millis(1300).checked_sub(t0.elapsed()) {
        match ev.read(left) {
            Ok(Some(e)) if e["type"] == "status" => {
                assert_eq!(e["status"]["workers_ready"], 1);
                periodic += 1;
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("EOF"),
            Err(()) => break,
        }
    }
    assert!(periodic >= 2, "{periodic} status events after the snapshot in 1.3 s: {:#?}", ev.seen);

    w.signal(libc::SIGTERM);
    let rest = ev.rest();
    let bye = rest.last().unwrap_or_else(|| panic!("no events before EOF"));
    assert_eq!((&bye["type"], &bye["reason"]), (&"bye".into(), &"SIGTERM".into()), "{rest:#?}");
    assert_eq!(worker_story(&rest, 1), ["stopping", "stopped"], "{rest:#?}");
    assert_eq!(wait_exit(&mut w), Some(0));
}

/// Worker mode: `worker` 0 is the host process, 1..=count its Worker threads.
#[test]
fn subscribe_in_worker_mode_names_threads_and_the_host() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start(
        "sub-threads",
        port,
        &format!(
            "[app]\nname = \"sub-threads\"\nentry = \"{}\"\nport = {port}\n[workers]\ncount = 2\nmode = \"worker\"\n[restart]\nbackoff_initial = 50\n[shutdown]\ndrain_ms = 100\n",
            fixture("app.ts")
        ),
    );
    let s = w.wait_for("2 ready threads", T, ready(2));
    let host = s["host"]["pid"].as_u64().unwrap();
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe","interval_ms":60000}"#);
    assert_eq!(ev.next()["type"], "hello");
    assert_eq!(ev.next()["type"], "status");

    // One Worker throws: that thread crashed, then the host is replaced.
    let _ = get(port, "/throw");
    let got = ev.until("the recovery", |e| e["type"] == "rollout_done");
    let crashed = got.iter().find(|e| e["type"] == "worker" && e["event"] == "crashed").unwrap();
    assert!([1, 2].contains(&crashed["worker"].as_u64().unwrap()), "a thread, not the host: {crashed}");
    assert_eq!(crashed["pid"], host);
    assert_eq!(got.last().unwrap()["outcome"]["kind"], "recovery", "{got:#?}");
    assert_eq!(got.last().unwrap()["outcome"]["ok"], true, "{got:#?}");
    let new_host = got.iter().find(|e| is_worker(e, 0, "starting")).unwrap()["pid"].as_u64().unwrap();
    assert_ne!(new_host, host);
    assert_eq!(worker_story(&got, 0), ["restarting", "starting", "ready", "stopping", "stopped"], "{got:#?}");
    for thread in [1, 2] {
        assert!(
            got.iter().any(|e| is_worker(e, thread, "ready") && e["pid"] == new_host),
            "Worker {thread} of the new host: {got:#?}"
        );
    }
    let old: Vec<&str> =
        got.iter().filter(|e| e["worker"] == 0 && e["pid"] == host).map(|e| e["event"].as_str().unwrap()).collect();
    assert_eq!(old, ["stopping", "stopped"], "{got:#?}");
}

/// A subscriber that never reads (its socket buffer full of log events) must
/// cost the supervisor nothing but its own events: other requests answer at
/// once, memory stays flat, and it is dropped after the write timeout.
#[test]
fn a_subscriber_that_never_reads_does_not_stall_the_supervisor() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = simple("sub-stuck", port, 1, "[logging]\nlevel = \"debug\"\n");
    let w = Warden::start_opts("sub-stuck", port, &cfg, &[("FIXTURE_SPAM", "1")], true);
    w.wait_for("1 ready worker", T, ready(1));
    let t0 = Instant::now();
    let mut stuck = std::os::unix::net::UnixStream::connect(w.socket()).unwrap();
    writeln!(stuck, r#"{{"cmd":"subscribe","interval_ms":250,"logs":true}}"#).unwrap();
    std::thread::sleep(Duration::from_millis(1500)); // its socket buffer fills up

    let mut lat: Vec<Duration> = (0..20)
        .map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            let (d, out) = w.request(r#"{"cmd":"status"}"#);
            assert!(out.contains("\"ok\":true"), "{out}");
            d
        })
        .collect();
    lat.sort();
    eprintln!("status latency with a stuck subscriber: p50 {:?}, max {:?}", lat[10], lat[19]);
    assert!(lat[10] < Duration::from_millis(100), "p50 {:?}", lat[10]);
    assert!(lat[19] < Duration::from_millis(500), "max {:?}", lat[19]);
    let rss = w.rss_kb();
    eprintln!("warden RSS with a stuck subscriber: {rss} kB");
    assert!(rss > 0 && rss < 40 * 1024, "RSS {rss} kB");

    // Not read for REQUEST_TIMEOUT (5 s): disconnected. What it had buffered
    // is still readable, then EOF.
    std::thread::sleep(Duration::from_secs(7).saturating_sub(t0.elapsed()));
    stuck.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let (mut total, mut buf, t1) = (0usize, vec![0u8; 64 * 1024], Instant::now());
    let mut first = Vec::new();
    loop {
        match stuck.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if first.len() < 4096 {
                    first.extend_from_slice(&buf[..n]);
                }
                total += n;
            }
            Err(e) => panic!("no EOF: the stuck subscriber was not disconnected ({e}, {total} bytes read)"),
        }
        assert!(t1.elapsed() < Duration::from_secs(10), "still streaming: the stuck subscriber was not disconnected");
    }
    let first = String::from_utf8_lossy(&first);
    assert!(first.starts_with(r#"{"type":"hello""#), "{first}");
    eprintln!("the stuck subscriber had {total} bytes buffered when it was dropped");
    assert_eq!(w.status().unwrap()["workers"][0]["state"], "RUNNING");
    let (_, out) = w.cli(&["logs", "--events", "-n", "500"]);
    assert!(out.contains("event subscriber stopped reading; disconnected it"), "{out}");
}

/// EOF without `bye` is how `wardend` tells a crash from an exit on purpose.
#[test]
fn a_supervisor_that_dies_says_no_bye() {
    if !have_bun() {
        return;
    }
    // The event loop panics on its 8th tick.
    let (mut w, _port) = fault_warden("sub-panic", "tick:8", 1);
    w.wait_for("1 ready worker", T, ready(1));
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe"}"#);
    assert_eq!(ev.next()["type"], "hello");
    let rest = ev.rest();
    assert!(!rest.iter().any(|e| e["type"] == "bye"), "{rest:#?}");
    assert_ne!(wait_exit(&mut w), Some(0));
    assert!(w.log().contains("warden panicked at"), "{}", w.log());
}

// ---- worker_output = "direct"

/// A scratch directory next to (not inside) a Warden's, which `start` wipes.
fn direct_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("warden-it-{name}-direct-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Output of every shape: empty lines, CRLF, bytes that aren't UTF-8,
/// a line far longer than a pipe (300 KB), and a last line without `\n`.
fn awkward_output(lines: usize) -> Vec<u8> {
    let mut data = Vec::new();
    for i in 0..lines {
        match i % 11 {
            0 => {}
            1 => data.extend_from_slice(b"crlf line\r"),
            2 => data.extend_from_slice(&[0xff, 0xfe, b' ', 0xc3]),
            _ => data.extend(format!("line {i} {}", "ab".repeat(i % 97)).bytes()),
        }
        if i == lines / 2 {
            data.extend(std::iter::repeat_n(b'x', 300_000));
        }
        data.push(b'\n');
    }
    data.extend_from_slice(b"final line without newline");
    data
}

fn direct_config(name: &str, script: &str, logging: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"{script}\"]\n\
         [workers]\nmin_uptime = 100\n[logging]\nworker_output = \"direct\"\n{logging}"
    )
}

/// Wait until `f` holds; panic with `what` and the Warden log otherwise.
fn eventually(w: &Warden, what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        if t0.elapsed() > T {
            panic!("timed out waiting for {what}\nlog:\n{}", w.log());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn direct_output_is_written_byte_for_byte() {
    let dir = direct_dir("direct-bytes");
    let (input, out, err) = (dir.join("input.bin"), dir.join("logs/out.log"), dir.join("logs/err.log"));
    let data = awkward_output(3000);
    std::fs::write(&input, &data).unwrap();
    let script = format!("cat {}; echo oops >&2; exec sleep 300", input.display());
    let logging = format!("out_file = \"{}\"\nerr_file = \"{}\"\n", out.display(), err.display());
    let w = Warden::start("direct-bytes", 0, &direct_config("direct-bytes", &script, &logging));
    eventually(&w, "the whole output in out.log", || std::fs::read(&out).is_ok_and(|b| b.len() >= data.len()));
    assert!(std::fs::read(&out).unwrap() == data, "out.log differs from what the worker wrote");
    eventually(&w, "stderr in err.log", || std::fs::read(&err).is_ok_and(|b| b == b"oops\n"));
    // Nothing went through Warden's own output.
    let log = w.log();
    assert!(!log.contains("final line without newline") && !log.contains(" OUT "), "{log}");
    drop(w);

    // Only out_file: stderr joins it through the same pipe, in the order written.
    let merged = dir.join("merged.log");
    let script = "echo one; echo two >&2; echo three; exec sleep 300";
    let logging = format!("out_file = \"{}\"\n", merged.display());
    let w = Warden::start("direct-merged", 0, &direct_config("direct-merged", script, &logging));
    eventually(&w, "stdout and stderr in one file", || {
        std::fs::read_to_string(&merged).is_ok_and(|t| t == "one\ntwo\nthree\n")
    });
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_output_rotates_losslessly_at_line_boundaries() {
    let dir = direct_dir("direct-rotate");
    let (input, out) = (dir.join("input.bin"), dir.join("logs/out.log"));
    let data = awkward_output(8000);
    std::fs::write(&input, &data).unwrap();
    let script = format!("cat {}; exec sleep 300", input.display());
    let logging = format!("out_file = \"{}\"\n[logging.rotate]\nmax_size = \"4K\"\nkeep = 1000\n", out.display());
    let w = Warden::start("direct-rotate", 0, &direct_config("direct-rotate", &script, &logging));
    // Rotated files oldest first (out.log.N … out.log.1), then out.log.
    let chain = || -> Vec<PathBuf> {
        let mut rotated: Vec<(u32, PathBuf)> = std::fs::read_dir(dir.join("logs"))
            .map(|rd| {
                rd.filter_map(|e| {
                    let p = e.ok()?.path();
                    let n = p.file_name()?.to_str()?.strip_prefix("out.log.")?.parse().ok()?;
                    Some((n, p))
                })
                .collect()
            })
            .unwrap_or_default();
        rotated.sort_by_key(|r| std::cmp::Reverse(r.0));
        rotated.into_iter().map(|r| r.1).chain([out.clone()]).collect()
    };
    let total = || chain().iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum::<u64>();
    eventually(&w, "every byte in the rotated files", || total() >= data.len() as u64);
    let files = chain();
    assert!(files.len() > 100, "{} files", files.len());
    let mut all = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let bytes = std::fs::read(f).unwrap();
        if i + 1 < files.len() {
            assert!(bytes.ends_with(b"\n"), "{} does not end with a newline", f.display());
            assert!(bytes.len() >= 4096, "{} rotated before max_size: {} bytes", f.display(), bytes.len());
            // Late by no more than the line that reached the limit.
            let last_line = bytes[..bytes.len() - 1].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
            assert!(last_line < 4096, "{} rotated late: {last_line} bytes before its last line", f.display());
        }
        all.extend(bytes);
    }
    assert_eq!(all.len(), data.len());
    assert!(all == data, "the rotated files together differ from what the worker wrote");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The pumps run on Warden's output thread: a panic there still kills the
/// worker so it restarts with a working pump (its output would block).
#[test]
fn direct_output_pump_failure_restarts_the_worker() {
    let dir = direct_dir("direct-fault");
    let out = dir.join("out.log");
    let script = "echo started; exec sleep 300";
    let cfg = direct_config("direct-fault", script, &format!("out_file = \"{}\"\n", out.display()));
    let w = Warden::start_env("direct-fault", 0, &cfg, &[("WARDEN_FAULT", "stdout:1")]);
    w.wait_for("a restarted worker", T, |s| s["workers_ready"] == 1 && s["workers"][0]["crashes"] == 1);
    assert!(w.log().contains("output reader failed"), "{}", w.log());
    eventually(&w, "output from the restarted worker", || {
        std::fs::read_to_string(&out).is_ok_and(|t| t.contains("started\n"))
    });
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_output_needs_a_file_per_worker_process() {
    let dir = direct_dir("direct-check");
    let cfg = dir.join("warden.toml");
    let base = "[app]\nname = \"dc\"\ncommand = \"sh\"\n[workers]\ncount = 2\n[logging]\nworker_output = \"direct\"\n\
                out_file = \"/tmp/dc-out.log\"\n";
    std::fs::write(&cfg, base).unwrap();
    let out = Command::new(BIN).arg("check").arg("-c").arg(&cfg).output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("workers.count = 2") && text.contains("per_worker_files = true"), "{text}");
    std::fs::write(&cfg, format!("{base}per_worker_files = true\n")).unwrap();
    let out = Command::new(BIN).arg("check").arg("-c").arg(&cfg).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_output_flush_and_logs() {
    let dir = direct_dir("direct-logs");
    let out = dir.join("logs/out.log");
    let script = "i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.05; done";
    let logging = format!("out_file = \"{}\"\nerr_file = \"{}\"\n", out.display(), dir.join("logs/err.log").display());
    let w = Warden::start("direct-logs", 0, &direct_config("direct-logs", script, &logging));
    let ticks = || -> Vec<u64> {
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        assert!(!text.contains('\0'), "a hole in the file: {text:?}");
        text.lines().filter_map(|l| l.strip_prefix("tick ")?.parse().ok()).collect()
    };
    eventually(&w, "ticks in out.log", || ticks().len() >= 5);

    // `warden logs`: the file's last lines, as worker output, next to Warden's events.
    let (code, text) = w.cli(&["logs", "--nostream", "-n", "5"]);
    assert_eq!(code, 0, "{text}");
    assert_eq!(text.lines().count(), 5, "{text}");
    assert!(text.lines().all(|l| l.contains(" OUT   worker=1 stdout: tick ")), "{text}");
    let (_, text) = w.cli(&["logs", "--nostream", "-n", "200"]);
    assert!(text.contains("INFO  worker ready") && text.contains("stdout: tick "), "{text}");
    let (_, text) = w.cli(&["logs", "--nostream", "--events", "-n", "200"]);
    assert!(!text.contains(" OUT "), "{text}");

    // `-f` shows new lines as they are written.
    let before = *ticks().last().unwrap();
    let mut follow =
        Command::new(BIN).args(["logs", "-f", "-n", "1", "-c"]).arg(&w.cfg).stdout(Stdio::piped()).spawn().unwrap();
    let mut reader = std::io::BufReader::new(follow.stdout.take().unwrap());
    let mut newer = None;
    let t0 = Instant::now();
    while newer.is_none() && t0.elapsed() < T {
        let mut line = String::new();
        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
            break;
        }
        let n: Option<u64> = line.trim_end().rsplit_once("stdout: tick ").and_then(|(_, n)| n.parse().ok());
        newer = n.filter(|n| *n > before + 2);
    }
    let _ = follow.kill();
    let _ = follow.wait();
    assert!(newer.is_some(), "`warden logs -f` showed no new line after tick {before}");

    // `warden flush` empties the file; writing goes on from its start.
    let before = *ticks().last().unwrap();
    let (code, text) = w.cli(&["flush"]);
    assert_eq!(code, 0, "{text}");
    let after = ticks();
    assert!(after.first().is_none_or(|n| *n > before), "old lines survived the flush: {after:?}");
    eventually(&w, "new ticks after the flush", || ticks().len() >= 3);
    let after = ticks();
    assert!(after[0] > before && after.windows(2).all(|p| p[1] == p[0] + 1), "{after:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- startup (boot and crash survival)

/// Fake systemctl, loginctl and launchctl that log their arguments to
/// `calls.log` and succeed, except for the (program, argument text, exit
/// code, error line) cases in `fail`.
struct Fakes {
    home: PathBuf,
}

impl Fakes {
    fn new(f: &Fleet, fail: &[(&str, &str, i32, &str)]) -> Fakes {
        let bin = f.home.join("fakebin");
        std::fs::create_dir_all(&bin).unwrap();
        for d in ["units", "user-units", "sysctl", "linger", "launchd"] {
            std::fs::create_dir_all(f.home.join(d)).unwrap();
        }
        let log = f.home.join("calls.log");
        let _ = std::fs::remove_file(&log);
        for prog in ["systemctl", "loginctl", "launchctl"] {
            let mut s = format!("#!/bin/sh\necho \"{prog} $*\" >> '{}'\ncase \"$*\" in\n", log.display());
            for (p, pat, code, err) in fail {
                if *p == prog {
                    s += &format!("  *'{pat}'*) echo '{err}' >&2; exit {code};;\n");
                }
            }
            s += "esac\nexit 0\n";
            let path = bin.join(prog);
            std::fs::write(&path, s).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        }
        Fakes { home: f.home.clone() }
    }

    fn env(&self) -> Vec<(String, String)> {
        let p = |s: &str| self.home.join(s).display().to_string();
        vec![
            ("WARDEN_SYSTEMCTL".into(), p("fakebin/systemctl")),
            ("WARDEN_LOGINCTL".into(), p("fakebin/loginctl")),
            ("WARDEN_UNIT_DIR".into(), p("units")),
            ("WARDEN_USER_UNIT_DIR".into(), p("user-units")),
            ("WARDEN_SYSCTL_DIR".into(), p("sysctl")),
            ("WARDEN_LINGER_DIR".into(), p("linger")),
            ("USER".into(), "wdtester".into()),
        ]
    }

    /// With launchctl instead of systemctl (what macOS has).
    fn launchd_env(&self) -> Vec<(String, String)> {
        let p = |s: &str| self.home.join(s).display().to_string();
        vec![("WARDEN_LAUNCHCTL".into(), p("fakebin/launchctl")), ("WARDEN_LAUNCHD_DIR".into(), p("launchd"))]
    }

    /// The calls logged since the last `take`.
    fn take(&self) -> String {
        let log = self.home.join("calls.log");
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        text
    }

    fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.home.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }
}

fn run_with(f: &Fleet, args: &[&str], env: &[(String, String)]) -> (i32, String) {
    let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    f.cli_env(args, &env)
}

/// An app whose config sits where `warden@.service` reads it (not running).
fn sleeper_config(f: &Fleet, name: &str) {
    let cfg = format!("[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n");
    std::fs::write(f.home.join(format!("{name}.toml")), cfg).unwrap();
}

#[test]
fn startup_installs_system_units_and_wardend() {
    let f = Fleet::new("st-system");
    sleeper_config(&f, "api");
    let fakes = Fakes::new(&f, &[]);
    let (code, out) = run_with(&f, &["startup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    for c in
        ["systemctl daemon-reload", "systemctl enable warden@api.service", "systemctl enable --now wardend.service"]
    {
        assert!(calls.lines().any(|l| l == c), "{c:?} in:\n{calls}");
    }
    assert!(!calls.contains("--user") && !calls.contains("loginctl"), "{calls}");
    let unit = fakes.read("units/warden@.service");
    let config = format!("\"{}/%i.toml\"", f.home.display());
    assert!(unit.contains(&format!("ExecStart=\"{BIN}\" start --config {config}")), "{unit}");
    assert!(unit.contains("User=www-data") && unit.contains("WantedBy=multi-user.target"), "{unit}");
    let wardend = fakes.read("units/wardend.service");
    assert!(wardend.contains(&format!("ExecStart=\"{BIN}\" daemon\n")), "no --resurrect under systemd:\n{wardend}");
    assert!(wardend.contains("KillMode=process") && wardend.contains("Restart=always"), "{wardend}");
    assert!(f.home.join("sysctl/99-warden.conf").exists());
    assert!(out.contains("wardend.service: enabled and started"), "{out}");

    // A saved app whose config is not where the unit reads it: told how to fix it.
    std::fs::create_dir_all(f.home.join("elsewhere")).unwrap();
    std::fs::write(f.home.join("elsewhere/web.toml"), "[app]\nname = \"web\"\ncommand = \"true\"\n").unwrap();
    std::fs::create_dir_all(f.home.join("state")).unwrap();
    let saved = serde_json::json!({"version": 1, "saved_at": "now", "apps": [
        {"name": "api", "config": f.home.join("api.toml"), "workers": 1, "stopped": false},
        {"name": "web", "config": f.home.join("elsewhere/web.toml"), "workers": 1, "stopped": false},
    ]});
    std::fs::write(f.home.join("state/dump.json"), saved.to_string()).unwrap();
    let (code, out) = run_with(&f, &["startup", "--system"], &fakes.env());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("warden@.service: unchanged"), "{out}");
    let fix = format!("ln -s {} {}", f.home.join("elsewhere/web.toml").display(), f.home.join("web.toml").display());
    assert!(out.contains(&fix) && out.contains("will not come back after a reboot"), "{out}");
    let calls = fakes.take();
    assert!(calls.contains("enable warden@api.service") && !calls.contains("warden@web"), "{calls}");
    std::fs::remove_file(f.home.join("state/dump.json")).unwrap();

    // unstartup: the units disabled and removed; the apps are left alone.
    let (code, out) = run_with(&f, &["unstartup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    for c in
        ["systemctl disable warden@api.service", "systemctl disable --now wardend.service", "systemctl daemon-reload"]
    {
        assert!(calls.lines().any(|l| l == c), "{c:?} in:\n{calls}");
    }
    assert!(!f.home.join("units/warden@.service").exists() && !f.home.join("units/wardend.service").exists());

    // A wardend already running outside systemd hands over to the unit.
    let d = Wardend::start(&f, &[]);
    let fakes = Fakes::new(&f, &[("systemctl", "is-active", 3, "")]);
    let (code, out) = run_with(&f, &["startup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("wardend: stopped the one running outside wardend.service"), "{out}");
    let t0 = Instant::now();
    while d.try_request(r#"{"cmd":"hello"}"#).is_some() {
        assert!(t0.elapsed() < T, "the old wardend kept running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn kill_and_delete_after_startup_stop_a_supervisor_running_outside_its_unit() {
    // Review R1: after `warden startup` installed a unit for an app that
    // still runs in the background, `systemctl stop` would succeed on the
    // inactive unit and leave the app running.
    let f = Fleet::new("st-outside");
    sleeper_config(&f, "api");
    sleeper_config(&f, "web");
    let fakes = Fakes::new(&f, &[]);
    let env = fakes.env();
    let (code, out) = run_with(&f, &["start", "all"], &env);
    assert_eq!(code, 0, "{out}");
    // System units as root, user units otherwise (as `startup` picks).
    let (code, out) = run_with(&f, &["startup"], &env);
    assert_eq!(code, 0, "{out}");
    fakes.take();

    let (code, out) = run_with(&f, &["kill", "api", "--yes"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(!f.home.join("run/api/control.sock").exists(), "api still running after: {out}");
    assert!(f.app("api")["status"].is_null(), "{out}");
    let calls = fakes.take();
    assert!(!calls.contains("stop warden@api"), "went through the inactive unit:\n{calls}");

    // delete: stopped directly, and the installed unit disabled so it
    // doesn't come back at the next boot.
    let (code, out) = run_with(&f, &["delete", "web"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(!f.home.join("run/web/control.sock").exists(), "web still running after: {out}");
    let calls = fakes.take();
    let disable =
        |l: &str| l == "systemctl disable warden@web.service" || l == "systemctl --user disable warden@web.service";
    assert!(calls.lines().any(disable), "{calls}");
    assert!(out.contains("warden@web.service disabled"), "{out}");
}

#[test]
fn startup_installs_user_units_and_lingering() {
    let f = Fleet::new("st-user");
    sleeper_config(&f, "api");
    let fakes = Fakes::new(&f, &[]);
    let (code, out) = run_with(&f, &["startup", "--user"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    for c in [
        "systemctl --user daemon-reload",
        "systemctl --user enable warden@api.service",
        "systemctl --user enable --now wardend.service",
        "loginctl enable-linger wdtester",
    ] {
        assert!(calls.lines().any(|l| l == c), "{c:?} in:\n{calls}");
    }
    let unit = fakes.read("user-units/warden@.service");
    for gone in ["User=", "Group=", "LimitNOFILE", "network-online", "multi-user.target"] {
        assert!(!unit.contains(gone), "{gone} in a user unit:\n{unit}");
    }
    assert!(unit.contains("WantedBy=default.target") && unit.contains("Environment=\"PATH="), "{unit}");
    assert!(unit.contains(&format!("Environment=\"WARDEN_HOME={}\"", f.home.display())), "{unit}");
    let wardend = fakes.read("user-units/wardend.service");
    assert!(wardend.contains("WantedBy=default.target") && !wardend.contains("--resurrect"), "{wardend}");
    assert!(!f.home.join("units/warden@.service").exists(), "nothing in the system directory");
    assert!(out.contains("lingering enabled for wdtester"), "{out}");

    // Lingering already on: loginctl is not asked again.
    std::fs::write(f.home.join("linger/wdtester"), "").unwrap();
    let (code, out) = run_with(&f, &["startup", "--user"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("lingering is on for wdtester") && !fakes.take().contains("loginctl"), "{out}");
    std::fs::remove_file(f.home.join("linger/wdtester")).unwrap();

    // Lingering needs privileges: the exact command, and why.
    let fakes = Fakes::new(&f, &[("loginctl", "enable-linger", 1, "Access denied")]);
    let (code, out) = run_with(&f, &["startup", "--user"], &fakes.env());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Access denied") && out.contains("sudo loginctl enable-linger wdtester"), "{out}");
    assert!(out.contains("only when you log in"), "{out}");

    // No user manager to talk to (no login session).
    let fakes =
        Fakes::new(&f, &[("systemctl", "--user daemon-reload", 1, "Failed to connect to bus: No medium found")]);
    let (code, out) = run_with(&f, &["startup", "--user"], &fakes.env());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("No medium found") && out.contains("user manager did not answer"), "{out}");
    assert!(!fakes.take().contains("enable"), "nothing enabled");

    // unstartup --user.
    let fakes = Fakes::new(&f, &[]);
    let (code, out) = run_with(&f, &["unstartup", "--user"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    assert!(calls.contains("systemctl --user disable --now wardend.service"), "{calls}");
    assert!(!f.home.join("user-units/warden@.service").exists() && out.contains("lingering stays on"), "{out}");
}

#[test]
fn startup_writes_a_launchd_job() {
    let f = Fleet::new("st-launchd");
    let fakes = Fakes::new(&f, &[]);
    let uid = unsafe { libc::getuid() };
    let plist = "launchd/io.github.oceanwap.warden.daemon.plist";
    for (flag, domain) in [("--system", "system".to_string()), ("--user", format!("gui/{uid}"))] {
        let (code, out) = run_with(&f, &["startup", flag], &fakes.launchd_env());
        assert_eq!(code, 0, "{out}");
        let calls = fakes.take();
        let target = format!("{domain}/io.github.oceanwap.warden.daemon");
        assert!(calls.contains(&format!("launchctl print {target}")), "{calls}");
        let bootstrap = format!("launchctl bootstrap {domain} {}", f.home.join(plist).display());
        assert!(calls.lines().any(|l| l == bootstrap), "{bootstrap:?} in:\n{calls}");
        let p = fakes.read(plist);
        let flat: String = p.split_whitespace().collect();
        assert!(
            flat.contains(&format!("<string>{BIN}</string><string>daemon</string><string>--resurrect</string>")),
            "{p}"
        );
        assert!(flat.contains("<key>RunAtLoad</key><true/>") && flat.contains("<key>KeepAlive</key>"), "{p}");
        assert!(flat.contains("<key>PATH</key>") && p.contains("state/logs/wardend.log"), "{p}");
        assert!(out.contains(&format!("{target}: loaded")), "{out}");
    }

    // Loading needs a desktop session: the plist stays, and the fix says so.
    let fakes = Fakes::new(
        &f,
        &[("launchctl", "bootstrap", 125, "Bootstrap failed: 125: Domain does not support specified action")],
    );
    let (code, out) = run_with(&f, &["startup", "--user"], &fakes.launchd_env());
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Domain does not support") && out.contains("sudo warden startup"), "{out}");

    let fakes = Fakes::new(&f, &[]);
    let (code, out) = run_with(&f, &["unstartup", "--user"], &fakes.launchd_env());
    assert_eq!(code, 0, "{out}");
    assert!(fakes.take().contains(&format!("launchctl bootout gui/{uid}/io.github.oceanwap.warden.daemon")));
    assert!(!f.home.join(plist).exists(), "{out}");
}

#[test]
fn startup_without_a_service_manager_says_what_to_run() {
    if std::path::Path::new("/run/systemd/system").exists() || cfg!(target_os = "macos") {
        eprintln!("skipping: this host has a service manager");
        return;
    }
    let f = Fleet::new("st-none");
    let (code, out) = f.cli(&["startup"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("warden resurrect") && out.contains("warden daemon --resurrect"), "{out}");
}

#[test]
fn wardend_resurrect_starts_the_saved_apps() {
    let f = Fleet::new("wd-resurrect");
    sleeper_config(&f, "one");
    sleeper_config(&f, "two");
    f.ok(&["start", "all"]);
    f.ok(&["save"]);
    f.ok(&["kill", "--yes"]);
    f.wait("all offline", |f| f.list().iter().all(|a| a["status"].is_null()));
    // `two` is running already: it must not be started twice.
    f.ok(&["start", "two"]);
    let two = supervisor_pid(&f, "two");

    let d = Wardend::start_args(&f, &["daemon", "--resurrect"], &[]);
    let a = d.wait_app("one resurrected", "one", |a| a["state"] == "running");
    assert_eq!(a["supervised_by"], "wardend", "{a:#}");
    f.wait("one online", |f| f.app("one")["status"]["workers_ready"] == 1);
    assert_eq!(supervisor_pid(&f, "two"), two, "not started twice");
    let log = d.log();
    assert!(log.contains("resurrected a saved app app=one"), "{log}");
    assert!(!log.contains("resurrected a saved app app=two"), "{log}");

    // Once per boot: a wardend restarted later (launchd's KeepAlive after a
    // crash) leaves apps stopped since then stopped.
    drop(d);
    f.ok(&["kill", "one", "--yes"]);
    f.wait("one offline", |f| f.app("one")["status"].is_null());
    let d = Wardend::start_args(&f, &["daemon", "--resurrect"], &[]);
    f.wait("second resurrect skipped", |_| d.log().contains("already resurrected this boot"));
    std::thread::sleep(Duration::from_millis(500));
    assert!(f.app("one")["status"].is_null(), "stopped app came back:\n{}", d.log());
}

#[test]
fn wardend_start_uses_the_systemd_unit_and_kill_stops_the_wardend_unit() {
    let f = Fleet::new("wd-units");
    sleeper_config(&f, "api");
    let fakes = Fakes::new(&f, &[]);
    for dir in ["units", "user-units"] {
        std::fs::write(f.home.join(dir).join("warden@.service"), "[Service]\n").unwrap();
        std::fs::write(f.home.join(dir).join("wardend.service"), "[Service]\n").unwrap();
    }
    let env = fakes.env();
    let envs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let d = Wardend::start(&f, &envs);
    let r = d.request(r#"{"cmd":"start","app":"api"}"#);
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["message"].as_str().unwrap().starts_with("starting warden@api.service"), "{r}");
    assert!(fakes.take().contains("start --no-block warden@api.service"));

    // `warden kill`: wardend runs as a unit with Restart=always, so the unit is stopped.
    let (code, out) = f.cli_env(&["kill", "--yes"], &envs);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("wardend: stopped wardend.service"), "{out}");
    assert!(fakes.take().contains("stop wardend.service"));
}

// ---- long-lived connections
//
// WebSocket and SSE clients held through `warden reload`: the old workers
// must end them cleanly (close 1001, a terminated chunked stream) within
// shutdown.long_lived_timeout, and the clients reconnect to new workers.

/// How a long-lived connection ended, as its client saw it.
#[derive(Debug, Clone, PartialEq)]
enum Ended {
    /// Still open when the test stopped watching (after the reload).
    Open,
    /// A WebSocket close frame with this code, answered, then FIN.
    WsClose(u16),
    /// The chunked SSE stream terminated; `complete`: after a whole event.
    SseEnd { complete: bool },
    /// Anything else: EOF without a close frame (WebSocket 1006) or without
    /// the last chunk, a reset, an error.
    Broken(String),
}

#[derive(Debug)]
struct Session {
    /// "<pid>:<thread>" (Bun) or "<pid>" (Node), from the first message / event.
    who: String,
    ended: Ended,
}

impl Session {
    fn pid(&self) -> u64 {
        self.who.split(':').next().and_then(|p| p.parse().ok()).unwrap_or(0)
    }
}

/// Reads into `buf` until `parse` takes what it needs from it. `Ok(None)`:
/// `stop` was set first. `Err`: EOF, a reset or another error.
fn read_for<T>(
    s: &mut TcpStream,
    buf: &mut Vec<u8>,
    stop: Option<&AtomicBool>,
    deadline: Instant,
    mut parse: impl FnMut(&mut Vec<u8>) -> Option<T>,
) -> Result<Option<T>, String> {
    let mut tmp = [0u8; 8192];
    loop {
        if let Some(t) = parse(buf) {
            return Ok(Some(t));
        }
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            return Ok(None);
        }
        if Instant::now() > deadline {
            return Err("timed out".into());
        }
        match s.read(&mut tmp) {
            Ok(0) => return Err("EOF".into()),
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => return Err("connection reset".into()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn http_head(b: &mut Vec<u8>) -> Option<String> {
    let i = b.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&b[..i]).to_string();
    b.drain(..i + 4);
    Some(head)
}

/// One server frame (unmasked) off the front of `buf`: (opcode, payload).
fn ws_frame(b: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if b.len() < 2 {
        return None;
    }
    let (len, off) = match b[1] & 0x7f {
        126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
        127 if b.len() >= 10 => (u64::from_be_bytes(b[2..10].try_into().unwrap()) as usize, 10),
        126 | 127 => return None,
        n => (n as usize, 2),
    };
    if b.len() < off + len {
        return None;
    }
    let frame = (b[0] & 0x0f, b[off..off + len].to_vec());
    b.drain(..off + len);
    Some(frame)
}

/// A client frame (masked, short payload).
fn ws_client_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let mut f = vec![0x80 | op, 0x80 | payload.len() as u8, mask[0], mask[1], mask[2], mask[3]];
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

fn connect_long_lived(port: u16, request: &str) -> Result<TcpStream, String> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("connect: {e}"))?;
    s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    s.write_all(request.as_bytes()).map_err(|e| format!("write: {e}"))?;
    Ok(s)
}

/// A WebSocket on /ws until the server closes it (answered like a browser
/// does) or `stop` is set. `hello` runs once the server's first message is in.
fn ws_session(port: u16, stop: &AtomicBool, hello: &dyn Fn()) -> Result<Session, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut s = connect_long_lived(
        port,
        "GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )?;
    let mut buf = Vec::new();
    let head = read_for(&mut s, &mut buf, None, deadline, http_head)?.unwrap_or_default();
    if !head.starts_with("HTTP/1.1 101") {
        return Err(format!("no upgrade: {head}"));
    }
    let who = match read_for(&mut s, &mut buf, None, deadline, ws_frame)? {
        Some((1, p)) => String::from_utf8_lossy(&p).to_string(),
        other => return Err(format!("expected the server's hello, got {other:?}")),
    };
    hello();
    let ended = loop {
        match read_for(&mut s, &mut buf, Some(stop), deadline, ws_frame) {
            Ok(None) => break Ended::Open,
            Ok(Some((8, p))) => {
                let code = if p.len() >= 2 { u16::from_be_bytes([p[0], p[1]]) } else { 1005 };
                let _ = s.write_all(&ws_client_frame(8, &p[..p.len().min(2)]));
                break match read_for(&mut s, &mut buf, None, deadline, |_| None::<()>) {
                    Err(e) if e == "EOF" => Ended::WsClose(code),
                    Err(e) => Ended::Broken(format!("close {code}, then {e}")),
                    Ok(_) => Ended::Broken("unreachable".into()),
                };
            }
            Ok(Some(_)) => {}
            Err(e) => break Ended::Broken(format!("{e} without a close frame (a browser reports 1006)")),
        }
    };
    Ok(Session { who, ended })
}

/// One chunk of a chunked body off the front of `buf` (empty: the last one).
fn http_chunk(b: &mut Vec<u8>) -> Option<Result<Vec<u8>, String>> {
    let i = b.windows(2).position(|w| w == b"\r\n")?;
    let line = String::from_utf8_lossy(&b[..i]).to_string();
    let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) else {
        return Some(Err(format!("bad chunk size line {line:?}")));
    };
    if b.len() < i + 2 + size + 2 {
        return None;
    }
    let data = b[i + 2..i + 2 + size].to_vec();
    b.drain(..i + 2 + size + 2);
    Some(Ok(data))
}

/// An SSE stream (an EventSource's request) until the server ends it or
/// `stop` is set. `hello` runs once the first event is in.
fn sse_session(port: u16, path: &str, stop: &AtomicBool, hello: &dyn Fn()) -> Result<Session, String> {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut s = connect_long_lived(
        port,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
        ),
    )?;
    let mut buf = Vec::new();
    let head = read_for(&mut s, &mut buf, None, deadline, http_head)?.unwrap_or_default();
    let lower = head.to_ascii_lowercase();
    if !head.starts_with("HTTP/1.1 200")
        || !lower.contains("content-type: text/event-stream")
        || !lower.contains("transfer-encoding: chunked")
    {
        return Err(format!("not a chunked event stream: {head}"));
    }
    let mut body = String::new();
    let mut who: Option<String> = None;
    let ended = loop {
        match read_for(&mut s, &mut buf, who.as_ref().map(|_| stop), deadline, http_chunk) {
            Ok(None) => break Ended::Open,
            Ok(Some(Err(e))) => break Ended::Broken(e),
            Ok(Some(Ok(data))) if data.is_empty() => break Ended::SseEnd { complete: body.ends_with("\n\n") },
            Ok(Some(Ok(data))) => {
                body.push_str(&String::from_utf8_lossy(&data));
                if who.is_none() {
                    // "id: 0\ndata: <who> 0\n\n"
                    if let Some(w) = body.lines().find_map(|l| l.strip_prefix("data: ")) {
                        who = w.split(' ').next().map(str::to_string);
                        hello();
                    }
                }
            }
            Err(e) => break Ended::Broken(format!("{e} before the stream's last chunk")),
        }
    };
    Ok(Session { who: who.unwrap_or_default(), ended })
}

/// A client that reconnects at once whenever its connection ends, until it
/// sits on a connection still open when `stop` is set.
fn hold(
    port: u16,
    path: &'static str,
    stop: Arc<AtomicBool>,
    connected: Arc<AtomicUsize>,
) -> std::thread::JoinHandle<Vec<Result<Session, String>>> {
    std::thread::spawn(move || {
        let mut sessions = Vec::new();
        let first = AtomicBool::new(true);
        let hello = || {
            if first.swap(false, Ordering::Relaxed) {
                connected.fetch_add(1, Ordering::Relaxed);
            }
        };
        while sessions.len() < 20 {
            let r =
                if path == "/ws" { ws_session(port, &stop, &hello) } else { sse_session(port, path, &stop, &hello) };
            let open = matches!(&r, Ok(s) if s.ended == Ended::Open);
            if r.is_err() {
                std::thread::sleep(Duration::from_millis(50));
            }
            sessions.push(r);
            if open || stop.load(Ordering::Relaxed) {
                break;
            }
        }
        sessions
    })
}

struct LongLivedRun {
    before: HashSet<u64>,
    after: HashSet<u64>,
    took: Duration,
    clients: Vec<(&'static str, Vec<Result<Session, String>>)>,
    ok: usize,
    fail: usize,
}

/// One client per path (`/ws` or an SSE path) plus a plain-request loop,
/// held through `warden reload`.
fn reload_holding(w: &Warden, workers: u64, paths: &[&'static str]) -> LongLivedRun {
    let port = w.port;
    let before = pid_set(&w.wait_for("ready", T, ready(workers)));
    let stop = Arc::new(AtomicBool::new(false));
    let connected = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = paths.iter().map(|p| (*p, hold(port, p, stop.clone(), connected.clone()))).collect();
    let t0 = Instant::now();
    while connected.load(Ordering::Relaxed) < paths.len() {
        assert!(t0.elapsed() < T, "long-lived clients did not connect\n{}", w.log());
        std::thread::sleep(Duration::from_millis(20));
    }
    let plain_stop = Arc::new(AtomicBool::new(false));
    let (ok, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let plain = {
        let (stop, ok, fail) = (plain_stop.clone(), ok.clone(), fail.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match get(port, "/whoami") {
                    Some(_) => ok.fetch_add(1, Ordering::Relaxed),
                    None => fail.fetch_add(1, Ordering::Relaxed),
                };
            }
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let t0 = Instant::now();
    let (code, out) = w.cli(&["reload"]);
    let took = t0.elapsed();
    assert_eq!(code, 0, "{out}\n{}", w.log());
    let after = pid_set(&w.status().unwrap());
    // Give the last reconnects a moment to land, then stop watching.
    std::thread::sleep(Duration::from_millis(300));
    plain_stop.store(true, Ordering::Relaxed);
    plain.join().unwrap();
    stop.store(true, Ordering::Relaxed);
    let clients = handles.into_iter().map(|(p, h)| (p, h.join().unwrap())).collect();
    LongLivedRun { before, after, took, clients, ok: ok.load(Ordering::Relaxed), fail: fail.load(Ordering::Relaxed) }
}

/// Every client was ended cleanly by an old worker, reconnected, and ends up
/// on a new one; plain requests didn't fail; the reload didn't wait for
/// grace_period.
fn assert_clean_handover(w: &Warden, run: &LongLivedRun, max_took: Duration) {
    let log = w.log();
    eprintln!("reload took {:?}; plain requests: {} ok, {} failed", run.took, run.ok, run.fail);
    assert!(run.before.is_disjoint(&run.after), "every worker replaced: {:?} -> {:?}", run.before, run.after);
    for (path, sessions) in &run.clients {
        eprintln!("{path}: {sessions:?}");
        let sessions: Vec<&Session> = sessions
            .iter()
            .map(|s| s.as_ref().unwrap_or_else(|e| panic!("{path}: a connection failed: {e}\n{log}")))
            .collect();
        assert!(sessions.len() >= 2, "{path}: never handed over: {sessions:?}\n{log}");
        let (last, ended) = sessions.split_last().unwrap();
        for s in ended {
            let clean = if *path == "/ws" { Ended::WsClose(1001) } else { Ended::SseEnd { complete: true } };
            assert_eq!(s.ended, clean, "{path}: {s:?}\n{log}");
            assert!(run.before.contains(&s.pid()), "{path}: only old workers end connections: {s:?}");
        }
        assert_eq!(last.ended, Ended::Open, "{path}: {last:?}");
        assert!(run.after.contains(&last.pid()), "{path}: reconnected to a new worker: {last:?}, new {:?}", run.after);
    }
    assert!(run.ok > 20, "plain requests kept being served: {} ok", run.ok);
    assert!(run.fail <= allowed_resets(), "plain requests failed during the reload: {}", run.fail);
    assert!(log.contains("closed long-lived connections so their clients reconnect to new workers"), "{log}");
    assert!(!log.contains("did not exit within grace period"), "{log}");
    assert!(run.took < max_took, "reload took {:?} (limit {max_took:?})\n{log}", run.took);
}

fn long_lived_config(name: &str, port: u16, app: &str, extra: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nport = {port}\n{app}\n[restart]\nbackoff_initial = 50\n\
         [shutdown]\ngrace_period = 30\ndrain_ms = 100\nlong_lived_timeout = 1\n{extra}"
    )
}

#[test]
fn long_lived_connections_hand_over_in_a_bun_reload() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let app = format!("args = [\"{}\"]\n[workers]\ncount = 2", fixture("longlived.ts"));
    let w = Warden::start("ll-bun", port, &long_lived_config("ll-bun", port, &app, ""));
    // Every kind of SSE body: a ReadableStream, a `type: "direct"` one, an async generator.
    let run = reload_holding(&w, 2, &["/ws", "/sse", "/sse-direct", "/sse-gen"]);
    // Two workers, each: start (~0.3 s), then at most long_lived_timeout (1 s)
    // + the closing handshakes. Without the shim ending them: grace_period (30 s) each.
    assert_clean_handover(&w, &run, Duration::from_secs(10));
    let log = w.log();
    assert!(log.contains("websockets=1") && log.contains("sse="), "{log}");
}

#[test]
fn long_lived_connections_hand_over_in_a_node_reload() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let app = format!("command = \"node\"\nargs = [\"{}\"]\n[workers]\ncount = 2", fixture("longlived_node.mjs"));
    let w = Warden::start("ll-node", port, &long_lived_config("ll-node", port, &app, ""));
    let run = reload_holding(&w, 2, &["/ws", "/sse"]);
    assert_clean_handover(&w, &run, Duration::from_secs(10));
}

#[test]
fn long_lived_connections_hand_over_in_bun_worker_mode() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let app = format!("entry = \"{}\"\n[workers]\ncount = 2\nmode = \"worker\"", fixture("longlived.ts"));
    let w = Warden::start("ll-threads", port, &long_lived_config("ll-threads", port, &app, ""));
    // The host is replaced as a whole: its Workers drain in parallel.
    let run = reload_holding(&w, 2, &["/ws", "/sse", "/sse-gen"]);
    assert_clean_handover(&w, &run, Duration::from_secs(8));
}

/// Without long-lived connections a drain is what it was: no wait for
/// long_lived_timeout, nothing closed, and normal requests in flight —
/// streamed downloads included — run to their end even past
/// long_lived_timeout (only grace_period bounds them).
#[test]
fn drain_without_long_lived_connections_is_unchanged() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"ll-none\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n\
         [shutdown]\ngrace_period = 30\ndrain_ms = 100\nlong_lived_timeout = 3\n",
        fixture("longlived.ts")
    );
    let w = Warden::start("ll-none", port, &cfg);
    let run = reload_holding(&w, 2, &[]);
    eprintln!("reload took {:?}; plain requests: {} ok, {} failed", run.took, run.ok, run.fail);
    assert!(run.before.is_disjoint(&run.after));
    assert!(run.fail <= allowed_resets(), "plain requests failed: {}", run.fail);
    // Waiting for long_lived_timeout would take >= 2 x 3 s.
    assert!(run.took < Duration::from_secs(5), "reload took {:?}\n{}", run.took, w.log());

    // A download and a slow request in flight through a reload, both longer
    // than long_lived_timeout: they finish, complete.
    let download = std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        write!(s, "GET /download?ms=4000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = Vec::new();
        let r = s.read_to_end(&mut out);
        (r.map_err(|e| e.to_string()), String::from_utf8_lossy(&out).to_string())
    });
    let slow = std::thread::spawn(move || {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        write!(s, "GET /slow?ms=4000 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        let r = s.read_to_string(&mut out);
        (r.map_err(|e| e.to_string()), out)
    });
    std::thread::sleep(Duration::from_millis(300));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let (r, body) = download.join().unwrap();
    assert!(r.is_ok(), "download: {r:?}");
    assert!(body.contains("line 9\nend\n") && body.ends_with("0\r\n\r\n"), "download cut short: {body:?}");
    let (r, body) = slow.join().unwrap();
    assert!(r.is_ok() && body.starts_with("HTTP/1.1 200"), "slow request: {r:?} {body:?}");
    let log = w.log();
    assert!(!log.contains("closed long-lived connections"), "{log}");
}

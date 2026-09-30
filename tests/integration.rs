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
        writeln!(s, "{req}").unwrap();
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
    eprintln!("status latency with a stalled stdout: p50 {:?}, max {:?}", lat[10], lat[19]);
    assert!(lat[10] < Duration::from_millis(100), "p50 {:?}", lat[10]);
    assert!(lat[19] < Duration::from_millis(500), "max {:?}", lat[19]);

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
        let out = Command::new(BIN)
            .args(args)
            .env("WARDEN_HOME", &self.home)
            .env("WARDEN_RUNTIME_DIR", self.home.join("run"))
            .env_remove("WARDEN_CONFIG")
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
    let cfg = format!(
        "[app]\nname = \"chatty\"\ncommand = \"sh\"\n\
         args = [\"-c\", \"for i in $(seq 1 400); do echo \\\"line $i padding padding padding\\\"; done; echo oops >&2; exec sleep 300\"]\n\
         [workers]\nmin_uptime = 300\n[logging]\nfile = \"{}\"\n[logging.rotate]\nmax_size = \"8K\"\nkeep = 3\ncompress = true\n",
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

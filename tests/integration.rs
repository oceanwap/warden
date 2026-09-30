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
            .stdout(log.try_clone().unwrap())
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
    assert_eq!(w.cli(&["restart"]).0, 0);
    let (code, took) = w.terminate(Duration::from_secs(10));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert!(!w.log().contains("INFO  starting all workers"), "must not restart during shutdown");

    // `restart` then `stop`: stays stopped.
    let port = free_port();
    let w = Warden::start("restartstop", port, &gated("restartstop", port, 2, ""));
    w.wait_for("ready", T, ready(2));
    assert_eq!(w.cli(&["restart"]).0, 0);
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

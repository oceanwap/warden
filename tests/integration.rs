//! End-to-end tests: run the real `warden` binary against real Bun processes.
//! Skipped (with a message) when `bun` is not on PATH.

use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_warden");

/// Where tests put their files. macOS's `$TMPDIR` (/var/folders/...) is too
/// long for the Unix socket paths under it (104 bytes at most), so /tmp there.
fn tmp() -> std::path::PathBuf {
    if cfg!(target_os = "macos") { std::path::PathBuf::from("/tmp") } else { std::env::temp_dir() }
}

/// `warden`, kept away from this machine's real launchd job: on macOS every
/// `start`, `kill`, `update` and `startup` would otherwise read, kickstart or
/// rewrite ~/Library/LaunchAgents/io.github.oceanwap.warden.daemon.plist. A
/// test that wants a launchd job sets both variables again (`Fakes::launchd_env`).
fn warden() -> Command {
    let mut cmd = Command::new(BIN);
    if cfg!(target_os = "macos") {
        let dir = tmp().join("warden-tests-launchd");
        cmd.env("WARDEN_LAUNCHD_DIR", &dir).env("WARDEN_LAUNCHCTL", dir.join("no-launchctl"));
    }
    cmd
}

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
    /// The supervisor SIGSTOP went to, for SIGCONT (a stopped one can't say its pid).
    stopped: std::cell::Cell<Option<u32>>,
}

impl Warden {
    /// The directory of a start: `warden-it-<name>-<pid>`, and for each later
    /// start of the same name in this run a new one (`-2`, `-3`...). Two tests
    /// may use the same name at once, and a test that starts a name again
    /// must not share the control socket path with the instance it just
    /// stopped: on linux-arm64 a second `loopnode` there went unreachable
    /// right after its workers were ready.
    fn dir_for(name: &str) -> PathBuf {
        static STARTS: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
        let mut starts = STARTS.lock().unwrap_or_else(|e| e.into_inner());
        starts.push(name.to_string());
        let n = starts.iter().filter(|s| *s == name).count();
        let pid = std::process::id();
        tmp().join(if n == 1 { format!("warden-it-{name}-{pid}") } else { format!("warden-it-{name}-{pid}-{n}") })
    }

    fn start(name: &str, port: u16, toml: &str) -> Warden {
        Self::start_env(name, port, toml, &[])
    }

    fn start_env(name: &str, port: u16, toml: &str, env: &[(&str, &str)]) -> Warden {
        Self::start_opts(name, port, toml, env, false)
    }

    /// `stall_stdout`: Warden's stdout is a pipe nobody reads (a stuck log
    /// consumer); stderr still goes to the log file.
    fn start_opts(name: &str, port: u16, toml: &str, env: &[(&str, &str)], stall_stdout: bool) -> Warden {
        let dir = Warden::dir_for(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("warden.toml");
        let text = format!("{toml}\n[control]\nsocket = \"{}\"\n", dir.join("w.sock").display());
        std::fs::write(&cfg, text).unwrap();
        let log = std::fs::File::create(dir.join("warden.log")).unwrap();
        let child = warden()
            .args(["start", "-c"])
            .arg(&cfg)
            .envs(env.iter().copied())
            .stdout(if stall_stdout { Stdio::piped() } else { Stdio::from(log.try_clone().unwrap()) })
            .stderr(log)
            .spawn()
            .unwrap();
        Warden { child, cfg, dir, port, stopped: Default::default() }
    }

    #[cfg(target_os = "linux")]
    /// Start Warden again on the same config and directory (after its
    /// processes were killed), its log appended to the same file.
    fn start_again(&mut self, env: &[(&str, &str)]) {
        let log = std::fs::OpenOptions::new().append(true).open(self.dir.join("warden.log")).unwrap();
        self.child = Command::new(BIN)
            .args(["start", "-c"])
            .arg(&self.cfg)
            .envs(env.iter().copied())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
    }

    fn cli(&self, args: &[&str]) -> (i32, String) {
        let out = warden().args(args).arg("-c").arg(&self.cfg).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        (out.status.code().unwrap_or(-1), text)
    }

    fn status(&self) -> Option<Value> {
        self.status_or_why().ok()
    }

    /// The status, or what `warden status` said instead (for a wait that times out).
    fn status_or_why(&self) -> Result<Value, String> {
        let (code, out) = self.cli(&["status", "--json"]);
        if code != 0 {
            return Err(format!("exit {code}: {out}"));
        }
        serde_json::from_str(&out).map_err(|e| format!("{e}: {out}"))
    }

    /// How long Warden says it was frozen (SIGSTOP, an overloaded or paused CI
    /// VM): its "event loop was blocked for_ms=N" warnings, summed.
    fn frozen(&self) -> Duration {
        let log = self.log();
        let ms: u64 = log
            .lines()
            .filter(|l| l.contains("event loop was blocked"))
            .filter_map(|l| l.split("for_ms=").nth(1)?.split(|c: char| !c.is_ascii_digit()).next()?.parse::<u64>().ok())
            .sum();
        Duration::from_millis(ms)
    }

    /// The time a wait has: `timeout`, and more for each stretch the host froze
    /// Warden (the way the watchdog forgives it): the test is about what Warden
    /// does, not about the host's pauses. At most a minute extra.
    fn wait_for(&self, what: &str, timeout: Duration, f: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        let mut last = None;
        let mut why = None;
        let mut deadline = timeout;
        while start.elapsed() < deadline {
            match self.status_or_why() {
                Ok(s) if f(&s) => return s,
                Ok(s) => last = Some(s),
                Err(e) => why = Some(e),
            }
            std::thread::sleep(Duration::from_millis(100));
            deadline = timeout + self.frozen().min(Duration::from_secs(60));
        }
        panic!(
            "timed out waiting for {what}; last status: {last:#?}\nlast failed status: {why:?}\nlog:\n{}",
            self.log()
        );
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

    /// To Warden's process, which passes it on to the supervisor (under the
    /// keeper); SIGSTOP and SIGCONT, which no process can pass on, go to the
    /// supervisor itself.
    fn signal(&self, sig: i32) {
        let pid = match sig {
            libc::SIGSTOP => {
                let pid = self.supervisor();
                self.stopped.set(Some(pid));
                pid
            }
            libc::SIGCONT => self.stopped.take().unwrap_or_else(|| self.supervisor()),
            _ => self.child.id(),
        };
        unsafe { libc::kill(pid as i32, sig) };
    }

    /// The supervisor's pid: the keeper's child (`supervisor_pid`), or Warden's own.
    fn supervisor(&self) -> u32 {
        self.status().and_then(|s| s["supervisor_pid"].as_u64()).map_or(self.child.id(), |p| p as u32)
    }

    /// Warden stopped (SIGSTOP) until the returned guard is dropped (SIGCONT,
    /// also when the test panics): nothing it would do meanwhile (reaping,
    /// reading counters) happens, so what the test does in that time is one
    /// instant to it.
    fn freeze(&self) -> Frozen<'_> {
        self.signal(libc::SIGSTOP);
        Frozen(self)
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

/// A frozen Warden ([`Warden::freeze`]); thawed when dropped.
struct Frozen<'a>(&'a Warden);

impl Drop for Frozen<'_> {
    fn drop(&mut self) {
        self.0.signal(libc::SIGCONT);
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
    every_warning_has_a_hint(&w.log());
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

/// The shim's Response and ReadableStream wrappers (Bun, for SSE drains)
/// pass for the native constructors: `constructor`, `toString`, `name`,
/// `length`, statics, `Symbol.hasInstance`, subclasses, the TypeError
/// without `new` (tests/fixtures/shim_response.ts lists every check).
#[test]
fn shim_response_wrapper_passes_for_the_native_one() {
    if !have_bun() {
        return;
    }
    let shim = format!("{}/shim/warden-shim.mjs", env!("CARGO_MANIFEST_DIR"));
    let out = Command::new("bun").args(["--preload", &shim, &fixture("shim_response.ts")]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{text}");
    let checks: Value =
        serde_json::from_str(text.lines().last().unwrap_or("")).unwrap_or_else(|e| panic!("{e}: {text}"));
    assert_eq!(checks["hooked"], true, "the wrappers are not installed: {checks:#}");
    let failed: Vec<&String> =
        checks.as_object().unwrap().iter().filter(|(_, v)| **v != true).map(|(k, _)| k).collect();
    assert!(failed.is_empty(), "the wrapper shows through: {failed:?}\n{checks:#}");
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
    every_warning_has_a_hint(&w.log());
}

/// Every WARN and ERROR line Warden wrote says what to do about it (a
/// `hint=`), as docs/review-process.md (C4) asks. Found missing on several
/// lines by `cargo xtask chaos`. Worker output is the app's, not Warden's.
fn every_warning_has_a_hint(log: &str) {
    let bad: Vec<&str> = log
        .lines()
        .filter(|l| !l.contains(" OUT ") && (l.contains(" WARN ") || l.contains(" ERROR ")))
        .filter(|l| !l.contains(" hint=") && !l.contains("panicked at"))
        .collect();
    assert!(bad.is_empty(), "WARN/ERROR lines without hint=:\n{}", bad.join("\n"));
}

#[test]
fn cli_errors() {
    let dir = tmp().join(format!("warden-it-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("bad.toml");
    std::fs::write(&cfg, "[app]\nname = \"x\"\n[workers]\ncount = 0\n").unwrap();
    let out = warden().args(["check", "-c"]).arg(&cfg).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("workers.count"));

    let out =
        warden().args(["status", "--socket"]).arg(dir.join("nobody.sock")).stdout(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running"), "{out:?}");
    // A socket a killed supervisor left behind refuses: still "not running", and why.
    let left = dir.join("left.sock");
    drop(std::os::unix::net::UnixListener::bind(&left).unwrap());
    let out = warden().args(["status", "--socket"]).arg(&left).stdout(Stdio::null()).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not running (its control socket"), "{out:?}");

    let out = warden().arg("--help").output().unwrap();
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

/// An app that serves TLS itself (no proxy in front) passes the health gates:
/// the shim's private health socket stays plain HTTP instead of copying the
/// app's `tls`, which made every reload fail ("not an HTTP response").
#[test]
fn an_app_serving_tls_itself_passes_health_checks() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("tls", port, 2, "").replace(&fixture("app.ts"), &fixture("tls_app.ts"));
    let w = Warden::start("tls", port, &cfg);
    let before = pid_set(&w.wait_for("ready", T, ready(2)));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before), "every worker must be new: {out}");
    assert_eq!(s["last_rollout"]["ok"], true);
}

/// The same with HTTP/3 on: Bun refuses `http3` without `tls`, so the
/// private health socket must drop it too or it never opens.
#[test]
fn an_app_serving_http3_itself_passes_health_checks() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg =
        gated("h3", port, 2, "env = { FIXTURE_HTTP3 = \"1\" }").replace(&fixture("app.ts"), &fixture("tls_app.ts"));
    let w = Warden::start("h3", port, &cfg);
    let before = pid_set(&w.wait_for("ready", T, ready(2)));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before), "every worker must be new: {out}");
    assert_eq!(s["last_rollout"]["ok"], true);
}

/// A node:http2 TLS app (createSecureServer) gets a private health socket
/// like a node:http one, so its reloads pass the health gates.
#[test]
fn a_node_http2_app_serving_tls_itself_passes_health_checks() {
    if !have_node() {
        return;
    }
    let port = free_port();
    let cfg =
        gated("nodeh2", port, 2, "command = \"node\"").replace(&fixture("app.ts"), &fixture("node_http2_tls.mjs"));
    let w = Warden::start("nodeh2", port, &cfg);
    let before = pid_set(&w.wait_for("ready", T, ready(2)));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before), "every worker must be new: {out}");
    assert_eq!(s["last_rollout"]["ok"], true);
}

/// One TLS connection to `port` with openssl: `-sess_out` saves its
/// session, `-sess_in` offers a saved one. The pid that answered and
/// whether the session was resumed.
fn tls_session(port: u16, flag: &str, file: &std::path::Path) -> (String, bool) {
    let mut child = Command::new("openssl")
        .args(["s_client", "-connect", &format!("127.0.0.1:{port}"), "-servername", "localhost", "-ign_eof", flag])
        .arg(file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let out = String::from_utf8_lossy(&child.wait_with_output().unwrap().stdout).to_string();
    let pid =
        out.lines().rev().find(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_digit())).unwrap_or("").to_string();
    (pid, out.lines().any(|l| l.starts_with("Reused,")))
}

fn have_openssl() -> bool {
    Command::new("openssl").arg("version").output().is_ok_and(|o| o.status.success())
}

/// A Node TLS app's workers share one session-ticket key, so a visitor's
/// ticket resumes on whichever worker the kernel picks (each worker's own
/// key would resume 1 in 2 here), and after a reload too.
#[test]
fn node_tls_workers_resume_each_others_sessions() {
    if !have_node() || !have_openssl() || !cfg!(target_os = "linux") {
        return;
    }
    let sess = tmp().join(format!("warden-tickets-{}.pem", std::process::id()));
    let port = free_port();
    let cfg = gated("tickets", port, 2, "command = \"node\"").replace(&fixture("app.ts"), &fixture("node_https.mjs"));
    let w = Warden::start("tickets", port, &cfg);
    w.wait_for("2 ready", T, ready(2));
    tls_session(port, "-sess_out", &sess);
    let runs: Vec<(String, bool)> = (0..16).map(|_| tls_session(port, "-sess_in", &sess)).collect();
    assert!(runs.iter().all(|(_, reused)| *reused), "{runs:?}\n{}", w.log());
    assert_eq!(runs.iter().map(|(p, _)| p).collect::<HashSet<_>>().len(), 2, "both workers answered: {runs:?}");
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    assert!(tls_session(port, "-sess_in", &sess).1, "the new workers have the same key");
    let _ = std::fs::remove_file(&sess);
}

/// An app that sets its own `ticketKeys` keeps them: a session from the
/// same app run outside Warden resumes under it.
#[test]
fn a_node_app_keeps_its_own_ticket_keys() {
    if !have_node() || !have_openssl() {
        return;
    }
    let sess = tmp().join(format!("warden-own-tickets-{}.pem", std::process::id()));
    let outside = free_port();
    let mut plain = Command::new("node")
        .arg(fixture("node_https.mjs"))
        .env("PORT", outside.to_string())
        .env("FIXTURE_TICKET_KEYS", "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + T;
    while std::net::TcpStream::connect(("127.0.0.1", outside)).is_err() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    tls_session(outside, "-sess_out", &sess);
    let _ = plain.kill();
    let _ = plain.wait();

    let port = free_port();
    let extra = "command = \"node\"\nenv = { FIXTURE_TICKET_KEYS = \"1\" }";
    let cfg = gated("own-tickets", port, 2, extra).replace(&fixture("app.ts"), &fixture("node_https.mjs"));
    let w = Warden::start("own-tickets", port, &cfg);
    w.wait_for("2 ready", T, ready(2));
    assert!(tls_session(port, "-sess_in", &sess).1, "the app's own key\n{}", w.log());
    let _ = std::fs::remove_file(&sess);
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
    every_warning_has_a_hint(&w.log());
}

/// Found by `cargo xtask chaos` (overlap): a rollout cut short by
/// `restart --hard` logged `ERROR aborted: …` with no hint, like every
/// failed rollout's line.
#[test]
fn an_aborted_rollout_says_what_to_do() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("abort", port, &gated("abort", port, 2, "").replace("canary_soak = 1", "canary_soak = 5"));
    w.wait_for("ready", T, ready(2));
    let (code, out) = w.cli(&["safe-reload", "--no-wait"]);
    assert_eq!(code, 0, "{out}");
    w.wait_for("the canary soaking", T, |s| !s["rollout"].is_null());
    let (code, out) = w.cli(&["restart", "--hard"]);
    assert_eq!(code, 0, "{out}");
    let log = w.wait_log("aborted: workers are being stopped", T);
    w.wait_for("ready again", T, |s| s["workers_ready"] == 2 && s["rollout"].is_null());
    every_warning_has_a_hint(&log);
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
    every_warning_has_a_hint(&w.log());
}

/// `[watchdog] port_lost`: a worker that closes its server and stays alive
/// serves nothing; it is stopped and restarted like a crash.
#[test]
fn a_worker_that_stops_listening_is_restarted() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("portlost", port, 2, "").replace("[restart]", "[watchdog]\nport_lost = 2\n[restart]");
    let w = Warden::start("portlost", port, &cfg);
    let before = Warden::pids(&w.wait_for("ready", T, ready(2)));
    let (one, two) = (before[0], before[1]);
    let _ = get(port, "/unlisten");
    // Whichever answered the request above stopped listening: wait for it to be replaced.
    let s = w.wait_for("the worker that stopped listening restarted", T, |s| {
        let pids = Warden::pids(s);
        s["workers"].as_array().is_some_and(|ws| ws.iter().all(|w| w["state"] == "RUNNING"))
            && pids.len() == 2
            && (!pids.contains(&one) || !pids.contains(&two))
    });
    let gone: Vec<u64> = before.iter().copied().filter(|p| !Warden::pids(&s).contains(p)).collect();
    assert_eq!(gone.len(), 1, "only the worker that stopped listening: {s:#?}");
    let workers = s["workers"].as_array().unwrap();
    assert!(workers.iter().any(|w| w["last_exit"].as_str().is_some_and(|e| e.contains("stopped listening"))), "{s:#?}");
    assert!(!alive(gone[0]));
    let log = w.log();
    assert!(log.contains("worker stopped listening; restarting it"), "{log}");
    every_warning_has_a_hint(&log);
    // The app serves on all workers again.
    assert!(get(port, "/whoami").is_some());
}

/// A server that restarts itself (closes, listens again) within port_lost is left alone.
#[test]
fn a_worker_that_listens_again_in_time_is_left_alone() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("relisten", port, 1, "").replace("[restart]", "[watchdog]\nport_lost = 4\n[restart]");
    let w = Warden::start("relisten", port, &cfg);
    let before = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let _ = get(port, "/relisten?ms=1500");
    std::thread::sleep(Duration::from_millis(6500));
    let s = w.status().unwrap();
    assert_eq!(Warden::pids(&s), vec![before], "{s:#?}");
    assert_eq!(s["workers"][0]["restarts"], 0, "{s:#?}");
    assert!(!w.log().contains("worker stopped listening"), "{}", w.log());
    assert!(get(port, "/whoami").is_some_and(|who| who.starts_with(&format!("{before}:"))));
}

/// A worker that never listened on a TCP port is not watched: no port, no restart.
#[test]
fn a_worker_without_a_port_is_not_watched() {
    if !have_bun() {
        return;
    }
    let w = Warden::start(
        "noport",
        0,
        &format!(
            "[app]\nname = \"noport\"\nargs = [\"{}\"]\nenv = {{ FIXTURE_NO_LISTEN = \"1\" }}\n[workers]\nmin_uptime = 100\n\
             [watchdog]\nport_lost = 1\n",
            fixture("app.ts")
        ),
    );
    let before = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    std::thread::sleep(Duration::from_secs(5));
    let s = w.status().unwrap();
    assert_eq!((Warden::pids(&s), s["workers"][0]["restarts"].as_u64()), (vec![before], Some(0)), "{s:#?}");
    assert!(!w.log().contains("worker stopped listening"));
}

/// Found by `cargo xtask chaos` (stop-supervisor): Warden frozen (SIGSTOP)
/// for longer than watchdog.timeout killed every healthy worker as hung when
/// it resumed, because their heartbeats were still waiting unread. The
/// workers served all along; they must be left alone.
#[test]
fn a_frozen_supervisor_does_not_kill_its_workers_as_hung() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("frozen", port, 2, "").replace("[restart]", "[watchdog]\ntimeout = 2\n[restart]");
    let w = Warden::start("frozen", port, &cfg);
    let before = pid_set(&w.wait_for("ready", T, ready(2)));
    std::thread::sleep(Duration::from_millis(1500)); // let heartbeats arm the watchdog
    w.signal(libc::SIGSTOP);
    std::thread::sleep(Duration::from_secs(5));
    assert!(get(port, "/whoami").is_some(), "the workers serve while Warden is frozen");
    w.signal(libc::SIGCONT);
    std::thread::sleep(Duration::from_secs(3)); // ticks that would have judged them
    let s = w.wait_for("ready", T, ready(2));
    assert_eq!(pid_set(&s), before, "no worker was replaced:\n{}", w.log());
    let log = w.log();
    assert!(!log.contains("worker hung"), "{log}");
    assert!(log.lines().any(|l| l.contains("event loop was blocked") && l.contains(" hint=")), "{log}");
    // The watchdog still works after the stall.
    let victim = *before.iter().next().unwrap();
    let _ = std::process::Command::new("kill").args(["-STOP", &victim.to_string()]).status();
    let s = w.wait_for("the stopped worker replaced", T, |s| s["workers_ready"] == 2 && !pid_set(s).contains(&victim));
    assert!(
        s["workers"].as_array().unwrap().iter().any(|w| w["last_exit"].as_str().is_some_and(|e| e.contains("hung")))
    );
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
    let dir = tmp().join(format!("warden-it-notify-sock-{}", std::process::id()));
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
    assert!(w.frozen() >= Duration::from_secs(1), "the freeze is read back from the log: {:?}", w.frozen());
}

fn fault_warden(name: &str, fault: &str, count: usize) -> (Warden, u16) {
    fault_warden_env(name, fault, count, &[])
}

fn fault_warden_env(name: &str, fault: &str, count: usize, env: &[(&str, &str)]) -> (Warden, u16) {
    let port = free_port();
    let mut env = env.to_vec();
    env.push(("WARDEN_FAULT", fault));
    let w = Warden::start_env(name, port, &gated(name, port, count, ""), &env);
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

/// kill -9 of the supervisor: its keeper (the process Warden was started
/// as) starts it again, and the new one takes back the same workers, which
/// serve throughout. What they print meanwhile is in the log, and they are
/// supervised again: a worker that crashes after it is replaced.
#[test]
fn a_killed_supervisor_is_started_again_by_its_keeper_and_takes_back_its_workers() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("keeper", port, &gated("keeper", port, 2, ""));
    let s = w.wait_for("2 ready", T, ready(2));
    let before = pid_set(&s);
    assert_eq!(s["pid"].as_u64(), Some(w.child.id() as u64), "the app's process is the keeper");
    let sup = w.supervisor();
    assert_ne!(sup, w.child.id(), "the supervisor is the keeper's child");

    unsafe { libc::kill(sup as i32, libc::SIGKILL) };
    let (mut ok, t0) = (0, Instant::now());
    while t0.elapsed() < Duration::from_secs(2) {
        let said = format!("while-{ok}");
        assert!(get(port, &format!("/say?w={said}")).is_some(), "request {ok} failed:\n{}", w.log());
        ok += 1;
        // Under the 10k lines/s output cap, which would drop lines a fast machine prints.
        std::thread::sleep(Duration::from_millis(1));
    }
    let s = w.wait_for("the new supervisor", T, |s| {
        s["workers_ready"] == 2 && s["supervisor_pid"].as_u64().is_some_and(|p| p != sup as u64)
    });
    assert_eq!(pid_set(&s), before, "the same workers:\n{}", w.log());
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["restarts"] == 0), "{s:#}");
    assert_eq!(s["pid"].as_u64(), Some(w.child.id() as u64));
    let log = w.log();
    assert!(log.contains("the supervisor died; its workers keep serving and it is starting again"), "{log}");
    assert!(log.contains("taking back the workers that kept running workers=2"), "{log}");
    for n in 0..ok {
        assert!(log.contains(&format!("fixture says while-{n} ")), "output of request {n} is in the log:\n{log}");
    }
    every_warning_has_a_hint(&log);

    // Supervised again: a crash is seen and the worker replaced.
    let victim = *before.iter().next().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let s = w.wait_for("the crashed worker replaced", T, |s| s["workers_ready"] == 2 && !pid_set(s).contains(&victim));
    // macOS has no subreaper: a worker taken back is not the new supervisor's
    // child, so its exit status cannot be read, only that it is gone.
    let said = if cfg!(target_os = "linux") { "SIGKILL" } else { "unknown exit" };
    assert!(
        s["workers"].as_array().unwrap().iter().any(|x| x["last_exit"].as_str().is_some_and(|e| e.contains(said))),
        "{s:#}"
    );

    // SIGTERM to the keeper: a clean stop of everything.
    let pids = pid_set(&s);
    let (code, _) = w.terminate(Duration::from_secs(10));
    assert_eq!(code, Some(0), "{}", w.log());
    assert!(pids.iter().all(|p| !running(*p)), "the workers stopped with it");
}

/// The keeper killed (kill -9): the supervisor tells the workers to stop and
/// exits, as if killed with it, so wardend or systemd can start the app again.
#[test]
fn a_killed_keeper_takes_its_supervisor_and_workers_down() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("keeper-kill", port, &gated("keeper-kill", port, 2, ""));
    let s = w.wait_for("2 ready", T, ready(2));
    let sup = w.supervisor();
    let pids = pid_set(&s);
    w.child.kill().unwrap();
    w.child.wait().unwrap();
    let t0 = Instant::now();
    while running(sup as u64) || pids.iter().any(|p| running(*p)) {
        assert!(t0.elapsed() < Duration::from_secs(10), "still running:\n{}", w.log());
        std::thread::sleep(Duration::from_millis(50));
    }
    w.wait_log("the keeper process of this app died", T);
}

/// The keeper moved to another warden binary (`upgrade`, what `warden update` sends): the
/// supervisor leaves its workers, the keeper re-executes itself from the new binary (same
/// pid), and the supervisor it starts from there takes the workers back. Requests are answered
/// throughout, and the workers are supervised again. A binary that can't read the config is
/// refused first, and nothing changes.
#[test]
fn an_upgrade_moves_the_keeper_and_supervisor_to_another_binary_and_keeps_the_workers() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let mut w = Warden::start("upgrade", port, &gated("upgrade", port, 2, ""));
    let s = w.wait_for("2 ready", T, ready(2));
    let before = pid_set(&s);
    let sup = w.supervisor();
    let upgrade = |exe: &Path| format!(r#"{{"cmd":"upgrade","exe":"{}"}}"#, exe.display());

    let (_, out) = w.request(&upgrade(Path::new("/bin/false")));
    assert!(out.contains(r#""ok":false"#) && out.contains("nothing changed"), "{out}");
    assert_eq!(w.supervisor(), sup, "refused: the same supervisor");

    let next = w.dir.join("warden-next");
    std::fs::copy(BIN, &next).unwrap();
    let next = std::fs::canonicalize(&next).unwrap();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let load = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (mut n, mut failed) = (0, Vec::new());
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                if get(port, &format!("/say?w=moving-{n}")).is_none() {
                    failed.push(n);
                }
                n += 1;
                // Under the 10k lines/s output cap.
                std::thread::sleep(Duration::from_millis(1));
            }
            (n, failed)
        })
    };
    std::thread::sleep(Duration::from_millis(200));
    let (_, out) = w.request(&upgrade(&next));
    assert!(out.contains(r#""ok":true"#), "{out}");
    let s = w.wait_for("the new supervisor", T, |s| {
        s["workers_ready"] == 2 && s["supervisor_pid"].as_u64().is_some_and(|p| p != sup as u64)
    });
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (n, failed) = load.join().unwrap();
    assert!(failed.is_empty(), "requests {failed:?} of {n} failed during the move:\n{}", w.log());
    assert_eq!(pid_set(&s), before, "the same workers:\n{}", w.log());
    assert_eq!(s["pid"].as_u64(), Some(w.child.id() as u64), "the keeper keeps its pid");
    assert_eq!(s["build"]["path"].as_str(), Some(next.to_str().unwrap()), "{s:#}");
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["restarts"] == 0), "{s:#}");
    let log = w.wait_log(&format!("fixture says moving-{} ", n - 1), T);
    for needle in [
        "moving to another warden binary: the keeper restarts from it, the workers keep serving",
        "the keeper runs the new warden binary",
        "taking back the workers that kept running workers=2",
    ] {
        assert!(log.contains(needle), "{needle}:\n{log}");
    }
    assert!(!log.contains("the supervisor died"), "a move is no crash:\n{log}");
    for i in 0..n {
        assert!(log.contains(&format!("fixture says moving-{i} ")), "output of request {i} is in the log:\n{log}");
    }
    every_warning_has_a_hint(&log);

    // Supervised again: a crash is seen (on Linux the worker is still the keeper's child after
    // the exec, so its exit status too) and the worker replaced.
    let victim = *before.iter().next().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let s = w.wait_for("the crashed worker replaced", T, |s| s["workers_ready"] == 2 && !pid_set(s).contains(&victim));
    let said = if cfg!(target_os = "linux") { "SIGKILL" } else { "unknown exit" };
    assert!(
        s["workers"].as_array().unwrap().iter().any(|x| x["last_exit"].as_str().is_some_and(|e| e.contains(said))),
        "{s:#}"
    );
    let pids = pid_set(&s);
    let (code, _) = w.terminate(Duration::from_secs(10));
    assert_eq!(code, Some(0), "{}", w.log());
    assert!(pids.iter().all(|p| !running(*p)), "the workers stopped with it");
}

/// `warden update` of an app scaled up since it started, right after a
/// reload: every worker is kept, the scaled-up ones too, and so is their age.
#[test]
fn an_upgrade_of_a_scaled_up_app_keeps_every_worker() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("reup", port, &gated("reup", port, 2, ""));
    w.wait_for("2 ready", T, ready(2));
    let (code, out) = w.cli(&["scale", "3"]);
    assert_eq!(code, 0, "{out}");
    w.wait_for("3 ready", T, ready(3));
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 0, "{out}");
    let before = pid_set(&w.status().unwrap());
    let sup = w.supervisor();
    std::thread::sleep(Duration::from_secs(2));
    let next = w.dir.join("warden-next");
    std::fs::copy(BIN, &next).unwrap();
    let next = std::fs::canonicalize(&next).unwrap();
    let (_, out) = w.request(&format!(r#"{{"cmd":"upgrade","exe":"{}"}}"#, next.display()));
    assert!(out.contains(r#""ok":true"#), "{out}");
    let s = w.wait_for("the new supervisor", T, |s| {
        s["workers_ready"] == 3 && s["supervisor_pid"].as_u64().is_some_and(|p| p != sup as u64)
    });
    assert_eq!(pid_set(&s), before, "every worker kept:\n{}", w.log());
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["uptime_secs"].as_u64() >= Some(2)), "{s:#}");
}

/// Every Warden process of the app killed at once (the supervisor and its
/// keeper; wardend is tested in `wardend_and_the_app_come_back_by_themselves`):
/// the workers keep serving and printing (a Node app's `console.log` must not
/// fail), and the next Warden of the app takes the same workers back, their
/// output included, and supervises them: a crash is restarted, and a stop
/// stops them.
#[cfg(target_os = "linux")]
fn every_warden_process_killed(name: &str, cfg: &str, port: u16) {
    let mut w = Warden::start(name, port, cfg);
    let s = w.wait_for("2 ready", T, ready(2));
    let before = pid_set(&s);
    let sup = w.supervisor();
    unsafe { libc::kill(sup as i32, libc::SIGKILL) };
    w.child.kill().unwrap();
    w.child.wait().unwrap();
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(2) {
        assert!(get(port, &format!("/say?w=alone-{n}")).is_some(), "request {n} with no Warden failed");
        n += 1;
        // Under `[logging] max_lines_per_sec` when the output is read all at once.
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(before.iter().all(|p| running(*p)), "the workers live on");

    w.start_again(&[]);
    let s = w.wait_for("the same workers, supervised again", T, |s| s["workers_ready"] == 2 && pid_set(s) == before);
    assert!(s["workers"].as_array().unwrap().iter().all(|x| x["restarts"] == 0), "{s:#}");
    // The log writer is asynchronous: wait for the last line.
    let log = w.wait_log(&format!("fixture says alone-{} ", n - 1), T);
    assert!(log.contains("worker kept running while no Warden process of this app ran; supervised again"), "{log}");
    for i in 0..n {
        let line = format!("fixture says alone-{i} ");
        assert!(log.contains(&line), "what a worker printed with no Warden is in the log: {line}\n{log}");
    }
    every_warning_has_a_hint(&log);
    for _ in 0..20 {
        assert!(get(port, "/say?w=back").is_some());
    }

    let victim = *before.iter().next().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let s = w.wait_for("the crashed worker replaced", T, |s| s["workers_ready"] == 2 && !pid_set(s).contains(&victim));
    let pids = pid_set(&s);
    let (code, _) = w.terminate(Duration::from_secs(10));
    assert_eq!(code, Some(0), "{}", w.log());
    assert!(pids.iter().all(|p| !running(*p)), "the workers stopped with it");
}

#[cfg(target_os = "linux")]
#[test]
fn bun_workers_hand_themselves_back_when_every_warden_process_dies() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    every_warden_process_killed("recover-bun", &gated("recover-bun", port, 2, ""), port);
}

#[cfg(target_os = "linux")]
#[test]
fn node_workers_hand_themselves_back_when_every_warden_process_dies() {
    if !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"recover-node\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n\
         [shutdown]\ndrain_ms = 100\n",
        fixture("node_app.mjs")
    );
    every_warden_process_killed("recover-node", &cfg, port);
}

/// A reload after the keeper restarted the supervisor: the new workers' health
/// sockets must not share names with the taken-back workers', which remove
/// theirs as they exit (instance ids are part of the name).
#[test]
fn a_reload_after_a_supervisor_restart_keeps_the_new_workers_health_sockets() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("inst-reuse", port, 2, "").replace(
        "[health]\npath = \"/health\"\n",
        "[health]\npath = \"/health\"\ninterval = 1\nfailure_threshold = 2\ninitial_delay = 0\n",
    );
    let w = Warden::start("inst-reuse", port, &cfg);
    w.wait_for("2 ready", T, ready(2));
    // Instances 3 and 4 after a reload, while a fresh supervisor would count from 1.
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.wait_for("reloaded", T, ready(2));
    let sup = w.supervisor();
    unsafe { libc::kill(sup as i32, libc::SIGKILL) };
    let kept = w.wait_for("taken back", T, |s| {
        s["workers_ready"] == 2 && s["supervisor_pid"].as_u64().is_some_and(|p| p != sup as u64)
    });
    assert_eq!(pid_set(&kept), pid_set(&s));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.wait_for("reloaded again", T, |s| s["workers_ready"] == 2 && pid_set(s).is_disjoint(&pid_set(&kept)));
    // The old workers drain and exit; each new one keeps its own socket.
    std::thread::sleep(Duration::from_secs(2));
    let sockets: Vec<String> = std::fs::read_dir(&w.dir)
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("inst-reuse.h") && n.ends_with(".sock"))
        .collect();
    assert_eq!(sockets.len(), 2, "one health socket per new worker: {sockets:?}");
    let log = w.log();
    let after = log.rsplit("taking back the workers").next().unwrap_or("");
    assert!(!after.contains("worker unhealthy"), "a new worker lost its health socket:\n{log}");
    assert_eq!(pid_set(&w.status().unwrap()), pid_set(&s), "the same workers serve");
}

/// Without the keeper (`WARDEN_KEEPER=0`); with it, the keeper starts the
/// supervisor again and the workers keep serving
/// (`a_killed_supervisor_is_started_again_by_its_keeper_and_takes_back_its_workers`).
#[test]
fn a_panic_in_the_event_loop_exits_and_takes_workers_down() {
    if !have_bun() {
        return;
    }
    let (mut w, port) = fault_warden_env("fault-tick", "tick:4", 2, &[("WARDEN_KEEPER", "0")]);
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

/// A stopped Warden (SIGSTOP, a frozen VM, a bug) never freezes its workers.
/// With fd 3 full (what hours of heartbeats do while Warden reads nothing,
/// done at once by the fixture's `/fill-ipc`), the worker keeps serving: its
/// writes to fd 3 don't block its event loop, the shim's heartbeats wait.
/// Once Warden runs again they flow again: the watchdog (4 s) keeps it.
#[test]
fn a_stopped_supervisor_never_freezes_its_workers() {
    if have_bun() {
        let port = free_port();
        stopped_supervisor_case("ipcstall", port, &simple("ipcstall", port, 1, "[watchdog]\ntimeout = 4\n"));
    }
    if have_node() {
        let port = free_port();
        let cfg = format!(
            "[app]\nname = \"ipcstalln\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 1\n\
             [watchdog]\ntimeout = 4\n",
            fixture("node_app.mjs")
        );
        stopped_supervisor_case("ipcstalln", port, &cfg);
    }
}

fn stopped_supervisor_case(name: &str, port: u16, cfg: &str) {
    let w = Warden::start(name, port, cfg);
    let s = w.wait_for("1 ready worker", T, ready(1));
    let pid = s["workers"][0]["pid"].clone();
    {
        let _frozen = w.freeze();
        let filled = get(port, "/fill-ipc").unwrap_or_else(|| panic!("{name}: no answer: a write to fd 3 blocked"));
        let filled: u64 = filled.parse().unwrap();
        assert!(filled > 0, "{name}: nothing written");
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(3) {
            assert!(
                get(port, "/whoami").is_some(),
                "{name}: the worker stopped answering while Warden was stopped (its heartbeats blocked it?)"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    // Warden reads the backlog, the heartbeats flow again: nobody is hung.
    std::thread::sleep(Duration::from_secs(9));
    let s = w.status().unwrap();
    let log = w.log();
    assert_eq!(s["workers"][0]["pid"], pid, "{name}: the worker was replaced\n{log}");
    assert_eq!(s["workers"][0]["state"], "RUNNING", "{name}\n{log}");
    assert!(!log.contains("worker hung"), "{name}\n{log}");
    assert!(get(port, "/whoami").is_some());
    every_warning_has_a_hint(&log);
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
    // The supervisor frees a slot when it sees the client gone, which can
    // take a moment after the close: ask until one is free.
    let t0 = Instant::now();
    loop {
        let first = Events::open(&w, r#"{"cmd":"subscribe"}"#).next();
        if first["type"] == "hello" {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(3), "a stream slot is free again: {first}");
        std::thread::sleep(Duration::from_millis(50));
    }
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
        let home = tmp().join(format!("wf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Fleet { home }
    }

    fn cli(&self, args: &[&str]) -> (i32, String) {
        self.cli_env(args, &[])
    }

    fn cli_env(&self, args: &[&str], env: &[(&str, &str)]) -> (i32, String) {
        let out = warden()
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

    // list: a boxed table like PM2's, one row per worker, ids first (the
    // apps are numbered in the order they were created: api 0, web 1).
    let out = f.ok(&["list"]);
    assert!(out.starts_with('┌') && out.lines().nth(1).is_some_and(|l| l.starts_with("│ id │ name")), "{out}");
    let rows = |id: &str, name: &str| {
        out.lines().filter(|l| l.starts_with(&format!("│ {id} ")) && l.contains(name) && l.contains("RUNNING")).count()
    };
    assert_eq!((rows("0", " api "), rows("1", " web ")), (2, 1), "{out}");
    assert_eq!(f.list().len(), 2);
    let ids: Vec<_> = f.list().iter().map(|a| (a["app"].as_str().unwrap().to_string(), a["id"].as_u64())).collect();
    assert_eq!(ids, [("api".to_string(), Some(0)), ("web".to_string(), Some(1))], "--json carries the ids");

    // An id nobody has is an error that says which ones exist, and nothing is done.
    let (code, out) = f.cli(&["restart", "0,7"]);
    assert_eq!(code, 2);
    assert!(out.contains("no app has the id 7 (the ids on this host: 0,1)"), "{out}");
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

    // describe: PM2's title, then a key | value box and the workers' box.
    let d = f.ok(&["describe", "0"]);
    assert!(d.starts_with(" Describing app with id 0 - name api\n┌"), "{d}");
    assert!(d.contains("│ namespace") && d.contains("backend") && d.contains("\n Workers\n┌"), "{d}");
    assert!(f.ok(&["status", "api"]).contains("│ status "), "one app: the same box");

    // Namespaces as targets: stop both, start one. A script's output has no
    // table after it; with WARDEN_TABLE=1 (or on a terminal) it does, as in PM2.
    let (code, out) = f.cli_env(&["stop", "backend"], &[("WARDEN_TABLE", "1")]);
    assert_eq!(code, 0, "{out}");
    let stopped = |id: &str, name: &str| {
        out.lines().filter(|l| l.starts_with(&format!("│ {id} ")) && l.contains(name) && l.contains("stopped")).count()
    };
    assert_eq!((stopped("0", " api "), stopped("1", " web ")), (2, 1), "the table after stop:\n{out}");
    f.wait("both stopped", |f| {
        f.list().iter().all(|a| {
            a["status"]["stopped"] == true
                && a["status"]["workers"].as_array().unwrap().iter().all(|w| w["state"] == "STOPPED")
        })
    });
    assert!(f.ok(&["list"]).contains("stopped"));
    let out = f.ok(&["start", "api"]);
    assert!(out.contains("online (2/2"), "{out}");

    // Ids and lists work for every command, as PM2's `start 0 1`: api is up
    // already, web comes up; then web goes down again by its id.
    let out = f.ok(&["start", "0,1"]);
    assert!(out.contains("web: online"), "{out}");
    f.ok(&["restart", "0", "1"]);
    f.ok(&["stop", "1"]);
    f.wait("web stopped by id", |f| f.app("web")["status"]["stopped"] == true);
    // A wrong item anywhere acts on nothing: api is untouched by this refusal.
    let pids_before = f.pids("api");
    let (code, out) = f.cli(&["stop", "0", "1:2"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("works on whole apps"), "{out}");
    assert_eq!(f.pids("api"), pids_before, "api kept running");
    assert_eq!(f.app("api")["status"]["stopped"], false);
    let (code, out) = f.cli(&["start", "9"]);
    assert_eq!(code, 2, "an unknown id is an error, not a script to try: {out}");
    assert!(out.contains("no app has the id 9"), "{out}");

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
    assert!(out.contains("│ namespace") && out.contains("backend") && out.contains("│ restart "), "{out}");
    assert!(out.contains("│ worker │"), "the workers' own box: {out}");
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

/// Bun's (major, minor) version; (0, 0) if it cannot be read.
fn bun_version() -> (u32, u32) {
    let out = Command::new("bun").arg("--version").output().ok();
    let v = out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    let mut n = v.split('.').map(|x| x.parse::<u32>().unwrap_or(0));
    (n.next().unwrap_or(0), n.next().unwrap_or(0))
}

fn have_node() -> bool {
    Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success())
}

/// Node apps get the shim through `--import`: several workers share the
/// port (SO_REUSEPORT), each knows its NODE_APP_INSTANCE, and a rolling
/// restart under load drops nothing.
/// `status --json` until `f` holds of it (the counts come with the
/// heartbeats, once a second).
fn wait_status(w: &Warden, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    w.wait_for(what, T, f)
}

/// Request health: a Node app's responses are counted by status through
/// the shim (`node:diagnostics_channel`), per worker and for the app; the
/// health checks on the private sockets are not; the port shows its queue.
#[test]
fn node_responses_are_counted_by_status() {
    if !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"noderq\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n\
         [health]\npath = \"/whoami\"\ninterval = 1\n",
        fixture("node_app.mjs")
    );
    let w = Warden::start("noderq", port, &cfg);
    w.wait_for("2 ready", T, ready(2));
    for (code, n) in [(200, 20), (301, 2), (404, 5), (400, 1), (503, 3)] {
        for _ in 0..n {
            let _ = get(port, &format!("/status?code={code}"));
        }
    }
    // All 31 counted, not just the 503s: a worker's counts arrive with its own
    // heartbeat, so one worker can have reported while the other has not yet.
    let s = wait_status(&w, "the responses counted", |s| {
        let t = &s["requests"]["total"];
        ["2xx", "3xx", "4xx", "5xx"].iter().map(|k| t[k].as_u64().unwrap_or(0)).sum::<u64>() >= 31
    });
    let total = &s["requests"]["total"];
    assert_eq!(
        (&total["2xx"], &total["3xx"], &total["4xx"], &total["404"], &total["5xx"]),
        (&json!(20), &json!(2), &json!(6), &json!(5), &json!(3)),
        "health checks (every second, on the private sockets) are not counted: {s:#?}"
    );
    assert_eq!(s["requests"]["minute"], s["requests"]["total"]);
    let per_worker: u64 =
        s["workers"].as_array().unwrap().iter().map(|w| w["requests"]["total"]["2xx"].as_u64().unwrap_or(0)).sum();
    assert_eq!(per_worker, 20, "the workers' counts add up to the app's");
    let p = &s["ports"][0];
    assert_eq!(p["port"], port);
    assert!(p["max_backlog"].as_u64().unwrap() > 0 && p["drops"].is_u64(), "{p}");
    // `warden status` says it in words.
    let (_, text) = w.cli(&["status"]);
    assert!(text.contains("last minute: 31 sent, 3 5xx, 6 4xx of which 5 404"), "{text}");
    assert!(text.contains("waiting to be accepted"), "{text}");
}

/// Warden's static server counts its own responses; `[metrics] requests =
/// false` turns the counting off (no figures at all, not zeros).
#[test]
fn static_responses_are_counted_and_can_be_turned_off() {
    let dir = tmp().join(format!("warden-it-site-staticrq-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), "<h1>home</h1>").unwrap();
    let toml = |name: &str, port: u16, extra: &str| {
        format!(
            "[app]\nname = \"{name}\"\nport = {port}\n[workers]\ncount = 1\n{extra}[static]\nroot = \"{}\"\n",
            dir.join("site").display()
        )
    };
    let (on, off) = (free_port(), free_port());
    let w = Warden::start("staticrq", on, &toml("staticrq", on, ""));
    let w_off = Warden::start("staticrq-off", off, &toml("staticrq-off", off, "[metrics]\nrequests = false\n"));
    w.wait_for("ready", T, ready(1));
    w_off.wait_for("ready", T, ready(1));
    for port in [on, off] {
        for _ in 0..7 {
            assert!(get(port, "/").is_some());
        }
        for _ in 0..3 {
            let _ = get(port, "/missing.html");
        }
    }
    let s = wait_status(&w, "the responses counted", |s| s["requests"]["total"]["404"] == 3);
    assert_eq!((&s["requests"]["total"]["2xx"], &s["workers"][0]["requests"]["total"]["2xx"]), (&json!(7), &json!(7)));
    // The static server's sockets defer accepting (TCP_DEFER_ACCEPT), which the kernel counts as drops: none shown.
    assert!(s["ports"][0]["max_backlog"].as_u64().unwrap() > 0 && s["ports"][0].get("drops").is_none(), "{s:#?}");
    // A beat for any count to arrive, then a status that answers (a busy runner can miss one).
    std::thread::sleep(Duration::from_millis(1500));
    let s = wait_status(&w_off, "a status", |s| s["workers"][0]["pid"].is_u64());
    assert!(s.get("requests").is_none() && s["workers"][0].get("requests").is_none(), "{s:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `[app] address`: the app's listen on its port is on that address, whatever
/// host it asked for (the fixtures ask for 127.0.0.1 or none), under Bun.serve,
/// Bun's node:http and Node. 127.0.0.2 stands in for an address of the
/// server's own: Linux has every 127.x already, so nothing is added.
#[test]
#[cfg(target_os = "linux")]
fn an_app_listens_on_its_own_address() {
    if !have_bun() {
        return;
    }
    let get2 = |port: u16| -> Option<String> {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 2], port));
        let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        write!(s, "GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).ok()?;
        buf.starts_with("HTTP/1.1 2").then_some(buf)
    };
    let mut cases = vec![("bun", fixture("app.ts")), ("bun", fixture("node_http.mjs"))];
    if have_node() {
        cases.push(("node", fixture("node_http.mjs")));
    }
    for (i, (command, script)) in cases.into_iter().enumerate() {
        let port = free_port();
        let name = format!("addr{i}");
        let w = Warden::start(
            &name,
            port,
            &format!(
                "[app]\nname = \"{name}\"\ncommand = \"{command}\"\nargs = [\"{script}\"]\nport = {port}\n\
                 address = \"127.0.0.2\"\n[workers]\ncount = 2\n"
            ),
        );
        w.wait_for("2 ready", T, ready(2));
        assert!(get2(port).is_some(), "{command} {script}: answers on 127.0.0.2:{port}\n{}", w.log());
        assert!(get(port, "/").is_none(), "{command} {script}: not on 127.0.0.1, the host it asked for");
    }
}

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

/// Connection handoff (src/handoff.rs): Warden owns the port and passes each
/// connection to a worker over Node's IPC channel, under Node and under Bun.
/// On by default on macOS with several workers; forced on here, so it runs
/// where CI does. Connections reach both workers, `Connection: close` ends
/// them (Bun keeps a handed-over socket open otherwise: `get` reads to EOF),
/// readiness comes through, and a rolling restart loses nothing.
#[test]
fn handed_over_connections_reach_every_worker_and_survive_a_rolling_restart() {
    if !have_bun() {
        return;
    }
    for runtime in ["node", "bun"] {
        if runtime == "node" && !have_node() {
            continue;
        }
        let port = free_port();
        let name = format!("handoff-{runtime}");
        let cfg = format!(
            "[app]\nname = \"{name}\"\ncommand = \"{runtime}\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n\
             [health]\npath = \"/whoami\"\n[reload]\nhealth_passes = 1\nhealth_interval_ms = 100\n[shutdown]\ndrain_ms = 200\n",
            fixture("node_app.mjs")
        );
        let w = Warden::start_env(&name, port, &cfg, &[("WARDEN_HANDOFF", "1")]);
        w.wait_for("2 ready", T, ready(2));
        // Bun before 1.4 cannot take a handed-over socket: its workers listen
        // on the port as without the handoff (the rest of the test holds).
        let takes = runtime == "node" || bun_version() >= (1, 4);
        assert_eq!(w.log().contains("workers take connections from Warden"), takes, "{runtime}: {}", w.log());
        // And says why, suggesting a newer Bun.
        const OLD_BUN: &str = "this Bun cannot take connections from Warden";
        let t0 = Instant::now();
        while !takes && !w.log().contains(OLD_BUN) && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(w.log().contains(OLD_BUN), !takes, "{runtime}: {}", w.log());
        let hint = w.status().and_then(|s| s["hint"].as_str().map(String::from)).unwrap_or_default();
        assert_eq!(
            hint.contains("bun upgrade"),
            !takes,
            "{runtime}: the status hint, for doctor and the GUI: {hint:?}"
        );
        let mut seen = HashSet::new();
        for _ in 0..40 {
            seen.insert(get(port, "/whoami").unwrap_or_else(|| panic!("{runtime}: request failed\n{}", w.log())));
        }
        let instances: HashSet<String> = seen.iter().map(|s| s.split(':').nth(1).unwrap().to_string()).collect();
        // Spread by the handoff, or by the kernel where it balances a shared port.
        if takes || cfg!(target_os = "linux") {
            assert_eq!(instances, HashSet::from(["0".to_string(), "1".to_string()]), "{runtime}: {seen:?}");
        }

        let stop = Arc::new(AtomicBool::new(false));
        let (ok, fail) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let client = {
            let (stop, ok, fail) = (stop.clone(), ok.clone(), fail.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    let n = if get(port, "/whoami").is_some() { &ok } else { &fail };
                    n.fetch_add(1, Ordering::Relaxed);
                }
            })
        };
        let before = pid_set(&w.status().unwrap());
        let (code, out) = w.cli(&["restart"]);
        stop.store(true, Ordering::Relaxed);
        client.join().unwrap();
        assert_eq!(code, 0, "{runtime}: {out}\n{}", w.log());
        assert!(before.is_disjoint(&pid_set(&w.status().unwrap())), "{runtime}: every worker replaced");
        let (ok, fail) = (ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed));
        eprintln!("{runtime} handoff rolling restart: {ok} ok, {fail} failed");
        assert!(ok > 50, "{runtime}: {ok} ok");
        assert!(fail <= allowed_resets(), "{runtime}: requests failed during the rolling restart: {fail}");
        assert!(get(port, "/whoami").is_some(), "{runtime}: served after the restart");
    }
}

/// A Bun with `server.adopt(fd)` (proposed upstream; releases up to 1.4.x
/// have none).
fn bun_adopts() -> bool {
    let js = "const s = Bun.serve({ port: 0, fetch: () => new Response() }); \
              console.log(typeof s.adopt); s.stop(true);";
    let out = Command::new("bun").args(["-e", js]).env_remove("BUN_OPTIONS").output();
    out.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "function")
}

/// The handoff for a Bun.serve app (TLS, served by the app itself): with
/// `server.adopt(fd)` the shim serves it on a private port and reports
/// `adopt`, Warden passes the bare descriptor (`"type":"fd"`), and Bun reads
/// the ClientHello still waiting in the kernel; both workers answer, through
/// a rolling restart too. A Bun without adopt() listens on the port as
/// before the handoff (the rest of the test holds).
#[test]
fn a_bun_serve_tls_app_adopts_handed_over_connections() {
    if !have_bun() || !Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success()) {
        return;
    }
    let takes = bun_adopts();
    let port = free_port();
    let cfg = gated("adopt", port, 2, "").replace(&fixture("app.ts"), &fixture("tls_app.ts"));
    let w = Warden::start_env("adopt", port, &cfg, &[("WARDEN_HANDOFF", "1")]);
    w.wait_for("2 ready", T, ready(2));
    assert_eq!(w.log().contains("workers take connections from Warden"), takes, "{}", w.log());
    let url = format!("https://localhost:{port}/");
    let get_tls = || {
        let out = Command::new("curl")
            .args(["-sk", "--max-time", "5", &url])
            .env("NO_PROXY", "*")
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .output()
            .unwrap();
        let body = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert!(out.status.success() && !body.is_empty(), "curl: {out:?}\n{}", w.log());
        body
    };
    let pids: HashSet<String> = (0..20).map(|_| get_tls()).collect();
    if takes || cfg!(target_os = "linux") {
        assert_eq!(pids, pid_set(&w.status().unwrap()).iter().map(|p| p.to_string()).collect(), "both answer");
    }
    let (code, out) = w.cli(&["restart"]);
    assert_eq!(code, 0, "{out}\n{}", w.log());
    get_tls();
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
         env = {{ FIXTURE_WAIT_READY = \"4000\", FIXTURE_SIGINT_ONLY = \"1\" }}\n\
         [workers]\ncount = 1\nwait_ready = true\n[shutdown]\nsignal = \"SIGINT\"\ngrace_period = 10\n",
        fixture("node_app.mjs")
    );
    let t0 = Instant::now();
    let mut w = Warden::start("pm2app", port, &cfg);
    // Listening early is not enough: ready only after process.send('ready').
    // The app sends it 4 s after it listens: room for a busy machine (tests
    // run 12 at a time in CI) between seeing it listen and asking for status.
    w.wait_for("listening", T, |_| get(port, "/whoami").is_some());
    assert_eq!(w.status().unwrap()["workers_ready"], 0, "not ready before process.send('ready')");
    w.wait_for("ready", T, ready(1));
    assert!(t0.elapsed() >= Duration::from_millis(3900), "{:?}", t0.elapsed());
    // SIGKILL would come after the 10 s grace period: well under it is SIGINT.
    let (code, took) = w.terminate(Duration::from_secs(20));
    assert_eq!(code, Some(0));
    assert!(took < Duration::from_secs(8), "stopped by SIGINT, not by the SIGKILL after grace: {took:?}");
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
        let dir = tmp().join(format!("warden-it-{name}-out-{}", std::process::id()));
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
        assert!(list.lines().any(|l| l.contains(&format!(" {app} ")) && l.contains("RUNNING")), "{app}:\n{list}");
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

fn have_python() -> bool {
    Command::new("python3").arg("-V").output().is_ok_and(|o| o.status.success())
}

/// The ports and Unix sockets an app really listens on are read from the OS:
/// the app's own, a child's (the wrapper case: `npm run start`), a socket
/// that is localhost only; and none of Warden's own.
#[test]
fn ports_show_what_the_app_really_listens_on() {
    if !have_python() {
        return;
    }
    let f = Fleet::new("ports");
    // free_port frees the port again, so the same one can come back: ask until they differ.
    let mut distinct = std::collections::BTreeSet::new();
    while distinct.len() < 4 {
        distinct.insert(free_port());
    }
    let mut ports = distinct.into_iter();
    let (p1, p2, p3, web) =
        (ports.next().unwrap(), ports.next().unwrap(), ports.next().unwrap(), ports.next().unwrap());
    let sock = f.home.join("m.sock");
    let script = f.home.join("multi.py");
    std::fs::write(
        &script,
        r#"import os, socket, subprocess, sys, time
p1, p2, p3, path = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
def tcp(host, port):
    s = socket.socket()
    s.bind((host, port))
    s.listen()
    return s
keep = [tcp("127.0.0.1", p1), tcp("0.0.0.0", p2)]
if os.path.exists(path):
    os.unlink(path)
u = socket.socket(socket.AF_UNIX)
u.bind(path)
u.listen()
keep.append(u)
# A child that listens too, and goes when its parent does.
child = """
import os, socket, time
parent = os.getppid()
s = socket.socket()
s.bind(("127.0.0.1", %d))
s.listen()
while os.getppid() == parent:
    time.sleep(0.2)
""" % p3
subprocess.Popen([sys.executable, "-c", child])
while True:
    time.sleep(1)
"#,
    )
    .unwrap();
    let out = f.ok(&[
        "start",
        script.to_str().unwrap(),
        "--name",
        "multi",
        "--port",
        &p1.to_string(),
        "--",
        &p1.to_string(),
        &p2.to_string(),
        &p3.to_string(),
        sock.to_str().unwrap(),
    ]);
    assert!(out.contains("multi: online"), "{out}");
    // A static site: its private health socket is Warden's, not the site's.
    let site = f.home.join("site");
    std::fs::create_dir_all(&site).unwrap();
    std::fs::write(site.join("index.html"), "hi").unwrap();
    f.ok(&["serve", site.to_str().unwrap(), &web.to_string(), "--name", "web"]);

    let sockets = |f: &Fleet, app: &str| -> Vec<Value> {
        f.app(app)["status"]["workers"][0]["listening"].as_array().cloned().unwrap_or_default()
    };
    let has_tcp = |l: &[Value], port: u16, addr: &str| {
        l.iter().any(|s| s["kind"] == "tcp" && s["port"] == port && s["addr"] == addr)
    };
    // The child binds a moment after the parent; the supervisor reads the OS every 2 s at most.
    f.wait("every socket", |f| {
        let l = sockets(f, "multi");
        has_tcp(&l, p1, "127.0.0.1") && has_tcp(&l, p2, "0.0.0.0") && has_tcp(&l, p3, "127.0.0.1") && l.len() == 4
    });
    let l = sockets(&f, "multi");
    assert!(l.iter().any(|s| s["kind"] == "unix" && s["path"] == sock.to_str().unwrap()), "{l:?}");
    // A site listens on its port and nothing else the user would call a socket.
    f.wait("the site's port", |f| sockets(f, "web").iter().any(|s| s["kind"] == "tcp" && s["port"] == web));
    let site_sockets = sockets(&f, "web");
    assert!(
        site_sockets.iter().all(|s| s["kind"] == "tcp"),
        "Warden's health socket is not the site's: {site_sockets:?}"
    );

    // The list's column, the boxes, and `warden ports` (table and JSON).
    let list = f.ok(&["list"]);
    let at = list.lines().position(|l| l.contains(" multi ") && l.contains("RUNNING")).expect("the multi row");
    // One socket per line in the row: p2 (all interfaces) is the plain port; the localhost-only
    // ones carry their host.
    let row: Vec<&str> = list.lines().skip(at).take(4).collect();
    let has = |cell: String| row.iter().any(|l| l.contains(&format!("│ {cell} ")));
    assert!(has(format!("localhost:{p1}")) && has(p2.to_string()) && has(format!("localhost:{p3}")), "{list}");
    assert!(list.contains(&format!("{web}")), "{list}");
    let describe = f.ok(&["describe", "multi"]);
    assert!(
        describe.contains(&format!("{p2} (all interfaces)")) && describe.contains(&format!("{p1} (localhost only)")),
        "{describe}"
    );
    assert!(describe.contains(&format!("unix {}", sock.display())), "{describe}");
    let table = f.ok(&["ports", "multi"]);
    assert!(table.contains("all interfaces") && table.contains("localhost only") && table.contains("unix"), "{table}");
    assert!(table.contains(&format!("http://localhost:{p2}")), "{table}");
    let json: Value = serde_json::from_str(&f.ok(&["ports", "--json"])).unwrap();
    let rows = json.as_array().unwrap();
    assert!(
        rows.iter()
            .any(|r| r["app"] == "web" && r["port"] == web && r["url"] == format!("http://localhost:{web}").as_str()),
        "{json}"
    );
    assert!(rows.iter().any(|r| r["app"] == "multi" && r["kind"] == "unix"), "{json}");
    assert!(rows.iter().all(|r| r["workers"] == serde_json::json!([1])), "{json}");

    // Stopped: nothing is listening, nothing is shown.
    f.ok(&["stop", "multi"]);
    f.wait("multi to show no sockets", |f| sockets(f, "multi").is_empty());
    let json: Value = serde_json::from_str(&f.ok(&["ports", "multi", "--json"])).unwrap();
    assert_eq!(json, serde_json::json!([]), "{json}");
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
    // "worker ready" comes min_uptime (300 ms) after the start; a fast flood
    // can be written, rotated and gzipped before that.
    f.wait("worker ready", |f| f.app("chatty")["status"]["workers_ready"] == 1);
    // The files lag the live log a little (they are written in batches): the
    // history reads the files, so wait until they hold the end of the flood
    // and the ready line (CI saw it read them at line 199, and before `ready`).
    f.wait("the files caught up", |f| {
        let (_, out) = f.cli(&["logs", "chatty", "--history"]);
        out.contains("line 400 padding") && out.contains("INFO  worker ready")
    });

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
    let mut child = warden()
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
    let dir = tmp().join(format!("warden-it-open-modes-{}", std::process::id()));
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
    // Let the files go quiet (2 s) so the response cache takes them: each
    // mode is then checked through the cache as well as the open path.
    std::thread::sleep(Duration::from_millis(2200));

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
        w.wait_log("via epoll", T);
        let get = |p: &str| get_close(port, p, "");
        // The first round fills the cache, the second is answered from it.
        for _ in 0..2 {
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
        }
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

/// `raw` with the value of every Date header the same (each checked to be an
/// HTTP date): two answers a second apart differ there and nowhere else.
fn undated(raw: &[u8]) -> Vec<u8> {
    const KEY: &[u8] = b"\r\nDate: ";
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with(KEY) && i + KEY.len() + 29 <= raw.len() {
            let date = &raw[i + KEY.len()..i + KEY.len() + 29];
            assert!(date.ends_with(b" GMT") && date[3] == b',', "an HTTP date: {:?}", String::from_utf8_lossy(date));
            out.extend_from_slice(KEY);
            out.extend_from_slice(b"Sun, 06 Nov 1994 08:49:37 GMT");
            i += KEY.len() + 29;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// Everything the server sends back for `req` (pipelined requests too),
/// until it closes the connection.
fn raw_exchange(port: u16, req: &str) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf);
    buf
}

/// The static cache (opt-in, `cache_size`): cached responses are byte-identical
/// to an uncached server's (the default); an edited or deleted file shows
/// within cache_valid_ms; a cached path swapped for a symlink out of the root
/// is refused, not served.
/// CPU time (user + system, in clock ticks) `pid` has used.
fn cpu_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // After "(comm)": the state is field 3, utime 14, stime 15.
    let f: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
    f[11].parse::<u64>().unwrap() + f[12].parse::<u64>().unwrap()
}

/// A static worker out of file descriptors (EMFILE) neither spins nor goes
/// quiet: it backs off, says why and how to fix it, and serves again once
/// descriptors are free.
#[test]
fn static_accept_errors_back_off_and_say_why() {
    let io = "epoll";
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tmp().join(format!("warden-it-emfile-{io}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), "hi").unwrap();
    let out = dir.join("out.log");
    let file = std::fs::File::create(&out).unwrap();
    let port = free_port();
    // The worker itself, with few descriptors: a few dozen idle connections use them up.
    let child = Command::new("sh")
        .args(["-c", "ulimit -n 48 && exec \"$0\" serve-static", BIN])
        .env("WARDEN_STATIC", serde_json::json!({ "root": dir.join("site") }).to_string())
        .env("PORT", port.to_string())
        .stdout(Stdio::from(file.try_clone().unwrap()))
        .stderr(file)
        .spawn()
        .unwrap();
    let w = Kill(child);
    let log = || std::fs::read_to_string(&out).unwrap_or_default();
    let t0 = Instant::now();
    while get(port, "/index.html").is_none() {
        assert!(t0.elapsed() < T, "the static worker does not serve:\n{}", log());
        std::thread::sleep(Duration::from_millis(50));
    }
    let via = io;
    assert!(log().contains("via epoll"), "{}", log());

    // Half a request head each: every connection holds a descriptor of the
    // worker until the head timeout (10 s).
    let held: Vec<std::net::TcpStream> = (0..80)
        .map(|_| {
            let mut c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            c.write_all(b"GET /index.html HTTP/1.1\r\n").unwrap();
            c
        })
        .collect();
    let t0 = Instant::now();
    while !log().contains("cannot accept connections") {
        assert!(t0.elapsed() < Duration::from_secs(5), "no accept error logged ({via}):\n{}", log());
        std::thread::sleep(Duration::from_millis(50));
    }
    // Backing off, not spinning on the error.
    let pid = w.0.id();
    let before = cpu_ticks(pid);
    std::thread::sleep(Duration::from_secs(1));
    let used = cpu_ticks(pid) - before;
    let text = log();
    assert!(used < 20, "{used} ticks of CPU within 1 s with {via}: spinning on EMFILE\n{text}");
    let line = text.lines().find(|l| l.contains("cannot accept connections")).unwrap();
    assert!(line.contains("Too many open files") && line.contains(&format!("via={via}")), "{line}");
    assert!(line.contains("retry_in_ms=") && line.contains("LimitNOFILE=65536"), "{line}");
    assert_eq!(text.matches("cannot accept connections").count(), 1, "logged at most every 10 s:\n{text}");

    // Descriptors free again: it accepts and serves.
    drop(held);
    let t0 = Instant::now();
    while get(port, "/index.html").is_none() {
        assert!(t0.elapsed() < T, "it does not serve again ({via}):\n{}", log());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(log().contains("accepting connections again"), "{}", log());
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn static_cache_hits_match_and_stay_fresh() {
    let io = "epoll";
    let dir = tmp().join(format!("warden-it-cache-{io}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let site = dir.join("site");
    std::fs::create_dir_all(site.join("docs")).unwrap();
    std::fs::write(site.join("index.html"), "<h1>home</h1>").unwrap();
    std::fs::write(site.join("docs/index.html"), "docs home").unwrap();
    std::fs::write(site.join("a.css"), "a{}").unwrap();
    std::fs::write(site.join("app.3f9a2c1b.js"), "console.log(1)").unwrap();
    std::fs::write(site.join("style.css"), "body{color:red}").unwrap();
    std::fs::write(site.join("style.css.gz"), "gzipped bytes").unwrap();
    std::fs::write(site.join("style.css.br"), "brotli bytes").unwrap();
    std::fs::write(site.join("edit.txt"), "version 1").unwrap();
    std::fs::write(site.join("inplace.txt"), "rsync v1").unwrap();
    std::fs::write(site.join("gone.txt"), "here").unwrap();
    std::fs::write(site.join("swap.txt"), "inside").unwrap();
    std::fs::write(dir.join("outside.txt"), "secret").unwrap();
    // Between the single-write limit (16 KB) and cache_max_file (64 KB):
    // cached in a memfd (MEMFD_MIN, 24 KB); and one above it (never cached).
    let mid: Vec<u8> = (0..40_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(site.join("mid.bin"), &mid).unwrap();
    let big: Vec<u8> = (0..100_000u32).map(|i| (i % 241) as u8).collect();
    std::fs::write(site.join("big.bin"), &big).unwrap();
    let written = Instant::now();

    let (on, off) = (free_port(), free_port());
    let toml = |name: &str, port: u16, extra: &str| {
        format!(
            "[app]\nname = \"{name}\"\nport = {port}\n[workers]\ncount = 1\n[static]\nroot = \"{}\"\naccess_log = true\n{extra}",
            site.display()
        )
    };
    let env: [(&str, &str); 0] = [];
    let mut w_on = Warden::start_env(
        &format!("cache-on-{io}"),
        on,
        &toml("cache-on", on, "cache_size = \"16MB\"\ncache_valid_ms = 300\n"),
        &env,
    );
    let w_off = Warden::start_env(&format!("cache-off-{io}"), off, &toml("cache-off", off, ""), &env);
    w_on.wait_for("cached static worker ready", T, ready(1));
    w_off.wait_for("uncached static worker ready", T, ready(1));
    let used = "epoll";
    w_on.wait_log(&format!("via {used}, cache 16384 KB per worker"), T);
    w_off.wait_log(&format!("via {used}, no cache"), T);
    // Files changed in the last 2 s are served but not cached (a write in
    // the same timestamp tick could go unnoticed): wait that out.
    std::thread::sleep(Duration::from_millis(2200).saturating_sub(written.elapsed()));

    let close = "GET /a.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    let (_, h, _) = get_close(on, "/a.css", "");
    let (etag, lm) = (h["etag"].clone(), h["last-modified"].clone());
    let requests = [
        "GET /a.css HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
        "HEAD /a.css HTTP/1.1\r\nHost: x\r\n\r\n".to_string(),
        format!("GET /a.css HTTP/1.1\r\nIf-None-Match: {etag}\r\n\r\n"),
        format!("HEAD /a.css HTTP/1.1\r\nIf-None-Match: \"other\", {etag}\r\n\r\n"),
        format!("GET /a.css HTTP/1.1\r\nIf-Modified-Since: {lm}\r\n\r\n"),
        "GET /a.css HTTP/1.1\r\nIf-Modified-Since: Thu, 01 Jan 1970 00:00:00 GMT\r\n\r\n".to_string(),
        "GET /a.css HTTP/1.1\r\nIf-None-Match: \"nope\"\r\n\r\n".to_string(),
        "GET /a.css?v=2 HTTP/1.1\r\nConnection: close\r\n\r\n".to_string(),
        "GET /a.css HTTP/1.0\r\n\r\n".to_string(),
        "GET /a.css HTTP/1.0\r\nConnection: keep-alive\r\n\r\n".to_string(),
        "GET /app.3f9a2c1b.js HTTP/1.1\r\n\r\n".to_string(),
        "GET /style.css HTTP/1.1\r\nAccept-Encoding: gzip, deflate, br\r\n\r\n".to_string(),
        "GET /style.css HTTP/1.1\r\nAccept-Encoding: gzip\r\n\r\n".to_string(),
        "HEAD /style.css HTTP/1.1\r\nAccept-Encoding: br\r\n\r\n".to_string(),
        "GET /style.css HTTP/1.1\r\n\r\n".to_string(),
        "GET /style.css HTTP/1.1\r\nRange: bytes=2-5\r\n\r\n".to_string(),
        "GET / HTTP/1.1\r\n\r\n".to_string(),
        "GET /docs/ HTTP/1.1\r\n\r\n".to_string(),
        "GET /docs HTTP/1.1\r\n\r\n".to_string(),
        "GET /docs/index.html/ HTTP/1.1\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.1\r\n\r\n".to_string(),
        // A memfd entry's other forms: HEAD, Connection: close, 304.
        "HEAD /mid.bin HTTP/1.1\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.0\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.1\r\nIf-Modified-Since: Fri, 01 Jan 2100 00:00:00 GMT\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.1\r\nRange: bytes=100-199\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.1\r\nRange: bytes=-10\r\n\r\n".to_string(),
        "GET /mid.bin HTTP/1.1\r\nRange: bytes=99999-\r\n\r\n".to_string(),
        "GET /big.bin HTTP/1.1\r\n\r\n".to_string(),
        "GET /missing.css HTTP/1.1\r\n\r\n".to_string(),
        "GET /../a.css HTTP/1.1\r\n\r\n".to_string(),
        "POST /a.css HTTP/1.1\r\nContent-Length: 0\r\n\r\n".to_string(),
        // Pipelined: many requests in one write, answered in order.
        "GET /a.css HTTP/1.1\r\n\r\nHEAD /a.css HTTP/1.1\r\n\r\n".repeat(25),
    ];
    for r in &requests {
        let full = format!("{r}{close}");
        let reference = raw_exchange(off, &full);
        assert!(reference.starts_with(b"HTTP/1."), "{io}: {r:?}");
        let miss = raw_exchange(on, &full);
        let hit = raw_exchange(on, &full);
        let show = |b: &[u8]| String::from_utf8_lossy(&b[..b.len().min(600)]).to_string();
        assert!(
            undated(&miss) == undated(&reference),
            "{io}: first answer differs for {r:?}:\n{}\nvs uncached:\n{}",
            show(&miss),
            show(&reference)
        );
        assert!(
            undated(&hit) == undated(&reference),
            "{io}: cached answer differs for {r:?}:\n{}\nvs uncached:\n{}",
            show(&hit),
            show(&reference)
        );
    }
    // Ranges are still right (and not from the cache).
    let (st, _, body) = get_close(on, "/mid.bin", "Range: bytes=100-199\r\n");
    assert!(st == 206 && body == mid[100..200], "{io}");
    assert!(get_close(on, "/mid.bin", "").2 == mid && get_close(on, "/big.bin", "").2 == big, "{io}");
    // The hits really came from the cache; the uncached server never says so.
    let log = w_on.log();
    let hits = log.lines().filter(|l| l.contains("cache=hit")).count();
    assert!(hits >= requests.len(), "{io}: only {hits} hits:\n{log}");
    assert!(log.lines().any(|l| l.contains("GET /mid.bin 200 40000B") && l.contains("cache=hit")), "{io}");
    assert!(log.lines().filter(|l| l.contains("GET /big.bin 200")).all(|l| l.contains("cache=miss")), "{io}: big.bin");
    assert!(!w_off.log().contains("cache="), "cache_size = 0: no cache at all");

    let valid = Duration::from_millis(300);
    // Polls `path` until `done`; every answer before that must pass `ok`.
    let until = |path: &str, what: &str, done: &dyn Fn(u16, &[u8]) -> bool, ok: &dyn Fn(u16, &[u8]) -> bool| {
        let t0 = Instant::now();
        loop {
            let (st, _, body) = get_close(on, path, "");
            if done(st, &body) {
                return t0.elapsed();
            }
            assert!(ok(st, &body), "{io}: {path} answered {st} {:?} before {what}", String::from_utf8_lossy(&body));
            assert!(t0.elapsed() < valid + Duration::from_millis(700), "{io}: {path} never {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    // Edited in place (same inode, same size): fresh within cache_valid_ms.
    for _ in 0..2 {
        assert_eq!(get_close(on, "/edit.txt", "").2, b"version 1");
    }
    std::fs::OpenOptions::new().write(true).open(site.join("edit.txt")).unwrap().write_all(b"version 2").unwrap();
    let took = until("/edit.txt", "showed the edit", &|_, b| b == b"version 2", &|_, b| b == b"version 1");
    eprintln!("{io}: edit served after {took:?}");
    // Rewritten with the old mtime put back (rsync --inplace): ctime tells.
    assert_eq!(get_close(on, "/inplace.txt", "").2, b"rsync v1");
    let old = std::fs::metadata(site.join("inplace.txt")).unwrap().modified().unwrap();
    let f = std::fs::OpenOptions::new().write(true).open(site.join("inplace.txt")).unwrap();
    (&f).write_all(b"rsync v2").unwrap();
    f.set_modified(old).unwrap();
    drop(f);
    until("/inplace.txt", "showed the rewrite", &|_, b| b == b"rsync v2", &|_, b| b == b"rsync v1");
    // Deleted: 404 within cache_valid_ms.
    assert_eq!(get_close(on, "/gone.txt", "").2, b"here");
    std::fs::remove_file(site.join("gone.txt")).unwrap();
    until("/gone.txt", "went 404", &|st, _| st == 404, &|st, b| st == 200 && b == b"here");
    // Cached, then swapped for a symlink leaving the root: the old content
    // for at most cache_valid_ms, then refused; never the target.
    for _ in 0..2 {
        assert_eq!(get_close(on, "/swap.txt", "").2, b"inside");
    }
    std::os::unix::fs::symlink(dir.join("outside.txt"), site.join("swap.tmp")).unwrap();
    std::fs::rename(site.join("swap.tmp"), site.join("swap.txt")).unwrap();
    until("/swap.txt", "was refused", &|st, _| st == 404, &|st, b| st == 200 && b == b"inside");
    for _ in 0..3 {
        assert_eq!(get_close(on, "/swap.txt", "").0, 404, "{io}");
    }
    // A directory's index swapped the same way.
    assert_eq!(get_close(on, "/docs/", "").2, b"docs home");
    std::fs::remove_file(site.join("docs/index.html")).unwrap();
    std::os::unix::fs::symlink("../../outside.txt", site.join("docs/index.html")).unwrap();
    until("/docs/", "was refused", &|st, _| st == 404, &|st, b| st == 200 && b == b"docs home");

    // On the way out the worker reports what the cache did.
    w_on.terminate(T);
    let log = w_on.wait_log("static cache: ", T);
    let line = log.lines().find(|l| l.contains("static cache: ")).unwrap();
    let n: u64 = line.split("static cache: ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
    assert!(n >= hits as u64, "{line}");
    assert!(line.contains("dropped as changed on disk"), "{line}");
    drop((w_on, w_off));
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
    assert!(out.contains("│ FAIL  │ app broken"), "{out}");
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
    // A site has no working_directory: the folder it serves is its cwd, in the status and in `describe`.
    let root = std::fs::canonicalize(&site).unwrap();
    assert_eq!(f.app("site")["status"]["cwd"], root.to_str().unwrap(), "{}", f.app("site"));
    let d = f.ok(&["describe", "site"]);
    assert!(d.lines().any(|l| l.starts_with("│ cwd ") && l.contains(root.to_str().unwrap())), "{d}");

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
        Self::start_args(f, &["wardend"], env)
    }

    fn start_args(f: &Fleet, args: &[&str], env: &[(&str, &str)]) -> Wardend {
        let out = f.home.join("wardend.out");
        let file = std::fs::File::create(&out).unwrap();
        let child = warden()
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
    let mut child = warden()
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
        // Only if that pid is still our `warden wardend` (not a reused pid).
        let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        if cmdline != format!("{BIN}\0wardend\0").as_bytes() {
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
    let (code, out) = f.cli(&["wardend", "status"]);
    assert_eq!(code, 1, "not running yet: {out}");
    assert!(out.contains("not running"), "{out}");

    // --background detaches it; status answers; a second one is refused.
    let out = f.ok(&["wardend", "--background"]);
    let bg = DetachedWardend::from_output(&out);
    assert!(bg.0.is_some(), "{out}");
    let out = f.ok(&["wardend", "status"]);
    assert!(out.contains("wardend: pid") && out.contains("protocol 1"), "{out}");
    // A second one is not needed and exits 0 (launchd would start a failed one again).
    let (code, out) = f.cli(&["wardend"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("already running"), "{out}");
    let out = f.ok(&["wardend", "--background"]);
    assert!(out.contains("already running"), "{out}");
    // `kill` (everything) is how it is stopped on purpose.
    let out = f.ok(&["kill", "--yes"]);
    assert!(out.contains("wardend: stopped"), "{out}");
    let (code, _) = f.cli(&["wardend", "status"]);
    assert_eq!(code, 1);
    assert!(!f.home.join("run/wardend.sock").exists(), "socket removed");
    assert!(!f.ok(&["kill", "--yes"]).contains("wardend"), "nothing left to stop");

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
    let out = f.ok(&["wardend", "status"]);
    assert!(
        out.lines().any(|l| l.starts_with("│ 0  │ api ") && l.contains("running") && l.contains("wardend")),
        "{out}"
    );
    let out = f.ok(&["kill", "--yes"]);
    assert!(out.contains("api: stopped") && out.contains("wardend: stopped"), "{out}");
    assert_eq!(f.cli(&["wardend", "status"]).0, 1);
}

/// wardend's pid, from `warden wardend status --json`; None when it does not answer.
fn wardend_pid(f: &Fleet, env: &[(&str, &str)]) -> Option<i32> {
    let (code, out) = f.cli_env(&["wardend", "status", "--json"], env);
    if code != 0 {
        return None;
    }
    serde_json::from_str::<Value>(&out).ok()?["hello"]["pid"].as_i64().map(|p| p as i32)
}

/// What it takes for wardend to be always on, with a short look interval so the test is quick.
const ALWAYS_ON: [(&str, &str); 2] = [("WARDEN_NO_DAEMON", "0"), ("WARDEN_REVIVE_EVERY_MS", "200")];

/// wardend comes back whenever it dies (a supervisor starts it again), except when it was
/// stopped on purpose: a clean exit removes its socket, a kill leaves it behind.
#[test]
fn wardend_comes_back_when_killed_but_not_when_stopped_on_purpose() {
    let f = Fleet::new("wd-revive");
    sleeper_config(&f, "api");
    let (code, out) = f.cli_env(&["start", "api"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("wardend started in the background"), "start starts wardend: {out}");
    let first = wardend_pid(&f, &ALWAYS_ON).expect("wardend answers after start");

    // kill -9: no clean exit, so the supervisor starts it again, with another pid.
    unsafe { libc::kill(first, libc::SIGKILL) };
    let t0 = Instant::now();
    let second = loop {
        match wardend_pid(&f, &ALWAYS_ON) {
            Some(p) if p != first => break p,
            _ => {
                assert!(t0.elapsed() < Duration::from_secs(30), "wardend was not started again");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    };
    assert!(f.home.join("run/wardend.lock").exists(), "its lock is there while it runs");

    // SIGTERM is a clean exit, which is what `warden kill` does: it stays stopped, however long the
    // supervisor keeps looking (200 ms here).
    unsafe { libc::kill(second, libc::SIGTERM) };
    let t0 = Instant::now();
    while wardend_pid(&f, &ALWAYS_ON).is_some() {
        assert!(t0.elapsed() < Duration::from_secs(10), "wardend did not stop");
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(2));
    assert!(wardend_pid(&f, &ALWAYS_ON).is_none(), "a clean stop is not undone");
    assert!(!f.home.join("run/wardend.sock").exists(), "a clean exit removes the socket");

    // `resurrect` (and `start`) start it again, even with nothing saved to start; `kill` stops it
    // with the apps.
    let (_, out) = f.cli_env(&["resurrect"], &ALWAYS_ON);
    assert!(wardend_pid(&f, &ALWAYS_ON).is_some(), "resurrect starts it again:\n{out}");
    let out = f.ok(&["kill", "--yes"]);
    assert!(out.contains("wardend: stopped"), "{out}");
    std::thread::sleep(Duration::from_secs(1));
    assert!(wardend_pid(&f, &ALWAYS_ON).is_none(), "kill stops it for good");
}

/// wardend, the keeper and the supervisor all killed at once: nothing of
/// Warden's is left, and nothing is restarted by hand. The workers start
/// wardend again themselves, wardend starts the app, and the app takes the
/// same workers back, which served throughout.
#[cfg(target_os = "linux")]
#[test]
fn wardend_and_the_app_come_back_by_themselves() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("wd-recover");
    let port = free_port();
    std::fs::write(f.home.join("api.toml"), gated("api", port, 2, "")).unwrap();
    let (code, out) = f.cli_env(&["start", "api"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    f.wait("2 ready", |f| f.app("api")["status"]["workers_ready"] == 2);
    let before: HashSet<u64> = f.pids("api").into_iter().collect();
    let status = f.app("api")["status"].clone();
    let wardend = wardend_pid(&f, &ALWAYS_ON).expect("wardend runs");
    let (keeper, sup) = (status["pid"].as_i64().unwrap() as i32, status["supervisor_pid"].as_i64().unwrap() as i32);
    for p in [wardend, sup, keeper] {
        unsafe { libc::kill(p, libc::SIGKILL) };
    }
    let t0 = Instant::now();
    let mut n = 0;
    while t0.elapsed() < Duration::from_secs(2) {
        assert!(get(port, &format!("/say?w=alone-{n}")).is_some(), "request {n} failed");
        n += 1;
        std::thread::sleep(Duration::from_millis(2));
    }
    f.wait("wardend and the app back, with the same workers", |f| {
        let s = &f.app("api")["status"];
        s["workers_ready"] == 2
            && s["supervisor_pid"].as_i64().is_some_and(|p| p as i32 != sup)
            && f.pids("api").into_iter().collect::<HashSet<u64>>() == before
    });
    assert!(wardend_pid(&f, &ALWAYS_ON).is_some_and(|p| p != wardend), "a new wardend");
    for _ in 0..20 {
        assert!(get(port, "/say?w=back").is_some());
    }
    let log = std::fs::read_to_string(f.home.join("state/logs/api.log")).unwrap_or_default();
    assert!(log.contains("worker kept running while no Warden process of this app ran; supervised again"), "{log}");
}

/// With `WARDEN_NO_DAEMON=1` nothing starts it, and nothing brings it back.
#[test]
fn no_daemon_keeps_wardend_off() {
    let f = Fleet::new("wd-none");
    sleeper_config(&f, "api");
    let env = [("WARDEN_NO_DAEMON", "1"), ("WARDEN_REVIVE_EVERY_MS", "200")];
    let (code, out) = f.cli_env(&["start", "api"], &env);
    assert_eq!(code, 0, "{out}");
    std::thread::sleep(Duration::from_secs(1));
    assert!(wardend_pid(&f, &env).is_none(), "off means off");
}

/// Four saved apps that each take 1.5 s to count as up (`min_uptime`), killed, ready to resurrect.
fn four_slow_apps(name: &str) -> (Fleet, [&'static str; 4]) {
    let f = Fleet::new(name);
    let names = ["a", "b", "c", "d"];
    for n in names {
        let cfg = format!(
            "[app]\nname = \"{n}\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\n[workers]\nmin_uptime = 1500\n"
        );
        std::fs::write(f.home.join(format!("{n}.toml")), cfg).unwrap();
    }
    f.ok(&["start", "all"]);
    f.ok(&["save"]);
    f.ok(&["kill", "--yes"]);
    f.wait("all offline", |f| f.list().iter().all(|a| a["status"].is_null()));
    (f, names)
}

/// `warden resurrect` starts the saved apps at the same time: apps that each take a while to
/// count as up come up together, not one after the other.
#[test]
fn resurrect_starts_the_saved_apps_in_parallel() {
    let (f, names) = four_slow_apps("resurrect-parallel");
    let t0 = Instant::now();
    // (`-j 4`: the default is one per core, and a CI runner may have one.)
    let out = f.ok(&["resurrect", "-j", "4"]);
    let took = t0.elapsed();
    for n in names {
        assert!(out.contains(&format!("{n}: online")), "{n}:\n{out}");
    }
    // One after another it takes 4 x 1.5 s of waiting; together, about 1.5 s plus the starts.
    assert!(took < Duration::from_millis(4500), "the apps start one after another: {took:?}\n{out}");
}

/// A host with many apps is not asked to start them all at once: `--parallel N` (or
/// `$WARDEN_PARALLEL`) starts N at a time, the next as one is up.
#[test]
fn resurrect_starts_no_more_apps_at_once_than_parallel_allows() {
    let (f, names) = four_slow_apps("resurrect-limit");
    // Two at a time: two rounds of about 1.5 s.
    let t0 = Instant::now();
    let out = f.ok(&["resurrect", "--parallel", "2"]);
    let took = t0.elapsed();
    for n in names {
        assert!(out.contains(&format!("{n}: online")), "{n}:\n{out}");
    }
    assert!(took >= Duration::from_millis(2900), "two rounds, not one: {took:?}\n{out}");
    assert!(took < Duration::from_millis(5800), "and not four: {took:?}\n{out}");

    // The same from the environment (what a launchd or systemd job can set).
    f.ok(&["kill", "--yes"]);
    f.wait("all offline", |f| f.list().iter().all(|a| a["status"].is_null()));
    let t0 = Instant::now();
    let (code, out) = f.cli_env(&["resurrect"], &[("WARDEN_PARALLEL", "2")]);
    assert_eq!(code, 0, "{out}");
    assert!(t0.elapsed() >= Duration::from_millis(2900), "WARDEN_PARALLEL=2: {:?}\n{out}", t0.elapsed());

    // And `--parallel` is refused where nothing starts.
    let (code, out) = f.cli(&["stop", "a", "-j", "2"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--parallel only applies"), "{out}");
}

/// `warden update`: every app moves to the warden binary that runs it, and wardend restarts
/// from it. An app with a keeper keeps its process (the keeper re-executes itself) and its
/// workers, under a new supervisor; one without is restarted. The app that was stopped stays
/// stopped.
#[test]
fn update_moves_every_app_and_wardend_to_this_binary() {
    let f = Fleet::new("wd-update");
    sleeper_config(&f, "one");
    sleeper_config(&f, "two");
    sleeper_config(&f, "idle");
    std::fs::write(
        f.home.join("bare.toml"),
        "[app]\nname = \"bare\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n[restart]\n\
         keep_workers_on_crash = false\n",
    )
    .unwrap();
    let (code, out) = f.cli_env(&["start", "all"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    f.ok(&["stop", "idle"]);
    let app = |name: &str| f.app(name)["status"].clone();
    let worker = |s: &Value| s["workers"][0]["pid"].as_u64();
    let before: Vec<Value> = ["one", "two", "idle", "bare"].map(app).into();
    let wardend = wardend_pid(&f, &ALWAYS_ON).expect("wardend runs");

    let (code, out) = f.cli_env(&["update", "--yes"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    for step in [
        "update: saving what runs",
        "one: moving to this warden binary; its workers keep serving",
        "idle: runs warden",
        "its workers stay stopped",
        "bare: this app runs without a keeper",
        "update: restarting wardend",
    ] {
        assert!(out.contains(step), "{step}:\n{out}");
    }
    for (name, was) in ["one", "two"].iter().zip(&before) {
        let now = app(name);
        assert_eq!(now["pid"], was["pid"], "{name} keeps its process (the keeper): {now:#}");
        assert_ne!(now["supervisor_pid"], was["supervisor_pid"], "{name} has a new supervisor");
        assert!(worker(&now).is_some() && worker(&now) == worker(was), "{name} keeps its worker: {now:#}");
        assert!(out.contains(&format!("{name}: runs warden")), "{name}:\n{out}");
    }
    let idle = app("idle");
    assert_eq!(idle["stopped"], true, "an app that was stopped stays stopped: {idle}");
    assert_ne!(idle["supervisor_pid"], before[2]["supervisor_pid"], "though its supervisor is new too");
    f.wait("bare is back", |f| f.app("bare")["status"]["workers_ready"] == 1);
    let bare = app("bare");
    assert_ne!(bare["pid"], before[3]["pid"], "bare has no keeper: it was restarted");
    assert_ne!(worker(&bare), worker(&before[3]), "with a new worker");
    let after = wardend_pid(&f, &ALWAYS_ON).expect("wardend runs again");
    assert_ne!(after, wardend, "a new wardend");

    // An update while nothing runs (one before it was cut short) keeps the saved list, with the
    // one it replaces as dump.json.bak, and starts the apps again.
    let (code, out) = f.cli_env(&["kill", "--yes"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    let (code, out) = f.cli_env(&["update", "--yes"], &ALWAYS_ON);
    assert_eq!(code, 0, "{out}");
    for name in ["one", "two", "idle", "bare"] {
        assert!(out.contains(&format!("{name}: not running; kept in the saved list")), "{name}:\n{out}");
    }
    assert!(out.contains("saved 4 app(s)") && out.contains("dump.json.bak"), "{out}");
    f.wait("the apps are back again", |f| {
        f.app("one")["status"]["pid"].is_u64() && f.app("two")["status"]["pid"].is_u64()
    });
    assert_eq!(f.app("idle")["status"]["stopped"], true, "still stopped");
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
    // The hint names the stuck process: the keeper's child.
    let stuck = f.app("api")["status"]["supervisor_pid"].as_u64().unwrap();
    assert_ne!(stuck, new_pid, "the supervisor runs under its keeper");
    unsafe { libc::kill(stuck as i32, libc::SIGSTOP) };
    let unresponsive = ev.wait("unresponsive", sup_event("api", "unresponsive"));
    assert_eq!(unresponsive["pid"].as_u64(), Some(new_pid));
    let a = d.app("api");
    assert_eq!(a["state"], "unreachable", "{a:#}");
    assert!(a["problem"].as_str().unwrap().contains(&format!("gdb -p {stuck}")), "{a:#}");
    assert!(get(port, "/whoami").is_some(), "workers still serve");
    assert!(alive(new_pid) && alive(stuck));
    unsafe { libc::kill(stuck as i32, libc::SIGCONT) };
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
    // wardend's log is written by a thread of its own: the event can come first.
    wait_log(&d, "then run `warden start api`");
    let log = d.log();
    assert!(log.contains("gave up restarting it"), "{log}");
    assert!(log.contains("last_lines=") && log.contains("then run `warden start api`"), "{log}");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(f.app("api")["status"].is_null(), "not restarted after giving up");

    // `warden start api` clears it.
    f.ok(&["start", "api"]);
    let a = d.wait_app("api running", "api", |a| a["state"] == "running");
    assert!(a["problem"].is_null(), "{a:#}");
}

// ---- wardend: alerts and resource history

/// A plain-http server standing in for a webhook: each request's path and
/// body come back on the channel; it answers 200.
fn webhook_server() -> (u16, std::sync::mpsc::Receiver<(String, String)>) {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let mut r = std::io::BufReader::new(s.try_clone().unwrap());
            let (mut first, mut len) = (String::new(), 0usize);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if first.is_empty() {
                    first = line.trim().to_string();
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            if tx.send((first, String::from_utf8_lossy(&body).into_owned())).is_err() {
                return;
            }
        }
    });
    (port, rx)
}

/// The alerts a `command` rule appended to `file`, one JSON object per line.
fn alerts_in(file: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// Wait until wardend's log (written by a thread of its own) has `needle`.
fn wait_log(d: &Wardend, needle: &str) {
    let t0 = Instant::now();
    while !d.log().contains(needle) {
        assert!(t0.elapsed() < T, "{needle:?} is not in wardend's log:\n{}", d.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_alerts(file: &std::path::Path, what: &str, f: impl Fn(&[Value]) -> bool) -> Vec<Value> {
    let t0 = Instant::now();
    loop {
        let got = alerts_in(file);
        if f(&got) {
            return got;
        }
        assert!(t0.elapsed() < T, "timed out waiting for {what}; alerts so far: {got:#?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn wardend_alerts_a_crash_loop_once_and_counts_duplicates() {
    if !have_bun() || Command::new("curl").arg("--version").output().is_err() {
        return;
    }
    let f = Fleet::new("wd-alerts");
    let (hook_port, hooks) = webhook_server();
    let out = f.home.join("alerts.jsonl");
    std::fs::write(
        f.home.join("wardend.toml"),
        format!(
            r#"
[[alert]]
name = "hook"
on = ["crash_loop"]
webhook = "http://127.0.0.1:{hook_port}/services/T0/B0/s3cr3t"
min_interval = "1h"

[[alert]]
name = "log"
on = ["all"]
command = ["/bin/sh", "-c", "cat >> '{out}'; echo >> '{out}'"]
min_interval = "15s"
"#,
            out = out.display()
        ),
    )
    .unwrap();
    // A dead supervisor is restarted after 100 ms; the third death gives up.
    let d = Wardend::start(&f, &[("WARDEN_DAEMON_POLICY", "initial_ms=100,max_ms=200,deaths=3,window_ms=60000")]);
    wait_log(&d, "alert rules read rules=2");
    assert!(f.list().iter().all(|a| a["app"] != "wardend"), "wardend.toml is not an app");
    let mut ev = d.subscribe(r#"{"cmd":"subscribe"}"#);

    // A worker that serves for a second, then exits 1: a crash loop, then FAILED.
    let (fx, port) = (fixture("app.ts"), free_port().to_string());
    let args = ["start", &fx, "--name", "boom", "--port", &port, "--env", "FIXTURE_EXIT_AFTER=1000"];
    let (code, text) = f.cli(&[&args[..], &["--max-restarts", "5", "--restart-delay", "20", "--no-wait"]].concat());
    assert_eq!(code, 0, "{text}");
    ev.wait("boom failed", |v| v["type"] == "worker" && v["app"] == "boom" && v["event"] == "failed");

    // The webhook gets the crash loop, once, as JSON with a text for people.
    let (first, body) = hooks.recv_timeout(T).expect("a webhook POST");
    assert_eq!(first, "POST /services/T0/B0/s3cr3t HTTP/1.1");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        (v["kind"].as_str(), v["app"].as_str(), v["rule"].as_str()),
        (Some("crash_loop"), Some("boom"), Some("hook"))
    );
    assert!(v["text"].as_str().unwrap().contains("boom crash loop: 3 worker crashes within 5m"), "{v}");
    // The command gets every kind: the crash loop and the failed worker.
    let got = wait_for_alerts(&out, "crash_loop and worker_failed", |a| {
        a.iter().any(|v| v["kind"] == "crash_loop") && a.iter().any(|v| v["kind"] == "worker_failed")
    });
    let wf = got.iter().find(|v| v["kind"] == "worker_failed").unwrap();
    assert!(wf["detail"].as_str().unwrap().contains("warden reset boom"), "{wf}");
    assert_eq!((wf["app"].as_str(), wf["count"].as_u64()), (Some("boom"), Some(1)), "{wf}");

    // A supervisor killed three times: `died` goes out once, the two deaths
    // within min_interval follow in one alert (count 2), `gave_up` on its own.
    let p2 = free_port().to_string();
    f.ok(&["start", &fx, "--name", "api", "--port", &p2]);
    let mut pid = d.wait_app("api watched", "api", |a| a["state"] == "running")["supervisor_pid"].as_u64().unwrap();
    for death in 1..=3 {
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        ev.wait("api died", sup_event("api", "died"));
        if death < 3 {
            pid = ev.wait("api started", sup_event("api", "started"))["pid"].as_u64().unwrap();
            d.wait_app("api running again", "api", |a| a["state"] == "running");
        }
    }
    ev.wait("api gave up", sup_event("api", "gave_up"));
    let got = wait_for_alerts(&out, "the held-back deaths", |a| {
        a.iter().filter(|v| v["kind"] == "died" && v["app"] == "api").count() >= 2
    });
    let died: Vec<&Value> = got.iter().filter(|v| v["kind"] == "died").collect();
    assert_eq!(died.len(), 2, "{got:#?}");
    assert_eq!((died[0]["count"].as_u64(), died[1]["count"].as_u64()), (Some(1), Some(2)), "{died:#?}");
    assert!(died[1]["text"].as_str().unwrap().contains("(×2 since"), "{}", died[1]);
    let gave_up = got.iter().find(|v| v["kind"] == "gave_up").expect("gave_up");
    assert!(gave_up["detail"].as_str().unwrap().contains("warden start api"), "{gave_up}");
    assert_eq!(got.iter().filter(|v| v["kind"] == "crash_loop").count(), 1, "one loop, one alert: {got:#?}");
    assert!(hooks.recv_timeout(Duration::from_millis(300)).is_err(), "no second webhook");
    assert!(!d.log().contains("s3cr3t"), "the webhook's secret never reaches the log:\n{}", d.log());

    // The resource history has boom's restarts (committed every 10 s).
    let t0 = Instant::now();
    loop {
        let h = d.request(r#"{"cmd":"history","app":"boom"}"#);
        assert_eq!(h["ok"], true, "{h}");
        let hist = &h["history"];
        let sum = |key: &str| -> u64 {
            hist["apps"][0][key].as_array().map(|a| a.iter().filter_map(Value::as_u64).sum()).unwrap_or(0)
        };
        if sum("restarts") > 0 && sum("rss_bytes") > 0 {
            assert_eq!(hist["step_s"], 10);
            let n = hist["points"].as_u64().unwrap() as usize;
            assert_eq!(hist["apps"][0]["workers_ready"].as_array().unwrap().len(), n);
            assert_eq!(hist["host"]["cpu_percent"].as_array().unwrap().len(), n);
            assert!(hist["host"]["mem_total_bytes"].as_u64() > Some(0), "{hist}");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(40), "no restarts in the history: {hist}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[test]
fn wardend_alert_rules_are_checked_and_read_again_when_the_file_changes() {
    let f = Fleet::new("wd-alert-rules");
    let file = f.home.join("wardend.toml");
    let file_arg = file.to_str().unwrap();
    // `warden check -c wardend.toml` is the check; no file: nothing to check.
    let (code, out) = f.cli(&["check", "-c", file_arg]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("does not exist"), "{out}");
    // Every problem, with where it is and how to fix it.
    std::fs::write(&file, "[[alert]]\non = [\"crashes\"]\nwebhook = \"ftp://x/s3cr3t\"\n\n[[alert]]\non = [\"all\"]\n")
        .unwrap();
    let (code, out) = f.cli(&["check", "-c", file_arg]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("has 3 problems"), "{out}");
    assert!(out.contains("alert #1 (line 1): unknown event \"crashes\""), "{out}");
    assert!(out.contains("alert #1 (line 1): webhook ftp://x/… must start with https://"), "{out}");
    assert!(out.contains("alert #2 (line 5): needs `command"), "{out}");
    assert!(!out.contains("s3cr3t"), "{out}");
    // `warden doctor` says so too, for the file wardend reads.
    let (_, out) = f.cli(&["doctor"]);
    assert!(out.contains("alerts") && out.contains("3 problem(s)"), "{out}");
    let good = "[[alert]]\nname = \"ops\"\non = [\"gave_up\", \"died\"]\ncommand = [\"/bin/sh\"]\n";
    std::fs::write(&file, good).unwrap();
    let out = f.ok(&["check", "-c", file_arg]);
    assert!(out.contains("ok, 1 alert rule"), "{out}");
    assert!(out.contains("ops: gave_up, died of every app → command /bin/sh (min_interval 5m)"), "{out}");
    let elsewhere = f.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let other = elsewhere.join("wardend.toml");
    std::fs::write(&other, "[[alert]]\non = [\"all\"]\ncommand = [\"/bin/sh\"]\napps = [\"ghost\"]\n").unwrap();
    let out = f.ok(&["check", "-c", other.to_str().unwrap()]);
    assert!(out.contains("no app \"ghost\" on this host"), "{out}");

    let mut d = Wardend::start(&f, &[]);
    wait_log(&d, "alert rules read rules=1");
    // A bad file is noticed by itself: the running rules stay, and the log says why.
    std::fs::write(&file, "[[alert]]\non = [\"all\"]\ncommand = \"/bin/sh\"\n").unwrap();
    wait_log(&d, "wardend.toml has errors; keeping the alert rules in force");
    assert!(d.log().contains("on=\"the file changed\""), "{}", d.log());
    // Fixed: read again by itself, no command and no signal.
    std::fs::write(&file, format!("{good}\n[[alert]]\non = [\"all\"]\ncommand = [\"/bin/sh\"]\n")).unwrap();
    wait_log(&d, "alert rules read rules=2 file=");
    // SIGHUP (and the `reload` request) still read it at once, and no longer stop wardend.
    unsafe { libc::kill(d.child.id() as i32, libc::SIGHUP) };
    std::thread::sleep(Duration::from_millis(300));
    assert!(d.child.try_wait().unwrap().is_none(), "still running after SIGHUP");
    let r = d.request(r#"{"cmd":"reload"}"#);
    assert_eq!(r["ok"], true, "{r}");
    assert!(r["message"].as_str().unwrap().contains("2 alert rules loaded from"), "{r}");

    // The history answers at once, with the host's series on one grid.
    let h = d.request(r#"{"cmd":"history","app":"","step_s":60}"#);
    assert_eq!(h["ok"], true, "{h}");
    assert_eq!(h["history"]["step_s"], 60);
    let n = h["history"]["points"].as_u64().unwrap();
    assert!((1440..=1442).contains(&n), "24 h at one point a minute: {n}");
    assert_eq!(h["history"]["host"]["load1"].as_array().unwrap().len() as u64, n);
    assert_eq!(h["history"]["apps"], serde_json::json!([]), "\"\" asks for the host only");
}

/// `history` of `app` on a fixed grid (10 s points from `since_ms`).
fn history_of(d: &Wardend, app: &str, since_ms: u64) -> Value {
    let h = d.request(&format!(r#"{{"cmd":"history","app":"{app}","since_ms":{since_ms},"step_s":10}}"#));
    assert_eq!(h["ok"], true, "{h}");
    h["history"].clone()
}

/// The points of `app`'s `key` series that have a value: (index, value).
fn points(h: &Value, key: &str) -> Vec<(usize, Value)> {
    let Some(a) = h["apps"].as_array().and_then(|a| a.first()) else { return Vec::new() };
    a[key].as_array().unwrap().iter().cloned().enumerate().filter(|(_, v)| !v.is_null()).collect()
}

/// The history file's modification time (a snapshot is renamed into place).
fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// wardend's resource history survives its restarts: a clean stop (SIGTERM)
/// saves it, a kill -9 keeps the last periodic snapshot, and a damaged file
/// is moved aside with a warning while wardend starts with an empty history.
#[test]
fn wardend_history_survives_restarts_and_a_bad_file_is_moved_aside() {
    use std::os::unix::fs::PermissionsExt;
    let f = Fleet::new("wd-history");
    f.ok(&["start", "sleep 300", "--name", "napper"]);
    let file = f.home.join("state/wardend-history.bin");
    // A snapshot every half second instead of every minute (debug builds only).
    let env = [("WARDEN_HISTORY_SAVE_MS", "500")];
    let since = Instant::now();
    let since_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 - 60_000;
    let d = Wardend::start(&f, &env);
    // Two committed samples of napper (a sample per 10 s of wall-clock time).
    let before = loop {
        let h = history_of(&d, "napper", since_ms);
        if points(&h, "rss_bytes").len() >= 2 {
            break h;
        }
        assert!(since.elapsed() < Duration::from_secs(45), "no samples of napper: {h}\n{}", d.log());
        std::thread::sleep(Duration::from_millis(500));
    };
    let n = before["points"].as_u64().unwrap() as usize;
    let keys = ["cpu_percent", "rss_bytes", "workers_ready", "workers_configured", "restarts"];

    // A clean stop saves it, privately; the next wardend loads it and
    // answers the same for those points.
    let pid = d.child.id();
    drop(d);
    assert!(!alive(pid as u64));
    let log = std::fs::read_to_string(f.home.join("wardend.out")).unwrap();
    assert!(log.contains("resource history saved file="), "{log}");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    assert!(!f.home.join("state/.wardend-history.bin.tmp").exists(), "no temporary file left");
    let d = Wardend::start(&f, &env);
    wait_log(&d, "resource history loaded file=");
    assert!(d.log().contains(" apps=1 "), "{}", d.log());
    let after = history_of(&d, "napper", since_ms);
    for key in keys {
        let old: Vec<(usize, Value)> = points(&before, key).into_iter().filter(|(i, _)| *i < n - 1).collect();
        let new: Vec<(usize, Value)> = points(&after, key).into_iter().filter(|(i, _)| *i < n - 1).collect();
        assert!(!old.is_empty(), "{key}: {before}");
        assert_eq!(old, new, "{key} before and after the restart");
    }
    assert_eq!(points(&after, "workers_ready")[0].1, 1, "{after}");

    // kill -9: the last periodic snapshot is there. Wait for a sample after
    // the restart, then for a snapshot written after it.
    let committed = points(&after, "rss_bytes").len();
    let t0 = Instant::now();
    let newer = loop {
        let h = history_of(&d, "napper", since_ms);
        if points(&h, "rss_bytes").len() > committed {
            break h;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "no new sample:\n{}", d.log());
        std::thread::sleep(Duration::from_millis(300));
    };
    // A file modified after this holds every sample `newer` has (the kernel's
    // file times lag the clock a little, never lead it).
    let seen = std::time::SystemTime::now();
    let t0 = Instant::now();
    while mtime(&file).is_none_or(|m| m <= seen) {
        assert!(t0.elapsed() < T, "no snapshot after the new sample:\n{}", d.log());
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut d = d;
    d.child.kill().unwrap();
    d.child.wait().unwrap();
    let d = Wardend::start(&f, &env);
    wait_log(&d, "resource history loaded file=");
    let back = history_of(&d, "napper", since_ms);
    let saved = points(&newer, "rss_bytes");
    let last = saved.last().unwrap().0;
    assert_eq!(
        points(&back, "rss_bytes").into_iter().filter(|(i, _)| *i <= last).collect::<Vec<_>>(),
        saved,
        "every sample up to the last snapshot survives a kill -9"
    );

    // A damaged file: a warning with the fix, the file moved aside, an
    // empty history (nothing before this start), and wardend runs on.
    drop(d);
    let bytes = std::fs::read(&file).unwrap();
    std::fs::write(&file, &bytes[..bytes.len() - 10]).unwrap();
    let started_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
    let d = Wardend::start(&f, &env);
    wait_log(&d, "cannot use the resource history on disk; starting with an empty one");
    let log = d.log();
    assert!(log.contains("it is truncated") && log.contains("moved_to="), "{log}");
    every_warning_has_a_hint(&log);
    assert_eq!(std::fs::read(f.home.join("state/wardend-history.bin.bad")).unwrap(), &bytes[..bytes.len() - 10]);
    let fresh = history_of(&d, "napper", since_ms);
    let first_new = ((started_ms / 1000 - since_ms / 1000) / 10).saturating_sub(1) as usize;
    assert!(points(&fresh, "rss_bytes").iter().all(|(i, _)| *i >= first_new), "nothing from before: {fresh}");
    // The next snapshot is a good one again.
    let t0 = Instant::now();
    while !file.exists() {
        assert!(t0.elapsed() < Duration::from_secs(30), "no new snapshot:\n{}", d.log());
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(d.request(r#"{"cmd":"hello"}"#)["ok"] == true);
}

/// `on = ["oom"]` fires on the supervisor's own OOM exit reason, once per OOM
/// kill, and never for a plain kill -9. A fake `memory.events` (debug builds
/// only) stands in for the kernel's OOM kill counter.
#[test]
fn wardend_alerts_an_oom_kill_once() {
    if !have_bun() {
        return;
    }
    let f = Fleet::new("wd-oom");
    let out = f.home.join("alerts.jsonl");
    let events = f.home.join("memory.events");
    std::fs::write(&events, "low 0\nhigh 0\nmax 5\noom 2\noom_kill 3\noom_group_kill 0\n").unwrap();
    std::fs::write(
        f.home.join("wardend.toml"),
        format!(
            "[[alert]]\nname = \"oom\"\non = [\"oom\"]\ncommand = [\"/bin/sh\", \"-c\", \"cat >> '{out}'; echo >> \
             '{out}'\"]\nmin_interval = \"0s\"\n",
            out = out.display()
        ),
    )
    .unwrap();
    let port = free_port().to_string();
    let (code, text) = f.cli_env(
        &["start", &fixture("app.ts"), "--name", "api", "--port", &port],
        &[("WARDEN_TEST_MEMORY_EVENTS", events.to_str().unwrap())],
    );
    assert_eq!(code, 0, "{text}");
    let d = Wardend::start(&f, &[]);
    wait_log(&d, "alert rules read rules=1");
    let mut ev = d.subscribe(r#"{"cmd":"subscribe"}"#);
    ev.wait("api status", |v| v["type"] == "status" && v["app"] == "api");
    let crashed = |v: &Value| v["type"] == "worker" && v["app"] == "api" && v["event"] == "crashed";

    // The OOM killer's kill: the counter rose, the worker died of SIGKILL.
    std::fs::write(&events, "low 0\nhigh 0\nmax 9\noom 3\noom_kill 4\noom_group_kill 0\n").unwrap();
    let pid = f.pids("api")[0];
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let c = ev.wait("the OOM crash", crashed);
    assert_eq!(c["detail"], "killed by the kernel OOM killer (out of memory)", "{c}");
    let got = wait_for_alerts(&out, "the oom alert", |a| !a.is_empty());
    assert_eq!(got.len(), 1, "{got:#?}");
    assert_eq!((got[0]["kind"].as_str(), got[0]["app"].as_str()), (Some("oom"), Some("api")), "{got:#?}");
    assert!(got[0]["detail"].as_str().unwrap().contains("killed for lack of memory"), "{got:#?}");

    // A kill -9 that is not the OOM killer (the counter didn't move): no alert.
    f.wait("api restarted", |f| f.pids("api").first().is_some_and(|p| *p != pid));
    d.wait_app("api ready", "api", |a| a["status"]["workers_ready"] == 1);
    let pid = f.pids("api")[0];
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let c = ev.wait("the kill -9 crash", crashed);
    assert_eq!(c["detail"], "killed by another process (SIGKILL)", "{c}");
    std::thread::sleep(Duration::from_millis(1500));
    let got = alerts_in(&out);
    assert_eq!(got.len(), 1, "one OOM kill, one alert: {got:#?}");
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
    // Old workers drain in the background, as the next ones are replaced.
    assert!(
        phases.iter().any(|p| p.contains("old worker draining (pid ") || p.contains("old workers draining (pid ")),
        "{phases:?}"
    );
    assert!(!phases.iter().any(|p| p.contains("0 old workers") || p.contains("(pid )")), "{phases:?}");
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
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe","interval_ms":500}"#);
    assert_eq!(ev.next()["type"], "hello");
    assert_eq!(ev.next()["type"], "status");
    // Two more (at 0.5 s and 1 s) in the 1.6 s after the snapshot: the clock
    // starts there, as on a busy machine connecting takes a while.
    let t0 = Instant::now();
    let mut periodic = 0;
    while let Some(left) = Duration::from_millis(1600).checked_sub(t0.elapsed()) {
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
    assert!(periodic >= 2, "{periodic} status events in the 1.6 s after the snapshot: {:#?}", ev.seen);

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
    // The event loop panics on its 8th tick; no keeper starts it again.
    let (mut w, _port) = fault_warden_env("sub-panic", "tick:8", 1, &[("WARDEN_KEEPER", "0")]);
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
    let dir = tmp().join(format!("warden-it-{name}-direct-{}", std::process::id()));
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
        // Time the host froze Warden for is not counted (see Warden::wait_for).
        if t0.elapsed() > T + w.frozen().min(Duration::from_secs(60)) {
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
    let out = warden().arg("check").arg("-c").arg(&cfg).output().unwrap();
    let text = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("workers.count = 2") && text.contains("per_worker_files = true"), "{text}");
    std::fs::write(&cfg, format!("{base}per_worker_files = true\n")).unwrap();
    let out = warden().arg("check").arg("-c").arg(&cfg).output().unwrap();
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
    let mut follow = warden().args(["logs", "-f", "-n", "1", "-c"]).arg(&w.cfg).stdout(Stdio::piped()).spawn().unwrap();
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

/// `warden logs -f` keeps up with a burst: 2,000 lines in one write used to
/// overflow the follow channel (256 one-line slots) at once, and a client
/// reading promptly saw "... lines skipped".
#[test]
fn logs_follow_takes_a_burst_whole() {
    let dir = direct_dir("follow-burst");
    let (input, go) = (dir.join("burst.txt"), dir.join("go"));
    let text: String = (1..=2000).map(|i| format!("burst {i}\n")).collect();
    std::fs::write(&input, text).unwrap();
    let script =
        format!("while [ ! -e {} ]; do sleep 0.05; done; cat {}; exec sleep 300", go.display(), input.display());
    let cfg = format!(
        "[app]\nname = \"follow-burst\"\ncommand = \"sh\"\nargs = [\"-c\", \"{script}\"]\n\
         [workers]\nmin_uptime = 100\n[logging]\nmax_lines_per_sec = 0\n"
    );
    let w = Warden::start("follow-burst", 0, &cfg);
    w.wait_log("worker ready", T);
    let mut follow = warden().args(["logs", "-f", "-n", "1", "-c"]).arg(&w.cfg).stdout(Stdio::piped()).spawn().unwrap();
    let mut reader = std::io::BufReader::new(follow.stdout.take().unwrap());
    let mut line = String::new();
    // The snapshot line: the follower is subscribed from here on.
    assert!(std::io::BufRead::read_line(&mut reader, &mut line).unwrap() > 0);
    std::fs::write(&go, "").unwrap();
    let (mut got, mut other) = (Vec::new(), Vec::new());
    let t0 = Instant::now();
    while got.last() != Some(&2000) && t0.elapsed() < T {
        line.clear();
        if std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) == 0 {
            break;
        }
        match line.trim_end().rsplit_once("stdout: burst ").and_then(|(_, n)| n.parse::<u32>().ok()) {
            Some(n) => got.push(n),
            None => other.push(line.clone()),
        }
    }
    let _ = follow.kill();
    let _ = follow.wait();
    assert!(other.iter().all(|l| !l.contains("skipped")), "{other:?}");
    assert_eq!(got, (1..=2000).collect::<Vec<u32>>(), "every line, in order; also seen: {other:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `warden flush` empties every current log file in both output modes,
/// like `pm2 flush`: Warden's log, each worker's out and err file (also one
/// no running worker writes), keeps the rotated ones, and writing goes on
/// from the start of each file with whole lines (no hole, no cut line).
#[test]
fn flush_empties_the_log_files_in_both_modes() {
    for mode in ["capture", "direct"] {
        let dir = direct_dir(&format!("flush-{mode}"));
        let logs = dir.join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // A worker of an earlier run (count was 3), with a rotated file.
        std::fs::write(logs.join("out-3.log"), "from an earlier run\n").unwrap();
        std::fs::write(logs.join("out-3.log.1"), "older still\n").unwrap();
        // A burst that rotates (4 KB files), then slow ticks that don't.
        let script = "i=0; while [ $i -lt 150 ]; do i=$((i+1)); echo burst $i padding padding padding padding; \
                      done; echo boom >&2; i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.05; done";
        let toml = format!(
            "[app]\nname = \"flush-{mode}\"\ncommand = \"sh\"\nargs = [\"-c\", \"{script}\"]\n\
             [workers]\ncount = 2\nmin_uptime = 100\n\
             [logging]\nworker_output = \"{mode}\"\nper_worker_files = true\nfile = \"{}\"\n\
             out_file = \"{}\"\nerr_file = \"{}\"\n[logging.rotate]\nmax_size = \"4K\"\nkeep = 10\n",
            logs.join("warden.log").display(),
            logs.join("out.log").display(),
            logs.join("err.log").display()
        );
        let w = Warden::start(&format!("flush-{mode}"), 0, &toml);
        w.wait_for("2 workers", T, ready(2));
        let ticks = |n: u32| -> Vec<u64> {
            let text = std::fs::read_to_string(logs.join(format!("out-{n}.log"))).unwrap_or_default();
            assert!(!text.contains('\0'), "{mode}: a hole in out-{n}.log: {text:?}");
            text.lines().filter_map(|l| l.strip_prefix("tick ")?.parse().ok()).collect()
        };
        eventually(&w, "ticks from both workers", || ticks(1).len() >= 3 && ticks(2).len() >= 3);
        assert!(logs.join("out-1.log.1").exists(), "{mode}: the burst rotated");
        let before = [*ticks(1).last().unwrap(), *ticks(2).last().unwrap()];

        let (code, text) = w.cli(&["flush"]);
        assert_eq!(code, 0, "{mode}: {text}");
        assert!(text.contains("rotated files are kept"), "{mode}: {text}");
        for f in ["warden.log", "out-1.log", "out-2.log", "out-3.log", "err-1.log", "err-2.log"] {
            assert!(text.contains(&logs.join(f).display().to_string()), "{mode}: {f} not listed:\n{text}");
        }
        assert!(!text.contains("out-1.log.1"), "{mode}: a rotated file listed:\n{text}");
        // Emptied: only what came after the flush is there.
        assert_eq!(std::fs::read_to_string(logs.join("out-3.log")).unwrap(), "", "{mode}");
        assert_eq!(std::fs::read_to_string(logs.join("err-1.log")).unwrap(), "", "{mode}");
        for (i, n) in [1, 2].into_iter().enumerate() {
            let after = ticks(n);
            assert!(after.first().is_none_or(|t| *t > before[i]), "{mode}: worker {n} kept old lines: {after:?}");
        }
        // Rotated files are kept, the burst with them.
        assert_eq!(std::fs::read_to_string(logs.join("out-3.log.1")).unwrap(), "older still\n", "{mode}");
        let rotated: String = (1..=10)
            .map(|i| std::fs::read_to_string(logs.join(format!("out-1.log.{i}"))).unwrap_or_default())
            .collect();
        assert!(rotated.contains("burst 1 padding"), "{mode}: rotated files lost: {rotated:?}");

        // Writing goes on from the start of each file, whole lines only.
        eventually(&w, "new ticks after the flush", || ticks(1).len() >= 3 && ticks(2).len() >= 3);
        for n in [1, 2] {
            let text = std::fs::read_to_string(logs.join(format!("out-{n}.log"))).unwrap();
            assert!(text.lines().all(|l| l.starts_with("tick ")), "{mode}: out-{n}.log: {text:?}");
            let t = ticks(n);
            assert!(t.windows(2).all(|p| p[1] == p[0] + 1), "{mode}: out-{n}.log: {t:?}");
        }
        // The log file is written by a thread of its own: the line that says the flush is done
        // follows the command's answer by a moment.
        eventually(&w, "the flush line in warden.log", || {
            std::fs::read_to_string(logs.join("warden.log")).is_ok_and(|l| l.contains("logs flushed on request"))
        });
        let log = std::fs::read_to_string(logs.join("warden.log")).unwrap();
        assert!(!log.contains('\0') && !log.contains("burst 1 ") && !log.contains("worker ready"), "{mode}: {log}");
        assert!(log.contains("logs flushed on request files=6 failed=0"), "{mode}: {log}");
        // The in-memory buffer was emptied too.
        let (_, recent) = w.cli(&["logs", "--nostream", "-n", "500"]);
        assert!(!recent.contains("burst 1 ") && !recent.contains("worker ready"), "{mode}: {recent}");
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `warden logs --history` with a file per worker (direct mode's, or
/// capture mode's out/err files): every worker's files, rotated and
/// gzipped ones too, each oldest first, workers in order; `--out`, `--err`,
/// `--worker N`, `--grep`/`search`, `--lines` and `--json` pick from them.
#[test]
fn log_history_reads_every_workers_files() {
    for mode in ["direct", "capture"] {
        let dir = direct_dir(&format!("history-{mode}"));
        let logs = dir.join("logs");
        let script = "i=0; while [ $i -lt 300 ]; do i=$((i+1)); echo w$WARDEN_WORKER_ID line $i padding padding; \
                      done; echo boom $WARDEN_WORKER_ID >&2; exec sleep 300";
        let toml = format!(
            "[app]\nname = \"history-{mode}\"\ncommand = \"sh\"\nargs = [\"-c\", \"{script}\"]\n\
             [workers]\ncount = 2\nmin_uptime = 100\n\
             [logging]\nworker_output = \"{mode}\"\nper_worker_files = true\nfile = \"{}\"\n\
             out_file = \"{}\"\nerr_file = \"{}\"\n[logging.rotate]\nmax_size = \"4K\"\nkeep = 20\ncompress = true\n",
            logs.join("warden.log").display(),
            logs.join("out.log").display(),
            logs.join("err.log").display()
        );
        let w = Warden::start(&format!("history-{mode}"), 0, &toml);
        w.wait_for("2 workers", T, ready(2));
        let has = |f: &str, text: &str| std::fs::read_to_string(logs.join(f)).is_ok_and(|t| t.contains(text));
        eventually(&w, "every line written", || has("err-1.log", "boom 1") && has("err-2.log", "boom 2"));
        eventually(&w, "rotated files gzipped", || {
            logs.join("out-1.log.1.gz").exists() && !logs.join("out-1.log.1").exists()
        });
        let history = |args: &[&str]| -> String {
            let mut all = vec!["logs", "--history"];
            all.extend_from_slice(args);
            let (code, text) = w.cli(&all);
            assert_eq!(code, 0, "{mode}: logs --history {args:?}:\n{text}");
            text
        };
        let numbers = |text: &str, prefix: &str| -> Vec<u32> {
            text.lines().filter_map(|l| l.strip_prefix(prefix)?.split(' ').next()?.parse().ok()).collect()
        };

        // --out: both workers' files, rotated and .gz included, in order, labelled.
        let out = history(&["--out"]);
        for n in [1, 2] {
            let got = numbers(&out, &format!("worker={n} stdout: w{n} line "));
            assert_eq!(got, (1..=300).collect::<Vec<_>>(), "{mode}: worker {n}:\n{out}");
        }
        let first_w2 = out.lines().position(|l| l.starts_with("worker=2 ")).unwrap();
        assert!(out.lines().skip(first_w2).all(|l| !l.starts_with("worker=1 ")), "{mode}: workers in order");
        assert!(!out.contains("boom"), "{mode}: --out has no stderr");
        // --out --worker 2: that file alone, as written.
        let two = history(&["--out", "--worker", "2"]);
        assert_eq!(numbers(&two, "w2 line "), (1..=300).collect::<Vec<_>>(), "{mode}:\n{two}");
        assert_eq!(two.lines().count(), 300, "{mode}:\n{two}");
        // --err: both workers' stderr.
        let err = history(&["--err"]);
        assert_eq!(err.lines().collect::<Vec<_>>(), vec!["worker=1 stderr: boom 1", "worker=2 stderr: boom 2"]);
        // --lines N: the last N of each file.
        let last = history(&["--out", "--lines", "2"]);
        assert_eq!(numbers(&last, "worker=1 stdout: w1 line "), vec![299, 300], "{mode}:\n{last}");
        assert_eq!(numbers(&last, "worker=2 stdout: w2 line "), vec![299, 300], "{mode}:\n{last}");
        // search / --grep: across every file.
        let (code, found) = w.cli(&["search", "w2 line 150 "]);
        assert_eq!(code, 0, "{found}");
        assert_eq!(found.lines().count(), 1, "{mode}:\n{found}");
        assert!(found.contains("w2 line 150 padding"), "{mode}:\n{found}");
        let json = history(&["--grep", "boom 2", "--json"]);
        let v: Value = serde_json::from_str(json.lines().next().unwrap()).unwrap();
        assert_eq!((v["worker"].as_str(), v["stream"].as_str()), (Some("2"), Some("stderr")), "{mode}: {json}");
        assert_eq!(v["message"], "boom 2");
        // --worker 1: Warden's lines about it and its output, nothing of worker 2.
        let one = history(&["--worker", "1"]);
        assert!(one.contains("worker ready worker=1"), "{mode}:\n{one}");
        assert!(one.contains("w1 line 300 ") && one.contains("boom 1"), "{mode}:\n{one}");
        assert!(!one.contains("w2 line") && !one.contains("boom 2"), "{mode}:\n{one}");
        if mode == "direct" {
            // Everything: Warden's events first, then each worker's files.
            let all = history(&[]);
            let events = all.lines().position(|l| l.contains("INFO  all workers ready")).unwrap();
            let output = all.lines().position(|l| l.starts_with("worker=1 stdout: w1 line 1 ")).unwrap();
            assert!(events < output, "{mode}: events first:\n{all}");
            assert_eq!(numbers(&all, "worker=2 stdout: w2 line ").len(), 300, "{mode}");
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// `warden env N` prints what worker N really started with: every value
/// matches its /proc/<pid>/environ (env_file, env, Warden's variables, and
/// Warden's winning over the app's).
#[test]
fn env_shows_what_a_worker_starts_with() {
    let dir = direct_dir("env");
    std::fs::write(dir.join("api.env"), "DB_URL=postgres://u:p@h/db\nNODE_ENV=dev\n").unwrap();
    let port = free_port();
    let toml = format!(
        "[app]\nname = \"envapp\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 300\"]\nport = {port}\n\
         env_file = \"{}\"\nenv = {{ NODE_ENV = \"production\", PORT = \"1\" }}\n\
         [workers]\ncount = 2\nport_strategy = \"offset\"\nready_timeout = 60\n",
        dir.join("api.env").display()
    );
    let w = Warden::start("env", port, &toml);
    let st = w.wait_for("2 workers started", T, |s| Warden::pids(s).len() == 2);
    let pid = st["workers"][1]["pid"].as_u64().unwrap();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    let real: std::collections::HashMap<String, String> = environ
        .split(|b| *b == 0)
        .filter_map(|kv| {
            let s = String::from_utf8_lossy(kv);
            s.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
        })
        .collect();
    let (code, text) = w.cli(&["env", "2", "--show-secrets"]);
    assert_eq!(code, 0, "{text}");
    let mut shown = std::collections::HashMap::new();
    for l in text.lines().filter(|l| !l.starts_with('#')) {
        let (k, v) = l.split_once('=').unwrap();
        shown.insert(k.to_string(), v.split("   #").next().unwrap().to_string()); // the last one wins
    }
    for (k, v) in &shown {
        assert_eq!(real.get(k), Some(v), "{k}: `warden env` says {v:?}, the worker has {:?}\n{text}", real.get(k));
    }
    for k in ["DB_URL", "NODE_ENV", "PORT", "NODE_APP_INSTANCE", "WARDEN_WORKER_ID", "WARDEN_APP", "WARDEN_IPC_FD"] {
        assert!(shown.contains_key(k), "{k} missing:\n{text}");
    }
    assert_eq!((shown["PORT"].as_str(), shown["NODE_ENV"].as_str()), (&*(port + 1).to_string(), "production"));
    assert_eq!((shown["NODE_APP_INSTANCE"].as_str(), shown["WARDEN_WORKER_ID"].as_str()), ("1", "2"));
    assert!(text.contains("overrides the value from env"), "{text}");
    // Without --show-secrets the app's values are hidden, Warden's are not.
    let (_, text) = w.cli(&["env"]);
    assert!(text.contains("DB_URL=(hidden, 19 chars)") && text.contains("WARDEN_WORKER_ID=1\n"), "{text}");
    drop(w);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Event-loop delay (W2): the shim measures each worker's and the heartbeat
/// carries it to `status --json`, `list` and `/metrics`: on Bun and Node
/// (whose histogram counts the sampling period, which must not show), per
/// Worker in worker mode; a loop that stays slow gets one WARN.
#[test]
fn event_loop_delay_is_reported() {
    if !have_bun() {
        return;
    }
    let delay = |s: &Value, i: usize, q: &str| s["workers"][i]["loop_delay"][q].as_f64();
    // Bun, process mode: worker 2 blocks its loop for 150 ms every 400 ms.
    // The shim samples every 100 ms, so each block holds up a sample by at
    // least 50 ms (shorter blocks are only caught when a sample falls in them).
    let (port, metrics) = (free_port(), free_port());
    let cfg = format!(
        "[app]\nname = \"loopy\"\nargs = [\"{}\"]\nport = {port}\nenv = {{ FIXTURE_BLOCK_MS = \"150\", \
         FIXTURE_BLOCK_WORKER = \"2\" }}\n[workers]\ncount = 2\n[watchdog]\nloop_delay_warn = 0.03\n\
         [metrics]\nlisten = \"127.0.0.1:{metrics}\"\n",
        fixture("app.ts")
    );
    let w = Warden::start("loopy", port, &cfg);
    let s = w.wait_for("loop delay of both workers", T, |s| {
        s["workers_ready"] == 2 && delay(s, 0, "p50_ms").is_some() && delay(s, 1, "max_ms").is_some_and(|m| m >= 40.0)
    });
    let (quiet, busy) = (delay(&s, 0, "p50_ms").unwrap(), delay(&s, 1, "max_ms").unwrap());
    assert!(quiet < 20.0, "an idle loop runs on time: p50 {quiet} ms\n{s:#}");
    assert!((40.0..2000.0).contains(&busy), "150 ms blocks: max {busy} ms\n{s:#}");
    let (_, list) = w.cli(&["status"]);
    assert!(list.contains("loop p99"), "{list}");
    // One WARN once it stays high for 10 heartbeats, naming the worker.
    let log = w.wait_log("worker event loop delay is high", T);
    let line = log.lines().find(|l| l.contains("worker event loop delay is high")).unwrap();
    assert!(line.contains("worker=2 ") && line.contains("hint="), "{line}");
    assert!(!log.contains("worker event loop delay is high worker=1 "), "{log}");
    let text = get(metrics, "/metrics").expect("metrics");
    assert!(text.contains("warden_worker_event_loop_delay_p99_seconds{app=\"loopy\",worker=\"2\"}"), "{text}");
    drop(w);

    // Node: its histogram's samples include the 100 ms period, which must
    // not show: an idle loop is ~0, a blocked one shows the block.
    if have_node() {
        for block in [None, Some(150)] {
            let port = free_port();
            let env = block.map(|ms| format!("env = {{ FIXTURE_BLOCK_MS = \"{ms}\" }}\n")).unwrap_or_default();
            let cfg = format!(
                "[app]\nname = \"loopnode\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n{env}\
                 [workers]\ncount = 1\n",
                fixture("node_app.mjs")
            );
            let w = Warden::start("loopnode", port, &cfg);
            let s = match block {
                None => w.wait_for("Node's loop delay", T, |s| delay(s, 0, "p50_ms").is_some()),
                Some(_) => w.wait_for("Node's blocked loop", T, |s| delay(s, 0, "max_ms").is_some_and(|m| m >= 40.0)),
            };
            if block.is_none() {
                assert!(delay(&s, 0, "p50_ms").unwrap() < 20.0, "{s:#}");
            }
            drop(w);
        }
    }

    // Worker mode: each Worker's own loop.
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"loopthreads\"\nentry = \"{}\"\nport = {port}\nenv = {{ FIXTURE_BLOCK_MS = \"150\", \
         FIXTURE_BLOCK_WORKER = \"2\" }}\n[workers]\ncount = 2\nmode = \"worker\"\n",
        fixture("app.ts")
    );
    let w = Warden::start("loopthreads", port, &cfg);
    let s = w.wait_for("each Worker's loop delay", T, |s| {
        delay(s, 0, "p50_ms").is_some() && delay(s, 1, "max_ms").is_some_and(|m| m >= 40.0)
    });
    assert!(delay(&s, 0, "p50_ms").unwrap() < 20.0, "Worker 1 is not blocked by Worker 2:\n{s:#}");
}

/// W5: `warden start` of an app that can't start fails fast, says why with
/// the app's own error output, and leaves it listed as errored with its
/// workers stopped (no restart loop); a later `warden start` tries again.
#[test]
fn start_fails_fast_when_every_worker_crashes() {
    let f = Fleet::new("failfast");
    let cfg = |name: &str, body: &str| std::fs::write(f.home.join(format!("{name}.toml")), body).unwrap();
    // Crashes at once until a file appears.
    cfg(
        "broken",
        &format!(
            "[app]\nname = \"broken\"\ncommand = \"sh\"\nargs = [\"-c\", \"[ -f ok ] || {{ echo 'error: cannot find \
             module express' >&2; exit 3; }}; exec sleep 300\"]\nworking_directory = \"{}\"\n[workers]\ncount = 2\n\
             min_uptime = 300\n",
            f.home.display()
        ),
    );
    let t0 = Instant::now();
    let (code, out) = f.cli(&["start", "broken"]);
    assert_eq!(code, 1, "{out}");
    assert!(t0.elapsed() < Duration::from_secs(15), "took {:?}:\n{out}", t0.elapsed());
    assert!(out.contains("broken: failed to start: all 2 workers crashed before one was ready (exit code 3)"), "{out}");
    assert_eq!(out.matches("error: cannot find module express").count(), 1, "its error, once:\n{out}");
    assert!(out.contains("hint:") && out.contains("`warden start broken` tries again"), "{out}");
    // Listed as errored, workers stopped: nothing restarts behind our back.
    let st = &f.app("broken")["status"];
    assert_eq!((st["stopped"].as_bool(), st["start_failed"].as_str()), (Some(true), Some("exit code 3")), "{st:#}");
    let restarts = |f: &Fleet| f.app("broken")["status"]["workers"][0]["restarts"].as_u64();
    let before = restarts(&f);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(restarts(&f), before, "no restart loop");
    // One app: `list` shows its detail view; the crash, not the stop, is the last exit.
    let list = f.ok(&["list"]);
    assert!(list.contains("│ status    │ errored") && list.contains("errored: its last start failed"), "{list}");
    assert!(list.lines().filter(|l| l.contains("STOPPED") && l.contains("exit code 3")).count() == 2, "{list}");
    // Warden's own log says it once, with a hint.
    let events = f.ok(&["logs", "broken", "--events", "--nostream", "-n", "200"]);
    assert_eq!(events.matches("app cannot start: every worker crashed before it was ready").count(), 1, "{events}");
    // Fixed: the next start works and the error is gone.
    std::fs::write(f.home.join("ok"), "").unwrap();
    let out = f.ok(&["start", "broken"]);
    assert!(out.contains("online (2/2 workers ready)"), "{out}");
    assert!(f.app("broken")["status"]["start_failed"].is_null());

    // An app that never listens on its port: killed at ready_timeout, then
    // the same, with a hint about the port.
    let port = free_port();
    cfg(
        "deaf",
        &format!(
            "[app]\nname = \"deaf\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 300\"]\nport = {port}\n\
             [workers]\nready_timeout = 1\n"
        ),
    );
    let (code, out) = f.cli(&["start", "deaf"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("(not ready in time)") && out.contains(&format!("sport = :{port}")), "{out}");
    assert!(out.contains("no output from it"), "{out}");
}

// ---- startup (boot and crash survival)

/// Fake systemctl, loginctl and launchctl that log their arguments to
/// `calls.log` and succeed, except for the (program, argument text, exit
/// code, error line) cases in `fail`. With exit code 0 the line is the
/// command's output instead.
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
                    let to = if *code == 0 { "" } else { " >&2" };
                    s += &format!("  *'{pat}'*) echo '{err}'{to}; exit {code};;\n");
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
#[cfg_attr(target_os = "macos", ignore = "systemd only: macOS always takes the launchd path")]
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
    // Root, as the apps ran; the runtime directory the CLI looks in, whatever User= a drop-in sets.
    assert!(!unit.lines().any(|l| l.starts_with("User=")) && unit.contains("WantedBy=multi-user.target"), "{unit}");
    let runtime = format!("Environment=\"WARDEN_RUNTIME_DIR={}\"", f.home.join("run").display());
    assert!(unit.contains(&runtime), "{unit}");
    let wardend = fakes.read("units/wardend.service");
    assert!(wardend.contains(&format!("ExecStart=\"{BIN}\" wardend\n")), "no --resurrect under systemd:\n{wardend}");
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

    // unstartup while an app still runs under its unit: disabled, but the
    // template stays (a running unit whose file is reloaded away is left half
    // configured: systemd killed it as hung every WatchdogSec on Ubuntu 24.04).
    let running = ("systemctl", "list-units", 0, "warden@api.service loaded active running Warden: api");
    let fakes = Fakes::new(&f, &[running]);
    let (code, out) = run_with(&f, &["unstartup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    for c in ["systemctl disable warden@api.service", "systemctl disable --now wardend.service"] {
        assert!(calls.lines().any(|l| l == c), "{c:?} in:\n{calls}");
    }
    assert!(f.home.join("units/warden@.service").exists(), "kept while api runs under it: {out}");
    assert!(!f.home.join("units/wardend.service").exists(), "{out}");
    assert!(out.contains("kept, because api still runs under it") && out.contains("`warden kill api`"), "{out}");

    // systemd that can't say what runs is not "nothing runs": the template stays.
    let broken = ("systemctl", "list-units", 1, "Failed to connect to bus");
    let fakes = Fakes::new(&f, &[broken]);
    let (code, out) = run_with(&f, &["unstartup", "--system"], &fakes.env());
    assert_eq!(code, 1, "{out}");
    assert!(f.home.join("units/warden@.service").exists(), "removed although the listing failed: {out}");
    assert!(out.contains("can't be told whether apps still run under it"), "{out}");

    // Once nothing runs under it: everything goes, the sysctl file too.
    let fakes = Fakes::new(&f, &[]);
    let (code, out) = run_with(&f, &["unstartup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    for c in ["systemctl disable warden@api.service", "systemctl daemon-reload"] {
        assert!(calls.lines().any(|l| l == c), "{c:?} in:\n{calls}");
    }
    assert!(!calls.contains("wardend"), "its unit is gone already, nothing to disable:\n{calls}");
    assert!(!f.home.join("units/warden@.service").exists() && !f.home.join("units/wardend.service").exists());
    assert!(!f.home.join("sysctl/99-warden.conf").exists(), "{out}");
    // Nothing left to remove: nothing to disable either, and no error.
    let (code, out) = run_with(&f, &["unstartup", "--system"], &fakes.env());
    assert_eq!(code, 0, "{out}");
    assert!(!fakes.take().contains("disable"), "{out}");

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
#[cfg_attr(target_os = "macos", ignore = "systemd only: macOS always takes the launchd path")]
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
#[cfg_attr(target_os = "macos", ignore = "systemd only: macOS always takes the launchd path")]
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
            flat.contains(&format!("<string>{BIN}</string><string>wardend</string><string>--resurrect</string>")),
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

    // unstartup: bootout, then wait until launchd no longer lists the job
    // (bootout returns while wardend is still exiting).
    let gone = ("launchctl", "print", 113, "Could not find service \"io.github.oceanwap.warden.daemon\" in domain");
    let fakes = Fakes::new(&f, &[gone]);
    let (code, out) = run_with(&f, &["unstartup", "--user"], &fakes.launchd_env());
    assert_eq!(code, 0, "{out}");
    let calls = fakes.take();
    let target = format!("gui/{uid}/io.github.oceanwap.warden.daemon");
    assert!(calls.contains(&format!("launchctl bootout {target}")), "{calls}");
    assert!(calls.contains(&format!("launchctl print {target}")), "waits for the job to go:\n{calls}");
    assert!(out.contains("unloaded; wardend stopped"), "{out}");
    assert!(!f.home.join(plist).exists(), "{out}");
}

/// `warden kill` stops wardend, and a wardend set up with `warden startup` stays stopped until
/// the next login or boot (launchd restarts it only after a crash). `warden resurrect` and
/// `warden start` must still bring it back, through launchd, not leave the window and the
/// restarts of dead supervisors without it.
#[test]
fn resurrect_starts_wardend_through_launchd_after_kill() {
    let f = Fleet::new("st-kickstart");
    sleeper_config(&f, "api");
    let fakes = Fakes::new(&f, &[]);
    let uid = unsafe { libc::getuid() };
    let target = format!("gui/{uid}/io.github.oceanwap.warden.daemon");
    std::fs::write(f.home.join("launchd/io.github.oceanwap.warden.daemon.plist"), "<plist/>").unwrap();
    // launchd's `kickstart` runs the job: here, wardend itself.
    let fake = format!(
        "#!/bin/sh\necho \"launchctl $*\" >> '{}'\ncase \"$1\" in kickstart) '{BIN}' wardend --background >/dev/null 2>&1;; esac\nexit 0\n",
        f.home.join("calls.log").display()
    );
    let path = f.home.join("fakebin/launchctl");
    std::fs::write(&path, fake).unwrap();
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let mut env = fakes.launchd_env();
    env.push(("WARDEN_NO_DAEMON".into(), "0".into()));

    let (code, out) = run_with(&f, &["start", "api"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("wardend started (launchd started {target}")), "{out}");
    assert!(!out.contains("is set up as a service"), "{out}");
    assert_eq!(run_with(&f, &["wardend", "status"], &env).0, 0, "wardend answers");

    let (code, out) = run_with(&f, &["save"], &env);
    assert_eq!(code, 0, "{out}");
    let (code, out) = run_with(&f, &["kill", "--yes"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("wardend: stopped"), "{out}");
    assert_eq!(run_with(&f, &["wardend", "status"], &env).0, 1, "kill stopped wardend");
    let _ = fakes.take();

    let (code, out) = run_with(&f, &["resurrect"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(&format!("wardend started (launchd started {target}")), "{out}");
    assert!(fakes.take().lines().any(|l| l == format!("launchctl kickstart {target}")));
    assert_eq!(run_with(&f, &["wardend", "status"], &env).0, 0, "wardend is back after resurrect");

    let _ = run_with(&f, &["kill", "--yes"], &env);
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
    assert!(out.contains("warden resurrect") && out.contains("warden wardend --resurrect"), "{out}");
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

    let d = Wardend::start_args(&f, &["wardend", "--resurrect"], &[]);
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
    let d = Wardend::start_args(&f, &["wardend", "--resurrect"], &[]);
    f.wait("second resurrect skipped", |_| d.log().contains("already resurrected this boot"));
    std::thread::sleep(Duration::from_millis(500));
    assert!(f.app("one")["status"].is_null(), "stopped app came back:\n{}", d.log());
}

#[test]
#[cfg_attr(target_os = "macos", ignore = "systemd only: macOS always takes the launchd path")]
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
    reload_holding_via(w, w.port, workers, paths)
}

/// `reload_holding`, the clients connecting to `port` (a proxy in front).
fn reload_holding_via(w: &Warden, port: u16, workers: u64, paths: &[&'static str]) -> LongLivedRun {
    rollout_holding_via(w, port, workers, paths, &["reload"])
}

/// Like `reload_holding`, through the rollout `command` runs (`reload`, `restart`).
fn rollout_holding(w: &Warden, workers: u64, paths: &[&'static str], command: &[&str]) -> LongLivedRun {
    rollout_holding_via(w, w.port, workers, paths, command)
}

/// `rollout_holding`, the clients connecting to `port`.
fn rollout_holding_via(w: &Warden, port: u16, workers: u64, paths: &[&'static str], command: &[&str]) -> LongLivedRun {
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
    let (code, out) = w.cli(command);
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
    // Not the time the host froze Warden (an overloaded CI runner), as the waits allow for it.
    let frozen = w.frozen();
    let took = run.took.saturating_sub(frozen);
    assert!(
        took < max_took,
        "reload took {:?}, {took:?} without {frozen:?} frozen (limit {max_took:?})\n{log}",
        run.took
    );
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
    // + the closing handshakes: ~3 s. Without the shim ending them:
    // grace_period (30 s) each. The limit leaves room for a CI runner that
    // stalls for seconds (seen: 5.5 s), and stays far below 30 s.
    assert_clean_handover(&w, &run, Duration::from_secs(20));
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
    assert_clean_handover(&w, &run, Duration::from_secs(20));
}

/// The drains of a rolling restart overlap (`[reload] max_draining`, 4 by
/// default): once a new worker took over, the next one is replaced while
/// the old one closes its WebSockets and SSE streams. 4 workers holding
/// long-lived clients restart in about one long_lived_timeout plus the
/// startups, not one long_lived_timeout per worker; every client still
/// gets a clean close and lands on a new worker, and `warden restart`
/// returns only once every old worker has exited.
#[test]
fn a_rolling_restart_overlaps_the_long_lived_drains() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let app = format!("args = [\"{}\"]\n[workers]\ncount = 4", fixture("longlived.ts"));
    let cfg =
        long_lived_config("ll-overlap", port, &app, "").replace("long_lived_timeout = 1", "long_lived_timeout = 2");
    let w = Warden::start("ll-overlap", port, &cfg);
    // Enough clients that every old worker holds some (SO_REUSEPORT spreads them).
    let paths: Vec<&'static str> = [["/ws"; 8], ["/sse"; 8]].concat();
    let run = rollout_holding(&w, 4, &paths, &["restart"]);
    assert_clean_handover(&w, &run, Duration::from_secs(30));
    let log = w.log();
    // Old workers drained side by side: some replacement took over while
    // other old workers were still draining (`draining=` counts them).
    let most = log
        .lines()
        .filter(|l| l.contains("worker replaced; draining old process"))
        .filter_map(|l| l.split("draining=").nth(1)?.split_whitespace().next()?.parse::<u64>().ok())
        .max()
        .unwrap_or(0);
    assert!(most >= 2, "no drains overlapped\n{log}");
    // One drain after another took at least 4 x long_lived_timeout (8 s) on
    // top of the startups; overlapping, about one (2 s) and the closing
    // handshakes. The startups are counted out (a loaded runner stretches
    // them to seconds).
    let startups: u64 = log
        .lines()
        .filter(|l| l.contains("replacement listening"))
        .filter_map(|l| l.split("startup_ms=").nth(1)?.split_whitespace().next()?.parse::<u64>().ok())
        .sum();
    let draining = run.took.saturating_sub(Duration::from_millis(startups));
    eprintln!("restart took {:?}, {startups} ms of it startups; at most {most} old workers drained at once", run.took);
    assert!(draining < Duration::from_secs(5), "restart took {:?}, {startups} ms of it startups\n{log}", run.took);
    // The CLI returned after the last old worker exited, not before.
    let s = w.status().unwrap();
    assert!(s["draining"].as_array().is_none_or(|d| d.is_empty()), "{s}");
    assert!(log.contains("every worker replaced; waiting for the old ones to finish draining"), "{log}");
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
    assert_clean_handover(&w, &run, Duration::from_secs(16));
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
    // Waiting for long_lived_timeout would add >= 2 x 3 s to the startups
    // (which a loaded CI runner stretches to seconds: count them out).
    let log = w.log();
    let startups: u64 = log
        .lines()
        .filter(|l| l.contains("replacement listening"))
        .filter_map(|l| l.split("startup_ms=").nth(1)?.split_whitespace().next()?.parse::<u64>().ok())
        .sum();
    let draining = run.took.saturating_sub(Duration::from_millis(startups));
    assert!(draining < Duration::from_secs(4), "reload took {:?}, {startups} ms of it startups\n{log}", run.took);
    assert!(!log.contains("closed long-lived connections"), "{log}");

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

// ------------------------------------------------------------ behind nginx

/// nginx run by this test in front of `app_port`, with the site file Warden
/// ships (contrib/nginx.conf): its own prefix, temp directories, logs, pid
/// file and a free port; nothing of the system's nginx is used. As root it
/// runs as nobody (setpriv), like any user. None (with the reason printed)
/// without nginx.
struct Nginx {
    child: Child,
    dir: PathBuf,
    port: u16,
}

impl Nginx {
    fn start(name: &str, app_port: u16) -> Option<Nginx> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let bin = ["/usr/sbin", "/usr/local/sbin", "/usr/local/nginx/sbin"]
            .iter()
            .map(PathBuf::from)
            .chain(std::env::split_paths(&path))
            .map(|d| d.join("nginx"))
            .find(|p| p.is_file());
        let Some(bin) = bin else {
            eprintln!("skipping: nginx is not installed (`apt-get install nginx`)");
            return None;
        };
        let dir = tmp().join(format!("warden-it-nginx-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let port = free_port();
        // The shipped file, with the test's ports, the fixture's SSE path, and
        // no IPv6 listener (a CI runner may have none).
        let site = std::fs::read_to_string(format!("{}/contrib/nginx.conf", env!("CARGO_MANIFEST_DIR")))
            .unwrap()
            .replace("server 127.0.0.1:3000 ", &format!("server 127.0.0.1:{app_port} "))
            .replace("    listen 80;\n", &format!("    listen 127.0.0.1:{port};\n"))
            .replace("    listen [::]:80;\n", "")
            .replace("location /events {", "location /sse {");
        assert_eq!(site.matches(&format!("127.0.0.1:{app_port} max_fails=0")).count(), 2, "{site}");
        assert!(site.contains(&format!("listen 127.0.0.1:{port};")) && site.contains("location /sse {"), "{site}");
        std::fs::write(dir.join("site.conf"), site).unwrap();
        let d = dir.display();
        let main = format!(
            "worker_processes 2;\npid {d}/nginx.pid;\nerror_log {d}/error.log info;\n\
             events {{ worker_connections 1024; }}\n\
             http {{\n    access_log {d}/access.log;\n    client_body_temp_path {d}/client_body;\n    \
             proxy_temp_path {d}/proxy;\n    fastcgi_temp_path {d}/fastcgi;\n    uwsgi_temp_path {d}/uwsgi;\n    \
             scgi_temp_path {d}/scgi;\n    include {d}/site.conf;\n}}\n"
        );
        std::fs::write(dir.join("nginx.conf"), main).unwrap();
        let out = std::fs::File::create(dir.join("nginx.out")).unwrap();
        let mut cmd = Command::new(&bin);
        let root = unsafe { libc::geteuid() } == 0;
        if root && std::path::Path::new("/usr/bin/setpriv").is_file() {
            std::os::unix::fs::chown(&dir, Some(65534), Some(65534)).unwrap();
            for f in ["site.conf", "nginx.conf", "nginx.out"] {
                std::os::unix::fs::chown(dir.join(f), Some(65534), Some(65534)).unwrap();
            }
            cmd = Command::new("/usr/bin/setpriv");
            cmd.args(["--reuid=65534", "--regid=65534", "--clear-groups", "--"]).arg(&bin);
        }
        let child = cmd
            .arg("-e")
            .arg(dir.join("error.log"))
            .arg("-p")
            .arg(&dir)
            .arg("-c")
            .arg(dir.join("nginx.conf"))
            .args(["-g", "daemon off;"])
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone().unwrap()))
            .stderr(out)
            .spawn()
            .unwrap();
        let mut n = Nginx { child, dir, port };
        let t0 = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(st)) = n.child.try_wait() {
                panic!("nginx exited ({st}):\n{}", n.logs());
            }
            assert!(t0.elapsed() < T, "nginx does not listen:\n{}", n.logs());
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(n)
    }

    fn logs(&self) -> String {
        let read = |f: &str| std::fs::read_to_string(self.dir.join(f)).unwrap_or_default();
        format!("{}{}", read("nginx.out"), read("error.log"))
    }
}

impl Drop for Nginx {
    fn drop(&mut self) {
        // SIGTERM: a fast shutdown of the master and its workers.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let t0 = Instant::now();
        while self.child.try_wait().ok().flatten().is_none() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A rolling restart under load through a real nginx with contrib/nginx.conf:
/// not one request fails (fresh connections, keep-alive GETs, keep-alive
/// POSTs), whatever net.ipv4.tcp_migrate_req says for the GETs (nginx
/// retries an idempotent request a closing worker's listener reset, over a
/// new connection). A POST nginx sends on a new upstream connection that
/// lands in that listener's queue is reset too and never retried: none with
/// tcp_migrate_req=1 (docs/proxies.md), else only such resets, as in nginx's
/// log. A WebSocket and an SSE stream held through it are ended
/// cleanly by their old worker, through nginx, and reconnect to new ones.
#[test]
fn rolling_restart_through_nginx_drops_nothing() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let app = format!("args = [\"{}\"]\n[workers]\ncount = 3", fixture("longlived.ts"));
    let w = Warden::start("nginx", port, &long_lived_config("nginx", port, &app, ""));
    let Some(ngx) = Nginx::start("restart", port) else { return };
    w.wait_for("ready", T, ready(3));
    assert!(get(ngx.port, "/health").is_some(), "the health location answers:\n{}", ngx.logs());

    let post = "POST /orders HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\n\
                Content-Length: 13\r\n\r\n{\"item\":\"42\"}";
    let ((run, ok, cut, failed), posted, post_cut, post_failed) =
        keep_alive_requests(ngx.port, post.to_string(), || {
            keep_alive_through(ngx.port, "/whoami", || reload_holding_via(&w, ngx.port, 3, &["/ws", "/sse"]))
        });
    let log = ngx.logs();
    let retried = log.lines().filter(|l| l.contains("[error]") && l.contains("upstream")).count();
    eprintln!(
        "through nginx: fresh {} ok / {} failed; keep-alive GET {ok} ok, {cut} cut, {failed} failed; \
         POST {posted} ok, {post_cut} cut, {post_failed} failed; {retried} upstream errors nginx retried",
        run.ok, run.fail
    );
    assert_eq!(run.fail, 0, "requests on fresh connections failed through nginx\n{log}");
    assert_eq!((cut, failed), (0, 0), "keep-alive GETs failed through nginx\n{log}");
    let post_resets = log
        .lines()
        .filter(|l| l.contains("Connection reset by peer") && l.contains("upstream") && l.contains("\"POST "))
        .count();
    let allowed = if allowed_resets() == 0 { 0 } else { post_resets.min(allowed_resets()) };
    assert!(
        post_cut + post_failed <= allowed,
        "POSTs failed through nginx ({post_cut} cut, {post_failed} failed)\n{log}"
    );
    assert!(ok > 50 && posted > 50, "{ok} GETs, {posted} POSTs");
    // The WebSocket and the SSE stream: closed 1001 / ended cleanly by an old
    // worker (through nginx), reconnected to a new worker.
    assert_clean_handover(&w, &run, Duration::from_secs(30));
    assert!(!log.contains("no live upstreams"), "the port was never taken out of the upstream\n{log}");
}

// ------------------------------------------------------------ surge rollouts

/// Requests on fresh connections from one client thread while `f` runs:
/// (what `f` returned, requests that succeeded, requests that failed).
fn under_load<R>(port: u16, f: impl FnOnce() -> R) -> (R, usize, usize) {
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
    let r = f();
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    (r, ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed))
}

/// A keep-alive client (a request every 10 ms on one connection, a new one
/// after `Connection: close`) while `f` runs: (what `f` returned, requests
/// answered, requests cut on a connection that had served earlier ones,
/// requests failed on a new connection).
fn keep_alive_through<R>(port: u16, path: &'static str, f: impl FnOnce() -> R) -> (R, usize, usize, usize) {
    keep_alive_requests(port, format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"), f)
}

/// `keep_alive_through` sending `request` (a whole HTTP/1.1 request; the
/// answer must be a 200 with a Content-Length).
fn keep_alive_requests<R>(port: u16, request: String, f: impl FnOnce() -> R) -> (R, usize, usize, usize) {
    let stop = Arc::new(AtomicBool::new(false));
    let counts = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0), AtomicUsize::new(0)]);
    let client = {
        let (stop, counts) = (stop.clone(), counts.clone());
        std::thread::spawn(move || {
            // One response off `s`: Some(the server asked to close).
            let exchange = |s: &mut TcpStream, buf: &mut Vec<u8>| -> Option<bool> {
                s.write_all(request.as_bytes()).ok()?;
                let mut tmp = [0u8; 8192];
                let end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    let n = s.read(&mut tmp).ok().filter(|n| *n > 0)?;
                    buf.extend_from_slice(&tmp[..n]);
                };
                let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                if !head.starts_with("http/1.1 200") {
                    return None;
                }
                let len: usize =
                    head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok())?;
                while buf.len() < end + len {
                    let n = s.read(&mut tmp).ok().filter(|n| *n > 0)?;
                    buf.extend_from_slice(&tmp[..n]);
                }
                buf.drain(..end + len);
                Some(head.contains("connection: close"))
            };
            let mut conn: Option<(TcpStream, Vec<u8>)> = None;
            while !stop.load(Ordering::Relaxed) {
                let fresh = conn.is_none();
                if fresh {
                    match TcpStream::connect(("127.0.0.1", port)) {
                        Ok(s) => {
                            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                            conn = Some((s, Vec::new()));
                        }
                        Err(_) => {
                            counts[2].fetch_add(1, Ordering::Relaxed);
                            std::thread::sleep(Duration::from_millis(10));
                            continue;
                        }
                    }
                }
                let Some((s, buf)) = conn.as_mut() else { continue };
                match exchange(s, buf) {
                    Some(close) => {
                        counts[0].fetch_add(1, Ordering::Relaxed);
                        if close {
                            conn = None;
                        }
                    }
                    None => {
                        counts[if fresh { 2 } else { 1 }].fetch_add(1, Ordering::Relaxed);
                        conn = None;
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    std::thread::sleep(Duration::from_millis(300));
    let r = f();
    std::thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    let n = |i: usize| counts[i].load(Ordering::Relaxed);
    (r, n(0), n(1), n(2))
}

/// Found by `cargo xtask chaos`: a draining static worker closed its idle
/// keep-alive connections at once, so a client sending its next request at
/// that moment lost it. Now requests during the drain get `Connection: close`.
#[test]
fn static_drain_answers_keep_alive_requests_instead_of_cutting_them() {
    {
        let io = "epoll";
        let dir = tmp().join(format!("warden-it-static-drain-{io}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<h1>home</h1>").unwrap();
        let port = free_port();
        let cfg = format!(
            "[app]\nname = \"sdrain-{io}\"\nport = {port}\n[workers]\ncount = 2\n[static]\nroot = \"{}\"\n",
            dir.display()
        );
        let w = Warden::start_env(&format!("sdrain-{io}"), port, &cfg, &[]);
        w.wait_for("ready", T, ready(2));
        let ((), ok, cut, fresh) = keep_alive_through(port, "/index.html", || {
            for _ in 0..3 {
                let (code, out) = w.cli(&["reload"]);
                assert_eq!(code, 0, "{out}");
            }
        });
        eprintln!("{io}: {ok} answered, {cut} cut, {fresh} failed on a new connection");
        assert!(ok > 50, "{io}: {ok} answered");
        assert_eq!(cut, 0, "{io}: keep-alive requests cut by a draining worker\n{}", w.log());
        assert!(fresh <= 2 * allowed_resets(), "{io}: {fresh} failed on a new connection");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Found by `cargo xtask chaos`: on Node, the shim's drain closed idle
/// keep-alive connections at once (http.Server#close() does it on Node 19+,
/// and so did its own sweep), so a request being sent on one was lost.
#[test]
fn node_drain_answers_keep_alive_requests_instead_of_cutting_them() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"ndrain\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n",
        fixture("node_app.mjs")
    );
    let w = Warden::start("ndrain", port, &cfg);
    w.wait_for("ready", T, ready(2));
    let ((), ok, cut, fresh) = keep_alive_through(port, "/whoami", || {
        for _ in 0..3 {
            let (code, out) = w.cli(&["reload"]);
            assert_eq!(code, 0, "{out}");
        }
    });
    eprintln!("node: {ok} answered, {cut} cut, {fresh} failed on a new connection");
    assert!(ok > 50, "{ok} answered");
    assert_eq!(cut, 0, "keep-alive requests cut by a draining worker\n{}", w.log());
    assert!(fresh <= 2 * allowed_resets(), "{fresh} failed on a new connection");
}

/// Bun.serve apps: outside a drain the shim adds nothing to a request (the
/// app's own fetch handler runs); a drain swaps in, with server.reload(), one
/// that adds `Connection: close`, so a keep-alive client moves to the new
/// workers instead of being cut when the old one exits.
#[test]
fn bun_drain_answers_keep_alive_requests_instead_of_cutting_them() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg =
        format!("[app]\nname = \"bdrain\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\n", fixture("app.ts"));
    let w = Warden::start("bdrain", port, &cfg);
    w.wait_for("ready", T, ready(2));
    let ((), ok, cut, fresh) = keep_alive_through(port, "/whoami", || {
        for _ in 0..3 {
            let (code, out) = w.cli(&["reload"]);
            assert_eq!(code, 0, "{out}");
        }
    });
    eprintln!("bun: {ok} answered, {cut} cut, {fresh} failed on a new connection");
    assert!(ok > 50, "{ok} answered");
    assert_eq!(cut, 0, "keep-alive requests cut by a draining worker\n{}", w.log());
    assert!(fresh <= 2 * allowed_resets(), "{fresh} failed on a new connection");
}

/// An app that swaps its own handler (server.reload) keeps it through a
/// drain: the drain's `Connection: close` handler wraps the handler the app
/// has now, never the one it started with.
#[test]
fn bun_drain_keeps_the_handler_the_app_reloaded() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"breload\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 1\n\
         [shutdown]\ndrain_ms = 4000\ngrace_period = 15\n",
        fixture("app.ts")
    );
    let w = Warden::start("breload", port, &cfg);
    w.wait_for("ready", T, ready(1));
    // One keep-alive connection to the (only) worker, which swaps its handler.
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut buf = Vec::new();
    let mut exchange = |s: &mut TcpStream, path: &str| -> (String, String) {
        write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut tmp = [0u8; 4096];
        let end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i + 4;
            }
            let n = s.read(&mut tmp).unwrap();
            assert!(n > 0, "connection closed before a response to {path}");
            buf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let len: usize =
            head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap();
        while buf.len() < end + len {
            let n = s.read(&mut tmp).unwrap();
            assert!(n > 0, "connection closed in the body of {path}");
            buf.extend_from_slice(&tmp[..n]);
        }
        let body = String::from_utf8_lossy(&buf[end..end + len]).to_string();
        buf.drain(..end + len);
        (head, body)
    };
    let (_, who) = exchange(&mut s, "/reload-v2");
    let (_, body) = exchange(&mut s, "/whoami");
    assert_eq!(body, format!("v2 {who}"), "the app's own reload took effect");
    // A reload: the new worker starts, then the old one drains (4 s), with
    // our connection still open to it.
    let reload = {
        let cfg = w.cfg.clone();
        std::thread::spawn(move || warden().args(["reload", "-c"]).arg(&cfg).output().unwrap())
    };
    // The new worker listens, Warden signals the old one, and the old one
    // starts its drain (4 s).
    w.wait_log("draining old process", T);
    wait_bun_drain_started(&w, port, "/whoami");
    let (head, body) = exchange(&mut s, "/whoami");
    assert_eq!(body, format!("v2 {who}"), "the drain brought back the handler the app replaced");
    assert!(head.contains("connection: close"), "no Connection: close in the drain: {head}");
    let out = reload.join().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Wait until the old Bun worker's drain has started. Bun 1.3: it stops
/// accepting then (one listener left). From Bun 1.4, where stopping closes
/// idle keep-alive connections, it accepts until the end of its drain window
/// and answers with `Connection: close`: a new connection that gets that
/// header reached it in its drain.
fn wait_bun_drain_started(w: &Warden, port: u16, path: &str) {
    let t0 = Instant::now();
    while listeners(port) != 1 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", port)) {
            s.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let _ = write!(s, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap_or(0);
            if String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase().contains("connection: close") {
                return;
            }
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "the old worker never started draining\n{}", w.log());
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Bun `routes` (functions, a static Response, per-method handlers) and the
/// `error` handler answer with `Connection: close` once a drain has started,
/// like `fetch` does: a keep-alive client must not be left on the old worker.
#[test]
fn bun_drain_closes_connections_answered_by_routes_and_error_handlers() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"broutes\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 1\n\
         [shutdown]\ndrain_ms = 4000\ngrace_period = 15\n",
        fixture("bun_routes.ts")
    );
    let w = Warden::start("broutes", port, &cfg);
    w.wait_for("ready", T, ready(1));
    let paths = ["/r", "/api/7", "/static", "/m", "/async", "/throw", "/nothing-here"];
    // One keep-alive connection per path, each used once before the drain
    // (answered normally) and once during it.
    let mut socks: Vec<TcpStream> = paths
        .iter()
        .map(|p| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            write!(s, "GET {p} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
            let mut buf = [0u8; 4096];
            let n = s.read(&mut buf).unwrap();
            let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
            assert!(!head.contains("connection: close"), "{p}: closed before any drain: {head}");
            s
        })
        .collect();
    let reload = {
        let cfg = w.cfg.clone();
        std::thread::spawn(move || warden().args(["reload", "-c"]).arg(&cfg).output().unwrap())
    };
    w.wait_log("draining old process", T);
    wait_bun_drain_started(&w, port, "/r");
    for (p, s) in paths.iter().zip(socks.iter_mut()) {
        write!(s, "GET {p} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut buf = [0u8; 4096];
        let n = s.read(&mut buf).unwrap_or(0);
        let head = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
        assert!(head.starts_with("http/1.1 "), "{p}: no answer in the drain: {head:?}\n{}", w.log());
        assert!(head.contains("connection: close"), "{p}: no Connection: close in the drain: {head}");
    }
    let out = reload.join().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

/// Node: a request in flight holds the drain until it is answered (the shim
/// reads the requests in flight off the open connections; it adds nothing
/// per request).
#[test]
fn node_drain_waits_for_requests_in_flight() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"nslow\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 1\n\
         [shutdown]\ndrain_ms = 100\ngrace_period = 15\n",
        fixture("node_app.mjs")
    );
    let w = Warden::start("nslow", port, &cfg);
    w.wait_for("ready", T, ready(1));
    let slow = std::thread::spawn(move || {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
        write!(s, "GET /slow?ms=2500 HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
        let mut out = String::new();
        let r = s.read_to_string(&mut out);
        (r.map_err(|e| e.to_string()), out)
    });
    std::thread::sleep(Duration::from_millis(300));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let (r, body) = slow.join().unwrap();
    assert!(
        r.is_ok() && body.starts_with("HTTP/1.1 200"),
        "slow request cut by the drain: {r:?} {body:?}\n{}",
        w.log()
    );
}

/// Node over TLS: the connection the shim tracks is the TLS socket, not the
/// raw TCP one (the HTTP state lives on it). On the raw one nothing was ever
/// in flight and a drain cut every slow request.
#[test]
fn node_https_drain_waits_for_requests_in_flight() {
    if !have_node() {
        return;
    }
    let tls = tmp().join(format!("warden-it-tls-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tls);
    std::fs::create_dir_all(&tls).unwrap();
    let made = Command::new("openssl")
        .args(["req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2", "-subj", "/CN=localhost"])
        .arg("-keyout")
        .arg(tls.join("key.pem"))
        .arg("-out")
        .arg(tls.join("cert.pem"))
        .output();
    if !made.as_ref().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no working openssl to make a test certificate ({made:?})");
        return;
    }
    if !Command::new("curl").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no curl");
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"nhttps\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n\
         env = {{ FIXTURE_TLS_DIR = \"{}\" }}\n[workers]\ncount = 1\n\
         [shutdown]\ndrain_ms = 100\ngrace_period = 15\n",
        fixture("node_app.mjs"),
        tls.display()
    );
    let w = Warden::start("nhttps", port, &cfg);
    w.wait_for("ready", T, ready(1));
    let url = format!("https://127.0.0.1:{port}/slow?ms=2500");
    let slow: Vec<_> = (0..4)
        .map(|_| {
            let url = url.clone();
            std::thread::spawn(move || {
                Command::new("curl").args(["-sk", "--max-time", "20", "-w", " %{http_code}", &url]).output().unwrap()
            })
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    for h in slow {
        let o = h.join().unwrap();
        let body = String::from_utf8_lossy(&o.stdout).to_string();
        assert!(
            o.status.success() && body.ends_with(" 200"),
            "a slow https request was cut by the drain: curl {:?} {body:?}\n{}",
            o.status.code(),
            w.log()
        );
    }
    let _ = std::fs::remove_dir_all(&tls);
}

/// None of the requests sent during a rolling replacement failed. Without
/// net.ipv4.tcp_migrate_req=1 the kernel resets connections queued on a
/// listener that closes, whatever the order of replacement (one at a time
/// too): then up to 1% may fail, as in `process_mode_lifecycle`.
fn no_dropped_requests(ok: usize, fail: usize) {
    if allowed_resets() == 0 {
        assert_eq!(fail, 0, "{fail} requests failed during the rollout ({ok} ok)");
    } else {
        assert!(fail * 100 <= ok, "{fail} requests failed during the rollout ({ok} ok)");
    }
}

/// `warden reload`, waited for: its duration and the CLI's output.
fn timed_reload(w: &Warden) -> (f64, String) {
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}\n{}", w.log());
    let s = w.status().unwrap();
    (s["last_rollout"]["duration_secs"].as_f64().unwrap(), out)
}

/// The `workers=` of each "starting new workers next to the old ones" log line.
fn batch_lines(w: &Warden) -> Vec<String> {
    w.log()
        .lines()
        .filter(|l| l.contains("starting new workers next to the old ones"))
        .filter_map(|l| l.split(" workers=").nth(1))
        .map(|rest| match rest.strip_prefix('"') {
            Some(q) => q.split('"').next().unwrap_or("").to_string(),
            None => rest.split(' ').next().unwrap_or("").to_string(),
        })
        .collect()
}

#[test]
fn surge_replaces_workers_in_batches_without_dropping_requests() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("surge", port, &gated("surge", port, 4, ""));
    let mut before = pid_set(&w.wait_for("ready", T, ready(4)));
    let good = std::fs::read_to_string(&w.cfg).unwrap();

    // One at a time (the default)...
    let ((one, out), ok, fail) = under_load(port, || timed_reload(&w));
    eprintln!("surge 1: {one}s, {ok} ok, {fail} failed");
    assert!(out.contains("reload complete: 4 worker(s) replaced") && !out.contains("batch"), "{out}");
    no_dropped_requests(ok, fail);
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before));
    before = pid_set(&s);
    assert!(batch_lines(&w).is_empty());

    // ...then two at a time: 2 batches, faster, nothing dropped.
    std::fs::write(&w.cfg, good.replace("[reload]\n", "[reload]\nsurge = 2\n")).unwrap();
    let ((two, out), ok, fail) = under_load(port, || timed_reload(&w));
    eprintln!("surge 2: {two}s, {ok} ok, {fail} failed");
    assert!(out.contains("4 worker(s) replaced") && out.contains("(2 batches of up to 2)"), "{out}");
    // Phases name the batch, and `done` counts the workers it finished.
    assert!(out.contains("[0/4] workers 1, 2: ") && out.contains("[2/4] workers 3, 4: "), "{out}");
    no_dropped_requests(ok, fail);
    assert!(ok > 20, "{ok}");
    assert_eq!(batch_lines(&w), ["1, 2", "3, 4"], "{}", w.log());
    // Faster on a quiet machine (1.7 vs 2.3 s); a loaded CI runner can stretch
    // one startup by seconds, so only "not slower" is checked here (the
    // benchmark measures the gain).
    assert!(two < one + 1.0, "surge 2 ({two}s) slower than one at a time ({one}s)");
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before), "every worker must be new");
    assert_eq!(s["workers_ready"], 4);
    assert!(w.log().contains("reload started workers=4 seq=2 surge=2"), "{}", w.log());
    before = pid_set(&s);

    // "all": every new worker at once, one batch.
    std::fs::write(&w.cfg, good.replace("[reload]\n", "[reload]\nsurge = \"all\"\n")).unwrap();
    let ((all, out), ok, fail) = under_load(port, || timed_reload(&w));
    eprintln!("surge all: {all}s, {ok} ok, {fail} failed");
    assert!(out.contains("4 worker(s) replaced") && out.contains("(1 batch of up to 4)"), "{out}");
    no_dropped_requests(ok, fail);
    assert_eq!(batch_lines(&w), ["1, 2", "3, 4", "1-4"]);
    let s = w.status().unwrap();
    assert!(pid_set(&s).is_disjoint(&before));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listeners(port), 4, "the old workers are gone, the new ones serve");

    // A rolling `restart` uses the same batches.
    let (code, out) = w.cli(&["restart"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("restart complete") && out.contains("(1 batch of up to 4)"), "{out}");
}

#[test]
fn surge_failure_rolls_back_the_whole_batch() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = gated("surgefail", port, 4, "").replace("[reload]\n", "[reload]\nsurge = 2\n");
    let w = Warden::start("surgefail", port, &cfg);
    let before = pid_set(&w.wait_for("ready", T, ready(4)));
    let good = std::fs::read_to_string(&w.cfg).unwrap();
    let with = |extra: &str| good.replace("[reload]\n", &format!("[reload]\n{extra}\n"));

    // Worker 2's new process fails its check; worker 1's passed, and is stopped too.
    std::fs::write(&w.cfg, with("verify_command = \"test $WARDEN_WORKER_ID != 2\"")).unwrap();
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("reload failed at worker 2: verify_command failed"), "{out}");
    assert!(out.contains("Rolled back: the 2 new workers started together were stopped"), "{out}");
    let s = w.status().unwrap();
    assert_eq!(pid_set(&s), before, "the old workers keep serving");
    assert_eq!(s["workers_ready"], 4);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listeners(port), 4, "no new worker left behind");

    // safe-reload: a failing canary rolls back before any batch starts.
    std::fs::write(&w.cfg, good.replace("[workers]", "env = { FIXTURE_HEALTH_FAIL = \"1\" }\n[workers]")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("safe-reload failed at worker 1") && out.contains("Rolled back"), "{out}");
    assert_eq!(pid_set(&w.status().unwrap()), before);

    // The canary passes, then the next batch fails at worker 3: the canary
    // stays, both new workers of that batch are stopped.
    std::fs::write(&w.cfg, with("verify_command = \"test $WARDEN_WORKER_ID != 3\"")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("safe-reload halted at worker 3") && out.contains("1/4 workers were replaced"), "{out}");
    let s = w.status().unwrap();
    let pid = |i: usize| s["workers"][i]["pid"].as_u64().unwrap();
    assert!(!before.contains(&pid(0)), "the canary took over worker 1");
    assert!((1..4).all(|i| before.contains(&pid(i))), "workers 2-4 still run the old version: {s:#?}");
    assert_eq!(batch_lines(&w).last().map(String::as_str), Some("2, 3"), "{}", w.log());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(listeners(port), 4);
}

// ------------------------------------------------------------ release pinning

/// releases/v1..v3 (each a copy of the release fixture) and `current` -> v1.
fn release_tree(name: &str) -> PathBuf {
    let dir = tmp().join(format!("warden-it-rel-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for v in ["v1", "v2", "v3"] {
        std::fs::create_dir_all(dir.join("releases").join(v)).unwrap();
        std::fs::copy(fixture("release_app.ts"), dir.join("releases").join(v).join("app.ts")).unwrap();
    }
    std::os::unix::fs::symlink(dir.join("releases/v1"), dir.join("current")).unwrap();
    dir
}

/// Point `current` at another release, atomically (as deploy tools do).
fn swap_current(dir: &std::path::Path, to: &str) {
    let tmp = dir.join("current.new");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(dir.join("releases").join(to), &tmp).unwrap();
    std::fs::rename(&tmp, dir.join("current")).unwrap();
}

/// The releases the workers answer from (their cwd). `script`: also check
/// that the script runs from the same release ("(mixed)" when it doesn't).
fn releases_seen(port: u16, script: bool) -> std::collections::BTreeSet<String> {
    let rel = |s: &str| s.split("releases/").nth(1).and_then(|r| r.split('/').next()).unwrap_or("?").to_string();
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..40 {
        let Some(w) = get(port, "/where") else { continue };
        let Some((cwd, path)) = w.split_once('|') else { continue };
        let mixed = script && rel(cwd) != rel(path);
        seen.insert(format!("{}{}", rel(cwd), if mixed { "(mixed)" } else { "" }));
    }
    seen
}

fn release_config(name: &str, port: u16, dir: &std::path::Path, extra: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nargs = [\"{}\"]\nworking_directory = \"{}\"\nport = {port}\n{extra}\n\
         [workers]\ncount = 2\n[restart]\nbackoff_initial = 50\n[shutdown]\ndrain_ms = 100\n",
        dir.join("current/app.ts").display(),
        dir.join("current").display(),
    )
}

/// kill -9 worker `idx` (0-based) and wait until it runs again; its old pid.
fn kill_worker(w: &Warden, idx: usize) -> u64 {
    let pid = w.status().unwrap()["workers"][idx]["pid"].as_u64().unwrap();
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    w.wait_for("worker restarted", T, |s| {
        let x = &s["workers"][idx];
        x["state"] == "RUNNING" && x["pid"].as_u64().is_some_and(|p| p != pid)
    });
    pid
}

#[test]
fn pinned_release_survives_a_symlink_swap_until_reload() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let dir = release_tree("pin");
    let w = Warden::start("pin", port, &release_config("pin", port, &dir, ""));
    let s = w.wait_for("ready", T, ready(2));
    let rel = |s: &Value| s["release"].as_str().unwrap_or("").to_string();
    assert!(rel(&s).ends_with("releases/v1"), "{s:#?}");
    assert_eq!(releases_seen(port, true), ["v1".to_string()].into(), "cwd and script both in the pinned release");
    assert!(w.log().contains("release pinned"));
    let (_, out) = w.cli(&["status"]);
    assert!(out.contains("│ release ") && out.contains("releases/v1"), "{out}");

    // Deploy, step 1: the symlink is swapped, and a worker crashes before the
    // reload. It comes back on the release the others run.
    swap_current(&dir, "v2");
    kill_worker(&w, 0);
    let s = w.status().unwrap();
    assert_eq!(s["workers"][0]["last_exit"], "killed by another process (SIGKILL)");
    assert!(rel(&s).ends_with("releases/v1"), "{s:#?}");
    assert_eq!(releases_seen(port, true), ["v1".to_string()].into(), "no mixed versions after a crash");
    let log = w.wait_log("something outside Warden sent SIGKILL", T);
    assert!(log.contains("worker crashed") && log.contains("killed by another process (SIGKILL)"), "{log}");

    // Step 2: the reload moves every worker to the new release.
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.status().unwrap();
    assert!(rel(&s).ends_with("releases/v2"), "{s:#?}");
    assert_eq!(releases_seen(port, true), ["v2".to_string()].into());
    let log = w.log();
    let started = log.lines().find(|l| l.contains("reload started")).unwrap_or_default();
    assert!(started.contains("release=") && started.contains("releases/v2"), "{started}");
    let (_, out) = w.cli(&["describe"]);
    assert!(out.contains("releases/v2 (pinned"), "{out}");

    // Old releases cleaned up while still pinned: the next crash restart
    // can't use v2 any more; it falls back to `current` (v3) and says so.
    swap_current(&dir, "v3");
    std::fs::remove_dir_all(dir.join("releases/v2")).unwrap();
    kill_worker(&w, 1);
    let log = w.wait_log("the pinned release directory is gone", T);
    assert!(log.contains("reload has moved every worker off them"), "{log}");
    let s = w.status().unwrap();
    assert!(rel(&s).ends_with("releases/v3"), "{s:#?}");
    let t0 = Instant::now();
    while !releases_seen(port, true).contains("v3") {
        assert!(t0.elapsed() < T, "the restarted worker should run v3");
    }

    // A failed rollout keeps the pin it had.
    swap_current(&dir, "v1");
    let cfg = std::fs::read_to_string(&w.cfg).unwrap();
    std::fs::write(&w.cfg, cfg.replace("[workers]", "[reload]\nverify_command = \"false\"\n[workers]")).unwrap();
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(rel(&w.status().unwrap()).ends_with("releases/v3"), "the failed reload's pin must be dropped");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_pinning_a_crash_restart_picks_up_the_new_release() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let dir = release_tree("nopin");
    let w = Warden::start("nopin", port, &release_config("nopin", port, &dir, "pin_release = false"));
    let s = w.wait_for("ready", T, ready(2));
    assert!(s["release"].is_null(), "{s:#?}");
    swap_current(&dir, "v2");
    kill_worker(&w, 0);
    // Today's behaviour without a pin, and why it is on by default: two
    // versions serve side by side.
    let t0 = Instant::now();
    while releases_seen(port, false) != ["v1".to_string(), "v2".to_string()].into() {
        assert!(t0.elapsed() < T, "expected v1 and v2 side by side");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// --------------------------------------------------------------- exit reasons

/// A plain program that kills itself with `sig` after a moment.
fn self_killer(name: &str, sig: &str) -> Warden {
    Warden::start(
        name,
        0,
        &format!(
            "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"sleep 0.3; kill -{sig} $$\"]\n\
             [workers]\nmin_uptime = 100\n[restart]\nbackoff_initial = 2000\nbackoff_max = 2000\n"
        ),
    )
}

#[test]
fn exit_reasons_name_crashes_and_who_stopped_a_worker() {
    // A segfault and an abort, as the kernel reports them.
    let w = self_killer("segv", "SEGV");
    let s = w.wait_for("a crash", T, |s| s["workers"][0]["crashes"].as_u64() >= Some(1));
    assert_eq!(s["workers"][0]["last_exit"], "crashed: SIGSEGV (segmentation fault)");
    let log = w.wait_log("coredumpctl", T);
    assert!(log.contains("worker crashed") && log.contains("reason=\"crashed: SIGSEGV"), "{log}");
    let w = self_killer("abrt", "ABRT");
    let s = w.wait_for("a crash", T, |s| s["workers"][0]["crashes"].as_u64() >= Some(1));
    assert_eq!(s["workers"][0]["last_exit"], "crashed: SIGABRT (aborted)");
    w.wait_log("aborted itself", T);
    if !have_bun() {
        return;
    }

    // Bun's own process.abort(), classified the same way once it dies. On
    // CI runners Bun's crash handler keeps the aborting process alive for
    // longer than this test waits (still RUNNING, using CPU, after 19 s,
    // even without core dumps), so there it is a note, not a failure: the
    // classification itself is covered by the `sh` abort above.
    let w = Warden::start(
        "bunabort",
        0,
        "[app]\nname = \"bunabort\"\ncommand = \"sh\"\n\
         args = [\"-c\", \"ulimit -c 0; exec bun -e 'setTimeout(() => process.abort(), 300)'\"]\n\
         [workers]\nmin_uptime = 100\n[restart]\nbackoff_initial = 2000\nbackoff_max = 2000\n",
    );
    let t0 = Instant::now();
    loop {
        let s = w.status();
        let w0 = s.as_ref().map(|s| s["workers"][0].clone()).unwrap_or_default();
        if w0["crashes"].as_u64() >= Some(1) {
            assert_eq!(w0["last_exit"], "crashed: SIGABRT (aborted)");
            break;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            eprintln!("note: Bun's process.abort() did not end the process within 10 s here; not checked");
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // `warden stop`: an exit after Warden's stop signal...
    let port = free_port();
    let w = Warden::start("stopsig", port, &simple("stopsig", port, 1, ""));
    w.wait_for("ready", T, ready(1));
    assert_eq!(w.cli(&["stop"]).0, 0);
    let s = w.wait_for("stopped", T, |s| s["workers"][0]["state"] == "STOPPED");
    let why = s["workers"][0]["last_exit"].as_str().unwrap();
    assert!(why == "exit code 0 after Warden's SIGTERM" || why == "stopped by Warden's SIGTERM", "{why}");

    // ...and Warden's SIGKILL once the grace period is over.
    let port = free_port();
    let w = Warden::start(
        "gracekill",
        port,
        &format!(
            "[app]\nname = \"gracekill\"\nargs = [\"{}\"]\nport = {port}\nshim = false\n\
             env = {{ FIXTURE_IGNORE_TERM = \"1\" }}\n[shutdown]\ngrace_period = 1\n",
            fixture("app.ts")
        ),
    );
    w.wait_for("ready", T, ready(1));
    assert_eq!(w.cli(&["stop"]).0, 0);
    let s = w.wait_for("stopped", T, |s| s["workers"][0]["state"] == "STOPPED");
    assert_eq!(s["workers"][0]["last_exit"], "killed by Warden (SIGKILL)");
}

/// OOM kills are told apart by the cgroup's oom_kill counter. A fake
/// `memory.events` (debug builds only) stands in for the kernel's here.
#[test]
fn oom_kill_is_told_apart_from_a_kill_9() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let events = tmp().join(format!("warden-it-memory.events-{}", std::process::id()));
    std::fs::write(&events, "low 0\nhigh 0\nmax 5\noom 2\noom_kill 3\noom_group_kill 0\n").unwrap();
    let ev = events.display().to_string();
    let w = Warden::start_env("oomfake", port, &simple("oomfake", port, 1, ""), &[("WARDEN_TEST_MEMORY_EVENTS", &ev)]);
    w.wait_for("ready", T, ready(1));
    std::fs::write(&events, "low 0\nhigh 0\nmax 9\noom 3\noom_kill 4\noom_group_kill 0\n").unwrap();
    kill_worker(&w, 0);
    let s = w.status().unwrap();
    assert_eq!(s["workers"][0]["last_exit"], "killed by the kernel OOM killer (out of memory)");
    let log = w.wait_log("raise memory.max / MemoryMax= or lower `[limits] max_memory`", T);
    assert!(log.contains("worker crashed") && log.contains("OOM killer"), "{log}");
    // The counter didn't move this time: someone's kill -9.
    kill_worker(&w, 0);
    let s = w.status().unwrap();
    assert_eq!(s["workers"][0]["last_exit"], "killed by another process (SIGKILL)");
    w.wait_log("Probably not the kernel's OOM killer", T);
    // It moved, but 3 s before the kill -9 (a helper process the app ran,
    // OOM-killed long ago): not this worker's death.
    std::fs::write(&events, "low 0\nhigh 0\nmax 9\noom 4\noom_kill 5\noom_group_kill 0\n").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    kill_worker(&w, 0);
    let s = w.status().unwrap();
    assert_eq!(s["workers"][0]["last_exit"], "killed by another process (SIGKILL)", "{}", w.log());
    let _ = std::fs::remove_file(&events);
}

/// One OOM kill counted while two workers of the same cgroup die of SIGKILL
/// at the same moment (the OOM killer took one, someone's kill -9 the
/// other): the shared count can't say which, so neither is called an OOM
/// kill for sure, nor "probably not the OOM killer"; both reasons say so.
#[test]
fn one_oom_kill_for_two_sigkill_deaths_is_reported_as_uncertain() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let events = tmp().join(format!("warden-it-memory.events-two-{}", std::process::id()));
    std::fs::write(&events, "low 0\nhigh 0\nmax 5\noom 2\noom_kill 3\noom_group_kill 0\n").unwrap();
    let ev = events.display().to_string();
    let w = Warden::start_env("oomtwo", port, &simple("oomtwo", port, 2, ""), &[("WARDEN_TEST_MEMORY_EVENTS", &ev)]);
    let s = w.wait_for("ready", T, ready(2));
    let pids: Vec<u64> = (0..2).map(|i| s["workers"][i]["pid"].as_u64().unwrap()).collect();
    // "At the same moment" has to be made true, not hoped for: a Warden that
    // runs between the two kill(2) calls handles the first death alone, and
    // is right to call it a certain OOM kill (the second, later, is then
    // someone else's kill -9). So Warden is frozen while the counter moves
    // and both workers get their SIGKILL, and sees both deaths at once when
    // it thaws, however loaded the machine and however far apart the calls.
    {
        let _frozen = w.freeze();
        std::fs::write(&events, "low 0\nhigh 0\nmax 9\noom 3\noom_kill 4\noom_group_kill 0\n").unwrap();
        for p in &pids {
            unsafe { libc::kill(*p as i32, libc::SIGKILL) };
        }
    }
    let s = w.wait_for("both restarted", T, |s| {
        (0..2).all(|i| {
            s["workers"][i]["state"] == "RUNNING" && !pids.contains(&s["workers"][i]["pid"].as_u64().unwrap_or(0))
        })
    });
    let mut reasons: Vec<String> =
        (0..2).map(|i| s["workers"][i]["last_exit"].as_str().unwrap_or_default().to_string()).collect();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            "killed by another process or the kernel OOM killer (SIGKILL)".to_string(),
            "probably killed by the kernel OOM killer (out of memory)".to_string()
        ],
        "{}",
        w.log()
    );
    let log = w.wait_log("names the pid the kernel killed", T);
    assert!(log.contains("the kernel may have killed one of them instead"), "{log}");
    assert!(!log.contains("Probably not the kernel's OOM killer"), "{log}");
    let _ = std::fs::remove_file(&events);
}

/// A child of our own memory cgroup with `limit` bytes (cgroup v1 or v2), if
/// this machine lets us make one: (its directory, its cgroup.procs).
fn memory_cgroup(test: &str, limit: u64) -> Option<(PathBuf, PathBuf)> {
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let name = format!("warden-it-{test}-{}", std::process::id());
    for line in cgroup.lines() {
        let mut f = line.splitn(3, ':');
        let (Some(_), Some(ctl), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
        let rel = path.trim_start_matches('/');
        let (dir, files) = if ctl.split(',').any(|c| c == "memory") {
            let dir = PathBuf::from("/sys/fs/cgroup/memory").join(rel).join(&name);
            (dir, [("memory.limit_in_bytes", limit.to_string()), ("memory.memsw.limit_in_bytes", limit.to_string())])
        } else if ctl.is_empty() && std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
            let dir = PathBuf::from("/sys/fs/cgroup").join(rel).join(&name);
            (dir, [("memory.max", limit.to_string()), ("memory.swap.max", "0".to_string())])
        } else {
            continue;
        };
        if std::fs::create_dir(&dir).is_err() {
            continue;
        }
        // The limit itself is required; a swap limit may not exist (no swap accounting).
        if std::fs::write(dir.join(files[0].0), &files[0].1).is_ok() {
            let _ = std::fs::write(dir.join(files[1].0), &files[1].1);
            let procs = dir.join("cgroup.procs");
            return Some((dir, procs));
        }
        let _ = std::fs::remove_dir(&dir);
    }
    None
}

impl Warden {
    /// Like `start`, with Warden (and so its workers) in the cgroup whose
    /// `cgroup.procs` is `procs`.
    fn start_in_cgroup(name: &str, port: u16, toml: &str, procs: &std::path::Path) -> Warden {
        let dir = Warden::dir_for(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("warden.toml");
        let text = format!("{toml}\n[control]\nsocket = \"{}\"\n", dir.join("w.sock").display());
        std::fs::write(&cfg, text).unwrap();
        let log = std::fs::File::create(dir.join("warden.log")).unwrap();
        let child = Command::new("sh")
            .args(["-c", "echo $$ > \"$0\" && exec \"$@\""])
            .arg(procs)
            .arg(BIN)
            .args(["start", "-c"])
            .arg(&cfg)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(log)
            .spawn()
            .unwrap();
        Warden { child, cfg, dir, port, stopped: Default::default() }
    }
}

/// A real OOM kill, where the sandbox lets us make a memory cgroup.
#[test]
fn a_real_oom_kill_is_reported_with_its_fix() {
    if !have_bun() {
        return;
    }
    let Some((cg, procs)) = memory_cgroup("oomreal", 192 << 20) else {
        eprintln!("skipping: can't create a memory cgroup with a limit here");
        return;
    };
    let port = free_port();
    let w = Warden::start_in_cgroup("oomreal", port, &simple("oomreal", port, 1, ""), &procs);
    let pid = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let _ = get(port, "/leak"); // ~200 MB more than the worker had
    let s = w.wait_for("OOM-killed and restarted", T, |s| {
        s["workers"][0]["state"] == "RUNNING" && s["workers"][0]["pid"].as_u64().is_some_and(|p| p != pid)
    });
    assert_eq!(s["workers"][0]["last_exit"], "killed by the kernel OOM killer (out of memory)", "{}", w.log());
    w.wait_log("raise memory.max / MemoryMax=", T);
    drop(w);
    for _ in 0..50 {
        if std::fs::remove_dir(&cg).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A worker in a memory cgroup of its own, below Warden's (its command
/// moves it there, as `systemd-run --scope` would): its OOM kill is counted
/// in that cgroup, not in Warden's, and still told apart. It used to read
/// as `killed by another process (SIGKILL)`, "probably not the OOM killer".
#[test]
fn a_real_oom_kill_in_a_cgroup_of_its_own_is_told_apart() {
    if !have_bun() {
        return;
    }
    let Some((cg, procs)) = memory_cgroup("oomown", 192 << 20) else {
        eprintln!("skipping: can't create a memory cgroup with a limit here");
        return;
    };
    let port = free_port();
    // Without Warden's shim (the command is sh): one worker, ready once it listens.
    let toml = format!(
        "[app]\nname = \"oomown\"\ncommand = \"sh\"\nargs = [\"-c\", \"echo $$ > {} && exec bun {}\"]\nport = {port}\n\
         [restart]\nbackoff_initial = 50\n[shutdown]\ngrace_period = 5\n",
        procs.display(),
        fixture("app.ts")
    );
    let w = Warden::start("oomown", port, &toml);
    let pid = Warden::pids(&w.wait_for("ready", T, ready(1)))[0];
    let own = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap_or_default();
    assert_ne!(own, std::fs::read_to_string("/proc/self/cgroup").unwrap(), "the worker runs in a cgroup of its own");
    let _ = get(port, "/leak"); // ~200 MB more than the worker had
    let s = w.wait_for("OOM-killed and restarted", T, |s| {
        s["workers"][0]["state"] == "RUNNING" && s["workers"][0]["pid"].as_u64().is_some_and(|p| p != pid)
    });
    assert_eq!(s["workers"][0]["last_exit"], "killed by the kernel OOM killer (out of memory)", "{}", w.log());
    drop(w);
    for _ in 0..50 {
        if std::fs::remove_dir(&cg).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// ---- standby
//
// `[workers] standby = N`: hot standbys, initialized but not listening,
// promoted into the slot of a worker that dies. Status lists them in
// `standbys` (absent without standbys), never in `workers`.

/// Pids of the workers (not the standbys).
fn worker_pids(s: &Value) -> HashSet<u64> {
    pid_set(s)
}

fn standby_rows(s: &Value) -> Vec<&Value> {
    s["standbys"].as_array().map(|a| a.iter().collect()).unwrap_or_default()
}

/// The pid of a standby ready to take over, if any.
fn ready_standby(s: &Value) -> Option<u64> {
    standby_rows(s).into_iter().find(|w| w["state"] == "STANDBY").and_then(|w| w["pid"].as_u64())
}

/// `gated` (health gates on the private socket) with one hot standby.
fn standby_config(name: &str, port: u16, count: usize, extra: &str) -> String {
    gated(name, port, count, extra).replace("[workers]\n", "[workers]\nstandby = 1\n")
}

/// kill -9 `victim`, then poll the port until an answer satisfies `until`;
/// returns it and the time since the kill.
fn kill_and_wait_for(port: u16, victim: u64, path: &str, until: impl Fn(&str) -> bool) -> (String, Duration) {
    let t0 = Instant::now();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    loop {
        if let Some(body) = get(port, path).filter(|b| until(b)) {
            return (body, t0.elapsed());
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "nothing new answered on {port} after kill -9 {victim}");
        std::thread::sleep(Duration::from_micros(200));
    }
}

fn env_of(pid: u64) -> String {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|b| String::from_utf8_lossy(&b).replace('\0', "\n"))
        .unwrap_or_default()
}

fn ms(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

/// Warden's own measure of the last promotion (promote message → listening).
fn promote_ms(w: &Warden) -> String {
    let log = w.wait_log("worker promoted from standby", T);
    let line = log.lines().rfind(|l| l.contains("worker promoted from standby")).unwrap_or_default();
    line.split("promote_ms=").nth(1).unwrap_or("?").trim().to_string()
}

/// The headline: kill -9 the only worker; the standby (initialized, never
/// listening, so no traffic) answers on the port within milliseconds as
/// worker 1, and a new standby starts in the background.
#[test]
fn standby_takes_over_a_killed_bun_worker_in_milliseconds() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("sb-bun", port, &standby_config("sb-bun", port, 1, ""));
    let s = w.wait_for("a worker and a standby", T, |s| s["workers_ready"] == 1 && ready_standby(s).is_some());
    let victim = s["workers"][0]["pid"].as_u64().unwrap();
    let standby = ready_standby(&s).unwrap();
    assert_eq!(standby_rows(&s).len(), 1);
    assert!(env_of(standby).contains("WARDEN_STANDBY=1\n"), "{}", env_of(standby));
    assert!(!env_of(victim).contains("WARDEN_STANDBY"), "workers are not standbys");

    // No traffic before promotion: the worker owns the port's only listener.
    assert_eq!(listeners(port), 1, "the standby must not join the port's listener group");
    for _ in 0..50 {
        let who = get(port, "/whoami").expect("request failed");
        assert!(who.starts_with(&format!("{victim}:")), "{who}");
    }
    let (_, out) = w.cli(&["status"]);
    assert!(
        out.lines().any(|l| l.starts_with("│ standby ") && l.contains("│ 1/1 ready"))
            && out.lines().any(|l| l.starts_with("│ s1 ") && l.contains("STANDBY")),
        "{out}"
    );
    assert_eq!(s["workers"].as_array().map(Vec::len), Some(1), "standbys are not workers");

    // The stream is live once Warden has said hello: it subscribed before it
    // answered. Events from before that are never replayed, so the kill must
    // wait for it (on a loaded machine Warden may take the subscription later
    // than the kill, and the story below would start in the middle).
    let mut ev = Events::open(&w, r#"{"cmd":"subscribe"}"#);
    assert_eq!(ev.next()["type"], "hello");
    assert_eq!(ev.next()["type"], "status");
    let (who, took) = kill_and_wait_for(port, victim, "/whoami", |b| !b.starts_with(&format!("{victim}:")));
    eprintln!(
        "standby promotion (Bun.serve): the port answered again {} after kill -9 (promote → listening: {} ms)",
        ms(took),
        promote_ms(&w)
    );
    // The behaviour, not a stopwatch (which load can break): the process that
    // answered after the kill is the standby that was already initialized,
    // not a worker started cold (that would be a new pid), and (events
    // below) it served before Warden started another process. How fast is
    // measured by the printed time and `promote_ms`, in docs/benchmarks.md.
    assert!(who.starts_with(&format!("{standby}:")), "the standby took over: {who}");

    // Events: the usual story of worker 1, the new process marked as promoted.
    let events = ev.until("worker 1 ready", |e| is_worker(e, 1, "ready"));
    assert_eq!(worker_story(&events, 1), ["crashed", "restarting", "starting", "ready"], "{events:#?}");
    assert!(
        !events.iter().any(|e| e["type"] == "worker" && e["standby"] == 1 && e["event"] == "starting"),
        "no new process was started before the promoted standby was serving: {events:#?}"
    );
    let promoted: Vec<&Value> = events.iter().filter(|e| e["worker"] == 1 && e["pid"] == standby).collect();
    assert_eq!(promoted.len(), 2, "{events:#?}");
    assert!(promoted.iter().all(|e| e["detail"].as_str().unwrap().contains("promoted from standby s1")));
    assert!(promoted.iter().all(|e| e.get("standby").is_none()), "worker 1's events, not a standby's: {events:#?}");
    // A standby's own events say which standby it is (`s1`, next to worker
    // 0), as `warden status` and the log lines name it: here the new s1.
    let fresh = ev.until("the new standby ready", |e| e["standby"] == 1 && e["event"] == "ready");
    let ready = fresh.last().unwrap();
    assert_eq!(ready["worker"], 0, "{ready}");
    assert_ne!(ready["pid"].as_u64(), Some(standby), "{ready}");
    assert!(ready["detail"].as_str().unwrap().ends_with("role=standby"), "{ready}");
    assert!(
        fresh.iter().any(|e| e["standby"] == 1 && e["event"] == "starting" && e["pid"] == ready["pid"]),
        "{fresh:#?}"
    );

    let s =
        w.wait_for("a new standby", T, |s| ready_standby(s).is_some_and(|p| p != standby) && s["workers_ready"] == 1);
    let w1 = &s["workers"][0];
    assert_eq!(
        (w1["pid"].as_u64(), w1["restarts"].as_u64(), w1["crashes"].as_u64()),
        (Some(standby), Some(1), Some(1))
    );
    assert!(w1["last_exit"].as_str().unwrap().contains("SIGKILL"), "{s:#?}");
    let log = w.wait_log("worker promoted from standby worker=1", T);
    assert!(log.contains(&format!("worker promoted from standby worker=1 pid={standby}")), "{log}");
    assert_eq!(listeners(port), 1, "the new standby does not listen either");
}

/// Node (node:http, PM2 style): the standby gets no traffic, then takes
/// worker 2's slot with its NODE_APP_INSTANCE.
#[test]
fn standby_node_worker_takes_the_slot_and_its_instance() {
    if !have_bun() || !have_node() {
        return;
    }
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"sb-node\"\ncommand = \"node\"\nargs = [\"{}\"]\nport = {port}\n[workers]\ncount = 2\nstandby = 1\n\
         [health]\npath = \"/whoami\"\n[reload]\nhealth_passes = 1\nhealth_interval_ms = 100\n[shutdown]\ndrain_ms = 100\n",
        fixture("node_app.mjs")
    );
    let w = Warden::start("sb-node", port, &cfg);
    let s = w.wait_for("2 workers and a standby", T, |s| s["workers_ready"] == 2 && ready_standby(s).is_some());
    let standby = ready_standby(&s).unwrap();
    assert!(env_of(standby).contains("NODE_APP_INSTANCE=2\n"), "past the workers' numbers until promoted");
    let mut seen = HashSet::new();
    for _ in 0..60 {
        seen.insert(get(port, "/whoami").expect("request failed"));
    }
    let pids: HashSet<u64> = seen.iter().map(|x| x.split(':').next().unwrap().parse().unwrap()).collect();
    assert_eq!(pids, worker_pids(&s), "only workers answer, never the standby: {seen:?}");
    assert_eq!(listeners(port), 2);

    let victim = s["workers"][1]["pid"].as_u64().unwrap(); // worker 2: instance 1
    let (who, took) = kill_and_wait_for(port, victim, "/whoami", |b| b.starts_with(&format!("{standby}:")));
    eprintln!(
        "standby promotion (node:http on Node): the standby answered {} after kill -9 (promote → listening: {} ms)",
        ms(took),
        promote_ms(&w)
    );
    assert_eq!(who, format!("{standby}:1"), "the promoted standby is worker 2, NODE_APP_INSTANCE 1");
    let s =
        w.wait_for("a new standby", T, |s| ready_standby(s).is_some_and(|p| p != standby) && s["workers_ready"] == 2);
    assert_eq!(s["workers"][1]["pid"].as_u64(), Some(standby));
    assert_eq!(s["workers"][0]["restarts"], 0, "worker 1 untouched");
}

/// NestJS (Express on node:http), on Bun and on Node: the whole app starts
/// in the standby; its `app.listen()` is held back until the promotion.
#[test]
fn standby_nestjs_on_bun_and_node() {
    let nest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench/nest");
    if !have_bun() || !have_node() || !nest.join("node_modules/@nestjs/core").exists() {
        eprintln!("skipping: NestJS is not installed in bench/nest (cd bench/nest && npm ci)");
        return;
    }
    // Node runs the bundled build, as bench/run.ts makes it.
    let built = Command::new("bun")
        .args(["build", "main.ts", "--target=node", "--packages=external", "--outfile=dist/main.mjs"])
        .current_dir(&nest)
        .output()
        .unwrap();
    assert!(built.status.success(), "{}", String::from_utf8_lossy(&built.stderr));
    for (name, command, entry) in [("sb-nest-bun", "bun", "main.ts"), ("sb-nest-node", "node", "dist/main.mjs")] {
        let port = free_port();
        let cfg = format!(
            "[app]\nname = \"{name}\"\ncommand = \"{command}\"\nargs = [\"{entry}\"]\nworking_directory = \"{}\"\nport = {port}\n\
             [workers]\nstandby = 1\nready_timeout = 60\n[health]\npath = \"/health\"\n[reload]\nhealth_passes = 1\n\
             health_interval_ms = 100\n[shutdown]\ndrain_ms = 100\n",
            nest.display()
        );
        let w = Warden::start(name, port, &cfg);
        let s = w.wait_for("a worker and a standby", Duration::from_secs(60), |s| {
            s["workers_ready"] == 1 && ready_standby(s).is_some()
        });
        let (victim, standby) = (s["workers"][0]["pid"].as_u64().unwrap(), ready_standby(&s).unwrap());
        assert_eq!(listeners(port), 1, "{name}: the standby must not listen");
        let (_, took) = kill_and_wait_for(port, victim, "/whoami", |b| b.starts_with(&format!("{standby}:")));
        eprintln!(
            "standby promotion (NestJS, {command}): the port answered again {} after kill -9 (promote → listening: {} ms)",
            ms(took),
            promote_ms(&w)
        );
        assert!(took < Duration::from_secs(1), "{name}: {took:?}\n{}", w.log());
        assert!(get(port, "/json").is_some_and(|b| b.contains("Hello")), "{name}: the app works after promotion");
        if command == "node" {
            // Node runs the listen callback (which logs) at promotion: by then
            // the standby's output carries the slot it took.
            let line = format!("worker=1 stdout: nest bench listening on :{port} ({standby}:");
            w.wait_log(&line, T);
            assert!(!w.log().contains("worker=s1 stdout: nest bench listening"), "{}", w.log());
        }
    }
}

/// A reload replaces the standby too (it runs the old code): after the
/// workers, through the gates, with no failed request.
#[test]
fn reload_with_a_standby_drops_nothing_and_replaces_the_standby() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("sb-reload", port, &standby_config("sb-reload", port, 2, ""));
    let s = w.wait_for("ready", T, |s| s["workers_ready"] == 2 && ready_standby(s).is_some());
    let (before, old) = (worker_pids(&s), ready_standby(&s).unwrap());
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
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}");
    let s = w.wait_for("new workers and a new standby", T, |s| {
        s["workers_ready"] == 2
            && worker_pids(s).is_disjoint(&before)
            && ready_standby(s).is_some_and(|p| p != old && !before.contains(&p))
    });
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    let (ok, fail) = (ok.load(Ordering::Relaxed), fail.load(Ordering::Relaxed));
    eprintln!("reload with a standby: {ok} ok, {fail} failed");
    assert!(ok > 50);
    assert!(fail <= allowed_resets(), "requests failed during the reload: {fail}");
    assert!(!worker_pids(&s).contains(&old), "the old standby was not promoted mid-reload");
    let log = w.log();
    let (done, replaced) = (log.find("reload complete").unwrap(), log.find("replacing standbys").unwrap());
    assert!(done < replaced, "the standby is replaced after the workers:\n{log}");
    let t0 = Instant::now();
    while alive(old) {
        assert!(t0.elapsed() < Duration::from_secs(5), "old standby still running");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A safe-reload that rolls back leaves the standby alone: like the
/// workers, it runs the previous version. A good deploy then replaces it.
#[test]
fn safe_reload_rollback_keeps_the_previous_standby() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("sb-rollback", port, &standby_config("sb-rollback", port, 2, ""));
    let s = w.wait_for("ready", T, |s| s["workers_ready"] == 2 && ready_standby(s).is_some());
    let (before, standby) = (worker_pids(&s), ready_standby(&s).unwrap());
    let good = std::fs::read_to_string(&w.cfg).unwrap();
    std::fs::write(&w.cfg, good.replace("[workers]", "env = { FIXTURE_HEALTH_FAIL = \"1\" }\n[workers]")).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("Rolled back"), "{out}");
    std::thread::sleep(Duration::from_millis(300));
    let s = w.status().unwrap();
    assert_eq!(worker_pids(&s), before);
    assert_eq!(ready_standby(&s), Some(standby), "the previous version's standby stays: {s:#?}");
    assert!(!w.log().contains("replacing standbys"));

    std::fs::write(&w.cfg, &good).unwrap();
    let (code, out) = w.cli(&["safe-reload"]);
    assert_eq!(code, 0, "{out}");
    w.wait_for("a fresh standby", T, |s| ready_standby(s).is_some_and(|p| p != standby));
}

/// A standby that keeps crashing is restarted with backoff, then the pool
/// is FAILED: a bounded number of starts, never a storm. Crashed workers
/// restart the normal way meanwhile; `warden reset` retries the standbys.
#[test]
fn a_crashing_standby_backs_off_and_never_storms() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = standby_config("sb-crash", port, 1, "env = { FIXTURE_STANDBY_EXIT = \"300\" }").replace(
        "[restart]\nbackoff_initial = 50\n",
        "[restart]\nbackoff_initial = 100\nmax_restarts = 3\nfailed_cooldown = 0\n",
    );
    let w = Warden::start("sb-crash", port, &cfg);
    let s = w.wait_for("standbys FAILED", T, |s| {
        s["workers_ready"] == 1 && standby_rows(s).first().is_some_and(|x| x["state"] == "FAILED")
    });
    let log = w.wait_log("standbys failed: too many standby crashes", T);
    assert_eq!(log.matches("standby starting").count(), 4, "a first start and 3 restarts:\n{log}");
    let delays: Vec<u64> = log
        .lines()
        .filter(|l| l.contains("standby restarting"))
        .filter_map(|l| l.split("in_ms=").nth(1)?.split_whitespace().next()?.parse().ok())
        .collect();
    assert_eq!(delays, [0, 100, 200], "backoff like a worker's:\n{log}");
    let row = standby_rows(&s)[0];
    assert_eq!((row["crashes"].as_u64(), row["restarts"].as_u64()), (Some(4), Some(3)), "{s:#?}");
    assert!(row["last_exit"].as_str().unwrap().contains("exit code 4"), "{s:#?}");

    // No standby: a crashed worker restarts the normal way.
    let victim = s["workers"][0]["pid"].as_u64().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    w.wait_for("worker 1 back", T, |s| s["workers_ready"] == 1 && s["workers"][0]["pid"].as_u64() != Some(victim));
    std::thread::sleep(Duration::from_millis(1500));
    let log = w.log();
    assert!(!log.contains("worker promoted from standby"), "{log}");
    assert_eq!(log.matches("standby starting").count(), 4, "nothing started while FAILED");

    let (code, out) = w.cli(&["reset"]);
    assert_eq!(code, 0, "{out}");
    let t0 = Instant::now();
    while w.log().matches("standby starting").count() < 5 {
        assert!(t0.elapsed() < T, "standbys not retried after reset:\n{}", w.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Recycling keeps the code: an available standby is the replacement for
/// an unhealthy worker (it listens next to it, passes the gates, then the
/// old one drains).
#[test]
fn an_unhealthy_worker_is_replaced_by_the_standby() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = standby_config("sb-sick", port, 1, "")
        .replace("[health]\n", "[health]\nenabled = true\ninterval = 1\nfailure_threshold = 2\ninitial_delay = 0\n");
    let w = Warden::start("sb-sick", port, &cfg);
    let s = w.wait_for("ready with a standby", T, |s| s["workers_ready"] == 1 && ready_standby(s).is_some());
    let (before, standby) = (s["workers"][0]["pid"].as_u64().unwrap(), ready_standby(&s).unwrap());
    let _ = get(port, "/sick");
    let s = w.wait_for("replaced by the standby", T, |s| {
        s["workers"][0]["pid"].as_u64() == Some(standby) && s["rollout"].is_null() && s["workers_ready"] == 1
    });
    assert_eq!(s["workers"][0]["crashes"], 0, "a graceful replacement is not a crash");
    assert_eq!(s["last_rollout"]["kind"], "replace");
    assert!(w.log().contains("standby promoted as the replacement"), "{}", w.log());
    w.wait_for("a new standby", T, |s| ready_standby(s).is_some_and(|p| p != standby));
    let t0 = Instant::now();
    while alive(before) {
        assert!(t0.elapsed() < Duration::from_secs(5), "the unhealthy worker was not drained");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Standbys need no socket path of their own: a runtime directory so long
/// that only the control socket still fits (a deep checkout) works, without
/// a health path (the Bun stand-in then serves on an ephemeral port).
#[test]
fn standby_works_from_a_long_runtime_directory() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let name = format!("sb-long-{}", "x".repeat(55));
    let cfg = simple("sb-long", port, 1, "").replace("[workers]\ncount = 1\n", "[workers]\ncount = 1\nstandby = 1\n");
    let w = Warden::start(&name, port, &cfg);
    assert!(w.socket().as_os_str().len() > 85, "{}", w.socket().display());
    let s = w.wait_for("a worker and a standby", T, |s| s["workers_ready"] == 1 && ready_standby(s).is_some());
    let (victim, standby) = (s["workers"][0]["pid"].as_u64().unwrap(), ready_standby(&s).unwrap());
    assert_eq!(listeners(port), 1, "the stand-in is not on the app's port");
    let (who, _) = kill_and_wait_for(port, victim, "/whoami", |b| !b.starts_with(&format!("{victim}:")));
    assert!(who.starts_with(&format!("{standby}:")), "{who}");
}

/// Release pinning and surge: a standby starts in the pinned release, so
/// after `current` is swapped a crashed worker's slot still gets the release
/// the others run; a surge reload (every worker at once) drops nothing and
/// then replaces the standby in the new release.
#[test]
fn standby_follows_the_pinned_release_and_a_surge_reload() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let dir = release_tree("sbpin");
    let cfg = release_config("sbpin", port, &dir, "")
        .replace("[workers]\ncount = 2\n", "[workers]\ncount = 2\nstandby = 1\n[reload]\nsurge = \"all\"\n");
    let w = Warden::start("sbpin", port, &cfg);
    let release_of = |pid: u64| {
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).unwrap_or_default();
        cwd.display().to_string().rsplit('/').next().unwrap_or_default().to_string()
    };
    let s = w.wait_for("ready with a standby", T, |s| s["workers_ready"] == 2 && ready_standby(s).is_some());
    let standby = ready_standby(&s).unwrap();
    assert_eq!(release_of(standby), "v1");

    // The deploy swaps `current`; a worker crashes before the reload: the
    // standby (v1) takes its slot, as the pin says, and its successor is v1.
    swap_current(&dir, "v2");
    let victim = s["workers"][0]["pid"].as_u64().unwrap();
    unsafe { libc::kill(victim as i32, libc::SIGKILL) };
    let s = w.wait_for("standby promoted, a new one ready", T, |s| {
        s["workers"][0]["pid"].as_u64() == Some(standby)
            && s["workers_ready"] == 2
            && ready_standby(s).is_some_and(|p| p != standby)
    });
    assert_eq!(releases_seen(port, true), ["v1".to_string()].into(), "no mixed versions after a crash");
    let second = ready_standby(&s).unwrap();
    assert_eq!(release_of(second), "v1", "a standby started between swap and reload stays on the pin");

    // The reload (surge: both workers at once) moves everyone to v2.
    let before = worker_pids(&s);
    let stop = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicUsize::new(0));
    let client = {
        let (stop, fail) = (stop.clone(), fail.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if get(port, "/where").is_none() {
                    fail.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
    };
    let (code, out) = w.cli(&["reload"]);
    assert_eq!(code, 0, "{out}\n{}", w.log());
    let s = w.wait_for("new workers and a v2 standby", T, |s| {
        worker_pids(s).is_disjoint(&before) && ready_standby(s).is_some_and(|p| p != second && release_of(p) == "v2")
    });
    stop.store(true, Ordering::Relaxed);
    client.join().unwrap();
    assert!(fail.load(Ordering::Relaxed) <= allowed_resets(), "requests failed during the surge reload");
    assert_eq!(releases_seen(port, true), ["v2".to_string()].into());
    assert!(s["release"].as_str().unwrap_or("").ends_with("releases/v2"), "{s:#?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A promoted standby's listeners were made at promotion, not at startup:
/// a reload must still end its WebSockets (1001) and SSE streams cleanly,
/// on Bun and on Node. One worker, so every client is on the promoted one.
#[test]
fn a_promoted_standby_hands_over_long_lived_connections() {
    if !have_bun() || !have_node() {
        return;
    }
    for (name, app, paths) in [
        ("sbll-bun", format!("args = [\"{}\"]", fixture("longlived.ts")), &["/ws", "/sse", "/sse-direct"][..]),
        (
            "sbll-node",
            format!("command = \"node\"\nargs = [\"{}\"]", fixture("longlived_node.mjs")),
            &["/ws", "/sse"][..],
        ),
    ] {
        let port = free_port();
        let app = format!("{app}\n[workers]\ncount = 1\nstandby = 1");
        let w = Warden::start(name, port, &long_lived_config(name, port, &app, ""));
        let s = w.wait_for("a worker and a standby", T, |s| s["workers_ready"] == 1 && ready_standby(s).is_some());
        let (victim, standby) = (s["workers"][0]["pid"].as_u64().unwrap(), ready_standby(&s).unwrap());
        unsafe { libc::kill(victim as i32, libc::SIGKILL) };
        w.wait_for("promoted", T, |s| s["workers"][0]["pid"].as_u64() == Some(standby) && s["workers_ready"] == 1);
        let run = reload_holding(&w, 1, paths);
        assert_eq!(run.before, HashSet::from([standby]), "{name}: the clients held the promoted standby");
        assert_clean_handover(&w, &run, Duration::from_secs(20));
    }
}

/// `standby = 0` (the default): no standby rows, processes, sockets or
/// env; a crash restarts cold, as before.
#[test]
fn standby_zero_changes_nothing() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let w = Warden::start("sb-zero", port, &simple("sb-zero", port, 1, ""));
    let s = w.wait_for("ready", T, ready(1));
    assert_eq!(s["workers"].as_array().unwrap().len(), 1, "no standby rows: {s:#?}");
    assert!(s.get("standbys").is_none(), "`standbys` is not even sent: {s:#?}");
    let victim = s["workers"][0]["pid"].as_u64().unwrap();
    assert!(!env_of(victim).contains("WARDEN_STANDBY"));
    let (_, took) = kill_and_wait_for(port, victim, "/whoami", |b| !b.starts_with(&format!("{victim}:")));
    eprintln!("cold restart (no standby, Bun fixture): the port answered again {} after kill -9", ms(took));
    let log = w.wait_log("worker ready worker=1", T);
    assert!(!log.to_lowercase().contains("standby"), "{log}");
    let s = w.status().unwrap();
    assert_eq!(s["workers"].as_array().unwrap().len(), 1);
    assert_eq!(s["workers"][0]["restarts"], 1);
}

/// Every `warden logs` command a standby hint names works and shows the
/// standbys' lines: their output (`worker=s1`), their own lines (`standby
/// crashed worker=s1`) and the pool's (`worker=standby`). So does the
/// `--worker standby` of the docs, with -c.
#[test]
fn standby_hints_name_log_commands_that_work() {
    if !have_bun() {
        return;
    }
    let port = free_port();
    let cfg = standby_config("sb-hints", port, 1, "env = { FIXTURE_STANDBY_EXIT = \"200\" }").replace(
        "[restart]\nbackoff_initial = 50\n",
        "[restart]\nbackoff_initial = 50\nmax_restarts = 1\nfailed_cooldown = 0\n",
    );
    let w = Warden::start("sb-hints", port, &cfg);
    let log = w.wait_log("standbys failed: too many standby crashes", T);
    let mut commands: Vec<String> = log
        .lines()
        .filter(|l| l.contains("standby") && l.contains("hint="))
        .filter_map(|l| l.split("hint=").nth(1))
        .flat_map(|h| h.split('`').skip(1).step_by(2).map(str::to_string).collect::<Vec<_>>())
        .filter(|c| c.starts_with("warden logs "))
        .collect();
    commands.sort();
    commands.dedup();
    assert_eq!(
        commands,
        ["warden logs sb-hints --worker s1", "warden logs sb-hints --worker standby"],
        "the hints:\n{log}"
    );
    let output = "OUT   worker=s1 stdout: fixture: standby exiting";
    for c in &commands {
        let args: Vec<&str> = c.split_whitespace().skip(1).chain(["--nostream", "-n", "100"]).collect();
        let (code, out) = w.cli(&args);
        assert_eq!(code, 0, "`{c}`: {out}");
        assert!(out.contains(output), "`{c}` shows the standby's output:\n{out}");
        assert!(out.contains("WARN  standby crashed worker=s1 pid="), "`{c}`:\n{out}");
        assert!(!out.contains("worker ready worker=1"), "`{c}` shows only standbys:\n{out}");
        if c.ends_with("standby") {
            assert!(out.contains("restart the normal way meanwhile worker=standby max_restarts=1"), "`{c}`:\n{out}");
        }
    }
    let (code, out) = w.cli(&["logs", "--worker", "standby", "--nostream", "-n", "100"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains(output) && out.contains("standby starting worker=s1"), "{out}");
    let (code, out) = w.cli(&["logs", "--worker", "s2", "--nostream"]);
    assert_eq!((code, out.contains("worker=s1")), (0, false), "{out}");
    let (code, out) = w.cli(&["logs", "--worker", "stand", "--nostream"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("`standby`"), "{out}");
    let (code, out) = w.cli(&["restart", "--worker", "standby"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("takes no commands of its own"), "{out}");
}

/// Wait until `pid` is gone; returns how long that took from `t0`.
fn gone_within(pid: u64, t0: Instant, limit: Duration, what: &str, w: &Warden) -> Duration {
    while alive(pid) {
        assert!(t0.elapsed() < limit, "{what}: pid {pid} still running after {limit:?}\n{}", w.log());
        std::thread::sleep(Duration::from_millis(20));
    }
    t0.elapsed()
}

/// A standby never holds up a stop, on Node and Bun: its shim reads
/// Warden's commands without blocking its exit (a Node standby's read of
/// fd 3 sat on a thread Node joins at exit, so every stop waited for the
/// grace period's SIGKILL), and Warden ends that read when it stops one.
/// The standby's own stop signal (fd 3 still open), `warden stop` and
/// `warden kill` each take well under the 20 s grace period.
#[test]
fn a_standby_never_holds_up_a_stop() {
    if !have_bun() || !have_node() {
        return;
    }
    for (name, app) in [
        ("sbstop-node", format!("command = \"node\"\nargs = [\"{}\"]", fixture("node_app.mjs"))),
        ("sbstop-bun", format!("args = [\"{}\"]", fixture("app.ts"))),
    ] {
        let port = free_port();
        let cfg = format!(
            "[app]\nname = \"{name}\"\n{app}\nport = {port}\n[workers]\ncount = 1\nstandby = 1\n\
             [restart]\nbackoff_initial = 50\n[shutdown]\ngrace_period = 20\ndrain_ms = 100\n"
        );
        let mut w = Warden::start(name, port, &cfg);
        let limit = Duration::from_secs(2);
        let up = |w: &Warden| w.wait_for("a worker and a standby", T, |s| ready(1)(s) && ready_standby(s).is_some());

        // Its own SIGTERM, from outside Warden: the shim drains and exits.
        let standby = ready_standby(&up(&w)).unwrap();
        let t0 = Instant::now();
        unsafe { libc::kill(standby as i32, libc::SIGTERM) };
        let took = gone_within(standby, t0, limit, &format!("{name}: SIGTERM to the standby"), &w);
        eprintln!("{name}: a standby's own SIGTERM: gone after {}", ms(took));

        // `warden stop`: the workers and the standby.
        let s = w.wait_for("a new standby", T, |s| ready_standby(s).is_some_and(|p| p != standby) && ready(1)(s));
        let (worker, standby) = (s["workers"][0]["pid"].as_u64().unwrap(), ready_standby(&s).unwrap());
        let t0 = Instant::now();
        let (code, out) = w.cli(&["stop"]);
        assert_eq!(code, 0, "{out}");
        gone_within(worker, t0, limit, &format!("{name}: warden stop (worker)"), &w);
        let took = gone_within(standby, t0, limit, &format!("{name}: warden stop (standby)"), &w);
        eprintln!("{name}: warden stop: the standby was gone after {}", ms(took));

        // `warden kill`: the supervisor exits once its workers and standby have.
        let (code, out) = w.cli(&["start", name]);
        assert_eq!(code, 0, "{out}");
        let standby = ready_standby(&up(&w)).unwrap();
        let t0 = Instant::now();
        let (code, out) = w.cli(&["kill", "--yes"]);
        assert_eq!(code, 0, "{out}");
        while w.child.try_wait().unwrap().is_none() {
            assert!(t0.elapsed() < limit, "{name}: warden kill took over {limit:?}\n{}", w.log());
            std::thread::sleep(Duration::from_millis(20));
        }
        eprintln!("{name}: warden kill: Warden exited after {}", ms(t0.elapsed()));
        assert!(!alive(standby), "{name}: the standby outlived Warden");
        let log = w.log();
        assert!(!log.contains("did not exit within grace period"), "{name}:\n{log}");
        assert!(log.contains("standby stopped"), "{name}:\n{log}");
    }
}

// ------------------------------------------------------------------ [watch]

/// A directory for an app: `src/main.js` and an (empty) `node_modules`.
fn watch_app_dir(f: &Fleet, name: &str) -> PathBuf {
    let dir = f.home.join(format!("{name}-dir"));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("node_modules")).unwrap();
    std::fs::write(dir.join("src/main.js"), "v1").unwrap();
    dir
}

/// The app's workers are all new (none of `old` is left) and running.
fn replaced(f: &Fleet, name: &str, old: &[u64]) -> bool {
    let a = f.app(name);
    let running = a["status"]["workers"].as_array().is_some_and(|w| w.iter().all(|x| x["state"] == "RUNNING"));
    let now = f.pids(name);
    running && now.len() == old.len() && now.iter().all(|p| !old.contains(p))
}

/// The supervisor has taken its first look at the files (what is written
/// after that is a change).
fn watching_since_start(f: &Fleet, name: &str) {
    f.wait("the files to be watched", |f| {
        f.cli(&["logs", name, "--nostream", "--events", "--lines", "100"]).1.contains("file watching started")
    });
}

/// `[watch]`: a file changed in the app's directory starts a gated rolling
/// restart; ignored files do not; an app without `[watch]` (the default) is
/// left alone; `list`, `describe` and `status` say which is which.
#[test]
fn watched_files_restart_the_app_and_the_default_is_off() {
    let f = Fleet::new("watch");
    for (name, watch) in [
        ("watched", "[watch]\nenabled = true\ndebounce_ms = 200\ninterval_ms = 100\n"),
        ("quiet", ""),
        ("off", "[watch]\nenabled = false\ndebounce_ms = 200\ninterval_ms = 100\n"),
    ] {
        let dir = watch_app_dir(&f, name);
        let cfg = format!(
            "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\
             working_directory = {:?}\n{watch}",
            dir.display().to_string()
        );
        std::fs::write(f.home.join(format!("{name}.toml")), cfg).unwrap();
        f.ok(&["start", name]);
    }
    let watching = |name: &str| f.app(name)["status"]["watching"].clone();
    assert_eq!((watching("watched"), watching("quiet"), watching("off")), (true.into(), false.into(), false.into()));
    let list = f.ok(&["list"]);
    let cell = |app: &str| list.lines().find(|l| l.contains(&format!(" {app} "))).unwrap_or_default().to_string();
    assert!(list.contains("watching"), "a column: {list}");
    assert!(cell("watched").contains("│ ✓ ") && cell("quiet").contains("│ ✗ "), "{list}");
    let d = f.ok(&["describe", "watched"]);
    assert!(d.contains("enabled: a rolling restart 200 ms after the files in ."), "{d}");
    assert!(f.ok(&["describe", "quiet"]).contains("disabled (`[watch] enabled = true`"));

    let (w0, q0, o0) = (f.pids("watched"), f.pids("quiet"), f.pids("off"));
    assert!(w0.len() == 1 && q0.len() == 1 && o0.len() == 1);
    watching_since_start(&f, "watched");

    // Ignored by default: node_modules, *.log. Nothing restarts.
    let dir = f.home.join("watched-dir");
    std::fs::write(dir.join("node_modules/dep.js"), "x").unwrap();
    std::fs::write(dir.join("debug.log"), "x").unwrap();
    std::thread::sleep(Duration::from_millis(1200));
    assert_eq!(f.pids("watched"), w0, "ignored files are not a change");

    // A source file is: written in all three apps, only the watched one restarts.
    for name in ["watched", "quiet", "off"] {
        std::fs::write(f.home.join(format!("{name}-dir/src/main.js")), "version 2").unwrap();
    }
    f.wait("the watched app's rolling restart", |f| replaced(f, "watched", &w0));
    let log = f.ok(&["logs", "watched", "--nostream", "--events", "--lines", "100"]);
    assert!(log.contains("files changed: rolling restart") && log.contains("src/main.js"), "{log}");
    assert!(log.contains("restart started"), "gated like any restart: {log}");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!((f.pids("quiet"), f.pids("off")), (q0, o0), "nothing watches the others");
    assert!(f.app("watched")["status"]["last_rollout"]["ok"] == true, "{}", f.app("watched"));
    // Once, not again: the files stayed as they are.
    let w1 = f.pids("watched");
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(f.pids("watched"), w1);

    // An app that exists is watched by turning `[watch]` on and `warden reload` (no restart of Warden).
    let path = f.home.join("quiet.toml");
    let cfg = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{cfg}[watch]\nenabled = true\ndebounce_ms = 200\ninterval_ms = 100\n")).unwrap();
    f.ok(&["reload", "quiet"]);
    f.wait("quiet to be watched", |f| f.app("quiet")["status"]["watching"] == true);
    watching_since_start(&f, "quiet");
    let q1 = f.pids("quiet");
    std::fs::write(f.home.join("quiet-dir/src/main.js"), "version 3").unwrap();
    f.wait("quiet's rolling restart", |f| replaced(f, "quiet", &q1));
    // And off again the same way.
    std::fs::write(&path, cfg).unwrap();
    f.ok(&["reload", "quiet"]);
    f.wait("quiet to stop being watched", |f| f.app("quiet")["status"]["watching"] == false);
}

/// PM2's flags: `warden start ... --watch --ignore-watch --watch-delay` write
/// the `[watch]` section, and what they ignore is ignored.
#[test]
fn start_watch_flags_write_the_watch_section() {
    let f = Fleet::new("watchflags");
    let dir = watch_app_dir(&f, "flagged");
    std::fs::create_dir_all(dir.join("dist")).unwrap();
    let out = f.ok(&[
        "start",
        "sleep 300",
        "--name",
        "flagged",
        "--cwd",
        dir.to_str().unwrap(),
        "--watch",
        "--ignore-watch",
        "dist,*.map",
        "--watch-delay",
        "200ms",
    ]);
    assert!(out.contains("flagged: online"), "{out}");
    let cfg = std::fs::read_to_string(f.home.join("flagged.toml")).unwrap();
    assert!(cfg.contains("[watch]") && cfg.contains("enabled = true") && cfg.contains("debounce_ms = 200"), "{cfg}");
    assert!(cfg.contains("\"dist\"") && cfg.contains("\"*.map\"") && cfg.contains("\"node_modules\""), "{cfg}");
    assert_eq!(f.app("flagged")["status"]["watching"], true);
    // The flags that go with --watch mean nothing without it.
    let (code, out) = f.cli(&["start", "sleep 300", "--name", "other", "--watch-delay", "4"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("go with --watch"), "{out}");

    let before = f.pids("flagged");
    watching_since_start(&f, "flagged");
    std::fs::write(dir.join("dist/bundle.js"), "x").unwrap();
    std::fs::write(dir.join("bundle.js.map"), "x").unwrap();
    // Two looks at the files (interval 1 s) have passed: no restart.
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(f.pids("flagged"), before, "dist and *.map are ignored");
    std::fs::write(dir.join("src/main.js"), "version 2").unwrap();
    f.wait("the rolling restart", |f| replaced(f, "flagged", &before));
}

/// What Warden has logged for `name`, its own events included.
fn watch_events(f: &Fleet, name: &str) -> String {
    f.ok(&["logs", name, "--nostream", "--events", "--lines", "200"])
}

/// An app that only sleeps, watching its directory (looking every 100 ms, restarting 200 ms after
/// the last change), with `extra` sections before `[watch]`.
fn watched_sleeper(f: &Fleet, name: &str, extra: &str) -> PathBuf {
    let dir = watch_app_dir(f, name);
    let cfg = format!(
        "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\
         working_directory = {:?}\n{extra}[watch]\nenabled = true\ndebounce_ms = 200\ninterval_ms = 100\n",
        dir.display().to_string()
    );
    std::fs::write(f.home.join(format!("{name}.toml")), cfg).unwrap();
    dir
}

/// File watching is optional, so a panic in it (injected with `WARDEN_FAULT=watch:1`, a debug
/// build's fault point) ends the watcher with an error line and leaves the app running. `status`
/// and `list` say it is not watching, a save restarts nothing, and `warden reload` starts another.
#[test]
fn a_watcher_that_panics_is_reported_and_a_reload_starts_another() {
    if !cfg!(debug_assertions) {
        return; // the fault point is compiled out of release builds
    }
    let f = Fleet::new("watchfault");
    let dir = watched_sleeper(&f, "faulty", "");
    let (code, out) = f.cli_env(&["start", "faulty"], &[("WARDEN_FAULT", "watch:1")]);
    assert_eq!(code, 0, "{out}");

    f.wait("the supervisor to report the dead watcher", |f| {
        watch_events(f, "faulty").contains("file watching has stopped")
    });
    let log = watch_events(&f, "faulty");
    assert!(log.contains("file watching stopped: the scan panicked"), "{log}");
    assert!(log.contains("injected fault at watch") && log.contains("`warden reload` starts watching again"), "{log}");
    assert_eq!(f.app("faulty")["status"]["watching"], false, "the status says so");
    // One app: `list` is the key-and-value box, with a `watching` row.
    let list = f.ok(&["list"]);
    assert!(
        list.contains("online") && list.lines().any(|l| l.contains("watching") && l.contains("disabled")),
        "{list}"
    );
    assert!(f.ok(&["describe", "faulty"]).contains("set, but not running"));

    // Nothing watches: a save restarts nothing (and the dead watcher is not logged again and again).
    let before = f.pids("faulty");
    assert_eq!(before.len(), 1);
    std::fs::write(dir.join("src/main.js"), "version 2").unwrap();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(f.pids("faulty"), before, "no watcher, no restart");
    assert_eq!(watch_events(&f, "faulty").matches("file watching has stopped").count(), 1, "said once");

    // `warden reload` is what the message promises: a watcher again, which restarts the app.
    f.ok(&["reload", "faulty"]);
    f.wait("watching again", |f| f.app("faulty")["status"]["watching"] == true);
    watching_since_start(&f, "faulty");
    f.wait("the workers to run", |f| {
        f.app("faulty")["status"]["workers"].as_array().is_some_and(|w| w.iter().all(|x| x["state"] == "RUNNING"))
    });
    let reloaded = f.pids("faulty");
    std::fs::write(dir.join("src/main.js"), "version 3").unwrap();
    f.wait("the rolling restart", |f| replaced(f, "faulty", &reloaded));
    assert!(f.ok(&["list"]).lines().any(|l| l.contains("watching") && l.contains("enabled")));
}

/// A control socket (so the runtime directory) in the watched directory used to hide the whole
/// directory: "file watching started files=0", and no restart, ever. Only what Warden writes
/// there is skipped, and a new file in it is a change.
#[test]
fn a_control_socket_in_the_watched_directory_does_not_hide_the_app() {
    // `Warden::start` puts the socket (w.sock), the log and the config in this directory.
    let name = "watchsock";
    let dir = tmp().join(format!("warden-it-{name}-{}", std::process::id()));
    let toml = format!(
        "[app]\nname = \"{name}\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\
         working_directory = {:?}\n[watch]\nenabled = true\ndebounce_ms = 200\ninterval_ms = 100\n",
        dir.display().to_string()
    );
    let w = Warden::start(name, 0, &toml);
    let pids = |s: &Value| -> Vec<u64> {
        let all = s["workers"].as_array().cloned().unwrap_or_default();
        if all.iter().all(|x| x["state"] == "RUNNING") {
            all.iter().filter_map(|x| x["pid"].as_u64()).collect()
        } else {
            vec![]
        }
    };
    let s = w.wait_for("the app to run", T, |s| !pids(s).is_empty());
    let log = w.wait_log("file watching started", T);
    let files: u64 = log
        .split("file watching started files=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no file count in the log:\n{log}"));
    assert!(files >= 1, "the config next to the socket is a file of the app, not Warden's: files={files}\n{log}");
    assert!(!log.contains("found no files under the watched paths"), "{log}");

    // Warden's own files in that directory (the socket, its scripts, its log) are not changes.
    let before = pids(&s);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(pids(&w.status().unwrap()), before, "Warden's own files restarted the app:\n{}", w.log());

    // Anything else in it is the app's.
    std::fs::write(dir.join("app.js"), "version 2").unwrap();
    w.wait_for("the rolling restart", T, |s| {
        let now = pids(s);
        now.len() == before.len() && now.iter().all(|p| !before.contains(p))
    });
    let log = w.wait_log("files changed: rolling restart", T);
    assert!(log.contains("file=app.js"), "{log}");
}

/// Watching an app whose workers cannot run side by side (`[workers] overlap = false`) says at
/// the start that a restart that fails cannot be rolled back; an app that can overlap hears
/// nothing. The restart itself works for both.
#[test]
fn watching_an_app_that_cannot_roll_back_says_so() {
    let f = Fleet::new("watchnoroll");
    let dirs = [
        ("serial", watched_sleeper(&f, "serial", "[workers]\noverlap = false\n")),
        ("shared", watched_sleeper(&f, "shared", "")),
    ];
    for (name, _) in &dirs {
        f.ok(&["start", name]);
        watching_since_start(&f, name);
    }
    let serial = watch_events(&f, "serial");
    assert!(serial.contains("file watching is on, but a failing restart cannot be rolled back"), "{serial}");
    assert!(
        serial.contains("[workers] overlap = false") && serial.contains("no old worker to roll back to"),
        "{serial}"
    );
    assert!(!watch_events(&f, "shared").contains("cannot be rolled back"), "workers that overlap need no warning");

    // Both restart when a file changes (the serial one stops first, then starts).
    let (a, b) = (f.pids("serial"), f.pids("shared"));
    for (_, dir) in &dirs {
        std::fs::write(dir.join("src/main.js"), "version 2").unwrap();
    }
    f.wait("serial's restart", |f| replaced(f, "serial", &a));
    f.wait("shared's rolling restart", |f| replaced(f, "shared", &b));
}

/// A watcher that has nothing to look at (every file ignored, or an empty directory) says so once
/// instead of silently never restarting; a file that appears later is noticed all the same.
#[test]
fn watching_no_files_says_so() {
    let f = Fleet::new("watchnone");
    let dir = watch_app_dir(&f, "nothing");
    std::fs::create_dir_all(dir.join("empty")).unwrap();
    let cfg = format!(
        "[app]\nname = \"nothing\"\ncommand = \"sh\"\nargs = [\"-c\", \"exec sleep 600\"]\n\
         working_directory = {:?}\n[watch]\nenabled = true\npaths = [\"empty\"]\ndebounce_ms = 200\n\
         interval_ms = 100\n",
        dir.display().to_string()
    );
    std::fs::write(f.home.join("nothing.toml"), cfg).unwrap();
    f.ok(&["start", "nothing"]);
    watching_since_start(&f, "nothing");
    let log = watch_events(&f, "nothing");
    assert!(
        log.contains("file watching found no files under the watched paths: nothing will restart the app"),
        "{log}"
    );
    assert!(log.contains("paths=empty"), "it names what it looked at: {log}");

    let before = f.pids("nothing");
    std::fs::write(dir.join("empty/main.js"), "x").unwrap();
    f.wait("the rolling restart", |f| replaced(f, "nothing", &before));
}

// ------------------------------------------------- workers a killed supervisor left behind

/// Is `pid` a live process? A zombie, which only waits for its parent to
/// collect it, is not (`kill(pid, 0)` says yes to it).
fn running(pid: u64) -> bool {
    if !cfg!(target_os = "linux") {
        // No /proc: ps prints nothing for a pid that is gone, and Z for a zombie.
        let out = Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        return !stat.trim().is_empty() && !stat.trim_start().starts_with('Z');
    }
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|s| s.rsplit_once(')').is_some_and(|(_, rest)| !rest.trim_start().starts_with(['Z', 'X'])))
}

/// `pid`'s start time as the kernel counts it (field 22 of /proc/pid/stat,
/// clock ticks after boot): what a record of workers keeps next to the pid.
fn start_ticks(pid: u64) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let f: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
    f[19].parse().unwrap()
}

fn wait_until(what: &str, f: impl Fn() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < T, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Kills the `sleep <tag>` processes a failed test left running.
struct Sleepers(String);

impl Drop for Sleepers {
    fn drop(&mut self) {
        let want = format!("sleep\0{}\0", self.0).into_bytes();
        let Ok(dir) = std::fs::read_dir("/proc") else { return };
        for e in dir.flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<i32>().ok()) else { continue };
            if std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| c == want) {
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
        }
    }
}

/// A tag that tells this test's `sleep` processes from anyone's, and from
/// those of another test (`which`) running in the same process.
fn sleep_tag(which: u32) -> String {
    (600_000 + which * 100_000 + std::process::id() % 100_000).to_string()
}

/// macOS has no parent-death signal, so a worker outlives a supervisor
/// killed with SIGKILL; the next supervisor of the app stops it before
/// starting new ones (src/platform/orphans.rs). A debug build on Linux can
/// behave that way (`WARDEN_TEST_MACOS_ORPHANS`), which runs the record, the
/// sweep and the log against real processes. Without the keeper
/// (`WARDEN_KEEPER=0`), which would keep the workers on and supervise them
/// again instead.
#[test]
fn a_killed_supervisors_workers_are_stopped_when_the_app_starts_again() {
    let tag = sleep_tag(0);
    let _sleepers = Sleepers(tag.clone());
    let f = Fleet::new("orphans");
    let env = [("WARDEN_TEST_MACOS_ORPHANS", "1"), ("WARDEN_KEEPER", "0")];
    let cmd = format!("exec sleep {tag}");
    let start = ["start", "/bin/sh", "--name", "orphan", "--interpreter", "none", "-i", "2", "--", "-c", &cmd];
    let (code, out) = f.cli_env(&start, &env);
    assert_eq!(code, 0, "{out}");
    assert!(
        !out.contains("waiting for the previous supervisor's workers"),
        "nothing to sweep, nothing to wait for: {out}"
    );
    f.wait("two workers", |f| f.pids("orphan").len() == 2);
    let old = f.pids("orphan");
    let sup = supervisor_pid(&f, "orphan");

    // The supervisor has written down who it is and who its workers are, each
    // with a start time: a pid alone could be someone else's later.
    let file = f.home.join(format!("state/orphans/orphan.{sup}.json"));
    let record = || -> Option<Value> { serde_json::from_str(&std::fs::read_to_string(&file).ok()?).ok() };
    let workers_of = |r: &Value| -> Vec<u64> {
        let mut pids: Vec<u64> = r["workers"].as_array().unwrap().iter().filter_map(|w| w["pid"].as_u64()).collect();
        pids.sort_unstable();
        pids
    };
    let mut want = old.clone();
    want.sort_unstable();
    wait_until("the record to list both workers", || record().is_some_and(|r| workers_of(&r) == want));
    let rec = record().unwrap();
    assert_eq!((rec["v"].as_u64(), rec["app"].as_str()), (Some(1), Some("orphan")), "{rec:#}");
    assert_eq!(rec["supervisor"]["pid"].as_u64(), Some(sup), "{rec:#}");
    assert_eq!(rec["supervisor"]["start"].as_u64(), Some(start_ticks(sup)), "{rec:#}");
    for w in rec["workers"].as_array().unwrap() {
        assert_eq!(w["start"].as_u64(), Some(start_ticks(w["pid"].as_u64().unwrap())), "{rec:#}");
    }
    let labels: Vec<&str> = rec["workers"].as_array().unwrap().iter().filter_map(|w| w["label"].as_str()).collect();
    assert!(labels.contains(&"1") && labels.contains(&"2"), "worker labels as in the logs: {rec:#}");

    // kill -9: the workers keep running, as they do on macOS.
    unsafe { libc::kill(sup as i32, libc::SIGKILL) };
    wait_until("the supervisor to be gone", || !running(sup));
    std::thread::sleep(Duration::from_millis(300));
    assert!(old.iter().all(|p| running(*p)), "the workers outlive the killed supervisor: {old:?}");
    assert!(file.exists(), "a killed supervisor does not clean up its record");

    // The app starts again: the old workers are stopped first, and the new
    // ones are not among them.
    let (code, out) = f.cli_env(&["start", "orphan"], &env);
    assert_eq!(code, 0, "{out}");
    f.wait("two new workers", |f| {
        let now = f.pids("orphan");
        now.len() == 2 && now.iter().all(|p| !old.contains(p))
    });
    assert!(old.iter().all(|p| !running(*p)), "the old workers were stopped: {old:?}");
    assert!(!file.exists(), "the record of the dead supervisor is dropped");
    let new_sup = supervisor_pid(&f, "orphan");
    assert_ne!(new_sup, sup);
    let log_file = f.home.join("state/logs/orphan.log");
    wait_until("the log to say what was stopped", || {
        std::fs::read_to_string(&log_file)
            .is_ok_and(|l| l.contains("stopped workers left behind by a previous supervisor"))
    });
    let log = std::fs::read_to_string(&log_file).unwrap();
    let warned = log
        .lines()
        .find(|l| l.contains("workers of a previous supervisor of this app are still running"))
        .unwrap_or("");
    assert!(warned.contains(&format!("previous_supervisor={sup}")), "{log}");
    for p in &old {
        assert!(warned.contains(&p.to_string()), "pid {p} is named: {warned}");
    }
    assert!(warned.contains(" WARN ") && warned.contains(" hint="), "{warned}");
    assert!(!log.contains("did not exit within the grace period"), "SIGTERM was enough:\n{log}");
    every_warning_has_a_hint(&log);

    // A normal stop leaves nothing to sweep.
    let new_file = f.home.join(format!("state/orphans/orphan.{new_sup}.json"));
    assert!(new_file.exists(), "the new supervisor keeps its own record");
    f.ok(&["shutdown", "orphan"]);
    wait_until("the supervisor to be gone", || !running(new_sup));
    assert!(!new_file.exists(), "a supervisor that stops its workers removes its record");
}

/// What the sweep must not touch: a pid that a record lists but that is
/// another process now (the worker died and the pid was handed out again),
/// and the workers of a supervisor that is still alive.
#[test]
fn records_of_processes_that_are_not_orphans_are_left_alone() {
    let tag = sleep_tag(1);
    let _sleepers = Sleepers(tag.clone());
    let f = Fleet::new("orphans-safe");
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").unwrap().trim().to_string();
    let sleeper = || Command::new("sleep").arg(&tag).stdout(Stdio::null()).spawn().unwrap();
    let (mut bystander, mut live_sup, mut live_worker) = (sleeper(), sleeper(), sleeper());
    let mut gone = Command::new("true").spawn().unwrap();
    gone.wait().unwrap();
    let rec_dir = f.home.join("state/orphans");
    std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(&rec_dir).unwrap();
    let member = |pid: u32, start: u64, label: &str| serde_json::json!({ "pid": pid, "start": start, "label": label });
    let write = |name: String, sup: Value, workers: Vec<Value>| {
        let rec = serde_json::json!({ "v": 1, "app": "safe", "boot": boot, "written_ms": 1, "supervisor": sup, "workers": workers });
        std::fs::write(rec_dir.join(name), rec.to_string()).unwrap();
    };
    // 1. The supervisor is gone, and so is the worker: the pid is the
    //    bystander's now, started at another time.
    let reused = format!("safe.{}.json", gone.id());
    write(
        reused.clone(),
        member(gone.id(), 1, ""),
        vec![member(bystander.id(), start_ticks(bystander.id() as u64) + 1, "1")],
    );
    // 2. The supervisor is alive (another one of the app), and its worker is
    //    exactly the process recorded.
    let busy = format!("safe.{}.json", live_sup.id());
    write(
        busy.clone(),
        member(live_sup.id(), start_ticks(live_sup.id() as u64), ""),
        vec![member(live_worker.id(), start_ticks(live_worker.id() as u64), "1")],
    );

    let env = [("WARDEN_TEST_MACOS_ORPHANS", "1"), ("WARDEN_KEEPER", "0")];
    let (code, out) = f.cli_env(&["start", "sleep 300", "--name", "safe"], &env);
    assert_eq!(code, 0, "{out}");
    f.wait("the worker", |f| f.pids("safe").len() == 1);
    for (name, c) in
        [("bystander", &mut bystander), ("live supervisor", &mut live_sup), ("its worker", &mut live_worker)]
    {
        assert!(c.try_wait().unwrap().is_none() && running(c.id() as u64), "the {name} was left alone");
    }
    assert!(!rec_dir.join(&reused).exists(), "a record with nothing left to stop is dropped");
    assert!(rec_dir.join(&busy).exists(), "the record of a live supervisor is kept");
    let log_file = f.home.join("state/logs/safe.log");
    wait_until("the log to name the other supervisor", || {
        std::fs::read_to_string(&log_file).is_ok_and(|l| l.contains("another supervisor of this app is running"))
    });
    let log = std::fs::read_to_string(&log_file).unwrap();
    assert!(!log.contains("workers of a previous supervisor"), "{log}");
    every_warning_has_a_hint(&log);
    for mut c in [bystander, live_sup, live_worker] {
        let _ = c.kill();
        let _ = c.wait();
    }
}

// ----------------------------------------------- the sweep while the supervisor runs

/// Two workers that ignore SIGTERM (so the sweep takes `grace_period` to end),
/// whose supervisor was killed with SIGKILL. Then the app's config says
/// `ready_timeout` and `grace_period`, and the workers it will start next obey
/// SIGTERM. Returns the old workers' pids and the dead supervisor's.
fn app_with_stubborn_orphans(f: &Fleet, name: &str, tag: &str, ready_timeout: u64, grace: u64) -> (Vec<u64>, u64) {
    let hook = [("WARDEN_TEST_MACOS_ORPHANS", "1"), ("WARDEN_KEEPER", "0")];
    let cmd = format!("trap '' TERM; exec sleep {tag}");
    let start = ["start", "/bin/sh", "--name", name, "--interpreter", "none", "-i", "2", "--", "-c", &cmd];
    let (code, out) = f.cli_env(&start, &hook);
    assert_eq!(code, 0, "{out}");
    f.wait("two workers", |f| f.pids(name).len() == 2);
    let old = f.pids(name);
    let sup = supervisor_pid(f, name);
    unsafe { libc::kill(sup as i32, libc::SIGKILL) };
    wait_until("the supervisor to be gone", || !running(sup));
    std::thread::sleep(Duration::from_millis(300));
    assert!(old.iter().all(|p| running(*p)), "the workers outlive the killed supervisor: {old:?}");
    // The next supervisor reads the config afresh: its workers obey SIGTERM.
    let path = f.home.join(format!("{name}.toml"));
    let cfg = std::fs::read_to_string(&path).unwrap();
    let (was, now) = ("[workers]\ncount = 2\n", format!("[workers]\ncount = 2\nready_timeout = {ready_timeout}\n"));
    assert!(cfg.contains(was) && cfg.contains("trap '' TERM; "), "{cfg}");
    let cfg = cfg.replace(was, &now).replace("trap '' TERM; ", "");
    std::fs::write(&path, format!("{cfg}\n[shutdown]\ngrace_period = {grace}\n")).unwrap();
    (old, sup)
}

/// `warden start <name>` in the background, its output in a file.
fn start_in_the_background(f: &Fleet, name: &str) -> (Child, PathBuf) {
    let out = f.home.join(format!("start-{name}.out"));
    let file = std::fs::File::create(&out).unwrap();
    let child = warden()
        .args(["start", name])
        .env("WARDEN_HOME", &f.home)
        .env("WARDEN_RUNTIME_DIR", f.home.join("run"))
        .env_remove("WARDEN_CONFIG")
        .env("WARDEN_NO_DAEMON", "1")
        .env("WARDEN_TEST_MACOS_ORPHANS", "1")
        .env("WARDEN_KEEPER", "0")
        .current_dir(&f.home)
        .stdout(file.try_clone().unwrap())
        .stderr(file)
        .spawn()
        .unwrap();
    (child, out)
}

/// While a supervisor stops the workers a killed one left behind (up to
/// `grace_period` + 3 s when they ignore the stop signal) it answers: `status`
/// says what it waits for, and a SIGTERM ends it at once, without starting a
/// worker only to stop it (it used to handle nothing until the sweep ended,
/// then start workers, and only then see the SIGTERM).
#[test]
fn a_supervisor_told_to_stop_during_the_sweep_stops_at_once_and_starts_no_worker() {
    let tag = sleep_tag(2);
    let _sleepers = Sleepers(tag.clone());
    let f = Fleet::new("sweep-term");
    let (old, dead) = app_with_stubborn_orphans(&f, "slow", &tag, 30, 12);
    let (mut cli, out) = start_in_the_background(&f, "slow");

    f.wait("the sweep to show in status", |f| f.app("slow")["status"]["rollout"]["kind"] == "sweep");
    let status = f.app("slow")["status"].clone();
    let sweep = &status["rollout"];
    assert_eq!((sweep["done"].as_u64(), sweep["total"].as_u64()), (Some(0), Some(2)), "{status:#}");
    let phase = sweep["phase"].as_str().unwrap();
    assert!(phase.contains("waiting for the previous supervisor's workers to stop (up to 15 s)"), "{phase}");
    assert_eq!(status["workers"].as_array().map(Vec::len), Some(0), "no worker started: {status:#}");
    // `warden status` and `warden list` show it, instead of an app that does not answer.
    let text = f.ok(&["status", "slow"]);
    assert!(text.contains("sweep 0/2") && text.contains("waiting for the previous supervisor's workers"), "{text}");
    let text = f.ok(&["list"]);
    assert!(text.contains("sweep 0/2") && text.contains("waiting for the previous supervisor's workers"), "{text}");
    // Requests that would start or change workers wait for it; the CLI says so.
    let (code, text) = f.cli(&["reload", "slow"]);
    assert_ne!(code, 0, "{text}");
    assert!(text.contains("try again then"), "{text}");
    // `warden start` printed what it waits for.
    wait_until("the CLI to say what it waits for", || {
        std::fs::read_to_string(&out).is_ok_and(|t| t.contains("waiting for the previous supervisor's workers"))
    });
    assert!(old.iter().all(|p| running(*p)), "SIGTERM is ignored by them: still there");

    let sup = supervisor_pid(&f, "slow");
    assert_ne!(sup, dead);
    let t0 = Instant::now();
    unsafe { libc::kill(sup as i32, libc::SIGTERM) };
    wait_until("the supervisor to stop", || !running(sup));
    let took = t0.elapsed();
    assert!(took < Duration::from_secs(3), "stopped in {took:?}, not after the sweep (12 s)");
    // The log has the killed supervisor's lines first: this one's start at its own `starting application`.
    let log = std::fs::read_to_string(f.home.join("state/logs/slow.log")).unwrap();
    let log = &log[log.rfind("starting application").expect("this supervisor's start")..];
    assert!(log.contains("shutting down reason=SIGTERM workers=0"), "{log}");
    assert!(log.contains("stopped while waiting for the previous supervisor's workers to stop"), "{log}");
    assert!(!log.contains("worker starting") && !log.contains("standby starting"), "no worker was started:\n{log}");
    assert!(!log.contains("event loop was blocked"), "the loop was never blocked:\n{log}");
    every_warning_has_a_hint(log);
    // The records stay, for the next start to finish what this one did not.
    assert!(f.home.join(format!("state/orphans/slow.{dead}.json")).exists());
    // The CLI that waited says why it stops waiting.
    let t1 = Instant::now();
    let status = loop {
        if let Some(st) = cli.try_wait().unwrap() {
            break st;
        }
        assert!(t1.elapsed() < Duration::from_secs(5), "warden start did not give up");
        std::thread::sleep(Duration::from_millis(50));
    };
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(status.code(), Some(1), "{text}");
    assert!(text.contains("the supervisor stopped while it was waiting"), "{text}");
}

/// `warden start` waits for a sweep longer than its own bound for the first
/// worker (`ready_timeout` + 15 s): the wait begins when the sweep is over.
/// It reported "not answering" and exited 1 while the app came up fine
/// right after.
#[test]
fn warden_start_waits_for_a_sweep_that_outlasts_ready_timeout() {
    let tag = sleep_tag(3);
    let _sleepers = Sleepers(tag.clone());
    let f = Fleet::new("sweep-wait");
    // ready_wait = 2 + 15 = 17 s; the old workers die of SIGKILL at 18 s.
    let (old, dead) = app_with_stubborn_orphans(&f, "slowboot", &tag, 2, 18);
    let t0 = Instant::now();
    let (code, out) = f.cli_env(&["start", "slowboot"], &[("WARDEN_TEST_MACOS_ORPHANS", "1"), ("WARDEN_KEEPER", "0")]);
    let took = t0.elapsed();
    assert_eq!(code, 0, "{out}");
    assert!(took > Duration::from_secs(17), "the sweep outlasted the 17 s bound: {took:?}");
    assert!(out.contains("waiting for the previous supervisor's workers to stop"), "{out}");
    assert!(out.contains("slowboot: online (2/2 workers ready)"), "{out}");
    let now = f.pids("slowboot");
    assert!(now.len() == 2 && now.iter().all(|p| !old.contains(p)), "{old:?} -> {now:?}");
    assert!(old.iter().all(|p| !running(*p)), "the old workers are gone");
    let log = std::fs::read_to_string(f.home.join("state/logs/slowboot.log")).unwrap();
    assert!(log.contains("did not exit within the grace period; sent SIGKILL"), "{log}");
    assert!(log.contains("stopped workers left behind by a previous supervisor"), "{log}");
    every_warning_has_a_hint(&log);
    assert!(!f.home.join(format!("state/orphans/slowboot.{dead}.json")).exists());
}

/// A static site made by `warden start` keeps the compressed copies it makes
/// in the background in Warden's state folder (one per app), never in the
/// folder it serves, and they are served: the default `compress_dir`.
#[test]
fn a_static_site_keeps_its_compressed_copies_in_the_state_folder() {
    let base = tmp().join(format!("warden-it-copies-site-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (site, home) = (base.join("site"), base.join("home"));
    std::fs::create_dir_all(&site).unwrap();
    let css: String =
        (0..1000).map(|i| format!(".rule-{i}{{color:#{:06x};margin:{}px}}\n", i * 7919 % 0xff_ffff, i % 40)).collect();
    std::fs::write(site.join("app.css"), &css).unwrap();
    let port = free_port();
    let toml = format!(
        "[app]\nname = \"copies\"\nport = {port}\n[workers]\ncount = 1\n[static]\nroot = \"{}\"\n",
        site.display()
    );
    let w = Warden::start_env("static-copies", port, &toml, &[("WARDEN_HOME", home.to_str().unwrap())]);
    w.wait_for("static worker ready", T, ready(1));
    let t0 = Instant::now();
    let copy = loop {
        let (status, h, body) = get_close(port, "/app.css", "Accept-Encoding: br\r\n");
        assert_eq!(status, 200);
        if h.get("content-encoding").map(String::as_str) == Some("br") {
            break (h, body);
        }
        assert_eq!(body, css.as_bytes(), "until the copy is made, the file itself");
        assert!(t0.elapsed() < Duration::from_secs(90), "no compressed copy after {:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(200));
    };
    assert!(copy.1.len() < css.len() / 2);
    let store = home.join("state/compress/copies");
    let files: Vec<_> = std::fs::read_dir(&store).unwrap().flatten().collect();
    assert!(files.iter().any(|f| f.path().is_dir()), "the copies are in {}", store.display());
    assert_eq!(std::fs::read_dir(&site).unwrap().count(), 1, "the served folder is as it was");
    drop(w);
    let _ = std::fs::remove_dir_all(&base);
}

/// `warden save` with nothing running keeps the list saved before (an update that stopped
/// halfway, run again, must not forget every app), and a save keeps the list it replaces.
#[test]
fn an_empty_save_keeps_the_saved_apps() {
    let f = Fleet::new("save-keeps");
    sleeper_config(&f, "one");
    sleeper_config(&f, "two");
    f.ok(&["start", "all"]);
    f.ok(&["save"]);
    let dump = f.home.join("state/dump.json");
    let saved = std::fs::read_to_string(&dump).unwrap();
    f.ok(&["kill", "--yes"]);
    f.wait("all offline", |f| f.list().iter().all(|a| a["status"].is_null()));

    let out = f.ok(&["save"]);
    assert!(out.contains("nothing was saved") && out.contains("one, two"), "{out}");
    assert_eq!(std::fs::read_to_string(&dump).unwrap(), saved, "the saved list stays");

    f.ok(&["start", "one"]);
    f.ok(&["save"]);
    assert_eq!(std::fs::read_to_string(f.home.join("state/dump.json.bak")).unwrap(), saved, "one save back");
    let out = f.ok(&["resurrect"]);
    assert!(!out.contains("two:"), "two was not running at the last save: {out}");
}

/// A launchd job an older `warden startup` wrote names a warden that is gone (or the old
/// `daemon` command): launchd retries it forever, so `warden start` must not wait for it to
/// bring wardend back. It says so and starts wardend itself.
#[test]
fn a_stale_launchd_job_is_not_waited_for() {
    let f = Fleet::new("st-stale-job");
    sleeper_config(&f, "api");
    let fakes = Fakes::new(&f, &[]);
    std::fs::write(
        f.home.join("launchd/io.github.oceanwap.warden.daemon.plist"),
        "<plist><dict><key>ProgramArguments</key><array><string>/nonexistent/warden</string>\
         <string>daemon</string><string>--resurrect</string></array></dict></plist>",
    )
    .unwrap();
    let mut env = fakes.launchd_env();
    env.push(("WARDEN_NO_DAEMON".into(), "0".into()));
    let t0 = Instant::now();
    let (code, out) = run_with(&f, &["start", "api"], &env);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("/nonexistent/warden does not exist") && out.contains("`warden startup` writes it again"),
        "{out}"
    );
    assert!(out.contains("wardend started in the background"), "{out}");
    assert!(!fakes.take().contains("kickstart"), "the stale job is not kickstarted");
    assert!(t0.elapsed() < Duration::from_secs(10), "no 10 s wait for launchd: {:?}", t0.elapsed());
    assert_eq!(run_with(&f, &["wardend", "status"], &env).0, 0, "wardend answers");
    let _ = run_with(&f, &["kill", "--yes"], &env);
}

/// A TLS ClientHello asking for `host`, as far as the router reads it.
fn client_hello(host: &str) -> Vec<u8> {
    let n = host.len() as u16;
    let mut ext = vec![0x00, 0x00];
    ext.extend_from_slice(&(n + 5).to_be_bytes());
    ext.extend_from_slice(&(n + 3).to_be_bytes());
    ext.push(0);
    ext.extend_from_slice(&n.to_be_bytes());
    ext.extend_from_slice(host.as_bytes());
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[7u8; 32]);
    body.push(0); // no session id
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01, 0x01, 0x00]);
    body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
    body.extend_from_slice(&ext);
    let mut hs = vec![0x01, 0x00];
    hs.extend_from_slice(&(body.len() as u16).to_be_bytes());
    hs.extend_from_slice(&body);
    let mut rec = vec![0x16, 0x03, 0x01];
    rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
    rec.extend_from_slice(&hs);
    rec
}

/// A backend that answers each connection with its tag, then echoes.
fn tagged_echo(tag: &'static str) -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for c in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut c = c;
                let _ = c.write_all(tag.as_bytes());
                let mut b = [0u8; 65536];
                while let Ok(n) = c.read(&mut b) {
                    if n == 0 || c.write_all(&b[..n]).is_err() {
                        break;
                    }
                }
            });
        }
    });
    port
}

/// `[route]`: each connection goes, by the hostname in its ClientHello, to
/// that app's port, with the hello and every byte after it passed on as is
/// (and a large transfer, through the splice path, intact).
#[test]
fn the_router_sends_each_hostname_to_its_app() {
    let (a, b) = (tagged_echo("A:"), tagged_echo("B:"));
    let port = free_port();
    let cfg = format!(
        "[app]\nname = \"edge\"\nport = {port}\n[workers]\ncount = 2\n\
         [route]\nhost = \"127.0.0.1\"\nclient_ip = false\nhosts = {{ \"a.test\" = {a}, \"*.b.test\" = {b} }}\n"
    );
    let w = Warden::start("route", port, &cfg);
    w.wait_for("ready", T, ready(2));
    let talk = |host: &str, extra: &[u8]| -> Vec<u8> {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut sent = client_hello(host);
        sent.extend_from_slice(extra);
        // Written from another thread: the echo comes back while we write.
        let mut writer = c.try_clone().unwrap();
        let out = sent.clone();
        let t = std::thread::spawn(move || writer.write_all(&out).unwrap());
        let mut got = Vec::new();
        let mut b = [0u8; 65536];
        while got.len() < 2 + sent.len() {
            match c.read(&mut b) {
                Ok(0) | Err(_) => break,
                Ok(n) => got.extend_from_slice(&b[..n]),
            }
        }
        t.join().unwrap();
        assert_eq!(&got[2.min(got.len())..], &sent[..], "every byte passed on as sent");
        got
    };
    assert!(talk("a.test", b"ping").starts_with(b"A:"));
    assert!(talk("www.b.test", b"ping").starts_with(b"B:"));
    assert!(talk("A.TEST", b"").starts_with(b"A:"), "hostnames are matched in lowercase");
    let big: Vec<u8> = (0..4u32 << 20).map(|i| (i % 251) as u8).collect();
    talk("a.test", &big);
    // No route for the name: the connection is closed, nothing is sent.
    let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    c.write_all(&client_hello("c.test")).unwrap();
    let mut b = [0u8; 16];
    assert_eq!(c.read(&mut b).unwrap_or(0), 0);
}

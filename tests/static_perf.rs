//! Static file serving: the speed guards, `html_max_age`, and the dates and
//! validators of responses (RFC 9110).
//!
//! Every test runs the real worker (`warden serve-static`, the process
//! `warden start` runs for a `[static]` site) on a loopback port, with the
//! environment the supervisor gives it. Set `WARDEN_PERF_BIN` to run them
//! against another build: a guard is only worth having if it fails when the
//! thing it guards is broken, and an older release shows that.
//!
//! The speed guards avoid wall-clock thresholds, which a loaded CI machine
//! makes flaky. They count what the kernel counts: data segments on the wire,
//! threads, context switches, system calls (when `strace` is installed) and
//! CPU time against a trivial server in the same process, best of several
//! rounds. The counts are exact and their limits tight (one system call more
//! per request fails); the ratios are a coarse net with room for a loaded
//! machine. Linux only (they read /proc and TCP_INFO); the behaviour tests
//! and the fairness test (a big download next to small requests) run
//! everywhere. The tests that need `strace` print `SKIPPED ...` on stderr
//! when it is missing or cannot attach, and fail instead when
//! `WARDEN_REQUIRE_STRACE=1` is set (a CI job that installs it should).

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where tests put their files. macOS's `$TMPDIR` (/var/folders/...) is too
/// long for the Unix socket paths under it (104 bytes at most), so /tmp there.
fn tmp() -> std::path::PathBuf {
    if cfg!(target_os = "macos") { std::path::PathBuf::from("/tmp") } else { std::env::temp_dir() }
}

const BIN: &str = env!("CARGO_BIN_EXE_warden");

fn bin() -> String {
    std::env::var("WARDEN_PERF_BIN").unwrap_or_else(|_| BIN.to_string())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The byte at `i` of the test files: a pattern that shows any shifted,
/// repeated or missing range.
fn pattern(n: usize) -> Vec<u8> {
    (0..n as u32).map(|i| (i % 251) as u8).collect()
}

/// A scratch directory, removed on drop.
struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        let dir = tmp().join(format!("warden-perf-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Tmp(dir)
    }

    fn site(&self) -> PathBuf {
        self.0.join("site")
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(site: &Path, rel: &str, bytes: &[u8]) {
    let p = site.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, bytes).unwrap();
}

/// A site with a page of each kind, a few sizes and an error page.
fn make_site(tmp: &Tmp) -> PathBuf {
    let site = tmp.site();
    write(&site, "index.html", b"<h1>home</h1>");
    write(&site, "page.html", b"<h1>page</h1>");
    write(&site, "sub/index.html", b"<h1>sub</h1>");
    write(&site, "404.html", b"<h1>nothing here</h1>");
    write(&site, "style.css", b"a{}");
    write(&site, "app.3f9a2c1b.js", b"console.log(1)");
    write(&site, "small.bin", &pattern(1_000));
    write(&site, "mid10k.bin", &pattern(10_000));
    write(&site, "mid12k.bin", &pattern(12_000));
    write(&site, "mid20k.bin", &pattern(20_000));
    write(&site, "big.bin", &pattern(3_000_000));
    site
}

/// The response cache only takes files that have not changed for 2 s (a
/// change within the clock's granularity could go unnoticed). Wait that
/// out, so the second request for a file is a cache hit.
fn quiet() {
    std::thread::sleep(Duration::from_millis(2200));
}

/// `warden serve-static` on its own port.
struct Worker {
    child: Child,
    port: u16,
    log: PathBuf,
}

impl Worker {
    /// `extra`: keys of the `[static]` section (`root` and `host` are set).
    fn start(tmp: &Tmp, root: &Path, extra: Value) -> Worker {
        Worker::start_env(tmp, root, extra, &[])
    }

    /// The same, with more environment variables for the worker.
    fn start_env(tmp: &Tmp, root: &Path, extra: Value, env: &[(&str, &str)]) -> Worker {
        let port = free_port();
        let mut cfg = json!({ "root": root, "host": "127.0.0.1" });
        for (k, v) in extra.as_object().unwrap() {
            cfg[k] = v.clone();
        }
        let log_path = tmp.0.join(format!("worker-{port}.log"));
        let log = std::fs::File::create(&log_path).unwrap();
        let child = Command::new(bin())
            .arg("serve-static")
            .env("WARDEN_STATIC", cfg.to_string())
            .env("PORT", port.to_string())
            // What the supervisor always sets: the worker's liveness timer.
            .env("WARDEN_HEARTBEAT_MS", "1000")
            .env("WARDEN_DRAIN_MS", "500")
            .envs(env.iter().copied())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(log)
            .spawn()
            .unwrap();
        let mut w = Worker { child, port, log: log_path };
        let t0 = Instant::now();
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(t0.elapsed() < Duration::from_secs(10), "the worker never listened on {port}");
            if let Some(st) = w.child.try_wait().unwrap() {
                panic!("the worker exited: {st}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        w
    }

    #[cfg(target_os = "linux")]
    fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Ask it to stop (SIGTERM) and return what it printed, which ends with
    /// its cache summary.
    fn stop(mut self) -> String {
        // SAFETY: plain kill(2) of a child this test spawned.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let t0 = Instant::now();
        while self.child.try_wait().unwrap().is_none() {
            assert!(t0.elapsed() < Duration::from_secs(10), "the worker did not stop");
            std::thread::sleep(Duration::from_millis(20));
        }
        std::fs::read_to_string(&self.log).unwrap()
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Resp {
    status: u16,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl Resp {
    /// A header's value ("" when absent); `name` in lowercase.
    fn h(&self, name: &str) -> &str {
        self.headers.get(name).map(String::as_str).unwrap_or("")
    }
}

/// One keep-alive connection that reads responses one by one, so pipelined
/// responses (and stray bytes after one) show.
struct Client {
    s: TcpStream,
    buf: Vec<u8>,
}

impl Client {
    fn connect(port: u16) -> Client {
        let s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_nodelay(true).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        Client { s, buf: Vec::new() }
    }

    fn send(&mut self, req: &str) {
        self.s.write_all(req.as_bytes()).unwrap();
    }

    fn fill(&mut self) {
        let mut chunk = vec![0u8; 64 * 1024];
        let n = self.s.read(&mut chunk).unwrap_or(0);
        assert!(
            n > 0,
            "the connection ended early; {} bytes pending: {:?}",
            self.buf.len(),
            String::from_utf8_lossy(&self.buf)
        );
        self.buf.extend_from_slice(&chunk[..n]);
    }

    /// The next response; `head_only` for the answer to a HEAD request.
    fn recv(&mut self, head_only: bool) -> Resp {
        let end = loop {
            if let Some(p) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p;
            }
            self.fill();
        };
        let head = String::from_utf8_lossy(&self.buf[..end]).to_string();
        self.buf.drain(..end + 4);
        let mut lines = head.lines();
        let status: u16 = lines.next().unwrap().split(' ').nth(1).unwrap().parse().unwrap();
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let len: usize = if head_only || status == 304 || status == 204 {
            0
        } else {
            headers.get("content-length").map_or(0, |v| v.parse().unwrap())
        };
        while self.buf.len() < len {
            self.fill();
        }
        let body = self.buf.drain(..len).collect();
        Resp { status, headers, body }
    }

    fn get(&mut self, path: &str, extra: &str) -> Resp {
        self.send(&format!("GET {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n"));
        self.recv(false)
    }

    fn head(&mut self, path: &str, extra: &str) -> Resp {
        self.send(&format!("HEAD {path} HTTP/1.1\r\nHost: x\r\n{extra}\r\n"));
        self.recv(true)
    }
}

// ---------------------------------------------------------------------------
// html_max_age
// ---------------------------------------------------------------------------

const JS_FINGERPRINTED: &str = "public, max-age=31536000, immutable";

/// (path, status, Cache-Control) of each kind of file, asked twice on one
/// connection: the second answer comes from the cache when there is one, and
/// must say the same as the first.
fn policies(w: &Worker, extra: &str) -> Vec<(&'static str, u16, String)> {
    let paths = ["/", "/page.html", "/sub/", "/missing", "/style.css", "/app.3f9a2c1b.js"];
    let mut c = Client::connect(w.port);
    let mut rounds = Vec::new();
    for _ in 0..2 {
        rounds.push(
            paths
                .iter()
                .map(|p| {
                    let r = c.get(p, extra);
                    (*p, r.status, r.h("cache-control").to_string())
                })
                .collect::<Vec<_>>(),
        );
    }
    assert_eq!(rounds[0], rounds[1], "a cache hit must say what the first answer said");
    rounds.remove(0)
}

fn expect(got: &[(&str, u16, String)], pages: &str, css: &str) {
    expect_all(got, pages, css, JS_FINGERPRINTED);
}

fn expect_all(got: &[(&str, u16, String)], pages: &str, css: &str, js: &str) {
    let want: Vec<(&str, u16, String)> = vec![
        ("/", 200, pages.to_string()),
        ("/page.html", 200, pages.to_string()),
        ("/sub/", 200, pages.to_string()),
        // The error page is never kept: a 404 must not outlive the fix.
        ("/missing", 404, "no-cache".to_string()),
        ("/style.css", 200, css.to_string()),
        ("/app.3f9a2c1b.js", 200, js.to_string()),
    ];
    assert_eq!(got, want.as_slice());
}

#[test]
fn html_pages_follow_html_max_age_and_other_files_do_not() {
    let tmp = Tmp::new("html-age");
    let site = make_site(&tmp);
    quiet();

    // Unset (the default) and 0: ask again on every page load, as before;
    // the other files too (`cache_max_age` unset). With and without the cache.
    for cache in ["0", "16MB"] {
        for extra in [json!({ "cache_size": cache }), json!({ "html_max_age": 0, "cache_size": cache })] {
            let w = Worker::start(&tmp, &site, extra);
            expect(&policies(&w, ""), "no-cache", "no-cache");
        }
    }

    // Set: pages (the index, other .html files, a directory's index) are
    // reused for that long; nothing else changes. With and without the cache.
    for cache in ["16MB", "0"] {
        let w = Worker::start(&tmp, &site, json!({ "html_max_age": 120, "cache_size": cache, "cache_max_age": 3600 }));
        let got = policies(&w, "");
        expect(&got, "public, max-age=120", "public, max-age=3600");

        // A conditional request is answered with the same policy, and a HEAD too.
        let mut c = Client::connect(w.port);
        let full = c.get("/", "");
        let etag = full.h("etag").to_string();
        assert!(!etag.is_empty());
        let nm = c.get("/", &format!("If-None-Match: {etag}\r\n"));
        assert_eq!((nm.status, nm.h("cache-control")), (304, "public, max-age=120"), "cache_size {cache}");
        assert!(nm.body.is_empty());
        let head = c.head("/", "");
        assert_eq!((head.status, head.h("cache-control")), (200, "public, max-age=120"));
    }

    // The file cache's own setting is independent of it.
    let w = Worker::start(&tmp, &site, json!({ "cache_max_age": 10 }));
    expect(&policies(&w, ""), "no-cache", "public, max-age=10");
    let w = Worker::start(&tmp, &site, json!({ "cache_max_age": 10, "html_max_age": 300 }));
    expect(&policies(&w, ""), "public, max-age=300", "public, max-age=10");

    // Behind a password a shared cache must not keep the page: `private`.
    let w = Worker::start(&tmp, &site, json!({ "html_max_age": 60, "basic_auth": "u:p" }));
    let mut c = Client::connect(w.port);
    let denied = c.get("/", "");
    assert_eq!(denied.status, 401);
    assert_eq!(denied.h("cache-control"), "", "the refusal says nothing about caching");
    let got = policies(&w, "Authorization: Basic dTpw\r\n");
    let private_js = "private, max-age=31536000, immutable";
    expect_all(&got, "private, max-age=60", "no-cache", private_js);
    // And no file: what may be kept, may be kept by the browser only.
    let w = Worker::start(&tmp, &site, json!({ "cache_max_age": 3600, "basic_auth": "u:p" }));
    let got = policies(&w, "Authorization: Basic dTpw\r\n");
    expect_all(&got, "no-cache", "private, max-age=3600", private_js);
}

#[test]
fn the_single_page_app_fallback_is_a_page_too() {
    let tmp = Tmp::new("html-spa");
    let site = make_site(&tmp);
    for (extra, want) in
        [(json!({ "spa": true }), "no-cache"), (json!({ "spa": true, "html_max_age": 45 }), "public, max-age=45")]
    {
        let w = Worker::start(&tmp, &site, extra);
        let mut c = Client::connect(w.port);
        let r = c.get("/app/route", "Accept: text/html\r\n");
        assert_eq!((r.status, r.body.as_slice()), (200, b"<h1>home</h1>".as_slice()));
        assert_eq!(r.h("cache-control"), want);
        // A missing file with an extension is still a 404 (no-cache), not the app.
        let r = c.get("/missing.png", "Accept: image/png\r\n");
        assert_eq!((r.status, r.h("cache-control")), (404, "no-cache"));
    }
}

/// A stopped-and-restarted `warden start` with a config file; `warden check`
/// says what is wrong with a bad value.
struct Started {
    child: Child,
    port: u16,
}

impl Drop for Started {
    fn drop(&mut self) {
        // SAFETY: plain kill(2) of a child this test spawned.
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

fn config(tmp: &Tmp, port: u16, site: &Path, static_extra: &str) -> PathBuf {
    let cfg = tmp.0.join(format!("warden-{port}.toml"));
    std::fs::write(
        &cfg,
        format!(
            "[app]\nname = \"perf-html\"\nport = {port}\n[workers]\ncount = 1\n\
             [static]\nroot = \"{}\"\nhost = \"127.0.0.1\"\n{static_extra}\n\
             [control]\nsocket = \"{}\"\n",
            site.display(),
            tmp.0.join(format!("w-{port}.sock")).display()
        ),
    )
    .unwrap();
    cfg
}

#[test]
fn html_max_age_comes_from_the_config_file_and_is_bounded() {
    let tmp = Tmp::new("html-config");
    let site = make_site(&tmp);

    // `warden check` accepts the range and names the key it refuses.
    let check = |value: &str| {
        let cfg = config(&tmp, 1, &site, &format!("html_max_age = {value}"));
        let out = Command::new(bin()).args(["check", "-c"]).arg(&cfg).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr))
    };
    for ok in ["0", "60", "31536000"] {
        let (good, text) = check(ok);
        assert!(good, "html_max_age = {ok} should be accepted:\n{text}");
    }
    let (good, text) = check("31536001");
    assert!(!good, "a value above a year should be refused:\n{text}");
    assert!(text.contains("static.html_max_age") && text.contains("31536000"), "{text}");
    let (good, text) = check("-5");
    assert!(!good && text.contains("html_max_age"), "{text}");

    // The running worker gets it from the file.
    let port = free_port();
    let cfg = config(&tmp, port, &site, "html_max_age = 90");
    let log = std::fs::File::create(tmp.0.join("warden.log")).unwrap();
    let child = Command::new(bin())
        .args(["start", "-c"])
        .arg(&cfg)
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(log)
        .spawn()
        .unwrap();
    let started = Started { child, port };
    let t0 = Instant::now();
    let cache_control = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", started.port)) {
            drop(s);
            let mut c = Client::connect(started.port);
            let r = c.get("/", "");
            if r.status == 200 {
                break r.h("cache-control").to_string();
            }
        }
        assert!(
            t0.elapsed() < Duration::from_secs(20),
            "the site never came up:\n{}",
            std::fs::read_to_string(tmp.0.join("warden.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(cache_control, "public, max-age=90");
    let css = Client::connect(started.port).get("/style.css", "");
    assert_eq!(css.h("cache-control"), "no-cache");
}

// ---------------------------------------------------------------------------
// Correct framing: HEAD, pipelining, large and cached bodies
// ---------------------------------------------------------------------------

#[test]
fn head_and_pipelined_requests_get_one_exact_response_each() {
    let tmp = Tmp::new("framing");
    // No 404.html here: the plain error responses are the ones under test.
    let site = tmp.site();
    write(&site, "index.html", b"<h1>home</h1>");
    write(&site, "empty/.keep", b"");
    write(&site, "empty/a.txt", b"a");
    let w = Worker::start(&tmp, &site, json!({ "listing": true }));

    // A HEAD answer is the head alone, error and listing responses included;
    // the next pipelined response must follow it directly.
    let mut c = Client::connect(w.port);
    c.send(
        "GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n\
         HEAD /nothing HTTP/1.1\r\nHost: x\r\n\r\n\
         GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n\
         HEAD /empty/ HTTP/1.1\r\nHost: x\r\n\r\n\
         GET /nothing HTTP/1.1\r\nHost: x\r\n\r\n\
         HEAD /index.html HTTP/1.1\r\nHost: x\r\n\r\n\
         GET /index.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
    );
    let home = b"<h1>home</h1>".as_slice();
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice()), (200, home));
    let r = c.recv(true);
    assert_eq!(r.status, 404);
    assert_eq!(r.h("content-length"), "14", "the head still says how long the body would be");
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice()), (200, home));
    let r = c.recv(true);
    assert_eq!((r.status, r.h("content-type")), (200, "text/html; charset=utf-8"));
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice()), (404, b"404 Not Found\n".as_slice()));
    let r = c.recv(true);
    assert_eq!((r.status, r.h("content-length")), (200, "13"));
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice(), r.h("connection")), (200, home, "close"));
    let mut rest = Vec::new();
    let _ = c.s.read_to_end(&mut rest);
    assert!(
        c.buf.is_empty() && rest.is_empty(),
        "stray bytes after the last response: {:?}",
        String::from_utf8_lossy(&rest)
    );

    // A refused path or method, asked by HEAD, ends the exchange the same way.
    for (req, status) in [
        ("HEAD /%2e%2e/x HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", 403),
        ("HEAD /nothing HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n", 404),
    ] {
        let mut c = Client::connect(w.port);
        c.send(req);
        assert_eq!(c.recv(true).status, status);
        let mut rest = Vec::new();
        let _ = c.s.read_to_end(&mut rest);
        assert!(c.buf.is_empty() && rest.is_empty(), "{req:?}: a HEAD answer has no body: {rest:?}");
    }
    let mut c = Client::connect(w.port);
    c.send("POST /index.html HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n");
    let r = c.recv(false);
    assert_eq!((r.status, r.h("allow")), (405, "GET, HEAD"));

    // The same behind a password.
    let w = Worker::start(&tmp, &site, json!({ "basic_auth": "u:p" }));
    let mut c = Client::connect(w.port);
    c.send(
        "HEAD / HTTP/1.1\r\nHost: x\r\n\r\nGET / HTTP/1.1\r\nHost: x\r\n\r\n\
         GET / HTTP/1.1\r\nHost: x\r\nAuthorization: Basic dTpw\r\n\r\n",
    );
    let r = c.recv(true);
    assert_eq!((r.status, r.h("www-authenticate")), (401, "Basic realm=\"warden\""));
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice()), (401, b"401 Unauthorized\n".as_slice()));
    let r = c.recv(false);
    assert_eq!((r.status, r.body.as_slice()), (200, home));
}

/// Everything a client sends on one connection, `pause` apart, and everything
/// the server sends back until it closes (the last request says
/// `Connection: close`, or is HTTP/1.0).
/// `raw` as text, with the value of each Date header (when the response was
/// made: two answers a second apart differ there and nowhere else) checked
/// to be an HTTP date and replaced by a fixed one.
fn undated(raw: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(raw).into_owned();
    let mut from = 0;
    while let Some(i) = s[from..].find("\r\nDate: ") {
        let at = from + i + 8;
        let date = s.get(at..at + 29).unwrap_or_default();
        assert!(date.ends_with(" GMT") && date.as_bytes()[3] == b',', "an HTTP date: {date:?}");
        s.replace_range(at..at + 29, "Sun, 06 Nov 1994 08:49:37 GMT");
        from = at + 29;
    }
    s
}

fn exchange(port: u16, segments: &[&str], pause: Duration) -> Vec<u8> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_nodelay(true).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    for (i, seg) in segments.iter().enumerate() {
        if i > 0 {
            std::thread::sleep(pause);
        }
        s.write_all(seg.as_bytes()).unwrap();
    }
    let mut out = Vec::new();
    s.read_to_end(&mut out).expect("the server closes the connection after the last response");
    out
}

/// A new connection whose request is already in the socket is answered from
/// the accept loop (`first_request`), with no task and no epoll registration,
/// when the file can be had without waiting (and, with a cache, when it is a
/// hit). The bytes must be exactly what the normal path sends
/// (`WARDEN_STATIC_INLINE=0`), whatever the request, however it arrives.
#[test]
fn a_request_answered_from_the_accept_loop_matches_the_normal_path() {
    accept_loop_matches_normal_path(false);
}

/// The same with the response cache on (`cache_size`).
#[test]
fn a_cache_hit_answered_from_the_accept_loop_matches_the_normal_path() {
    accept_loop_matches_normal_path(true);
}

fn accept_loop_matches_normal_path(cache: bool) {
    let tmp = Tmp::new(if cache { "inline-cache" } else { "inline" });
    let site = make_site(&tmp);
    write(&site, "gz.js", b"console.log('plain')");
    write(&site, "gz.js.gz", b"not really gzip, but a sibling");
    write(&site, "sp ace.txt", b"percent-encoded in the URL");
    quiet();
    // With a cache, entries stay fresh for the whole test (the default is
    // 1 s, after which a hit takes the normal path to be checked against the
    // disk): every case below is a hit, and on the first worker an
    // accept-loop one.
    let fresh = if cache { json!({ "cache_size": "16MB", "cache_valid_ms": 600_000 }) } else { json!({}) };
    let inline = Worker::start(&tmp, &site, fresh.clone());
    let normal = Worker::start_env(&tmp, &site, fresh, &[("WARDEN_STATIC_INLINE", "0")]);

    let paths = [
        "/index.html",
        "/",
        "/sub/",
        "/page.html",
        "/style.css",
        "/app.3f9a2c1b.js",
        "/small.bin",
        "/mid10k.bin",
        "/mid12k.bin",
        "/gz.js",
        "/sp%20ace.txt",
        "/style.css?v=1",
    ];
    // The first answer of each is a miss and fills the cache, for both.
    for w in [&inline, &normal] {
        for p in paths {
            for enc in ["", "Accept-Encoding: gzip, br\r\n"] {
                let r = Client::connect(w.port).get(p, &format!("{enc}Connection: close\r\n"));
                assert_eq!(r.status, 200, "{p}");
            }
        }
    }
    let etag = Client::connect(inline.port).get("/style.css", "").h("etag").to_string();
    let none = Duration::ZERO;
    let slow = Duration::from_millis(120);
    let mut cases: Vec<(String, Vec<String>, Duration)> = Vec::new();
    let mut add = |name: &str, segs: &[&str], pause: Duration| {
        cases.push((name.to_string(), segs.iter().map(|s| s.to_string()).collect(), pause));
    };
    for p in paths {
        for (how, extra) in [
            ("GET", "Connection: close\r\n"),
            ("GET gzip", "Accept-Encoding: gzip, br\r\nConnection: close\r\n"),
            ("GET gzip;q", "Accept-Encoding: deflate, gzip;q=0.5\r\nConnection: close\r\n"),
            ("GET conditional", &format!("If-None-Match: {etag}\r\nConnection: close\r\n")),
            ("GET since", "If-Modified-Since: Fri, 02 Oct 2099 00:00:00 GMT\r\nConnection: close\r\n"),
            ("GET range", "Range: bytes=1-4\r\nConnection: close\r\n"),
        ] {
            add(&format!("{how} {p}"), &[&format!("GET {p} HTTP/1.1\r\nHost: x\r\n{extra}\r\n")], none);
        }
        add(&format!("HEAD {p}"), &[&format!("HEAD {p} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")], none);
        add(&format!("HTTP/1.0 {p}"), &[&format!("GET {p} HTTP/1.0\r\nHost: x\r\n\r\n")], none);
        add(
            &format!("HTTP/1.0 keep-alive {p}"),
            &[&format!("GET {p} HTTP/1.0\r\nConnection: keep-alive\r\n\r\nGET {p} HTTP/1.0\r\n\r\n")],
            none,
        );
    }
    add("missing", &["GET /missing.txt HTTP/1.1\r\nConnection: close\r\n\r\n"], none);
    add("traversal", &["GET /%2e%2e/x HTTP/1.1\r\nConnection: close\r\n\r\n"], none);
    add("method", &["POST /index.html HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"], none);
    add("bare LF", &["GET /index.html HTTP/1.1\nHost: x\nConnection: close\n\n"], none);
    add("header cases", &["GET /index.html HTTP/1.1\r\nhost: x\r\nCONNECTION:   Close  \r\n\r\n"], none);
    add("stray blank line first", &["\r\nGET /index.html HTTP/1.1\r\nConnection: close\r\n\r\n"], none);
    // Kept alive, then more requests: one write, or one after another.
    let two =
        "GET /index.html HTTP/1.1\r\nHost: x\r\n\r\nGET /style.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n";
    add("pipelined", &[two], none);
    add(
        "one after another",
        &[
            "GET /index.html HTTP/1.1\r\nHost: x\r\n\r\n",
            "GET /style.css HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
        ],
        slow,
    );
    add(
        "three, the last half sent late",
        &[
            "GET /index.html HTTP/1.1\r\n\r\nHEAD /style.css HTTP/1.1\r\n\r\nGET /small.bin HTTP/1.1\r\nCon",
            "nection: close\r\n\r\n",
        ],
        slow,
    );
    // A head that arrives in pieces: the first piece is not a request yet.
    add("split head", &["GET /index.html HTTP/1.1\r\nHo", "st: x\r\nConnection: close\r\n\r\n"], slow);
    add("split at the end", &["GET /index.html HTTP/1.1\r\nConnection: close\r\n\r", "\n"], slow);
    add("split in the request line", &["GET /ind", "ex.html HTTP/1.1\r\nConnection: close\r\n\r\n"], slow);

    assert!(cases.len() > 100, "{} cases", cases.len());
    for (name, segs, pause) in &cases {
        let segs: Vec<&str> = segs.iter().map(String::as_str).collect();
        let a = exchange(inline.port, &segs, *pause);
        let b = exchange(normal.port, &segs, *pause);
        assert!(a.starts_with(b"HTTP/1.1 "), "{name}: {:?}", String::from_utf8_lossy(&a));
        assert_eq!(undated(&a), undated(&b), "{name}: the accept-loop answer differs from the normal one");
    }

    // The comparison above means something only if the accept loop really did
    // answer on the first worker, and never on the second: the workers say
    // how many when they stop.
    let n = answered(&inline.stop());
    assert!(n > cases.len() as u64 / 2, "only {n} of {} exchanges were answered in the accept loop", cases.len());
    assert_eq!(answered(&normal.stop()), 0, "WARDEN_STATIC_INLINE=0 turns it off");
}

/// How many requests the accept loop answered, from the worker's output when
/// it stopped.
fn answered(log: &str) -> u64 {
    let line = log
        .lines()
        .find(|l| l.contains("requests answered in the accept loop"))
        .unwrap_or_else(|| panic!("no summary in {log}"));
    let n = line.strip_prefix("static: ").and_then(|l| l.split_once(' ')).unwrap_or_else(|| panic!("{line}")).0;
    n.parse().unwrap_or_else(|_| panic!("{line}"))
}

/// Without openat2 every open waits for a thread: the accept loop leaves the
/// requests to tasks (it would only wait, give up, and have the task open the
/// file a second time), and the answers are the same.
#[test]
fn without_openat2_every_request_is_answered_by_a_task() {
    let tmp = Tmp::new("legacy-open");
    let site = make_site(&tmp);
    let w = Worker::start_env(&tmp, &site, json!({}), &[("WARDEN_STATIC_OPEN", "legacy")]);
    for path in ["/index.html", "/small.bin", "/mid20k.bin", "/missing"] {
        let r = Client::connect(w.port).get(path, "Connection: close\r\n");
        assert_eq!(r.status, if path == "/missing" { 404 } else { 200 }, "{path}");
    }
    let log = w.stop();
    assert!(log.contains("realpath check"), "{log}");
    assert_eq!(answered(&log), 0);
}

/// Files that are compressed already get no sibling looked up (`logo.png.br`
/// is not served: it would save nothing), and everything else still does.
#[test]
fn precompressed_siblings_are_served_for_text_and_not_looked_up_for_the_listed_formats() {
    let tmp = Tmp::new("precompressed");
    let site = tmp.site();
    write(&site, "style.css", b"a{}");
    write(&site, "style.css.gz", b"gz bytes");
    write(&site, "style.css.br", b"br bytes");
    write(&site, "plain.js", b"console.log(1)");
    write(&site, "logo.png", b"png bytes");
    write(&site, "logo.png.br", b"br bytes of the png");
    let w = Worker::start(&tmp, &site, json!({}));
    let get = |path: &str, accept: &str| {
        let r = Client::connect(w.port).get(path, &format!("Accept-Encoding: {accept}\r\nConnection: close\r\n"));
        (r.status, r.h("content-encoding").to_string(), r.body)
    };
    // br is preferred, gzip when it is all the client takes, neither when it takes neither.
    assert_eq!(get("/style.css", "gzip, deflate, br"), (200, "br".into(), b"br bytes".to_vec()));
    assert_eq!(get("/style.css", "gzip"), (200, "gzip".into(), b"gz bytes".to_vec()));
    assert_eq!(get("/style.css", "identity"), (200, "".into(), b"a{}".to_vec()));
    // No sibling: the file itself.
    assert_eq!(get("/plain.js", "gzip, br"), (200, "".into(), b"console.log(1)".to_vec()));
    // An image is sent as it is, whatever sits next to it.
    assert_eq!(get("/logo.png", "gzip, br"), (200, "".into(), b"png bytes".to_vec()));

    // `precompressed_skip` replaces the list: now the image is looked up, and the stylesheet is not.
    let w = Worker::start(&tmp, &site, json!({ "precompressed_skip": ["css"] }));
    let get = |path: &str| {
        let r = Client::connect(w.port).get(path, "Accept-Encoding: gzip, br\r\nConnection: close\r\n");
        (r.status, r.h("content-encoding").to_string(), r.body)
    };
    assert_eq!(get("/logo.png"), (200, "br".into(), b"br bytes of the png".to_vec()));
    assert_eq!(get("/style.css"), (200, "".into(), b"a{}".to_vec()));
    // An empty list looks up every file.
    let w = Worker::start(&tmp, &site, json!({ "precompressed_skip": [] }));
    let r = Client::connect(w.port).get("/style.css", "Accept-Encoding: br\r\nConnection: close\r\n");
    assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("br", b"br bytes".as_slice()));
}

/// `Accept-Encoding: br;q=0` says "not brotli": the gzip file or the plain one.
#[test]
fn an_encoding_refused_with_q_zero_is_not_served() {
    let tmp = Tmp::new("q-zero");
    let site = tmp.site();
    write(&site, "style.css", b"a{}");
    write(&site, "style.css.gz", b"gz bytes");
    write(&site, "style.css.br", b"br bytes");
    let w = Worker::start(&tmp, &site, json!({}));
    let get = |accept: &str| {
        let r =
            Client::connect(w.port).get("/style.css", &format!("Accept-Encoding: {accept}\r\nConnection: close\r\n"));
        (r.h("content-encoding").to_string(), r.body)
    };
    assert_eq!(get("br;q=0, gzip"), ("gzip".into(), b"gz bytes".to_vec()));
    assert_eq!(get("br;q=0, gzip;q=0"), ("".into(), b"a{}".to_vec()));
    assert_eq!(get("GZIP"), ("gzip".into(), b"gz bytes".to_vec()));
    assert_eq!(get("*"), ("".into(), b"a{}".to_vec()));
}

// ---------------------------------------------------------------------------
// Compression in the background
// ---------------------------------------------------------------------------

/// About `n` bytes of text that compresses well and differs with `seed`.
fn text(n: usize, seed: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while out.len() < n {
        out.extend_from_slice(
            format!(".rule-{seed}-{i}{{color:#{:06x};margin:{}px}}\n", i * 7919 % 0xff_ffff, i % 40).as_bytes(),
        );
        i += 1;
    }
    out.truncate(n);
    out
}

fn unbr(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    brotli::Decompressor::new(b, 4096).read_to_end(&mut out).expect("a brotli stream");
    out
}

fn ungz(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(b).read_to_end(&mut out).expect("a gzip stream");
    out
}

/// Ask for `path` until the answer is a compressed copy (at most 90 s), and
/// give it. The file is not compressed before it has been still for 2 s, and
/// the compressor runs at the lowest priority.
fn until_encoded(port: u16, path: &str, accept: &str) -> Resp {
    let t0 = Instant::now();
    loop {
        let r = Client::connect(port).get(path, &format!("Accept-Encoding: {accept}\r\nConnection: close\r\n"));
        if !r.h("content-encoding").is_empty() {
            return r;
        }
        assert!(t0.elapsed() < Duration::from_secs(90), "no compressed copy of {path} after {:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// The folders of the store: one for each file that has copies.
fn copy_folders(copies: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(copies)
        .map(|d| d.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect())
        .unwrap_or_default()
}

/// A file that is asked for is sent as it is, at once, and compressed in the
/// background; later requests get the copy, which decodes to the file. An
/// edit is never answered from the old copy.
#[test]
fn a_file_is_sent_plain_first_and_from_its_compressed_copy_later() {
    let tmp = Tmp::new("background");
    let site = tmp.site();
    let copies = tmp.0.join("copies");
    let css = text(20_000, 1);
    write(&site, "app.css", &css);
    write(&site, "other.css", &text(20_000, 2));
    let w = Worker::start(&tmp, &site, json!({ "compress_dir": copies, "compress_jobs": 2 }));
    let both = "Accept-Encoding: gzip, deflate, br\r\nConnection: close\r\n";
    let get = |path: &str, extra: &str| Client::connect(w.port).get(path, extra);

    let first = get("/app.css", both);
    assert_eq!((first.status, first.h("content-encoding"), first.body.as_slice()), (200, "", css.as_slice()));
    assert_eq!(first.h("vary"), "Accept-Encoding");

    let br = until_encoded(w.port, "/app.css", "gzip, deflate, br");
    assert_eq!((br.status, br.h("content-encoding"), br.h("vary")), (200, "br", "Accept-Encoding"));
    assert_eq!(unbr(&br.body), css);
    assert!(br.body.len() < css.len() / 2);
    assert_eq!(br.h("content-length"), br.body.len().to_string());
    assert!(br.h("etag").ends_with("-br\""), "{}", br.h("etag"));
    // The copy has the file's date, and its own ETag and length. (The first
    // answer may have come within the second the file was written in: then
    // it had no Last-Modified.)
    let plain = get("/app.css", "Connection: close\r\n");
    assert!(!plain.h("last-modified").is_empty());
    assert_eq!(br.h("last-modified"), plain.h("last-modified"));
    assert_ne!(br.h("etag"), first.h("etag"));
    // gzip when that is all the client takes; the file when it takes neither.
    let gz = until_encoded(w.port, "/app.css", "gzip");
    assert_eq!((gz.h("content-encoding"), ungz(&gz.body)), ("gzip", css.clone()));
    for accept in ["identity", "br;q=0, gzip;q=0", "deflate"] {
        let r = get("/app.css", &format!("Accept-Encoding: {accept}\r\nConnection: close\r\n"));
        assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("", css.as_slice()), "{accept}");
    }
    let r = get("/app.css", "Connection: close\r\n");
    assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("", css.as_slice()));
    // A range is of the file itself.
    let r = get("/app.css", "Accept-Encoding: br\r\nRange: bytes=10-19\r\nConnection: close\r\n");
    assert_eq!((r.status, r.h("content-encoding"), r.body.as_slice()), (206, "", &css[10..20]));
    // HEAD says what GET would send; the validators work on the copy.
    let h = Client::connect(w.port).head("/app.css", both);
    assert_eq!((h.h("content-encoding"), h.h("content-length")), ("br", br.h("content-length")));
    let nm =
        get("/app.css", &format!("Accept-Encoding: br\r\nIf-None-Match: {}\r\nConnection: close\r\n", br.h("etag")));
    assert_eq!(nm.status, 304);
    // The copies are in the store, and nothing was added to the site.
    let folders = copy_folders(&copies);
    assert!(!folders.is_empty());
    assert_eq!(std::fs::read_dir(&site).unwrap().count(), 2, "the site is as it was");

    // An edit: the next request has the new file, never the old copy, and a new copy follows.
    let new = text(21_000, 3);
    write(&site, "app.css", &new);
    let r = get("/app.css", both);
    assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("", new.as_slice()));
    let again = until_encoded(w.port, "/app.css", "br");
    assert_eq!(unbr(&again.body), new);
    // The old version's copies are removed once the new ones are in place: by
    // the job that made them, right after it moved them there, so a moment
    // after the first request that was answered from them.
    let t0 = Instant::now();
    let app_folder = |dir: &Path| std::fs::read_dir(dir).map(|d| d.flatten().count()).unwrap_or(0);
    let counts = || -> Vec<usize> { copy_folders(&copies).iter().map(|f| app_folder(f)).collect() };
    let mut count = counts();
    while !count.iter().all(|n| *n <= 2) && t0.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
        count = counts();
    }
    assert!(count.iter().all(|n| *n <= 2), "{count:?} after {:?}", t0.elapsed());

    // The compressing was done by other processes, not by threads of the worker (a thread
    // makes every later system call of the worker dearer; the few the runtime starts for a
    // lookup that has to wait are another matter).
    #[cfg(target_os = "linux")]
    for task in std::fs::read_dir(format!("/proc/{}/task", w.pid())).unwrap().flatten() {
        let name = std::fs::read_to_string(task.path().join("comm")).unwrap_or_default();
        assert!(!name.contains("compress"), "a thread for compressing: {name}");
    }
    let out = w.stop();
    assert!(out.contains("compressing files in the background: up to 2 processes at a time"), "{out}");
    assert!(out.contains("static compression: compressed "), "{out}");
}

/// What is never queued: a worker with `compress = false`; files outside
/// the size limits; requests that cannot be answered with a copy.
#[test]
fn background_compression_is_off_when_asked_and_leaves_what_it_should() {
    let tmp = Tmp::new("background-limits");
    let site = tmp.site();
    let copies = tmp.0.join("copies");
    write(&site, "small.css", &text(5_000, 1));
    write(&site, "mid.css", &text(20_000, 2));
    write(&site, "huge.css", &text(60_000, 3));
    write(&site, "never.css", &text(20_000, 4));
    write(&site, "logo.png", &text(20_000, 5));
    let both = "Accept-Encoding: gzip, br\r\nConnection: close\r\n";

    // Off: the directory is not even made.
    let off = Worker::start(&tmp, &site, json!({ "compress": false, "compress_dir": copies }));
    for _ in 0..4 {
        let r = Client::connect(off.port).get("/mid.css", both);
        assert_eq!(r.h("content-encoding"), "");
        std::thread::sleep(Duration::from_millis(700));
    }
    assert!(!copies.exists(), "compress = false makes nothing");
    let out = off.stop();
    assert!(!out.contains("compressing files"), "{out}");

    // Limits: only mid.css is between 10 KB and 40 KB. The requests that cannot be answered
    // with a copy never queue one, however late they come.
    let w = Worker::start(
        &tmp,
        &site,
        json!({ "compress_dir": copies, "compress_min_file": "10KB", "compress_max_file": "40KB" }),
    );
    let t0 = Instant::now();
    let copy = loop {
        for path in ["/small.css", "/huge.css", "/logo.png"] {
            let r = Client::connect(w.port).get(path, both);
            assert_eq!(r.h("content-encoding"), "", "{path}");
        }
        let h = Client::connect(w.port).head("/never.css", both);
        let ranged = Client::connect(w.port)
            .get("/never.css", "Accept-Encoding: br\r\nRange: bytes=0-9\r\nConnection: close\r\n");
        assert_eq!((h.h("content-encoding"), ranged.status), ("", 206));
        for accept in ["identity", "br;q=0, gzip;q=0"] {
            let r = Client::connect(w.port)
                .get("/never.css", &format!("Accept-Encoding: {accept}\r\nConnection: close\r\n"));
            assert_eq!(r.h("content-encoding"), "");
        }
        let r = Client::connect(w.port).get("/never.css", "Connection: close\r\n");
        assert_eq!(r.h("content-encoding"), "");
        let r = Client::connect(w.port).get("/mid.css", both);
        if r.h("content-encoding") == "br" {
            break r;
        }
        assert!(t0.elapsed() < Duration::from_secs(90), "no copy of mid.css");
        std::thread::sleep(Duration::from_millis(250));
    };
    assert_eq!(unbr(&copy.body), text(20_000, 2));
    // Everything those requests could have queued has had its chance by now.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(copy_folders(&copies).len(), 1, "only mid.css has copies");
}

/// A file with a compressed sibling of its own (made by the site's build) is
/// served from that, and not compressed again.
#[test]
fn a_file_with_a_compressed_sibling_is_left_to_it() {
    let tmp = Tmp::new("background-sibling");
    let site = tmp.site();
    let copies = tmp.0.join("copies");
    write(&site, "site.js", &text(20_000, 1));
    write(&site, "site.js.br", b"the site's own brotli");
    let w = Worker::start(&tmp, &site, json!({ "compress_dir": copies }));
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(4500) {
        let r = Client::connect(w.port).get("/site.js", "Accept-Encoding: br\r\nConnection: close\r\n");
        assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("br", b"the site's own brotli".as_slice()));
        // gzip only: no sibling for it, so the file; and still nothing is made.
        let r = Client::connect(w.port).get("/site.js", "Accept-Encoding: gzip\r\nConnection: close\r\n");
        assert_eq!(r.h("content-encoding"), "");
        std::thread::sleep(Duration::from_millis(300));
    }
    assert!(copy_folders(&copies).is_empty(), "no copy of a file that has a sibling");
}

/// The response cache must not pin the plain response of a file whose copy is
/// on its way, and serves the copy once there is one.
#[test]
fn the_response_cache_follows_the_file_to_its_compressed_copy() {
    let tmp = Tmp::new("background-cache");
    let site = tmp.site();
    let css = text(20_000, 7);
    write(&site, "app.css", &css);
    write(&site, "page.html", b"<h1>small, and below the minimum</h1>");
    let w = Worker::start(
        &tmp,
        &site,
        json!({ "compress_dir": tmp.0.join("copies"), "cache_size": "16MB", "cache_valid_ms": 600_000, "access_log": true }),
    );
    let both = "Accept-Encoding: gzip, br\r\nConnection: close\r\n";
    let r = Client::connect(w.port).get("/app.css", both);
    assert_eq!((r.h("content-encoding"), r.body.as_slice()), ("", css.as_slice()));
    // Several requests while the copy is made: all plain, none of them kept.
    std::thread::sleep(Duration::from_millis(300));
    let r = Client::connect(w.port).get("/app.css", both);
    assert_eq!(r.h("content-encoding"), "");
    let copy = until_encoded(w.port, "/app.css", "gzip, br");
    assert_eq!(unbr(&copy.body), css);
    // The copy is cached now (once it has been still for 2 s, like any file), and so is a
    // file that is too small to have one.
    quiet();
    for _ in 0..3 {
        let r = Client::connect(w.port).get("/app.css", both);
        assert_eq!((r.h("content-encoding"), unbr(&r.body)), ("br", css.clone()));
        let r = Client::connect(w.port).get("/page.html", both);
        assert_eq!(r.h("content-encoding"), "");
    }
    let out = w.stop();
    assert!(out.lines().any(|l| l.contains("GET /app.css 200") && l.contains("cache=hit")), "{out}");
    assert!(out.lines().any(|l| l.contains("GET /page.html 200") && l.contains("cache=hit")), "{out}");
}

/// The folder of the copies must be the user's, closed to others, and not
/// something the site serves; when it is not, serving goes on without.
#[test]
fn a_compression_folder_that_is_not_private_or_is_served_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = Tmp::new("background-refused");
    let site = tmp.site();
    write(&site, "app.css", &text(20_000, 1));
    let open_dir = tmp.0.join("open");
    std::fs::create_dir_all(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    for (dir, why) in [(open_dir, "open to other users"), (site.join(".copies"), "inside static.root")] {
        let w = Worker::start(&tmp, &site, json!({ "compress_dir": dir }));
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_millis(3000) {
            let r = Client::connect(w.port).get("/app.css", "Accept-Encoding: br\r\nConnection: close\r\n");
            assert_eq!((r.status, r.h("content-encoding")), (200, ""));
            std::thread::sleep(Duration::from_millis(300));
        }
        let out = w.stop();
        assert!(out.contains("compression in the background is off") && out.contains(why), "{why}: {out}");
        assert!(!out.contains("compressing files in the background"), "{out}");
    }
    assert_eq!(std::fs::read_dir(&site).unwrap().count(), 1, "nothing was put in the served folder");
}

/// The process that compresses starts with whatever descriptors its parent
/// had (the supervisor's channel is one, and is not close-on-exec) and must
/// close them: one that kept it would keep the supervisor from seeing the
/// worker end.
#[cfg(target_os = "linux")]
#[test]
fn a_compression_process_closes_the_descriptors_it_inherited() {
    use std::os::fd::AsRawFd;
    let tmp = Tmp::new("job-fds");
    let site = tmp.site();
    write(&site, "a.css", &text(20_000, 1));
    let copies = tmp.0.join("copies");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&copies).unwrap();
    }
    // Hold the only place for compressing, so that the job waits where it can be looked at.
    let place = std::fs::OpenOptions::new().create(true).append(true).open(copies.join("place-0")).unwrap();
    // SAFETY: flock(2) on a descriptor this test owns.
    assert_eq!(unsafe { libc::flock(place.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let spec = json!({
        "parent": std::process::id(), "root": site, "store": copies, "cap": 268_435_456u64,
        "task": { "Compress": {
            "rel": "a.css", "min": 1024, "max": 8_388_608, "jobs": 1,
            "version": { "dev": 0, "ino": 0, "size": 0, "mtime": [0, 0], "ctime": [0, 0] },
        } },
    });
    // bash opens a file on descriptor 9 and becomes the job: 9 is inherited and not close-on-exec.
    let mut job = Command::new("bash")
        .args(["-c", "exec 9</etc/hostname; exec \"$0\" static-compress", &bin()])
        .env_clear()
        .env("WARDEN_COMPRESS_JOB", spec.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(800));
    assert!(job.try_wait().unwrap().is_none(), "the job should be waiting for its place");
    let held: Vec<String> = std::fs::read_dir(format!("/proc/{}/fd", job.id()))
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().parse::<i32>().unwrap_or(0) > 2)
        .filter_map(|e| std::fs::read_link(e.path()).ok())
        .map(|p| p.display().to_string())
        .collect();
    let _ = job.kill();
    let _ = job.wait();
    // What it opened itself: the site, its folder of copies and the lock files.
    assert!(held.iter().all(|p| p.starts_with(tmp.0.to_str().unwrap())), "descriptors of the parent kept: {held:?}");
}

/// A bug in a handler costs the connection it was serving and nothing else.
/// The accept loop answers new connections itself, so a panic there must not
/// take the worker down (a panic in a task never does). Uses the fault
/// injection of debug builds (`WARDEN_FAULT`); a release binary has none, and
/// the test skips.
#[test]
fn a_panicking_handler_costs_its_connection_and_nothing_else() {
    let tmp = Tmp::new("panic");
    let site = make_site(&tmp);
    let w = Worker::start_env(&tmp, &site, json!({}), &[("WARDEN_FAULT", "static:3")]);
    for n in 1..=4 {
        let mut c = Client::connect(w.port);
        c.send("GET /index.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let mut got = Vec::new();
        let _ = c.s.read_to_end(&mut got);
        if n == 3 && got.starts_with(b"HTTP/1.1 200 ") {
            eprintln!(
                "SKIPPED a_panicking_handler_costs_its_connection_and_nothing_else: no fault injection in this binary"
            );
            return;
        }
        if n == 3 {
            assert!(got.is_empty(), "the connection of the panic is closed without an answer: {got:?}");
        } else {
            assert!(got.starts_with(b"HTTP/1.1 200 "), "request {n}: {:?}", String::from_utf8_lossy(&got));
        }
    }
    let log = w.stop();
    assert!(log.contains("a request handler panicked (injected fault at static (hit 3))"), "{log}");
}

/// The limits on a connection that is waiting for a request (idle.rs): the
/// first head, the wait between requests, and nothing while a response is
/// being sent. The limits are shortened for the test.
#[test]
fn connections_waiting_for_a_request_time_out_and_busy_ones_do_not() {
    let tmp = Tmp::new("timeouts");
    let site = make_site(&tmp);
    write(&site, "huge.bin", &pattern(40_000_000));
    let w = Worker::start_env(&tmp, &site, json!({ "cache_size": "16MB" }), &[("WARDEN_STATIC_TIMEOUTS", "2,5")]);
    quiet(); // so that the files can be cached

    // Closed after how long, or None if it stays open past `within`.
    fn closes_after(s: &mut TcpStream, within: Duration) -> Option<Duration> {
        s.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let t0 = Instant::now();
        let mut b = [0u8; 1];
        while t0.elapsed() < within {
            match s.read(&mut b) {
                Ok(0) => return Some(t0.elapsed()),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => return Some(t0.elapsed()),
            }
        }
        None
    }

    // Connects and says nothing: closed at the head limit (2 s, to within the
    // sweep's second; the kernel holds such a connection back first).
    let mut s = TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    let t = closes_after(&mut s, Duration::from_secs(9)).expect("a silent connection is closed");
    assert!(t >= Duration::from_millis(1900), "not at once: {t:?}");

    // Sends half a head and stalls: the same.
    let mut s = TcpStream::connect(("127.0.0.1", w.port)).unwrap();
    s.write_all(b"GET /index.html HTTP/1.1\r\nHo").unwrap();
    let t = closes_after(&mut s, Duration::from_secs(9)).expect("a stalled head is closed");
    assert!(t >= Duration::from_millis(1900) && t <= Duration::from_secs(4), "{t:?}");

    // Idle between requests: closed at the idle limit (5 s), not at the head
    // limit (2 s). One connection's first request fills the response cache
    // (a miss, served by its task); the second's is then answered from the
    // cache by the accept loop. Both wait for their next request the same.
    let mut a = Client::connect(w.port);
    assert_eq!(a.get("/index.html", "").status, 200);
    let t_a = Instant::now();
    let mut b = Client::connect(w.port);
    assert_eq!(b.get("/index.html", "").status, 200);
    let t_b = Instant::now();
    let (mut closed_a, mut closed_b) = (None, None);
    a.s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    b.s.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    while (closed_a.is_none() || closed_b.is_none()) && t_a.elapsed() < Duration::from_secs(12) {
        for (s, since, closed) in [(&mut a.s, t_a, &mut closed_a), (&mut b.s, t_b, &mut closed_b)] {
            if closed.is_some() {
                continue;
            }
            match s.read(&mut [0u8; 1]) {
                Ok(0) => *closed = Some(since.elapsed()),
                Err(e) if !matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                    *closed = Some(since.elapsed())
                }
                _ => {}
            }
        }
    }
    for (who, t) in [("served by its task", closed_a), ("answered in the accept loop", closed_b)] {
        let t = t.unwrap_or_else(|| panic!("an idle keep-alive connection {who} is closed"));
        assert!(t >= Duration::from_millis(4500) && t <= Duration::from_secs(8), "{who}: {t:?}");
    }

    // A connection that keeps asking stays open past the idle limit: each
    // request starts a new wait.
    let mut c = Client::connect(w.port);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(7) {
        assert_eq!(c.get("/index.html", "").status, 200, "after {:?}", t0.elapsed());
        std::thread::sleep(Duration::from_millis(900));
    }

    // A client that stops reading in the middle of a big response is not
    // idle: the server is sending, and no limit applies to that.
    let mut c = Client::connect(w.port);
    c.send("GET /huge.bin HTTP/1.1\r\nHost: x\r\n\r\n");
    let mut head = [0u8; 100];
    let n = c.s.read(&mut head).unwrap();
    assert!(n > 0);
    std::thread::sleep(Duration::from_secs(5));
    c.buf.extend_from_slice(&head[..n]);
    let r = c.recv(false);
    assert_eq!((r.status, r.body.len()), (200, 40_000_000), "the whole file arrives after the pause");
}

#[test]
fn large_bodies_arrive_intact_on_kept_alive_connections() {
    bodies_arrive_intact("0");
}

#[test]
fn cached_and_large_bodies_arrive_intact_on_kept_alive_connections() {
    bodies_arrive_intact("16MB");
}

fn bodies_arrive_intact(cache: &str) {
    let tmp = Tmp::new(&format!("bodies-{cache}"));
    let site = make_site(&tmp);
    quiet();
    let w = Worker::start(&tmp, &site, json!({ "cache_size": cache }));

    // 20 KB: kept in a memfd and sent with sendfile on a cache hit. Three
    // pipelined requests (the first fills the cache), then a close.
    let mid = pattern(20_000);
    for round in 0..2 {
        let mut c = Client::connect(w.port);
        let req = "GET /mid20k.bin HTTP/1.1\r\nHost: x\r\n\r\n";
        c.send(&format!("{req}{req}{req}"));
        for i in 0..3 {
            let r = c.recv(false);
            assert!(r.status == 200 && r.body == mid, "round {round}, response {i}: {} bytes", r.body.len());
        }
        c.send("GET /mid20k.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
        let r = c.recv(false);
        assert!(r.status == 200 && r.body == mid && r.h("connection") == "close");
        let mut rest = Vec::new();
        let _ = c.s.read_to_end(&mut rest);
        assert!(rest.is_empty());
    }

    // 3 MB straight from the file, read slowly (the socket fills up and the
    // server waits for room), then another request on the same connection.
    let big = pattern(3_000_000);
    let mut c = Client::connect(w.port);
    c.send("GET /big.bin HTTP/1.1\r\nHost: x\r\n\r\n");
    let r = slow_recv(&mut c);
    assert_eq!(r.status, 200);
    assert!(r.body == big, "3 MB body differs ({} bytes)", r.body.len());
    let r = c.get("/small.bin", "");
    assert!(
        r.status == 200 && r.body == pattern(1_000),
        "the connection must still frame correctly after a large body"
    );

    // A range of it, also read slowly: exactly those bytes.
    c.send("GET /big.bin HTTP/1.1\r\nHost: x\r\nRange: bytes=1000000-1999999\r\n\r\n");
    let r = slow_recv(&mut c);
    assert_eq!((r.status, r.h("content-range")), (206, "bytes 1000000-1999999/3000000"));
    assert!(r.body == big[1_000_000..2_000_000], "range body differs");
    let r = c.get("/index.html", "");
    assert_eq!((r.status, r.body.as_slice()), (200, b"<h1>home</h1>".as_slice()));
}

/// Like `Client::recv`, but the reader dawdles between reads.
fn slow_recv(c: &mut Client) -> Resp {
    let mut chunk = vec![0u8; 64 * 1024];
    let end = loop {
        if let Some(p) = c.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        c.fill();
    };
    let head = String::from_utf8_lossy(&c.buf[..end]).to_string();
    let len: usize = head
        .lines()
        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse().unwrap()))
        .unwrap();
    while c.buf.len() < end + 4 + len {
        std::thread::sleep(Duration::from_millis(3));
        let n = c.s.read(&mut chunk).unwrap();
        assert!(n > 0, "the connection ended after {} of {} bytes", c.buf.len().saturating_sub(end + 4), len);
        c.buf.extend_from_slice(&chunk[..n]);
    }
    c.recv(false)
}

// ---------------------------------------------------------------------------
// Fairness: a big download and everyone else
// ---------------------------------------------------------------------------

/// A big download must not keep the worker from its other connections. The
/// worker is one thread: it sends a file in pieces of at most 1 MiB and lets
/// the others in between (`yield_now`). With one long `sendfile` call, or
/// pieces without the yield, a client asking for a small page next to a
/// 128 MB download gets none or one answer until the download is over (the
/// "static files take 3-10 ms" report: 5 ms next to 10 MB). Counted, not
/// timed: how many small answers arrive before the download ends. One per
/// piece is the ideal (about 128, measured 124); pieces without the yield
/// gave 1 to 22 in twelve attempts (the runtime lets others in only when its
/// budget runs out). The limit sits between, and the best of three attempts
/// counts, so a loaded machine does not trip it.
#[test]
fn a_big_download_does_not_hold_up_other_requests() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    const HUGE: u64 = 128 << 20;
    const AT_LEAST: u32 = 40;

    let tmp = Tmp::new("fair");
    let site = make_site(&tmp);
    // Sparse: the kernel serves the holes as zeros, no disk is written.
    std::fs::File::create(site.join("huge.bin")).unwrap().set_len(HUGE).unwrap();
    quiet();
    let w = Worker::start(&tmp, &site, json!({}));

    let mut counts = Vec::new();
    for _ in 0..3 {
        let mut small = Client::connect(w.port);
        assert_eq!(small.get("/index.html", "").status, 200);
        let (started, done) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (port, flags) = (w.port, (started.clone(), done.clone()));
        let download = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
            s.write_all(b"GET /huge.bin HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
            let (mut buf, mut total) = (vec![0u8; 1 << 20], 0u64);
            loop {
                let n = s.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                total += n as u64;
                flags.0.store(true, Ordering::Release);
            }
            flags.1.store(true, Ordering::Release);
            total
        });
        let t0 = Instant::now();
        while !started.load(Ordering::Acquire) {
            assert!(t0.elapsed() < Duration::from_secs(20), "the download never started");
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut answered = 0u32;
        while !done.load(Ordering::Acquire) {
            assert_eq!(small.get("/index.html", "").status, 200);
            answered += 1;
        }
        let total = download.join().unwrap();
        assert!(total >= HUGE, "the download ended after {total} of {HUGE} bytes");
        eprintln!("{answered} small answers while {} MB went out", HUGE >> 20);
        counts.push(answered);
        if answered >= AT_LEAST {
            return;
        }
    }
    panic!(
        "next to a {} MB download the page was answered {counts:?} times (at least {AT_LEAST} wanted): \
         the worker does not let other connections in between the pieces of a big file",
        HUGE >> 20
    );
}

// ---------------------------------------------------------------------------
// The speed guards (Linux: /proc, TCP_INFO)
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::os::fd::AsRawFd;

    /// Data segments the kernel has received on `s` (TCP_INFO). None where
    /// the kernel does not report it.
    fn data_segs_in(s: &TcpStream) -> Option<u32> {
        // SAFETY: tcp_info is plain data (zero is valid); getsockopt writes at
        // most `len` bytes into it and updates `len`.
        let (rc, info, len) = unsafe {
            let mut info: libc::tcp_info = std::mem::zeroed();
            let mut len = std::mem::size_of::<libc::tcp_info>() as libc::socklen_t;
            let rc = libc::getsockopt(
                s.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_INFO,
                (&raw mut info).cast::<libc::c_void>(),
                &mut len,
            );
            (rc, info, len as usize)
        };
        let needs = std::mem::offset_of!(libc::tcp_info, tcpi_data_segs_in) + 4;
        (rc == 0 && len >= needs).then_some(info.tcpi_data_segs_in)
    }

    fn status_field(pid: u32, key: &str) -> u64 {
        let st = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        st.lines().find_map(|l| l.strip_prefix(key)?.trim_start_matches(':').trim().parse().ok()).unwrap()
    }

    /// Run time of a task in ns (/proc/.../schedstat, first field).
    fn run_ns(path: &str) -> u64 {
        std::fs::read_to_string(path).unwrap().split_whitespace().next().unwrap().parse().unwrap()
    }

    /// A kept-alive request with a response of one segment: how many data
    /// segments came back?
    fn segments(port: u16, req: &str, head_only: bool) -> (u16, u32) {
        let mut c = Client::connect(port);
        c.send(req);
        let r = c.recv(head_only);
        let segs = data_segs_in(&c.s).expect("TCP_INFO has no tcpi_data_segs_in on this kernel");
        (r.status, segs)
    }

    #[test]
    fn a_small_response_leaves_in_one_segment() {
        let tmp = Tmp::new("segments");
        let site = make_site(&tmp);
        // The plain error pages come from a site without 404.html.
        let bare = tmp.0.join("bare");
        write(&bare, "index.html", b"<h1>home</h1>");
        write(&bare, "empty/a.txt", b"a");
        quiet();

        // Head and body in two sends are two packets: with TCP_NODELAY the
        // first leaves at once, and the client's ACK delay is a latency of
        // its own. Only responses well under the first send buffer count
        // (a larger one may legitimately need several).
        let cached = Worker::start(&tmp, &site, json!({ "cache_size": "16MB" }));
        let uncached = Worker::start(&tmp, &site, json!({ "cache_size": 0 }));
        let plain = Worker::start(&tmp, &bare, json!({ "listing": true }));
        let locked = Worker::start(&tmp, &bare, json!({ "basic_auth": "u:p" }));
        let get = |p: &str, extra: &str| format!("GET {p} HTTP/1.1\r\nHost: x\r\n{extra}\r\n");

        // The same files give the same ETag on both servers; ask the one
        // without a cache, so the cached server's first requests are misses.
        let etag = Client::connect(uncached.port).get("/index.html", "").h("etag").to_string();
        let mut cases: Vec<(String, u16, String, bool, u16)> = Vec::new();
        let mut case = |what: &str, port: u16, req: String, head_only: bool, status: u16| {
            cases.push((what.to_string(), port, req, head_only, status));
        };
        // Twice for the cache: the first is a miss, the second a hit.
        for round in ["miss", "hit"] {
            case(&format!("cached 13 B page ({round})"), cached.port, get("/index.html", ""), false, 200);
            case(&format!("cached 1 KB file ({round})"), cached.port, get("/small.bin", ""), false, 200);
            case(&format!("cached 10 KB file in a memfd ({round})"), cached.port, get("/mid10k.bin", ""), false, 200);
        }
        case("cached page, Connection: close", cached.port, get("/index.html", "Connection: close\r\n"), false, 200);
        case(
            "cached memfd body, Connection: close",
            cached.port,
            get("/mid10k.bin", "Connection: close\r\n"),
            false,
            200,
        );
        case("cached 304", cached.port, get("/index.html", &format!("If-None-Match: {etag}\r\n")), false, 304);
        case("cached HEAD", cached.port, "HEAD /index.html HTTP/1.1\r\nHost: x\r\n\r\n".to_string(), true, 200);
        case("custom 404 page", cached.port, get("/missing", ""), false, 404);
        case("uncached 13 B page", uncached.port, get("/index.html", ""), false, 200);
        case("uncached 1 KB file", uncached.port, get("/small.bin", ""), false, 200);
        case("uncached 12 KB file", uncached.port, get("/mid12k.bin", ""), false, 200);
        case("uncached 304", uncached.port, get("/index.html", &format!("If-None-Match: {etag}\r\n")), false, 304);
        case("plain 404", plain.port, get("/nothing", ""), false, 404);
        case("directory listing", plain.port, get("/empty/", ""), false, 200);
        case("405", plain.port, "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n".to_string(), false, 405);
        case("401", locked.port, get("/", ""), false, 401);

        let mut bad = Vec::new();
        for (what, port, req, head_only, status) in cases {
            let (got, segs) = segments(port, &req, head_only);
            if got != status {
                bad.push(format!("{what}: status {got}, wanted {status}"));
            } else if segs != 1 {
                bad.push(format!("{what}: {segs} data segments, wanted 1"));
            }
        }
        assert!(bad.is_empty(), "responses that did not leave in one packet:\n  {}", bad.join("\n  "));
    }

    #[test]
    fn the_worker_stays_on_one_thread_and_sleeps_once_per_request() {
        let tmp = Tmp::new("threads");
        let site = make_site(&tmp);
        quiet();
        for (what, extra) in [("cache on", json!({ "cache_size": "16MB" })), ("cache off", json!({ "cache_size": 0 }))]
        {
            let w = Worker::start(&tmp, &site, extra);
            let mut c = Client::connect(w.port);
            for _ in 0..200 {
                c.get("/index.html", "");
            }
            assert_eq!(status_field(w.pid(), "Threads"), 1, "{what}: one thread serves everything");
            let before = status_field(w.pid(), "voluntary_ctxt_switches");
            const N: u64 = 2000;
            for _ in 0..N {
                let r = c.get("/index.html", "");
                assert_eq!(r.status, 200);
            }
            let slept = status_field(w.pid(), "voluntary_ctxt_switches") - before;
            assert_eq!(status_field(w.pid(), "Threads"), 1, "{what}: a request must not start a thread");
            // One wait for the next request, nothing else that blocks: a
            // thread-pool hop or a second wait would double it.
            let per_request = slept as f64 / N as f64;
            eprintln!("{what}: {per_request:.3} voluntary context switches per request");
            assert!(per_request < 1.5, "{what}: {per_request:.2} sleeps per request");
        }
    }

    /// A test that needs `strace` cannot run: say so on stderr (`SKIPPED`,
    /// visible with `--nocapture` and in the failure output of a run that
    /// has other failures), or fail when `WARDEN_REQUIRE_STRACE=1`, which a
    /// CI job that installs strace sets so that a missing or unusable one
    /// cannot make these tests pass without checking anything.
    fn skipped(test: &str, why: &str) {
        if std::env::var("WARDEN_REQUIRE_STRACE").is_ok_and(|v| v == "1") {
            panic!("{test}: {why}, and WARDEN_REQUIRE_STRACE=1 requires strace");
        }
        eprintln!("SKIPPED {test}: {why} (WARDEN_REQUIRE_STRACE=1 makes this a failure)");
    }

    /// Is there a `strace`? If not, the test says it was skipped (or fails).
    fn have_strace(test: &str) -> bool {
        let found = Command::new("strace").arg("-V").output().is_ok();
        if !found {
            skipped(test, "strace not found on PATH");
        }
        found
    }

    /// What `strace` (with `args`) wrote about `pid` while `work` ran; None
    /// when strace is missing or cannot attach.
    fn strace(pid: u32, args: &[&str], work: impl FnOnce()) -> Option<String> {
        let out = tmp().join(format!("warden-perf-strace-{}-{pid}", std::process::id()));
        let mut strace = Command::new("strace")
            .args(args)
            .args(["-f", "-p", &pid.to_string(), "-o"])
            .arg(&out)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // Attached once the worker stops answering as fast: just wait.
        std::thread::sleep(Duration::from_millis(500));
        if strace.try_wait().ok()?.is_some() {
            let _ = std::fs::remove_file(&out);
            return None;
        }
        work();
        // SAFETY: plain kill(2) of the strace child; SIGINT makes it detach and print.
        unsafe { libc::kill(strace.id() as i32, libc::SIGINT) };
        let _ = strace.wait();
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        Some(text)
    }

    /// Total system calls of `pid` while `work` runs, from `strace -c`.
    fn count_syscalls(pid: u32, work: impl FnOnce()) -> Option<u64> {
        let text = strace(pid, &["-c"], work)?;
        // Rows: % time, seconds, usecs/call, calls, [errors,] syscall.
        let calls: Vec<u64> = text
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                let name = f.last()?;
                (f.len() >= 5 && *name != "total" && f[0].parse::<f64>().is_ok()).then(|| f[3].parse().ok())?
            })
            .collect();
        (!calls.is_empty()).then(|| calls.iter().sum())
    }

    /// Every sendfile asks for at most 1 MiB, so one worker thread comes back
    /// to its other connections between pieces (a 10 MB file used to be one
    /// 5 ms call: every other request on the worker waited for it), and a
    /// body of 1.25 MiB or more is corked, once, and uncorked at its end.
    #[test]
    fn big_bodies_go_out_in_chunks_and_are_corked() {
        if !have_strace("big_bodies_go_out_in_chunks_and_are_corked") {
            return;
        }
        let tmp = Tmp::new("chunks");
        let site = tmp.site();
        write(&site, "below.bin", &pattern(1_310_719));
        write(&site, "exact.bin", &pattern(1_310_720));
        write(&site, "big.bin", &pattern(3_000_000));
        write(&site, "mid.bin", &pattern(500_000));
        quiet();
        let w = Worker::start(&tmp, &site, json!({}));
        // (file, size, corked)
        let cases = [
            ("mid.bin", 500_000usize, false),
            ("below.bin", 1_310_719, false),
            ("exact.bin", 1_310_720, true),
            ("big.bin", 3_000_000, true),
        ];
        let mut ran = 0;
        for (file, size, corked) in cases {
            let mut body = Vec::new();
            let text = strace(w.pid(), &["-e", "trace=sendfile,setsockopt"], || {
                let mut c = Client::connect(w.port);
                body = c.get(&format!("/{file}"), "").body;
            });
            let Some(text) = text else {
                skipped("big_bodies_go_out_in_chunks_and_are_corked", "strace cannot attach to the worker");
                return;
            };
            assert!(body == pattern(size), "{file}: the body differs");
            // `sendfile(11, 12, [0] => [1048576], 1048576) = 1048576`: the last
            // number before ") = " is the count asked for.
            let asked: Vec<usize> = text
                .lines()
                .filter(|l| l.contains("sendfile("))
                .filter_map(|l| l.rsplit_once(") = ")?.0.rsplit(", ").next()?.parse().ok())
                .collect();
            assert!(!asked.is_empty(), "{file}: no sendfile in the trace:\n{text}");
            assert!(asked.iter().all(|n| *n <= 1 << 20), "{file}: a sendfile asked for more than 1 MiB: {asked:?}");
            assert!(asked.len() >= size.div_ceil(1 << 20), "{file}: {} sendfile calls for {size} bytes", asked.len());
            let cork: Vec<&str> = text.lines().filter(|l| l.contains("TCP_CORK")).collect();
            if corked {
                assert!(
                    cork.len() == 2 && cork[0].contains("[1]") && cork[1].contains("[0]"),
                    "{file}: corked once, then off:\n{text}"
                );
            } else {
                assert!(cork.is_empty(), "{file}: smaller bodies must not be corked:\n{text}");
            }
            ran += 1;
        }
        assert_eq!(ran, cases.len());
    }

    #[test]
    fn system_calls_per_request_stay_low() {
        if !have_strace("system_calls_per_request_stay_low") {
            return;
        }
        let tmp = Tmp::new("syscalls");
        let site = make_site(&tmp);
        quiet();
        // (what, config, most calls per kept-alive request, most per request
        // on a new connection)
        //
        // With a cache. Kept alive: wait for readiness, read the request,
        // send the response (3 measured). A new connection whose request is
        // already in the socket and whose answer is cached never leaves the
        // accept loop: wait for the listener, accept, read, send, close (5
        // measured when connections arrive one by one; 4 when they queue up).
        // It used to register the socket with epoll and take it out again,
        // start a task and a timer (9, and 11 before the worker stopped
        // setting TCP_NODELAY and shutting down each connection).
        //
        // Without: the file is opened, looked at, read (or sent) and closed
        // each time, four calls more: kept alive 8 measured; a new connection
        // 10 (it also never leaves the accept loop: no epoll registration).
        //
        // One system call more per request is a regression; the margin is the
        // heartbeat and idle-sweep timers, which a slower (debug) build runs
        // more often per request.
        for (what, cfg, keep_max, fresh_max) in
            [("cache", json!({ "cache_size": "16MB" }), 3.5, 5.6), ("no cache", json!({}), 8.5, 10.6)]
        {
            let w = Worker::start(&tmp, &site, cfg);
            // Warm the cache and the connection setup.
            for _ in 0..20 {
                Client::connect(w.port).get("/index.html", "Connection: close\r\n");
            }

            const N: u64 = 300;
            let keep = count_syscalls(w.pid(), || {
                let mut c = Client::connect(w.port);
                for _ in 0..N {
                    c.get("/index.html", "");
                }
            });
            let Some(keep) = keep else {
                skipped("system_calls_per_request_stay_low", "strace cannot attach to the worker");
                return;
            };
            let fresh = count_syscalls(w.pid(), || {
                for _ in 0..N {
                    Client::connect(w.port).get("/index.html", "Connection: close\r\n");
                }
            })
            .unwrap();
            let (keep, fresh) = (keep as f64 / N as f64, fresh as f64 / N as f64);
            eprintln!("system calls per request, {what}: {keep:.2} kept alive, {fresh:.2} on a new connection");
            assert!(keep <= keep_max, "{what}: {keep:.2} system calls per kept-alive request");
            assert!(fresh <= fresh_max, "{what}: {fresh:.2} system calls per new-connection request");
        }
    }

    /// A trivial HTTP server: one thread, blocking sockets, one canned
    /// response. About the least work any server can do per request.
    struct Trivial {
        port: u16,
        tid: u32,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        join: Option<std::thread::JoinHandle<()>>,
    }

    impl Trivial {
        fn start(body: usize) -> Trivial {
            use std::sync::atomic::Ordering;
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = l.local_addr().unwrap().port();
            let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let (tx, rx) = std::sync::mpsc::channel();
            let st = stop.clone();
            let join = std::thread::spawn(move || {
                // SAFETY: gettid has no arguments and cannot fail.
                tx.send(unsafe { libc::syscall(libc::SYS_gettid) } as u32).unwrap();
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {body}\r\nConnection: keep-alive\r\n\r\n"
                )
                .into_bytes();
                resp.resize(resp.len() + body, b'x');
                let mut buf = [0u8; 4096];
                for conn in l.incoming() {
                    if st.load(Ordering::Relaxed) {
                        break;
                    }
                    let Ok(mut c) = conn else { continue };
                    let _ = c.set_nodelay(true);
                    loop {
                        match c.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if c.write_all(&resp).is_err()
                                    || buf[..n].windows(17).any(|w| w == b"Connection: close")
                                {
                                    break;
                                }
                            }
                        }
                    }
                }
            });
            Trivial { port, tid: rx.recv().unwrap(), stop, join: Some(join) }
        }

        fn cpu_ns(&self) -> u64 {
            run_ns(&format!("/proc/self/task/{}/schedstat", self.tid))
        }
    }

    impl Drop for Trivial {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(j) = self.join.take() {
                let _ = j.join();
            }
        }
    }

    /// (server CPU per request in µs, median round trip in µs) of `n`
    /// requests; `cpu` reads the server's CPU time.
    fn measure(port: u16, n: usize, keep_alive: bool, cpu: &dyn Fn() -> u64) -> (f64, f64) {
        let mut times = Vec::with_capacity(n);
        let mut c = keep_alive.then(|| Client::connect(port));
        let c0 = cpu();
        for _ in 0..n {
            let t = Instant::now();
            match &mut c {
                Some(c) => {
                    c.get("/index.html", "");
                }
                None => {
                    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
                    s.set_nodelay(true).unwrap();
                    s.write_all(b"GET /index.html HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
                    let mut sink = Vec::new();
                    s.read_to_end(&mut sink).unwrap();
                    assert!(sink.starts_with(b"HTTP/1.1 200"));
                }
            }
            times.push(t.elapsed().as_nanos() as f64 / 1000.0);
        }
        let used = (cpu() - c0) as f64 / 1000.0 / n as f64;
        times.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (used, times[n / 2])
    }

    #[test]
    fn answers_within_a_small_factor_of_a_trivial_server() {
        let tmp = Tmp::new("ratio");
        let site = tmp.site();
        // The trivial server answers with the same body size.
        let page = vec![b'x'; 1000];
        write(&site, "index.html", &page);
        quiet();
        let w = Worker::start(&tmp, &site, json!({}));
        let t = Trivial::start(1000);
        let warden_cpu = || run_ns(&format!("/proc/{}/schedstat", w.pid()));
        let trivial_cpu = || t.cpu_ns();
        for (what, keep_alive, n) in [("kept alive", true, 3000), ("new connection", false, 1500)] {
            // Warm up, then take the best of several rounds of each, taken
            // alternately: a noisy neighbour spoils a round, not the best one.
            measure(w.port, 300, keep_alive, &warden_cpu);
            measure(t.port, 300, keep_alive, &trivial_cpu);
            let (mut wc, mut wl, mut tc, mut tl) = (f64::MAX, f64::MAX, f64::MAX, f64::MAX);
            for _ in 0..5 {
                let (c, l) = measure(w.port, n, keep_alive, &warden_cpu);
                wc = wc.min(c);
                wl = wl.min(l);
                let (c, l) = measure(t.port, n, keep_alive, &trivial_cpu);
                tc = tc.min(c);
                tl = tl.min(l);
            }
            eprintln!(
                "{what}: warden {wc:.1} us CPU, {wl:.1} us median; trivial server {tc:.1} us CPU, {tl:.1} us median \
                 (CPU x{:.2}, latency x{:.2})",
                wc / tc,
                wl / tl
            );
            assert!(
                wc <= tc * CPU_FACTOR,
                "{what}: {wc:.1} us of CPU per request against {tc:.1} for a trivial server"
            );
            assert!(
                wl <= tl * LATENCY_FACTOR + 50.0,
                "{what}: {wl:.1} us per request against {tl:.1} for a trivial server"
            );
        }
    }

    /// A good build measures 1.2 to 1.5 times the trivial server's CPU and
    /// latency on a loaded 2-core VM (docs/benchmarks.md); the limits are
    /// about three times that. They catch a gross regression (a thread-pool
    /// hop, a copy of every body), not a single extra system call, which
    /// `system_calls_per_request_stay_low` counts.
    ///
    /// A plain `cargo test` runs the debug build of the server, which uses
    /// about three times the CPU of a release build for the same request
    /// (48 us against 16 us kept alive), so the limits are wider there; the
    /// trivial server is a test helper and is the same in both.
    const CPU_FACTOR: f64 = if cfg!(debug_assertions) { 16.0 } else { 4.0 };
    const LATENCY_FACTOR: f64 = if cfg!(debug_assertions) { 6.0 } else { 3.0 };
}

// ---------------------------------------------------------------------------
// Date and validators (RFC 9110)
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// Seconds since 1970 of an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`).
fn parse_date(s: &str) -> u64 {
    assert!(s.len() == 29 && s.ends_with(" GMT") && &s[3..5] == ", ", "not an HTTP date: {s:?}");
    let num = |r: std::ops::Range<usize>| s[r].parse::<i64>().unwrap_or_else(|_| panic!("not an HTTP date: {s:?}"));
    let m = MONTHS.iter().position(|m| *m == &s[8..11]).unwrap_or_else(|| panic!("month of {s:?}")) as i64 + 1;
    let (y, d) = (num(12..16), num(5..7));
    // Days from the civil date (H. Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    (days * 86_400 + num(17..19) * 3600 + num(20..22) * 60 + num(23..25)) as u64
}

/// The HTTP date of `secs` (seconds since 1970).
fn http_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let wd = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"][days.rem_euclid(7) as usize];
    let mon = MONTHS[(m - 1) as usize];
    format!("{wd}, {d:02} {mon} {y} {:02}:{:02}:{:02} GMT", rem / 3600, rem / 60 % 60, rem % 60)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// Give `rel` the modification time `secs` and `nanos` (seconds since 1970).
fn set_mtime(site: &Path, rel: &str, secs: u64, nanos: u32) {
    let f = std::fs::File::options().write(true).open(site.join(rel)).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::new(secs, nanos)).unwrap();
}

/// `r` has a Date, an HTTP date of the time it was sent (between `before`
/// and now, in seconds), and a Last-Modified, if any, before it. Gives the
/// Date in seconds.
fn dated(r: &Resp, before: u64, what: &str) -> u64 {
    let date = r.h("date");
    assert!(!date.is_empty(), "{what}: no Date ({:?})", r.headers);
    let t = parse_date(date);
    let after = unix_now();
    assert!(before <= t && t <= after, "{what}: Date {date} is not when it was sent ({before}..={after})");
    if !r.h("last-modified").is_empty() {
        assert!(parse_date(r.h("last-modified")) < t, "{what}: Last-Modified not before the Date: {:?}", r.headers);
    }
    t
}

#[test]
fn the_date_helpers_agree_with_each_other() {
    assert_eq!(parse_date("Sun, 06 Nov 1994 08:49:37 GMT"), 784_111_777);
    assert_eq!(http_date(784_111_777), "Sun, 06 Nov 1994 08:49:37 GMT");
    for t in [0, 951_782_400, 1_790_769_601, unix_now()] {
        assert_eq!(parse_date(&http_date(t)), t, "{t}");
    }
}

/// Every response says when it was made: files, 304s, ranges, errors,
/// redirects, from the cache or not (a cached one says the time it is sent,
/// never the time it was cached), and a Last-Modified is never after it.
#[test]
fn every_response_says_when_it_was_made() {
    for cache in [false, true] {
        let tmp = Tmp::new(if cache { "date-cache" } else { "date" });
        let site = make_site(&tmp);
        quiet();
        let cfg = if cache { json!({ "cache_size": "16MB", "cache_valid_ms": 600_000 }) } else { json!({}) };
        let w = Worker::start(&tmp, &site, cfg);
        let what = |s: &str| format!("{s} (cache: {cache})");
        let mut c = Client::connect(w.port);
        let t0 = unix_now();

        // Files, asked twice a second apart: the second answer (a cache hit,
        // with a cache; in a memfd for the 20 KB file) says the later time.
        for (path, body) in [("/style.css", b"a{}".to_vec()), ("/mid20k.bin", pattern(20_000))] {
            let a = c.get(path, "");
            assert_eq!((a.status, &a.body), (200, &body), "{}", what(path));
            let ta = dated(&a, t0, &what(path));
            std::thread::sleep(Duration::from_millis(1100));
            let t1 = unix_now();
            let b = c.get(path, "");
            assert_eq!((b.status, &b.body), (200, &body), "{}", what(path));
            let tb = dated(&b, t1, &what(path));
            assert!(tb > ta, "{}: the second answer is dated {} like the first", what(path), b.h("date"));
            assert_eq!((b.h("etag"), b.h("last-modified")), (a.h("etag"), a.h("last-modified")));
            let close = Client::connect(w.port).get(path, "Connection: close\r\n");
            assert_eq!((close.status, close.h("connection")), (200, "close"));
            dated(&close, t1, &what(&format!("{path}, Connection: close")));
            let head = c.head(path, "");
            assert_eq!(head.h("content-length"), body.len().to_string());
            dated(&head, t1, &what(&format!("HEAD {path}")));
        }

        // A 304 says what the 200 would have: Date, ETag, Cache-Control, Vary.
        let ok = c.get("/style.css", "");
        for cond in
            [format!("If-None-Match: {}", ok.h("etag")), format!("If-Modified-Since: {}", ok.h("last-modified"))]
        {
            let nm = c.get("/style.css", &format!("{cond}\r\n"));
            assert_eq!(nm.status, 304, "{}", what(&cond));
            dated(&nm, t0, &what(&cond));
            for h in ["etag", "cache-control", "vary", "last-modified"] {
                assert_eq!(nm.h(h), ok.h(h), "{}: {h}", what(&cond));
            }
            assert!(nm.h("content-type").is_empty() && nm.body.is_empty());
        }

        // Ranges, errors and redirects.
        for (req, status) in [
            ("GET /big.bin HTTP/1.1\r\nRange: bytes=0-9\r\n", 206),
            ("GET /style.css HTTP/1.1\r\nRange: bytes=1000-\r\n", 416),
            ("GET /missing.txt HTTP/1.1\r\n", 404),
            ("GET /sub HTTP/1.1\r\n", 301),
            ("POST /style.css HTTP/1.1\r\nContent-Length: 0\r\n", 405),
            ("GET /style.css HTTP/1.1\r\nIf-Match: \"x\"\r\n", 412),
            ("GET /%2e%2e/x HTTP/1.1\r\n", 403),
        ] {
            c.send(&format!("{req}Host: x\r\n\r\n"));
            let r = c.recv(false);
            assert_eq!(r.status, status, "{}", what(req));
            dated(&r, t0, &what(req));
        }
        drop(c);
        drop(w);

        let locked = Worker::start(&tmp, &site, json!({ "basic_auth": "u:p" }));
        let r = Client::connect(locked.port).get("/style.css", "");
        assert_eq!(r.status, 401);
        dated(&r, t0, &what("401"));
    }
}

/// Validators that never pass a changed file off as the one the client has
/// (RFC 9110, 8.8 and 13): an edit within the second of the last one is a
/// new ETag; a file stamped in the future has no Last-Modified, and a date
/// ahead of the server's clock validates nothing; a range is resumed only
/// for the same version; If-Match and If-Unmodified-Since get a 412 for any
/// other one.
#[test]
fn validators_tell_versions_apart() {
    for cache in [false, true] {
        let tmp = Tmp::new(if cache { "validators-cache" } else { "validators" });
        let site = tmp.site();
        let then = unix_now() - 100;
        write(&site, "v.txt", b"aaaa");
        set_mtime(&site, "v.txt", then, 100_000_000);
        write(&site, "r.txt", b"0123456789");
        set_mtime(&site, "r.txt", then, 0);
        write(&site, "future.txt", b"from the future");
        set_mtime(&site, "future.txt", unix_now() + 3600, 0);
        // With a cache, every hit is checked against the disk first; the files
        // are cached once they have been still for a while.
        let cfg = if cache { json!({ "cache_size": "16MB", "cache_valid_ms": 0 }) } else { json!({}) };
        if cache {
            quiet();
        }
        let w = Worker::start(&tmp, &site, cfg);
        let what = |s: &str| format!("{s} (cache: {cache})");
        let mut c = Client::connect(w.port);
        let get = |c: &mut Client, path: &str, extra: &str| c.get(path, &format!("{extra}\r\n"));

        // Same size, same second, another version: another ETag. The date
        // can't tell them apart; the tag can, and a client with the old one
        // gets the new file.
        let v1 = c.get("/v.txt", "");
        assert_eq!((v1.status, v1.body.as_slice()), (200, &b"aaaa"[..]));
        assert_eq!(v1.h("last-modified"), http_date(then));
        assert_eq!(get(&mut c, "/v.txt", &format!("If-None-Match: {}", v1.h("etag"))).status, 304);
        write(&site, "v.txt", b"bbbb");
        set_mtime(&site, "v.txt", then, 600_000_000);
        let v2 = c.get("/v.txt", "");
        assert_eq!((v2.status, v2.body.as_slice()), (200, &b"bbbb"[..]), "{}", what("same-second edit"));
        assert_eq!(v2.h("last-modified"), v1.h("last-modified"));
        assert_ne!(v2.h("etag"), v1.h("etag"), "{}", what("same-second edit"));
        let old = get(&mut c, "/v.txt", &format!("If-None-Match: {}", v1.h("etag")));
        assert_eq!((old.status, old.body.as_slice()), (200, &b"bbbb"[..]), "{}", what("the old tag"));
        assert_eq!(get(&mut c, "/v.txt", &format!("If-None-Match: {}", v2.h("etag"))).status, 304);

        // A file stamped ahead of the clock: no Last-Modified (it would say
        // the file changed after the response was made), and no 304 for a
        // date at all.
        let f = c.get("/future.txt", "");
        assert_eq!(f.status, 200);
        assert!(f.h("last-modified").is_empty() && !f.h("etag").is_empty(), "{:?}", f.headers);
        dated(&f, then, &what("future file"));
        for since in [unix_now(), unix_now() + 7200] {
            let r = get(&mut c, "/future.txt", &format!("If-Modified-Since: {}", http_date(since)));
            assert_eq!(r.status, 200, "{}", what(&http_date(since)));
        }
        assert_eq!(get(&mut c, "/future.txt", &format!("If-None-Match: {}", f.h("etag"))).status, 304);

        // If-Modified-Since: the file's own date and later ones up to now
        // validate; one ahead of the server's clock does not.
        let r = c.get("/r.txt", "");
        let lm = r.h("last-modified").to_string();
        assert_eq!(lm, http_date(then));
        assert_eq!(get(&mut c, "/r.txt", &format!("If-Modified-Since: {lm}")).status, 304);
        assert_eq!(get(&mut c, "/r.txt", &format!("If-Modified-Since: {}", http_date(then + 50))).status, 304);
        assert_eq!(get(&mut c, "/r.txt", &format!("If-Modified-Since: {}", http_date(then - 1))).status, 200);
        let ahead = get(&mut c, "/r.txt", &format!("If-Modified-Since: {}", http_date(unix_now() + 3600)));
        assert_eq!((ahead.status, ahead.body.as_slice()), (200, &b"0123456789"[..]), "{}", what("IMS ahead"));

        // If-Range: the rest of the same version, or all of this one.
        let range =
            |c: &mut Client, if_range: &str| get(c, "/r.txt", &format!("Range: bytes=2-4\r\nIf-Range: {if_range}"));
        let same = range(&mut c, &lm);
        assert_eq!((same.status, same.body.as_slice()), (206, &b"234"[..]), "{}", what("If-Range: its date"));
        for other in [http_date(then - 1), http_date(then + 1), r.h("etag").to_string(), "\"x\"".into()] {
            let full = range(&mut c, &other);
            assert_eq!((full.status, full.body.as_slice()), (200, &b"0123456789"[..]), "{}", what(&other));
        }

        // If-Match (strong: no weak tag meets it) and If-Unmodified-Since.
        for (cond, status) in [
            ("If-Match: *".to_string(), 200),
            (format!("If-Match: {}", r.h("etag")), 412),
            (format!("If-Unmodified-Since: {lm}"), 200),
            (format!("If-Unmodified-Since: {}", http_date(then - 10)), 412),
        ] {
            let g = get(&mut c, "/r.txt", &cond);
            assert_eq!(g.status, status, "{}", what(&cond));
            if status == 412 {
                assert_eq!(g.body, b"412 Precondition Failed\n");
            }
            let h = c.head("/r.txt", &format!("{cond}\r\n"));
            assert_eq!(h.status, status, "{}", what(&format!("HEAD, {cond}")));
        }
        drop(c);
        // With a cache, the answers above came from it, and the edit dropped
        // the old version.
        let log = w.stop();
        if cache {
            let line = log.lines().find(|l| l.starts_with("static cache:")).unwrap_or_else(|| panic!("{log}"));
            let n = |what: &str| -> u64 {
                let at = line.find(what).unwrap_or_else(|| panic!("{line}"));
                line[..at].trim_end().rsplit([' ', ',']).next().unwrap().parse().unwrap()
            };
            assert!(n(" hits") >= 5 && n(" dropped as changed on disk") >= 1, "{line}");
        }
    }
}

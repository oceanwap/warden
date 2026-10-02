//! Static file serving: the speed guards and `html_max_age`.
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
use std::time::{Duration, Instant};

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
        let dir = std::env::temp_dir().join(format!("warden-perf-{name}-{}", std::process::id()));
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
}

impl Worker {
    /// `extra`: keys of the `[static]` section (`root` and `host` are set).
    fn start(tmp: &Tmp, root: &Path, extra: Value) -> Worker {
        let port = free_port();
        let mut cfg = json!({ "root": root, "host": "127.0.0.1" });
        for (k, v) in extra.as_object().unwrap() {
            cfg[k] = v.clone();
        }
        let log = std::fs::File::create(tmp.0.join(format!("worker-{port}.log"))).unwrap();
        let child = Command::new(bin())
            .arg("serve-static")
            .env("WARDEN_STATIC", cfg.to_string())
            .env("PORT", port.to_string())
            // What the supervisor always sets: the worker's liveness timer.
            .env("WARDEN_HEARTBEAT_MS", "1000")
            .env("WARDEN_DRAIN_MS", "500")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(log)
            .spawn()
            .unwrap();
        let mut w = Worker { child, port };
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
    let want: Vec<(&str, u16, String)> = vec![
        ("/", 200, pages.to_string()),
        ("/page.html", 200, pages.to_string()),
        ("/sub/", 200, pages.to_string()),
        // The error page is never kept: a 404 must not outlive the fix.
        ("/missing", 404, "no-cache".to_string()),
        ("/style.css", 200, css.to_string()),
        ("/app.3f9a2c1b.js", 200, JS_FINGERPRINTED.to_string()),
    ];
    assert_eq!(got, want.as_slice());
}

#[test]
fn html_pages_follow_html_max_age_and_other_files_do_not() {
    let tmp = Tmp::new("html-age");
    let site = make_site(&tmp);
    quiet();

    // Unset (the default) and 0: ask again on every page load, as before.
    for extra in [json!({}), json!({ "html_max_age": 0 })] {
        let w = Worker::start(&tmp, &site, extra);
        expect(&policies(&w, ""), "no-cache", "public, max-age=3600");
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
    expect(&got, "private, max-age=60", "public, max-age=3600");
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
    assert_eq!(css.h("cache-control"), "public, max-age=3600");
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

#[test]
fn cached_and_large_bodies_arrive_intact_on_kept_alive_connections() {
    let tmp = Tmp::new("bodies");
    let site = make_site(&tmp);
    quiet();
    let w = Worker::start(&tmp, &site, json!({}));

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
        let cached = Worker::start(&tmp, &site, json!({}));
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
        for (what, extra) in [("cache on", json!({})), ("cache off", json!({ "cache_size": 0 }))] {
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
        let out = std::env::temp_dir().join(format!("warden-perf-strace-{}-{pid}", std::process::id()));
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
        let w = Worker::start(&tmp, &site, json!({}));
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
        eprintln!("system calls per request: {keep:.2} kept alive, {fresh:.2} on a new connection");
        // Kept alive: wait for readiness, read the request, send the response
        // (3 measured). A new connection adds accept, the epoll registration
        // and the close (9 measured; the worker used to set TCP_NODELAY on
        // each connection and shut it down before closing: 11). One system
        // call more per request is a regression; the margin is the heartbeat
        // timer and the freshness check of a cached file, which a slower
        // (debug) build runs more often per request (10.02 measured there).
        assert!(keep <= 3.5, "{keep:.2} system calls per kept-alive request");
        assert!(fresh <= 10.5, "{fresh:.2} system calls per new-connection request");
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

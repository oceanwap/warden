//! Warden's built-in static file server (`warden serve <dir> <port>`, like
//! `pm2 serve` or `npx serve`). It runs as the worker process of an app with
//! a `[static]` section, so it is supervised like any app: several workers
//! share the port with SO_REUSEPORT, each reports readiness and a heartbeat
//! on Warden's IPC pipe and serves a private health socket, and it drains on
//! the stop signal, so rolling restarts drop no requests.
//!
//! HTTP/1.1 with keep-alive; GET and HEAD; ETag / Last-Modified with 304s;
//! single byte ranges (206); precompressed `.br` / `.gz` siblings; SPA
//! fallback; `404.html`; Basic auth; extra headers. Paths can't leave the
//! root (`..`, symlinks pointing outside, NUL), dotfiles are hidden by
//! default, request heads are capped at 16 KB and must arrive within 10 s (to within
//! a second).
//!
//! Small files are answered from an in-memory cache of complete responses
//! (`cache.rs`: one send(2) per hit, or one sendfile(2) from a sealed memfd
//! for bodies of 8 KB and more). Connections are driven by tokio
//! (epoll). An io_uring transport was tried and dropped: it was slower than
//! epoll with the cache (docs/benchmarks.md).
//!
//! Per request the code allocates nothing: the head is parsed where it lies
//! in a reused buffer (`Request` borrows it), and the waits for a request
//! are limited by one sweep a second (`idle.rs`), not a timer each. A new
//! connection whose request is already in the socket and whose answer is
//! cached never becomes a task: `first_request` answers it from the accept
//! loop.

mod cache;
mod idle;

use crate::config::Static;
use cache::{Cache, Dep, Entry, Lookup, Seen, Stamp};
use std::borrow::Cow;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const MAX_HEAD: usize = 16 * 1024;
const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_CONNECTIONS: usize = 10_000;

struct Site {
    root: PathBuf,
    /// The root directory, opened once: files are opened relative to it.
    dir: Arc<OwnedFd>,
    /// How files are opened (OPEN_*); can only step down, never back up.
    open_mode: AtomicU8,
    /// RESOLVE_CACHED lookups answered from the dentry cache / not.
    cached_hits: AtomicU64,
    cached_misses: AtomicU64,
    cfg: Static,
    auth: Option<String>,
    /// Prebuilt responses of small files (None: `cache_size = 0`).
    cache: Option<Cache>,
    draining: AtomicBool,
    active: AtomicUsize,
    /// Limits on connections waiting for a request (see `idle.rs`).
    idle: Arc<idle::Idle>,
    /// How long the first request's head may take, and the wait between
    /// requests on a kept-alive connection. `WARDEN_STATIC_TIMEOUTS=<head
    /// seconds>,<idle seconds>` changes them (tests, troubleshooting).
    head_timeout: Duration,
    idle_timeout: Duration,
    /// Request-head buffers of finished connections, for the next ones.
    bufs: Mutex<Vec<Vec<u8>>>,
    /// Requests answered by the accept loop itself (`first_request`).
    inline: AtomicU64,
}

/// Entry point of `warden serve-static` (started by the supervisor).
pub fn main() -> i32 {
    let cfg: Static = match std::env::var("WARDEN_STATIC")
        .map_err(|e| e.to_string())
        .and_then(|j| serde_json::from_str(&j).map_err(|e| e.to_string()))
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warden serve-static: WARDEN_STATIC is missing or invalid ({e}); this command is run by Warden");
            return 78;
        }
    };
    let port: u16 = match std::env::var("PORT").ok().and_then(|p| p.parse().ok()) {
        Some(p) => p,
        None => {
            eprintln!("warden serve-static: PORT is not set; set app.port in the config");
            return 78;
        }
    };
    // Resolve `current` symlinks once: this worker serves one release.
    let root = match std::fs::canonicalize(&cfg.root) {
        Ok(r) if r.is_dir() => r,
        Ok(r) => {
            eprintln!("warden serve-static: {} is not a directory", r.display());
            return 78;
        }
        Err(e) => {
            eprintln!("warden serve-static: static.root {}: {e}", cfg.root.display());
            return 78;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("warden serve-static: cannot start the runtime: {e}");
            return 1;
        }
    };
    let dir = match std::fs::File::open(&root) {
        Ok(f) => Arc::new(OwnedFd::from(f)),
        Err(e) => {
            eprintln!("warden serve-static: cannot open static.root {}: {e}", root.display());
            return 78;
        }
    };
    let auth = cfg.basic_auth.as_deref().map(|a| format!("Basic {}", base64(a.as_bytes())));
    // Troubleshooting and tests: force a slower way of opening files.
    let open_mode = match std::env::var("WARDEN_STATIC_OPEN").as_deref() {
        // Not Linux: no openat2 (sys::openat2 is Unsupported), realpath only.
        _ if !cfg!(target_os = "linux") => OPEN_LEGACY,
        Ok("beneath") => OPEN_BENEATH,
        Ok("legacy") => OPEN_LEGACY,
        _ => OPEN_CACHED,
    };
    let cache = Cache::new(cfg.cache_size, cfg.cache_max_file, cfg.cache_valid_ms, crate::sys::nofile_limit().0);
    let (head_timeout, idle_timeout) = std::env::var("WARDEN_STATIC_TIMEOUTS")
        .ok()
        .and_then(|v| {
            let (h, i) = v.split_once(',')?;
            Some((Duration::from_secs(h.trim().parse().ok()?), Duration::from_secs(i.trim().parse().ok()?)))
        })
        .unwrap_or((HEAD_TIMEOUT, IDLE_TIMEOUT));
    let site = Arc::new(Site {
        root,
        dir,
        open_mode: AtomicU8::new(open_mode),
        cached_hits: AtomicU64::new(0),
        cached_misses: AtomicU64::new(0),
        cfg,
        auth,
        cache,
        draining: AtomicBool::new(false),
        active: AtomicUsize::new(0),
        idle: idle::Idle::new(),
        head_timeout,
        idle_timeout,
        bufs: Mutex::new(Vec::new()),
        inline: AtomicU64::new(0),
    });
    match rt.block_on(serve(site, port)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("warden serve-static: {e}");
            1
        }
    }
}

fn reuseport_listener(host: &str, port: u16, reuse_port: bool) -> Result<std::net::TcpListener, String> {
    let ip: std::net::IpAddr = host.parse().map_err(|_| format!("static.host {host:?} is not an IP address"))?;
    let addr = std::net::SocketAddr::new(ip, port);
    crate::sys::listen_tcp(addr, reuse_port, 1024).map_err(|e| format!("cannot listen on {addr}: {e}"))
}

/// One JSON line to Warden on the IPC pipe (fd from WARDEN_IPC_FD).
fn report(msg: serde_json::Value) {
    let Some(fd) = std::env::var("WARDEN_IPC_FD").ok().and_then(|v| v.parse::<i32>().ok()) else { return };
    let mut line = msg.to_string();
    line.push('\n');
    // A lost message only delays readiness/heartbeat; Warden handles that.
    let _ = crate::sys::write_fd(fd, line.as_bytes());
}

/// Where TCP connections come from. The listening socket is registered with
/// tokio's reactor (epoll; kqueue outside Linux) so the loop can wait for it,
/// but connections are taken off it here, as bare descriptors: a connection
/// that is answered at once never needs to be registered with the reactor at
/// all (see `first_request`).
struct Listener(tokio::io::unix::AsyncFd<std::net::TcpListener>);

impl Listener {
    fn new(l: std::net::TcpListener) -> Result<Listener, String> {
        tokio::io::unix::AsyncFd::with_interest(l, tokio::io::Interest::READABLE)
            .map(Listener)
            .map_err(|e| e.to_string())
    }

    fn name(&self) -> &'static str {
        if cfg!(target_os = "linux") { "epoll" } else { "kqueue" }
    }

    async fn accept(&self) -> std::io::Result<OwnedFd> {
        loop {
            // `AsyncFd::readable` does not count against tokio's cooperative
            // budget (its own `TcpListener::accept` does), and a connection
            // that is answered inline never awaits. With the backlog never
            // empty this loop would then never give the thread back: no
            // heartbeat, no SIGTERM, no idle sweep, no established
            // connection served. Spend one unit here, before anything is
            // accepted, so the future can be suspended (and dropped by the
            // `select!`) without holding a descriptor.
            tokio::task::consume_budget().await;
            let mut ready = self.0.readable().await?;
            if let Ok(r) = ready.try_io(|l| crate::sys::accept_nonblocking(l.get_ref().as_fd())) {
                return r;
            }
        }
    }
}

/// After a failed accept, the next comes at once for the first two in a
/// row, then after a pause that doubles from this …
const ACCEPT_PAUSE_MIN: Duration = Duration::from_millis(5);
/// … to this: a lack of descriptors or memory must not spin a CPU (the
/// listener stays ready, the connection waiting in its backlog).
const ACCEPT_PAUSE_MAX: Duration = Duration::from_secs(1);
/// Failed accepts are logged at most this often, with how many there were.
const ACCEPT_LOG_EVERY: Duration = Duration::from_secs(10);

/// Failed accepts on one listener (both I/O modes, and the health socket).
struct AcceptErrors {
    /// What it accepts ("connections").
    what: &'static str,
    /// How ("epoll", "kqueue").
    via: &'static str,
    /// Failures in a row.
    streak: u32,
    /// Failures since the last log line.
    unlogged: u64,
    logged_at: Option<Instant>,
    /// A failure of this streak was logged: say when it ends.
    told: bool,
    /// Accepting is paused until then.
    resume_at: Option<tokio::time::Instant>,
}

impl AcceptErrors {
    fn new(what: &'static str, via: &'static str) -> AcceptErrors {
        AcceptErrors { what, via, streak: 0, unlogged: 0, logged_at: None, told: false, resume_at: None }
    }

    /// Until `until` (never, without one).
    async fn pause(until: Option<tokio::time::Instant>) {
        match until {
            Some(t) => tokio::time::sleep_until(t).await,
            None => std::future::pending().await,
        }
    }

    fn accepted(&mut self) {
        if self.told {
            crate::info!(
                format!("accepting {} again", self.what),
                via = self.via,
                after_failures = self.streak,
                pid = std::process::id()
            );
            self.told = false;
        }
        self.streak = 0;
    }

    fn failed(&mut self, e: &std::io::Error) {
        self.streak = self.streak.saturating_add(1);
        let pause = accept_pause(self.streak);
        if !pause.is_zero() {
            self.resume_at = Some(tokio::time::Instant::now() + pause);
        }
        // A client that left before it was accepted is routine, unless it
        // keeps happening.
        if accept_error_is_transient(e) && self.streak < 10 {
            return;
        }
        self.unlogged += 1;
        if self.logged_at.is_some_and(|t| t.elapsed() < ACCEPT_LOG_EVERY) {
            return;
        }
        crate::warn!(
            format!("cannot accept {}; they wait in the listen backlog while this worker retries", self.what),
            error = e,
            via = self.via,
            failed = self.unlogged,
            retry_in_ms = pause.as_millis(),
            open_files_limit = crate::sys::nofile_limit().0,
            pid = std::process::id(),
            hint = accept_hint(e),
        );
        self.unlogged = 0;
        self.logged_at = Some(Instant::now());
        self.told = true;
    }
}

/// The pause before accepting again after `streak` failures in a row.
fn accept_pause(streak: u32) -> Duration {
    if streak <= 2 {
        return Duration::ZERO;
    }
    (ACCEPT_PAUSE_MIN * (1u32 << (streak - 3).min(16))).min(ACCEPT_PAUSE_MAX)
}

/// accept(2) errors about one connection, not the listener (Linux passes
/// on the new socket's pending network errors): the next one may well work.
fn accept_error_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::ECONNABORTED
                | libc::EINTR
                | libc::EAGAIN
                | libc::EPROTO
                | libc::EPERM
                | libc::ENETDOWN
                | libc::ENETUNREACH
                | libc::EHOSTUNREACH
                | libc::ENOPROTOOPT
                | libc::EOPNOTSUPP
        )
    )
}

fn accept_hint(e: &std::io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(libc::EMFILE) => {
            "this worker has used up its open-file limit (each connection takes a descriptor; up to 10000 per \
             worker): raise the limit Warden and its workers run with, LimitNOFILE=65536 in the systemd unit \
             (`systemctl edit <unit>`) or `ulimit -n 65536` in the shell that runs `warden start`; `warden doctor` \
             shows it"
        }
        Some(libc::ENFILE) => {
            "the whole system is out of file descriptors: raise fs.file-max (`sysctl fs.file-max`), or find what \
             holds them (`lsof -n | wc -l`)"
        }
        Some(libc::ENOBUFS | libc::ENOMEM) => {
            "the kernel is short of memory for sockets: check the host's and the cgroup's memory (`free -m`, \
             memory.max) and `sysctl net.ipv4.tcp_mem`"
        }
        _ => "accept(2) keeps failing on this listener; if it persists, `warden restart <app>` starts fresh workers",
    }
}

async fn serve(site: Arc<Site>, port: u16) -> Result<(), String> {
    let reuse = std::env::var("WARDEN_REUSE_PORT").is_ok_and(|v| v == "1");
    let std_listener = reuseport_listener(&site.cfg.host, port, reuse)?;
    // Wake up for a connection only once its request has arrived. Optional:
    // without it the server is just as correct, a little busier.
    let deferred = crate::sys::tcp_defer_accept(std_listener.as_fd(), site.head_timeout.as_secs() as i32).is_ok();
    // With the request already waiting when the connection is accepted (Linux,
    // TCP_DEFER_ACCEPT), a cached answer can be sent from the accept loop
    // itself, `first_request`. `WARDEN_STATIC_INLINE=0` turns that off (tests,
    // troubleshooting).
    let inline_first = cfg!(target_os = "linux")
        && deferred
        && site.cache.is_some()
        && std::env::var("WARDEN_STATIC_INLINE").as_deref() != Ok("0");
    // TCP_NODELAY (responses go out at once, never held back for Nagle's
    // algorithm) once on the listener: Linux hands it to every connection it
    // accepts, so there is no setsockopt per connection. Where that is not
    // the rule (or setting it failed) each connection gets it when accepted.
    let nodelay_inherited =
        crate::sys::NODELAY_INHERITED && crate::sys::set_tcp_nodelay(std_listener.as_fd(), true).is_ok();
    let listener = Listener::new(std_listener)?;
    let worker: u64 = std::env::var("WARDEN_WORKER_ID").ok().and_then(|v| v.parse().ok()).unwrap_or(0);

    // Private socket for this worker's health checks.
    let mut unix = None;
    if let Ok(dir) = std::env::var("WARDEN_HEALTH_DIR") {
        let app = std::env::var("WARDEN_APP").unwrap_or_else(|_| "app".into());
        let inst = std::env::var("WARDEN_INSTANCE").unwrap_or_else(|_| std::process::id().to_string());
        let path = PathBuf::from(dir).join(format!("{app}.h{inst}-{worker}.sock"));
        if path.as_os_str().len() <= 100 {
            let _ = std::fs::remove_file(&path);
            if let Ok(l) = tokio::net::UnixListener::bind(&path) {
                unix = Some((l, path));
            }
        }
    }
    let mut msg = serde_json::json!({"ev": "listening", "port": port, "worker": worker});
    if let Some((_, p)) = &unix {
        msg["socket"] = p.display().to_string().into();
    }
    report(msg);
    let how = match site.open_mode.load(Ordering::Relaxed) {
        OPEN_CACHED => "openat2, cache-first",
        OPEN_BENEATH => "openat2",
        _ => "realpath check",
    };
    let cache = match &site.cache {
        Some(_) => format!("cache {} KB per worker", site.cfg.cache_size >> 10),
        None => "no cache".to_string(),
    };
    println!(
        "serving {} on {}:{port} (files opened with {how}) via {}, {cache}",
        site.root.display(),
        site.cfg.host,
        listener.name()
    );

    let beat_ms: u64 = std::env::var("WARDEN_HEARTBEAT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if beat_ms > 0 {
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(beat_ms));
            loop {
                t.tick().await;
                report(serde_json::json!({"ev": "heartbeat", "worker": worker}));
            }
        });
    }

    // The limits on connections waiting for a request (idle.rs): one tick a
    // second for all of them, in place of a timer per request.
    let idle = site.idle.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            idle.sweep();
        }
    });

    let stop_name = std::env::var("WARDEN_STOP_SIGNAL").unwrap_or_else(|_| "SIGTERM".into());
    let stop_sig = crate::signals::parse(&stop_name).unwrap_or(libc::SIGTERM);
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(stop_sig))
        .map_err(|e| format!("installing the {stop_name} handler: {e}"))?;
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    let unix_listener = unix.as_ref().map(|(l, _)| l);
    let mut tcp_errors = AcceptErrors::new("connections", listener.name());
    let mut unix_errors = AcceptErrors::new("health checks", "its health socket");
    loop {
        tokio::select! {
            _ = stop.recv() => break,
            _ = AcceptErrors::pause(tcp_errors.resume_at), if tcp_errors.resume_at.is_some() => tcp_errors.resume_at = None,
            _ = AcceptErrors::pause(unix_errors.resume_at), if unix_errors.resume_at.is_some() => unix_errors.resume_at = None,
            acc = listener.accept(), if tcp_errors.resume_at.is_none() => {
                let accepted = match acc {
                    Ok(a) => {
                        tcp_errors.accepted();
                        a
                    }
                    Err(e) => {
                        tcp_errors.failed(&e);
                        continue;
                    }
                };
                let fd = accepted;
                // At the limit a connection is dropped before anything is sent
                // (an answer promising keep-alive, then a close, is worse).
                // Only this loop takes permits, so one free now is still free
                // below.
                if slots.available_permits() == 0 {
                    continue;
                }
                if !nodelay_inherited {
                    let _ = crate::sys::set_tcp_nodelay(fd.as_fd(), true);
                }
                let _ = crate::sys::set_nosigpipe(fd.as_fd());
                let (fd, head, pending, answered) = match first_request(&site, fd, inline_first) {
                    First::Done => continue,
                    First::Go { fd, head, pending, answered } => (fd, head, pending, answered),
                };
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let Ok(stream) = tokio::net::TcpStream::from_std(std::net::TcpStream::from(fd)) else { continue };
                let site = site.clone();
                let guard = site.idle.register(stream.as_raw_fd());
                tokio::spawn(async move {
                    let _permit = permit;
                    // Locals drop in reverse order: the guard (which takes the
                    // connection out of the idle sweep) goes before the stream
                    // closes its descriptor.
                    let mut stream = stream;
                    let guard = guard;
                    // Borrowed halves: dropping them does nothing, and the
                    // stream's own drop is just close(2). Owned halves shut
                    // the write side down first, one more system call for
                    // every connection (the FIN is the same either way).
                    let (r, w) = stream.split();
                    connection(r, Conn::Tcp(w), site, &guard, head, pending, answered).await;
                });
            }
            acc = async { match unix_listener { Some(l) => l.accept().await.map(|(s, _)| s), None => std::future::pending().await } }, if unix_errors.resume_at.is_none() => {
                let stream = match acc {
                    Ok(s) => {
                        unix_errors.accepted();
                        s
                    }
                    Err(e) => {
                        unix_errors.failed(&e);
                        continue;
                    }
                };
                let site = site.clone();
                let guard = site.idle.register(stream.as_raw_fd());
                tokio::spawn(async move {
                    let mut stream = stream;
                    let guard = guard;
                    let (r, w) = stream.split();
                    let head = HeadBuf::take(&site);
                    connection(r, Conn::Unix(w), site, &guard, head, None, false).await;
                });
            }
        }
    }

    // Drain: stop accepting; answer whatever arrives on open connections
    // with `Connection: close` (see `connection`), and let requests in flight
    // finish (at least WARDEN_DRAIN_MS, at most ~grace).
    drop(listener);
    site.draining.store(true, Ordering::SeqCst);
    report(serde_json::json!({"ev": "draining", "worker": worker}));
    let drain_ms: u64 = std::env::var("WARDEN_DRAIN_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(500);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(drain_ms) || site.active.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Some((_, p)) = unix {
        let _ = std::fs::remove_file(p);
    }
    if let Some(c) = &site.cache {
        let (files, used, cap) = c.usage();
        println!(
            "static cache: {} hits, {} misses, {} dropped as changed on disk, {} evicted; {files} files in {} of {} KB \
             ({} of them in memfds); {} requests answered in the accept loop",
            c.hits.load(Ordering::Relaxed),
            c.misses.load(Ordering::Relaxed),
            c.stale.load(Ordering::Relaxed),
            c.evicted.load(Ordering::Relaxed),
            used.div_ceil(1024),
            cap >> 10,
            c.memfds(),
            site.inline.load(Ordering::Relaxed)
        );
    }
    Ok(())
}

/// Bytes to send: a buffer built for this response, or a range of a cached
/// one (no copy).
pub enum OutBuf {
    Vec(Vec<u8>),
    Shared(Arc<[u8]>, std::ops::Range<usize>),
}

impl OutBuf {
    fn as_slice(&self) -> &[u8] {
        match self {
            OutBuf::Vec(v) => v,
            OutBuf::Shared(b, r) => &b[r.clone()],
        }
    }

    /// Without the first `n` bytes.
    fn advance(self, n: usize) -> OutBuf {
        match self {
            OutBuf::Vec(mut v) => {
                v.drain(..n);
                OutBuf::Vec(v)
            }
            OutBuf::Shared(b, r) => OutBuf::Shared(b, r.start + n..r.end),
        }
    }
}

impl From<String> for OutBuf {
    fn from(s: String) -> Self {
        OutBuf::Vec(s.into_bytes())
    }
}

impl From<Vec<u8>> for OutBuf {
    fn from(v: Vec<u8>) -> Self {
        OutBuf::Vec(v)
    }
}

/// The writing half of a client connection. TCP bodies go out with
/// sendfile(2); the Unix health socket uses a plain copy.
enum Conn<'a> {
    Tcp(tokio::net::tcp::WriteHalf<'a>),
    Unix(tokio::net::unix::WriteHalf<'a>),
}

impl Conn<'_> {
    async fn write_all(&mut self, b: impl Into<OutBuf>) -> std::io::Result<()> {
        let b = b.into();
        match self {
            Conn::Tcp(w) => w.write_all(b.as_slice()).await,
            Conn::Unix(w) => w.write_all(b.as_slice()).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(w) => w.flush().await,
            Conn::Unix(w) => w.flush().await,
        }
    }

    /// The response `head` (possibly empty), then `count` bytes of a cached
    /// response's memfd from `offset`: on TCP by page reference (sendfile; the
    /// head goes in the same system call on macOS, and in the same packet on
    /// Linux); on the Unix health socket a plain copy (a memfd read never
    /// waits for a disk).
    async fn send_cached(&mut self, head: &[u8], file: &std::fs::File, offset: u64, count: u64) -> std::io::Result<()> {
        match self {
            Conn::Tcp(w) => sendfile_all(w.as_ref(), head, file, offset, count).await.map(|_| ()),
            Conn::Unix(w) => {
                let mut buf = vec![0u8; usize::try_from(count).map_err(std::io::Error::other)?];
                file.read_exact_at(&mut buf, offset)?;
                w.write_all(head).await?;
                w.write_all(&buf).await
            }
        }
    }

    /// The response `head`, then `count` bytes of `file` from `offset`.
    async fn send_file(&mut self, head: &[u8], file: std::fs::File, offset: u64, count: u64) -> std::io::Result<u64> {
        match self {
            Conn::Tcp(w) => sendfile_all(w.as_ref(), head, &file, offset, count).await,
            Conn::Unix(w) => {
                w.write_all(head).await?;
                let mut f = tokio::fs::File::from_std(file);
                if offset > 0 {
                    use tokio::io::AsyncSeekExt;
                    f.seek(std::io::SeekFrom::Start(offset)).await?;
                }
                tokio::io::copy(&mut f.take(count), w).await
            }
        }
    }
}

/// The most one sendfile(2) call is asked to move. The worker is one thread:
/// while a call runs nobody else is served, and on a loopback or fast local
/// link the kernel takes a whole multi-megabyte file in one call (10 MB:
/// 5 ms, during which every other request on the worker waited). After each
/// chunk the connection yields, so the others get their turn: a 1 KB request
/// next to a 10 MB download took 5.4 ms and takes 0.34 ms (docs/benchmarks.md).
/// Files up to this size still go in one call, as before; a smaller chunk
/// (256 KB) would shorten the wait further but made files of 0.5 to 1 MB
/// slower (an extra reader wake-up per chunk). Big files cost the same CPU.
const SEND_CHUNK: u64 = 1 << 20;

/// Bodies of this size and more go out with TCP_CORK: every segment full, the
/// reader woken less often. Measured on loopback: a 2 MB file 1.06 ms
/// uncorked, 0.70 corked; 10 MB 5.5 ms, 3.5. Below it corking costs more than
/// it saves (a 256 KB file: 41 us of CPU uncorked, 97 corked), and between
/// 1 and 1.25 MB the two are within the noise of this machine.
const CORK_MIN: u64 = 5 << 18;

/// TCP_CORK on for the length of a big body; off again, which sends what is
/// held, however the transfer ends.
struct Corked<'a>(std::os::fd::BorrowedFd<'a>);

impl Drop for Corked<'_> {
    fn drop(&mut self) {
        let _ = crate::sys::set_tcp_cork(self.0, false);
    }
}

/// Zero-copy body: the kernel moves file pages to the socket, no userspace
/// buffer, after the response `head` (empty when it is already part of
/// what the file holds). In chunks of `SEND_CHUNK`, yielding between them;
/// loops on partial sends and waits for writability on EAGAIN. Where the
/// system can, the head and the start of the body are one system call and
/// one packet (`sys::sendfile_head`).
async fn sendfile_all(
    sock: &tokio::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    offset: u64,
    count: u64,
) -> std::io::Result<u64> {
    let mut head_left = head;
    let mut off = offset as i64;
    let mut left = count;
    let _cork =
        (count >= CORK_MIN && crate::sys::set_tcp_cork(sock.as_fd(), true).is_ok()).then(|| Corked(sock.as_fd()));
    while !head_left.is_empty() || left > 0 {
        sock.writable().await?;
        let chunk = left.min(SEND_CHUNK) as usize;
        let asked = head_left.len() + chunk;
        let res = sock.try_io(tokio::io::Interest::WRITABLE, || {
            crate::sys::sendfile_head(sock.as_fd(), file.as_fd(), &mut off, chunk, head_left)
        });
        match res {
            // The file shrank under us: the promised Content-Length can't be
            // met, so the connection must close.
            Ok(0) => {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file truncated while sending"));
            }
            Ok(n) => match crate::sys::split_head_body(n, head_left.len(), left) {
                Some((h, b)) => {
                    head_left = &head_left[h..];
                    left -= b;
                    // A whole chunk went in and there is more: let the other
                    // connections run before the next one. (A short send means
                    // the socket is full; waiting for it does that already.)
                    if left > 0 && n >= asked {
                        tokio::task::yield_now().await;
                    }
                }
                None => return Err(std::io::Error::other("sendfile reported more bytes than were asked for")),
            },
            // Nothing of this call went out (`sendfile_head` reports a head that
            // did as an Ok, also when the body then met EAGAIN or EINTR), so
            // trying again cannot send the head twice.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(count)
}

/// Files up to this size are copied into the response and sent with the
/// headers in one write; larger ones go headers (MSG_MORE) + sendfile.
/// Measured (bench/static.ts, 48 KB file): 55k req/s copied vs 62–68k with
/// sendfile; a 1.5 KB page is the same either way. 16 KB is past the point
/// where the copy costs more than the extra syscall.
const SMALL_FILE: u64 = 16 * 1024;

/// One request head, borrowed from the connection's read buffer: nothing is
/// copied or allocated per request. The headers the server looks at on every
/// request are picked out in one pass over the head; any other name is
/// searched for in `rest` when asked for. A name that appears twice gives its
/// first value.
struct Request<'a> {
    method: &'a str,
    path: &'a str,
    keep_alive: bool,
    accept_encoding: Option<&'a str>,
    range: Option<&'a str>,
    if_none_match: Option<&'a str>,
    if_modified_since: Option<&'a str>,
    authorization: Option<&'a str>,
    accept: Option<&'a str>,
    /// The header lines, as received.
    rest: &'a str,
}

impl<'a> Request<'a> {
    fn header(&self, name: &str) -> Option<&'a str> {
        let hot = match name {
            "accept-encoding" => Some(self.accept_encoding),
            "range" => Some(self.range),
            "if-none-match" => Some(self.if_none_match),
            "if-modified-since" => Some(self.if_modified_since),
            "authorization" => Some(self.authorization),
            "accept" => Some(self.accept),
            _ => None,
        };
        if let Some(v) = hot {
            return v;
        }
        self.rest.split('\n').find_map(|l| {
            let (k, v) = l.strip_suffix('\r').unwrap_or(l).split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

/// A connection's bytes read but not yet used: the request head being
/// received, and anything the client sent after it. Buffers are reused from
/// one connection to the next (`Site::bufs`).
struct HeadBuf {
    buf: Vec<u8>,
    /// Where the unused bytes start.
    pos: usize,
    /// How far into the head being received (from `pos`) `find_head` has
    /// looked: the start of its first unfinished line. A head arriving a few
    /// bytes at a time is then scanned once, not once per read.
    scan: usize,
}

const HEAD_BUF: usize = 8 * 1024;
const HEAD_BUFS_KEPT: usize = 256;

impl HeadBuf {
    fn take(site: &Site) -> HeadBuf {
        let buf = site.bufs.lock().unwrap_or_else(|e| e.into_inner()).pop();
        HeadBuf { buf: buf.unwrap_or_else(|| Vec::with_capacity(HEAD_BUF)), pos: 0, scan: 0 }
    }

    fn give_back(mut self, site: &Site) {
        // A buffer that grew past the head limit is not worth keeping.
        if self.buf.capacity() <= 2 * MAX_HEAD {
            self.buf.clear();
            let mut pool = site.bufs.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < HEAD_BUFS_KEPT {
                pool.push(self.buf);
            }
        }
    }

    /// The head just found has been served: its `n` bytes are used up.
    fn consume(&mut self, n: usize) {
        self.pos += n;
        self.scan = 0;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
    }

    /// Room to read more: the bytes already used are dropped from the front.
    fn make_room(&mut self) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            self.scan = 0;
        } else if self.pos > 0 && self.buf.capacity() - self.buf.len() < 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.capacity() - self.buf.len() < 1024 {
            self.buf.reserve(HEAD_BUF);
        }
    }
}

/// Where the head in `buf` ends.
#[derive(Debug, PartialEq)]
enum Head {
    /// `len` bytes from `skip` on are a whole head (up to and including its
    /// blank line); `skip` is the blank lines before it (a client may send a
    /// stray one between requests).
    Done { skip: usize, len: usize },
    /// Not all here yet. `skip` is the blank lines before the head; `scan`
    /// where to pick up the search (from the head's start, after `skip`) when
    /// more has arrived: the first line not yet ended.
    More { skip: usize, scan: usize },
    /// Over the 16 KB limit before its blank line.
    TooLong,
}

/// Find the end of the head at the start of `buf`: the first blank line after
/// at least one other line, `\r\n` or a bare `\n` ending each line. A head
/// may be at most `MAX_HEAD` bytes with its blank line.
fn find_head(buf: &[u8]) -> Head {
    find_head_from(buf, 0)
}

/// `find_head`, resuming after an earlier `More`: `buf` starts at the head
/// (the blank lines before it dropped) and its lines before `scan` are known
/// not to end it.
fn find_head_from(buf: &[u8], scan: usize) -> Head {
    let mut start = 0;
    if scan == 0 {
        loop {
            match &buf[start..] {
                [b'\n', ..] => start += 1,
                [b'\r', b'\n', ..] => start += 2,
                // Perhaps the first half of a blank line.
                [b'\r'] => return Head::More { skip: start, scan: 0 },
                _ => break,
            }
        }
    }
    let mut line = start + scan;
    loop {
        let Some(nl) = memchr::memchr(b'\n', &buf[line..]) else {
            return if buf.len() - start > MAX_HEAD {
                Head::TooLong
            } else {
                Head::More { skip: start, scan: line - start }
            };
        };
        let end = line + nl + 1;
        if end - start > MAX_HEAD {
            return Head::TooLong;
        }
        let l = &buf[line..end];
        if line > start && (l == b"\n" || l == b"\r\n") {
            return Head::Done { skip: start, len: end - start };
        }
        line = end;
    }
}

/// Read until one whole request head is in the buffer; its length (from
/// `h.pos`). `Ok(None)`: the client closed between requests. Errors are the
/// status to answer with: 431 over the head limit, 400 for a read error or a
/// close in the middle of a head. A line is never read past the limit, so a
/// client that never sends a newline gets 431, not unbounded buffering.
async fn read_head<R: tokio::io::AsyncRead + Unpin>(r: &mut R, h: &mut HeadBuf) -> Result<Option<usize>, u16> {
    loop {
        match find_head_from(&h.buf[h.pos..], h.scan) {
            Head::Done { skip, len } => {
                h.pos += skip;
                h.scan = 0;
                return Ok(Some(len));
            }
            Head::TooLong => return Err(431),
            Head::More { skip, scan } => {
                h.pos += skip;
                h.scan = scan;
            }
        }
        h.make_room();
        let n = r.read_buf(&mut h.buf).await.map_err(|_| 400u16)?;
        if n == 0 {
            return if h.pos == h.buf.len() { Ok(None) } else { Err(400) };
        }
    }
}

/// Parse a head found by `find_head`. Errors: 400 for bytes that are not
/// UTF-8 or a request line without a target.
fn parse_head(bytes: &[u8]) -> Result<Request<'_>, u16> {
    let text = std::str::from_utf8(bytes).map_err(|_| 400u16)?;
    let b = text.as_bytes();
    let first_end = memchr::memchr(b'\n', b).unwrap_or(b.len());
    let rest = text.get(first_end + 1..).unwrap_or("");
    let first = text[..first_end].strip_suffix('\r').unwrap_or(&text[..first_end]);
    let mut parts = first.split(' ');
    let (method, target, version) =
        (parts.next().unwrap_or(""), parts.next().ok_or(400u16)?, parts.next().unwrap_or(""));
    let mut req = Request {
        method,
        path: target,
        keep_alive: false,
        accept_encoding: None,
        range: None,
        if_none_match: None,
        if_modified_since: None,
        authorization: None,
        accept: None,
        rest,
    };
    let mut connection = None;
    let mut at = 0;
    let r = rest.as_bytes();
    while at < r.len() {
        let end = memchr::memchr(b'\n', &r[at..]).map_or(r.len(), |i| at + i);
        let line = &rest[at..end];
        at = end + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(colon) = memchr::memchr(b':', line.as_bytes()) else { continue };
        let (k, v) = (line[..colon].trim(), line[colon + 1..].trim());
        let slot = match k.len() {
            5 if k.eq_ignore_ascii_case("range") => &mut req.range,
            6 if k.eq_ignore_ascii_case("accept") => &mut req.accept,
            10 if k.eq_ignore_ascii_case("connection") => &mut connection,
            13 if k.eq_ignore_ascii_case("if-none-match") => &mut req.if_none_match,
            13 if k.eq_ignore_ascii_case("authorization") => &mut req.authorization,
            15 if k.eq_ignore_ascii_case("accept-encoding") => &mut req.accept_encoding,
            17 if k.eq_ignore_ascii_case("if-modified-since") => &mut req.if_modified_since,
            _ => continue,
        };
        slot.get_or_insert(v);
    }
    req.keep_alive = match version {
        "HTTP/1.1" => !connection.is_some_and(|c| c.eq_ignore_ascii_case("close")),
        _ => connection.is_some_and(|c| c.eq_ignore_ascii_case("keep-alive")),
    };
    Ok(req)
}

/// One client connection: requests in a loop while keep-alive holds.
///
/// A drain does not close idle keep-alive connections: a client may be
/// sending its next request at that very moment, and would see it fail
/// (found by `cargo xtask chaos`). A request that arrives while the worker
/// drains is answered with `Connection: close`, like the shim does for
/// Bun and Node apps; a connection still idle when the drain ends closes
/// as the worker exits.
async fn connection<R>(
    mut r: R,
    mut w: Conn<'_>,
    site: Arc<Site>,
    watch: &idle::Guard,
    mut head: HeadBuf,
    pending: Option<Pending>,
    answered: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    if let Some(p) = pending {
        // `first_request` sent part of a response and the socket was full:
        // the rest goes first. It counts as a request in flight, so a drain
        // waits for it as it does for any other.
        site.active.fetch_add(1, Ordering::SeqCst);
        let sent = w.write_all(p.out.advance(p.at)).await;
        site.active.fetch_sub(1, Ordering::SeqCst);
        if sent.is_err() || !p.keep {
            head.give_back(&site);
            return;
        }
    }
    serve_requests(&mut r, &mut w, &site, watch, &mut head, answered).await;
    head.give_back(&site);
}

/// `answered`: a request has been answered on this connection already, so the
/// wait for the next is the one between requests, not the first.
async fn serve_requests<R>(
    r: &mut R,
    w: &mut Conn<'_>,
    site: &Site,
    watch: &idle::Guard,
    head: &mut HeadBuf,
    answered: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut first = !answered;
    // The cache key of the request being served, built in place.
    let mut key = String::new();
    loop {
        // The wait for a head is limited (idle.rs); serving a request is not.
        watch.waiting(if first { site.head_timeout } else { site.idle_timeout });
        first = false;
        let read = read_head(r, head).await;
        watch.busy();
        let len = match read {
            Ok(Some(n)) => n,
            Ok(None) => return,
            Err(code) => {
                let _ = respond_error(w, code, false, false).await;
                return;
            }
        };
        let req = match parse_head(&head.buf[head.pos..head.pos + len]) {
            Ok(req) => req,
            Err(code) => {
                let _ = respond_error(w, code, false, false).await;
                return;
            }
        };
        site.active.fetch_add(1, Ordering::SeqCst);
        let t0 = site.cfg.access_log.then(Instant::now);
        let keep = req.keep_alive && !site.draining.load(Ordering::SeqCst);
        let mut cached = "";
        let result = handle(&req, site, w, keep, &mut cached, &mut key).await;
        site.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(t0) = t0 {
            let (status, bytes) = result.as_ref().map(|x| *x).unwrap_or((0, 0));
            println!(
                "{} {} {status} {bytes}B {:.1}ms{cached}",
                req.method,
                req.path,
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        // Closing: the caller's drop of the connection is close(2), which
        // sends the FIN after the response bytes (no shutdown(2) first).
        if result.is_err() || !keep || site.draining.load(Ordering::SeqCst) {
            return;
        }
        head.consume(len);
    }
}

/// The part of a response `first_request` could not send yet.
struct Pending {
    out: OutBuf,
    /// How much of `out` has gone out.
    at: usize,
    /// Keep the connection for more requests afterwards.
    keep: bool,
}

enum First {
    /// Finished: the response went out and the connection is closed (or the
    /// client had gone away).
    Done,
    /// Carry on as a task: `head` holds the bytes read so far (a head not
    /// complete yet, or a request the quick path leaves to the normal one, or
    /// what follows a request already answered), `pending` the rest of a
    /// response that did not fit the socket, `answered` whether a request was
    /// answered already (the connection then waits for the next one as a
    /// kept-alive connection does, with the longer limit).
    Go { fd: OwnedFd, head: HeadBuf, pending: Option<Pending>, answered: bool },
}

/// A request answered entirely from the response cache.
struct Inlined {
    out: OutBuf,
    status: u16,
    bytes: u64,
    keep: bool,
    /// Method and path, when the access log wants them.
    log: Option<(String, String)>,
}

/// The first request of a new connection, answered without leaving the accept
/// loop when that costs nothing: the request is already in the socket (Linux
/// accepts a connection only once its data has arrived, TCP_DEFER_ACCEPT) and
/// is a plain GET or HEAD of a file whose response is cached and fresh. Then
/// the whole exchange is `recv`, `send` and, unless the client wants the
/// connection kept, `close`: no task, no epoll registration (and so no
/// deregistration), no timer, nothing waited for. A client that sends
/// `Connection: close` (or HTTP/1.0: `ab`, `curl`, health checks, a browser's
/// first request on each connection) costs about half what it did.
///
/// Anything else (a miss, a stale entry, auth, a range, a head still arriving,
/// a body too big for the socket's room) is handed on as it stands, as `Go`.
fn first_request(site: &Site, fd: OwnedFd, try_inline: bool) -> First {
    let mut head = HeadBuf::take(site);
    if !try_inline {
        return First::Go { fd, head, pending: None, answered: false };
    }
    match crate::sys::recv_into(fd.as_fd(), &mut head.buf) {
        Ok(0) => {
            // The client closed before it sent anything.
            head.give_back(site);
            return First::Done;
        }
        Ok(_) => {}
        // Nothing there yet (Linux hands over a connection with its data, but
        // not always: the defer timeout, a peer that connected and stalled).
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return First::Go { fd, head, pending: None, answered: false };
        }
        Err(_) => {
            head.give_back(site);
            return First::Done;
        }
    }
    let (skip, len) = match find_head(&head.buf) {
        Head::Done { skip, len } => (skip, len),
        Head::More { skip, scan } => {
            // Part of a head: the task carries on from here.
            head.pos = skip;
            head.scan = scan;
            return First::Go { fd, head, pending: None, answered: false };
        }
        Head::TooLong => return First::Go { fd, head, pending: None, answered: false },
    };
    head.pos = skip;
    let t0 = site.cfg.access_log.then(Instant::now);
    let mut key = String::new();
    let Some(hit) = inline_hit(site, &head.buf[skip..skip + len], &mut key) else {
        // The head is whole and `pos` is at it: the normal path finds it again.
        return First::Go { fd, head, pending: None, answered: false };
    };
    let sent = crate::sys::send(fd.as_fd(), hit.out.as_slice(), false);
    if site.cfg.access_log {
        let (m, p) = hit.log.as_ref().map(|(m, p)| (m.as_str(), p.as_str())).unwrap_or(("", ""));
        let ms = t0.map_or(0.0, |t| t.elapsed().as_secs_f64() * 1000.0);
        println!("{m} {p} {} {}B {ms:.1}ms cache=hit", hit.status, hit.bytes);
    }
    let at = match sent {
        Ok(n) => {
            site.inline.fetch_add(1, Ordering::Relaxed);
            n
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
        Err(_) => {
            head.give_back(site);
            return First::Done;
        }
    };
    head.consume(len);
    if at < hit.out.as_slice().len() {
        let pending = Some(Pending { out: hit.out, at, keep: hit.keep });
        return First::Go { fd, head, pending, answered: true };
    }
    if !hit.keep {
        head.give_back(site);
        return First::Done;
    }
    First::Go { fd, head, pending: None, answered: true }
}

/// The cached answer to the request in `bytes`, if it can be sent at once
/// from memory and is exactly what `handle` would send. The checks are
/// `handle`'s own, in its order, minus everything that waits.
fn inline_hit(site: &Site, bytes: &[u8], key: &mut String) -> Option<Inlined> {
    let cache = site.cache.as_ref()?;
    let req = parse_head(bytes).ok()?;
    if (req.method != "GET" && req.method != "HEAD") || site.auth.is_some() || req.range.is_some() {
        return None;
    }
    let cfg = &site.cfg;
    let rel = relative_cow(req.path, cfg.dotfiles).ok()?;
    let url_path = req.path.split(['?', '#']).next().unwrap_or("/");
    cache_key_into(key, &rel, cfg, &req, url_path.ends_with('/'));
    let Lookup::Fresh(e) = cache.lookup(key, Instant::now()) else { return None };
    let not_modified = is_not_modified(&req, &e.etag, e.mtime);
    if e.file.is_some() && !not_modified && req.method != "HEAD" {
        return None; // a body in a memfd goes by sendfile, on the normal path
    }
    cache.hits.fetch_add(1, Ordering::Relaxed);
    let keep = req.keep_alive && !site.draining.load(Ordering::SeqCst);
    let (out, status, bytes) = cached_bytes(&req, &e, keep, not_modified);
    let log = cfg.access_log.then(|| (req.method.to_string(), req.path.to_string()));
    Some(Inlined { out, status, bytes, keep, log })
}

/// A plain-text error. The head and the body go out in one write (one
/// packet; two writes were two packets and two system calls), and a HEAD
/// request gets the head alone, which must not be followed by body bytes.
async fn respond_error(w: &mut Conn<'_>, code: u16, keep: bool, head_only: bool) -> std::io::Result<(u16, u64)> {
    let body = format!("{code} {}\n", reason(code));
    let mut out = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: {}\r\n",
        reason(code),
        body.len(),
        if keep { "keep-alive" } else { "close" }
    );
    if code == 405 {
        out += "Allow: GET, HEAD\r\n";
    }
    out += "\r\n";
    let len = body.len() as u64;
    if !head_only {
        out += &body;
    }
    w.write_all(out).await?;
    w.flush().await?;
    Ok((code, if head_only { 0 } else { len }))
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

/// URL path → a path relative to the root ("" for the root itself), or
/// why not. Rejects `..`, NUL, and (unless enabled) dotfiles.
pub fn relative(url_path: &str, dotfiles: bool) -> Result<String, u16> {
    let path = url_path.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path).ok_or(400u16)?;
    if decoded.contains('\0') || !decoded.starts_with('/') {
        return Err(400);
    }
    let mut out = String::with_capacity(decoded.len());
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(403),
            s if s.starts_with('.') && !dotfiles && s != ".well-known" => return Err(404),
            s => {
                if Path::new(s).components().any(|c| !matches!(c, Component::Normal(_))) {
                    return Err(400);
                }
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(s);
            }
        }
    }
    Ok(out)
}

/// `relative`, without allocating for the paths nearly every request has:
/// no percent-escapes, no NUL, no empty, `.`, `..` or dot-leading segment
/// (a trailing slash is fine). Those come back as a slice of `url_path`;
/// anything else takes the full `relative` and its answer, so the two always
/// agree (a test compares them on thousands of paths).
fn relative_cow(url_path: &str, dotfiles: bool) -> Result<Cow<'_, str>, u16> {
    let path = match url_path.find(['?', '#']) {
        Some(i) => &url_path[..i],
        None => url_path,
    };
    if let Some(rest) = path.strip_prefix('/') {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        let plain = rest.is_empty()
            || rest
                .split('/')
                .all(|seg| !seg.is_empty() && !seg.starts_with('.') && !seg.bytes().any(|b| b == b'%' || b == 0));
        if plain {
            return Ok(Cow::Borrowed(rest));
        }
    }
    relative(url_path, dotfiles).map(Cow::Owned)
}

fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

// How files are opened. Starts at OPEN_CACHED and steps down if the kernel
// or filesystem can't do better.
/// openat2(RESOLVE_BENEATH | RESOLVE_CACHED) inline: one syscall, the
/// kernel keeps the lookup inside the root, and it never waits for the disk
/// (a lookup not in the dentry cache goes to a thread instead).
const OPEN_CACHED: u8 = 0;
/// openat2(RESOLVE_BENEATH) inline (kernels 5.6–5.11, or filesystems whose
/// lookups are never served from cache).
const OPEN_BENEATH: u8 = 1;
/// No openat2 (kernel before 5.6, or blocked by seccomp): realpath check
/// then open, on a thread.
const OPEN_LEGACY: u8 = 2;

/// An open file (or directory) under the root and its metadata.
struct Opened {
    file: std::fs::File,
    meta: std::fs::Metadata,
}

/// Flags for every open: O_NONBLOCK so a FIFO in the root can't hang the
/// worker (regular-file reads ignore it); O_NOCTTY for devices.
const OPEN_FLAGS: i32 = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;

fn opened(fd: OwnedFd) -> std::io::Result<Opened> {
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    Ok(Opened { file, meta })
}

/// Open `rel` under the root. Errors: NotFound (also for paths the kernel
/// or the realpath check says leave the root), PermissionDenied, others.
async fn open(site: &Site, rel: &str) -> std::io::Result<Opened> {
    use std::io::{Error, ErrorKind};
    let mode = site.open_mode.load(Ordering::Relaxed);
    if mode != OPEN_LEGACY {
        let path = std::ffi::CString::new(if rel.is_empty() { "." } else { rel })
            .map_err(|_| Error::from(ErrorKind::InvalidInput))?;
        let beneath = crate::sys::RESOLVE_BENEATH | crate::sys::RESOLVE_NO_MAGICLINKS;
        let resolve = if mode == OPEN_CACHED { beneath | crate::sys::RESOLVE_CACHED } else { beneath };
        let mut result = crate::sys::openat2(site.dir.as_fd(), &path, OPEN_FLAGS, resolve);
        if mode == OPEN_CACHED {
            match &result {
                Ok(_) => {
                    site.cached_hits.fetch_add(1, Ordering::Relaxed);
                }
                // Not all in the dentry cache: the lookup may wait for the
                // disk, so it runs on a thread.
                Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
                    let misses = site.cached_misses.fetch_add(1, Ordering::Relaxed) + 1;
                    if misses >= 256 && site.cached_hits.load(Ordering::Relaxed) == 0 {
                        // This filesystem never answers from cache (e.g. it
                        // revalidates every lookup): stop asking.
                        site.open_mode.store(OPEN_BENEATH, Ordering::Relaxed);
                    }
                    let dir = site.dir.clone();
                    let p = path.clone();
                    result =
                        tokio::task::spawn_blocking(move || crate::sys::openat2(dir.as_fd(), &p, OPEN_FLAGS, beneath))
                            .await
                            .map_err(Error::other)?;
                }
                // RESOLVE_CACHED is newer (5.12) than openat2 (5.6).
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                    site.open_mode.store(OPEN_BENEATH, Ordering::Relaxed);
                    result = crate::sys::openat2(site.dir.as_fd(), &path, OPEN_FLAGS, beneath);
                }
                Err(_) => {}
            }
        }
        match result {
            Ok(fd) => return opened(fd),
            Err(e) => match e.raw_os_error() {
                Some(libc::ENOSYS | libc::EPERM) => {
                    site.open_mode.store(OPEN_LEGACY, Ordering::Relaxed);
                    eprintln!(
                        "warden serve-static: openat2 is unavailable ({e}); checking paths with realpath instead"
                    );
                }
                // The lookup left the root: `..` in a symlink, or an absolute
                // symlink. Absolute symlinks that point back inside the root
                // were always allowed, so the realpath check below decides.
                Some(libc::EXDEV) => {}
                _ => return Err(e),
            },
        }
    }
    let path = if rel.is_empty() { site.root.clone() } else { site.root.join(rel) };
    let root = site.root.clone();
    tokio::task::spawn_blocking(move || {
        if !inside(&root, &path) {
            return Err(Error::from(ErrorKind::NotFound));
        }
        let file =
            std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(&path)?;
        let meta = file.metadata()?;
        Ok(Opened { file, meta })
    })
    .await
    .map_err(Error::other)?
}

/// Read `buf[from..]` from `file` at `offset`. Cached data is read inline
/// (preadv2 RWF_NOWAIT never waits for the disk); anything else is read on
/// a thread, so a cold file can't stall the other connections. The file
/// comes back too (the cache checks it did not change meanwhile).
async fn read_body(
    file: std::fs::File,
    mut buf: Vec<u8>,
    from: usize,
    offset: u64,
) -> std::io::Result<(std::fs::File, Vec<u8>)> {
    use std::io::{Error, ErrorKind};
    let mut done = 0usize;
    while from + done < buf.len() {
        match crate::sys::pread_nowait(file.as_fd(), &mut buf[from + done..], offset + done as u64) {
            Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "file truncated while reading")),
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::Unsupported
                    || e.raw_os_error() == Some(libc::EOPNOTSUPP) =>
            {
                return tokio::task::spawn_blocking(move || {
                    file.read_exact_at(&mut buf[from + done..], offset + done as u64).map(|()| (file, buf))
                })
                .await
                .map_err(Error::other)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((file, buf))
}

/// Response status for a failed open.
fn open_error_status(e: &std::io::Error) -> u16 {
    if e.kind() == std::io::ErrorKind::PermissionDenied { 403 } else { 404 }
}

/// A resolved path is only served if, after following symlinks, it is still
/// inside root.
fn inside(root: &Path, p: &Path) -> bool {
    std::fs::canonicalize(p).is_ok_and(|c| c.starts_with(root))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// A cache miss being served: the response is cached if it turns out to
/// be a plain 200 from a small file. `deps` collects every lookup that
/// chose it, so a later check can tell whether the same request would
/// still get the same response.
struct Fill {
    key: String,
    deps: Vec<Dep>,
}

/// Precompressed encodings (in preference order) and their file suffixes.
const ENCODINGS: [(&str, &str); 2] = [("br", "br"), ("gzip", "gz")];

fn accepts(accept_encoding: &str, enc: &str) -> bool {
    accept_encoding.split(',').any(|a| a.trim().split(';').next() == Some(enc))
}

/// Cache key: the checked relative path, which precompressed encodings the
/// client accepts (they pick the variant), and whether the URL ended in a
/// slash (`/docs` is a redirect, `/docs/` the index page). Built into `out`
/// (cleared first), a buffer the connection keeps, so a cache hit allocates
/// nothing for it.
fn cache_key_into(out: &mut String, rel: &str, cfg: &Static, req: &Request, slash: bool) {
    let mut mask = 0u8;
    if cfg.precompressed {
        let accept = req.header("accept-encoding").unwrap_or("");
        for (bit, (enc, _)) in ENCODINGS.iter().enumerate() {
            if accepts(accept, enc) {
                mask |= 1 << bit;
            }
        }
    }
    out.clear();
    out.push_str(rel);
    // NUL can't be in `rel` (relative() refuses it), so keys can't collide.
    out.push('\0');
    out.push((b'0' + mask) as char);
    if slash {
        out.push('/');
    }
}

#[cfg(test)]
fn cache_key(rel: &str, cfg: &Static, req: &Request, slash: bool) -> String {
    let mut out = String::new();
    cache_key_into(&mut out, rel, cfg, req, slash);
    out
}

/// Would the same lookups find the same things now? Each path goes through
/// the normal open path, so a file that was swapped for a symlink leaving
/// the root no longer matches.
async fn still_matches(site: &Site, deps: &[Dep]) -> bool {
    for d in deps {
        let now = open(site, &d.path).await.ok();
        if !d.seen.matches(now.as_ref().map(|o| &o.meta)) {
            return false;
        }
    }
    true
}

async fn handle(
    req: &Request<'_>,
    site: &Site,
    w: &mut Conn<'_>,
    keep: bool,
    cached: &mut &'static str,
    key: &mut String,
) -> std::io::Result<(u16, u64)> {
    if req.method != "GET" && req.method != "HEAD" {
        return respond_error(w, 405, keep, false).await;
    }
    if let Some(expected) = &site.auth {
        if req.header("authorization") != Some(expected.as_str()) {
            let body = "401 Unauthorized\n";
            let mut out = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"warden\"\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: {}\r\n\r\n",
                body.len(),
                if keep { "keep-alive" } else { "close" }
            );
            let head_only = req.method == "HEAD";
            if !head_only {
                out += body;
            }
            w.write_all(out).await?;
            w.flush().await?;
            return Ok((401, if head_only { 0 } else { body.len() as u64 }));
        }
    }
    let cfg = &site.cfg;
    let rel = match relative_cow(req.path, cfg.dotfiles) {
        Ok(p) => p,
        Err(code) => return not_found_or(w, site, req, code, keep).await,
    };
    let url_path = req.path.split(['?', '#']).next().unwrap_or("/");
    // The cache answers plain GET / HEAD (conditional or not) for a path
    // that passed the checks above; ranges take the normal path.
    let mut fill = None;
    if let Some(cache) = &site.cache {
        if req.header("range").is_none() {
            cache_key_into(key, &rel, cfg, req, url_path.ends_with('/'));
            let now = Instant::now();
            let hit = match cache.lookup(key, now) {
                Lookup::Fresh(e) => Some(e),
                Lookup::Stale(e) if still_matches(site, &e.deps).await => {
                    cache.confirm(key, &e, now);
                    Some(e)
                }
                Lookup::Stale(e) => {
                    cache.invalidate(key, &e);
                    None
                }
                Lookup::Miss => None,
            };
            if let Some(e) = hit {
                cache.hits.fetch_add(1, Ordering::Relaxed);
                *cached = " cache=hit";
                return respond_cached(w, req, &e, keep).await;
            }
            cache.misses.fetch_add(1, Ordering::Relaxed);
            *cached = " cache=miss";
            fill = Some(Fill { key: key.clone(), deps: Vec::new() });
        }
    }
    let found = match open(site, &rel).await {
        Ok(o) => o,
        Err(e) => return not_found_or(w, site, req, open_error_status(&e), keep).await,
    };
    if found.meta.is_dir() {
        drop(found);
        if !url_path.ends_with('/') {
            let loc = format!("{url_path}/");
            let head = format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: {loc}\r\nContent-Length: 0\r\nConnection: {}\r\n\r\n",
                if keep { "keep-alive" } else { "close" }
            );
            w.write_all(head).await?;
            w.flush().await?;
            return Ok((301, 0));
        }
        let index = join_rel(&rel, &cfg.index);
        return match open(site, &index).await {
            Ok(o) if o.meta.is_file() => {
                if let Some(f) = &mut fill {
                    f.deps.push(Dep { path: rel.as_ref().into(), seen: Seen::Dir });
                    f.deps.push(Dep { path: index.as_str().into(), seen: Seen::File(Stamp::of(&o.meta)) });
                }
                send_file(w, site, req, &index, o, 200, keep, fill).await
            }
            _ => {
                let dir = if rel.is_empty() { site.root.clone() } else { site.root.join(&*rel) };
                if cfg.listing && inside(&site.root, &dir) {
                    listing(w, &dir, url_path, req.method == "HEAD", keep).await
                } else {
                    not_found_or(w, site, req, 404, keep).await
                }
            }
        };
    }
    if !found.meta.is_file() {
        return not_found_or(w, site, req, 404, keep).await;
    }
    if let Some(f) = &mut fill {
        f.deps.push(Dep { path: rel.as_ref().into(), seen: Seen::File(Stamp::of(&found.meta)) });
    }
    send_file(w, site, req, &rel, found, 200, keep, fill).await
}

/// SPA fallback to index.html, then 404.html, then a plain 404.
async fn not_found_or(
    w: &mut Conn<'_>,
    site: &Site,
    req: &Request<'_>,
    code: u16,
    keep: bool,
) -> std::io::Result<(u16, u64)> {
    if code == 404 {
        let wants_page = req.header("accept").is_none_or(|a| a.contains("text/html") || a.contains("*/*"));
        let last = req.path.split(['?', '#']).next().unwrap_or("").rsplit('/').next().unwrap_or("");
        if site.cfg.spa && wants_page && !last.contains('.') {
            if let Ok(o) = open(site, &site.cfg.index).await {
                if o.meta.is_file() {
                    return send_file(w, site, req, &site.cfg.index, o, 200, keep, None).await;
                }
            }
        }
        if let Ok(o) = open(site, "404.html").await {
            if o.meta.is_file() {
                return send_file(w, site, req, "404.html", o, 404, keep, None).await;
            }
        }
    }
    respond_error(w, code, keep, req.method == "HEAD").await
}

async fn listing(
    w: &mut Conn<'_>,
    dir: &Path,
    url_path: &str,
    head_only: bool,
    keep: bool,
) -> std::io::Result<(u16, u64)> {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let mut names: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().to_string()))
                .filter(|(_, n)| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut body = format!("<!doctype html><meta charset=utf-8><title>{0}</title><h1>{0}</h1><ul>", esc(url_path));
    if url_path != "/" {
        body += "<li><a href=\"../\">../</a>";
    }
    for (is_dir, n) in names {
        let shown = if is_dir { format!("{n}/") } else { n };
        body += &format!("<li><a href=\"{0}\">{0}</a>", esc(&shown));
    }
    body += "</ul>\n";
    let mut out = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\
         Connection: {}\r\n\r\n",
        body.len(),
        if keep { "keep-alive" } else { "close" }
    );
    let len = body.len() as u64;
    if !head_only {
        out += &body;
    }
    w.write_all(out).await?;
    w.flush().await?;
    Ok((200, len))
}

/// The parts of a file response's head that don't depend on the request.
struct HeadParts<'a> {
    etag: &'a str,
    last_modified: &'a str,
    cache_control: &'a str,
    vary: &'a str,
    extra: &'a str,
}

/// The head of a file response, and where its Connection value starts
/// (a cached head is stored with `keep-alive` there and patched for
/// `close`). `range`: (first byte, file length) for a 206.
fn file_head(
    p: &HeadParts,
    code: u16,
    mime: &str,
    count: u64,
    conn: &str,
    encoding: Option<&str>,
    range: Option<(u64, u64)>,
) -> (String, usize) {
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {mime}\r\nContent-Length: {count}\r\nETag: {}\r\n\
         Last-Modified: {}\r\nCache-Control: {}\r\nAccept-Ranges: bytes\r\n{}{}Connection: ",
        reason(code),
        p.etag,
        p.last_modified,
        p.cache_control,
        p.vary,
        p.extra,
    );
    let conn_at = head.len();
    head += conn;
    head += "\r\n";
    if let Some(e) = encoding {
        head += &format!("Content-Encoding: {e}\r\n");
    }
    if let Some((start, len)) = range {
        head += &format!("Content-Range: bytes {start}-{}/{len}\r\n", start + count - 1);
    }
    head += "\r\n";
    (head, conn_at)
}

/// The 304 head, and where its Connection value starts.
fn not_modified_head(p: &HeadParts, conn: &str) -> (String, usize) {
    let mut head = format!(
        "HTTP/1.1 304 Not Modified\r\nETag: {}\r\nLast-Modified: {}\r\nCache-Control: {}\r\n{}{}Connection: ",
        p.etag, p.last_modified, p.cache_control, p.vary, p.extra
    );
    let conn_at = head.len();
    head += conn;
    head += "\r\n\r\n";
    (head, conn_at)
}

/// Cache-Control of an HTML page (also an index page and the SPA fallback).
/// By default `no-cache`: the browser asks again on every navigation and
/// gets a 304 (one round trip per page load, whatever the file size).
/// `html_max_age` (seconds) lets it reuse the page without asking for that
/// long; unset or 0 keeps `no-cache`. `private` with Basic auth, so a shared
/// cache in front never stores a page that needed a password (`public`
/// would allow it for requests with an Authorization header).
fn html_cache_control(cfg: &Static) -> String {
    match cfg.html_max_age {
        None | Some(0) => "no-cache".to_string(),
        Some(n) if cfg.basic_auth.is_some() => format!("private, max-age={n}"),
        Some(n) => format!("public, max-age={n}"),
    }
}

/// If-None-Match (wins when present), else If-Modified-Since.
fn is_not_modified(req: &Request<'_>, etag: &str, mtime: u64) -> bool {
    match req.header("if-none-match") {
        Some(tags) => tags.split(',').any(|t| t.trim() == etag || t.trim() == "*"),
        None => req.header("if-modified-since").and_then(parse_http_date).is_some_and(|since| mtime <= since),
    }
}

/// Answer from a cached entry: 304, HEAD or the full response, the same
/// bytes the normal path sends. Keep-alive: one send of the stored bytes.
async fn respond_cached(
    w: &mut Conn<'_>,
    req: &Request<'_>,
    e: &Arc<Entry>,
    keep: bool,
) -> std::io::Result<(u16, u64)> {
    let not_modified = is_not_modified(req, &e.etag, e.mtime);
    if let (Some(mem), false, false) = (&e.file, not_modified, req.method == "HEAD") {
        // The whole response is in a memfd: one sendfile(2), no copy.
        if keep {
            w.send_cached(&[], &mem.file, 0, (e.head_len as u64) + e.body_len).await?;
        } else {
            let mut head = Vec::with_capacity(e.head_len);
            head.extend_from_slice(&e.resp[..e.conn_at]);
            head.extend_from_slice(b"close");
            head.extend_from_slice(&e.resp[e.conn_at + cache::KEEP_ALIVE.len()..e.head_len]);
            w.send_cached(&head, &mem.file, e.head_len as u64, e.body_len).await?;
        }
        w.flush().await?;
        return Ok((200, e.body_len));
    }
    let (out, status, bytes) = cached_bytes(req, e, keep, not_modified);
    w.write_all(out).await?;
    w.flush().await?;
    Ok((status, bytes))
}

/// The bytes that answer `req` from the cached `e`, and the status and body
/// length they carry: a 304, a HEAD's head, or the whole response, with the
/// Connection value patched for `close` when the connection is not kept.
/// (Not for a GET of an entry whose body is in a memfd: that is sent by
/// sendfile, `respond_cached`.)
fn cached_bytes(req: &Request<'_>, e: &Entry, keep: bool, not_modified: bool) -> (OutBuf, u16, u64) {
    let (buf, conn_at, end, status, bytes) = if not_modified {
        (&e.not_modified, e.nm_conn_at, e.not_modified.len(), 304, 0)
    } else if req.method == "HEAD" {
        (&e.resp, e.conn_at, e.head_len, 200, 0)
    } else {
        (&e.resp, e.conn_at, e.resp.len(), 200, e.body_len)
    };
    let out = if keep {
        OutBuf::Shared(buf.clone(), 0..end)
    } else {
        let mut v = Vec::with_capacity(end);
        v.extend_from_slice(&buf[..conn_at]);
        v.extend_from_slice(b"close");
        v.extend_from_slice(&buf[conn_at + cache::KEEP_ALIVE.len()..end]);
        OutBuf::Vec(v)
    };
    (out, status, bytes)
}

/// `rel` is the file's path under the root (its name picks the MIME type
/// and cache policy); `found` is that file, already open. With `fill` (a
/// cache miss), a small 200 response is built once, cached, and sent from
/// the cache entry.
#[allow(clippy::too_many_arguments)]
async fn send_file(
    w: &mut Conn<'_>,
    site: &Site,
    req: &Request<'_>,
    rel: &str,
    found: Opened,
    status: u16,
    keep: bool,
    mut fill: Option<Fill>,
) -> std::io::Result<(u16, u64)> {
    let cfg = &site.cfg;
    let name = Path::new(rel);
    let ext = name.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    // Precompressed sibling the client accepts.
    let accept = req.header("accept-encoding").unwrap_or("");
    let mut body = found;
    let mut encoding = None;
    if cfg.precompressed && status == 200 && req.header("range").is_none() {
        for (enc, suffix) in ENCODINGS {
            if accepts(accept, enc) {
                let sibling = format!("{rel}.{suffix}");
                let got = open(site, &sibling).await;
                if let Some(f) = &mut fill {
                    let seen = match &got {
                        Ok(o) if o.meta.is_file() => Seen::File(Stamp::of(&o.meta)),
                        _ => Seen::NotFile,
                    };
                    f.deps.push(Dep { path: sibling.into(), seen });
                }
                if let Ok(o) = got {
                    if o.meta.is_file() {
                        body = o;
                        encoding = Some(enc);
                        break;
                    }
                }
            }
        }
    }
    let meta = &body.meta;
    let len = meta.len();
    let mtime = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let etag = format!("W/\"{len:x}-{mtime:x}{}\"", encoding.map(|e| format!("-{e}")).unwrap_or_default());
    let last_modified = http_date(mtime);

    let html = matches!(ext.as_str(), "html" | "htm");
    let cache_control = if status != 200 {
        "no-cache".to_string()
    } else if html {
        html_cache_control(cfg)
    } else if fingerprinted(name) {
        "public, max-age=31536000, immutable".to_string()
    } else {
        format!("public, max-age={}", cfg.cache_max_age)
    };
    let mut extra = String::new();
    for (k, v) in &cfg.headers {
        extra += &format!("{k}: {v}\r\n");
    }
    let vary = if cfg.precompressed { "Vary: Accept-Encoding\r\n" } else { "" };
    let parts =
        HeadParts { etag: &etag, last_modified: &last_modified, cache_control: &cache_control, vary, extra: &extra };

    // A small file on a cache miss: build the whole response once, keep it
    // (unless the file changed while it was read, or so recently that a
    // change could go unnoticed), and answer from it.
    if let (Some(fill), Some(cache)) = (fill, &site.cache) {
        let stamp = Stamp::of(meta);
        if status == 200 && len <= cache.max_file && meta.is_file() && !stamp.racy(std::time::SystemTime::now()) {
            let (head, conn_at) = file_head(&parts, 200, mime(&ext), len, "keep-alive", encoding, None);
            let (not_modified, nm_conn_at) = not_modified_head(&parts, "keep-alive");
            let head_len = head.len();
            let mut buf = head.into_bytes();
            buf.resize(head_len + len as usize, 0);
            let (file, buf) = read_body(body.file, buf, head_len, 0).await?;
            let unchanged = file.metadata().is_ok_and(|m| Stamp::of(&m) == stamp);
            // A big body goes into a memfd (sent by page reference); the
            // head stays in memory too, for HEAD and `Connection: close`.
            let mem = cache.memfd(&buf, len);
            let resp: Arc<[u8]> = if mem.is_some() { buf[..head_len].into() } else { buf.into() };
            let entry = Arc::new(Entry {
                resp,
                file: mem,
                head_len,
                conn_at,
                not_modified: not_modified.into_bytes().into(),
                nm_conn_at,
                etag: etag.into(),
                mtime,
                body_len: len,
                deps: fill.deps,
            });
            if unchanged {
                cache.insert(fill.key, entry.clone(), Instant::now());
            }
            return respond_cached(w, req, &entry, keep).await;
        }
    }

    let conn = if keep { "keep-alive" } else { "close" };
    let not_modified = status == 200 && is_not_modified(req, &etag, mtime);
    if not_modified {
        let (head, _) = not_modified_head(&parts, conn);
        w.write_all(head).await?;
        w.flush().await?;
        return Ok((304, 0));
    }
    // Single byte range.
    let mut range = None;
    if status == 200 && encoding.is_none() {
        if let Some(r) = req.header("range") {
            match parse_range(r, len) {
                Some(Ok(rg)) => range = Some(rg),
                Some(Err(())) => {
                    let h = format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{len}\r\nContent-Length: 0\r\n\
                         Connection: {conn}\r\n\r\n"
                    );
                    w.write_all(h).await?;
                    w.flush().await?;
                    return Ok((416, 0));
                }
                None => {}
            }
        }
    }
    let (code, start, count) = match range {
        Some((a, b)) => (206, a, b - a + 1),
        None => (status, 0, len),
    };
    let (head, _) = file_head(&parts, code, mime(&ext), count, conn, encoding, (code == 206).then_some((start, len)));
    if req.method == "HEAD" {
        w.write_all(head).await?;
        w.flush().await?;
        return Ok((code, 0));
    }
    if count <= SMALL_FILE {
        // Headers and body in one write.
        let mut buf = head.into_bytes();
        let body_start = buf.len();
        buf.resize(body_start + count as usize, 0);
        let (_, buf) = read_body(body.file, buf, body_start, start).await?;
        w.write_all(buf).await?;
        w.flush().await?;
        return Ok((code, count));
    }
    let sent = w.send_file(head.as_bytes(), body.file, start, count).await?;
    w.flush().await?;
    Ok((code, sent))
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` against `len`. None: ignore (not a
/// single bytes range); Some(Err): unsatisfiable.
pub fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multiple ranges: send the whole file
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let r = match (a.is_empty(), b.is_empty()) {
        (true, false) => {
            let n: u64 = b.parse().ok()?;
            if n == 0 || len == 0 {
                return Some(Err(()));
            }
            (len.saturating_sub(n), len - 1)
        }
        (false, _) => {
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse().ok()? };
            if start >= len || end < start {
                return Some(Err(()));
            }
            (start, end.min(len - 1))
        }
        _ => return None,
    };
    Some(Ok(r))
}

/// `app.3f9a2c1b.js`, `index-DkS8xW2q.css`: a content hash in the name.
fn fingerprinted(p: &Path) -> bool {
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    name.split(['.', '-', '_']).any(|part| {
        part.len() >= 8
            && part.chars().all(|c| c.is_ascii_alphanumeric())
            && part.chars().any(|c| c.is_ascii_digit())
            && part.chars().any(|c| c.is_ascii_alphabetic())
    })
}

fn mime(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "yaml" | "yml" => "application/yaml",
        _ => "application/octet-stream",
    }
}

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// IMF-fixdate: `Wed, 30 Sep 2026 12:00:01 GMT`.
pub fn http_date(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = crate::logging::civil_from_days(days);
    format!(
        "{}, {d:02} {} {y} {:02}:{:02}:{:02} GMT",
        DAYS[(days.rem_euclid(7)) as usize],
        MONTHS[(m - 1) as usize],
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

pub fn parse_http_date(s: &str) -> Option<u64> {
    let p: Vec<&str> = s.split_whitespace().collect();
    if p.len() != 6 || p[5] != "GMT" {
        return None;
    }
    let d: i64 = p[1].parse().ok()?;
    let m = MONTHS.iter().position(|x| *x == p[2])? as i64 + 1;
    let y: i64 = p[3].parse().ok()?;
    let hms: Vec<i64> = p[4].split(':').filter_map(|x| x.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hms[0] * 3600 + hms[1] * 60 + hms[2];
    (secs >= 0).then_some(secs as u64)
}

pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_accepts_back_off_and_are_logged_sparingly() {
        let ms = |n| accept_pause(n).as_millis();
        assert_eq!([ms(1), ms(2), ms(3), ms(4), ms(5), ms(9)], [0, 0, 5, 10, 20, 320]);
        assert_eq!((ms(10), ms(11), ms(u32::MAX)), (640, 1000, 1000));
        let e = |n| std::io::Error::from_raw_os_error(n);
        assert!(accept_error_is_transient(&e(libc::ECONNABORTED)));
        assert!(!accept_error_is_transient(&e(libc::EMFILE)));
        assert!(accept_hint(&e(libc::EMFILE)).contains("LimitNOFILE=65536"));
        assert!(accept_hint(&e(libc::ENFILE)).contains("fs.file-max"));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut a = AcceptErrors::new("connections", "epoll");
            a.failed(&e(libc::EMFILE));
            assert!(a.resume_at.is_none(), "the first retry is at once");
            a.failed(&e(libc::EMFILE));
            a.failed(&e(libc::EMFILE));
            assert!(a.resume_at.is_some(), "then it pauses");
            assert_eq!((a.unlogged, a.told), (2, true), "logged once, the rest counted for the next line");
            a.accepted();
            assert_eq!((a.streak, a.told), (0, false));
            // A client gone before its accept: retried at once, not logged.
            let mut b = AcceptErrors::new("connections", "epoll");
            b.failed(&e(libc::ECONNABORTED));
            assert!(b.resume_at.is_none() && b.logged_at.is_none());
        });
    }

    #[test]
    fn paths_stay_inside_root() {
        assert_eq!(relative("/a/b.js?v=1", false), Ok("a/b.js".into()));
        assert_eq!(relative("/a%20b.txt", false), Ok("a b.txt".into()));
        assert_eq!(relative("/", false), Ok("".into()));
        assert_eq!(relative("//a//./b/", false), Ok("a/b".into()));
        assert_eq!(relative("/../etc/passwd", false), Err(403));
        assert_eq!(relative("/a/%2e%2e/%2e%2e/etc/passwd", false), Err(403));
        assert_eq!(relative("/.env", false), Err(404));
        assert!(relative("/.env", true).is_ok());
        assert!(relative("/.well-known/security.txt", false).is_ok());
        assert_eq!(relative("/a%00b", false), Err(400));
        assert_eq!(relative("/%zz", false), Err(400));
        assert_eq!(relative("noslash", false), Err(400));
        assert_eq!(join_rel("", "index.html"), "index.html");
        assert_eq!(join_rel("docs", "index.html"), "docs/index.html");
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=50-500", 100), Some(Ok((50, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    #[test]
    fn dates_and_misc() {
        assert_eq!(http_date(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date(1_790_769_601), "Wed, 30 Sep 2026 12:00:01 GMT");
        assert_eq!(parse_http_date("Wed, 30 Sep 2026 12:00:01 GMT"), Some(1_790_769_601));
        assert_eq!(parse_http_date("nonsense"), None);
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab"), "YWI=");
        assert!(fingerprinted(Path::new("app.3f9a2c1b.js")));
        assert!(fingerprinted(Path::new("index-DkS8xW2q.css")));
        assert!(!fingerprinted(Path::new("favicon.ico")));
        assert!(!fingerprinted(Path::new("background.png")));
        assert_eq!(mime("woff2"), "font/woff2");
    }

    /// The heads, byte for byte as the server wrote them before they were
    /// split around the Connection value (the cache stores `keep-alive`
    /// and patches `close` in at `conn_at`).
    #[test]
    fn heads_are_byte_identical_and_patchable() {
        let (etag, lm) = ("W/\"5dc-6a1b2c3d-br\"", "Wed, 30 Sep 2026 12:00:01 GMT");
        for (vary, extra) in [("", ""), ("Vary: Accept-Encoding\r\n", "X-Frame-Options: DENY\r\nX-A: b\r\n")] {
            for cache in ["no-cache", "public, max-age=3600"] {
                let p = HeadParts { etag, last_modified: lm, cache_control: cache, vary, extra };
                for conn in ["keep-alive", "close"] {
                    let old_304 = format!(
                        "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nLast-Modified: {lm}\r\nCache-Control: {cache}\r\n\
                         {vary}{extra}Connection: {conn}\r\n\r\n"
                    );
                    assert_eq!(not_modified_head(&p, conn).0, old_304);
                    for (code, encoding, start, count, len) in [
                        (200u16, None, 0u64, 1500u64, 1500u64),
                        (200, Some("br"), 0, 300, 300),
                        (404, None, 0, 14, 14),
                        (206, None, 100, 10_000, 15_000),
                    ] {
                        let mut old = format!(
                            "HTTP/1.1 {code} {}\r\nContent-Type: {}\r\nContent-Length: {count}\r\nETag: {etag}\r\n\
                             Last-Modified: {lm}\r\nCache-Control: {cache}\r\nAccept-Ranges: bytes\r\n{vary}{extra}Connection: {conn}\r\n",
                            reason(code),
                            mime("css"),
                        );
                        if let Some(e) = encoding {
                            old += &format!("Content-Encoding: {e}\r\n");
                        }
                        if code == 206 {
                            old += &format!("Content-Range: bytes {start}-{}/{len}\r\n", start + count - 1);
                        }
                        old += "\r\n";
                        let range = (code == 206).then_some((start, len));
                        assert_eq!(file_head(&p, code, mime("css"), count, conn, encoding, range).0, old);
                    }
                }
                // Patching `close` into the keep-alive form gives the close form.
                let (keep, at) = file_head(&p, 200, "text/css", 3, "keep-alive", Some("gzip"), None);
                let patched = format!("{}close{}", &keep[..at], &keep[at + cache::KEEP_ALIVE.len()..]);
                assert_eq!(patched, file_head(&p, 200, "text/css", 3, "close", Some("gzip"), None).0);
                let (keep, at) = not_modified_head(&p, "keep-alive");
                let patched = format!("{}close{}", &keep[..at], &keep[at + cache::KEEP_ALIVE.len()..]);
                assert_eq!(patched, not_modified_head(&p, "close").0);
            }
        }
    }

    /// A request head with these headers, to parse with `.req()`.
    struct Owned(String);

    impl Owned {
        fn req(&self) -> Request<'_> {
            parse_head(self.0.as_bytes()).unwrap()
        }
    }

    fn req(lines: &[(&str, &str)]) -> Owned {
        let mut head = "GET /a.css HTTP/1.1\r\n".to_string();
        for (k, v) in lines {
            head += &format!("{k}: {v}\r\n");
        }
        head += "\r\n";
        Owned(head)
    }

    #[test]
    fn cache_keys_separate_what_changes_the_response() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        let plain = cache_key("a.css", &cfg, &req(&[]).req(), false);
        let both = cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "gzip, deflate, br")]).req(), false);
        let gz = cache_key("a.css", &cfg, &req(&[("accept-encoding", "gzip;q=1")]).req(), false);
        let slash = cache_key("a.css", &cfg, &req(&[]).req(), true);
        let all = [&plain, &both, &gz, &slash];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Encodings the server never picks don't split the cache.
        assert_eq!(cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "deflate, zstd")]).req(), false), plain);
        // Without precompressed files, Accept-Encoding doesn't matter.
        cfg.precompressed = false;
        assert_eq!(
            cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "br")]).req(), false),
            cache_key("a.css", &cfg, &req(&[]).req(), false)
        );
        assert_ne!(cache_key("a", &cfg, &req(&[]).req(), false), cache_key("a\u{1}", &cfg, &req(&[]).req(), false));
    }

    #[test]
    fn html_pages_revalidate_unless_html_max_age_is_set() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        assert_eq!(cfg.html_max_age, None);
        assert_eq!(html_cache_control(&cfg), "no-cache", "the default: ask again on every page load");
        cfg.html_max_age = Some(0);
        assert_eq!(html_cache_control(&cfg), "no-cache", "0 is the same as unset");
        cfg.html_max_age = Some(60);
        assert_eq!(html_cache_control(&cfg), "public, max-age=60");
        cfg.basic_auth = Some("user:pass".into());
        assert_eq!(html_cache_control(&cfg), "private, max-age=60", "a shared cache must not keep a password page");
        // Not a knob for the other files: their policy is cache_max_age.
        cfg.html_max_age = None;
        assert_eq!(html_cache_control(&cfg), "no-cache");
    }

    #[test]
    fn conditional_requests() {
        let etag = "W/\"3-5\"";
        assert!(is_not_modified(&req(&[("If-None-Match", "\"x\", W/\"3-5\"")]).req(), etag, 5));
        assert!(is_not_modified(&req(&[("If-None-Match", "*")]).req(), etag, 5));
        // If-None-Match wins over If-Modified-Since.
        assert!(!is_not_modified(
            &req(&[("If-None-Match", "\"x\""), ("If-Modified-Since", "Wed, 30 Sep 2026 12:00:01 GMT")]).req(),
            etag,
            5
        ));
        assert!(is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:05 GMT")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "garbage")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[]).req(), etag, 5));
    }

    /// The head parser, on bytes arriving as the test says.
    async fn heads(chunks: &[&[u8]]) -> (Result<Option<usize>, u16>, HeadBuf) {
        let (mut tx, mut rx) = tokio::io::duplex(1 << 20);
        for c in chunks {
            tx.write_all(c).await.unwrap();
        }
        drop(tx);
        let mut h = HeadBuf { buf: Vec::with_capacity(HEAD_BUF), pos: 0, scan: 0 };
        let got = read_head(&mut rx, &mut h).await;
        (got, h)
    }

    /// A head line that never ends is refused at the limit, not buffered.
    #[tokio::test]
    async fn endless_head_lines_are_cut_off() {
        let mut endless = tokio::io::repeat(b'a');
        let mut h = HeadBuf { buf: Vec::new(), pos: 0, scan: 0 };
        assert_eq!(read_head(&mut endless, &mut h).await.err(), Some(431));
        assert!(h.buf.len() <= MAX_HEAD + 2 * HEAD_BUF, "bounded: {}", h.buf.len());
    }

    #[tokio::test]
    async fn heads_are_found_however_they_arrive() {
        let (got, h) = heads(&[b"\r\nGET /x HTTP/1.0\r\nConnection: keep-alive\r\nA:b\r\n\r\nrest"]).await;
        let len = got.unwrap().unwrap();
        let req = parse_head(&h.buf[h.pos..h.pos + len]).unwrap();
        assert_eq!((req.method, req.path, req.keep_alive), ("GET", "/x", true));
        assert_eq!(req.header("a"), Some("b"));
        assert_eq!(&h.buf[h.pos + len..], b"rest", "what follows stays for the next request");

        // Split anywhere, a bare \n for line ends, a lone CR before the LF arrives.
        let whole: &[u8] = b"GET /y HTTP/1.1\nHost: x\n\n";
        for cut in 1..whole.len() {
            let (got, h) = heads(&[&whole[..cut], &whole[cut..]]).await;
            let len = got.unwrap().unwrap();
            assert_eq!(&h.buf[h.pos..h.pos + len], whole, "cut at {cut}");
        }
        let (got, h) = heads(&[b"GET /z HTTP/1.1\r\n\r", b"\n"]).await;
        let len = got.unwrap().unwrap();
        assert_eq!(&h.buf[h.pos..h.pos + len], b"GET /z HTTP/1.1\r\n\r\n");

        let (got, _) = heads(&[b"GET / HTTP/1.1\r\nHost: x"]).await;
        assert_eq!(got.err(), Some(400), "cut short");
        let (got, _) = heads(&[b""]).await;
        assert!(got.unwrap().is_none(), "clean end");
        let (got, _) = heads(&[b"\r\n\r\n"]).await;
        assert!(got.unwrap().is_none(), "only blank lines, then the end: clean");
    }

    #[test]
    fn the_head_limit_counts_the_blank_line() {
        let mut head = b"GET / HTTP/1.1\r\nX: ".to_vec();
        let filler = MAX_HEAD - head.len() - 4;
        head.extend(std::iter::repeat_n(b'a', filler));
        head.extend_from_slice(b"\r\n\r\n");
        assert_eq!(head.len(), MAX_HEAD);
        assert_eq!(find_head(&head), Head::Done { skip: 0, len: MAX_HEAD }, "exactly the limit");
        let mut over = head.clone();
        over.insert(20, b'a');
        assert_eq!(find_head(&over), Head::TooLong, "one byte more");
        assert_eq!(find_head(&head[..MAX_HEAD - 1]), Head::More { skip: 0, scan: MAX_HEAD - 2 });
        assert_eq!(find_head(b"\r\n\nGET / HTTP/1.1\r\n"), Head::More { skip: 3, scan: 16 });
    }

    /// Resuming from where the last search stopped finds what a search of
    /// the whole buffer finds, however the bytes are cut up.
    #[test]
    fn a_head_found_in_pieces_is_the_head_found_whole() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut done, mut more, mut too_long) = (0, 0, 0);
        for round in 0..20_000 {
            // Few distinct bytes, so that blank lines, bare CRs and lines
            // that end are all common.
            let len = 1 + next() as usize % 60;
            let buf: Vec<u8> = (0..len).map(|_| b"ab\r\n\n"[next() as usize % 5]).collect();
            let (mut pos, mut scan) = (0, 0);
            let mut end = 0;
            while end < buf.len() {
                end = (end + 1 + next() as usize % 7).min(buf.len());
                let piece = find_head_from(&buf[pos..end], scan);
                let whole = find_head(&buf[..end]);
                let want = match piece {
                    Head::Done { skip, len } => Head::Done { skip: pos + skip, len },
                    Head::TooLong => Head::TooLong,
                    Head::More { skip, scan: s } => {
                        let w = Head::More { skip: pos + skip, scan: s };
                        pos += skip;
                        scan = s;
                        w
                    }
                };
                assert_eq!(want, whole, "round {round}, {:?} up to {end}", String::from_utf8_lossy(&buf));
                match whole {
                    Head::Done { .. } => {
                        done += 1;
                        break;
                    }
                    Head::More { .. } => more += 1,
                    Head::TooLong => {
                        too_long += 1;
                        break;
                    }
                }
            }
        }
        assert!(done > 2000 && more > 2000, "the cases are varied: {done} {more} {too_long}");

        // Past the limit, in pieces: the same answer, and each piece looks
        // at the new bytes only (the search resumes at the unfinished line).
        let mut buf = Vec::new();
        let (mut pos, mut scan) = (0, 0);
        let mut got = None;
        for _ in 0..200 {
            buf.extend(std::iter::repeat_n(*b"a\n", 100).flatten());
            match find_head_from(&buf[pos..], scan) {
                Head::More { skip, scan: s } => {
                    assert_eq!(skip, 0);
                    assert_eq!(s, buf.len(), "every line so far has ended: resume after them");
                    (pos, scan) = (pos + skip, s);
                }
                other => {
                    got = Some(other);
                    break;
                }
            }
        }
        assert_eq!(got, Some(Head::TooLong));
        assert_eq!(find_head(&buf), Head::TooLong);
    }

    /// The first value of a name wins, names are case-insensitive, values are
    /// trimmed, and lines without a colon are ignored.
    #[test]
    fn headers_are_picked_out_as_before() {
        let head = b"GET /p?q=1 HTTP/1.1\r\nrange:  bytes=0-1 \r\nRANGE: bytes=5-6\r\nnonsense\r\n\
                     Accept-Encoding:gzip\r\nX-Other: 1\r\nx-other: 2\r\nIf-None-Match: \"a\"\r\n\
                     Authorization: Basic eA==\r\nAccept: text/html\r\nIf-Modified-Since: d\r\n\r\n";
        let r = parse_head(head).unwrap();
        assert_eq!((r.method, r.path), ("GET", "/p?q=1"));
        assert_eq!(r.header("range"), Some("bytes=0-1"));
        assert_eq!(r.header("accept-encoding"), Some("gzip"));
        assert_eq!(r.header("if-none-match"), Some("\"a\""));
        assert_eq!(r.header("authorization"), Some("Basic eA=="));
        assert_eq!(r.header("accept"), Some("text/html"));
        assert_eq!(r.header("if-modified-since"), Some("d"));
        assert_eq!(r.header("x-other"), Some("1"));
        assert_eq!(r.header("nonsense"), None);
        assert_eq!(r.header("host"), None);
        assert!(r.keep_alive, "HTTP/1.1 stays open by default");
        let close = parse_head(b"GET / HTTP/1.1\r\nConnection: Close\r\n\r\n").unwrap();
        assert!(!close.keep_alive);
        let old = parse_head(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        assert!(!old.keep_alive, "HTTP/1.0 closes unless asked");
        let no_version = parse_head(b"GET /\r\n\r\n").unwrap();
        assert_eq!((no_version.path, no_version.keep_alive), ("/", false));
        assert_eq!(parse_head(b"GET\r\n\r\n").err(), Some(400), "no target");
        assert_eq!(parse_head(b"GET /\xff HTTP/1.1\r\n\r\n").err(), Some(400), "not UTF-8");
    }

    /// The allocation-free path normalizer gives the same answer as the full
    /// one, on every combination of the pieces that make paths interesting.
    #[test]
    fn relative_cow_agrees_with_relative() {
        let pieces = [
            "/",
            "/",
            "a",
            "b.js",
            ".",
            "..",
            ".x",
            ".well-known",
            "%2e",
            "%2E%2e",
            "%2f",
            "%00",
            "%",
            "%zz",
            "?",
            "#",
            "x y",
            "é",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut checked = 0;
        for _ in 0..20_000 {
            let mut p = String::new();
            for _ in 0..(next() % 7) {
                p += pieces[(next() % pieces.len() as u64) as usize];
            }
            for dotfiles in [false, true] {
                assert_eq!(
                    relative_cow(&p, dotfiles).map(|c| c.into_owned()),
                    relative(&p, dotfiles),
                    "{p:?} dotfiles={dotfiles}"
                );
                checked += 1;
            }
        }
        for p in ["/", "/a", "/a/", "/a/b.js", "/a/b.js?x=1#y", "", "a", "/%41", "//a", "/a//b", "/a/./b", "/.git/x"] {
            for dotfiles in [false, true] {
                assert_eq!(relative_cow(p, dotfiles).map(|c| c.into_owned()), relative(p, dotfiles), "{p:?}");
            }
        }
        assert!(checked > 0);
        // The ordinary paths borrow.
        assert!(matches!(relative_cow("/assets/app.3f9a.js?v=2", false), Ok(Cow::Borrowed("assets/app.3f9a.js"))));
        assert!(matches!(relative_cow("/", false), Ok(Cow::Borrowed(""))));
        assert!(matches!(relative_cow("/docs/", false), Ok(Cow::Borrowed("docs"))));
    }

    // ---- first_request: answering from the accept loop

    fn test_site(access_log: bool) -> Site {
        let cfg: Static = serde_json::from_value(serde_json::json!({"root": "/", "access_log": access_log})).unwrap();
        Site {
            root: PathBuf::from("/"),
            dir: Arc::new(OwnedFd::from(std::fs::File::open("/").unwrap())),
            open_mode: AtomicU8::new(OPEN_CACHED),
            cached_hits: AtomicU64::new(0),
            cached_misses: AtomicU64::new(0),
            cache: Cache::new(8 << 20, 1 << 16, 1000, 1024),
            cfg,
            auth: None,
            draining: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: idle::Idle::new(),
            head_timeout: HEAD_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            bufs: Mutex::new(Vec::new()),
            inline: AtomicU64::new(0),
        }
    }

    /// A cache entry for `/t.js`: a response of `body` bytes, as the server builds them.
    fn put_entry(site: &Site, body: usize) -> Arc<Entry> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {body}\r\nETag: W/\"1-2\"\r\n\
             Last-Modified: Thu, 01 Jan 1970 00:00:02 GMT\r\nConnection: keep-alive\r\n\r\n"
        );
        let conn_at = head.find("keep-alive").unwrap();
        let nm = "HTTP/1.1 304 Not Modified\r\nETag: W/\"1-2\"\r\nConnection: keep-alive\r\n\r\n";
        let mut resp = head.clone().into_bytes();
        resp.extend((0..body).map(|i| b'a' + (i % 26) as u8));
        let e = Arc::new(Entry {
            resp: resp.into(),
            file: None,
            head_len: head.len(),
            conn_at,
            not_modified: nm.as_bytes().to_vec().into(),
            nm_conn_at: nm.find("keep-alive").unwrap(),
            etag: "W/\"1-2\"".into(),
            mtime: 2,
            body_len: body as u64,
            deps: vec![],
        });
        site.cache.as_ref().unwrap().insert("t.js\u{0}0".into(), e.clone(), Instant::now());
        e
    }

    /// A connected pair: the client end, and the accepted end as the accept loop gets it.
    fn accepted() -> (std::net::TcpStream, OwnedFd) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        s.set_nonblocking(true).unwrap();
        (c, OwnedFd::from(s))
    }

    fn wait_readable(c: &std::net::TcpStream) {
        // The request has crossed loopback by the time a peek sees it.
        c.set_nonblocking(false).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }

    #[test]
    fn a_cached_first_request_is_answered_without_leaving_the_accept_loop() {
        use std::io::{Read, Write};
        let site = test_site(false);
        let e = put_entry(&site, 1000);

        // Connection: close. The whole exchange, and the connection is closed.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert_eq!(got, want.as_slice());
        assert!(String::from_utf8_lossy(&got).contains("Connection: close\r\n"));
        assert_eq!(site.cache.as_ref().unwrap().hits.load(Ordering::Relaxed), 1);
        assert_eq!(site.inline.load(Ordering::Relaxed), 1, "counted");

        // Keep-alive: answered, and the connection goes on as a task. What
        // came after the request is kept for it.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nHost: x\r\n\r\nGET /next HTTP/1.1\r\n").unwrap();
        wait_readable(&c);
        let First::Go { fd: _fd, head, pending, answered } = first_request(&site, fd, true) else {
            panic!("kept alive")
        };
        assert!(pending.is_none());
        assert!(answered, "one request is done: the next wait is the one between requests");
        assert_eq!(&head.buf[head.pos..], b"GET /next HTTP/1.1\r\n");
        let mut got = vec![0u8; e.resp.len()];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got[..], &e.resp[..], "the keep-alive response, byte for byte");

        // HEAD and a 304 come from the same entry.
        let (mut c, fd) = accepted();
        c.write_all(b"HEAD /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"HTTP/1.1 200 OK\r\n") && got.ends_with(b"\r\n\r\n"), "head only");
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nIf-None-Match: W/\"1-2\"\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"HTTP/1.1 304 Not Modified\r\n"));
    }

    #[test]
    fn what_the_quick_path_leaves_alone_is_handed_on_whole() {
        use std::io::{Read, Write};
        let site = test_site(false);
        put_entry(&site, 1000);
        let leaves = |raw: &[u8]| {
            let (mut c, fd) = accepted();
            c.write_all(raw).unwrap();
            wait_readable(&c);
            let First::Go { fd: _fd, head, pending, answered } = first_request(&site, fd, true) else {
                panic!("{raw:?}")
            };
            assert!(pending.is_none() && !answered, "{raw:?}");
            assert_eq!(&head.buf[head.pos..], raw, "every byte is still there");
            c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            assert!(c.read(&mut [0u8; 1]).is_err(), "nothing was sent: {raw:?}");
        };
        leaves(b"GET /missing.js HTTP/1.1\r\nConnection: close\r\n\r\n"); // not cached
        leaves(b"GET /t.js HTTP/1.1\r\nRange: bytes=0-1\r\nConnection: close\r\n\r\n"); // a range
        leaves(b"POST /t.js HTTP/1.1\r\nConnection: close\r\n\r\n"); // not GET or HEAD
        leaves(b"GET /t.js HTTP/1.1\r\nHost: x"); // the head is still arriving
        leaves(b"GET /t.js HTTP/1.1\r\nAccept-Encoding: br\r\n\r\n"); // another variant of the file
        leaves(b"GET /../etc/passwd HTTP/1.1\r\n\r\n");

        // Nothing sent yet: carry on.
        let (_c, fd) = accepted();
        let First::Go { head, .. } = first_request(&site, fd, true) else { panic!("no data yet") };
        assert!(head.buf.is_empty());
        // Closed without a word: finished.
        let (c, fd) = accepted();
        drop(c);
        std::thread::sleep(Duration::from_millis(30));
        assert!(matches!(first_request(&site, fd, true), First::Done));
        // Switched off: the request is not even read.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let First::Go { head, .. } = first_request(&site, fd, false) else { panic!("off") };
        assert!(head.buf.is_empty());
        // A site with Basic auth never takes the quick path.
        let mut guarded = test_site(false);
        guarded.auth = Some("Basic eDp5".into());
        put_entry(&guarded, 10);
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&guarded, fd, true), First::Go { .. }));
    }

    /// A response that does not fit the socket's room: what was sent is
    /// counted, and the rest is handed on to be written by the task.
    #[test]
    fn a_response_too_big_for_the_socket_goes_on_as_a_task() {
        use std::io::{Read, Write};
        let site = test_site(false);
        let e = put_entry(&site, 2_000_000);
        let (mut c, fd) = accepted();
        crate::sys::set_send_buffer(fd.as_fd(), 4096).unwrap();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let got = first_request(&site, fd, true);
        let First::Go { fd: _fd, pending: Some(p), answered, .. } = got else {
            panic!(
                "partial: {}",
                match got {
                    First::Done => "Done".to_string(),
                    First::Go { pending, head, .. } =>
                        format!("Go pending={} head={}", pending.is_some(), head.buf.len()),
                }
            )
        };
        assert!(!p.keep && answered);
        let total = p.out.as_slice().len();
        assert!(p.at < total, "the socket took {} of {total}", p.at);
        // What was sent plus what is pending is the whole response, in order.
        let mut sent = vec![0u8; p.at];
        c.read_exact(&mut sent).unwrap();
        let rest = p.out.advance(p.at);
        sent.extend_from_slice(rest.as_slice());
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert_eq!(sent, want.as_slice());
    }
    /// What `first_request` could not send is finished by the connection's
    /// task, which counts it as a request in flight (a drain waits for it).
    #[tokio::test(flavor = "current_thread")]
    async fn the_task_finishes_a_partial_response_and_counts_it_as_active() {
        use std::io::{Read, Write};
        let site = Arc::new(test_site(false));
        let e = put_entry(&site, 2_000_000);
        let (mut c, fd) = accepted();
        crate::sys::set_send_buffer(fd.as_fd(), 4096).unwrap();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let First::Go { fd, head, pending, answered } = first_request(&site, fd, true) else { panic!("partial") };
        assert!(pending.is_some() && answered);

        let stream = tokio::net::TcpStream::from_std(std::net::TcpStream::from(fd)).unwrap();
        let guard = site.idle.register(stream.as_raw_fd());
        let s2 = site.clone();
        let task = tokio::spawn(async move {
            let mut stream = stream;
            let guard = guard;
            let (r, w) = stream.split();
            connection(r, Conn::Tcp(w), s2, &guard, head, pending, answered).await;
        });
        // The client has read nothing, so the socket is full: the write is
        // in progress, and counted.
        let t0 = Instant::now();
        while site.active.load(Ordering::SeqCst) == 0 {
            assert!(t0.elapsed() < Duration::from_secs(5), "the task never began the write");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(site.active.load(Ordering::SeqCst), 1, "still writing");
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            got
        });
        task.await.unwrap();
        assert_eq!(site.active.load(Ordering::SeqCst), 0, "done");
        let got = reader.join().unwrap();
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert!(
            got == want.as_slice(),
            "the whole response, once, then the close ({} of {} bytes)",
            got.len(),
            want.as_slice().len()
        );
    }

    /// With the backlog never empty and every connection answered at once,
    /// the accept loop must still give the thread back now and then.
    #[tokio::test(flavor = "current_thread")]
    async fn accepting_from_a_full_backlog_lets_other_tasks_run() {
        let l = crate::sys::listen_tcp("127.0.0.1:0".parse().unwrap(), false, 1024).unwrap();
        let addr = l.local_addr().unwrap();
        let mut clients = Vec::new();
        for _ in 0..400 {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
                Ok(c) => clients.push(c),
                Err(_) => break,
            }
        }
        if clients.len() < 300 {
            eprintln!("skipped: this system queues only {} connections", clients.len());
            return;
        }
        let listener = Listener::new(l).unwrap();
        // The first wait for readiness suspends the task whatever else is
        // done (the reactor has not looked yet); start counting after it.
        let mut taken = vec![listener.accept().await.unwrap()];
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });
        while !ran.load(Ordering::SeqCst) && taken.len() < clients.len() {
            taken.push(listener.accept().await.unwrap());
        }
        assert!(ran.load(Ordering::SeqCst), "{} connections taken before any other task had a turn", taken.len());
        assert!(taken.len() < 200, "{} connections taken before another task ran", taken.len());
    }
}

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
//! default, request heads are capped at 16 KB and must arrive within 10 s.
//!
//! Small files are answered from an in-memory cache of complete responses
//! (`cache.rs`: one send(2) per hit). Connections are driven by tokio
//! (epoll) or, with `io = "uring"`, by io_uring (`uring_io.rs`): the HTTP
//! code is the same either way, only the bytes travel differently.

mod cache;
#[cfg(target_os = "linux")]
mod uring_io;

use crate::config::{Static, StaticIo};
use cache::{Cache, Dep, Entry, Lookup, Seen, Stamp};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

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
    let cache = Cache::new(cfg.cache_size, cfg.cache_max_file, cfg.cache_valid_ms);
    let io = match std::env::var("WARDEN_STATIC_IO").as_deref() {
        Ok("uring") => StaticIo::Uring,
        Ok("epoll") => StaticIo::Epoll,
        Ok(other) => {
            crate::warn!(
                "ignoring WARDEN_STATIC_IO: it is neither \"epoll\" nor \"uring\"",
                value = other,
                using = format!("{:?}", cfg.io).to_lowercase(),
                hint = "unset WARDEN_STATIC_IO, or set it to epoll or uring"
            );
            cfg.io
        }
        Err(_) => cfg.io,
    };
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
    });
    match rt.block_on(serve(site, port, io)) {
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

/// Where TCP connections come from: tokio's listener (epoll), or io_uring.
enum Listener {
    Tokio(tokio::net::TcpListener),
    #[cfg(target_os = "linux")]
    Uring(uring_io::Listener),
}

enum Accepted {
    Tokio(tokio::net::TcpStream),
    #[cfg(target_os = "linux")]
    Uring(uring_io::Stream),
}

impl Listener {
    /// The listener for `io`; io_uring falls back to epoll (with a warning
    /// that says why and what to do) wherever it can't be set up.
    fn new(l: std::net::TcpListener, io: StaticIo) -> Result<Listener, String> {
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut l = l;
        if io == StaticIo::Uring {
            #[cfg(target_os = "linux")]
            match uring_io::Listener::new(l) {
                Ok(u) => return Ok(Listener::Uring(u)),
                Err((e, back)) => {
                    crate::warn!(
                        "io_uring is unavailable, so this worker serves with epoll instead",
                        error = e,
                        hint = "io_uring is blocked by seccomp (Docker's default profile), turned off by sysctl \
                                kernel.io_uring_disabled, or missing from the kernel: allow it, or set [static] \
                                io = \"epoll\" (and unset WARDEN_STATIC_IO) to silence this"
                    );
                    l = back;
                }
            }
            #[cfg(not(target_os = "linux"))]
            crate::warn!(
                "io_uring is Linux-only, so this worker serves with the default I/O instead",
                hint = "set [static] io = \"epoll\" (and unset WARDEN_STATIC_IO) to silence this"
            );
        }
        tokio::net::TcpListener::from_std(l).map(Listener::Tokio).map_err(|e| e.to_string())
    }

    fn name(&self) -> &'static str {
        match self {
            Listener::Tokio(_) => "epoll",
            #[cfg(target_os = "linux")]
            Listener::Uring(_) => "io_uring",
        }
    }

    async fn accept(&mut self) -> std::io::Result<Accepted> {
        match self {
            Listener::Tokio(l) => l.accept().await.map(|(s, _)| Accepted::Tokio(s)),
            #[cfg(target_os = "linux")]
            Listener::Uring(l) => l.accept().await.map(Accepted::Uring),
        }
    }
}

async fn serve(site: Arc<Site>, port: u16, io: StaticIo) -> Result<(), String> {
    let reuse = std::env::var("WARDEN_REUSE_PORT").is_ok_and(|v| v == "1");
    let std_listener = reuseport_listener(&site.cfg.host, port, reuse)?;
    // Wake up for a connection only once its request has arrived. Optional:
    // without it the server is just as correct, a little busier.
    let _ = crate::sys::tcp_defer_accept(std_listener.as_fd(), HEAD_TIMEOUT.as_secs() as i32);
    let mut listener = Listener::new(std_listener, io)?;
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

    let stop_name = std::env::var("WARDEN_STOP_SIGNAL").unwrap_or_else(|_| "SIGTERM".into());
    let stop_sig = crate::signals::parse(&stop_name).unwrap_or(libc::SIGTERM);
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(stop_sig))
        .map_err(|e| format!("installing the {stop_name} handler: {e}"))?;
    let (drain_tx, drain_rx) = tokio::sync::watch::channel(false);
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));

    let unix_listener = unix.as_ref().map(|(l, _)| l);
    loop {
        tokio::select! {
            _ = stop.recv() => break,
            acc = listener.accept() => {
                let Ok(accepted) = acc else { continue };
                let Ok(permit) = slots.clone().try_acquire_owned() else { continue };
                let (site, drain) = (site.clone(), drain_rx.clone());
                match accepted {
                    Accepted::Tokio(stream) => {
                        let _ = stream.set_nodelay(true);
                        tokio::spawn(async move {
                            let _permit = permit;
                            let (r, w) = stream.into_split();
                            connection(BufReader::new(r), Conn::Tcp(w), site, drain).await;
                        });
                    }
                    #[cfg(target_os = "linux")]
                    Accepted::Uring(stream) => {
                        tokio::spawn(async move {
                            let _permit = permit;
                            let (r, w) = stream.split();
                            connection(r, Conn::Uring(w), site, drain).await;
                        });
                    }
                }
            }
            acc = async { match unix_listener { Some(l) => l.accept().await.map(|(s, _)| s), None => std::future::pending().await } } => {
                let Ok(stream) = acc else { continue };
                let (site, drain) = (site.clone(), drain_rx.clone());
                tokio::spawn(async move {
                    let (r, w) = stream.into_split();
                    connection(BufReader::new(r), Conn::Unix(w), site, drain).await;
                });
            }
        }
    }

    // Drain: stop accepting, tell idle keep-alive connections to close, let
    // requests in flight finish (at least WARDEN_DRAIN_MS, at most ~grace).
    drop(listener);
    site.draining.store(true, Ordering::SeqCst);
    let _ = drain_tx.send(true);
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
            "static cache: {} hits, {} misses, {} dropped as changed on disk, {} evicted; {files} files in {} of {} KB",
            c.hits.load(Ordering::Relaxed),
            c.misses.load(Ordering::Relaxed),
            c.stale.load(Ordering::Relaxed),
            c.evicted.load(Ordering::Relaxed),
            used.div_ceil(1024),
            cap >> 10
        );
    }
    Ok(())
}

/// Bytes to send, owned, so an io_uring send can keep them until the
/// kernel is done with them (no copy): a buffer built for this response,
/// or a range of a cached one.
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
enum Conn {
    Tcp(tokio::net::tcp::OwnedWriteHalf),
    Unix(tokio::net::unix::OwnedWriteHalf),
    #[cfg(target_os = "linux")]
    Uring(uring_io::Writer),
}

impl Conn {
    async fn write_all(&mut self, b: impl Into<OutBuf>) -> std::io::Result<()> {
        let b = b.into();
        match self {
            Conn::Tcp(w) => w.write_all(b.as_slice()).await,
            Conn::Unix(w) => w.write_all(b.as_slice()).await,
            #[cfg(target_os = "linux")]
            Conn::Uring(w) => w.send_all(b, false).await,
        }
    }

    async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(w) => w.flush().await,
            Conn::Unix(w) => w.flush().await,
            #[cfg(target_os = "linux")]
            Conn::Uring(_) => Ok(()),
        }
    }

    async fn shutdown(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(w) => w.shutdown().await,
            Conn::Unix(w) => w.shutdown().await,
            #[cfg(target_os = "linux")]
            Conn::Uring(w) => w.shutdown(),
        }
    }

    /// Response headers that a sendfile body follows: on TCP, MSG_MORE lets
    /// the kernel put them in the same packet as the first body bytes.
    async fn write_head_more(&mut self, b: impl Into<OutBuf>) -> std::io::Result<()> {
        let b = b.into();
        match self {
            Conn::Tcp(w) => {
                let b = b.as_slice();
                let sock: &tokio::net::TcpStream = w.as_ref();
                let mut off = 0;
                while off < b.len() {
                    sock.writable().await?;
                    match sock.try_io(tokio::io::Interest::WRITABLE, || crate::sys::send(sock.as_fd(), &b[off..], true))
                    {
                        Ok(n) => off += n,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => return Err(e),
                    }
                }
                Ok(())
            }
            Conn::Unix(w) => w.write_all(b.as_slice()).await,
            #[cfg(target_os = "linux")]
            Conn::Uring(w) => w.send_all(b, true).await,
        }
    }

    /// Send `count` bytes of `file` from `offset`.
    async fn send_file(&mut self, file: std::fs::File, offset: u64, count: u64) -> std::io::Result<u64> {
        match self {
            Conn::Tcp(w) => sendfile_all(w.as_ref(), &file, offset, count).await,
            Conn::Unix(w) => {
                let mut f = tokio::fs::File::from_std(file);
                if offset > 0 {
                    use tokio::io::AsyncSeekExt;
                    f.seek(std::io::SeekFrom::Start(offset)).await?;
                }
                tokio::io::copy(&mut f.take(count), w).await
            }
            #[cfg(target_os = "linux")]
            Conn::Uring(w) => w.send_file(&file, offset, count).await,
        }
    }
}

/// Zero-copy body: the kernel moves file pages to the socket, no userspace
/// buffer. Loops on partial sends and waits for writability on EAGAIN.
async fn sendfile_all(
    sock: &tokio::net::TcpStream,
    file: &std::fs::File,
    offset: u64,
    count: u64,
) -> std::io::Result<u64> {
    let mut off = offset as i64;
    let mut left = count;
    while left > 0 {
        sock.writable().await?;
        let chunk = left.min(1 << 30) as usize;
        let res = sock.try_io(tokio::io::Interest::WRITABLE, || {
            crate::sys::sendfile(sock.as_fd(), file.as_fd(), &mut off, chunk)
        });
        match res {
            // The file shrank under us: the promised Content-Length can't be
            // met, so the connection must close.
            Ok(0) => {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file truncated while sending"));
            }
            Ok(n) => left -= (n as u64).min(left),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
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

struct Request {
    method: String,
    path: String,
    keep_alive: bool,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Read one request head. A line is read at most up to the head limit, so
/// a client that never sends a newline gets 431, not unbounded buffering.
async fn read_head<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> Result<Option<Request>, u16> {
    let mut head = Vec::with_capacity(512);
    loop {
        let before = head.len();
        let room = (MAX_HEAD + 1 - before) as u64;
        let n = (&mut *r).take(room).read_until(b'\n', &mut head).await.map_err(|_| 400u16)?;
        if n == 0 {
            return if head.is_empty() { Ok(None) } else { Err(400) };
        }
        if head.len() > MAX_HEAD {
            return Err(431);
        }
        let line = &head[before..];
        if line == b"\r\n" || line == b"\n" {
            if before == 0 {
                head.clear(); // tolerate a stray empty line between requests
                continue;
            }
            break;
        }
    }
    let text = std::str::from_utf8(&head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n").flat_map(|l| l.split('\n')).filter(|l| !l.is_empty());
    let first = lines.next().ok_or(400u16)?;
    let mut parts = first.split(' ');
    let (method, target, version) =
        (parts.next().ok_or(400u16)?, parts.next().ok_or(400u16)?, parts.next().unwrap_or(""));
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
    let conn = headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("connection")).map(|(_, v)| v.to_ascii_lowercase());
    let keep_alive = match version {
        "HTTP/1.1" => conn.as_deref() != Some("close"),
        _ => conn.as_deref() == Some("keep-alive"),
    };
    Ok(Some(Request { method: method.to_string(), path: target.to_string(), keep_alive, headers }))
}

/// One client connection: requests in a loop while keep-alive holds. `r`
/// is buffered (tokio's BufReader, or the io_uring reader's own buffer).
async fn connection<R>(mut r: R, mut w: Conn, site: Arc<Site>, mut drain: tokio::sync::watch::Receiver<bool>)
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut first = true;
    loop {
        // A new connection always gets its first request served (a client
        // that connected just before the drain began must not see an empty
        // reply); only idle keep-alive connections are closed by the drain.
        let head = if first {
            first = false;
            tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut r)).await
        } else {
            tokio::select! {
                biased;
                h = tokio::time::timeout(IDLE_TIMEOUT, read_head(&mut r)) => h,
                _ = drain.wait_for(|d| *d) => return,
            }
        };
        let req = match head {
            Ok(Ok(Some(req))) => req,
            Ok(Ok(None)) | Err(_) => return,
            Ok(Err(code)) => {
                let _ = respond_error(&mut w, code, false).await;
                return;
            }
        };
        site.active.fetch_add(1, Ordering::SeqCst);
        let t0 = Instant::now();
        let keep = req.keep_alive && !site.draining.load(Ordering::SeqCst);
        let mut cached = "";
        let result = handle(&req, &site, &mut w, keep, &mut cached).await;
        site.active.fetch_sub(1, Ordering::SeqCst);
        if site.cfg.access_log {
            let (status, bytes) = result.as_ref().map(|x| *x).unwrap_or((0, 0));
            println!(
                "{} {} {status} {bytes}B {:.1}ms{cached}",
                req.method,
                req.path,
                t0.elapsed().as_secs_f64() * 1000.0
            );
        }
        if result.is_err() || !keep || site.draining.load(Ordering::SeqCst) {
            let _ = w.shutdown().await;
            return;
        }
    }
}

async fn respond_error(w: &mut Conn, code: u16, keep: bool) -> std::io::Result<(u16, u64)> {
    let body = format!("{code} {}\n", reason(code));
    let mut head = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: {}\r\n",
        reason(code),
        body.len(),
        if keep { "keep-alive" } else { "close" }
    );
    if code == 405 {
        head += "Allow: GET, HEAD\r\n";
    }
    head += "\r\n";
    let len = body.len() as u64;
    w.write_all(head).await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok((code, len))
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
/// slash (`/docs` is a redirect, `/docs/` the index page).
fn cache_key(rel: &str, cfg: &Static, req: &Request, slash: bool) -> String {
    let mut mask = 0u8;
    if cfg.precompressed {
        let accept = req.header("accept-encoding").unwrap_or("");
        for (bit, (enc, _)) in ENCODINGS.iter().enumerate() {
            if accepts(accept, enc) {
                mask |= 1 << bit;
            }
        }
    }
    // NUL can't be in `rel` (relative() refuses it), so keys can't collide.
    format!("{rel}\0{mask}{}", if slash { "/" } else { "" })
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
    req: &Request,
    site: &Site,
    w: &mut Conn,
    keep: bool,
    cached: &mut &'static str,
) -> std::io::Result<(u16, u64)> {
    if req.method != "GET" && req.method != "HEAD" {
        return respond_error(w, 405, keep).await;
    }
    if let Some(expected) = &site.auth {
        if req.header("authorization") != Some(expected.as_str()) {
            let body = "401 Unauthorized\n";
            let head = format!(
                "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"warden\"\r\nContent-Type: text/plain\r\n\
                 Content-Length: {}\r\nConnection: {}\r\n\r\n",
                body.len(),
                if keep { "keep-alive" } else { "close" }
            );
            w.write_all(head).await?;
            w.write_all(body.to_string()).await?;
            w.flush().await?;
            return Ok((401, body.len() as u64));
        }
    }
    let cfg = &site.cfg;
    let rel = match relative(&req.path, cfg.dotfiles) {
        Ok(p) => p,
        Err(code) => return not_found_or(w, site, req, code, keep).await,
    };
    let url_path = req.path.split(['?', '#']).next().unwrap_or("/");
    // The cache answers plain GET / HEAD (conditional or not) for a path
    // that passed the checks above; ranges take the normal path.
    let mut fill = None;
    if let Some(cache) = &site.cache {
        if req.header("range").is_none() {
            let key = cache_key(&rel, cfg, req, url_path.ends_with('/'));
            let now = Instant::now();
            let hit = match cache.lookup(&key, now) {
                Lookup::Fresh(e) => Some(e),
                Lookup::Stale(e) if still_matches(site, &e.deps).await => {
                    cache.confirm(&key, &e, now);
                    Some(e)
                }
                Lookup::Stale(e) => {
                    cache.invalidate(&key, &e);
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
            fill = Some(Fill { key, deps: Vec::new() });
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
                    f.deps.push(Dep { path: rel.as_str().into(), seen: Seen::Dir });
                    f.deps.push(Dep { path: index.as_str().into(), seen: Seen::File(Stamp::of(&o.meta)) });
                }
                send_file(w, site, req, &index, o, 200, keep, fill).await
            }
            _ => {
                let dir = if rel.is_empty() { site.root.clone() } else { site.root.join(&rel) };
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
        f.deps.push(Dep { path: rel.as_str().into(), seen: Seen::File(Stamp::of(&found.meta)) });
    }
    send_file(w, site, req, &rel, found, 200, keep, fill).await
}

/// SPA fallback to index.html, then 404.html, then a plain 404.
async fn not_found_or(w: &mut Conn, site: &Site, req: &Request, code: u16, keep: bool) -> std::io::Result<(u16, u64)> {
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
    respond_error(w, code, keep).await
}

async fn listing(w: &mut Conn, dir: &Path, url_path: &str, head_only: bool, keep: bool) -> std::io::Result<(u16, u64)> {
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
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\
         Connection: {}\r\n\r\n",
        body.len(),
        if keep { "keep-alive" } else { "close" }
    );
    let len = body.len() as u64;
    w.write_all(head).await?;
    if !head_only {
        w.write_all(body).await?;
    }
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

/// If-None-Match (wins when present), else If-Modified-Since.
fn is_not_modified(req: &Request, etag: &str, mtime: u64) -> bool {
    match req.header("if-none-match") {
        Some(tags) => tags.split(',').any(|t| t.trim() == etag || t.trim() == "*"),
        None => req.header("if-modified-since").and_then(parse_http_date).is_some_and(|since| mtime <= since),
    }
}

/// Answer from a cached entry: 304, HEAD or the full response, the same
/// bytes the normal path sends. Keep-alive: one send of the stored bytes.
async fn respond_cached(w: &mut Conn, req: &Request, e: &Arc<Entry>, keep: bool) -> std::io::Result<(u16, u64)> {
    let (buf, conn_at, end, status, bytes) = if is_not_modified(req, &e.etag, e.mtime) {
        (&e.not_modified, e.nm_conn_at, e.not_modified.len(), 304, 0)
    } else if req.method == "HEAD" {
        (&e.resp, e.conn_at, e.head_len, 200, 0)
    } else {
        (&e.resp, e.conn_at, e.resp.len(), 200, e.body_len)
    };
    if keep {
        w.write_all(OutBuf::Shared(buf.clone(), 0..end)).await?;
    } else {
        let mut v = Vec::with_capacity(end);
        v.extend_from_slice(&buf[..conn_at]);
        v.extend_from_slice(b"close");
        v.extend_from_slice(&buf[conn_at + cache::KEEP_ALIVE.len()..end]);
        w.write_all(v).await?;
    }
    w.flush().await?;
    Ok((status, bytes))
}

/// `rel` is the file's path under the root (its name picks the MIME type
/// and cache policy); `found` is that file, already open. With `fill` (a
/// cache miss), a small 200 response is built once, cached, and sent from
/// the cache entry.
#[allow(clippy::too_many_arguments)]
async fn send_file(
    w: &mut Conn,
    site: &Site,
    req: &Request,
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
    let cache_control = if html || status != 200 {
        "no-cache".to_string()
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
            let entry = Arc::new(Entry {
                resp: buf.into(),
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
    w.write_head_more(head).await?;
    let sent = w.send_file(body.file, start, count).await?;
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

    fn req(lines: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".into(),
            path: "/a.css".into(),
            keep_alive: true,
            headers: lines.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        }
    }

    #[test]
    fn cache_keys_separate_what_changes_the_response() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        let plain = cache_key("a.css", &cfg, &req(&[]), false);
        let both = cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "gzip, deflate, br")]), false);
        let gz = cache_key("a.css", &cfg, &req(&[("accept-encoding", "gzip;q=1")]), false);
        let slash = cache_key("a.css", &cfg, &req(&[]), true);
        let all = [&plain, &both, &gz, &slash];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Encodings the server never picks don't split the cache.
        assert_eq!(cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "deflate, zstd")]), false), plain);
        // Without precompressed files, Accept-Encoding doesn't matter.
        cfg.precompressed = false;
        assert_eq!(
            cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "br")]), false),
            cache_key("a.css", &cfg, &req(&[]), false)
        );
        assert_ne!(cache_key("a", &cfg, &req(&[]), false), cache_key("a\u{1}", &cfg, &req(&[]), false));
    }

    #[test]
    fn conditional_requests() {
        let etag = "W/\"3-5\"";
        assert!(is_not_modified(&req(&[("If-None-Match", "\"x\", W/\"3-5\"")]), etag, 5));
        assert!(is_not_modified(&req(&[("If-None-Match", "*")]), etag, 5));
        // If-None-Match wins over If-Modified-Since.
        assert!(!is_not_modified(
            &req(&[("If-None-Match", "\"x\""), ("If-Modified-Since", "Wed, 30 Sep 2026 12:00:01 GMT")]),
            etag,
            5
        ));
        assert!(is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:05 GMT")]), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "garbage")]), etag, 5));
        assert!(!is_not_modified(&req(&[]), etag, 5));
    }

    /// A head line that never ends is refused at the limit, not buffered.
    #[tokio::test]
    async fn endless_head_lines_are_cut_off() {
        let endless = tokio::io::repeat(b'a');
        let mut r = BufReader::new(endless);
        assert_eq!(read_head(&mut r).await.err(), Some(431));
        let mut r = BufReader::new(&b"\r\nGET /x HTTP/1.0\r\nConnection: keep-alive\r\nA:b\r\n\r\nrest"[..]);
        let req = read_head(&mut r).await.unwrap().unwrap();
        assert_eq!((req.method.as_str(), req.path.as_str(), req.keep_alive), ("GET", "/x", true));
        assert_eq!(req.header("a"), Some("b"));
        let mut rest = String::new();
        r.read_to_string(&mut rest).await.unwrap();
        assert_eq!(rest, "rest");
        let mut r = BufReader::new(&b"GET / HTTP/1.1\r\nHost: x"[..]);
        assert_eq!(read_head(&mut r).await.err(), Some(400), "cut short");
        let mut r = BufReader::new(&b""[..]);
        assert!(read_head(&mut r).await.unwrap().is_none(), "clean end");
    }
}

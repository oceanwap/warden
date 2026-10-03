//! Warden's built-in static file server (`warden serve-static`, run as the
//! worker process of an app with a `[static]` section, like `pm2 serve` or
//! `npx serve`). It is supervised like any app: several workers share the
//! port with SO_REUSEPORT, each reports readiness and a heartbeat on Warden's
//! IPC pipe and serves a private health socket, and it drains on the stop
//! signal, so rolling restarts drop no requests.
//!
//! HTTP/1.1 with keep-alive; GET and HEAD; ETag / Last-Modified with 304s;
//! single byte ranges (206); precompressed `.br` / `.gz` siblings; SPA
//! fallback; `404.html`; Basic auth; extra headers. Paths can't leave the
//! root (`..`, symlinks pointing outside, NUL), dotfiles are hidden by
//! default, request heads are capped at 16 KB and must arrive within 10 s (to
//! within a second).
//!
//! Files are not cached: each request opens its file and sends it (a small
//! one in a single write, a larger one with sendfile(2)), which costs about
//! what nginx's open-file cache saves it. An opt-in cache of complete
//! responses (`cache_size`, `cache.rs`) exists for sites that want it.
//! Connections are driven by tokio (epoll). An io_uring transport was tried
//! and dropped: it was slower than epoll (docs/benchmarks.md).
//!
//! Per request the code allocates next to nothing: the head is parsed where
//! it lies in a reused buffer (`Request` borrows it), the response head is
//! written into one buffer with the body of a small file, and the waits for a
//! request are limited by one sweep a second (`idle.rs`), not a timer each. A
//! new connection whose request is already in the socket never becomes a task
//! when its answer fits the socket: `first_request` answers it from the
//! accept loop.
//!
//! The parts:
//! - `serve`: the worker's life (bind, announce, accept loop, drain);
//!   `accept`: the listener and the pacing of failed accepts;
//! - `connection`: a connection's requests, and `first_request`;
//!   `head`: finding and parsing a request head; `conn`: writing a response;
//! - `handler`: answering a request; `path` and `open`: from the URL to an
//!   open file; `names`: MIME types and fingerprinted names;
//! - `response` and `text`: heads and small generated responses, and the
//!   numbers and dates in them; `cached` and `cache`: the optional cache;
//!   `idle`: the limits on connections waiting for a request.

mod accept;
mod cache;
mod cached;
mod compress;
mod conn;
mod connection;
mod handler;
mod head;
mod idle;
mod job;
mod names;
mod open;
mod path;
mod response;
mod serve;
mod store;
mod text;

use crate::config::Static;
use cache::Cache;
use head::Request;
use open::{OPEN_BENEATH, OPEN_CACHED, OPEN_LEGACY};
use response::{html_cache_control, plain_cache_control};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const HEAD_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(15);

/// The site one worker serves, and the counters it keeps.
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
    /// The parts of a response that the configuration decides, made once.
    fixed: Fixed,
    /// Extensions of files that are not looked up for a compressed sibling
    /// (`precompressed_skip`).
    skip_siblings: names::ExtSet,
    /// Makes compressed copies of files in the background (`compress`); None
    /// when that is off or its directory could not be had.
    compressor: Option<Arc<compress::Compressor>>,
    /// The `Authorization` value a request needs (`basic_auth`).
    auth: Option<String>,
    /// Prebuilt responses of small files (None: `cache_size = 0`, the default).
    cache: Option<Cache>,
    /// The stop signal has arrived: answer with `Connection: close`.
    draining: AtomicBool,
    /// Requests in flight (`Busy`).
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
    /// The responses sent, by status, for the heartbeat (`None`:
    /// `WARDEN_REQUESTS=0`, `[metrics] requests = false`).
    responses: Option<Arc<Responses>>,
}

/// Responses by status class since the worker started: one add per response.
#[derive(Default)]
struct Responses {
    ok: AtomicU64,
    redirect: AtomicU64,
    client_error: AtomicU64,
    not_found: AtomicU64,
    server_error: AtomicU64,
}

impl Responses {
    fn count(&self, status: u16) {
        let class = match status {
            200..=299 => &self.ok,
            300..=399 => &self.redirect,
            404 => {
                self.not_found.fetch_add(1, Ordering::Relaxed);
                &self.client_error
            }
            400..=499 => &self.client_error,
            500..=599 => &self.server_error,
            _ => return,
        };
        class.fetch_add(1, Ordering::Relaxed);
    }

    /// What the heartbeat carries (`IpcMsg::requests` reads it).
    fn json(&self) -> serde_json::Value {
        let n = |c: &AtomicU64| c.load(Ordering::Relaxed);
        serde_json::json!({
            "2xx": n(&self.ok),
            "3xx": n(&self.redirect),
            "4xx": n(&self.client_error),
            "404": n(&self.not_found),
            "5xx": n(&self.server_error),
        })
    }
}

impl Site {
    /// Count a response that was sent with `status`.
    fn responded(&self, status: u16) {
        if let Some(r) = &self.responses {
            r.count(status);
        }
    }
}

/// Header text that depends on the configuration only, so a request copies
/// it instead of building it.
struct Fixed {
    /// Cache-Control of a file that is neither a page nor fingerprinted.
    cc_plain: String,
    /// Cache-Control of an HTML page.
    cc_html: String,
    /// Cache-Control of a fingerprinted file: a year, immutable.
    cc_immutable: &'static str,
    /// The configured `headers`, one `Name: value\r\n` line each.
    extra: String,
}

impl Fixed {
    fn new(cfg: &Static) -> Fixed {
        let mut extra = String::new();
        for (k, v) in &cfg.headers {
            extra += &format!("{k}: {v}\r\n");
        }
        // `private` behind a password: `public` would let a shared cache
        // (a CDN, a proxy) keep the file and give it to anyone.
        let cc_immutable = if cfg.basic_auth.is_some() {
            "private, max-age=31536000, immutable"
        } else {
            "public, max-age=31536000, immutable"
        };
        Fixed { cc_plain: plain_cache_control(cfg), cc_html: html_cache_control(cfg), cc_immutable, extra }
    }
}

/// A request and how it is answered: what handlers need besides the
/// connection to write to.
#[derive(Clone, Copy)]
struct Exchange<'a> {
    site: &'a Site,
    req: &'a Request<'a>,
    /// The connection stays open for more requests afterwards.
    keep: bool,
}

impl Exchange<'_> {
    fn head_only(&self) -> bool {
        self.req.method == "HEAD"
    }
}

/// A request in flight, counted as one until this is dropped; a drain waits
/// for them.
struct Busy<'a>(&'a AtomicUsize);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Site {
    fn busy(&self) -> Busy<'_> {
        self.active.fetch_add(1, Ordering::SeqCst);
        Busy(&self.active)
    }

    /// The site as Warden's environment describes it, and the port to serve
    /// it on. The error is what to tell the person who started the command.
    fn from_env() -> Result<(Arc<Site>, u16), String> {
        let cfg: Static = std::env::var("WARDEN_STATIC")
            .map_err(|e| e.to_string())
            .and_then(|j| serde_json::from_str(&j).map_err(|e| e.to_string()))
            .map_err(|e| format!("WARDEN_STATIC is missing or invalid ({e}); this command is run by Warden"))?;
        let port: u16 = std::env::var("PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .ok_or("PORT is not set; set app.port in the config")?;
        // Resolve `current` symlinks once: this worker serves one release.
        let root = std::fs::canonicalize(&cfg.root).map_err(|e| format!("static.root {}: {e}", cfg.root.display()))?;
        if !root.is_dir() {
            return Err(format!("{} is not a directory", root.display()));
        }
        let dir = std::fs::File::open(&root)
            .map(|f| Arc::new(OwnedFd::from(f)))
            .map_err(|e| format!("cannot open static.root {}: {e}", root.display()))?;
        let auth = cfg.basic_auth.as_deref().map(|a| format!("Basic {}", text::base64(a.as_bytes())));
        // Troubleshooting and tests: force a slower way of opening files.
        let open_mode = match std::env::var("WARDEN_STATIC_OPEN").as_deref() {
            // Not Linux: no openat2 (sys::openat2 is Unsupported), realpath only.
            _ if !cfg!(target_os = "linux") => OPEN_LEGACY,
            Ok("beneath") => OPEN_BENEATH,
            Ok("legacy") => OPEN_LEGACY,
            _ => OPEN_CACHED,
        };
        let (head_timeout, idle_timeout) = std::env::var("WARDEN_STATIC_TIMEOUTS")
            .ok()
            .and_then(|v| {
                let (h, i) = v.split_once(',')?;
                Some((Duration::from_secs(h.trim().parse().ok()?), Duration::from_secs(i.trim().parse().ok()?)))
            })
            .unwrap_or((HEAD_TIMEOUT, IDLE_TIMEOUT));
        // Background compression needs its directory; without one (a worker
        // started by hand) it is off. A directory that cannot be had is
        // reported, and serving goes on without.
        let compressor = if cfg.compress && cfg.precompressed && cfg.compress_dir.is_some() {
            match compress::Compressor::start(&cfg, &root, crate::supervisor::own_exe()) {
                Ok(c) => Some(c),
                Err(e) => {
                    eprintln!("warden serve-static: compression in the background is off: {e}");
                    None
                }
            }
        } else {
            None
        };
        let site = Site {
            root,
            dir,
            open_mode: AtomicU8::new(open_mode),
            cached_hits: AtomicU64::new(0),
            cached_misses: AtomicU64::new(0),
            fixed: Fixed::new(&cfg),
            skip_siblings: names::ExtSet::new(&cfg.precompressed_skip),
            compressor,
            cache: Cache::new(cfg.cache_size, cfg.cache_max_file, cfg.cache_valid_ms, crate::sys::nofile_limit().0),
            cfg,
            auth,
            draining: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: idle::Idle::new(),
            head_timeout,
            idle_timeout,
            bufs: Mutex::new(Vec::new()),
            inline: AtomicU64::new(0),
            responses: (std::env::var("WARDEN_REQUESTS").as_deref() != Ok("0")).then(Default::default),
        };
        Ok((Arc::new(site), port))
    }
}

/// Entry point of `warden static-compress`: one background compression, run
/// by a worker (`compress.rs`).
pub fn compress_main() -> i32 {
    job::main()
}

/// Entry point of `warden serve-static` (started by the supervisor). Exits 78
/// (EX_CONFIG) when the configuration or the environment is wrong, 1 when the
/// server cannot run.
pub fn main() -> i32 {
    let run = || -> Result<(), (i32, String)> {
        let (site, port) = Site::from_env().map_err(|e| (78, e))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| (1, format!("cannot start the runtime: {e}")))?;
        rt.block_on(serve::serve(site, port)).map_err(|e| (1, e))
    };
    match run() {
        Ok(()) => 0,
        Err((code, e)) => {
            eprintln!("warden serve-static: {e}");
            code
        }
    }
}

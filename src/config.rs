//! TOML configuration. Deliberately flat: one file, a handful of sections,
//! every field optional except `[app] name` and the command / entry.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub app: App,
    #[serde(default)]
    pub workers: Workers,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default)]
    pub shutdown: Shutdown,
    #[serde(default)]
    pub health: Health,
    #[serde(default)]
    pub reload: Reload,
    #[serde(default)]
    pub watchdog: Watchdog,
    #[serde(default)]
    pub limits: Limits,
    /// Opt-in file watching: a rolling restart when files change (PM2's
    /// `watch`). Off by default.
    #[serde(default)]
    pub watch: Watch,
    #[serde(default)]
    pub logging: Logging,
    #[serde(default)]
    pub metrics: Metrics,
    #[serde(default)]
    pub control: Control,
    /// Serve a directory of static files with Warden's built-in server
    /// (`warden serve <dir> <port>`, like `pm2 serve`). `app.command` is then
    /// not used.
    #[serde(default, rename = "static")]
    pub static_files: Option<Static>,
    /// Hostnames nginx sends to this app (`warden expose`, which writes this
    /// section and the nginx site file). Warden itself never reads requests.
    #[serde(default)]
    pub expose: Option<Expose>,
    /// Warden's own hostname router (`docs/routing.md`): the workers listen
    /// on `app.port` (443), read the hostname each TLS connection asks for
    /// and pass the still-encrypted connection to that app's port. Several
    /// apps share one IP; each app does its own TLS. `app.command` is then not
    /// used.
    #[serde(default)]
    pub route: Option<Route>,
}

/// `[route]`: hostname → app.
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// `"api.example.com" = "api"` (an app's name: its `app.port`) or
    /// `= 8480` (a port). `*.example.com` matches one label; `"*"` takes
    /// every hostname nothing else matches (and connections without one).
    pub hosts: std::collections::BTreeMap<String, RouteTarget>,
    /// Address to listen on (the port is `app.port`).
    #[serde(default = "default_static_host")]
    pub host: String,
    /// Apps see the visitor's IP address, not the router's: the router
    /// connects from the visitor's address (Linux, IP_TRANSPARENT, with two
    /// routing rules Warden adds; needs root). Default: on, on Linux.
    #[serde(default)]
    pub client_ip: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(untagged)]
pub enum RouteTarget {
    Port(u16),
    App(String),
}

impl Route {
    /// Whether apps get the visitor's address (the default on Linux).
    pub fn client_ip(&self) -> bool {
        self.client_ip.unwrap_or(cfg!(target_os = "linux"))
    }

    pub fn check(&self) -> Result<(), String> {
        if self.hosts.is_empty() {
            return Err(
                "route.hosts must send at least one hostname to an app, e.g. \"api.example.com\" = \"api\"".into()
            );
        }
        for (h, t) in &self.hosts {
            if h != "*" {
                valid_hostname(h).map_err(|e| format!("route.hosts: {e}"))?;
                if h.chars().any(|c| c.is_ascii_uppercase()) {
                    return Err(format!("route.hosts: write {h:?} in lowercase"));
                }
            }
            match t {
                RouteTarget::Port(0) => return Err(format!("route.hosts.{h:?}: port 0 is not a port")),
                RouteTarget::App(name)
                    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) =>
                {
                    return Err(format!("route.hosts.{h:?} = {name:?}: an app's name, or a port number"));
                }
                _ => {}
            }
        }
        if self.host.parse::<std::net::IpAddr>().is_err() {
            return Err(format!("route.host {:?} is not an IP address", self.host));
        }
        if self.client_ip == Some(true) && !cfg!(target_os = "linux") {
            return Err("route.client_ip needs Linux (IP_TRANSPARENT); set client_ip = false here".into());
        }
        Ok(())
    }
}

/// `[expose]`: what `warden expose` wrote the nginx site file from, so running
/// it again (another hostname, a new certificate, `--remove`) rewrites the
/// same file.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Expose {
    /// `server_name`s: api.example.com, or *.example.com.
    pub hosts: Vec<String>,
    /// TLS certificate chain and key for nginx; unset: plain HTTP on port 80
    /// (TLS ends at Cloudflare or a load balancer in front).
    #[serde(default)]
    pub cert: Option<PathBuf>,
    #[serde(default)]
    pub key: Option<PathBuf>,
    /// Instead of cert/key: nginx's ACME module gets a Let's Encrypt
    /// certificate and renews it; this is the contact e-mail.
    #[serde(default)]
    pub acme: Option<String>,
    /// The nginx site file (default: warden-<app>.conf in nginx's conf.d).
    #[serde(default)]
    pub site: Option<PathBuf>,
    /// Paths that carry WebSockets / Server-Sent Events: long read timeouts,
    /// no buffering for SSE.
    #[serde(default)]
    pub websocket_paths: Vec<String>,
    #[serde(default)]
    pub sse_paths: Vec<String>,
}

impl Expose {
    pub fn check(&self) -> Result<(), String> {
        if self.hosts.is_empty() {
            return Err("expose.hosts must name at least one hostname".into());
        }
        for h in &self.hosts {
            valid_hostname(h).map_err(|e| format!("expose.hosts: {e}"))?;
        }
        if self.cert.is_some() != self.key.is_some() {
            return Err("expose.cert and expose.key go together (the certificate chain and its key)".into());
        }
        if let Some(a) = &self.acme {
            if self.cert.is_some() {
                return Err("expose.acme and expose.cert are two ways to get a certificate: set one".into());
            }
            if !a.contains('@') || a.chars().any(|c| c.is_whitespace() || ";{}\"'".contains(c)) {
                return Err(format!("expose.acme = {a:?}: an e-mail address for Let's Encrypt"));
            }
            if let Some(h) = self.hosts.iter().find(|h| h.starts_with("*.")) {
                return Err(format!(
                    "expose: {h} is a wildcard, which Let's Encrypt's HTTP check cannot cover; use cert/key"
                ));
            }
        }
        for p in self.websocket_paths.iter().chain(&self.sse_paths) {
            if !p.starts_with('/') || p.chars().any(|c| c.is_whitespace() || c == ';' || c == '{' || c == '}') {
                return Err(format!("expose: {p:?} is not a URL path (starts with /, no spaces, ; or braces)"));
            }
        }
        for p in [&self.cert, &self.key, &self.site].into_iter().flatten() {
            let s = p.to_string_lossy();
            if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == ';' || c == '{' || c == '}' || c == '"') {
                return Err(format!("expose: path {s:?} cannot go in an nginx file (no spaces, quotes, ; or braces)"));
            }
        }
        Ok(())
    }
}

/// A name nginx's `server_name` takes as an exact or leading-wildcard name:
/// labels of [A-Za-z0-9-], optionally `*.` first.
pub fn valid_hostname(h: &str) -> Result<(), String> {
    let rest = h.strip_prefix("*.").unwrap_or(h);
    let ok = !rest.is_empty()
        && h.len() <= 253
        && rest.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        });
    if ok { Ok(()) } else { Err(format!("{h:?} is not a hostname (like api.example.com or *.example.com)")) }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Static {
    /// Directory to serve; relative paths are relative to the working
    /// directory (or the config file). A `current` symlink is resolved when
    /// each worker starts, so a rolling restart picks up a new release.
    pub root: PathBuf,
    /// Address to bind (the port is `app.port`).
    #[serde(default = "default_static_host")]
    pub host: String,
    /// Single-page app: unknown paths get `index.html`.
    #[serde(default)]
    pub spa: bool,
    #[serde(default = "default_index")]
    pub index: String,
    /// Opt in to browsers reusing files other than HTML pages for this many
    /// seconds (`Cache-Control: max-age`) instead of asking again on every
    /// use: the default 0 sends `no-cache` (a 304 while unchanged), so a
    /// deploy shows at once. A file changed within that time is shown old
    /// until it runs out. Fingerprinted names like `app.3f9a2c1b.js` are
    /// cached a year, immutable, whatever this says; HTML follows
    /// `html_max_age`.
    #[serde(default)]
    pub cache_max_age: u64,
    /// Opt in to browsers reusing HTML pages for this many seconds
    /// (`Cache-Control: max-age`, at most a year) instead of asking again on
    /// every page load: the default `no-cache` costs a round trip (a 304) per
    /// navigation. A new deploy reaches a browser only after this time, so
    /// keep it short (60-300). Unset or 0: always revalidate.
    #[serde(default)]
    pub html_max_age: Option<u64>,
    /// HTML listing for directories without an index.
    #[serde(default)]
    pub listing: bool,
    /// Serve dotfiles (except `.well-known`, always served).
    #[serde(default)]
    pub dotfiles: bool,
    /// Serve `file.br` / `file.gz` next to `file` when the client accepts them.
    #[serde(default = "yes")]
    pub precompressed: bool,
    /// File extensions that are never looked up for a `.br` / `.gz` sibling:
    /// formats that are compressed already, where a sibling would save
    /// nothing and the lookups (two failed system calls) are a cost of every
    /// request. The default is `PRECOMPRESSED_SKIP`; a list replaces it, and
    /// `[]` looks up every file.
    #[serde(default = "default_precompressed_skip", deserialize_with = "extensions")]
    pub precompressed_skip: Vec<String>,
    /// Build `.br` and `.gz` copies of files in the background, and serve
    /// them from the next request on (the request that finds none is answered
    /// with the file as it is). Needs `precompressed`. The copies live in
    /// `compress_dir`, never in `root`, and belong to one version of the file:
    /// an edited file is not served from an old copy.
    #[serde(default = "yes")]
    pub compress: bool,
    /// Compressions running at once, all workers of the site together
    /// (they run at the lowest priority); 0: one per CPU core.
    #[serde(default)]
    pub compress_jobs: u32,
    /// Where the copies are kept: a private folder (created with mode 0700,
    /// refused when it is not yours or lies inside `root`). Unset: a folder
    /// of Warden's state directory named after the app.
    #[serde(default)]
    pub compress_dir: Option<PathBuf>,
    /// About this much room for copies (per site); the oldest are removed
    /// first.
    #[serde(default = "default_compress_dir_size", deserialize_with = "size_bytes")]
    pub compress_dir_size: u64,
    /// Files smaller than this are sent as they are.
    #[serde(default = "default_compress_min_file", deserialize_with = "size_bytes")]
    pub compress_min_file: u64,
    /// Files larger than this are sent as they are.
    #[serde(default = "default_compress_max_file", deserialize_with = "size_bytes")]
    pub compress_max_file: u64,
    /// `user:password` for HTTP Basic auth.
    #[serde(default)]
    pub basic_auth: Option<String>,
    /// Extra response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// One stdout line per request (method, path, status, bytes, ms).
    #[serde(default)]
    pub access_log: bool,
    /// Per worker: memory for complete prebuilt responses of small files
    /// ("16MB"). Off (0) unless set: like nginx, files are read from the
    /// operating system's page cache on every request, which costs about as
    /// much as a hit here (docs/benchmarks.md) and never serves a stale or
    /// memory-hungry copy. A hit is one send(2).
    #[serde(default = "default_cache_size", deserialize_with = "size_bytes")]
    pub cache_size: u64,
    /// Files larger than this are not cached (they go out with sendfile).
    /// At most 16M: a miss reads the whole file into memory first.
    #[serde(default = "default_cache_max_file", deserialize_with = "size_bytes")]
    pub cache_max_file: u64,
    /// A cached file is checked against the disk at most this often (ms):
    /// an edited or deleted file is served fresh within this time.
    #[serde(default = "default_cache_valid_ms")]
    pub cache_valid_ms: u64,
}

/// `static.precompressed_skip` unless it is set: images, fonts, audio, video,
/// archives, and PDF and office documents (archives inside).
pub const PRECOMPRESSED_SKIP: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "heic", "jxl", // images
    "woff", "woff2", // fonts
    "mp3", "m4a", "aac", "flac", "ogg", "opus", "mp4", "m4v", "mov", "mkv", "webm", // audio and video
    "zip", "gz", "br", "zst", "bz2", "xz", "7z", "rar", "tgz", "jar", "apk", // archives
    "pdf", "docx", "xlsx", "pptx", "odt", "ods", "odp", "epub", // documents
];

fn default_precompressed_skip() -> Vec<String> {
    PRECOMPRESSED_SKIP.iter().map(|e| e.to_string()).collect()
}

/// `precompressed_skip = ["png", ".PDF"]`: extensions, made lowercase and
/// without their dot (the worker matches against the last extension of a file
/// name in lowercase).
fn extensions<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    use serde::de::Error;
    Vec::<String>::deserialize(d)?
        .into_iter()
        .map(|given| {
            let ext = given.trim().trim_start_matches('.').to_ascii_lowercase();
            let fine = !ext.is_empty()
                && ext.len() <= 12
                && ext.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-+".contains(&b));
            if fine {
                Ok(ext)
            } else {
                Err(D::Error::custom(format!(
                    "static.precompressed_skip: {given:?} is not a file extension (letters and digits, \
                     at most 12, like \"png\"; a dot in front is fine)"
                )))
            }
        })
        .collect()
}

fn default_compress_dir_size() -> u64 {
    256 << 20
}
fn default_compress_min_file() -> u64 {
    1 << 10
}
fn default_compress_max_file() -> u64 {
    8 << 20
}

fn default_cache_size() -> u64 {
    0
}
fn default_cache_max_file() -> u64 {
    64 << 10
}
fn default_cache_valid_ms() -> u64 {
    1000
}

fn default_static_host() -> String {
    "0.0.0.0".into()
}
fn default_index() -> String {
    "index.html".into()
}
/// `static.html_max_age` at most: a year, like fingerprinted assets.
pub const MAX_HTML_MAX_AGE: u64 = 31_536_000;
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub name: String,
    /// Executable to run. Process mode: the app itself (`bun`, `node`, …).
    /// Worker mode: the Bun binary that hosts the Workers.
    #[serde(default = "default_command")]
    pub command: String,
    /// Process mode: arguments to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Worker mode: the module each Worker imports (e.g. `dist/main.js`).
    pub entry: Option<String>,
    pub working_directory: Option<PathBuf>,
    /// Port the app listens on. Enables readiness detection and sets `PORT`.
    pub port: Option<u16>,
    /// This app's own IP address (an IPv6 from the server's /64, or a spare
    /// IPv4): its workers listen on `address:port` only, so several apps can
    /// each have port 443 on one server with nothing in front
    /// (`docs/routing.md`). On Linux the supervisor adds the address to the
    /// network interface when it is missing. Needs the shim (Bun) or Node.
    pub address: Option<std::net::IpAddr>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A file of `KEY=VALUE` lines (secrets kept out of the config, mode
    /// 0600), relative to the config file. Read when the config is loaded, so
    /// `warden reload` picks up changes. `env` wins over it.
    pub env_file: Option<PathBuf>,
    /// The variables read from `env_file` (filled by `load`).
    #[serde(skip)]
    pub env_from_file: BTreeMap<String, String>,
    /// Inject Warden's Bun shim (reusePort, readiness, drain).
    /// Default: on when `command` is `bun`.
    pub shim: Option<bool>,
    /// Group name for fleet commands (`warden reload backend`), like PM2's
    /// namespace. Default: "default".
    pub namespace: Option<String>,
    /// Env var holding the worker's 0-based index (PM2's `instance_var`), so
    /// apps that run cron jobs only on instance 0 keep working. "" = unset.
    #[serde(default = "default_instance_var")]
    pub instance_var: String,
    /// Start workers in the real path of `working_directory` (a `current`
    /// symlink resolved), taken at start and at each reload / safe-reload /
    /// restart of every worker. Crash restarts reuse it, so a crash after
    /// the symlink was swapped doesn't start the new release next to the old.
    #[serde(default = "yes")]
    pub pin_release: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Process,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PortStrategy {
    /// All workers bind the same port with SO_REUSEPORT.
    Shared,
    /// Worker i gets PORT = port + i - 1 (for platforms without reuseport).
    Offset,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Workers {
    /// A number, "max" (one per CPU) or "max-N".
    #[serde(deserialize_with = "count_or_max")]
    pub count: usize,
    pub mode: Mode,
    pub port_strategy: PortStrategy,
    /// Seconds a worker may take to start listening.
    pub ready_timeout: u64,
    /// Ready when the app calls `process.send('ready')` (PM2's `wait_ready`),
    /// not when it starts listening.
    pub wait_ready: bool,
    /// Milliseconds an app without a port must stay up to count as ready
    /// (PM2's `min_uptime`).
    pub min_uptime: u64,
    /// Start the new worker before stopping the old one in rolling restarts.
    /// Default: yes, unless the app can't share its port (see `overlap()`).
    pub overlap: Option<bool>,
    /// Hot standbys (process mode): extra workers kept started but not
    /// listening; when a worker dies one takes its slot within milliseconds.
    /// Each costs about one worker's memory. 0 = none.
    pub standby: usize,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Restart {
    pub enabled: bool,
    pub max_restarts: u32,
    /// Seconds.
    pub restart_window: u64,
    /// Milliseconds.
    pub backoff_initial: u64,
    /// Milliseconds.
    pub backoff_max: u64,
    /// Seconds after which a FAILED worker is tried again (like Kubernetes'
    /// CrashLoopBackOff cap). 0 = stay FAILED until `warden restart`.
    pub failed_cooldown: u64,
    /// Cron schedule (5 fields, server local time) for a rolling restart,
    /// e.g. "0 3 * * *" (PM2's `cron_restart`).
    pub schedule: Option<String>,
    /// Exit codes that mean "done, don't restart" (PM2's `stop_exit_codes`).
    pub stop_exit_codes: Vec<i32>,
    /// When Warden's supervisor of this app crashes, its workers keep
    /// serving: a small keeper process holds them and hands them to the
    /// supervisor it starts again (`crate::keeper`). Off: they stop with it.
    pub keep_workers_on_crash: bool,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Shutdown {
    /// Seconds to wait for workers to exit before SIGKILL.
    pub grace_period: u64,
    /// Milliseconds the shim keeps answering (with `Connection: close`)
    /// after closing its listener. 0 disables the shim's SIGTERM handling.
    pub drain_ms: u64,
    /// Signal that asks a worker to stop: SIGTERM, or SIGINT for apps written
    /// for PM2 (its default). SIGKILL follows after `grace_period`.
    pub signal: String,
    /// Seconds (fractions allowed) a draining worker lets WebSockets and SSE
    /// streams end by themselves; then the shim closes WebSockets with 1001
    /// (Going Away) and ends SSE responses cleanly, so clients reconnect to
    /// the new workers. Must be below `grace_period`. 0 = leave them alone
    /// (they hold the worker until `grace_period`). Default: 2, or half of
    /// `grace_period` when that is shorter (`long_lived_timeout()`).
    pub long_lived_timeout: Option<f64>,
}

/// `long_lived_timeout` when it isn't set.
pub const DEFAULT_LONG_LIVED_TIMEOUT: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum OnHealthFailure {
    /// Only log and export the metric.
    Log,
    /// Gracefully replace the failing worker (new one ready first).
    Replace,
    /// App-level check failing: rolling reload of every worker.
    Reload,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    pub enabled: bool,
    /// Path checked on each worker's private socket (shim); sets both
    /// `live_path` and `ready_path`. Default: the path of `url`.
    pub path: Option<String>,
    /// Liveness: "is this process OK" — no dependency checks. Periodic checks
    /// on it drive replacement of a failing worker.
    pub live_path: Option<String>,
    /// Readiness: "can it serve" — may check dependencies. New workers must
    /// pass it in rollout gates.
    pub ready_path: Option<String>,
    /// Optional app-level check through the shared port (any worker may answer).
    pub url: String,
    /// Seconds.
    pub interval: u64,
    /// Seconds.
    pub timeout: u64,
    pub failure_threshold: u32,
    /// Seconds after a worker is ready before periodic checks start.
    pub initial_delay: u64,
    /// Fraction of workers failing at the same time that means "a dependency
    /// is down, not the workers": replacements are held. 1.0 disables.
    pub outage_threshold: f64,
    pub on_failure: OnHealthFailure,
}

/// Gates every replacement worker must pass before the one it replaces is
/// drained (reload, safe-reload, restart N, health/memory/lifetime recycling).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Reload {
    /// Consecutive health passes on the new worker's private socket. 0 = listening is enough.
    pub health_passes: u32,
    /// Milliseconds between gate checks.
    pub health_interval_ms: u64,
    /// Shell command run against each new worker (env: WARDEN_WORKER_ID,
    /// WARDEN_WORKER_PID, WARDEN_WORKER_SOCKET, PORT). Exit 0 = pass.
    pub verify_command: Option<String>,
    /// Seconds a new worker must stay up and healthy before it takes over.
    pub min_ready: u64,
    /// safe-reload: seconds the canary runs next to the worker it replaces.
    pub canary_soak: u64,
    /// safe-reload: seconds to wait between workers.
    pub pause: u64,
    /// Shell command run before anything is touched (reload, safe-reload). Exit 0 = go.
    pub preflight: Option<String>,
    /// Seconds allowed for one worker's gates (ready, checks, verify, soak).
    pub timeout: u64,
    /// New workers started at once in a rolling replacement, each next to
    /// the one it replaces: 1 (one at a time), N, or "all". Each passes the
    /// gates; then the N old ones drain together. Runs up to N extra workers
    /// for a few seconds.
    #[serde(deserialize_with = "surge_count")]
    pub surge: Surge,
    /// Old workers that may still be draining (closing WebSockets and SSE
    /// streams, finishing requests in flight) while the rollout replaces the
    /// next ones. A drain starts once the replacement passed its gates, so
    /// capacity never drops; each draining worker holds its memory until it
    /// exits, so a rollout runs at most max(max_draining, surge) processes
    /// beyond the worker count. 1: each old worker exits before the next
    /// replacement starts.
    pub max_draining: usize,
}

/// `reload.max_draining` without a value: a 4-worker rolling restart with
/// long-lived connections overlaps every drain (about one
/// `long_lived_timeout` in all instead of one per worker).
pub const DEFAULT_MAX_DRAINING: usize = 4;

/// `[reload] surge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surge {
    Count(usize),
    All,
}

impl Surge {
    /// Workers per batch for a rollout of `total` workers.
    pub fn batch(self, total: usize) -> usize {
        match self {
            Surge::Count(n) => n.clamp(1, total.max(1)),
            Surge::All => total.max(1),
        }
    }
    pub fn is_one(self) -> bool {
        self == Surge::Count(1)
    }
}

impl std::fmt::Display for Surge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Surge::Count(n) => write!(f, "{n}"),
            Surge::All => f.write_str("all"),
        }
    }
}

impl serde::Serialize for Surge {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Surge::Count(n) => s.serialize_u64(*n as u64),
            Surge::All => s.serialize_str("all"),
        }
    }
}

/// `surge = 2` or `surge = "all"`.
fn surge_count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Surge, D::Error> {
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum S {
        Num(i64),
        Text(String),
    }
    match S::deserialize(d)? {
        S::Num(n) if (1..=1024).contains(&n) => Ok(Surge::Count(n as usize)),
        S::Num(n) => Err(D::Error::custom(format!("reload.surge = {n}: must be between 1 and 1024, or \"all\""))),
        S::Text(t) if t.trim() == "all" => Ok(Surge::All),
        S::Text(t) => Err(D::Error::custom(format!("reload.surge = {t:?}: expected a number or \"all\""))),
    }
}

/// Liveness: the shim sends a heartbeat from each worker's event loop. No
/// heartbeat for `timeout` seconds = hung worker, killed and restarted.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Watchdog {
    /// Seconds. 0 disables the watchdog.
    pub timeout: u64,
    /// Seconds a worker that listened on a TCP port may go without any while
    /// it runs before it is restarted (0 = never): alive, but serving
    /// nothing (a dev server whose app crashed and that waits for a change, a
    /// server closed by an error the app caught). Read from the kernel's
    /// socket tables, never from inside the process; a worker that never
    /// listened is not watched.
    pub port_lost: u64,
    /// Seconds (fractions ok): warn when a worker's event-loop delay (p99,
    /// from its heartbeats) stays at or above this for LOOP_WARN_AFTER
    /// heartbeats in a row. 0 = never.
    pub loop_delay_warn: f64,
}

/// Heartbeats (about seconds) in a row the event-loop delay must stay high
/// before `loop_delay_warn` warns: a single slow second is not news.
pub const LOOP_WARN_AFTER: u32 = 10;

/// Graceful recycling (zero downtime: replacement first).
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// MB of RSS per worker process (worker mode: the host process). 0 = off.
    pub max_memory: u64,
    /// Seconds; each worker is recycled after this, ±10% jitter. 0 = off.
    pub max_lifetime: u64,
}

/// Opt-in file watching (PM2's `watch`): Warden looks at the files of the
/// app on a timer, and once they have been quiet for `debounce_ms` after a
/// change it starts a gated rolling restart, like `warden restart`.
/// docs/watch.md has the details.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Watch {
    /// Watch the files at all (`warden start --watch`). Default false: a
    /// production app is replaced on deploy with `warden reload`.
    pub enabled: bool,
    /// Directories or files to watch, relative to `[app] working_directory`
    /// (else the directory Warden was started in); absolute paths are fine.
    pub paths: Vec<String>,
    /// Names, `*`/`?`/`**` globs and paths that are never watched (a
    /// directory that matches is skipped with everything in it). A pattern
    /// without a `/` matches a file or directory name at any depth; one with
    /// a `/` matches the path from the working directory; `/abs/path` is an
    /// absolute path.
    pub ignore: Vec<String>,
    /// Milliseconds the files must stay unchanged before the restart starts
    /// (a build writes many files: this waits for it to finish).
    pub debounce_ms: u64,
    /// Milliseconds between looks at the files (the scan runs on its own
    /// thread; a big tree is looked at less often, see docs/watch.md).
    pub interval_ms: u64,
    /// Most files and directories looked at; the rest are not watched (and
    /// one warning says so).
    pub max_files: usize,
}

/// `watch.ignore` when it isn't set: version control, dependencies, logs,
/// editor droppings. Setting `ignore` replaces this list.
pub const DEFAULT_WATCH_IGNORE: [&str; 11] =
    ["node_modules", ".git", ".hg", ".svn", "*.log", "*.pid", "*.sock", "*.swp", "*.swx", "*~", ".DS_Store"];

impl Default for Watch {
    fn default() -> Self {
        Self {
            enabled: false,
            paths: vec![".".into()],
            ignore: DEFAULT_WATCH_IGNORE.iter().map(|s| s.to_string()).collect(),
            debounce_ms: 500,
            interval_ms: 1000,
            max_files: 10_000,
        }
    }
}

impl Watch {
    /// The limits `check_bounds` and `validate` enforce.
    pub const MAX_PATHS: usize = 64;
    pub const MAX_IGNORE: usize = 256;
    pub const MAX_PATTERN_LEN: usize = 512;
    pub const MAX_FILES: usize = 100_000;
    pub const MAX_DEBOUNCE_MS: u64 = 600_000;
    pub const MIN_INTERVAL_MS: u64 = 100;
    pub const MAX_INTERVAL_MS: u64 = 3_600_000;

    /// What is wrong with these settings, or Ok. Checked even when the
    /// section is off, so a typo does not wait for the day it is enabled.
    pub fn check(&self) -> Result<(), String> {
        if self.paths.is_empty() {
            return Err("watch.paths must name at least one directory or file (default [\".\"])".into());
        }
        if self.paths.len() > Self::MAX_PATHS {
            return Err(format!("watch.paths has {} entries (at most {})", self.paths.len(), Self::MAX_PATHS));
        }
        for p in &self.paths {
            if p.is_empty() || p.contains('\0') || p.len() > 4096 {
                return Err(format!("watch.paths: {p:?} is not a path"));
            }
        }
        if self.ignore.len() > Self::MAX_IGNORE {
            return Err(format!("watch.ignore has {} entries (at most {})", self.ignore.len(), Self::MAX_IGNORE));
        }
        for pat in &self.ignore {
            crate::watch::Glob::new(pat).map_err(|e| format!("watch.ignore: {pat:?}: {e}"))?;
        }
        if self.debounce_ms > Self::MAX_DEBOUNCE_MS {
            return Err(format!(
                "watch.debounce_ms = {} is out of range (maximum {})",
                self.debounce_ms,
                Self::MAX_DEBOUNCE_MS
            ));
        }
        if !(Self::MIN_INTERVAL_MS..=Self::MAX_INTERVAL_MS).contains(&self.interval_ms) {
            return Err(format!(
                "watch.interval_ms = {} must be between {} and {}",
                self.interval_ms,
                Self::MIN_INTERVAL_MS,
                Self::MAX_INTERVAL_MS
            ));
        }
        if self.max_files == 0 || self.max_files > Self::MAX_FILES {
            return Err(format!("watch.max_files = {} must be between 1 and {}", self.max_files, Self::MAX_FILES));
        }
        Ok(())
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_millis(self.debounce_ms.min(Self::MAX_DEBOUNCE_MS))
    }

    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.clamp(Self::MIN_INTERVAL_MS, Self::MAX_INTERVAL_MS))
    }
}

/// On the wire too (`log-level`), so it lives in the protocol crate.
pub use warden_protocol::Level;

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Logging {
    pub level: Level,
    /// `true`, `false`, or omitted = auto (off under journald).
    pub timestamps: Option<bool>,
    /// "capture" (default): worker lines go through Warden with a `worker=N`
    /// prefix and into `warden logs`. "inherit": workers write straight to
    /// Warden's stdout — for very chatty apps. "direct": worker bytes go
    /// unchanged to `out_file`/`err_file`, moved by the kernel (splice),
    /// still rotated — for apps that log heavily and only need the files.
    pub worker_output: WorkerOutput,
    /// Also write the log to this file, rotated by size (for people who
    /// `tail -f` log files, as with PM2). Default: stdout only (journald).
    pub file: Option<PathBuf>,
    /// How every log file (file, out_file, err_file) is rotated.
    pub rotate: Rotate,
    /// Worker stdout as the app wrote it (PM2's `out_file`).
    pub out_file: Option<PathBuf>,
    /// Worker stderr as the app wrote it (PM2's `error_file`).
    pub err_file: Option<PathBuf>,
    /// One out/err file per worker (`out-2.log`), like PM2 without
    /// `merge_logs`. Default: one file for all workers.
    pub per_worker_files: bool,
    /// Prefix out/err lines with a timestamp (PM2's `time`).
    pub file_timestamps: bool,
    /// Most worker output lines kept per second per worker and stream;
    /// above it lines are dropped and counted, so one runaway worker can't
    /// eat the host's CPU and disk. 0 = keep everything (as PM2 does).
    pub max_lines_per_sec: u32,
}

/// Log rotation, built in (no pm2-logrotate module needed).
#[derive(Debug, Clone, Deserialize, serde::Serialize, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Rotate {
    /// Rotate when a file reaches this size: "10M", "1G", "500K" or bytes.
    /// 0 = only on `interval`.
    #[serde(deserialize_with = "size_bytes")]
    pub max_size: u64,
    /// Rotated files kept per log file; older ones are deleted.
    pub keep: u32,
    /// Also rotate on a schedule (cron, server local time), e.g.
    /// "0 0 * * *" for daily files.
    pub interval: Option<String>,
    /// gzip rotated files (`app.log.1.gz`).
    pub compress: bool,
    /// Name rotated files by time (`app.log.2026-09-30T00-00-00`) instead
    /// of by number (`app.log.1`).
    pub date_suffix: bool,
    /// Also delete rotated files older than this many days. 0 = keep `keep`.
    pub max_age_days: u64,
}

impl Default for Rotate {
    fn default() -> Self {
        Rotate { max_size: 10 << 20, keep: 5, interval: None, compress: false, date_suffix: false, max_age_days: 0 }
    }
}

/// `max_size = "10M"` or `max_size = 10485760`.
fn size_bytes<'de, D: serde::Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum S {
        Num(u64),
        Text(String),
    }
    match S::deserialize(d)? {
        S::Num(n) => Ok(n),
        S::Text(t) => parse_size(&t).map_err(D::Error::custom),
    }
}

pub fn parse_size(t: &str) -> Result<u64, String> {
    let u = t.trim().to_ascii_uppercase();
    let u = u.strip_suffix('B').unwrap_or(&u);
    let (num, mult) = match u.chars().last() {
        Some('K') => (&u[..u.len() - 1], 1u64 << 10),
        Some('M') => (&u[..u.len() - 1], 1 << 20),
        Some('G') => (&u[..u.len() - 1], 1 << 30),
        _ => (u, 1),
    };
    let n: f64 = num.trim().parse().map_err(|_| format!("{t:?} is not a size like \"10M\" or \"1G\""))?;
    if !(0.0..=1e15).contains(&n) {
        return Err(format!("{t:?} is out of range"));
    }
    Ok((n * mult as f64) as u64)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkerOutput {
    Capture,
    Inherit,
    /// Byte for byte into `out_file` / `err_file`: no prefix, no ring, no
    /// line budget, no copy to stdout; the kernel moves the bytes.
    Direct,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Metrics {
    /// e.g. "127.0.0.1:9464". Prometheus text format at /metrics.
    pub listen: Option<String>,
    /// Count the responses the workers send, by status, for `warden list`,
    /// the GUI and the metrics: Warden's static server counts its own, and
    /// the shim, under Node, subscribes to `node:diagnostics_channel` (no
    /// code of the app or of `http` is wrapped). `false` turns both off.
    pub requests: bool,
}

impl Default for Metrics {
    fn default() -> Self {
        Metrics { listen: None, requests: true }
    }
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Control {
    /// Unix socket for the CLI. Default: `<runtime dir>/<name>/control.sock`,
    /// where the runtime dir is /run/warden for root (systemd's
    /// RuntimeDirectory=warden/%i), else $XDG_RUNTIME_DIR/warden or
    /// /tmp/warden-<uid>; $WARDEN_RUNTIME_DIR overrides it.
    pub socket: Option<PathBuf>,
}

fn default_command() -> String {
    "bun".into()
}

fn default_instance_var() -> String {
    "NODE_APP_INSTANCE".into()
}

pub fn cpu_count() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// A hint for an app in worker mode where its threads cost speed (`warden doctor`, the GUI, a
/// warning at start): on macOS the threads share the port but the kernel gives one of them most
/// connections, and before Bun 1.4 they ran 20-50 % slower than processes (NestJS, measured
/// 2026-10-05; on 1.4.2 they are level, with 14-22 % less memory). `bun`: the runtime's
/// version, when known.
pub fn worker_mode_hint(cfg: &Config, macos: bool, bun: Option<(u32, u32)>) -> Option<String> {
    if cfg.workers.mode != Mode::Worker {
        return None;
    }
    if macos {
        return Some(
            "worker mode on macOS: the threads share the port, but macOS gives one of them most connections; \
             `mode = \"process\"` spreads them over the workers"
                .into(),
        );
    }
    match bun {
        Some((major, minor)) if (major, minor) < (1, 4) => Some(format!(
            "worker mode on Bun {major}.{minor}: before Bun 1.4 its threads run 20-50 % slower than processes; \
             upgrade Bun, or use `mode = \"process\"`"
        )),
        _ => None,
    }
}

/// `<command> --version` as (major, minor), when it answers.
pub fn runtime_version(command: &str) -> Option<(u32, u32)> {
    let out = std::process::Command::new(command).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    crate::doctor::major_minor(&String::from_utf8_lossy(&out.stdout))
}

/// A hint for an app that runs one worker on a Linux host with more cores: there the workers
/// share the port and the kernel spreads the connections over them, so throughput grows with
/// them, and one worker leaves the other cores idle under load. Only a
/// hint (`warden doctor`, the GUI): an app that keeps state in memory needs its one worker.
/// `count`: the workers it runs now (after `warden scale`); `linux` and `cpus`: the host's.
pub fn more_workers_hint(cfg: &Config, count: usize, linux: bool, cpus: usize) -> Option<String> {
    // Without a shared port (none, or one per worker) more workers are not one faster app.
    if !linux || cpus < 2 || count != 1 || cfg.app.port.is_none() || cfg.workers.port_strategy != PortStrategy::Shared {
        return None;
    }
    Some(format!(
        "1 worker on a host with {cpus} cores: under load the other cores stay idle; \
         `count = \"max\"` under [workers] runs one per core (keep 1 if the app holds state in memory)"
    ))
}

/// Whether the config sets `[workers] count`.
fn workers_count_set(text: &str) -> bool {
    toml::from_str::<toml::Table>(text).ok().is_some_and(|t| t.get("workers").and_then(|w| w.get("count")).is_some())
}

/// `count = 4`, `count = "max"`, `count = "max-1"`.
fn count_or_max<'de, D: serde::Deserializer<'de>>(d: D) -> Result<usize, D::Error> {
    use serde::de::Error;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum N {
        Num(i64),
        Text(String),
    }
    match N::deserialize(d)? {
        N::Num(n) if n >= 0 => Ok(n as usize),
        N::Num(n) => Err(D::Error::custom(format!("workers.count = {n}: must be positive, \"max\" or \"max-N\""))),
        N::Text(t) => {
            let t = t.trim();
            let cpus = cpu_count();
            if t == "max" {
                return Ok(cpus);
            }
            match t.strip_prefix("max-").and_then(|n| n.trim().parse::<usize>().ok()) {
                Some(n) => Ok(cpus.saturating_sub(n).max(1)),
                None => {
                    Err(D::Error::custom(format!("workers.count = {t:?}: expected a number, \"max\" or \"max-N\"")))
                }
            }
        }
    }
}

impl Default for Workers {
    fn default() -> Self {
        Self {
            count: 1,
            mode: Mode::Process,
            port_strategy: PortStrategy::Shared,
            ready_timeout: 30,
            wait_ready: false,
            min_uptime: 1000,
            overlap: None,
            standby: 0,
        }
    }
}

impl Default for Restart {
    fn default() -> Self {
        Self {
            enabled: true,
            max_restarts: 10,
            restart_window: 60,
            backoff_initial: 100,
            backoff_max: 10_000,
            failed_cooldown: 300,
            schedule: None,
            stop_exit_codes: Vec::new(),
            keep_workers_on_crash: true,
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self { grace_period: 30, drain_ms: 500, signal: "SIGTERM".into(), long_lived_timeout: None }
    }
}

impl Default for Health {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            live_path: None,
            ready_path: None,
            url: String::new(),
            interval: 5,
            timeout: 2,
            failure_threshold: 3,
            initial_delay: 10,
            outage_threshold: 0.5,
            on_failure: OnHealthFailure::Replace,
        }
    }
}

impl Default for Reload {
    fn default() -> Self {
        Self {
            health_passes: 3,
            health_interval_ms: 500,
            verify_command: None,
            min_ready: 0,
            canary_soak: 30,
            pause: 0,
            preflight: None,
            timeout: 120,
            surge: Surge::Count(1),
            max_draining: DEFAULT_MAX_DRAINING,
        }
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self { timeout: 60, port_lost: 10, loop_delay_warn: 0.5 }
    }
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            level: Level::Info,
            timestamps: None,
            worker_output: WorkerOutput::Capture,
            file: None,
            rotate: Rotate::default(),
            out_file: None,
            err_file: None,
            per_worker_files: false,
            file_timestamps: false,
            max_lines_per_sec: 10_000,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
        let mut cfg = Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        // Relative working directories are relative to the config file.
        if let Some(wd) = &cfg.app.working_directory {
            if wd.is_relative() {
                let base = path.parent().unwrap_or(Path::new("."));
                cfg.app.working_directory = Some(base.join(wd));
            }
        }
        if let Some(f) = &cfg.app.env_file {
            let file = if f.is_relative() { path.parent().unwrap_or(Path::new(".")).join(f) } else { f.clone() };
            let text = std::fs::read_to_string(&file)
                .map_err(|e| format!("{}: app.env_file {}: {e}", path.display(), file.display()))?;
            cfg.app.env_from_file = parse_env_file(&text)
                .map_err(|e| format!("{}: app.env_file {}: {e}", path.display(), file.display()))?;
            cfg.app.env_file = Some(file);
        }
        // A relative static root, and a relative compress_dir: from the working
        // directory, else the config file.
        if let Some(st) = &mut cfg.static_files {
            let base = cfg
                .app
                .working_directory
                .clone()
                .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")));
            if st.root.is_relative() {
                st.root = base.join(&st.root);
            }
            if let Some(dir) = st.compress_dir.as_mut().filter(|d| d.is_relative()) {
                *dir = base.join(&*dir);
            }
        }
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let mut cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        // The router: one worker per core unless the config says otherwise
        // (Linux, where the kernel spreads connections over them).
        if cfg.route.is_some() && cfg!(target_os = "linux") && !workers_count_set(text) {
            cfg.workers.count = cpu_count();
        }
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        self.check_bounds()?;
        let a = &self.app;
        if a.name.is_empty() || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("app.name must be non-empty and contain only [A-Za-z0-9._-]".into());
        }
        // `.` and `..` name directories: the config would be `..toml` and the control socket
        // `<runtime dir>/../control.sock`.
        if a.name.chars().all(|c| c == '.') {
            return Err("app.name cannot be only dots (`.` and `..` are directories)".into());
        }
        if a.command.is_empty() {
            return Err("app.command must not be empty".into());
        }
        if let Some(ns) = &a.namespace {
            if ns.is_empty() || !ns.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
                return Err("app.namespace must be non-empty and contain only [A-Za-z0-9._-]".into());
            }
            if ns == "all" {
                return Err("app.namespace cannot be \"all\" (that targets every app)".into());
            }
        }
        if a.name == "all" {
            return Err("app.name cannot be \"all\" (that targets every app)".into());
        }
        if let Some(ip) = a.address {
            if a.port.is_none() {
                return Err("app.address needs app.port (the port to listen on at that address)".into());
            }
            if ip.is_unspecified() || ip.is_multicast() {
                return Err(format!("app.address {ip} must be one address of this server (not {ip})"));
            }
            if self.route.is_some() {
                return Err("app.address does not apply to [route]: set route.host".into());
            }
            if self.static_files.is_some() {
                return Err("app.address does not apply to [static]: set static.host".into());
            }
        }
        if let Some(r) = &self.route {
            r.check()?;
            if self.app.port.is_none() {
                return Err("[route] needs app.port (the port to listen on, usually 443)".into());
            }
            if self.static_files.is_some() {
                return Err("[route] and [static] are two kinds of app: an app has one".into());
            }
            if self.workers.mode == Mode::Worker {
                return Err("[route] runs Warden's own router: use workers.mode = \"process\"".into());
            }
            if self.watch.enabled {
                return Err("[watch] does not apply to [route]: the router runs Warden's own code".into());
            }
            if let Some(RouteTarget::Port(p)) =
                r.hosts.values().find(|t| **t == RouteTarget::Port(self.app.port.unwrap_or(0)))
            {
                return Err(format!("route.hosts: port {p} is the router's own port"));
            }
        }
        if let Some(x) = &self.expose {
            x.check()?;
            if self.app.port.is_none() {
                return Err("[expose] needs app.port (the port nginx sends requests to)".into());
            }
        }
        if let Some(st) = &self.static_files {
            if self.app.port.is_none() {
                return Err("[static] needs app.port (the port to serve on)".into());
            }
            if self.workers.mode == Mode::Worker {
                return Err("[static] runs Warden's own file server: use workers.mode = \"process\"".into());
            }
            if st.root.as_os_str().is_empty() {
                return Err("static.root must name the directory to serve".into());
            }
            if let Some(a) = &st.basic_auth {
                if !a.contains(':') {
                    return Err("static.basic_auth must be \"user:password\"".into());
                }
            }
            for (name, value, max, unit) in [
                ("static.cache_size", st.cache_size, 4 << 30, "4G"),
                ("static.cache_max_file", st.cache_max_file, 16 << 20, "16M"),
                ("static.cache_valid_ms", st.cache_valid_ms, 3_600_000, "3600000"),
            ] {
                if value > max {
                    return Err(format!("{name} = {value} is out of range (maximum {unit})"));
                }
            }
            if st.compress_jobs > 256 {
                return Err(format!(
                    "static.compress_jobs = {} is out of range (maximum 256; 0 is one per core)",
                    st.compress_jobs
                ));
            }
            if st.compress_max_file > 64 << 20 {
                return Err(format!(
                    "static.compress_max_file = {} is out of range (maximum 64M)",
                    st.compress_max_file
                ));
            }
            if st.compress_min_file > st.compress_max_file {
                return Err("static.compress_min_file must not be above static.compress_max_file".into());
            }
            if st.compress_dir_size < st.compress_max_file.max(1 << 20) {
                return Err("static.compress_dir_size must hold at least one file of compress_max_file (and 1M)".into());
            }
            if st.compress_dir.as_ref().is_some_and(|d| d.as_os_str().is_empty()) {
                return Err("static.compress_dir must name a directory (unset it for the default)".into());
            }
            if let Some(n) = st.html_max_age.filter(|n| *n > MAX_HTML_MAX_AGE) {
                return Err(format!(
                    "static.html_max_age = {n} is out of range (maximum {MAX_HTML_MAX_AGE}, one year; \
                     unset or 0 revalidates on every page load)"
                ));
            }
        }
        if self.workers.wait_ready && !self.shim_enabled() {
            return Err("workers.wait_ready needs Warden's shim, which loads into bun and node commands \
                        (process.send('ready') has nowhere to go otherwise); use a port or min_uptime instead"
                .into());
        }
        if crate::signals::parse(&self.shutdown.signal).is_none() {
            return Err(format!(
                "shutdown.signal = {:?} is not a signal name (use SIGTERM or SIGINT)",
                self.shutdown.signal
            ));
        }
        if let Some(t) = self.shutdown.long_lived_timeout {
            let grace = self.shutdown.grace_period;
            if !t.is_finite() || t < 0.0 {
                return Err(format!("shutdown.long_lived_timeout = {t} must be a number of seconds >= 0"));
            }
            if t > 0.0 && t >= grace as f64 {
                return Err(format!(
                    "shutdown.long_lived_timeout = {t} must be below shutdown.grace_period = {grace}: \
                     WebSockets and SSE streams are closed at long_lived_timeout, and the worker needs the \
                     rest of the grace period to finish (or it is SIGKILLed and clients see resets). \
                     Lower long_lived_timeout or raise grace_period"
                ));
            }
        }
        let warn = self.watchdog.loop_delay_warn;
        if !warn.is_finite() || !(0.0..=3600.0).contains(&warn) {
            return Err(format!("watchdog.loop_delay_warn = {warn} must be a number of seconds from 0 (off) to 3600"));
        }
        if let Some(expr) = &self.restart.schedule {
            crate::schedule::Cron::parse(expr).map_err(|e| format!("restart.schedule = {expr:?}: {e}"))?;
        }
        self.watch.check()?;
        if self.watch.enabled && self.static_files.is_some() {
            return Err("[watch] does not apply to a [static] site: Warden's file server reads the files from disk \
                        (a changed file is served after [static] cache_valid_ms), so there is nothing to restart"
                .into());
        }
        if (self.logging.out_file.is_some() || self.logging.err_file.is_some())
            && self.logging.worker_output == WorkerOutput::Inherit
        {
            return Err("logging.out_file / err_file need worker_output = \"capture\" or \"direct\" (with \
                        \"inherit\", worker output bypasses Warden)"
                .into());
        }
        if self.logging.worker_output == WorkerOutput::Direct {
            self.check_direct_output()?;
        }
        if let Some(expr) = &self.logging.rotate.interval {
            crate::schedule::Cron::parse(expr).map_err(|e| format!("logging.rotate.interval = {expr:?}: {e}"))?;
        }
        if self.logging.rotate.max_size != 0 && self.logging.rotate.max_size < 4096 {
            return Err("logging.rotate.max_size must be at least 4K (or 0 to rotate only on the interval)".into());
        }
        if self.workers.count == 0 || self.workers.count > 1024 {
            return Err("workers.count must be between 1 and 1024".into());
        }
        if self.workers.ready_timeout == 0 {
            return Err("workers.ready_timeout must be > 0".into());
        }
        match self.workers.mode {
            Mode::Process => {
                if a.entry.is_some() && a.args.is_empty() {
                    return Err("app.entry is for worker mode; in process mode use app.args".into());
                }
            }
            Mode::Worker => {
                if a.entry.is_none() {
                    return Err("worker mode needs app.entry (the module each Worker imports)".into());
                }
                if !a.args.is_empty() {
                    return Err("worker mode ignores app.args; set app.entry instead".into());
                }
                if self.workers.port_strategy == PortStrategy::Offset {
                    return Err("worker mode requires workers.port_strategy = \"shared\"".into());
                }
                if a.shim == Some(false) {
                    return Err("worker mode requires the shim (app.shim cannot be false)".into());
                }
            }
        }
        if self.workers.standby > 0 {
            self.check_standby()?;
        }
        if self.workers.port_strategy == PortStrategy::Offset {
            match a.port {
                None => return Err("workers.port_strategy = \"offset\" needs app.port".into()),
                Some(p) if p as usize + self.workers.count - 1 > u16::MAX as usize => {
                    return Err("app.port + workers.count exceeds 65535".into());
                }
                _ => {}
            }
        }
        let r = &self.restart;
        if r.backoff_initial == 0 || r.backoff_max < r.backoff_initial {
            return Err("restart.backoff_initial must be > 0 and <= restart.backoff_max".into());
        }
        if r.restart_window == 0 {
            return Err("restart.restart_window must be > 0".into());
        }
        let h = &self.health;
        if !h.url.is_empty() {
            crate::health::parse_url(&h.url).map_err(|e| format!("health.url: {e}"))?;
        }
        for (name, p) in
            [("health.path", &h.path), ("health.live_path", &h.live_path), ("health.ready_path", &h.ready_path)]
        {
            if let Some(p) = p {
                if !p.starts_with('/') || p.contains(char::is_whitespace) {
                    return Err(format!("{name} must start with '/' and contain no spaces"));
                }
            }
        }
        let per_worker = h.path.is_some() || h.live_path.is_some() || h.ready_path.is_some();
        if h.enabled && h.url.is_empty() && !per_worker {
            return Err("health.enabled needs health.path (per-worker) and/or health.url (app-level)".into());
        }
        if per_worker && h.url.is_empty() && !self.health_sockets() {
            return Err("health paths are checked through each worker's private socket, which Warden's shim \
                        (bun and node commands) or its static server opens; for other programs set health.url"
                .into());
        }
        if self.any_worker_path() && self.health_sockets() {
            // The shim names each worker's socket <dir>/<name>.h<instance>-<worker>.sock
            // and Unix socket paths are limited to ~104 bytes.
            let dir = self.socket_path().parent().map(|d| d.as_os_str().len()).unwrap_or(0);
            let longest = dir + 1 + self.app.name.len() + ".h9999999999-9999.sock".len();
            if longest > 100 {
                return Err(format!(
                    "per-worker health sockets would need paths of up to {longest} bytes (limit 100): use a shorter [control] socket directory"
                ));
            }
        }
        if h.interval == 0 || h.timeout == 0 || h.failure_threshold == 0 {
            return Err("health.interval, health.timeout and health.failure_threshold must be > 0".into());
        }
        let rl = &self.reload;
        if rl.health_interval_ms < 50 || rl.timeout == 0 {
            return Err("reload.health_interval_ms must be >= 50 and reload.timeout > 0".into());
        }
        if !(1..=1024).contains(&rl.max_draining) {
            return Err(format!(
                "reload.max_draining = {} must be between 1 and 1024 (1: each old worker exits before the next \
                 replacement starts)",
                rl.max_draining
            ));
        }
        if !rl.surge.is_one() && !self.overlap() {
            let s = rl.surge;
            return Err(if self.workers.port_strategy == PortStrategy::Offset {
                format!(
                    "reload.surge = {s} starts new workers next to the ones they replace, but with \
                     workers.port_strategy = \"offset\" each worker owns its port, so a worker and its \
                     replacement can't run at the same time. Fix: remove reload.surge (one worker at a \
                     time, stopped then started), or use port_strategy = \"shared\""
                )
            } else {
                format!(
                    "reload.surge = {s} starts new workers next to the ones they replace, but this app's \
                     workers can't overlap (workers.overlap = false, or a port without Warden's shim, so \
                     no SO_REUSEPORT). Fix: remove reload.surge, or let the workers share the port"
                )
            });
        }
        for (name, cmd) in [("reload.verify_command", &rl.verify_command), ("reload.preflight", &rl.preflight)] {
            if cmd.as_deref().is_some_and(|c| c.trim().is_empty()) {
                return Err(format!("{name} must not be empty (omit it instead)"));
            }
        }
        if let Some(l) = &self.metrics.listen {
            l.parse::<std::net::SocketAddr>().map_err(|e| format!("metrics.listen: {e}"))?;
        }
        for k in a.env.keys() {
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                return Err(format!("app.env: invalid variable name {k:?}"));
            }
        }
        Ok(())
    }

    /// `worker_output = "direct"`: the files Warden hands the bytes to.
    /// stdout needs `out_file`; stderr goes to `err_file`, or with it unset
    /// (or the same path) into `out_file` too, like `>out.log 2>&1`.
    /// Several worker processes can't share a file: nothing splits their
    /// writes into lines, so a partial line of one could meet another's.
    fn check_direct_output(&self) -> Result<(), String> {
        let l = &self.logging;
        if l.out_file.is_none() {
            return Err(if l.err_file.is_some() {
                "logging.worker_output = \"direct\" needs logging.out_file for stdout (stderr goes to err_file, \
                 or into out_file when err_file is unset)"
                    .into()
            } else {
                "logging.worker_output = \"direct\" writes worker output straight to files: set logging.out_file \
                 (and err_file for a separate stderr file), or use worker_output = \"capture\""
                    .into()
            });
        }
        if self.workers.mode == Mode::Process && self.workers.count > 1 && !l.per_worker_files {
            return Err(format!(
                "logging.worker_output = \"direct\" with workers.count = {} needs logging.per_worker_files = true: \
                 each worker writes its own bytes unparsed, so in one shared file a partial line of one worker \
                 could meet another's. Fix: add per_worker_files = true under [logging] (files out-1.log, \
                 out-2.log…), or use worker_output = \"capture\"",
                self.workers.count
            ));
        }
        if l.file_timestamps {
            return Err("logging.file_timestamps needs worker_output = \"capture\": \"direct\" writes the app's \
                        bytes unchanged (let the app's logger add timestamps)"
                .into());
        }
        Ok(())
    }

    /// `[workers] standby`: the shim defers the app's listen on its port until
    /// a standby is promoted into a slot (process mode only).
    fn check_standby(&self) -> Result<(), String> {
        let n = self.workers.standby;
        if self.workers.mode == Mode::Worker {
            return Err(format!(
                "workers.standby = {n} is for process mode: in worker mode the unit is the whole host process. \
                 Fix: set standby = 0, or workers.mode = \"process\""
            ));
        }
        if self.builtin_server() {
            return Err(
                "workers.standby: [static] and [route] run Warden's own server, which starts in milliseconds; \
                        remove standby"
                    .into(),
            );
        }
        if !self.shim_enabled() {
            return Err("workers.standby needs Warden's shim (bun and node commands, app.shim not false): it is \
                        what holds a standby's listen back until it is promoted"
                .into());
        }
        if self.app.port.is_none() {
            return Err("workers.standby needs app.port: a standby defers its listen on that port until it is \
                        promoted"
                .into());
        }
        if self.workers.port_strategy != PortStrategy::Shared {
            return Err("workers.standby needs workers.port_strategy = \"shared\": a promoted standby joins the \
                        shared port"
                .into());
        }
        if self.logging.worker_output == WorkerOutput::Direct {
            return Err("workers.standby does not work with logging.worker_output = \"direct\": a standby's output \
                        file could not follow it into the slot it takes. Fix: worker_output = \"capture\" (the \
                        default)"
                .into());
        }
        // No socket path to check: a Bun standby's stand-in server uses the
        // private health socket (checked above) or an ephemeral TCP port.
        Ok(())
    }

    /// Upper bounds on every number, so no value can overflow time arithmetic
    /// (`Instant + Duration`) or counters at runtime. Generous: they only
    /// exclude values that are certainly mistakes.
    fn check_bounds(&self) -> Result<(), String> {
        const HOUR: u64 = 3600;
        const DAY: u64 = 86_400;
        let checks: [(&str, u64, u64); 27] = [
            ("workers.ready_timeout", self.workers.ready_timeout, HOUR),
            ("workers.standby", self.workers.standby as u64, 1024),
            ("restart.max_restarts", self.restart.max_restarts as u64, 10_000),
            ("restart.restart_window", self.restart.restart_window, 30 * DAY),
            ("restart.backoff_initial", self.restart.backoff_initial, HOUR * 1000),
            ("restart.backoff_max", self.restart.backoff_max, HOUR * 1000),
            ("restart.failed_cooldown", self.restart.failed_cooldown, 30 * DAY),
            ("shutdown.grace_period", self.shutdown.grace_period, HOUR),
            ("shutdown.drain_ms", self.shutdown.drain_ms, HOUR * 1000),
            ("health.interval", self.health.interval, HOUR),
            ("health.timeout", self.health.timeout, HOUR),
            ("health.failure_threshold", self.health.failure_threshold as u64, 1000),
            ("health.initial_delay", self.health.initial_delay, DAY),
            ("reload.health_passes", self.reload.health_passes as u64, 1000),
            ("reload.health_interval_ms", self.reload.health_interval_ms, 60_000),
            ("reload.min_ready", self.reload.min_ready, DAY),
            ("reload.canary_soak", self.reload.canary_soak, DAY),
            ("reload.pause", self.reload.pause, DAY),
            ("reload.timeout", self.reload.timeout, DAY),
            ("watchdog.timeout", self.watchdog.timeout, DAY),
            ("watchdog.port_lost", self.watchdog.port_lost, HOUR),
            ("limits.max_memory", self.limits.max_memory, 1 << 20),
            ("limits.max_lifetime", self.limits.max_lifetime, 365 * DAY),
            ("logging.rotate.max_size", self.logging.rotate.max_size, 1 << 40),
            ("workers.min_uptime", self.workers.min_uptime, HOUR * 1000),
            ("logging.rotate.keep", self.logging.rotate.keep as u64, 1000),
            ("logging.rotate.max_age_days", self.logging.rotate.max_age_days, 3650),
        ];
        for (name, value, max) in checks {
            if value > max {
                return Err(format!("{name} = {value} is out of range (maximum {max})"));
            }
        }
        if !(0.0..=1.0).contains(&self.health.outage_threshold) {
            return Err("health.outage_threshold must be between 0.0 and 1.0 (1.0 disables the guard)".into());
        }
        Ok(())
    }

    /// Workers report a private health socket (the shim, or the static server).
    pub fn health_sockets(&self) -> bool {
        self.shim_enabled() || self.builtin_server()
    }

    /// The workers run Warden's own server (`[static]` or `[route]`), not
    /// `app.command`.
    pub fn builtin_server(&self) -> bool {
        self.static_files.is_some() || self.route.is_some()
    }

    /// Whether to inject the Bun shim.
    pub fn shim_enabled(&self) -> bool {
        match self.workers.mode {
            Mode::Worker => true,
            Mode::Process => self.app.shim.unwrap_or_else(|| is_bun(&self.app.command) || is_node(&self.app.command)),
        }
    }

    /// Log files as the writer thread needs them. With `worker_output =
    /// "direct"` the out/err files are written by the workers' pumps
    /// instead (`process::Output::from_config`), not by the writer thread.
    pub fn log_files(&self) -> crate::logging::Files {
        let l = &self.logging;
        let direct = l.worker_output == WorkerOutput::Direct;
        crate::logging::Files {
            file: l.file.clone(),
            out_file: l.out_file.clone().filter(|_| !direct),
            err_file: l.err_file.clone().filter(|_| !direct),
            per_worker: l.per_worker_files,
            timestamps: l.file_timestamps,
            rotate: crate::logging::RotatePolicy::from(&l.rotate),
            direct: match crate::process::Output::from_config(l) {
                crate::process::Output::Direct(d) => Some(d),
                _ => None,
            },
        }
    }

    /// The signal that asks a worker to stop (validated at load).
    pub fn stop_signal(&self) -> i32 {
        crate::signals::parse(&self.shutdown.signal).unwrap_or(libc::SIGTERM)
    }

    /// Can a new worker run next to the old one during a rolling restart?
    /// Not when each worker owns its port (offset), or when the app binds a
    /// port without Warden's shim (no SO_REUSEPORT: the new one would fail
    /// with EADDRINUSE). Then the old worker stops first.
    pub fn overlap(&self) -> bool {
        self.no_overlap_reason().is_none()
    }

    /// Why a rolling restart stops the old worker before it starts the new
    /// one (so a version that fails to start leaves that worker down, with
    /// nothing to roll back to), or None when workers overlap.
    pub fn no_overlap_reason(&self) -> Option<&'static str> {
        match self.workers.overlap {
            Some(true) => None,
            Some(false) => Some("[workers] overlap = false"),
            None if self.workers.port_strategy == PortStrategy::Offset => {
                Some("port_strategy = \"offset\": each worker owns its port")
            }
            None if self.app.port.is_none()
                || self.shim_enabled()
                || self.builtin_server()
                || self.workers.mode == Mode::Worker =>
            {
                None
            }
            None => Some("the app binds its port without Warden's shim, so two workers cannot share it"),
        }
    }

    pub fn ready_timeout(&self) -> Duration {
        Duration::from_secs(self.workers.ready_timeout)
    }

    pub fn grace_period(&self) -> Duration {
        Duration::from_secs(self.shutdown.grace_period)
    }

    /// When a draining worker closes its WebSockets and SSE streams (zero:
    /// never). Unset: 2 s, or half the grace period when that is shorter, so
    /// a short `grace_period` doesn't make the default invalid.
    pub fn long_lived_timeout(&self) -> Duration {
        let s = &self.shutdown;
        let secs = s.long_lived_timeout.unwrap_or_else(|| DEFAULT_LONG_LIVED_TIMEOUT.min(s.grace_period as f64 / 2.0));
        // Validated finite and >= 0 at load; clamp anyway (a Duration panics on NaN).
        Duration::from_secs_f64(if secs.is_finite() { secs.clamp(0.0, 3600.0) } else { 0.0 })
    }

    /// HTTP path for per-worker checks: `health.path`, else the path of `health.url`.
    pub fn health_path(&self) -> Option<String> {
        if let Some(p) = &self.health.path {
            return Some(p.clone());
        }
        crate::health::parse_url(&self.health.url).ok().map(|t| t.path)
    }

    /// Liveness path (periodic checks, drives replacement).
    pub fn live_path(&self) -> Option<String> {
        self.health.live_path.clone().or_else(|| self.health_path())
    }

    /// Readiness path (rollout gates).
    pub fn ready_path(&self) -> Option<String> {
        self.health.ready_path.clone().or_else(|| self.health_path())
    }

    /// Any per-worker health path configured.
    pub fn any_worker_path(&self) -> bool {
        self.live_path().is_some() || self.ready_path().is_some()
    }

    /// Workers open a private health socket only when something uses it: a
    /// per-worker health path, or a verify_command (it gets the socket as
    /// WARDEN_WORKER_SOCKET). A second server per worker costs memory
    /// (measured: ~6 MB per Bun worker), so none without a reason.
    pub fn private_sockets(&self) -> bool {
        self.health_sockets() && (self.any_worker_path() || self.reload.verify_command.is_some())
    }

    pub fn socket_path(&self) -> PathBuf {
        match &self.control.socket {
            Some(p) => p.clone(),
            None => app_runtime_dir(&self.app.name).join("control.sock"),
        }
    }
}

/// The worker environment from the config: `env_file`, then `env` on top.
impl App {
    pub fn environment(&self) -> impl Iterator<Item = (&String, &String)> {
        self.env_from_file.iter().filter(|(k, _)| !self.env.contains_key(*k)).chain(self.env.iter())
    }
}

/// What an env file accepts as a variable name: letters, digits and `_`, not
/// starting with a digit (the portable shell rule).
pub fn valid_env_name(k: &str) -> bool {
    !k.is_empty()
        && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !k.starts_with(|c: char| c.is_ascii_digit())
}

/// `KEY=VALUE` lines as in systemd's EnvironmentFile and dotenv: blank lines
/// and `#` comments skipped, an optional `export `, values optionally in
/// double quotes (with \n, \t, \", \\ escapes) or single quotes (literal).
pub fn parse_env_file(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=VALUE", i + 1));
        };
        let k = k.trim();
        if !valid_env_name(k) {
            return Err(format!(
                "line {}: {k:?} is not a valid variable name (letters, digits and _, not starting with a digit); \
                 rename or remove that line",
                i + 1
            ));
        }
        let v = v.trim();
        let value = if let Some(inner) = v.strip_prefix('"') {
            let Some(inner) = inner.strip_suffix('"') else {
                return Err(format!("line {}: unterminated double quote", i + 1));
            };
            let mut s = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    s.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some(o) => s.push(o),
                    None => s.push('\\'),
                }
            }
            s
        } else if let Some(inner) = v.strip_prefix('\'') {
            inner.strip_suffix('\'').ok_or_else(|| format!("line {}: unterminated single quote", i + 1))?.to_string()
        } else {
            // Unquoted: a trailing ` # comment` is not part of the value.
            v.split(" #").next().unwrap_or("").trim_end().to_string()
        };
        out.insert(k.to_string(), value);
    }
    Ok(out)
}

/// Find the control socket even when the file no longer parses (someone is
/// mid-edit or a deploy broke it): scan for `[app] name` and `[control] socket`
/// line by line, so `warden safe-reload` can still reach Warden and report the
/// config error instead of failing on the client side.
pub fn socket_path_lenient(path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(path).ok()?;
    let (mut section, mut name, mut socket) = (String::new(), None, None);
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with('[') {
            section = l.trim_matches(|c| c == '[' || c == ']').trim().to_string();
            continue;
        }
        let Some((k, v)) = l.split_once('=') else { continue };
        let v = v.trim();
        let Some(v) = v.strip_prefix('"').and_then(|r| r.split('"').next()) else { continue };
        match (section.as_str(), k.trim()) {
            ("app", "name") => name = Some(v.to_string()),
            ("control", "socket") => socket = Some(PathBuf::from(v)),
            _ => {}
        }
    }
    socket.or_else(|| name.map(|n| app_runtime_dir(&n).join("control.sock")))
}

pub fn is_bun(command: &str) -> bool {
    Path::new(command).file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "bun" || n == "bun.exe")
}

pub fn is_node(command: &str) -> bool {
    Path::new(command).file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "node" || n == "nodejs")
}

/// Directory for the control socket and the embedded JS files.
/// Where every app's runtime directory lives (sockets, shim).
/// `$WARDEN_RUNTIME_DIR`, else `/run/warden` for root, else
/// `$XDG_RUNTIME_DIR/warden`, else `/tmp/warden-<uid>` (the protocol crate
/// has the logic, shared with the GUI).
pub fn runtime_dir() -> PathBuf {
    warden_protocol::paths::runtime_dir(crate::sys::euid(), crate::sys::uid())
}

/// One app's runtime directory: matches systemd's `RuntimeDirectory=warden/%i`.
pub fn app_runtime_dir(name: &str) -> PathBuf {
    runtime_dir().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = "[app]\nname = \"api\"\nargs = [\"run\", \"server.ts\"]\n";

    #[test]
    fn defaults() {
        let c = Config::parse(MIN).unwrap();
        assert_eq!(c.workers.count, 1);
        assert_eq!(c.workers.mode, Mode::Process);
        assert_eq!(c.app.command, "bun");
        assert!(c.shim_enabled());
        assert_eq!(c.restart.max_restarts, 10);
        assert_eq!(c.shutdown.grace_period, 30);
        assert!(!c.health.enabled);
        assert_eq!(c.reload.health_passes, 3);
        assert_eq!(c.watchdog.timeout, 60);
        assert_eq!(c.restart.failed_cooldown, 300);
        assert_eq!(c.health_path(), None);
    }

    #[test]
    fn an_app_address_is_checked() {
        let c = Config::parse("[app]\nname = \"api\"\nport = 443\naddress = \"2001:db8::10\"\n").unwrap();
        assert_eq!(c.app.address, Some("2001:db8::10".parse().unwrap()));
        let err = |text: &str| Config::parse(text).unwrap_err();
        assert!(err("[app]\nname = \"api\"\naddress = \"2001:db8::10\"\n").contains("needs app.port"));
        assert!(err("[app]\nname = \"api\"\nport = 443\naddress = \"::\"\n").contains("one address of this server"));
        assert!(err("[app]\nname = \"api\"\nport = 443\naddress = \"api.example.com\"\n").contains("address"));
        assert!(
            err("[app]\nname = \"e\"\nport = 443\naddress = \"2001:db8::1\"\n[route]\nhosts = { \"a.test\" = 1 }\n")
                .contains("route.host")
        );
    }

    #[test]
    fn a_route_section_is_checked() {
        let base = "[app]\nname = \"edge\"\nport = 443\n";
        let ok = |extra: &str| Config::parse(&format!("{base}{extra}"));
        let c = ok("[route]\nhosts = { \"api.example.com\" = \"api\", \"*.example.com\" = 8480, \"*\" = \"web\" }\n")
            .unwrap();
        let r = c.route.unwrap();
        assert_eq!(r.hosts["*.example.com"], RouteTarget::Port(8480));
        assert_eq!(r.hosts["api.example.com"], RouteTarget::App("api".into()));
        assert_eq!(r.host, "0.0.0.0");
        assert_eq!(r.client_ip(), cfg!(target_os = "linux"));
        let err = |extra: &str| ok(extra).unwrap_err();
        assert!(err("[route]\nhosts = {}\n").contains("at least one hostname"));
        assert!(err("[route]\nhosts = { \"API.example.com\" = 1 }\n").contains("lowercase"));
        assert!(err("[route]\nhosts = { \"a.test\" = 443 }\n").contains("router's own port"));
        assert!(err("[route]\nhost = \"localhost\"\nhosts = { \"a.test\" = 1 }\n").contains("not an IP address"));
        let no_port = Config::parse("[app]\nname = \"edge\"\n[route]\nhosts = { \"a.test\" = 1 }\n").unwrap_err();
        assert!(no_port.contains("needs app.port"), "{no_port}");
    }

    #[test]
    fn a_router_runs_one_worker_per_core_unless_told() {
        let base = "[app]\nname = \"edge\"\nport = 443\n[route]\nhosts = { \"a.test\" = 1 }\n";
        let n = if cfg!(target_os = "linux") { cpu_count() } else { 1 };
        assert_eq!(Config::parse(base).unwrap().workers.count, n);
        assert_eq!(Config::parse(&format!("{base}[workers]\ncount = 2\n")).unwrap().workers.count, 2);
        assert_eq!(Config::parse(MIN).unwrap().workers.count, 1, "apps keep their default");
    }

    /// Every numeric field: huge, boundary and random values either parse
    /// within bounds or are rejected with the field's name; never a panic, and
    /// anything accepted is safe for `Instant + Duration`.
    #[test]
    fn numeric_fields_are_bounded() {
        let fields = [
            ("workers", "ready_timeout"),
            ("restart", "max_restarts"),
            ("restart", "restart_window"),
            ("restart", "backoff_initial"),
            ("restart", "backoff_max"),
            ("restart", "failed_cooldown"),
            ("shutdown", "grace_period"),
            ("shutdown", "drain_ms"),
            ("shutdown", "long_lived_timeout"),
            ("health", "interval"),
            ("health", "timeout"),
            ("health", "failure_threshold"),
            ("health", "initial_delay"),
            ("reload", "health_passes"),
            ("reload", "health_interval_ms"),
            ("reload", "min_ready"),
            ("reload", "canary_soak"),
            ("reload", "pause"),
            ("reload", "timeout"),
            ("reload", "max_draining"),
            ("watchdog", "timeout"),
            ("watchdog", "port_lost"),
            ("limits", "max_memory"),
            ("limits", "max_lifetime"),
            ("watch", "debounce_ms"),
            ("watch", "interval_ms"),
            ("watch", "max_files"),
        ];
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for (section, key) in fields {
            let mut values = vec![0u64, 1, 59, 3600, 86_400, u32::MAX as u64, i64::MAX as u64, u64::MAX];
            values.extend((0..40).map(|_| next() >> (next() % 64)));
            for v in values {
                let text = format!("{MIN}[{section}]\n{key} = {v}\n");
                match std::panic::catch_unwind(|| Config::parse(&text)) {
                    Err(_) => panic!("{section}.{key} = {v} panicked"),
                    Ok(Err(e)) => {
                        assert!(
                            e.contains(key) || e.contains("TOML") || e.contains("invalid"),
                            "{section}.{key} = {v}: {e}"
                        )
                    }
                    Ok(Ok(c)) => {
                        let now = std::time::Instant::now();
                        for secs in [
                            c.reload.timeout,
                            c.limits.max_lifetime,
                            c.restart.failed_cooldown,
                            c.shutdown.grace_period,
                        ] {
                            let _ = now + std::time::Duration::from_secs(secs);
                        }
                        let _ = now + c.long_lived_timeout();
                        let _ = now + c.watch.debounce() + c.watch.interval();
                        assert!(c.watch.max_files <= Watch::MAX_FILES && c.watch.max_files > 0);
                    }
                }
            }
        }
        let e = Config::parse(&format!("{MIN}[reload]\ntimeout = 18446744073709551615\n")).unwrap_err();
        assert!(e.contains("reload.timeout") && e.contains("out of range"), "{e}");
        assert!(Config::parse(&format!("{MIN}[health]\noutage_threshold = 1.5\n")).is_err());
    }

    /// `[watch]`: off unless asked; every value is checked even while it is off.
    #[test]
    fn watch_defaults_and_rules() {
        let c = Config::parse(MIN).unwrap();
        let w = &c.watch;
        assert!(!w.enabled, "opt-in");
        assert_eq!(
            (w.paths.as_slice(), w.debounce_ms, w.interval_ms, w.max_files),
            ([".".to_string()].as_slice(), 500, 1000, 10_000)
        );
        assert!(["node_modules", ".git", "*.log"].iter().all(|d| w.ignore.iter().any(|i| i == d)), "{:?}", w.ignore);
        assert_eq!(w.ignore.len(), DEFAULT_WATCH_IGNORE.len());
        let on = |extra: &str| Config::parse(&format!("{MIN}[watch]\nenabled = true\n{extra}"));
        let c = on("paths = [\"src\", \"/etc/app.json\"]\nignore = [\"dist\", \"**/*.test.ts\", \"/tmp/x\"]\ndebounce_ms = 0\n").unwrap();
        assert!(c.watch.enabled && c.watch.debounce_ms == 0);
        assert_eq!(c.watch.paths, ["src", "/etc/app.json"]);
        // Rejected, with the key named, on or off.
        for (extra, why) in [
            ("paths = []\n", "watch.paths"),
            ("paths = [\"\"]\n", "watch.paths"),
            ("ignore = [\"!keep\"]\n", "negation"),
            ("ignore = [\"\"]\n", "watch.ignore"),
            ("ignore = [\"a\\\\\"]\n", "lone"),
            ("interval_ms = 99\n", "watch.interval_ms"),
            ("interval_ms = 3600001\n", "watch.interval_ms"),
            ("max_files = 0\n", "watch.max_files"),
            ("max_files = 100001\n", "watch.max_files"),
            ("debounce_ms = 600001\n", "watch.debounce_ms"),
            ("poll = 5\n", "unknown field"),
        ] {
            let e = Config::parse(&format!("{MIN}[watch]\n{extra}")).unwrap_err();
            assert!(e.contains(why), "{extra}: {e}");
            assert!(on(extra).is_err(), "{extra}");
        }
        let many = format!("ignore = [{}]\n", vec!["\"x\""; Watch::MAX_IGNORE + 1].join(","));
        assert!(Config::parse(&format!("{MIN}[watch]\n{many}")).unwrap_err().contains("watch.ignore"));
        let many = format!("paths = [{}]\n", vec!["\"x\""; Watch::MAX_PATHS + 1].join(","));
        assert!(Config::parse(&format!("{MIN}[watch]\n{many}")).unwrap_err().contains("watch.paths"));
        // A static site reads its files from disk: nothing to restart.
        let site = "[app]\nname = \"s\"\nport = 8080\n[static]\nroot = \"/srv/s\"\n";
        assert!(Config::parse(site).is_ok());
        let e = Config::parse(&format!("{site}[watch]\nenabled = true\n")).unwrap_err();
        assert!(e.contains("[static]"), "{e}");
        // The settings reach the status/config JSON that `warden describe` reads.
        let v = serde_json::to_value(on("").unwrap()).unwrap();
        assert_eq!((v["watch"]["enabled"].clone(), v["watch"]["max_files"].clone()), (true.into(), 10_000.into()));
        assert_eq!(on("debounce_ms = 250\ninterval_ms = 100\n").unwrap().watch.debounce(), Duration::from_millis(250));
    }

    #[test]
    fn long_lived_timeout_default_and_bounds() {
        let ll = |extra: &str| Config::parse(&format!("{MIN}[shutdown]\n{extra}"));
        assert_eq!(Config::parse(MIN).unwrap().long_lived_timeout(), Duration::from_secs(2));
        // Unset with a short grace period: half of it, so the config stays valid.
        assert_eq!(ll("grace_period = 1\n").unwrap().long_lived_timeout(), Duration::from_millis(500));
        assert_eq!(ll("grace_period = 0\n").unwrap().long_lived_timeout(), Duration::ZERO);
        assert_eq!(ll("long_lived_timeout = 0.25\n").unwrap().long_lived_timeout(), Duration::from_millis(250));
        assert_eq!(ll("long_lived_timeout = 5\n").unwrap().long_lived_timeout(), Duration::from_secs(5));
        // 0: leave them alone, whatever the grace period.
        assert_eq!(ll("grace_period = 0\nlong_lived_timeout = 0\n").unwrap().long_lived_timeout(), Duration::ZERO);
        let e = ll("grace_period = 5\nlong_lived_timeout = 5\n").unwrap_err();
        assert!(e.contains("must be below shutdown.grace_period = 5") && e.contains("raise grace_period"), "{e}");
        for bad in ["-1", "nan", "inf", "-inf"] {
            let e = ll(&format!("long_lived_timeout = {bad}\n")).unwrap_err();
            assert!(e.contains("long_lived_timeout"), "{bad}: {e}");
        }
        assert!(ll("long_lived_timeout = \"2s\"\n").is_err());
    }

    #[test]
    fn health_socket_paths_must_fit() {
        let long = format!("{MIN}[health]\npath = \"/health\"\n[control]\nsocket = \"/{}/c.sock\"\n", "d".repeat(80));
        assert!(Config::parse(&long).unwrap_err().contains("shorter"));
        let py = "[app]\nname = \"a\"\ncommand = \"python3\"\nargs = [\"s.py\"]\n[health]\npath = \"/health\"\n";
        assert!(Config::parse(py).unwrap_err().contains("set health.url"));
    }

    #[test]
    fn health_path_derivation() {
        let c = Config::parse(&format!("{MIN}[health]\nenabled = true\nurl = \"http://127.0.0.1:3000/healthz\"\n"))
            .unwrap();
        assert_eq!(c.health_path().as_deref(), Some("/healthz"));
        let c = Config::parse(&format!("{MIN}[health]\npath = \"/ready\"\nurl = \"http://127.0.0.1:3000/healthz\"\n"))
            .unwrap();
        assert_eq!(c.health_path().as_deref(), Some("/ready"));
        assert!(Config::parse(&format!("{MIN}[health]\nenabled = true\n")).is_err());
        assert!(Config::parse(&format!("{MIN}[health]\npath = \"health\"\n")).is_err());
        assert!(Config::parse(&format!("{MIN}[reload]\nverify_command = \" \"\n")).is_err());
    }

    #[test]
    fn example_config_parses_and_documents_the_defaults() {
        let text = include_str!("../warden.example.toml");
        let c = Config::parse(text).unwrap();
        assert_eq!(c.workers.count, 4);
        assert_eq!(c.logging.max_lines_per_sec, Logging::default().max_lines_per_sec);
        assert_eq!(c.logging.rotate, Rotate::default());
        assert_eq!(c.shutdown.long_lived_timeout, Some(DEFAULT_LONG_LIVED_TIMEOUT));
        assert_eq!(c.reload.surge, Reload::default().surge);
        assert_eq!(c.reload.max_draining, DEFAULT_MAX_DRAINING);
        assert_eq!(c.watchdog.loop_delay_warn, Watchdog::default().loop_delay_warn);
        assert!(c.app.pin_release);
        // The [watch] block lists the defaults too (and is off).
        assert_eq!(c.watch, Watch::default());
        // The commented [static] block is valid too.
        let uncommented: String = text
            .split("# [static]")
            .nth(1)
            .unwrap()
            .lines()
            .map(|l| l.strip_prefix("# ").unwrap_or(l))
            .collect::<Vec<_>>()
            .join("\n");
        let site = format!("[app]\nname = \"site\"\nport = 8080\n[static]{uncommented}\n");
        let c = Config::parse(&site).unwrap();
        let st = c.static_files.unwrap();
        assert_eq!((st.index.as_str(), st.cache_max_age, st.precompressed), ("index.html", 0, true));
        assert_eq!((st.cache_size, st.cache_max_file, st.cache_valid_ms), (0, 64 << 10, 1000));
    }

    #[test]
    fn static_cache_and_io_settings() {
        let base = "[app]\nname = \"site\"\nport = 8080\n[static]\nroot = \"/srv/site\"\n";
        // No cache unless asked for.
        let st = Config::parse(base).unwrap().static_files.unwrap();
        assert_eq!((st.cache_size, st.cache_max_file, st.cache_valid_ms), (0, 64 << 10, 1000));
        let st =
            Config::parse(&format!("{base}cache_size = \"64MB\"\ncache_max_file = \"256K\"\ncache_valid_ms = 0\n"))
                .unwrap()
                .static_files
                .unwrap();
        assert_eq!((st.cache_size, st.cache_max_file, st.cache_valid_ms), (64 << 20, 256 << 10, 0));
        // 0 is the default, no cache; plain numbers are bytes.
        let st =
            Config::parse(&format!("{base}cache_size = 0\ncache_max_file = 1000\n")).unwrap().static_files.unwrap();
        assert_eq!((st.cache_size, st.cache_max_file), (0, 1000));
        // Out of range or malformed values are refused with the key's name.
        for (bad, why) in [
            ("cache_size = \"5G\"", "static.cache_size"),
            ("cache_max_file = \"17M\"", "static.cache_max_file"),
            ("cache_valid_ms = 3600001", "static.cache_valid_ms"),
            ("cache_size = \"lots\"", "not a size"),
            // io_uring was tried and dropped (docs/benchmarks.md): the key is gone.
            ("io = \"uring\"", "unknown field"),
        ] {
            let e = Config::parse(&format!("{base}{bad}\n")).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        // The worker gets the section as JSON (WARDEN_STATIC); sizes survive the round trip.
        let json = serde_json::to_string(&st).unwrap();
        assert_eq!(serde_json::from_str::<Static>(&json).unwrap(), st);
    }

    #[test]
    fn static_precompressed_skip_has_a_default_and_can_be_replaced() {
        let base = "[app]\nname = \"site\"\nport = 8080\n[static]\nroot = \"/srv/site\"\n";
        let skip =
            |extra: &str| Config::parse(&format!("{base}{extra}\n")).unwrap().static_files.unwrap().precompressed_skip;
        // Unset: formats that are compressed already, and nothing a text file could be.
        let default = skip("");
        for ext in ["png", "jpg", "webp", "woff2", "mp4", "zip", "pdf", "docx"] {
            assert!(default.iter().any(|e| e == ext), "{ext}");
        }
        for ext in ["js", "css", "html", "svg", "json", "txt", "csv", "wasm", "ttf", "ico"] {
            assert!(!default.iter().any(|e| e == ext), "{ext}");
        }
        assert_eq!(default.len(), PRECOMPRESSED_SKIP.len());
        // A list replaces the default, and says what it means: no merging.
        assert_eq!(skip("precompressed_skip = [\"png\", \"bin\"]"), ["png", "bin"]);
        assert!(skip("precompressed_skip = []").is_empty(), "[] looks up every file");
        // Dots and capitals are forgiven.
        assert_eq!(skip("precompressed_skip = [\".PNG\", \" Jpg \"]"), ["png", "jpg"]);
        // Anything that is not an extension is refused, with the key's name.
        for bad in ["\"\"", "\"a/b\"", "\"png.br\"", "\"waytoolongextension\"", "\"p ng\"", "5"] {
            let e = Config::parse(&format!("{base}precompressed_skip = [{bad}]\n")).unwrap_err();
            assert!(e.contains("precompressed_skip") || e.contains("invalid type"), "{bad}: {e}");
        }
        // The worker gets it in WARDEN_STATIC.
        let st = Config::parse(&format!("{base}precompressed_skip = [\"png\"]\n")).unwrap().static_files.unwrap();
        let json = serde_json::to_string(&st).unwrap();
        assert_eq!(serde_json::from_str::<Static>(&json).unwrap().precompressed_skip, ["png"]);
        // A worker started with a configuration that has no such key has the default.
        let old = serde_json::json!({ "root": "/srv/site" }).to_string();
        assert_eq!(serde_json::from_str::<Static>(&old).unwrap().precompressed_skip, default);
    }

    #[test]
    fn static_background_compression_is_on_by_default_and_checked() {
        let base = "[app]\nname = \"site\"\nport = 8080\n[static]\nroot = \"/srv/site\"\n";
        let st = |extra: &str| Config::parse(&format!("{base}{extra}\n"));
        let d = st("").unwrap().static_files.unwrap();
        assert!(d.compress && d.precompressed);
        assert_eq!(
            (d.compress_jobs, d.compress_dir.as_deref()),
            (0, None),
            "one per core; the supervisor picks a folder"
        );
        assert_eq!((d.compress_dir_size, d.compress_min_file, d.compress_max_file), (256 << 20, 1 << 10, 8 << 20));
        let s = st(
            "compress = false\ncompress_jobs = 3\ncompress_dir = \"/var/cache/site\"\ncompress_dir_size = \"1GB\"\n",
        )
        .unwrap()
        .static_files
        .unwrap();
        assert!(!s.compress);
        assert_eq!((s.compress_jobs, s.compress_dir_size), (3, 1 << 30));
        assert_eq!(s.compress_dir.as_deref(), Some(Path::new("/var/cache/site")));
        let s = st("compress_min_file = \"512\"\ncompress_max_file = \"2MB\"").unwrap().static_files.unwrap();
        assert_eq!((s.compress_min_file, s.compress_max_file), (512, 2 << 20));
        for (bad, why) in [
            ("compress_jobs = 1000", "compress_jobs"),
            ("compress_jobs = -1", "compress_jobs"),
            ("compress_max_file = \"100MB\"", "compress_max_file"),
            ("compress_min_file = \"2MB\"\ncompress_max_file = \"1MB\"", "compress_min_file"),
            ("compress_dir_size = \"1MB\"\ncompress_max_file = \"8MB\"", "compress_dir_size"),
            ("compress_dir = \"\"", "compress_dir"),
            ("compress = \"yes please\"", "compress"),
        ] {
            let e = st(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
        // A worker started without these keys has the same defaults.
        let old = serde_json::json!({ "root": "/srv/site" }).to_string();
        let w = serde_json::from_str::<Static>(&old).unwrap();
        assert!(w.compress && w.compress_dir.is_none());
        // A relative folder is read from the working directory like `root` is.
        let dir = std::env::temp_dir().join(format!("warden-compress-dir-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("warden.toml");
        std::fs::write(&file, format!("{base}compress_dir = \"copies\"\n").replace("/srv/site", "site")).unwrap();
        let loaded = Config::load(&file).unwrap().static_files.unwrap();
        assert_eq!((loaded.root, loaded.compress_dir), (dir.join("site"), Some(dir.join("copies"))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn static_html_max_age_is_opt_in_and_bounded() {
        let base = "[app]\nname = \"site\"\nport = 8080\n[static]\nroot = \"/srv/site\"\n";
        let parse = |extra: &str| Config::parse(&format!("{base}{extra}\n"));
        assert_eq!(parse("").unwrap().static_files.unwrap().html_max_age, None, "unset: revalidate, as before");
        for (text, want) in [("0", 0), ("300", 300), ("31536000", 31_536_000)] {
            let st = parse(&format!("html_max_age = {text}")).unwrap().static_files.unwrap();
            assert_eq!(st.html_max_age, Some(want));
            // The worker gets it in WARDEN_STATIC.
            let back: Static = serde_json::from_str(&serde_json::to_string(&st).unwrap()).unwrap();
            assert_eq!(back.html_max_age, Some(want));
        }
        for (bad, why) in [
            ("html_max_age = 31536001", "static.html_max_age = 31536001 is out of range"),
            ("html_max_age = -1", "invalid value"),
            ("html_max_age = \"60\"", "invalid type"),
            ("html_max_age = 1.5", "invalid type"),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    #[test]
    fn env_files() {
        let e = parse_env_file(
            "# secrets\n\nDB_URL=postgres://u:p@h/db\nexport TOKEN = abc # note\nQUOTED=\"a b\\n\\\"c\\\"\"\nLIT='x $y \\n'\nEMPTY=\n",
        )
        .unwrap();
        assert_eq!(e["DB_URL"], "postgres://u:p@h/db");
        assert_eq!(e["TOKEN"], "abc");
        assert_eq!(e["QUOTED"], "a b\n\"c\"");
        assert_eq!(e["LIT"], "x $y \\n");
        assert_eq!(e["EMPTY"], "");
        assert!(parse_env_file("NOEQUALS\n").unwrap_err().contains("line 1"));
        assert!(parse_env_file("1BAD=x\n").unwrap_err().contains("not a valid variable name"));
        // The error says what is wrong and what to do (a hyphen is the usual culprit).
        let e = parse_env_file("A=1\nmy-app=x\n").unwrap_err();
        assert!(e.contains("line 2") && e.contains("\"my-app\"") && e.contains("rename or remove"), "{e}");
        assert!(valid_env_name("_x1") && !valid_env_name("") && !valid_env_name("a-b") && !valid_env_name("1a"));
        assert!(parse_env_file("A=\"open\n").unwrap_err().contains("unterminated"));
        // Loaded relative to the config file; `env` wins over the file.
        let dir = std::env::temp_dir().join(format!("warden-envfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("api.env"), "A=from-file\nB=file-only\n").unwrap();
        std::fs::write(dir.join("api.toml"), format!("{MIN}env_file = \"api.env\"\nenv = {{ A = \"from-config\" }}\n"))
            .unwrap();
        let c = Config::load(&dir.join("api.toml")).unwrap();
        let env: BTreeMap<&String, &String> = c.app.environment().collect();
        assert_eq!(env.get(&"A".to_string()).map(|s| s.as_str()), Some("from-config"));
        assert_eq!(env.get(&"B".to_string()).map(|s| s.as_str()), Some("file-only"));
        std::fs::remove_file(dir.join("api.env")).unwrap();
        assert!(Config::load(&dir.join("api.toml")).unwrap_err().contains("env_file"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn private_sockets_only_when_something_uses_them() {
        let c = Config::parse(MIN).unwrap();
        assert!(c.health_sockets() && !c.private_sockets(), "no health path, no verify_command: no socket");
        let c = Config::parse(&format!("{MIN}[health]\npath = \"/health\"\n")).unwrap();
        assert!(c.private_sockets());
        let c = Config::parse(&format!("{MIN}[health]\nenabled = true\nurl = \"http://127.0.0.1:3000/hz\"\n")).unwrap();
        assert!(c.private_sockets(), "an app-level URL's path is checked per worker too");
        let c =
            Config::parse(&format!("{MIN}[reload]\nverify_command = \"curl --unix-socket $WARDEN_WORKER_SOCKET x\"\n"))
                .unwrap();
        assert!(c.private_sockets(), "verify_command gets WARDEN_WORKER_SOCKET");
    }

    #[test]
    fn prd_example_parses() {
        let text = r#"
[app]
name = "travelerwe-api"
command = "bun"
args = ["run", "dist/main.js"]
working_directory = "/srv/apps/travelerwe/api"

[workers]
count = 4
mode = "process"

[restart]
enabled = true
max_restarts = 10
restart_window = 60
backoff_initial = 100
backoff_max = 10000

[shutdown]
grace_period = 30

[health]
enabled = true
url = "http://127.0.0.1:3000/health"
interval = 5
timeout = 2
failure_threshold = 3

[logging]
level = "info"
"#;
        let c = Config::parse(text).unwrap();
        assert_eq!(c.workers.count, 4);
        assert!(c.health.enabled);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_values() {
        assert!(Config::parse(&format!("{MIN}[workers]\ncount = 0\n")).is_err());
        assert!(Config::parse(&format!("{MIN}[workers]\ncuont = 2\n")).is_err());
        assert!(Config::parse(&format!("{MIN}[workers]\nmode = \"thread\"\n")).is_err());
        assert!(Config::parse("[app]\nname = \"a b\"\n").is_err());
        assert!(Config::parse(&format!("{MIN}[health]\nenabled = true\nurl = \"https://x\"\n")).is_err());
        assert!(Config::parse(&format!("{MIN}[restart]\nbackoff_initial = 500\nbackoff_max = 100\n")).is_err());
    }

    #[test]
    fn direct_output_rules() {
        let direct = |extra: &str| Config::parse(&format!("{MIN}{extra}"));
        let l = "[logging]\nworker_output = \"direct\"\n";
        let err = direct(l).unwrap_err();
        assert!(err.contains("set logging.out_file"), "{err}");
        let err = direct(&format!("{l}err_file = \"/tmp/e.log\"\n")).unwrap_err();
        assert!(err.contains("needs logging.out_file for stdout"), "{err}");
        let c = direct(&format!("{l}out_file = \"/tmp/o.log\"\n")).unwrap();
        let files = c.log_files();
        assert!(files.out_file.is_none() && files.err_file.is_none(), "the writer thread leaves them alone");
        // Several processes need a file each.
        let many = format!("[workers]\ncount = 3\n{l}out_file = \"/tmp/o.log\"\n");
        let err = direct(&many).unwrap_err();
        assert!(err.contains("workers.count = 3") && err.contains("per_worker_files = true"), "{err}");
        assert!(direct(&format!("{many}per_worker_files = true\n")).is_ok());
        // Worker mode is one process, whatever the count.
        let w = format!(
            "[app]\nname = \"a\"\nentry = \"main.js\"\n[workers]\nmode = \"worker\"\ncount = 4\n{l}out_file = \"/tmp/o.log\"\n"
        );
        assert!(Config::parse(&w).is_ok());
        let err = direct(&format!("{l}out_file = \"/tmp/o.log\"\nfile_timestamps = true\n")).unwrap_err();
        assert!(err.contains("file_timestamps"), "{err}");
        // capture keeps its files for the writer thread; inherit can't have any.
        let c = direct("[logging]\nout_file = \"/tmp/o.log\"\n").unwrap();
        assert!(c.log_files().out_file.is_some());
        let err = direct("[logging]\nworker_output = \"inherit\"\nout_file = \"/tmp/o.log\"\n").unwrap_err();
        assert!(err.contains("\"capture\" or \"direct\""), "{err}");
    }

    #[test]
    fn standby_rules() {
        let ok = format!("{MIN}port = 3000\n[workers]\ncount = 2\nstandby = 1\n");
        let c = Config::parse(&ok).unwrap();
        assert_eq!(c.workers.standby, 1);
        assert_eq!(Config::parse(MIN).unwrap().workers.standby, 0, "off by default");
        let node =
            "[app]\nname = \"api\"\ncommand = \"node\"\nargs = [\"s.js\"]\nport = 3000\n[workers]\nstandby = 2\n";
        assert!(Config::parse(node).is_ok());
        let err = |toml: &str| Config::parse(toml).unwrap_err();
        let e = err(&format!("{MIN}[workers]\nstandby = 1\n"));
        assert!(e.contains("needs app.port"), "{e}");
        let e =
            err("[app]\nname = \"a\"\nentry = \"main.js\"\nport = 3000\n[workers]\nmode = \"worker\"\nstandby = 1\n");
        assert!(e.contains("for process mode"), "{e}");
        let e = err(&format!("{MIN}port = 3000\nshim = false\n[workers]\nstandby = 1\n"));
        assert!(e.contains("needs Warden's shim"), "{e}");
        let e = err("[app]\nname = \"a\"\ncommand = \"python3\"\nport = 3000\n[workers]\nstandby = 1\n");
        assert!(e.contains("needs Warden's shim"), "{e}");
        let e = err(&format!("{MIN}port = 3000\n[workers]\ncount = 2\nstandby = 1\nport_strategy = \"offset\"\n"));
        assert!(e.contains("port_strategy = \"shared\""), "{e}");
        let e = err(&format!(
            "{MIN}port = 3000\n[workers]\nstandby = 1\n[logging]\nworker_output = \"direct\"\nout_file = \"/tmp/o.log\"\n"
        ));
        assert!(e.contains("\"direct\""), "{e}");
        let e = err(&format!("{MIN}port = 3000\n[static]\nroot = \"/srv\"\n[workers]\nstandby = 1\n"));
        assert!(e.contains("[static]"), "{e}");
        let e = err(&format!("{MIN}port = 3000\n[workers]\nstandby = 2000\n"));
        assert!(e.contains("out of range"), "{e}");
        // Any checkout path: standbys need no socket path of their own.
        let long = format!("{ok}[control]\nsocket = \"/tmp/{}/c.sock\"\n", "d".repeat(150));
        assert!(Config::parse(&long).is_ok());
    }

    #[test]
    fn worker_mode_rules() {
        let w = "[app]\nname = \"a\"\nentry = \"main.js\"\n[workers]\nmode = \"worker\"\n";
        assert!(Config::parse(w).is_ok());
        assert!(Config::parse("[app]\nname = \"a\"\n[workers]\nmode = \"worker\"\n").is_err());
        assert!(Config::parse(&format!("{w}port_strategy = \"offset\"\n")).is_err());
    }

    #[test]
    fn surge_values_and_rules() {
        let c = Config::parse(MIN).unwrap();
        assert_eq!(c.reload.surge, Surge::Count(1));
        assert!(c.app.pin_release, "pinning is on by default");
        let c = Config::parse(&format!("{MIN}[reload]\nsurge = 2\n")).unwrap();
        assert_eq!((c.reload.surge, c.reload.surge.batch(4), c.reload.surge.batch(1)), (Surge::Count(2), 2, 1));
        let c = Config::parse(&format!("{MIN}[reload]\nsurge = \"all\"\n")).unwrap();
        assert_eq!((c.reload.surge, c.reload.surge.batch(4)), (Surge::All, 4));
        assert_eq!(serde_json::to_value(c.reload.surge).unwrap(), serde_json::json!("all"));
        assert_eq!(serde_json::to_value(Surge::Count(3)).unwrap(), serde_json::json!(3));
        for bad in ["0", "-1", "2000", "\"most\""] {
            let e = Config::parse(&format!("{MIN}[reload]\nsurge = {bad}\n")).unwrap_err();
            assert!(e.contains("surge"), "{bad}: {e}");
        }
        // Workers that can't overlap can't surge.
        let offset =
            "[app]\nname = \"a\"\nargs = [\"s.ts\"]\nport = 3000\n[workers]\ncount = 4\nport_strategy = \"offset\"\n";
        assert!(Config::parse(offset).is_ok());
        let e = Config::parse(&format!("{offset}[reload]\nsurge = 2\n")).unwrap_err();
        assert!(e.contains("port_strategy = \"offset\"") && e.contains("Fix:"), "{e}");
        assert!(Config::parse(&format!("{offset}[reload]\nsurge = 1\n")).is_ok());
        let py =
            "[app]\nname = \"a\"\ncommand = \"python3\"\nargs = [\"s.py\"]\nport = 8000\n[reload]\nsurge = \"all\"\n";
        assert!(Config::parse(py).unwrap_err().contains("can't overlap"));
        let c = Config::parse(&format!("{MIN}pin_release = false\n")).unwrap();
        assert!(!c.app.pin_release);
    }

    #[test]
    fn max_draining_default_and_bounds() {
        assert_eq!(Config::parse(MIN).unwrap().reload.max_draining, DEFAULT_MAX_DRAINING);
        assert_eq!(DEFAULT_MAX_DRAINING, 4, "warden.example.toml and docs/configuration.md say 4");
        let c = Config::parse(&format!("{MIN}[reload]\nmax_draining = 1\n")).unwrap();
        assert_eq!(c.reload.max_draining, 1);
        assert_eq!(Config::parse(&format!("{MIN}[reload]\nmax_draining = 1024\n")).unwrap().reload.max_draining, 1024);
        for bad in ["0", "1025", "-1", "\"all\"", "2.5"] {
            let e = Config::parse(&format!("{MIN}[reload]\nmax_draining = {bad}\n")).unwrap_err();
            assert!(e.contains("max_draining"), "{bad}: {e}");
        }
        // Workers that can't overlap stop before their replacement starts: allowed, it changes nothing there.
        let offset = "[app]\nname = \"a\"\nargs = [\"s.ts\"]\nport = 3000\n[workers]\ncount = 4\n\
                      port_strategy = \"offset\"\n[reload]\nmax_draining = 8\n";
        assert!(Config::parse(offset).is_ok());
    }

    #[test]
    fn offset_needs_port() {
        let base = format!("{MIN}[workers]\ncount = 4\nport_strategy = \"offset\"\n");
        assert!(Config::parse(&base).is_err());
        assert!(Config::parse(&base.replace("[workers]", "port = 3000\n[workers]")).is_ok());
    }

    #[test]
    fn lenient_socket_lookup() {
        let dir = std::env::temp_dir().join(format!("warden-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("broken.toml");
        std::fs::write(&f, "[app]\nname = \"api\"\n[control]\nsocket = \"/run/warden/api.sock\"\n[workers\n").unwrap();
        assert!(Config::load(&f).is_err());
        assert_eq!(socket_path_lenient(&f), Some(PathBuf::from("/run/warden/api.sock")));
        std::fs::write(&f, "[app]\nname = \"api\" # comment\n[workers\n").unwrap();
        assert_eq!(socket_path_lenient(&f), Some(runtime_dir().join("api").join("control.sock")));
    }

    #[test]
    fn shim_default_follows_command() {
        let node = "[app]\nname = \"a\"\ncommand = \"/usr/bin/node\"\nargs = [\"s.js\"]\n";
        assert!(Config::parse(node).unwrap().shim_enabled(), "node gets the shim via --import");
        let py = "[app]\nname = \"a\"\ncommand = \"python3\"\nargs = [\"s.py\"]\n";
        assert!(!Config::parse(py).unwrap().shim_enabled());
        assert!(is_bun("/home/x/.bun/bin/bun"));
        // No shim and a port: a rolling restart stops the old worker first.
        let c = Config::parse(&format!("{py}port = 8000\n")).unwrap();
        assert!(!c.overlap());
        assert!(Config::parse(&format!("{node}port = 8000\n")).unwrap().overlap());
        assert!(Config::parse(py).unwrap().overlap(), "no port: nothing to share");
    }

    /// `overlap()` and its reason agree: a rolling restart that stops the old
    /// worker first has nothing to roll back to, and `[watch]` says why.
    #[test]
    fn a_restart_without_overlap_says_why() {
        let py = "[app]\nname = \"a\"\ncommand = \"python3\"\nargs = [\"s.py\"]\nport = 8000\n";
        let why = |extra: &str| Config::parse(&format!("{py}{extra}")).unwrap().no_overlap_reason();
        assert!(why("").unwrap().contains("without Warden's shim"));
        assert!(why("[workers]\noverlap = false\n").unwrap().contains("overlap = false"));
        assert!(why("[workers]\nport_strategy = \"offset\"\n").unwrap().contains("offset"));
        assert_eq!(why("[workers]\noverlap = true\n"), None, "asked for");
        let node = "[app]\nname = \"a\"\ncommand = \"node\"\nargs = [\"s.js\"]\nport = 8000\n";
        assert_eq!(Config::parse(node).unwrap().no_overlap_reason(), None, "the shim shares the port");
        let c = Config::parse(&format!("{node}[workers]\nport_strategy = \"offset\"\n")).unwrap();
        assert!(c.no_overlap_reason().is_some() && !c.overlap());
        assert!(Config::parse("[app]\nname = \"a\"\ncommand = \"python3\"\n").unwrap().overlap(), "no port");
    }

    #[test]
    fn one_worker_on_a_many_core_linux_host_gets_the_max_hint() {
        let one = Config::parse(&format!("{MIN}port = 3000\n")).unwrap();
        let hint = more_workers_hint(&one, 1, true, 8).expect("1 worker of 8 cores");
        assert!(hint.contains("8 cores") && hint.contains("count = \"max\""), "{hint}");
        assert_eq!(more_workers_hint(&one, 1, false, 8), None, "macOS spreads no connections over the workers");
        assert_eq!(more_workers_hint(&one, 1, true, 1), None, "one core: nothing idle");
        assert_eq!(more_workers_hint(&one, 2, true, 8), None, "scaled up already");
        let no_port = Config::parse(MIN).unwrap();
        assert_eq!(more_workers_hint(&no_port, 1, true, 8), None, "no port to share");
        let offset = Config::parse(&format!("{MIN}port = 3000\n[workers]\nport_strategy = \"offset\"\n")).unwrap();
        assert_eq!(more_workers_hint(&offset, 1, true, 8), None, "a port per worker");
    }

    #[test]
    fn worker_mode_is_hinted_on_macos_and_before_bun_1_4() {
        let worker = Config::parse(
            "[app]\nname = \"w\"\nentry = \"main.ts\"\nport = 3000\n[workers]\nmode = \"worker\"\ncount = 4\n",
        )
        .unwrap();
        assert!(worker_mode_hint(&worker, true, Some((1, 4))).unwrap().contains("macOS"));
        assert!(worker_mode_hint(&worker, false, Some((1, 3))).unwrap().contains("Bun 1.3"));
        assert_eq!(worker_mode_hint(&worker, false, Some((1, 4))), None, "Bun 1.4 on Linux: level with processes");
        assert_eq!(worker_mode_hint(&worker, false, None), None, "version unknown: nothing to say");
        let process = Config::parse(&format!("{MIN}port = 3000\n")).unwrap();
        assert_eq!(worker_mode_hint(&process, true, Some((1, 3))), None);
    }
}

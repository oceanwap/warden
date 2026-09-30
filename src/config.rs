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
    /// `Cache-Control: max-age` in seconds for files (HTML is always
    /// revalidated; fingerprinted names like `app.3f9a2c1b.js` are cached a
    /// year, immutable).
    #[serde(default = "default_cache_max_age")]
    pub cache_max_age: u64,
    /// HTML listing for directories without an index.
    #[serde(default)]
    pub listing: bool,
    /// Serve dotfiles (except `.well-known`, always served).
    #[serde(default)]
    pub dotfiles: bool,
    /// Serve `file.br` / `file.gz` next to `file` when the client accepts them.
    #[serde(default = "yes")]
    pub precompressed: bool,
    /// `user:password` for HTTP Basic auth.
    #[serde(default)]
    pub basic_auth: Option<String>,
    /// Extra response headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// One stdout line per request (method, path, status, bytes, ms).
    #[serde(default)]
    pub access_log: bool,
}

fn default_static_host() -> String {
    "0.0.0.0".into()
}
fn default_index() -> String {
    "index.html".into()
}
fn default_cache_max_age() -> u64 {
    3600
}
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
    #[serde(default)]
    pub env: BTreeMap<String, String>,
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
}

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
}

/// Liveness: the shim sends a heartbeat from each worker's event loop. No
/// heartbeat for `timeout` seconds = hung worker, killed and restarted.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Watchdog {
    /// Seconds. 0 disables the watchdog.
    pub timeout: u64,
}

/// Graceful recycling (zero downtime: replacement first).
#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// MB of RSS per worker process (worker mode: the host process). 0 = off.
    pub max_memory: u64,
    /// Seconds; each worker is recycled after this, ±10% jitter. 0 = off.
    pub max_lifetime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
#[repr(u8)]
pub enum Level {
    Debug = 0,
    Info = 1,
    Warn = 2,
    Error = 3,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Logging {
    pub level: Level,
    /// `true`, `false`, or omitted = auto (off under journald).
    pub timestamps: Option<bool>,
    /// "capture" (default): worker lines go through Warden with a `worker=N`
    /// prefix and into `warden logs`. "inherit": workers write straight to
    /// Warden's stdout — for very chatty apps.
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
}

#[derive(Debug, Clone, Deserialize, serde::Serialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Metrics {
    /// e.g. "127.0.0.1:9464". Prometheus text format at /metrics.
    pub listen: Option<String>,
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
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self { grace_period: 30, drain_ms: 500, signal: "SIGTERM".into() }
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
        }
    }
}

impl Default for Watchdog {
    fn default() -> Self {
        Self { timeout: 60 }
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
        // A relative static root: from the working directory, else the config file.
        if let Some(st) = &mut cfg.static_files {
            if st.root.is_relative() {
                let base = cfg
                    .app
                    .working_directory
                    .clone()
                    .unwrap_or_else(|| path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(".")));
                st.root = base.join(&st.root);
            }
        }
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        self.check_bounds()?;
        let a = &self.app;
        if a.name.is_empty() || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("app.name must be non-empty and contain only [A-Za-z0-9._-]".into());
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
        if let Some(expr) = &self.restart.schedule {
            crate::schedule::Cron::parse(expr).map_err(|e| format!("restart.schedule = {expr:?}: {e}"))?;
        }
        if (self.logging.out_file.is_some() || self.logging.err_file.is_some())
            && self.logging.worker_output == WorkerOutput::Inherit
        {
            return Err("logging.out_file / err_file need worker_output = \"capture\" (with \"inherit\", \
                        worker output bypasses Warden)"
                .into());
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

    /// Upper bounds on every number, so no value can overflow time arithmetic
    /// (`Instant + Duration`) or counters at runtime. Generous: they only
    /// exclude values that are certainly mistakes.
    fn check_bounds(&self) -> Result<(), String> {
        const HOUR: u64 = 3600;
        const DAY: u64 = 86_400;
        let checks: [(&str, u64, u64); 25] = [
            ("workers.ready_timeout", self.workers.ready_timeout, HOUR),
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
        self.shim_enabled() || self.static_files.is_some()
    }

    /// Whether to inject the Bun shim.
    pub fn shim_enabled(&self) -> bool {
        match self.workers.mode {
            Mode::Worker => true,
            Mode::Process => self.app.shim.unwrap_or_else(|| is_bun(&self.app.command) || is_node(&self.app.command)),
        }
    }

    /// Log files as the writer thread needs them.
    pub fn log_files(&self) -> crate::logging::Files {
        let l = &self.logging;
        crate::logging::Files {
            file: l.file.clone(),
            out_file: l.out_file.clone(),
            err_file: l.err_file.clone(),
            per_worker: l.per_worker_files,
            timestamps: l.file_timestamps,
            rotate: crate::logging::RotatePolicy::from(&l.rotate),
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
        match self.workers.overlap {
            Some(v) => v,
            None => {
                self.workers.port_strategy != PortStrategy::Offset
                    && (self.app.port.is_none()
                        || self.shim_enabled()
                        || self.static_files.is_some()
                        || self.workers.mode == Mode::Worker)
            }
        }
    }

    pub fn ready_timeout(&self) -> Duration {
        Duration::from_secs(self.workers.ready_timeout)
    }

    pub fn grace_period(&self) -> Duration {
        Duration::from_secs(self.shutdown.grace_period)
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
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("WARDEN_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    if crate::sys::is_root() {
        return PathBuf::from("/run/warden");
    }
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("warden");
    }
    let uid = crate::sys::uid();
    std::env::temp_dir().join(format!("warden-{uid}"))
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
            ("watchdog", "timeout"),
            ("limits", "max_memory"),
            ("limits", "max_lifetime"),
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
                    }
                }
            }
        }
        let e = Config::parse(&format!("{MIN}[reload]\ntimeout = 18446744073709551615\n")).unwrap_err();
        assert!(e.contains("reload.timeout") && e.contains("out of range"), "{e}");
        assert!(Config::parse(&format!("{MIN}[health]\noutage_threshold = 1.5\n")).is_err());
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
        assert_eq!((st.index.as_str(), st.cache_max_age, st.precompressed), ("index.html", 3600, true));
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
    fn worker_mode_rules() {
        let w = "[app]\nname = \"a\"\nentry = \"main.js\"\n[workers]\nmode = \"worker\"\n";
        assert!(Config::parse(w).is_ok());
        assert!(Config::parse("[app]\nname = \"a\"\n[workers]\nmode = \"worker\"\n").is_err());
        assert!(Config::parse(&format!("{w}port_strategy = \"offset\"\n")).is_err());
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
}

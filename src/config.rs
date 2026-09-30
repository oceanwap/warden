//! TOML configuration. Deliberately flat: one file, a handful of sections,
//! every field optional except `[app] name` and the command / entry.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Process,
    Worker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PortStrategy {
    /// All workers bind the same port with SO_REUSEPORT.
    Shared,
    /// Worker i gets PORT = port + i - 1 (for platforms without reuseport).
    Offset,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Workers {
    pub count: usize,
    pub mode: Mode,
    pub port_strategy: PortStrategy,
    /// Seconds a worker may take to start listening.
    pub ready_timeout: u64,
}

#[derive(Debug, Clone, Deserialize)]
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
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Shutdown {
    /// Seconds to wait for workers to exit before SIGKILL.
    pub grace_period: u64,
    /// Milliseconds the shim keeps answering (with `Connection: close`)
    /// after closing its listener. 0 disables the shim's SIGTERM handling.
    pub drain_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnHealthFailure {
    /// Only log and export the metric.
    Log,
    /// Gracefully replace the failing worker (new one ready first).
    Replace,
    /// App-level check failing: rolling reload of every worker.
    Reload,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Health {
    pub enabled: bool,
    /// Path checked on each worker's private socket (shim). Default: the path of `url`.
    pub path: Option<String>,
    /// Optional app-level check through the shared port (any worker may answer).
    pub url: String,
    /// Seconds.
    pub interval: u64,
    /// Seconds.
    pub timeout: u64,
    pub failure_threshold: u32,
    pub on_failure: OnHealthFailure,
}

/// Gates every replacement worker must pass before the one it replaces is
/// drained (reload, safe-reload, restart N, health/memory/lifetime recycling).
#[derive(Debug, Clone, Deserialize)]
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
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Watchdog {
    /// Seconds. 0 disables the watchdog.
    pub timeout: u64,
}

/// Graceful recycling (zero downtime: replacement first).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Limits {
    /// MB of RSS per worker process (worker mode: the host process). 0 = off.
    pub max_memory: u64,
    /// Seconds; each worker is recycled after this, ±10% jitter. 0 = off.
    pub max_lifetime: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Logging {
    pub level: Level,
    /// `true`, `false`, or omitted = auto (off under journald).
    pub timestamps: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Metrics {
    /// e.g. "127.0.0.1:9464". Prometheus text format at /metrics.
    pub listen: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Control {
    /// Unix socket for the CLI. Default: $XDG_RUNTIME_DIR/warden/<name>.sock
    /// or /tmp/warden-<uid>/<name>.sock.
    pub socket: Option<PathBuf>,
}

fn default_command() -> String {
    "bun".into()
}

impl Default for Workers {
    fn default() -> Self {
        Self { count: 1, mode: Mode::Process, port_strategy: PortStrategy::Shared, ready_timeout: 30 }
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
        }
    }
}

impl Default for Shutdown {
    fn default() -> Self {
        Self { grace_period: 30, drain_ms: 500 }
    }
}

impl Default for Health {
    fn default() -> Self {
        Self {
            enabled: false,
            path: None,
            url: String::new(),
            interval: 5,
            timeout: 2,
            failure_threshold: 3,
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
        Self { level: Level::Info, timestamps: None }
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
        Ok(cfg)
    }

    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), String> {
        let a = &self.app;
        if a.name.is_empty() || !a.name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("app.name must be non-empty and contain only [A-Za-z0-9._-]".into());
        }
        if a.command.is_empty() {
            return Err("app.command must not be empty".into());
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
        if let Some(p) = &h.path {
            if !p.starts_with('/') || p.contains(char::is_whitespace) {
                return Err("health.path must start with '/' and contain no spaces".into());
            }
        }
        if h.enabled && h.url.is_empty() && h.path.is_none() {
            return Err("health.enabled needs health.path (per-worker) and/or health.url (app-level)".into());
        }
        if h.path.is_some() && h.url.is_empty() && !self.shim_enabled() {
            return Err("health.path is checked through the Bun shim's private socket; without the shim (non-Bun command or shim = false) set health.url instead".into());
        }
        if self.health_path().is_some() && self.shim_enabled() {
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

    /// Whether to inject the Bun shim.
    pub fn shim_enabled(&self) -> bool {
        match self.workers.mode {
            Mode::Worker => true,
            Mode::Process => self.app.shim.unwrap_or_else(|| is_bun(&self.app.command)),
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

    pub fn socket_path(&self) -> PathBuf {
        match &self.control.socket {
            Some(p) => p.clone(),
            None => runtime_dir().join(format!("{}.sock", self.app.name)),
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
    socket.or_else(|| name.map(|n| runtime_dir().join(format!("{n}.sock"))))
}

pub fn is_bun(command: &str) -> bool {
    Path::new(command).file_name().and_then(|n| n.to_str()).is_some_and(|n| n == "bun" || n == "bun.exe")
}

/// Directory for the control socket and the embedded JS files.
pub fn runtime_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("warden");
    }
    // SAFETY: getuid never fails.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("warden-{uid}"))
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
    fn health_socket_paths_must_fit() {
        let long = format!("{MIN}[health]\npath = \"/health\"\n[control]\nsocket = \"/{}/c.sock\"\n", "d".repeat(80));
        assert!(Config::parse(&long).unwrap_err().contains("shorter"));
        let node = "[app]\nname = \"a\"\ncommand = \"node\"\nargs = [\"s.js\"]\n[health]\npath = \"/health\"\n";
        assert!(Config::parse(node).unwrap_err().contains("set health.url"));
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
        assert_eq!(socket_path_lenient(&f), Some(runtime_dir().join("api.sock")));
    }

    #[test]
    fn shim_default_follows_command() {
        let node = "[app]\nname = \"a\"\ncommand = \"/usr/bin/node\"\nargs = [\"s.js\"]\n";
        assert!(!Config::parse(node).unwrap().shim_enabled());
        assert!(is_bun("/home/x/.bun/bin/bun"));
    }
}

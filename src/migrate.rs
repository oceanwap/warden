//! `warden pm2-migrate`: turn what PM2 runs into Warden configs, then
//! optionally cut over one app at a time, with rollback.
//!
//! Sources: `pm2 jlist` (the running daemon, default), `$PM2_HOME/dump.pm2`
//! (what `pm2 save` stored, when PM2 is down), or an ecosystem file
//! (evaluated with node, so the env is exactly what it declares).
//! Output per app: `<app>.toml`, `<app>.env` (mode 0600: env values never go
//! into the config) and one `MIGRATION.md` listing every field as mapped,
//! approximated (with the difference) or unsupported (with the reason).

use crate::cli::{Action, Args, Command, StartOpts};
use crate::config::Config;
use crate::fleet;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MigrateOpts {
    /// "jlist" (default), "dump", or an ecosystem file.
    pub from: Option<String>,
    /// Ecosystem `env_<name>` block to apply (PM2's `--env`).
    pub env: Option<String>,
    /// Only these apps.
    pub apps: Vec<String>,
    /// Where configs go (default: Warden's config directory).
    pub out: Option<PathBuf>,
    pub dry_run: bool,
    /// "process" or "worker": skip the per-app question.
    pub mode: Option<String>,
    pub cutover: Option<Cutover>,
    pub finalize: bool,
    pub overwrite: bool,
    pub yes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Cutover {
    /// Warden's workers join the port next to PM2's (reusePort), then PM2 stops.
    Overlap,
    /// PM2 stops, Warden starts on the same port; PM2 is restored on failure.
    SamePort,
    /// Warden starts on another port; switch the proxy, then stop PM2.
    NewPort(u16),
}

pub fn parse_cutover(s: &str) -> Result<Cutover, String> {
    match s {
        "overlap" => Ok(Cutover::Overlap),
        "same-port" => Ok(Cutover::SamePort),
        s => match s.strip_prefix("new-port:").and_then(|p| p.parse::<u16>().ok()) {
            Some(p) if p > 0 => Ok(Cutover::NewPort(p)),
            _ => Err(format!("--cutover {s:?}: overlap, same-port or new-port:<port>")),
        },
    }
}

/// One PM2 app, the same whatever the source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Pm2App {
    pub name: String,
    pub script: PathBuf,
    pub cwd: Option<PathBuf>,
    pub args: Vec<String>,
    /// "node", "bun", "none" (run the script itself), or a program.
    pub interpreter: Option<String>,
    pub interpreter_args: Vec<String>,
    /// A number, or "max" / "max-1".
    pub instances: String,
    pub cluster: bool,
    pub env: BTreeMap<String, String>,
    pub namespace: Option<String>,
    pub max_memory_mb: Option<u64>,
    pub cron: Option<String>,
    pub kill_timeout_ms: Option<u64>,
    pub kill_signal: Option<String>,
    pub wait_ready: bool,
    pub listen_timeout_ms: Option<u64>,
    pub min_uptime_ms: Option<u64>,
    pub max_restarts: Option<u32>,
    pub restart_delay_ms: Option<u64>,
    pub autorestart: bool,
    pub stop_exit_codes: Vec<i32>,
    pub out_file: Option<PathBuf>,
    pub err_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub merge_logs: bool,
    pub time: bool,
    pub instance_var: Option<String>,
    pub port: Option<u16>,
    pub pids: Vec<u32>,
    /// (field, how it was handled): "approximated: …" / "unsupported: …".
    pub notes: Vec<(String, String)>,
    /// Env names left out as inherited from the shell that ran `pm2 start`.
    pub dropped_env: Vec<String>,
}

// ------------------------------------------------------------------ sources

fn pm2_bin() -> String {
    std::env::var("WARDEN_PM2").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "pm2".into())
}

fn pm2_home() -> PathBuf {
    std::env::var_os("PM2_HOME").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
        std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/root")).join(".pm2")
    })
}

fn pm2(args: &[&str]) -> Result<String, String> {
    let bin = pm2_bin();
    let out = std::process::Command::new(&bin).args(args).output().map_err(|e| {
        format!("running `{bin} {}`: {e} (is PM2 installed? set WARDEN_PM2 to its path)", args.join(" "))
    })?;
    if !out.status.success() {
        return Err(format!(
            "`{bin} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("no output")
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// The environment of a process (the PM2 daemon), from /proc.
fn environ_of(pid: u32) -> Option<BTreeMap<String, String>> {
    let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter_map(|kv| {
                let kv = String::from_utf8_lossy(kv);
                kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect(),
    )
}

fn daemon_env() -> Option<BTreeMap<String, String>> {
    let pid: u32 = std::fs::read_to_string(pm2_home().join("pm2.pid")).ok()?.trim().parse().ok()?;
    environ_of(pid)
}

fn daemon_pid() -> Option<u32> {
    std::fs::read_to_string(pm2_home().join("pm2.pid")).ok()?.trim().parse().ok()
}

/// Apps from the source named by `from`.
pub fn load(o: &MigrateOpts) -> Result<(Vec<Pm2App>, String), String> {
    let from = o.from.clone().unwrap_or_else(|| "jlist".into());
    let (mut apps, label) = match from.as_str() {
        "jlist" => {
            let text = pm2(&["jlist"])?;
            let list: Vec<Value> =
                serde_json::from_str(text.trim()).map_err(|e| format!("`pm2 jlist` did not print JSON: {e}"))?;
            // Inherited = equal to the daemon's own environment.
            let base = daemon_env().unwrap_or_else(|| std::env::vars().collect());
            let entries: Vec<(Value, Option<u32>)> = list
                .into_iter()
                .map(|p| {
                    let pid = p.get("pid").and_then(Value::as_u64).map(|n| n as u32).filter(|n| *n > 0);
                    (p.get("pm2_env").cloned().unwrap_or(Value::Null), pid)
                })
                .collect();
            (group(entries, Some(&base)), "`pm2 jlist` (the running PM2 daemon)".to_string())
        }
        "dump" => {
            let path = pm2_home().join("dump.pm2");
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let list: Vec<Value> = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
            let base: BTreeMap<String, String> = std::env::vars().collect();
            (
                group(list.into_iter().map(|v| (v, None)).collect(), Some(&base)),
                format!("{} (`pm2 save`)", path.display()),
            )
        }
        file => {
            let path = Path::new(file);
            let list = ecosystem(path)?;
            let apps = list
                .iter()
                .map(|a| from_ecosystem(a, path.parent().unwrap_or(Path::new(".")), o.env.as_deref()))
                .collect::<Result<Vec<_>, _>>()?;
            (apps, format!("{} (ecosystem file)", path.display()))
        }
    };
    if !o.apps.is_empty() {
        for want in &o.apps {
            if !apps.iter().any(|a| &a.name == want) {
                let have: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
                return Err(format!("no PM2 app named {want:?} in {label}; there is: {}", have.join(", ")));
            }
        }
        apps.retain(|a| o.apps.contains(&a.name));
    }
    for a in &mut apps {
        if a.port.is_none() {
            a.port = detect_port(a);
        }
    }
    Ok((apps, label))
}

/// Ecosystem file → its `apps` list, evaluated by node (it is JavaScript).
fn ecosystem(path: &Path) -> Result<Vec<Value>, String> {
    let abs = std::fs::canonicalize(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let script = "const p=process.argv[1];(async()=>{let m;try{m=require(p)}catch(e){if(e.code!=='ERR_REQUIRE_ESM'&&e.code!=='ERR_REQUIRE_ASYNC_MODULE')throw e;m=(await import('file://'+p)).default}process.stdout.write(JSON.stringify(m&&m.default?m.default:m))})().catch(e=>{console.error(e&&e.message||e);process.exit(1)})";
    let out = std::process::Command::new("node")
        .args(["-e", script, &abs.display().to_string()])
        .output()
        .map_err(|e| format!("evaluating {} needs node: {e}", path.display()))?;
    if !out.status.success() {
        return Err(format!("evaluating {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    let v: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("{}: not JSON-like: {e}", path.display()))?;
    match v.get("apps").cloned().unwrap_or(v) {
        Value::Array(a) => Ok(a),
        o @ Value::Object(_) => Ok(vec![o]),
        _ => Err(format!("{}: no `apps` list", path.display())),
    }
}

// ------------------------------------------------------------ normalizing

fn s(v: &Value, k: &str) -> Option<String> {
    match v.get(k)? {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn n(v: &Value, k: &str) -> Option<u64> {
    match v.get(k)? {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn b(v: &Value, k: &str) -> Option<bool> {
    match v.get(k)? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => Some(s == "true"),
        _ => None,
    }
}

fn list(v: &Value, k: &str) -> Vec<String> {
    match v.get(k) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(Value::String(s)) => s.split_whitespace().map(String::from).collect(),
        _ => Vec::new(),
    }
}

/// "300M", "1G", 314572800 → MB.
fn memory_mb(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().map(|b| b.div_ceil(1 << 20)).filter(|m| *m > 0),
        Value::String(s) => crate::cli::parse_mb(s).ok(),
        _ => None,
    }
}

/// "1s", "500", "2m" (PM2's min_uptime) → ms.
fn duration_ms(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => {
            let s = s.trim();
            let (num, mult) = match s.chars().last()? {
                'h' => (&s[..s.len() - 1], 3_600_000),
                'm' => (&s[..s.len() - 1], 60_000),
                's' => (&s[..s.len() - 1], 1000),
                _ => (s, 1),
            };
            num.trim().parse::<u64>().ok().map(|n| n * mult)
        }
        _ => None,
    }
}

/// Environment variables PM2 or a login shell sets that no app config means.
const NOT_APP_ENV: &[&str] = &[
    "pm_id",
    "name",
    "NODE_APP_INSTANCE",
    "PM2_HOME",
    "PM2_USAGE",
    "PM2_JSON_PROCESSING",
    "PM2_INTERACTOR_PROCESSING",
    "PM2_PROGRAMMATIC",
    "PM2_DISCRETE_MODE",
    "unique_id",
    "km_link",
    "vizion_running",
    "windowsHide",
    "_",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "TERM",
    "COLORTERM",
    "LS_COLORS",
    "DISPLAY",
    "MAIL",
    "SSH_CLIENT",
    "SSH_CONNECTION",
    "SSH_TTY",
    "SSH_AUTH_SOCK",
    "TMUX",
    "TMUX_PANE",
    "STY",
    "WINDOW",
    "XDG_SESSION_ID",
    "XDG_SESSION_TYPE",
    "XDG_SESSION_CLASS",
    "LESSOPEN",
    "LESSCLOSE",
    "HISTSIZE",
    "HISTFILESIZE",
    "PS1",
    "PROMPT_COMMAND",
    "SUDO_USER",
    "SUDO_UID",
    "SUDO_GID",
    "SUDO_COMMAND",
    "BUN_OPTIONS",
];

/// PM2 entries (one per instance) → one app per name.
fn group(entries: Vec<(Value, Option<u32>)>, base_env: Option<&BTreeMap<String, String>>) -> Vec<Pm2App> {
    let mut apps: Vec<Pm2App> = Vec::new();
    for (env, pid) in entries {
        let Some(name) = s(&env, "name") else { continue };
        if let Some(a) = apps.iter_mut().find(|a| a.name == name) {
            a.instances = (a.instances.parse::<usize>().unwrap_or(1) + 1).to_string();
            a.pids.extend(pid);
            continue;
        }
        let mut a = from_pm2_env(&env, base_env);
        a.instances = "1".into();
        a.pids.extend(pid);
        apps.push(a);
    }
    apps
}

/// A `pm2_env` object (jlist entry or dump.pm2 entry).
pub fn from_pm2_env(e: &Value, base_env: Option<&BTreeMap<String, String>>) -> Pm2App {
    let mut a = common(e);
    a.script = s(e, "pm_exec_path").map(PathBuf::from).unwrap_or_default();
    a.cwd = s(e, "pm_cwd").map(PathBuf::from);
    a.cluster = s(e, "exec_mode").is_some_and(|m| m.starts_with("cluster"));
    a.out_file = s(e, "pm_out_log_path").filter(|p| p != "/dev/null").map(PathBuf::from);
    a.err_file = s(e, "pm_err_log_path").filter(|p| p != "/dev/null").map(PathBuf::from);
    // pm2_env.env is the whole environment of whoever ran `pm2 start`
    // (hundreds of keys, secrets included): keep what differs from the base.
    if let Some(Value::Object(env)) = e.get("env") {
        for (k, v) in env {
            let Some(v) = v.as_str().map(String::from).or_else(|| (!v.is_object()).then(|| v.to_string())) else {
                continue;
            };
            let inherited = base_env.is_some_and(|b| b.get(k) == Some(&v));
            if inherited || NOT_APP_ENV.contains(&k.as_str()) || k.starts_with("PM2_") || k.starts_with("LC_") {
                a.dropped_env.push(k.clone());
            } else {
                a.env.insert(k.clone(), v);
            }
        }
    }
    // PM2 applies these defaults to every app; only other values mean something.
    if a.kill_timeout_ms == Some(1600) {
        a.kill_timeout_ms = None;
    }
    if a.max_restarts == Some(16) {
        a.max_restarts = None;
    }
    if a.min_uptime_ms == Some(1000) {
        a.min_uptime_ms = None;
    }
    a
}

/// An ecosystem file app (options as the user wrote them).
pub fn from_ecosystem(e: &Value, dir: &Path, env_name: Option<&str>) -> Result<Pm2App, String> {
    let mut a = common(e);
    if a.name.is_empty() {
        a.name = s(e, "script")
            .and_then(|p| Path::new(&p).file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "app".into());
    }
    let cwd = s(e, "cwd").map(|c| dir.join(c)).unwrap_or_else(|| dir.to_path_buf());
    let script = s(e, "script").ok_or_else(|| format!("app {:?}: no `script`", a.name))?;
    a.script = cwd.join(script);
    a.cwd = Some(cwd);
    a.cluster = s(e, "exec_mode").is_some_and(|m| m.starts_with("cluster")) || e.get("instances").is_some();
    a.out_file = s(e, "out_file").or_else(|| s(e, "output")).map(PathBuf::from);
    a.err_file = s(e, "error_file").or_else(|| s(e, "error")).map(PathBuf::from);
    let mut add_env = |block: Option<&Value>| {
        if let Some(Value::Object(m)) = block {
            for (k, v) in m {
                let v = v.as_str().map(String::from).unwrap_or_else(|| v.to_string());
                a.env.insert(k.clone(), v);
            }
        }
    };
    add_env(e.get("env"));
    if let Some(name) = env_name {
        add_env(e.get(format!("env_{name}").as_str()));
    }
    if let Some(i) = e.get("instances") {
        a.instances = match i {
            Value::Number(n) => match n.as_i64() {
                Some(0) => "max".into(),
                Some(-1) => "max-1".into(),
                Some(k) if k > 0 => k.to_string(),
                _ => "1".into(),
            },
            Value::String(s) => match s.trim() {
                "max" | "0" => "max".into(),
                "-1" => "max-1".into(),
                s => s.parse::<usize>().map(|n| n.to_string()).unwrap_or_else(|_| "1".into()),
            },
            _ => "1".into(),
        };
    }
    Ok(a)
}

/// Fields that mean the same in every source.
fn common(e: &Value) -> Pm2App {
    let mut a = Pm2App {
        name: s(e, "name").unwrap_or_default(),
        args: list(e, "args"),
        interpreter: s(e, "exec_interpreter").or_else(|| s(e, "interpreter")),
        interpreter_args: {
            let mut v = list(e, "node_args");
            v.extend(list(e, "interpreter_args"));
            v
        },
        instances: "1".into(),
        namespace: s(e, "namespace").filter(|n| n != "default"),
        max_memory_mb: e.get("max_memory_restart").and_then(memory_mb),
        cron: s(e, "cron_restart").or_else(|| s(e, "cron")),
        kill_timeout_ms: n(e, "kill_timeout"),
        kill_signal: s(e, "kill_signal"),
        wait_ready: b(e, "wait_ready").unwrap_or(false),
        listen_timeout_ms: n(e, "listen_timeout"),
        min_uptime_ms: e.get("min_uptime").and_then(duration_ms),
        max_restarts: n(e, "max_restarts").map(|n| n as u32),
        restart_delay_ms: n(e, "restart_delay").filter(|d| *d > 0).or_else(|| n(e, "exp_backoff_restart_delay")),
        autorestart: b(e, "autorestart").unwrap_or(true),
        stop_exit_codes: match e.get("stop_exit_codes") {
            Some(Value::Array(a)) => a.iter().filter_map(|c| c.as_i64().map(|c| c as i32)).collect(),
            Some(Value::Number(c)) => c.as_i64().map(|c| vec![c as i32]).unwrap_or_default(),
            _ => Vec::new(),
        },
        log_file: s(e, "log_file").or_else(|| s(e, "log")).filter(|p| p != "true").map(PathBuf::from),
        merge_logs: b(e, "merge_logs").unwrap_or(false),
        time: b(e, "time").unwrap_or(false) || s(e, "log_date_format").is_some(),
        instance_var: s(e, "instance_var").filter(|v| v != "NODE_APP_INSTANCE"),
        port: n(e, "port").and_then(|p| u16::try_from(p).ok()),
        ..Default::default()
    };
    if b(e, "watch").unwrap_or(false) || matches!(e.get("watch"), Some(Value::Array(_)) | Some(Value::String(_))) {
        a.notes.push(("watch".into(), "unsupported: restart on deploy with `warden reload` instead".into()));
    }
    if b(e, "shutdown_with_message").unwrap_or(false) {
        a.notes.push((
            "shutdown_with_message".into(),
            "unsupported: Warden stops workers with a signal ([shutdown] signal)".into(),
        ));
    }
    if s(e, "increment_var").is_some() {
        a.notes.push((
            "increment_var".into(),
            "unsupported: use the worker index in `instance_var` (0..N-1) and add your base in the app".into(),
        ));
    }
    if s(e, "uid").is_some() || s(e, "gid").is_some() {
        a.notes.push(("uid/gid".into(), "unsupported: set User=/Group= in the systemd unit".into()));
    }
    if s(e, "exp_backoff_restart_delay").is_some() {
        a.notes.push((
            "exp_backoff_restart_delay".into(),
            "approximated: [restart] backoff_initial; Warden's backoff doubles per crash up to backoff_max".into(),
        ));
    }
    a
}

// ---------------------------------------------------------------- ports

fn port_from_args(args: &[String]) -> Option<u16> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if let Some(v) = a.strip_prefix("--port=") {
            return v.parse().ok();
        }
        if a == "--port" || a == "-p" {
            return it.next().and_then(|v| v.parse().ok());
        }
    }
    None
}

/// PORT in env, then --port in args, then what the processes listen on.
fn detect_port(a: &Pm2App) -> Option<u16> {
    if let Some(p) = a.env.get("PORT").and_then(|p| p.parse().ok()) {
        return Some(p);
    }
    if let Some(p) = port_from_args(&a.args) {
        return Some(p);
    }
    // Cluster mode: the daemon holds the listening socket, not the workers.
    let pids: Vec<u32> = if a.cluster { daemon_pid().into_iter().collect() } else { a.pids.clone() };
    let mut ports: Vec<u16> = pids.iter().flat_map(|p| listening_ports(*p)).collect();
    ports.sort_unstable();
    ports.dedup();
    // Several (the daemon serving several cluster apps): ambiguous, leave it.
    (ports.len() == 1).then(|| ports[0])
}

/// TCP ports a process listens on (its socket inodes against /proc/net/tcp*).
fn listening_ports(pid: u32) -> Vec<u16> {
    let inodes: Vec<String> = std::fs::read_dir(format!("/proc/{pid}/fd"))
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| std::fs::read_link(e.path()).ok())
                .filter_map(|l| {
                    let l = l.to_string_lossy().to_string();
                    l.strip_prefix("socket:[").and_then(|x| x.strip_suffix(']')).map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else { continue };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() > 9 && f[3] == "0A" && inodes.iter().any(|i| i == f[9]) {
                if let Some(p) = f[1].rsplit(':').next().and_then(|h| u16::from_str_radix(h, 16).ok()) {
                    out.push(p);
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------ generating

fn runtime_of(a: &Pm2App) -> &'static str {
    let i = a.interpreter.as_deref().unwrap_or("");
    let file = Path::new(i).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
    let ext = a.script.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
    if file.starts_with("bun") || (i.is_empty() && matches!(ext.as_str(), "ts" | "tsx")) {
        "bun"
    } else if file.starts_with("node") || (i.is_empty() && matches!(ext.as_str(), "js" | "mjs" | "cjs")) {
        "node"
    } else {
        "other"
    }
}

/// The Warden config (TOML) and env file for one app.
pub fn generate(a: &Pm2App, worker_mode: bool, env_file: &str) -> Result<(String, String), String> {
    let o = StartOpts {
        name: Some(a.name.clone()),
        instances: Some(a.instances.clone()),
        port: a.port,
        interpreter: match a.interpreter.as_deref() {
            None => None,
            Some(i) => {
                let file = Path::new(i).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
                // PM2 stores node's absolute path; keep bare names so upgrades work.
                if file == "node" || file == "bun" { Some(file) } else { Some(i.to_string()) }
            }
        },
        interpreter_args: a.interpreter_args.clone(),
        namespace: a.namespace.clone(),
        cwd: a.cwd.clone(),
        env: Vec::new(),
        max_memory_mb: a.max_memory_mb,
        script_args: a.args.clone(),
        autorestart: (!a.autorestart).then_some(false),
        kill_timeout_ms: a.kill_timeout_ms,
        // PM2 stops apps with SIGINT; many were written to handle only that.
        kill_signal: Some(a.kill_signal.clone().unwrap_or_else(|| "SIGINT".into())),
        restart_delay_ms: a.restart_delay_ms,
        max_restarts: a.max_restarts,
        cron: a.cron.clone(),
        stop_exit_codes: a.stop_exit_codes.clone(),
        wait_ready: a.wait_ready,
        listen_timeout_ms: a.listen_timeout_ms,
        time: a.time,
        out_file: a.out_file.clone(),
        err_file: a.err_file.clone(),
        log_file: a.log_file.clone(),
        merge_logs: a.merge_logs,
        ..Default::default()
    };
    if worker_mode && (!a.args.is_empty() || !a.interpreter_args.is_empty()) {
        return Err(format!(
            "app {:?}: worker mode runs the entry module without arguments; choose processes, or move the \
             arguments into env",
            a.name
        ));
    }
    let (_, text) = fleet::quick_config(&fleet::Launch::Script(a.script.clone()), &o)?;
    let mut out = String::new();
    for line in text.lines() {
        if line.starts_with("# Written by") {
            out += "# Written by `warden pm2-migrate` from PM2 app ";
            out += &format!("{:?}. Every setting: warden.example.toml\n", a.name);
            continue;
        }
        if worker_mode && line.starts_with("args = ") {
            out += &format!("entry = {}\n", toml_str(&a.script.display().to_string()));
            continue;
        }
        out += line;
        out += "\n";
        if line.starts_with("working_directory = ") {
            if !a.env.is_empty() {
                out += &format!("env_file = {}\n", toml_str(env_file));
            }
            if let Some(v) = &a.instance_var {
                out += &format!("instance_var = {}\n", toml_str(v));
            }
        }
        if line.starts_with("count = ") {
            if worker_mode {
                out += "mode = \"worker\"\n";
            }
            if let Some(ms) = a.min_uptime_ms {
                out += &format!("min_uptime = {ms}\n");
            }
        }
    }
    if worker_mode {
        // Worker mode runs the entry in Bun Workers under a bun host.
        out = out.replace("command = \"node\"\n", "command = \"bun\"\n");
    }
    Config::parse(&out).map_err(|e| format!("app {:?}: the generated config is invalid: {e}\n{out}", a.name))?;
    let mut env = String::from("# Environment for this app (secrets live here, not in the config). Mode 0600.\n");
    for (k, v) in &a.env {
        env += &format!("{k}={}\n", env_quote(v));
    }
    Ok((out, env))
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// A value for the env file, quoted when it needs to be.
fn env_quote(v: &str) -> String {
    let plain =
        !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@%+,=".contains(c)) && !v.contains(" #");
    if plain {
        return v.to_string();
    }
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => s += "\\\"",
            '\\' => s += "\\\\",
            '\n' => s += "\\n",
            '\t' => s += "\\t",
            '\r' => s += "\\r",
            c => s.push(c),
        }
    }
    s + "\""
}

// ------------------------------------------------------------------ report

fn report(apps: &[(Pm2App, bool)], source: &str) -> String {
    let mut r = format!("# PM2 → Warden migration\n\nSource: {source}\n\n");
    for (a, worker) in apps {
        r += &format!("## {}\n\n", a.name);
        r += &format!(
            "- Mode: {} ({} instance(s); PM2 ran {} mode)\n",
            if *worker { "worker threads" } else { "processes" },
            a.instances,
            if a.cluster { "cluster" } else { "fork" }
        );
        r += &format!(
            "- Port: {}\n",
            a.port
                .map(|p| p.to_string())
                .unwrap_or_else(|| "none found: a port-less app (ready after min_uptime)".into())
        );
        r += "- Stop signal: ";
        r += &match &a.kill_signal {
            Some(s) => format!("{s} (as configured in PM2)\n"),
            None => "SIGINT (PM2's default; Warden's own default is SIGTERM)\n".into(),
        };
        if a.kill_timeout_ms.is_none() {
            r += "- Grace period: approximated: PM2's default 1.6 s kill_timeout becomes Warden's 30 s drain window\n";
        }
        if a.cluster {
            r += "- Cluster mode: approximated: Warden runs N processes sharing the port with SO_REUSEPORT (Node 22.12+ via its shim) instead of a master process that hands out connections\n";
        }
        if a.cron.is_some() {
            r += "- cron_restart: mapped to [restart] schedule, a rolling restart through the health gates (PM2 restarts all at once)\n";
        }
        if a.max_memory_mb.is_some() {
            r += "- max_memory_restart: mapped to [limits] max_memory: the worker is replaced gracefully, not killed\n";
        }
        if !a.env.is_empty() {
            let names: Vec<&str> = a.env.keys().map(String::as_str).collect();
            r += &format!(
                "- Environment kept ({}): {} (values in {}.env, mode 0600)\n",
                names.len(),
                names.join(", "),
                a.name
            );
        }
        if !a.dropped_env.is_empty() {
            r += &format!(
                "- Environment left out ({}): inherited from the shell that ran `pm2 start` or set by PM2 itself. Review the names; add any the app needs to {}.env: {}\n",
                a.dropped_env.len(),
                a.name,
                a.dropped_env.join(", ")
            );
        }
        for (field, how) in &a.notes {
            r += &format!("- {field}: {how}\n");
        }
        r += "\n";
    }
    r += "Next: `warden check -c <app>.toml`, then `warden pm2-migrate --cutover <mode>` (or start the apps yourself and `pm2 stop` them), and when everything runs under Warden: `warden pm2-migrate --finalize`.\n";
    r
}

// -------------------------------------------------------------------- run

fn ask_mode(a: &Pm2App, o: &MigrateOpts) -> Result<bool, String> {
    if let Some(m) = &o.mode {
        return match m.as_str() {
            "process" | "processes" => Ok(false),
            "worker" | "workers" | "threads" if runtime_of(a) == "bun" => Ok(true),
            "worker" | "workers" | "threads" => {
                eprintln!(
                    "warden: {}: worker (thread) mode is for Bun apps; this one runs on {}, so it gets processes",
                    a.name,
                    runtime_of(a)
                );
                Ok(false)
            }
            m => Err(format!("--mode {m:?}: process or worker")),
        };
    }
    if runtime_of(a) != "bun" || o.yes || !crate::sys::isatty(0) {
        return Ok(false);
    }
    println!(
        "{}: run it as\n  1) {} processes (recommended: a crash takes down one worker; same throughput)\n  2) {} worker threads in one Bun process (~20% less memory idle, worse p99, one crash takes all down)",
        a.name, a.instances, a.instances
    );
    print!("choose 1 or 2 [1]: ");
    use std::io::Write as _;
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    Ok(answer.trim() == "2")
}

fn write_file(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("writing {}: {e}", path.display()))?;
    // An existing file keeps its old mode with create(); make it right.
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .map_err(|e| format!("chmod {}: {e}", path.display()))
}

pub async fn run(args: &Args, o: &MigrateOpts) -> i32 {
    match run_inner(args, o).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("warden: pm2-migrate: {e}");
            1
        }
    }
}

async fn run_inner(args: &Args, o: &MigrateOpts) -> Result<i32, String> {
    if o.finalize {
        return finalize(args, o).await;
    }
    let (apps, source) = load(o)?;
    if apps.is_empty() {
        println!("PM2 runs no apps ({source}); nothing to migrate.");
        return Ok(0);
    }
    let out_dir = o.out.clone().unwrap_or_else(fleet::config_dir);
    let mut done: Vec<(Pm2App, bool)> = Vec::new();
    let mut code = 0;
    for mut a in apps {
        if let Some(Cutover::NewPort(p)) = o.cutover {
            a.port = Some(p);
        }
        let worker = ask_mode(&a, o)?;
        let env_name = format!("{}.env", a.name);
        let (toml, env) = match generate(&a, worker, &env_name) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("warden: {e}");
                code = 1;
                continue;
            }
        };
        let cfg_path = out_dir.join(format!("{}.toml", a.name));
        if o.dry_run {
            println!("# ---- {}\n{toml}", cfg_path.display());
            if !a.env.is_empty() {
                println!(
                    "# ---- {} (values not shown)\n{}",
                    out_dir.join(&env_name).display(),
                    a.env.keys().cloned().collect::<Vec<_>>().join("\n")
                );
            }
        } else {
            if cfg_path.exists() && !o.overwrite {
                eprintln!("warden: {}: {} exists; left as it is (--overwrite replaces it)", a.name, cfg_path.display());
                code = 1;
                continue;
            }
            if !a.env.is_empty() {
                write_file(&out_dir.join(&env_name), &env, 0o600)?;
            }
            write_file(&cfg_path, &toml, 0o644)?;
            println!(
                "{}: wrote {}{}",
                a.name,
                cfg_path.display(),
                if a.env.is_empty() { String::new() } else { format!(" and {env_name}") }
            );
        }
        done.push((a, worker));
    }
    let md = report(&done, &source);
    if o.dry_run {
        println!("# ---- MIGRATION.md\n{md}");
        return Ok(code);
    }
    let report_path = out_dir.join("MIGRATION.md");
    write_file(&report_path, &md, 0o644)?;
    println!("report: {}", report_path.display());
    if let Some(mode) = o.cutover {
        for (a, _) in &done {
            let cfg = out_dir.join(format!("{}.toml", a.name));
            if cutover(args, a, &cfg, mode).await != 0 {
                code = 1;
            }
        }
    }
    Ok(code)
}

/// Arguments for a fleet command on one app, from this command's.
fn args_for(args: &Args, target: Option<&str>) -> Args {
    Args {
        command: Command::Help,
        target: target.map(String::from),
        config: None,
        socket: None,
        json: false,
        no_wait: args.no_wait,
        yes: true,
        table_only: false,
    }
}

async fn warden_start(args: &Args, cfg: &Path) -> i32 {
    fleet::start(&args_for(args, None), &cfg.display().to_string(), &StartOpts::default()).await
}

/// Undo a failed start: workers and supervisor go away, the files stay.
async fn warden_stop(args: &Args, name: &str) {
    let _ = fleet::act(&args_for(args, Some(name)), &Action::Shutdown).await;
}

/// One app from PM2 to Warden; PM2 keeps (or gets back) the app on failure.
async fn cutover(args: &Args, a: &Pm2App, cfg: &Path, mode: Cutover) -> i32 {
    let name = &a.name;
    println!(
        "{name}: cutover ({})",
        match mode {
            Cutover::Overlap => "overlap".to_string(),
            Cutover::SamePort => "same-port".to_string(),
            Cutover::NewPort(p) => format!("new-port:{p}"),
        }
    );
    match mode {
        Cutover::Overlap | Cutover::NewPort(_) => {
            if warden_start(args, cfg).await != 0 {
                warden_stop(args, name).await;
                eprintln!(
                    "warden: {name}: Warden's workers did not come up; PM2 still serves the app, nothing changed"
                );
                return 1;
            }
            if let Cutover::NewPort(p) = mode {
                println!(
                    "{name}: Warden serves it on port {p}. Point your proxy (nginx upstream, load balancer) at 127.0.0.1:{p}, reload it, then `pm2 stop {name}`."
                );
                return 0;
            }
            match pm2(&["stop", name]) {
                Ok(_) => {
                    println!("{name}: now served by Warden; PM2's copy is stopped (`pm2 start {name}` brings it back)");
                    0
                }
                Err(e) => {
                    eprintln!("warden: {name}: Warden is up, but stopping PM2's copy failed: {e}");
                    1
                }
            }
        }
        Cutover::SamePort => {
            let t0 = std::time::Instant::now();
            if let Err(e) = pm2(&["stop", name]) {
                eprintln!("warden: {name}: `pm2 stop` failed, nothing changed: {e}");
                return 1;
            }
            if warden_start(args, cfg).await == 0 {
                println!("{name}: now served by Warden ({} ms without a listener)", t0.elapsed().as_millis());
                return 0;
            }
            warden_stop(args, name).await;
            match pm2(&["start", name]) {
                Ok(_) => {
                    eprintln!("warden: {name}: Warden's workers did not come up; rolled back: PM2 serves it again")
                }
                Err(e) => eprintln!(
                    "warden: {name}: Warden failed AND restarting PM2's copy failed: {e}; run `pm2 start {name}`"
                ),
            }
            1
        }
    }
}

/// Remove the migrated apps from PM2 and make Warden's set the one that
/// comes back after a reboot.
async fn finalize(args: &Args, o: &MigrateOpts) -> Result<i32, String> {
    let (apps, _) = load(&MigrateOpts { from: Some("jlist".into()), ..o.clone() })?;
    let ctx = fleet::context(args);
    // Running under Warden = its workers are up, not just a supervisor.
    let serving: Vec<String> = fleet::statuses(&ctx.apps)
        .await
        .into_iter()
        .filter(|(_, st)| st.as_ref().is_ok_and(|st| !st.stopped && st.workers_ready > 0))
        .map(|(app, _)| app.name)
        .collect();
    let mut removed = Vec::new();
    for a in &apps {
        if !serving.contains(&a.name) {
            println!("{}: not running under Warden yet; left in PM2", a.name);
            continue;
        }
        pm2(&["delete", &a.name])?;
        removed.push(a.name.clone());
        println!("{}: removed from PM2", a.name);
    }
    if !removed.is_empty() {
        let _ = pm2(&["save", "--force"]);
    }
    let code = fleet::save(args).await;
    println!(
        "Next, so PM2 no longer starts at boot and Warden does: `pm2 unstartup` (it prints the exact command) and `sudo warden startup`."
    );
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn jlist_entries_group_by_name_and_drop_the_shell() {
        let base: BTreeMap<String, String> =
            [("PATH", "/usr/bin"), ("HOME", "/root"), ("AWS_SECRET_ACCESS_KEY", "shh")]
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .into();
        let entry = |pid: u64| {
            (
                json!({"name":"api","pm_exec_path":"/srv/api/server.js","pm_cwd":"/srv/api","exec_mode":"cluster_mode",
                       "exec_interpreter":"/usr/local/bin/node","node_args":["--max-old-space-size=512"],"args":["--verbose"],
                       "kill_timeout":1600,"max_restarts":16,"min_uptime":1000,"cron_restart":"0 3 * * *",
                       "max_memory_restart":314572800,"pm_out_log_path":"/root/.pm2/logs/api-out.log",
                       "env":{"PATH":"/usr/bin","HOME":"/root","AWS_SECRET_ACCESS_KEY":"shh","NODE_ENV":"production",
                              "DB_URL":"postgres://x","pm_id":"0","PM2_HOME":"/root/.pm2","PORT":"4101","_":"/usr/bin/pm2"}}),
                Some(pid as u32),
            )
        };
        let apps = group(vec![entry(10), entry(11)], Some(&base));
        assert_eq!(apps.len(), 1);
        let a = &apps[0];
        assert_eq!((a.instances.as_str(), a.cluster, a.pids.clone()), ("2", true, vec![10, 11]));
        assert_eq!(a.env.keys().collect::<Vec<_>>(), vec!["DB_URL", "NODE_ENV", "PORT"]);
        for gone in ["AWS_SECRET_ACCESS_KEY", "PATH", "HOME", "pm_id", "PM2_HOME", "_"] {
            assert!(a.dropped_env.iter().any(|d| d == gone), "{gone} should be dropped");
        }
        assert_eq!(a.kill_timeout_ms, None, "PM2's default is not carried over");
        assert_eq!((a.max_restarts, a.min_uptime_ms, a.max_memory_mb), (None, None, Some(300)));
        assert_eq!(detect_port(a), Some(4101));
    }

    #[test]
    fn generated_configs_parse_and_keep_secrets_out() {
        let dir = std::env::temp_dir().join(format!("warden-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.js");
        std::fs::write(&script, "").unwrap();
        let mut a = Pm2App {
            name: "api".into(),
            script: script.clone(),
            cwd: Some(dir.clone()),
            interpreter: Some("/usr/local/bin/node".into()),
            interpreter_args: vec!["--max-old-space-size=512".into()],
            args: vec!["--verbose".into()],
            instances: "4".into(),
            cluster: true,
            env: [("DB_URL".to_string(), "postgres://u:p w@h/db".to_string()), ("PORT".into(), "4101".into())].into(),
            cron: Some("0 3 * * *".into()),
            max_memory_mb: Some(300),
            autorestart: true,
            port: Some(4101),
            min_uptime_ms: Some(5000),
            instance_var: Some("INSTANCE_ID".into()),
            ..Default::default()
        };
        let (toml, env) = generate(&a, false, "api.env").unwrap();
        assert!(!toml.contains("postgres"), "no secret in the config: {toml}");
        assert!(env.contains("DB_URL=\"postgres://u:p w@h/db\""), "{env}");
        assert_eq!(crate::config::parse_env_file(&env).unwrap()["DB_URL"], "postgres://u:p w@h/db");
        let c = Config::parse(&toml).unwrap();
        assert_eq!(c.app.command, "node", "a bare interpreter name, not PM2's absolute path");
        assert_eq!(c.app.args, vec!["--max-old-space-size=512", &script.display().to_string(), "--verbose"]);
        assert_eq!(
            (c.workers.count, c.app.port, c.limits.max_memory, c.workers.min_uptime),
            (4, Some(4101), 300, 5000)
        );
        assert_eq!((c.shutdown.signal.as_str(), c.restart.schedule.as_deref()), ("SIGINT", Some("0 3 * * *")));
        assert_eq!(c.app.env_file.as_deref(), Some(Path::new("api.env")));
        assert_eq!(c.app.instance_var, "INSTANCE_ID");
        // Worker mode for a Bun app: the entry module, no arguments.
        a.interpreter = Some("bun".into());
        assert!(generate(&a, true, "api.env").unwrap_err().contains("without arguments"));
        a.args.clear();
        a.interpreter_args.clear();
        let (toml, _) = generate(&a, true, "api.env").unwrap();
        let c = Config::parse(&toml).unwrap();
        assert_eq!(
            (c.workers.mode, c.app.entry.as_deref()),
            (crate::config::Mode::Worker, Some(script.display().to_string().as_str()))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ecosystem_apps_and_env_blocks() {
        let dir = Path::new("/srv/app");
        let e = json!({"name":"web","script":"dist/main.js","cwd":"current","instances":"max","exec_mode":"cluster",
                       "env":{"NODE_ENV":"development","A":"1"},"env_production":{"NODE_ENV":"production"},
                       "watch":true,"kill_signal":"SIGTERM","kill_timeout":5000,"min_uptime":"10s","max_memory_restart":"1G"});
        let a = from_ecosystem(&e, dir, Some("production")).unwrap();
        assert_eq!(a.script, PathBuf::from("/srv/app/current/dist/main.js"));
        assert_eq!(a.env["NODE_ENV"], "production");
        assert_eq!(a.env["A"], "1");
        assert_eq!(
            (a.instances.as_str(), a.kill_timeout_ms, a.min_uptime_ms, a.max_memory_mb),
            ("max", Some(5000), Some(10_000), Some(1024))
        );
        assert!(a.notes.iter().any(|(f, h)| f == "watch" && h.starts_with("unsupported")));
        let e = json!({"script":"worker.js","instances":-1});
        let a = from_ecosystem(&e, dir, None).unwrap();
        assert_eq!((a.name.as_str(), a.instances.as_str()), ("worker", "max-1"));
        assert!(from_ecosystem(&json!({"name":"x"}), dir, None).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(port_from_args(&["--port".into(), "3000".into()]), Some(3000));
        assert_eq!(port_from_args(&["--port=8080".into()]), Some(8080));
        assert_eq!(port_from_args(&["-p".into(), "81".into()]), Some(81));
        assert_eq!(port_from_args(&["--verbose".into()]), None);
        assert_eq!(duration_ms(&json!("2m")), Some(120_000));
        assert_eq!(duration_ms(&json!(500)), Some(500));
        assert_eq!(memory_mb(&json!("300M")), Some(300));
        assert_eq!(env_quote("abc"), "abc");
        assert_eq!(env_quote("a b"), "\"a b\"");
        assert_eq!(env_quote("x\"y\\z\n"), "\"x\\\"y\\\\z\\n\"");
        assert_eq!(env_quote(""), "\"\"");
        assert_eq!(parse_cutover("new-port:4200"), Ok(Cutover::NewPort(4200)));
        assert!(parse_cutover("new-port:0").is_err());
        assert!(parse_cutover("fast").is_err());
    }
}

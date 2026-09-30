//! Command-line parsing and rendering. Hand-rolled to keep the dependency
//! list short. Commands and flags follow PM2's where the idea is the same
//! (`list`, `restart api`, `logs api --lines 100`, `save`, `startup`), so
//! PM2 users need no new habits; `fleet.rs` runs them.

use crate::config::Level;
use crate::control::{self, Request, Status};
use std::path::PathBuf;

pub const USAGE: &str = "\
warden - a fast, crash-safe supervisor for Bun and Node apps

USAGE:
    warden <COMMAND> [TARGET] [OPTIONS]

    TARGET is an app name, a namespace, `all`, or `app:N` for one worker.
    With -c <config>, commands act on that one app and TARGET may be a
    worker number.

APPS (familiar from PM2):
    start <app|config.toml|script>   Start an app. A script gets a config written for
                     it: `warden start server.js --name api -i 4 --port 3000`
    list             Every app and worker (also: ls, ps, status)  [--json]
    describe <app>   Config, paths, restart policy, workers, last rollout (also: show)
    restart <target> Replace the workers one at a time through the health gates (no
                     downtime); --hard stops them all, then starts them (like PM2)
    reload <target>  Like restart, but re-reads the config first (new code or settings);
                     a failure rolls back
    deploy <target>  Safest reload: preflight, canary with soak, then the rest, with
                     rollback (also: safe-reload)
    stop <target>    Stop the workers; the app stays listed (start brings it back)
    delete <target>  Stop the app for good and move its config to deleted/
    scale <app> <N>  Set the number of workers (N, +N or -N)
    logs [target]    Recent lines, then follow on a terminal
                     [--lines N] [--err|--out] [--events] [--nostream] [-f]
    flush [target]   Empty the log buffer and truncate log files
    env <app>        The app's environment (values hidden unless --show-secrets)
    reset <target>   Zero restart counters and retry FAILED workers now
    signal <SIG> <target>   Send a signal to the workers (SIGUSR2, USR2, 12)
    top              Live view of every app (also: monit)
    save             Remember the running apps, worker counts and stopped state
    resurrect        Start what `save` remembered
    startup          Install the systemd unit and enable saved apps at boot (root)
    unstartup        Disable them again
    kill [target]    Stop every app's supervisor (asks first on a terminal; --yes)

SUPERVISOR:
    start            With no app: run the supervisor in the foreground for -c
                     (what systemd runs; also: run)
    shutdown         Stop the workers and exit the supervisor
    config <app>     Effective config as JSON (values hidden unless --show-secrets)
    log-level [target] [debug|info|warn|error]   Show or change it at runtime
    check            Validate the config file and exit
    version          Print the version

OPTIONS:
    -c, --config <PATH>   One app's config [default: $WARDEN_CONFIG; else every app in
                          $WARDEN_HOME, /etc/warden (root) or ~/.config/warden]
    -s, --socket <PATH>   One app's control socket
        --json            JSON output for list / status / describe
        --no-wait         Return as soon as a reload/restart has started
    -h, --help            Show this help

START OPTIONS (script):
    --name <NAME>  -i, --instances <N|max>  --port <PORT>  --interpreter <bun|node|none>
    --namespace <NS>  --cwd <DIR>  --env KEY=VALUE  --max-memory-restart <300M>  -- <script args>

reload, deploy and restart app:N wait for the rollout, print its progress and
exit 1 if it failed (or 2 if Warden is unreachable), so they fit `ExecReload=`
and deploy scripts.
";

#[derive(Debug, PartialEq)]
pub enum Command {
    /// The supervisor in the foreground (`start` with no app, or `run`).
    Run,
    Check,
    Version,
    Help,
    /// Commands that talk to running supervisors.
    Act(Action),
    Start {
        what: String,
        opts: StartOpts,
    },
    Delete {
        target: String,
    },
    Save,
    Resurrect,
    Startup,
    Unstartup,
    Kill,
    Top,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    List,
    Describe,
    Stop,
    Shutdown,
    Restart { hard: bool },
    Reload { safe: bool },
    Reset,
    Flush,
    Scale(ScaleArg),
    Logs { lines: usize, follow: Option<bool>, events: bool, stream: Option<String> },
    LogLevel(Option<Level>),
    Env { show_secrets: bool },
    Config { show_secrets: bool },
    Signal(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScaleArg {
    To(usize),
    By(i64),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartOpts {
    pub name: Option<String>,
    pub instances: Option<String>,
    pub port: Option<u16>,
    pub interpreter: Option<String>,
    pub namespace: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub max_memory_mb: Option<u64>,
    pub script_args: Vec<String>,
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    pub target: Option<String>,
    /// Explicit config (`-c` or `$WARDEN_CONFIG`): single-app context.
    pub config: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub json: bool,
    pub no_wait: bool,
    pub yes: bool,
    /// `workers`: only the worker table.
    pub table_only: bool,
}

fn parse_level(s: &str) -> Option<Level> {
    match s {
        "debug" => Some(Level::Debug),
        "info" => Some(Level::Info),
        "warn" | "warning" => Some(Level::Warn),
        "error" => Some(Level::Error),
        _ => None,
    }
}

/// `300M`, `1G`, `512` (MB) → MB.
fn parse_mb(s: &str) -> Result<u64, String> {
    let t = s.trim().to_ascii_uppercase();
    let (num, mult) = match t.chars().last() {
        Some('K') => (&t[..t.len() - 1], 1.0 / 1024.0),
        Some('M') => (&t[..t.len() - 1], 1.0),
        Some('G') => (&t[..t.len() - 1], 1024.0),
        _ => (t.as_str(), 1.0),
    };
    let v: f64 = num.parse().map_err(|_| format!("--max-memory-restart {s:?}: expected e.g. 300M or 1G"))?;
    Ok((v * mult).ceil().max(1.0) as u64)
}

pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut config: Option<PathBuf> = None;
    let mut socket = None;
    let (mut json, mut no_wait, mut yes, mut show_secrets, mut hard) = (false, false, false, false, false);
    let mut lines: Option<usize> = None;
    let mut follow: Option<bool> = None;
    let mut worker: Option<String> = None;
    let mut events = false;
    let mut stream: Option<String> = None;
    let mut so = StartOpts::default();
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "--" => {
                so.script_args = it.by_ref().cloned().collect();
                break;
            }
            "-c" | "--config" => config = Some(value(a)?.into()),
            "-s" | "--socket" => socket = Some(value(a)?.into()),
            "-n" | "--lines" => lines = Some(value(a)?.parse().map_err(|_| format!("{a} expects a number"))?),
            "-f" | "--follow" => follow = Some(true),
            "--nostream" | "--no-stream" => follow = Some(false),
            "-w" | "--worker" => worker = Some(value(a)?),
            "--events" => events = true,
            "--err" | "--error" => stream = Some("stderr".into()),
            "--out" | "--output" => stream = Some("stdout".into()),
            "--json" => json = true,
            "--no-wait" => no_wait = true,
            "-y" | "--yes" => yes = true,
            "--hard" => hard = true,
            "--show-secrets" => show_secrets = true,
            "--name" => so.name = Some(value(a)?),
            "-i" | "--instances" => so.instances = Some(value(a)?),
            "--port" => so.port = Some(value(a)?.parse().map_err(|_| "--port expects a port number".to_string())?),
            "--interpreter" => so.interpreter = Some(value(a)?),
            "--namespace" => so.namespace = Some(value(a)?),
            "--cwd" => so.cwd = Some(value(a)?.into()),
            "--env" => {
                let kv = value(a)?;
                let (k, v) = kv.split_once('=').ok_or_else(|| format!("--env {kv:?}: expected KEY=VALUE"))?;
                so.env.push((k.to_string(), v.to_string()));
            }
            "--max-memory-restart" => so.max_memory_mb = Some(parse_mb(&value(a)?)?),
            "--watch" => {
                return Err(
                    "--watch is not supported: Warden is for production; restart on deploy with `warden reload`".into(),
                );
            }
            "-h" | "--help" => positional.insert(0, "help".into()),
            "-V" | "--version" => positional.insert(0, "version".into()),
            s if s.starts_with("--config=") => config = Some(s["--config=".len()..].into()),
            s if s.starts_with("--socket=") => socket = Some(s["--socket=".len()..].into()),
            s if s.starts_with("--lines=") => {
                lines = Some(s["--lines=".len()..].parse().map_err(|_| "--lines expects a number".to_string())?)
            }
            s if s.starts_with('-') && s.len() > 1 && s.parse::<i64>().is_err() => {
                return Err(format!("unknown option {s}"));
            }
            s => positional.push(s.to_string()),
        }
    }
    let config = config.or_else(|| std::env::var_os("WARDEN_CONFIG").filter(|v| !v.is_empty()).map(PathBuf::from));
    let mut pos = positional.into_iter();
    let cmd = pos.next().unwrap_or_else(|| "help".into());
    let rest: Vec<String> = pos.collect();
    let mut target = None;
    let mut table_only = false;
    let too_many = |max: usize| -> Result<(), String> {
        if rest.len() > max {
            Err(format!("unexpected argument {:?} (see `warden --help`)", rest[max]))
        } else {
            Ok(())
        }
    };
    let one = |rest: &[String]| rest.first().cloned();
    let command = match cmd.as_str() {
        "start" => {
            too_many(1)?;
            match one(&rest) {
                None => Command::Run,
                Some(what) => Command::Start { what, opts: so.clone() },
            }
        }
        "run" => {
            too_many(0)?;
            Command::Run
        }
        "check" => Command::Check,
        "version" => Command::Version,
        "help" => Command::Help,
        "list" | "ls" | "ps" | "l" | "status" | "jlist" | "prettylist" => {
            too_many(1)?;
            target = one(&rest);
            if cmd == "jlist" {
                json = true;
            }
            Command::Act(Action::List)
        }
        "workers" => {
            too_many(1)?;
            target = one(&rest);
            table_only = true;
            Command::Act(Action::List)
        }
        "describe" | "show" | "info" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Describe)
        }
        "reload" | "gracefulReload" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Reload { safe: false })
        }
        "safe-reload" | "deploy" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Reload { safe: true })
        }
        "restart" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Restart { hard })
        }
        "stop" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Stop)
        }
        "shutdown" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Shutdown)
        }
        "reset" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Reset)
        }
        "flush" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Flush)
        }
        "env" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Env { show_secrets })
        }
        "config" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Config { show_secrets })
        }
        "delete" | "del" | "rm" => {
            too_many(1)?;
            Command::Delete { target: one(&rest).ok_or("delete needs an app name (or `all`)")? }
        }
        "scale" => {
            too_many(2)?;
            let (t, n) = match rest.as_slice() {
                [n] => (None, n.clone()),
                [t, n] => (Some(t.clone()), n.clone()),
                _ => return Err("scale needs a number: `warden scale api 4` (or +2 / -1)".into()),
            };
            target = t;
            let arg = if n.starts_with('+') || n.starts_with('-') {
                ScaleArg::By(n.parse().map_err(|_| format!("scale expects a number, got {n:?}"))?)
            } else {
                ScaleArg::To(n.parse().map_err(|_| format!("scale expects a number, got {n:?}"))?)
            };
            Command::Act(Action::Scale(arg))
        }
        "logs" | "log" => {
            too_many(1)?;
            target = one(&rest);
            Command::Act(Action::Logs { lines: lines.unwrap_or(15), follow, events, stream: stream.clone() })
        }
        "log-level" => {
            too_many(2)?;
            let level = match rest.as_slice() {
                [] => None,
                [l] if parse_level(l).is_some() => parse_level(l),
                [t] => {
                    target = Some(t.clone());
                    None
                }
                [t, l] => {
                    target = Some(t.clone());
                    Some(
                        parse_level(l)
                            .ok_or_else(|| format!("unknown log level {l:?} (debug, info, warn or error)"))?,
                    )
                }
                _ => None,
            };
            if let [l] = rest.as_slice() {
                if parse_level(l).is_none() && l.chars().all(|c| c.is_ascii_lowercase()) && config.is_some() {
                    return Err(format!("unknown log level {l:?} (debug, info, warn or error)"));
                }
            }
            Command::Act(Action::LogLevel(level))
        }
        "signal" | "sendSignal" => {
            too_many(2)?;
            let sig = rest.first().cloned().ok_or("signal needs a signal name: `warden signal SIGUSR2 api`")?;
            target = rest.get(1).cloned();
            Command::Act(Action::Signal(sig))
        }
        "save" | "dump" => Command::Save,
        "resurrect" => Command::Resurrect,
        "startup" => Command::Startup,
        "unstartup" => Command::Unstartup,
        "kill" => {
            too_many(1)?;
            target = one(&rest);
            Command::Kill
        }
        "top" | "monit" => {
            too_many(1)?;
            target = one(&rest);
            Command::Top
        }
        other => return Err(format!("unknown command {other:?} (see `warden --help`)")),
    };
    if let Some(w) = worker {
        target = Some(format!("{}:{w}", target.unwrap_or_default()));
    }
    if matches!(command, Command::Check | Command::Version | Command::Help | Command::Save | Command::Resurrect)
        && !rest.is_empty()
    {
        return Err(format!("{cmd} takes no argument"));
    }
    Ok(Args { command, target, config, socket, json, no_wait, yes, table_only })
}

/// Follow a rollout until it finishes; exit code 0 = succeeded.
pub async fn wait_for_rollout(socket: &std::path::Path, seq: u64) -> i32 {
    let mut last = String::new();
    let mut sink = std::io::sink();
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let st = match control::call(socket, &Request::Status, &mut sink).await {
            Ok(Some(r)) => r.status,
            Ok(None) => None,
            Err(e) => {
                eprintln!("warden: lost contact while waiting: {e}");
                return 2;
            }
        };
        let Some(st) = st else { continue };
        if let Some(o) = st.last_rollout.as_ref().filter(|o| o.seq >= seq) {
            if o.ok {
                println!("{}", o.message);
                return 0;
            }
            eprintln!("warden: {}", o.message);
            return 1;
        }
        if let Some(r) = st.rollout.as_ref().filter(|r| r.seq == seq) {
            if r.phase != last {
                println!("  [{}/{}] {}", r.done, r.total, r.phase);
                last = r.phase.clone();
            }
        }
    }
}

pub fn render_status(s: &Status, table_only: bool) -> String {
    let mut o = String::new();
    if !table_only {
        o += &format!("Application: {}\n", s.app);
        o += &format!("Mode:        {}\n", s.mode);
        o += &format!("Workers:     {}\n", s.workers_configured);
        o += &format!("Ready:       {}\n", s.workers_ready);
        o += &format!("PID:         {}\n", s.pid);
        o += &format!("Uptime:      {}\n", duration(s.uptime_secs));
        if let Some(r) = s.supervisor_rss_bytes {
            o += &format!("Memory:      {} (supervisor)\n", bytes(r));
        }
        if let Some(h) = &s.host {
            o += &format!(
                "Host:        pid {}, up {}, {} RSS, {} CPU, {} restarts\n",
                h.pid,
                duration(h.uptime_secs),
                h.rss_bytes.map(bytes).unwrap_or_else(|| "-".into()),
                h.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into()),
                h.restarts
            );
        }
        if let Some(h) = s.healthy {
            o += &format!("Health:      {}\n", if h { "healthy" } else { "UNHEALTHY" });
        }
        if let Some(r) = &s.rollout {
            o += &format!("Rollout:     {} {}/{}: {} ({}s)\n", r.kind, r.done, r.total, r.phase, r.elapsed_secs);
        }
        if let Some(r) = &s.last_rollout {
            o += &format!("Last:        {} {} - {}\n", r.kind, if r.ok { "ok" } else { "FAILED" }, r.message);
        }
        if s.stopped {
            o += "State:       stopped (`warden start` starts the workers)\n";
        }
        if s.shutting_down {
            o += "State:       shutting down\n";
        }
        o += "\n";
    }
    o += &format!(
        "{:<8} {:<11} {:<8} {:<8} {:<9} {:<10} {:<7} {:<8} {}\n",
        "Worker", "Status", "PID", "Uptime", "Restarts", "RSS", "CPU", "Health", "Last exit"
    );
    for w in &s.workers {
        o += &format!(
            "{:<8} {:<11} {:<8} {:<8} {:<9} {:<10} {:<7} {:<8} {}\n",
            w.id,
            w.state,
            w.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
            w.uptime_secs.map(duration).unwrap_or_else(|| "-".into()),
            w.restarts,
            w.rss_bytes.map(bytes).unwrap_or_else(|| "-".into()),
            w.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into()),
            health_word(w.healthy),
            w.last_exit.as_deref().unwrap_or("-"),
        );
    }
    o
}

fn health_word(h: Option<bool>) -> &'static str {
    match h {
        Some(true) => "ok",
        Some(false) => "FAILING",
        None => "-",
    }
}

/// Aligned columns; the last column is not padded.
fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let widths: Vec<usize> =
        (0..cols).map(|c| rows.iter().filter_map(|r| r.get(c)).map(|s| s.chars().count()).max().unwrap_or(0)).collect();
    let mut o = String::new();
    for r in rows {
        let last = r.len().saturating_sub(1);
        for (i, cell) in r.iter().enumerate() {
            if i == last {
                o += cell;
            } else {
                o += &format!("{cell:<w$}  ", w = widths[i]);
            }
        }
        o += "\n";
    }
    o
}

/// `warden list`: one row per worker, like `pm2 list`.
pub fn render_list(all: &[(crate::fleet::App, Result<Status, String>)]) -> String {
    let mut rows = vec![
        ["App", "Namespace", "Worker", "Status", "PID", "Uptime", "Restarts", "CPU", "Memory", "Health", "Last exit"]
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>(),
    ];
    let mut notes = Vec::new();
    for (app, st) in all {
        match st {
            Ok(s) => {
                for w in &s.workers {
                    let state = if s.stopped && w.state == "STOPPED" { "stopped".to_string() } else { w.state.clone() };
                    rows.push(vec![
                        app.name.clone(),
                        s.namespace.clone(),
                        w.id.to_string(),
                        state,
                        w.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                        w.uptime_secs.map(duration).unwrap_or_else(|| "-".into()),
                        w.restarts.to_string(),
                        w.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into()),
                        w.rss_bytes.map(bytes).unwrap_or_else(|| "-".into()),
                        health_word(w.healthy).into(),
                        w.last_exit.clone().unwrap_or_else(|| "-".into()),
                    ]);
                }
                if let Some(r) = &s.rollout {
                    notes.push(format!("{}: {} in progress, {}/{}: {}", app.name, r.kind, r.done, r.total, r.phase));
                }
                if s.health_suspended {
                    notes.push(format!(
                        "{}: most workers fail health checks; replacements held (dependency outage?)",
                        app.name
                    ));
                }
            }
            Err(e) => {
                let what = if e == "not running" { "offline" } else { "unreachable" };
                rows.push(vec![
                    app.name.clone(),
                    app.namespace.clone(),
                    "-".into(),
                    what.into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                    "-".into(),
                ]);
                if what == "offline" {
                    notes.push(format!("{}: not running; `warden start {}` starts it", app.name, app.name));
                } else {
                    notes.push(format!("{}: {e}", app.name));
                }
            }
        }
        if let Some(p) = &app.problem {
            notes.push(format!("{}: config problem: {p}", app.name));
        }
    }
    if all.is_empty() {
        return "no apps yet: `warden start server.js --name api`, or `warden pm2-migrate` to bring PM2's over\n"
            .into();
    }
    let mut o = table(&rows);
    for n in notes {
        o += &format!("  {n}\n");
    }
    o
}

/// `warden describe api`.
pub fn render_describe(s: &Status, info: &serde_json::Value) -> String {
    let c = |p: &str| info.pointer(&format!("/config{p}")).cloned().unwrap_or(serde_json::Value::Null);
    let text = |v: serde_json::Value| match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => "-".into(),
        v => v.to_string(),
    };
    let num = |p: &str| c(p).as_u64().unwrap_or(0);
    let state = if s.stopped {
        "stopped".to_string()
    } else if s.shutting_down {
        "shutting down".to_string()
    } else {
        format!("online, {}/{} workers ready", s.workers_ready, s.workers_configured)
    };
    let mut o = format!("{} (namespace {}): {state}\n\n", s.app, s.namespace);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row = |k: &str, v: String| rows.push(vec![format!("  {k}"), v]);
    row("config", text(info["config_path"].clone()));
    let args: Vec<String> =
        c("/app/args").as_array().map(|a| a.iter().map(|x| text(x.clone())).collect()).unwrap_or_default();
    row("command", format!("{} {}", text(c("/app/command")), args.join(" ")).trim().to_string());
    row("cwd", text(c("/app/working_directory")));
    row(
        "port",
        match c("/app/port").as_u64() {
            Some(p) => format!("{p} ({})", text(c("/workers/port_strategy"))),
            None => "none (not an HTTP app)".into(),
        },
    );
    row("mode", format!("{}, {} worker(s)", s.mode, s.workers_configured));
    row(
        "restart",
        if c("/restart/enabled").as_bool() == Some(false) {
            "disabled".into()
        } else {
            format!(
                "at once, then backoff {} ms up to {} ms; FAILED after {} restarts in {} s, retried after {} s",
                num("/restart/backoff_initial"),
                num("/restart/backoff_max"),
                num("/restart/max_restarts"),
                num("/restart/restart_window"),
                num("/restart/failed_cooldown")
            )
        },
    );
    let health = match c("/health/path") {
        serde_json::Value::String(p) if !p.is_empty() => format!(
            "{p} every {} s, unhealthy after {} failures; reload gates: {} passes",
            num("/health/interval"),
            num("/health/failure_threshold"),
            num("/reload/health_passes")
        ),
        _ => "no health path: new workers are gated on listening only".into(),
    };
    row("health", health);
    row("shutdown", format!("grace {} s, drain {} ms", num("/shutdown/grace_period"), num("/shutdown/drain_ms")));
    let mem = num("/limits/max_memory");
    let life = num("/limits/max_lifetime");
    row(
        "limits",
        format!(
            "max memory {}, max lifetime {}",
            if mem > 0 { format!("{mem} MB") } else { "-".into() },
            if life > 0 { duration(life) } else { "-".into() }
        ),
    );
    row(
        "logs",
        match &s.log_file {
            Some(f) => format!("{f} (+ `warden logs {}`)", s.app),
            None => format!("stdout / journald (+ `warden logs {}`)", s.app),
        },
    );
    row("socket", text(info["socket"].clone()));
    row(
        "supervisor",
        format!(
            "pid {}, up {}, {}{}, warden {}",
            s.pid,
            duration(s.uptime_secs),
            s.supervisor_rss_bytes.map(bytes).unwrap_or_else(|| "-".into()),
            s.unit.as_ref().map(|u| format!(", unit {u}")).unwrap_or_default(),
            s.version
        ),
    );
    if let Some(env) = c("/app/env").as_object() {
        let items: Vec<String> = env.iter().map(|(k, v)| format!("{k}={}", text(v.clone()))).collect();
        row("env", if items.is_empty() { "-".into() } else { items.join(", ") });
    }
    if let Some(r) = &s.last_rollout {
        row("last rollout", format!("{} {}: {}", r.kind, if r.ok { "ok" } else { "FAILED" }, r.message));
    }
    o += &table(&rows);
    o += "\n";
    o += &render_status(s, true);
    o
}

pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s:02}s"),
        (0, _, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h:02}h"),
    }
}

pub fn bytes(b: u64) -> String {
    let mb = b as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.1} MB") }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Result<Args, String> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    fn act(s: &str) -> (Action, Option<String>) {
        let a = p(s).unwrap();
        match a.command {
            Command::Act(x) => (x, a.target),
            other => panic!("{s}: {other:?}"),
        }
    }

    #[test]
    fn commands() {
        assert_eq!(p("start -c /etc/w.toml").unwrap().command, Command::Run);
        assert_eq!(p("start -c /etc/w.toml").unwrap().config, Some(PathBuf::from("/etc/w.toml")));
        assert_eq!(act("restart 2"), (Action::Restart { hard: false }, Some("2".into())));
        assert_eq!(act("restart api:2"), (Action::Restart { hard: false }, Some("api:2".into())));
        assert_eq!(act("restart"), (Action::Restart { hard: false }, None));
        assert_eq!(act("restart api --hard"), (Action::Restart { hard: true }, Some("api".into())));
        assert_eq!(act("scale 8"), (Action::Scale(ScaleArg::To(8)), None));
        assert_eq!(act("scale api +2"), (Action::Scale(ScaleArg::By(2)), Some("api".into())));
        assert_eq!(act("scale api -1"), (Action::Scale(ScaleArg::By(-1)), Some("api".into())));
        assert_eq!(
            act("logs -n 5 -f"),
            (Action::Logs { lines: 5, follow: Some(true), events: false, stream: None }, None)
        );
        assert_eq!(
            act("logs --worker 2 --events --nostream"),
            (Action::Logs { lines: 15, follow: Some(false), events: true, stream: None }, Some(":2".into()))
        );
        assert_eq!(
            act("logs api --lines 100 --err"),
            (
                Action::Logs { lines: 100, follow: None, events: false, stream: Some("stderr".into()) },
                Some("api".into())
            )
        );
        assert_eq!(act("log-level"), (Action::LogLevel(None), None));
        assert_eq!(act("log-level debug"), (Action::LogLevel(Some(Level::Debug)), None));
        assert_eq!(act("log-level api warn"), (Action::LogLevel(Some(Level::Warn)), Some("api".into())));
        assert!(p("log-level -c x.toml loud").is_err());
        assert!(p("scale").is_err());
        assert!(p("scale x").is_err());
        assert!(p("status a b").is_err());
        assert!(p("frobnicate").is_err());
        assert!(p("status --bogus").is_err());
        assert_eq!(p("").unwrap().command, Command::Help);
        assert!(p("status --json").unwrap().json);
        assert!(p("jlist").unwrap().json);
        assert_eq!(act("safe-reload"), (Action::Reload { safe: true }, None));
        assert_eq!(act("deploy api"), (Action::Reload { safe: true }, Some("api".into())));
        assert!(p("reload --no-wait").unwrap().no_wait);
        assert_eq!(act("ls"), (Action::List, None));
        assert_eq!(act("signal SIGUSR2 api"), (Action::Signal("SIGUSR2".into()), Some("api".into())));
        assert_eq!(p("delete api").unwrap().command, Command::Delete { target: "api".into() });
        assert!(p("delete").is_err());
        assert!(p("start app.js --watch").is_err());
    }

    #[test]
    fn start_options() {
        let a = p("start server.js --name api -i max --port 3000 --env A=1 --max-memory-restart 1G -- --x y").unwrap();
        let Command::Start { what, opts } = a.command else { panic!() };
        assert_eq!(what, "server.js");
        assert_eq!(opts.name.as_deref(), Some("api"));
        assert_eq!(opts.instances.as_deref(), Some("max"));
        assert_eq!(opts.port, Some(3000));
        assert_eq!(opts.env, vec![("A".to_string(), "1".to_string())]);
        assert_eq!(opts.max_memory_mb, Some(1024));
        assert_eq!(opts.script_args, vec!["--x".to_string(), "y".to_string()]);
        assert_eq!(parse_mb("300M"), Ok(300));
        assert_eq!(parse_mb("512"), Ok(512));
        assert!(parse_mb("lots").is_err());
    }

    #[test]
    fn formatting() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(125), "2m05s");
        assert_eq!(duration(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(duration(90_000), "1d01h");
        assert_eq!(bytes(64 * 1024 * 1024), "64.0 MB");
        assert_eq!(table(&[vec!["a".into(), "bb".into()], vec!["ccc".into(), "d".into()]]), "a    bb\nccc  d\n");
    }
}

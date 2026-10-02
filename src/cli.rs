//! Command-line parsing and rendering. Hand-rolled to keep the dependency
//! list short. Commands and flags follow PM2's where the idea is the same
//! (`list`, `restart api`, `logs api --lines 100`, `save`, `startup`), so
//! PM2 users need no new habits; `fleet.rs` runs them.

use crate::config::Level;
use crate::control::{self, Request, Status};
use crate::table::{Cell, DIM, Fmt, GREEN, KEY, NAME, RED, YELLOW, boxed, clip};
use std::path::PathBuf;

pub mod ports;

pub const USAGE: &str = "\
warden - a fast, crash-safe supervisor for Bun and Node apps

USAGE:
    warden <COMMAND> [TARGET] [OPTIONS]

    TARGET is an app name, its id (the first column of `warden list`), a
    namespace, `all`, or `app:N` for one worker. Several at once, with commas
    or spaces; ids can be ranges: `warden start 0,1,2`, `warden stop 0-3`,
    `warden restart api web:2`. With -c <config>, commands act on that one app
    and TARGET may be a worker number (`0,1`, `0-2`).

APPS (familiar from PM2):
    start <app|id|config.toml|script>   Start an app. A script gets a config written for
                     it: `warden start server.js --name api -i 4 --port 3000`.
                     Waits for it; if every worker crashes first, exits 1 with the
                     app's errors and leaves it stopped (errored)
    list             Every app and worker as a table with ids (also: ls, ps, status)  [--json]
    describe <app>   Config, paths, restart policy, workers, last rollout (also: show)
    ports [app]      The ports and unix sockets each app listens on, read from the OS, with where they
                     are reachable (all interfaces / localhost only) and a URL  [--json]
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
                     --worker N: one worker; s1, s2…: one hot standby; standby: all
                     --history: from the log files (rotated and .gz too, every
                     worker's with per_worker_files) or journald; --lines N there
                     is the last N of each file
                     --grep TEXT --exclude TEXT --ignore-case --since 2h --until
                     2026-09-30T12:00 --level warn --json (one object per line)
    search <text> [target]   Search all of an app's logs (= logs --history --grep)
    flush [target]   Empty the log buffer and the current log files ([logging] file, out
                     and err files, every worker's); rotated files (.1, .gz) are kept
    env <app>        The environment a worker starts with: env_file, env, then Warden's
                     variables (app:N for worker N; values hidden unless --show-secrets)
    reset <target>   Zero restart counters and retry FAILED workers now
    signal <SIG> <target>   Send a signal to the workers (SIGUSR2, USR2, 12)
    top              Live view of every app (also: monit)
    serve [dir] [port]   Serve static files (default: . on 8080) with Warden's built-in
                     server, like `pm2 serve`: --name --spa --listing -i N
                     --basic-auth user:pass (or --basic-auth-username/-password)
    save             Remember the running apps, worker counts and stopped state
    resurrect        Start what `save` remembered
    startup          Bring the saved apps and wardend back after a reboot or a crash:
                     systemd units (root: system units; a user or --user: your own,
                     with lingering), a launchd job on macOS. Without a service
                     manager it says what to run at boot instead  [--user|--system]
    unstartup        Remove what `startup` installed (apps keep running)  [--user|--system]
    kill [target]    Stop every app's supervisor (asks first on a terminal; --yes)
    pm2-migrate      Import PM2's apps: a config, a 0600 .env file and a MIGRATION.md
                     report per app. --from jlist|dump|<ecosystem file>  --env <name>
                     --apps a,b  --out <dir>  --dry-run  --mode process|worker
                     --overwrite  --cutover overlap|same-port|new-port:<port>
                     (switch with rollback)  --finalize (remove them from PM2)

WARDEND (optional daemon: restarts background supervisors that die, one socket
for live events; apps never depend on it and keep running without it):
    daemon           Run wardend in the foreground; --background detaches it (log in
                     the state directory: logs/wardend.log). `warden start` starts it
                     in the background for you unless WARDEN_NO_DAEMON=1.
                     --resurrect: first start the apps `warden save` recorded (a
                     container entrypoint; what launchd runs on macOS)
    daemon status    wardend's pid and every app it watches  [--json]; exit 1 when
                     it is not running
    daemon stop      Stop wardend; every app keeps running (`warden kill` stops it too)
    daemon check     Validate the alert rules in <config dir>/wardend.toml (or -c FILE)
    daemon reload    Make the running wardend read wardend.toml again (as SIGHUP does);
                     on an error it keeps the rules it had
    events [target]  Live events, one line each: workers, rollouts, supervisors
                     [--json] (NDJSON)  [--logs] (log lines too)  [--interval MS]
                     From wardend when it runs, else from the apps' sockets

SUPERVISOR:
    start            With no app: run the supervisor in the foreground for -c
                     (what systemd runs; also: run)
    shutdown         Stop the workers and exit the supervisor
    config <app>     Effective config as JSON (values hidden unless --show-secrets)
    log-level [target] [debug|info|warn|error]   Show or change it at runtime
    check            Validate the config file and exit
    doctor           Check this host for the problems Warden knows about, with a fix
                     for each  [--json]
    version          Print the version

OPTIONS:
    -c, --config <PATH>   One app's config [default: $WARDEN_CONFIG; else every app in
                          $WARDEN_HOME, /etc/warden (root) or ~/.config/warden]
    -s, --socket <PATH>   One app's control socket
        --json            JSON output for list / status / describe
        --no-wait         Return as soon as a start/reload/restart has begun, without
                          waiting for workers to be ready
    -h, --help            Show this help

START OPTIONS (a script, a program or a command line, as with PM2):
    warden start server.js --name api -i 4 --port 3000
    warden start worker.py --name queue                 (interpreter picked by extension)
    warden start ./bin/server --name go-api -- --flag   (any executable)
    warden start npm --name web -- start                (a program on PATH)
    warden start \"python3 -m http.server 8000\" --name files   (a command line)
    --name <NAME>  -i, --instances <N|max|max-1>  --port <PORT>  --namespace <NS>  --cwd <DIR>
    --interpreter <bun|node|python3|bash|none|...>  --interpreter-args \"<args>\" (also --node-args)
    --env KEY=VALUE  --max-memory-restart <300M>  --cron \"<m h dom mon dow>\"  --no-autorestart
    --kill-signal SIGINT  --kill-timeout <ms>  --restart-delay <ms>  --max-restarts <N>
    --stop-exit-codes 0,1  --wait-ready  --listen-timeout <ms>  --no-shim
    -o, --output <file>  -e, --error <file>  -l, --log <file>  --time  --merge-logs
    -- <args for the app>

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
        opts: Box<StartOpts>,
    },
    /// `warden serve <dir> [port]`, like `pm2 serve`.
    Serve {
        dir: PathBuf,
        port: u16,
        opts: Box<StartOpts>,
    },
    Delete {
        target: String,
    },
    Save,
    Resurrect,
    Startup(crate::startup::Want),
    Unstartup(crate::startup::Want),
    Kill,
    Top,
    Doctor,
    Pm2Migrate(Box<crate::migrate::MigrateOpts>),
    /// wardend: run it, or ask the running one.
    Daemon(DaemonCmd),
    /// `warden events [target]`: live events.
    Events {
        logs: bool,
        interval_ms: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DaemonCmd {
    /// `warden daemon [--background] [--resurrect]`.
    Run {
        background: bool,
        resurrect: bool,
    },
    Status,
    Stop,
    /// `warden daemon check [-c FILE]`: validate wardend.toml.
    Check,
    /// `warden daemon reload`: the running wardend reads it again.
    Reload,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    List,
    Describe,
    /// The sockets the apps listen on (`warden ports`).
    Ports,
    Stop,
    Shutdown,
    Restart {
        hard: bool,
    },
    Reload {
        safe: bool,
    },
    Reset,
    Flush,
    Scale(ScaleArg),
    Logs {
        /// Last N lines; `None`: 15 from memory, everything from history.
        lines: Option<usize>,
        follow: Option<bool>,
        /// Read log files / journald instead of the in-memory buffer.
        history: bool,
        query: crate::logview::Query,
    },
    LogLevel(Option<Level>),
    Env {
        show_secrets: bool,
    },
    Config {
        show_secrets: bool,
    },
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
    pub interpreter_args: Vec<String>,
    pub namespace: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub max_memory_mb: Option<u64>,
    pub script_args: Vec<String>,
    pub autorestart: Option<bool>,
    pub kill_timeout_ms: Option<u64>,
    pub kill_signal: Option<String>,
    pub restart_delay_ms: Option<u64>,
    pub max_restarts: Option<u32>,
    pub cron: Option<String>,
    pub stop_exit_codes: Vec<i32>,
    pub wait_ready: bool,
    pub listen_timeout_ms: Option<u64>,
    pub time: bool,
    pub out_file: Option<PathBuf>,
    pub err_file: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub merge_logs: bool,
    pub shim: Option<bool>,
    /// `serve`: single-page app fallback, directory listing, Basic auth.
    pub spa: bool,
    pub listing: bool,
    pub basic_auth: Option<String>,
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

fn num(flag: &str, v: &str) -> Result<u64, String> {
    v.trim().parse().map_err(|_| format!("{flag} expects a number, got {v:?}"))
}

/// `300M`, `1G`, `512` (MB) → MB.
pub fn parse_mb(s: &str) -> Result<u64, String> {
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
    // Its options (--env NAME, --out DIR) differ from `start`'s.
    if argv.first().map(String::as_str) == Some("pm2-migrate") {
        return parse_migrate(&argv[1..]);
    }
    let mut config: Option<PathBuf> = None;
    let mut socket = None;
    let (mut json, mut no_wait, mut yes, mut show_secrets, mut hard) = (false, false, false, false, false);
    let mut lines: Option<usize> = None;
    let mut follow: Option<bool> = None;
    let mut worker: Option<String> = None;
    let mut q = crate::logview::Query::default();
    let mut history = false;
    let (mut background, mut with_logs, mut resurrect) = (false, false, false);
    let mut scope = crate::startup::Want::Auto;
    let mut interval_ms: Option<u64> = None;
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
            "--events" => q.events = true,
            "--err" => q.stream = Some("stderr".into()),
            "--out" => q.stream = Some("stdout".into()),
            "--grep" | "--search" => q.grep.push(value(a)?),
            "--exclude" | "--grep-v" => q.exclude.push(value(a)?),
            "--ignore-case" => q.ignore_case = true,
            "--since" => q.since = Some(crate::logview::parse_time(&value(a)?)?),
            "--until" => q.until = Some(crate::logview::parse_time(&value(a)?)?),
            "--level" => {
                let l = value(a)?;
                q.level = Some(parse_level(&l).ok_or_else(|| format!("--level {l:?}: debug, info, warn or error"))?);
            }
            "--history" | "--files" | "--all" => history = true,
            "--background" => background = true,
            "--resurrect" => resurrect = true,
            "--user" => scope = crate::startup::Want::User,
            "--system" => scope = crate::startup::Want::System,
            "--logs" => with_logs = true,
            "--interval" => interval_ms = Some(num(a, &value(a)?)?),
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
            "--interpreter-args" | "--node-args" => {
                so.interpreter_args.extend(value(a)?.split_whitespace().map(String::from))
            }
            "--no-autorestart" => so.autorestart = Some(false),
            "--kill-timeout" => so.kill_timeout_ms = Some(num(a, &value(a)?)?),
            "--kill-signal" => so.kill_signal = Some(value(a)?),
            "--restart-delay" | "--exp-backoff-restart-delay" => so.restart_delay_ms = Some(num(a, &value(a)?)?),
            "--max-restarts" => so.max_restarts = Some(num(a, &value(a)?)? as u32),
            "--cron" | "--cron-restart" => so.cron = Some(value(a)?),
            "--stop-exit-codes" => {
                for c in value(a)?.split([',', ' ']).filter(|c| !c.is_empty()) {
                    so.stop_exit_codes
                        .push(c.parse().map_err(|_| format!("--stop-exit-codes: {c:?} is not a number"))?);
                }
            }
            "--wait-ready" => so.wait_ready = true,
            "--listen-timeout" => so.listen_timeout_ms = Some(num(a, &value(a)?)?),
            "--time" => so.time = true,
            "-o" | "--output" => so.out_file = Some(value(a)?.into()),
            "-e" | "--error" => so.err_file = Some(value(a)?.into()),
            "--error-file" | "--err-file" => so.err_file = Some(value(a)?.into()),
            "-l" | "--log" => so.log_file = Some(value(a)?.into()),
            "--merge-logs" => so.merge_logs = true,
            "--spa" => so.spa = true,
            "--listing" => so.listing = true,
            "--basic-auth" => so.basic_auth = Some(value(a)?),
            "--basic-auth-username" => {
                let u = value(a)?;
                let pass = so.basic_auth.take().and_then(|x| x.split_once(':').map(|(_, p)| p.to_string()));
                so.basic_auth = Some(format!("{u}:{}", pass.unwrap_or_default()));
            }
            "--basic-auth-password" => {
                let pw = value(a)?;
                let user = so.basic_auth.take().and_then(|x| x.split_once(':').map(|(u, _)| u.to_string()));
                so.basic_auth = Some(format!("{}:{pw}", user.unwrap_or_default()));
            }
            "--shim" => so.shim = Some(true),
            "--no-shim" => so.shim = Some(false),
            "--shutdown-with-message" => {
                return Err("--shutdown-with-message is not supported: Warden stops workers with a signal \
                            (--kill-signal)"
                    .into());
            }
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
    // Apps by id or name, as `warden stop 0 1` or `warden stop 0,1` (PM2 takes both).
    let many = |rest: &[String]| (!rest.is_empty()).then(|| rest.join(","));
    let command = match cmd.as_str() {
        "start" => {
            // Several words are apps (ids or names); a command line goes in quotes.
            if rest.len() > 1 && rest.iter().any(|w| w.contains(char::is_whitespace)) {
                return Err("start takes one script or command line, or several app names or ids: \
                            `warden start 0 1`, `warden start \"node server.js\" --name api`"
                    .into());
            }
            match many(&rest) {
                None => Command::Run,
                Some(what) => Command::Start { what, opts: Box::new(so.clone()) },
            }
        }
        "serve" => {
            too_many(2)?;
            let dir = PathBuf::from(rest.first().cloned().unwrap_or_else(|| ".".into()));
            let port = match rest.get(1) {
                Some(p) => p.parse().map_err(|_| format!("serve: {p:?} is not a port"))?,
                None => so.port.unwrap_or(8080),
            };
            Command::Serve { dir, port, opts: Box::new(so.clone()) }
        }
        "run" => {
            too_many(0)?;
            Command::Run
        }
        "check" => Command::Check,
        "version" => Command::Version,
        "help" => Command::Help,
        "list" | "ls" | "ps" | "l" | "status" | "jlist" | "prettylist" => {
            target = many(&rest);
            if cmd == "jlist" {
                json = true;
            }
            Command::Act(Action::List)
        }
        "workers" => {
            target = many(&rest);
            table_only = true;
            Command::Act(Action::List)
        }
        "ports" | "port" | "listening" => {
            target = many(&rest);
            Command::Act(Action::Ports)
        }
        "describe" | "show" | "info" => {
            target = many(&rest);
            Command::Act(Action::Describe)
        }
        "reload" | "gracefulReload" => {
            target = many(&rest);
            Command::Act(Action::Reload { safe: false })
        }
        "safe-reload" | "deploy" => {
            target = many(&rest);
            Command::Act(Action::Reload { safe: true })
        }
        "restart" => {
            target = many(&rest);
            Command::Act(Action::Restart { hard })
        }
        "stop" => {
            target = many(&rest);
            Command::Act(Action::Stop)
        }
        "shutdown" => {
            target = many(&rest);
            Command::Act(Action::Shutdown)
        }
        "reset" => {
            target = many(&rest);
            Command::Act(Action::Reset)
        }
        "flush" => {
            target = many(&rest);
            Command::Act(Action::Flush)
        }
        "env" => {
            target = many(&rest);
            Command::Act(Action::Env { show_secrets })
        }
        "config" => {
            target = many(&rest);
            Command::Act(Action::Config { show_secrets })
        }
        "delete" | "del" | "rm" => {
            Command::Delete { target: many(&rest).ok_or("delete needs an app name or id (or `all`)")? }
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
            target = many(&rest);
            Command::Act(Action::Logs { lines, follow, history, query: q.clone() })
        }
        "search" | "grep" => {
            too_many(2)?;
            let text = rest.first().cloned().ok_or("search needs the text to find: `warden search timeout api`")?;
            target = rest.get(1).cloned();
            let mut query = q.clone();
            query.grep.insert(0, text);
            Command::Act(Action::Logs { lines, follow: Some(false), history: true, query })
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
        "startup" => {
            too_many(0)?;
            Command::Startup(scope)
        }
        "unstartup" => {
            too_many(0)?;
            Command::Unstartup(scope)
        }
        "kill" => {
            target = many(&rest);
            Command::Kill
        }
        "doctor" => {
            too_many(0)?;
            Command::Doctor
        }
        "top" | "monit" => {
            target = many(&rest);
            Command::Top
        }
        "daemon" | "wardend" => {
            too_many(1)?;
            Command::Daemon(match one(&rest).as_deref() {
                None | Some("start") | Some("run") => DaemonCmd::Run { background, resurrect },
                Some("status") => DaemonCmd::Status,
                Some("stop") => DaemonCmd::Stop,
                Some("check") => DaemonCmd::Check,
                Some("reload") => DaemonCmd::Reload,
                Some(other) => {
                    return Err(format!("daemon {other:?}: expected `status`, `stop`, `check`, `reload` or nothing"));
                }
            })
        }
        "events" => {
            target = many(&rest);
            Command::Events { logs: with_logs, interval_ms }
        }
        other => return Err(format!("unknown command {other:?} (see `warden --help`)")),
    };
    if background && !matches!(command, Command::Daemon(DaemonCmd::Run { .. })) {
        return Err("--background only applies to `warden daemon` (`warden start` already runs apps in the \
                    background)"
            .into());
    }
    if resurrect && !matches!(command, Command::Daemon(DaemonCmd::Run { .. })) {
        return Err(
            "--resurrect only applies to `warden daemon` (`warden resurrect` starts the saved apps once)".into()
        );
    }
    if scope != crate::startup::Want::Auto && !matches!(command, Command::Startup(_) | Command::Unstartup(_)) {
        return Err("--user and --system only apply to `warden startup` and `warden unstartup`".into());
    }
    if let Some(w) = worker {
        let t = target.unwrap_or_default();
        if t.contains(',') {
            return Err("--worker takes one app: `warden restart api --worker 2` (or `api:2`)".into());
        }
        target = Some(format!("{t}:{w}"));
    }
    if matches!(command, Command::Check | Command::Version | Command::Help | Command::Save | Command::Resurrect)
        && !rest.is_empty()
    {
        return Err(format!("{cmd} takes no argument"));
    }
    Ok(Args { command, target, config, socket, json, no_wait, yes, table_only })
}

fn parse_migrate(argv: &[String]) -> Result<Args, String> {
    let mut o = crate::migrate::MigrateOpts::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut value = || it.next().cloned().ok_or_else(|| format!("pm2-migrate: {a} needs a value"));
        match a.as_str() {
            "--from" => o.from = Some(value()?),
            "--env" => o.env = Some(value()?),
            "--apps" | "--only" => {
                o.apps.extend(value()?.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from))
            }
            "--out" => o.out = Some(value()?.into()),
            "--dry-run" => o.dry_run = true,
            "--mode" => o.mode = Some(value()?),
            "--cutover" => o.cutover = Some(crate::migrate::parse_cutover(&value()?)?),
            "--finalize" => o.finalize = true,
            "--overwrite" => o.overwrite = true,
            "-y" | "--yes" => o.yes = true,
            "-h" | "--help" => return Ok(Args { command: Command::Help, ..empty_args() }),
            s if s.starts_with('-') => return Err(format!("pm2-migrate: unknown option {s}")),
            name => o.apps.push(name.to_string()),
        }
    }
    if o.dry_run && (o.cutover.is_some() || o.finalize) {
        return Err(
            "pm2-migrate: --dry-run changes nothing, so it can't be combined with --cutover or --finalize".into()
        );
    }
    let yes = o.yes;
    Ok(Args { command: Command::Pm2Migrate(Box::new(o)), yes, ..empty_args() })
}

fn empty_args() -> Args {
    Args {
        command: Command::Help,
        target: None,
        config: None,
        socket: None,
        json: false,
        no_wait: false,
        yes: false,
        table_only: false,
    }
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

/// The app's state as one word, and how to paint it: what `warden list` shows
/// per worker, for the app as a whole.
fn app_status(s: &Status) -> (String, &'static str) {
    if s.shutting_down {
        ("shutting down".into(), YELLOW)
    } else if s.stopped {
        if s.start_failed.is_some() { ("errored".into(), RED) } else { ("stopped".into(), YELLOW) }
    } else if s.start_failed.is_some() {
        ("not started".into(), RED)
    } else if s.workers_ready >= s.workers_configured {
        ("online".into(), GREEN)
    } else {
        (format!("{}/{} workers ready", s.workers_ready, s.workers_configured), YELLOW)
    }
}

/// What to do about the state, when there is something to say.
fn status_note(s: &Status) -> Option<String> {
    if let Some(n) = start_failed_note(s) {
        Some(n)
    } else if s.stopped {
        Some("stopped (`warden start` starts the workers)".into())
    } else {
        None
    }
}

/// A key | value row.
fn kv(rows: &mut Vec<Vec<Cell>>, key: &str, value: impl Into<String>) {
    rows.push(vec![Cell::styled(key, KEY), Cell::plain(value)]);
}

fn kv_styled(rows: &mut Vec<Vec<Cell>>, key: &str, value: impl Into<String>, style: &'static str) {
    rows.push(vec![Cell::styled(key, KEY), Cell::styled(value, style)]);
}

/// A section heading above a table, like PM2's ` Describing process …`.
fn heading(text: &str, fmt: &Fmt) -> String {
    format!("{}\n", fmt.bold(&format!(" {text}")))
}

/// The worker table: `warden workers`, and under the key | value box of
/// `warden status <app>` and `warden describe`.
fn worker_table(s: &Status, fmt: &Fmt) -> String {
    // Workers by number, old processes still draining after a rollout
    // (`2 (old)`), then hot standbys as `s1`, `s2`…
    let workers = s
        .workers
        .iter()
        .map(|w| (w.id.to_string(), w))
        .chain(s.draining.iter().map(|w| (draining_name(s, w), w)))
        .chain(s.standbys.iter().map(|w| (standby_name(w), w)));
    let rows: Vec<Vec<Cell>> = workers
        .map(|(name, w)| {
            vec![
                Cell::styled(name, NAME),
                state_cell(&w.state),
                Cell::plain(w.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
                Cell::plain(w.uptime_secs.map(duration).unwrap_or_else(|| "-".into())),
                Cell::plain(w.restarts.to_string()),
                Cell::plain(w.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into())),
                Cell::plain(w.rss_bytes.map(bytes).unwrap_or_else(|| "-".into())),
                Cell::plain(s.user.clone().unwrap_or_else(|| "-".into())),
                Cell::plain(ports::cell(&w.listening)),
                Cell::plain(loop_p99(w)),
                health_cell(w.healthy),
                Cell::plain(clip(w.last_exit.as_deref().unwrap_or("-"), LAST_EXIT_MAX)),
            ]
        })
        .collect();
    boxed(
        Some(&[
            "worker",
            "status",
            "pid",
            "uptime",
            "↺",
            "cpu",
            "mem",
            "user",
            "ports",
            "loop p99",
            "health",
            "last exit",
        ]),
        &rows,
        fmt,
        None,
    )
}

/// `warden status <app>` (and `list <app>`, `-c`): the app's state as a
/// key | value box, then its workers. `table_only`: just the workers
/// (`warden workers`). `id`: the number `warden list` shows, when known.
pub fn render_status_with(s: &Status, table_only: bool, id: Option<u32>, fmt: &Fmt) -> String {
    if table_only {
        return worker_table(s, fmt);
    }
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let (word, style) = app_status(s);
    kv_styled(&mut rows, "status", word, style);
    if let Some(note) = status_note(s) {
        kv(&mut rows, "note", note);
    }
    if let Some(id) = id {
        kv(&mut rows, "id", id.to_string());
    }
    kv(&mut rows, "namespace", s.namespace.clone());
    if let Some(user) = &s.user {
        kv(&mut rows, "user", user.clone());
    }
    kv(&mut rows, "mode", s.mode.clone());
    kv(&mut rows, "workers", format!("{} configured, {} ready", s.workers_configured, s.workers_ready));
    let listening = ports::app_listeners(s);
    if !listening.is_empty() {
        kv(&mut rows, "ports", ports::detail(&listening));
    }
    if !s.standbys.is_empty() {
        let up = s.standbys.iter().filter(|w| w.state == crate::control::STANDBY).count();
        kv(&mut rows, "standby", format!("{up}/{} ready to take over a crashed worker", s.standbys.len()));
    }
    if !s.draining.is_empty() {
        kv(
            &mut rows,
            "draining",
            format!("{} old process(es) replaced by the rollout, finishing their connections", s.draining.len()),
        );
    }
    kv(&mut rows, "pid", s.pid.to_string());
    kv(&mut rows, "uptime", duration(s.uptime_secs));
    if let Some(r) = &s.release {
        kv(&mut rows, "release", r.clone());
    }
    if let Some(r) = s.supervisor_rss_bytes {
        kv(&mut rows, "memory", format!("{} (supervisor)", bytes(r)));
    }
    if let Some(h) = &s.host {
        kv(
            &mut rows,
            "host",
            format!(
                "pid {}, up {}, {} RSS, {} CPU, {} restarts",
                h.pid,
                duration(h.uptime_secs),
                h.rss_bytes.map(bytes).unwrap_or_else(|| "-".into()),
                h.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into()),
                h.restarts
            ),
        );
    }
    if let Some(h) = s.healthy {
        kv_styled(&mut rows, "health", if h { "healthy" } else { "UNHEALTHY" }, if h { GREEN } else { RED });
    }
    if let Some(r) = &s.rollout {
        kv(&mut rows, "rollout", format!("{} {}/{}: {} ({}s)", r.kind, r.done, r.total, r.phase, r.elapsed_secs));
    }
    if let Some(r) = &s.last_rollout {
        kv(&mut rows, "last rollout", format!("{} {} - {}", r.kind, if r.ok { "ok" } else { "FAILED" }, r.message));
    }
    let mut o = heading(&s.app, fmt);
    o += &boxed(None, &rows, fmt, Some(1));
    o += &heading("Workers", fmt);
    o += &worker_table(s, fmt);
    o
}

/// What `Status.start_failed` means for this app now: stopped after it
/// (errored), or still restarting with backoff.
fn start_failed_note(s: &Status) -> Option<String> {
    let reason = s.start_failed.as_ref()?;
    let app = &s.app;
    Some(if s.stopped {
        format!(
            "errored: its last start failed, every worker crashed before it was ready ({reason}); `warden logs {app} \
             --err` shows why, `warden start {app}` tries again"
        )
    } else {
        format!(
            "not started: every worker crashed before it was ready ({reason}); restarting with backoff (`warden \
             logs {app} --err` shows why, `warden stop {app}` stops it)"
        )
    })
}

/// The Worker column of a hot standby (`Status.standbys`): `s1`, `s2`…
fn standby_name(w: &crate::control::WorkerStatus) -> String {
    format!("s{}", w.id)
}

/// The Worker column of an old process draining after a rollout replaced
/// it (`Status.draining`): `2 (old)`, or `host (old)` in worker mode.
fn draining_name(s: &Status, w: &crate::control::WorkerStatus) -> String {
    if s.mode == "worker" { "host (old)".into() } else { format!("{} (old)", w.id) }
}

/// The Loop p99 column: the event-loop delay's 99th percentile over the
/// last second (`-`: no shim, or no recent heartbeat).
fn loop_p99(w: &crate::control::WorkerStatus) -> String {
    w.loop_delay.map(|d| millis(d.p99_ms)).unwrap_or_else(|| "-".into())
}

/// `0.41ms`, `12.3ms`, `250ms`, `1.20s`.
pub fn millis(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.2}ms")
    } else if ms < 100.0 {
        format!("{ms:.1}ms")
    } else if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

fn health_word(h: Option<bool>) -> &'static str {
    match h {
        Some(true) => "ok",
        Some(false) => "FAILING",
        None => "-",
    }
}

/// The Last exit column is cut here, so one long reason cannot make the
/// table wider than a terminal (`warden describe` has the whole text).
const LAST_EXIT_MAX: usize = 40;

/// A worker or app state as a cell: green when it serves, yellow while it
/// changes or is stopped, red when it failed, dim when there is nothing to run.
fn state_cell(state: &str) -> Cell {
    let style = match state {
        "RUNNING" | "online" => GREEN,
        "STARTING" | "STOPPING" | "STANDBY" | "STOPPED" | "stopped" | "DRAINING" => YELLOW,
        "FAILED" | "CRASHED" | "errored" | "unreachable" => RED,
        "offline" => DIM,
        _ => "",
    };
    Cell::styled(state, style)
}

fn health_cell(h: Option<bool>) -> Cell {
    Cell::styled(
        health_word(h),
        match h {
            Some(true) => GREEN,
            Some(false) => RED,
            None => "",
        },
    )
}

/// `warden list`: a boxed table like `pm2 list` with each app's id (what
/// `warden start 0,1` takes), one row per worker; painted when `color` is on
/// (a terminal).
pub fn render_list_with(all: &[(crate::fleet::App, Result<Status, String>)], fmt: &Fmt) -> String {
    if all.is_empty() {
        return "no apps yet: `warden start server.js --name api`, or `warden pm2-migrate` to bring PM2's over\n"
            .into();
    }
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut notes = Vec::new();
    let mut offline: Vec<&crate::fleet::App> = Vec::new();
    for (app, st) in all {
        let id = app.id.map(|i| i.to_string()).unwrap_or_else(|| "-".into());
        // An app's later rows (more workers) repeat id and name, so every
        // row stands alone for grep, but dimmed.
        let lead = |first: bool, namespace: &str| {
            let (a, b, c) = if first { (KEY, NAME, "") } else { (DIM, DIM, DIM) };
            vec![Cell::styled(id.clone(), a), Cell::styled(app.name.clone(), b), Cell::styled(namespace.to_string(), c)]
        };
        match st {
            Ok(s) => {
                let all = s
                    .workers
                    .iter()
                    .map(|w| (w.id.to_string(), w))
                    .chain(s.draining.iter().map(|w| (draining_name(s, w), w)));
                for (n, (name, w)) in all.chain(s.standbys.iter().map(|w| (standby_name(w), w))).enumerate() {
                    // Stopped after a start that failed: PM2's `errored`.
                    let state = match (s.stopped && w.state == "STOPPED", &s.start_failed) {
                        (true, Some(_)) => "errored".to_string(),
                        (true, None) => "stopped".to_string(),
                        _ => w.state.clone(),
                    };
                    let mut row = lead(n == 0, &s.namespace);
                    row.extend([
                        Cell::plain(name),
                        state_cell(&state),
                        Cell::plain(w.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
                        Cell::plain(w.uptime_secs.map(duration).unwrap_or_else(|| "-".into())),
                        Cell::plain(w.restarts.to_string()),
                        Cell::plain(w.cpu_percent.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "-".into())),
                        Cell::plain(w.rss_bytes.map(bytes).unwrap_or_else(|| "-".into())),
                        Cell::plain(s.user.clone().unwrap_or_else(|| "-".into())),
                        Cell::plain(ports::cell(&w.listening)),
                        Cell::plain(loop_p99(w)),
                        health_cell(w.healthy),
                        Cell::plain(clip(w.last_exit.as_deref().unwrap_or("-"), LAST_EXIT_MAX)),
                    ]);
                    rows.push(row);
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
                if let Some(n) = start_failed_note(s) {
                    notes.push(format!("{}: {n}", app.name));
                }
            }
            Err(e) => {
                let what = if e == "not running" { "offline" } else { "unreachable" };
                let mut row = lead(true, &app.namespace);
                row.push(Cell::plain("-"));
                row.push(state_cell(what));
                row.extend((0..10).map(|_| Cell::plain("-")));
                rows.push(row);
                if what == "offline" {
                    offline.push(app);
                } else {
                    notes.push(format!("{}: {e}", app.name));
                }
            }
        }
        if let Some(p) = &app.problem {
            notes.push(format!("{}: config problem: {p}", app.name));
        }
    }
    if !offline.is_empty() {
        // The exact command: ids as a list when every app has one.
        let ids: Vec<u32> = offline.iter().filter_map(|a| a.id).collect();
        let which = if ids.len() == offline.len() {
            crate::ids::compact(&ids)
        } else {
            offline.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(",")
        };
        let (n, them) = (offline.len(), if offline.len() == 1 { "it" } else { "them" });
        notes.insert(0, format!("{n} offline: `warden start {which}` starts {them}"));
    }
    let mut o = boxed(
        Some(&[
            "id",
            "name",
            "namespace",
            "worker",
            "status",
            "pid",
            "uptime",
            "↺",
            "cpu",
            "mem",
            "user",
            "ports",
            "loop p99",
            "health",
            "last exit",
        ]),
        &rows,
        fmt,
        None,
    );
    for n in notes {
        o += &format!("  {n}\n");
    }
    o
}

/// `warden describe api`.
pub fn render_describe(s: &Status, info: &serde_json::Value, id: Option<u32>, fmt: &Fmt) -> String {
    let c = |p: &str| info.pointer(&format!("/config{p}")).cloned().unwrap_or(serde_json::Value::Null);
    let text = |v: serde_json::Value| match v {
        serde_json::Value::String(s) => s,
        serde_json::Value::Null => "-".into(),
        v => v.to_string(),
    };
    let num = |p: &str| c(p).as_u64().unwrap_or(0);
    // What is running, first (PM2's status / name / id / namespace), then
    // the configuration.
    let mut head: Vec<Vec<Cell>> = Vec::new();
    let (word, style) = app_status(s);
    kv_styled(&mut head, "status", word, style);
    if let Some(note) = status_note(s) {
        kv(&mut head, "note", note);
    }
    kv(&mut head, "name", s.app.clone());
    if let Some(id) = id {
        kv(&mut head, "id", id.to_string());
    }
    kv(&mut head, "namespace", s.namespace.clone());
    if let Some(user) = &s.user {
        kv(&mut head, "user", user.clone());
    }
    let mut rows: Vec<Vec<Cell>> = Vec::new();
    let mut row = |k: &str, v: String| kv(&mut rows, k, v);
    row(
        "config",
        format!("{} (every setting, defaults included: `warden config {}`)", text(info["config_path"].clone()), s.app),
    );
    let args: Vec<String> =
        c("/app/args").as_array().map(|a| a.iter().map(|x| text(x.clone())).collect()).unwrap_or_default();
    row("command", format!("{} {}", text(c("/app/command")), args.join(" ")).trim().to_string());
    row("cwd", text(c("/app/working_directory")));
    row(
        "release",
        match (&s.release, c("/app/pin_release").as_bool()) {
            (Some(r), _) => format!("{r} (pinned: crash restarts stay on it; reload and restart move it)"),
            (None, Some(false)) => "not pinned ([app] pin_release = false)".into(),
            (None, _) => "-".into(),
        },
    );
    row(
        "port",
        match c("/app/port").as_u64() {
            Some(p) => format!("{p} ({})", text(c("/workers/port_strategy"))),
            None => "none (not an HTTP app)".into(),
        },
    );
    // What the workers really listen on, next to what the config says.
    let listening = ports::app_listeners(s);
    let tcp: Vec<u16> = listening
        .iter()
        .filter_map(|l| if let crate::control::Listener::Tcp { port, .. } = l { Some(*port) } else { None })
        .collect();
    let mut seen = if listening.is_empty() {
        if s.workers.iter().any(|w| w.pid.is_some()) {
            "none yet (a port shows a second or two after the app binds it)".to_string()
        } else {
            "-".to_string()
        }
    } else {
        ports::detail(&listening)
    };
    // A worker that has been up a while and listens, but not on the port the
    // config names (for `offset`, base + its number - 1): readiness waits for that one.
    let strategy = text(c("/workers/port_strategy"));
    match c("/app/port").as_u64() {
        Some(base) => {
            let lost: Vec<String> = s
                .workers
                .iter()
                .filter(|w| w.pid.is_some() && w.uptime_secs.is_some_and(|u| u >= 10))
                .filter(|w| {
                    let want = if strategy == "offset" { base + w.id as u64 - 1 } else { base };
                    let on: Vec<u64> = w
                        .listening
                        .iter()
                        .filter_map(|l| {
                            if let crate::control::Listener::Tcp { port, .. } = l {
                                Some(u64::from(*port))
                            } else {
                                None
                            }
                        })
                        .collect();
                    !on.is_empty() && !on.contains(&want)
                })
                .map(|w| w.id.to_string())
                .collect();
            if !lost.is_empty() {
                seen += &format!(
                    ". Worker(s) {} listen on other ports than the configured one (port {base}, {strategy}): check `port` in [app]",
                    lost.join(", ")
                );
            }
        }
        None if !tcp.is_empty() => {
            seen += ". [app] has no `port`, so Warden does not wait for or check any of them";
        }
        None => {}
    }
    row("ports", seen);
    row("mode", format!("{}, {} worker(s)", s.mode, s.workers_configured));
    if num("/workers/standby") > 0 {
        row(
            "standby",
            format!(
                "{} hot standby(s): started, not listening; one takes a crashed worker's slot in milliseconds",
                num("/workers/standby")
            ),
        );
    }
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
    row(
        "rollouts",
        match c("/reload/surge") {
            serde_json::Value::Number(n) if n.as_u64() == Some(1) => "one worker at a time".into(),
            serde_json::Value::Null => "one worker at a time".into(),
            v => format!("surge {}: that many new workers at a time, next to the old ones", text(v)),
        },
    );
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
        "watchdog",
        match num("/watchdog/timeout") {
            0 => "off (the heartbeat still reports event-loop delay)".into(),
            t => format!("hung after {t} s without an event-loop heartbeat"),
        },
    );
    let delays: Vec<String> = s
        .workers
        .iter()
        .filter_map(|w| {
            w.loop_delay.map(|d| format!("{} {}/{}/{}", w.id, millis(d.p50_ms), millis(d.p99_ms), millis(d.max_ms)))
        })
        .collect();
    let warn = c("/watchdog/loop_delay_warn").as_f64().unwrap_or(0.0);
    row(
        "event loop",
        format!(
            "delay p50/p99/max over the last second: {}; {}",
            if delays.is_empty() { "- (needs the shim)".to_string() } else { delays.join(", ") },
            if warn > 0.0 {
                format!("warns when p99 stays at {} or more for 10 s", millis(warn * 1000.0))
            } else {
                "no warning (loop_delay_warn = 0)".into()
            }
        ),
    );
    row(
        "logs",
        match &s.log_file {
            Some(f) => format!("{f} (+ `warden logs {}`)", s.app),
            None => format!("stdout / journald (+ `warden logs {}`)", s.app),
        },
    );
    let files: Vec<String> =
        ["/logging/out_file", "/logging/err_file"].iter().filter_map(|p| c(p).as_str().map(String::from)).collect();
    row(
        "worker output",
        format!(
            "{}{}{}",
            text(c("/logging/worker_output")),
            if files.is_empty() { String::new() } else { format!(" → {}", files.join(", ")) },
            if c("/logging/per_worker_files").as_bool() == Some(true) && !files.is_empty() {
                " (one file per worker: out-1.log…)"
            } else {
                ""
            }
        ),
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
        // One KEY=value per line, then where they come from.
        let mut lines: Vec<String> = env.iter().map(|(k, v)| format!("{k}={}", text(v.clone()))).collect();
        if lines.is_empty() {
            lines.push("-".into());
        }
        lines.push(match c("/app/env_file").as_str() {
            Some(f) => format!("(env_file {f}, then env; `warden env {}` adds Warden's)", s.app),
            None => format!("(`warden env {}` adds Warden's)", s.app),
        });
        row("env", lines.join("\n"));
    }
    if let Some(r) = &s.last_rollout {
        row("last rollout", format!("{} {}: {}", r.kind, if r.ok { "ok" } else { "FAILED" }, r.message));
    }
    head.extend(rows);
    let title = match id {
        Some(id) => format!(" Describing app with id {id} - name {}", s.app),
        None => format!(" Describing app {}", s.app),
    };
    let mut o = format!("{}\n", fmt.bold(&title));
    o += &boxed(None, &head, fmt, Some(1));
    o += &heading("Workers", fmt);
    o += &worker_table(s, fmt);
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
            (Action::Logs { lines: Some(5), follow: Some(true), history: false, query: Default::default() }, None)
        );
        assert_eq!(
            act("logs --worker 2 --events --nostream"),
            (
                Action::Logs {
                    lines: None,
                    follow: Some(false),
                    history: false,
                    query: crate::logview::Query { events: true, ..Default::default() }
                },
                Some(":2".into())
            )
        );
        assert_eq!(
            act("logs api --lines 100 --err"),
            (
                Action::Logs {
                    lines: Some(100),
                    follow: None,
                    history: false,
                    query: crate::logview::Query { stream: Some("stderr".into()), ..Default::default() }
                },
                Some("api".into())
            )
        );
        assert_eq!(act("log-level"), (Action::LogLevel(None), None));
        assert_eq!(act("log-level debug"), (Action::LogLevel(Some(Level::Debug)), None));
        assert_eq!(act("log-level api warn"), (Action::LogLevel(Some(Level::Warn)), Some("api".into())));
        assert!(p("log-level -c x.toml loud").is_err());
        assert!(p("scale").is_err());
        assert!(p("scale x").is_err());
        // Several apps (or ids), as one comma list or as separate words (PM2 takes both).
        assert_eq!(act("status a b"), (Action::List, Some("a,b".into())));
        assert_eq!(act("restart 0,1,2"), (Action::Restart { hard: false }, Some("0,1,2".into())));
        assert_eq!(act("stop 0 1 api:2"), (Action::Stop, Some("0,1,api:2".into())));
        assert_eq!(act("reload 0-2,web"), (Action::Reload { safe: false }, Some("0-2,web".into())));
        assert_eq!(
            p("start 0,1,2").unwrap().command,
            Command::Start { what: "0,1,2".into(), opts: Default::default() }
        );
        assert_eq!(p("start 3 4").unwrap().command, Command::Start { what: "3,4".into(), opts: Default::default() });
        assert_eq!(p("delete 2 3").unwrap().command, Command::Delete { target: "2,3".into() });
        assert!(p("delete").unwrap_err().contains("name or id"));
        // Options that take a value still own it: `-n 5` is not an app.
        assert_eq!(
            act("logs 0 -n 5"),
            (
                Action::Logs { lines: Some(5), follow: None, history: false, query: Default::default() },
                Some("0".into())
            )
        );
        // --worker belongs to one app; several words for `start` are apps, not a command line.
        assert!(p("restart api web --worker 1").is_err());
        assert_eq!(act("restart api --worker 1"), (Action::Restart { hard: false }, Some("api:1".into())));
        // (`p` splits on spaces, so build the argv with a real command-line word.)
        let argv = |a: &[&str]| parse(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        let e = argv(&["start", "node a.js", "b"]).unwrap_err();
        assert!(e.contains("several app names or ids") && e.contains("node server.js"), "{e}");
        assert_eq!(
            argv(&["start", "node a.js", "--name", "x"]).unwrap().command,
            Command::Start {
                what: "node a.js".into(),
                opts: Box::new(StartOpts { name: Some("x".into()), ..Default::default() })
            }
        );
        assert_eq!(
            p("start node server.js").unwrap().command,
            Command::Start { what: "node,server.js".into(), opts: Default::default() }
        );
        // Commands whose extra words mean something else keep their shape.
        assert!(p("scale a 3 4").is_err());
        assert!(p("signal USR2 a b c").is_err());
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
    fn daemon_and_events_commands() {
        let run = |background, resurrect| Command::Daemon(DaemonCmd::Run { background, resurrect });
        assert_eq!(p("daemon").unwrap().command, run(false, false));
        assert_eq!(p("daemon --background").unwrap().command, run(true, false));
        assert_eq!(p("daemon --resurrect").unwrap().command, run(false, true));
        assert_eq!(p("daemon --background --resurrect").unwrap().command, run(true, true));
        assert!(p("start app.js --resurrect").is_err());
        use crate::startup::Want;
        assert_eq!(p("startup").unwrap().command, Command::Startup(Want::Auto));
        assert_eq!(p("startup --user").unwrap().command, Command::Startup(Want::User));
        assert_eq!(p("unstartup --system").unwrap().command, Command::Unstartup(Want::System));
        assert!(p("startup api").is_err());
        assert!(p("list --user").is_err(), "only for startup");
        assert_eq!(p("daemon status").unwrap().command, Command::Daemon(DaemonCmd::Status));
        assert!(p("daemon status --json").unwrap().json);
        assert_eq!(p("daemon stop").unwrap().command, Command::Daemon(DaemonCmd::Stop));
        assert_eq!(p("daemon check").unwrap().command, Command::Daemon(DaemonCmd::Check));
        let a = p("daemon check -c /tmp/wardend.toml").unwrap();
        assert_eq!((a.command, a.config), (Command::Daemon(DaemonCmd::Check), Some("/tmp/wardend.toml".into())));
        assert_eq!(p("wardend reload").unwrap().command, Command::Daemon(DaemonCmd::Reload));
        assert!(p("daemon reload --background").is_err());
        assert!(p("daemon frobnicate").is_err());
        assert!(p("daemon stop now").is_err());
        assert!(p("start app.js --background").is_err(), "only for the daemon");
        let a = p("events api --json --logs --interval 500").unwrap();
        assert_eq!(a.command, Command::Events { logs: true, interval_ms: Some(500) });
        assert_eq!((a.target.as_deref(), a.json), (Some("api"), true));
        assert_eq!(p("events").unwrap().command, Command::Events { logs: false, interval_ms: None });
        assert!(p("events --interval soon").is_err());
    }

    #[test]
    fn serve_command() {
        let a = p("serve ./dist 3000 --spa --name site --basic-auth-username u --basic-auth-password p").unwrap();
        let Command::Serve { dir, port, opts } = a.command else { panic!() };
        assert_eq!((dir, port), (PathBuf::from("./dist"), 3000));
        assert!(opts.spa);
        assert_eq!(opts.name.as_deref(), Some("site"));
        assert_eq!(opts.basic_auth.as_deref(), Some("u:p"));
        let Command::Serve { dir, port, .. } = p("serve").unwrap().command else { panic!() };
        assert_eq!((dir, port), (PathBuf::from("."), 8080));
        assert!(p("serve dist notaport").is_err());
    }

    #[test]
    fn log_search() {
        let (a, t) = act("search ECONNREFUSED api --since 2026-09-30 --ignore-case");
        let Action::Logs { history, follow, query, .. } = a else { panic!() };
        assert!(history && follow == Some(false));
        assert_eq!(t.as_deref(), Some("api"));
        assert_eq!(query.grep, vec!["ECONNREFUSED".to_string()]);
        assert!(query.ignore_case);
        assert_eq!(query.since.as_deref(), Some("2026-09-30T00:00:00.000Z"));
        let (a, _) = act("logs api --history --grep a --grep b --exclude health --level warn");
        let Action::Logs { query, .. } = a else { panic!() };
        assert_eq!((query.grep.len(), query.exclude.len(), query.level), (2, 1, Some(Level::Warn)));
        assert!(p("logs --since yesterday").is_err());
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

    /// A start that failed: `errored` rows and a note while stopped after
    /// it, a note while it still restarts; the Loop p99 column.
    #[test]
    fn list_shows_errored_apps_and_loop_delay() {
        let st = |stopped: bool| -> Status {
            serde_json::from_value(serde_json::json!({
                "app": "api", "namespace": "default", "mode": "process", "pid": 1, "uptime_secs": 1,
                "workers_configured": 1, "workers_ready": 0, "healthy": null, "supervisor_rss_bytes": null,
                "host": null, "reloading": false, "shutting_down": false, "stopped": stopped,
                "start_failed": "exit code 3",
                "workers": [{"id": 1, "state": if stopped { "STOPPED" } else { "RESTARTING" }, "pid": null,
                    "uptime_secs": null, "restarts": 1, "crashes": 1, "rss_bytes": null, "cpu_seconds": null,
                    "cpu_percent": null, "last_exit": "exit code 3",
                    "loop_delay": {"p50_ms": 0.1, "p99_ms": 12.34, "max_ms": 20.0}}]
            }))
            .unwrap()
        };
        let app = crate::fleet::App {
            name: "api".into(),
            namespace: "default".into(),
            config: None,
            socket: "/run/w/api/control.sock".into(),
            problem: None,
            id: Some(3),
        };
        let text = render_list_with(&[(app.clone(), Ok(st(true)))], &Fmt::PLAIN);
        assert!(text.contains("loop p99") && text.contains("12.3ms"), "{text}");
        // top rule, header, rule, then the app's row: its id first.
        assert!(text.lines().nth(3).is_some_and(|l| l.starts_with("│ 3 ") && l.contains(" errored ")), "{text}");
        assert!(text.contains("api: errored: its last start failed"), "{text}");
        let text = render_list_with(&[(app, Ok(st(false)))], &Fmt::PLAIN);
        assert!(text.contains(" RESTARTING ") && text.contains("api: not started: every worker crashed"), "{text}");
        let one = render_status(&st(true), false);
        assert!(
            one.contains("│ status    │ errored ") && one.contains("│ note      │ errored: its last start failed"),
            "{one}"
        );
    }

    #[test]
    fn formatting() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(125), "2m05s");
        assert_eq!(duration(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(duration(90_000), "1d01h");
        assert_eq!(bytes(64 * 1024 * 1024), "64.0 MB");
        assert_eq!(
            [millis(0.414), millis(12.34), millis(250.4), millis(1200.0)],
            ["0.41ms", "12.3ms", "250ms", "1.20s"].map(String::from)
        );
    }

    fn row(id: usize, state: &str, pid: u32) -> crate::control::WorkerStatus {
        crate::control::WorkerStatus {
            id,
            state: state.into(),
            pid: Some(pid),
            uptime_secs: Some(5),
            restarts: 1,
            crashes: 0,
            rss_bytes: None,
            cpu_seconds: None,
            cpu_percent: None,
            last_exit: None,
            healthy: None,
            loop_delay: None,
            listening: Vec::new(),
        }
    }

    /// Old processes draining after a rollout are listed after the workers
    /// as `N (old)`, never as workers; standbys come last.
    #[test]
    fn status_lists_draining_old_processes() {
        let mut s: Status = serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": 7, "uptime_secs": 9, "workers_configured": 2, "workers_ready": 2,
            "healthy": null, "supervisor_rss_bytes": null, "host": null, "reloading": true, "shutting_down": false,
            "workers": []
        }))
        .unwrap();
        s.workers = vec![row(1, "RUNNING", 101), row(2, "RUNNING", 102)];
        s.draining = vec![row(1, crate::control::DRAINING, 91), row(2, crate::control::DRAINING, 92)];
        s.standbys = vec![row(1, crate::control::STANDBY, 103)];
        let out = render_status(&s, false);
        assert!(out.contains("│ draining  │ 2 old process(es)"), "{out}");
        // The boxed table's rows: "│ 1 (old) │ DRAINING │ 91 │ …".
        let first_cells = |text: &str| -> Vec<String> {
            text.lines()
                .skip_while(|l| !l.starts_with("│ worker "))
                .skip(2)
                .take_while(|l| l.starts_with('│'))
                .map(|l| l.trim_start_matches('│').split('│').next().unwrap_or("").trim().to_string())
                .collect()
        };
        assert_eq!(first_cells(&out), ["1", "2", "1 (old)", "2 (old)", "s1"], "{out}");
        assert!(out.lines().any(|l| l.starts_with("│ 2 (old)") && l.contains("DRAINING") && l.contains("92")), "{out}");
        s.mode = "worker".into();
        s.draining = vec![row(0, crate::control::DRAINING, 90)];
        assert!(render_status(&s, true).lines().any(|l| l.starts_with("│ host (old)") && l.contains("90")));
        s.draining.clear();
        assert!(!render_status(&s, false).contains("draining"));
    }

    /// `warden describe` is two boxes, like PM2's: a key | value box (status
    /// first, then the id and the settings), then the workers.
    #[test]
    fn describe_is_a_key_value_box_then_a_worker_box() {
        let mut s: Status = serde_json::from_value(serde_json::json!({
            "app": "api", "namespace": "web", "mode": "process", "pid": 7, "uptime_secs": 9, "workers_configured": 1,
            "workers_ready": 1, "healthy": null, "supervisor_rss_bytes": null, "host": null, "reloading": false,
            "shutting_down": false, "workers": []
        }))
        .unwrap();
        s.workers = vec![row(1, "RUNNING", 101)];
        let info = serde_json::json!({
            "config_path": "/etc/warden/api.toml",
            "config": {"app": {"command": "bun", "args": ["run", "server.ts"], "port": 3000, "working_directory": "/srv/api"}}
        });
        let out = render_describe(&s, &info, Some(4), &Fmt::PLAIN);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], " Describing app with id 4 - name api", "{out}");
        assert!(lines[1].starts_with('┌') && lines[2].starts_with("│ status "), "{out}");
        // status, name, id, namespace come first.
        let keys: Vec<&str> = lines[2..]
            .iter()
            .take_while(|l| l.starts_with('│'))
            .map(|l| l.trim_start_matches('│').split('│').next().unwrap_or("").trim())
            .collect();
        assert_eq!(&keys[..4], ["status", "name", "id", "namespace"], "{out}");
        assert!(out.contains("│ command ") && out.contains("bun run server.ts") && out.contains("3000"), "{out}");
        // The worker box follows, under its own heading.
        assert!(out.contains("\n Workers\n┌") && out.contains("│ worker │ status "), "{out}");
        // Every box is a rectangle.
        for chunk in out.split("\n Workers\n") {
            let boxed_lines: Vec<&str> = chunk.lines().filter(|l| l.starts_with(['┌', '│', '├', '└'])).collect();
            let w = boxed_lines[0].chars().count();
            assert!(boxed_lines.iter().all(|l| l.chars().count() == w), "{chunk}");
        }
        // A wrapped terminal keeps nothing out of the box.
        let narrow = render_describe(&s, &info, Some(4), &Fmt { color: false, width: Some(60) });
        let settings = narrow.split("\n Workers\n").next().unwrap_or("");
        assert!(settings.lines().all(|l| l.chars().count() <= 60), "{narrow}");
        assert!(narrow.contains("`warden config api`)"), "nothing is lost when wrapping:\n{narrow}");
    }

    fn listed_app(name: &str, id: Option<u32>) -> crate::fleet::App {
        crate::fleet::App {
            name: name.into(),
            namespace: "default".into(),
            config: None,
            socket: format!("/run/w/{name}/control.sock").into(),
            problem: None,
            id,
        }
    }

    fn render_status(s: &Status, table_only: bool) -> String {
        render_status_with(s, table_only, None, &Fmt::PLAIN)
    }

    fn plain(s: &str) -> String {
        // Drop SGR sequences (`ESC [ … m`).
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for c in it.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    /// The `warden list` table is boxed like PM2's, with ids first; colors add
    /// escape codes and nothing else.
    #[test]
    fn list_is_a_pm2_style_box_with_ids() {
        let off = |n: &str, id| (listed_app(n, id), Err::<Status, String>("not running".into()));
        let all = vec![off("booking-manager", Some(0)), off("code-intel-mcp", Some(1)), off("themes", Some(2))];
        let text = render_list_with(&all, &Fmt::PLAIN);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with('┌') && lines[0].ends_with('┐'), "{text}");
        assert!(lines[1].starts_with("│ id │ name ") && lines[1].contains("│ ↺ │"), "{text}");
        assert!(lines[2].starts_with('├') && lines[2].ends_with('┤'), "{text}");
        assert!(lines[3].starts_with("│ 0  │ booking-manager ") && lines[3].contains(" offline "), "{text}");
        assert!(lines[5].starts_with("│ 2  │ themes "), "{text}");
        assert!(lines[6].starts_with('└') && lines[6].ends_with('┘'), "{text}");
        // One box: every line of it is as wide as the top rule.
        let width = lines[0].chars().count();
        assert!(lines[..7].iter().all(|l| l.chars().count() == width), "{text}");
        // One line says how to start them all, with the ids as a range.
        assert_eq!(lines[7], "  3 offline: `warden start 0-2` starts them", "{text}");

        // Colors never change the layout.
        let painted = render_list_with(&all, &Fmt { color: true, width: None });
        assert!(painted.contains("\x1b[") && !text.contains('\x1b'));
        assert_eq!(plain(&painted), text);

        // An app without an id shows `-` and is started by name.
        let mixed = render_list_with(&[off("a", Some(4)), off("b", None)], &Fmt::PLAIN);
        assert!(mixed.contains("│ -  │ b ") && mixed.contains("2 offline: `warden start a,b` starts them"), "{mixed}");
        assert!(render_list_with(&[], &Fmt::PLAIN).starts_with("no apps yet"));
    }

    /// The `user` column shows who the app runs as; `-` when unknown
    /// (an older Warden sends none, an app that is not running has no process).
    #[test]
    fn the_user_shows_in_the_list_the_workers_table_and_the_boxes() {
        let mut s: Status = serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": 7, "uptime_secs": 9, "workers_configured": 1, "workers_ready": 1,
            "healthy": null, "supervisor_rss_bytes": null, "host": null, "reloading": false, "shutting_down": false,
            "workers": [], "user": "deploy"
        }))
        .unwrap();
        s.workers = vec![row(1, "RUNNING", 101)];
        let list = render_list_with(&[(listed_app("api", Some(0)), Ok(s.clone()))], &Fmt::PLAIN);
        // The cells of one box line, empty ones included (the namespace is empty here).
        let cells = |line: &str| -> Vec<String> {
            let mut v: Vec<String> = line.split('│').map(|c| c.trim().to_string()).collect();
            v.remove(0);
            v.pop();
            v
        };
        let at = cells(list.lines().nth(1).unwrap()).iter().position(|c| c == "user").expect("a user column");
        assert_eq!(cells(list.lines().nth(3).unwrap())[at], "deploy", "{list}");
        assert!(render_status(&s, true).contains("deploy"), "the workers table");
        assert!(render_status(&s, false).contains("│ user      │ deploy"), "the status box");
        // An older Warden's status has no user: a dash, no `user` row.
        s.user = None;
        let list = render_list_with(&[(listed_app("api", Some(0)), Ok(s.clone()))], &Fmt::PLAIN);
        assert_eq!(cells(list.lines().nth(3).unwrap())[at], "-", "{list}");
        assert!(!render_status(&s, false).contains("│ user      │"));
    }

    fn api_status() -> Status {
        serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": 7, "uptime_secs": 9, "workers_configured": 2, "workers_ready": 2,
            "healthy": null, "supervisor_rss_bytes": null, "host": null, "reloading": false, "shutting_down": false,
            "workers": []
        }))
        .unwrap()
    }

    /// What the workers listen on shows in the list, the boxes and `warden ports`.
    #[test]
    fn the_sockets_show_in_the_list_the_boxes_and_the_ports_table() {
        use crate::control::Listener;
        let tcp = |addr: &str, port| Listener::Tcp { addr: addr.into(), port };
        let mut s = api_status();
        let (mut one, mut two) = (row(1, "RUNNING", 101), row(2, "RUNNING", 102));
        // Both workers share 3000 (SO_REUSEPORT); only the first has the debugger and a socket.
        one.listening = vec![
            tcp("0.0.0.0", 3000),
            tcp("::", 3000),
            tcp("127.0.0.1", 9229),
            Listener::Unix { path: "/tmp/api.sock".into() },
        ];
        two.listening = vec![tcp("0.0.0.0", 3000), tcp("::", 3000)];
        s.workers = vec![one, two];
        let app = listed_app("api", Some(3));

        let list = render_list_with(&[(app.clone(), Ok(s.clone()))], &Fmt::PLAIN);
        // Cut at 26 characters: the unix socket is the "+1" (`warden ports` has it).
        assert!(list.contains("│ ports") && list.contains("3000,localhost:9229,+1"), "{list}");
        assert!(list.lines().any(|l| l.contains("│ 2 ") && l.contains("│ 3000 ")), "{list}");
        // Status box: every socket once, with where it is reachable.
        let status = render_status(&s, false);
        assert!(status.contains("3000 (all interfaces), 9229 (localhost only), unix /tmp/api.sock"), "{status}");
        // Describe: the same, and a configured port that nothing listens on is said.
        let info = serde_json::json!({"config_path": "/x.toml", "config": {"app": {"command": "bun", "port": 4000}}});
        let d = render_describe(&s, &info, Some(3), &Fmt::PLAIN);
        // Young workers (5 s) are not judged: their port may still be about to move.
        assert!(!d.contains("other ports than the configured one"), "{d}");
        let mut old = s.clone();
        for w in &mut old.workers {
            w.uptime_secs = Some(30);
        }
        let d = render_describe(&old, &info, Some(3), &Fmt::PLAIN);
        assert!(
            d.contains("3000 (all interfaces)")
                && d.contains("Worker(s) 1, 2 listen on other ports than the configured one (port 4000"),
            "{d}"
        );
        let info = serde_json::json!({"config_path": "/x.toml", "config": {"app": {"command": "bun", "port": 3000}}});
        assert!(!render_describe(&old, &info, Some(3), &Fmt::PLAIN).contains("other ports than the configured one"));
        // No port in the config but the app listens: the hint to set one.
        let info = serde_json::json!({"config_path": "/x.toml", "config": {"app": {"command": "bun"}}});
        assert!(render_describe(&s, &info, Some(3), &Fmt::PLAIN).contains("[app] has no `port`"));

        // warden ports: one row per socket, a port on 0.0.0.0 and :: is one, workers merged.
        let table = ports::render(&[(app.clone(), Ok(s.clone()))], &Fmt::PLAIN);
        assert_eq!(table.lines().filter(|l| l.contains("│ tcp ")).count(), 2, "{table}");
        let row3000 = table.lines().find(|l| l.contains("│ 3000 ")).unwrap();
        assert!(
            row3000.contains("all interfaces")
                && row3000.contains("│ 1,2 ")
                && row3000.contains("http://localhost:3000"),
            "{table}"
        );
        let row9229 = table.lines().find(|l| l.contains("│ 9229 ")).unwrap();
        assert!(row9229.contains("localhost only") && row9229.contains("│ 1 "), "{table}");
        let unix = table.lines().find(|l| l.contains("│ unix ")).unwrap();
        assert!(unix.contains("/tmp/api.sock") && table.contains("curl --unix-socket"), "{table}");
        // JSON: one object per socket, workers and URL included.
        let j = ports::json(&ports::collect(&[(app.clone(), Ok(s.clone()))]));
        let first = &j[0];
        assert_eq!(
            (first["kind"].as_str(), first["port"].as_u64(), first["app"].as_str()),
            (Some("tcp"), Some(3000), Some("api")),
            "{j}"
        );
        assert_eq!(first["workers"], serde_json::json!([1, 2]));
        assert_eq!(first["url"], "http://localhost:3000");
        assert!(j.as_array().unwrap().iter().any(|o| o["kind"] == "unix" && o["path"] == "/tmp/api.sock"), "{j}");
    }

    #[test]
    fn nothing_listening_is_a_dash_and_a_sentence() {
        let mut s = api_status();
        s.workers = vec![row(1, "RUNNING", 101)];
        let list = render_list_with(&[(listed_app("api", Some(0)), Ok(s.clone()))], &Fmt::PLAIN);
        let cells: Vec<String> = list.lines().nth(3).unwrap().split('│').map(|c| c.trim().to_string()).collect();
        let head: Vec<String> = list.lines().nth(1).unwrap().split('│').map(|c| c.trim().to_string()).collect();
        let at = head.iter().position(|c| c == "ports").expect("a ports column");
        assert_eq!(cells[at], "-", "{list}");
        assert!(!render_status(&s, false).contains("│ ports     │"), "no row without sockets");
        let out = ports::render(&[(listed_app("api", Some(0)), Ok(s))], &Fmt::PLAIN);
        assert!(out.starts_with("nothing is listening"), "{out}");
        // An offline app is counted, not an error.
        let off = ports::render(&[(listed_app("web", Some(1)), Err("not running".into()))], &Fmt::PLAIN);
        assert!(off.contains("1 app(s) offline"), "{off}");
    }

    #[test]
    fn the_ports_command_parses_with_an_optional_app() {
        let a = parse(&["ports".to_string()]).unwrap();
        assert!(matches!(a.command, Command::Act(Action::Ports)) && a.target.is_none());
        let a = parse(&["ports".to_string(), "api".to_string(), "--json".to_string()]).unwrap();
        assert!(matches!(a.command, Command::Act(Action::Ports)) && a.target.as_deref() == Some("api") && a.json);
    }

    #[test]
    fn a_long_last_exit_is_cut_and_the_box_stays_whole() {
        let mut s: Status = serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": 7, "uptime_secs": 9, "workers_configured": 2, "workers_ready": 0,
            "healthy": null, "supervisor_rss_bytes": null, "host": null, "reloading": false, "shutting_down": false,
            "workers": []
        }))
        .unwrap();
        let mut w = row(1, "CRASHED", 0);
        w.last_exit =
            Some("killed by SIGKILL (the kernel's OOM killer: the cgroup's memory limit was reached)\nline 2".into());
        s.workers = vec![w, row(2, "RUNNING", 5)];
        let text = render_list_with(&[(listed_app("api", Some(9)), Ok(s))], &Fmt::PLAIN);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[3].contains("killed by SIGKILL") && lines[3].contains('…') && !text.contains("line 2"), "{text}");
        let width = lines[0].chars().count();
        assert!(lines[..6].iter().all(|l| l.chars().count() == width), "{text}");
        // Both workers carry the id and name, so each row stands alone.
        assert!(lines[3].starts_with("│ 9  │ api ") && lines[4].starts_with("│ 9  │ api "), "{text}");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
    }
}

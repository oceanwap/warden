//! Command-line parsing and the client side of the control commands.
//! Hand-rolled to keep the dependency list short.

use crate::control::{self, Request, Status};
use std::path::PathBuf;

pub const USAGE: &str = "\
warden - a small supervisor for Bun/Node HTTP workers

USAGE:
    warden <COMMAND> [OPTIONS]

COMMANDS:
    start          Run the supervisor in the foreground (what systemd runs)
    status         Application and worker status
    workers        Worker table only
    safe-reload    Production deploy: preflight, canary (next to the old worker, rolled
                   back on failure), then one worker at a time through the health gates
    reload         Rolling restart through the health gates (no canary soak, no pauses)
    restart [N]    Replace worker N through the gates; without N, stop and start all workers
    stop           Stop all workers; the supervisor stays up
    shutdown       Stop all workers and exit the supervisor
    scale <N>      Change the number of workers (runtime only, not saved)
    logs           Recent log lines  [-n LINES] [-f|--follow]
    check          Validate the config file and exit
    version        Print the version

OPTIONS:
    -c, --config <PATH>   Config file [default: $WARDEN_CONFIG or ./warden.toml]
    -s, --socket <PATH>   Control socket [default: from the config]
        --json            JSON output for status / workers
        --no-wait         Return as soon as a reload/restart has started
    -h, --help            Show this help

reload, safe-reload and restart N wait for the rollout, print its progress and
exit 1 if it failed (or 2 if Warden is unreachable), so they fit `ExecReload=`
and deploy scripts.
";

#[derive(Debug, PartialEq)]
pub enum Command {
    Start,
    Check,
    Version,
    Help,
    Client(Request),
    Workers,
}

#[derive(Debug, PartialEq)]
pub struct Args {
    pub command: Command,
    pub config: PathBuf,
    pub socket: Option<PathBuf>,
    pub json: bool,
    pub no_wait: bool,
}

pub fn parse(argv: &[String]) -> Result<Args, String> {
    let mut config: Option<PathBuf> = None;
    let mut socket = None;
    let mut json = false;
    let mut no_wait = false;
    let mut lines = 50usize;
    let mut follow = false;
    let mut positional: Vec<String> = Vec::new();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match a.as_str() {
            "-c" | "--config" => config = Some(value(a)?.into()),
            "-s" | "--socket" => socket = Some(value(a)?.into()),
            "-n" | "--lines" => lines = value(a)?.parse().map_err(|_| "-n expects a number".to_string())?,
            "-f" | "--follow" => follow = true,
            "--json" => json = true,
            "--no-wait" => no_wait = true,
            "-h" | "--help" => positional.insert(0, "help".into()),
            "-V" | "--version" => positional.insert(0, "version".into()),
            s if s.starts_with("--config=") => config = Some(s["--config=".len()..].into()),
            s if s.starts_with("--socket=") => socket = Some(s["--socket=".len()..].into()),
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s}")),
            s => positional.push(s.to_string()),
        }
    }
    let config = config
        .or_else(|| std::env::var_os("WARDEN_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("warden.toml"));
    let mut pos = positional.into_iter();
    let cmd = pos.next().unwrap_or_else(|| "help".into());
    let arg = pos.next();
    if let Some(extra) = pos.next() {
        return Err(format!("unexpected argument {extra:?}"));
    }
    let number = |what: &str| -> Result<usize, String> {
        arg.as_deref()
            .ok_or_else(|| format!("{what} needs a number"))?
            .parse()
            .map_err(|_| format!("{what} expects a number"))
    };
    let command = match cmd.as_str() {
        "start" | "run" => Command::Start,
        "check" => Command::Check,
        "version" => Command::Version,
        "help" => Command::Help,
        "status" => Command::Client(Request::Status),
        "workers" => Command::Workers,
        "reload" => Command::Client(Request::Reload { safe: false }),
        "safe-reload" | "deploy" => Command::Client(Request::Reload { safe: true }),
        "stop" => Command::Client(Request::Stop),
        "shutdown" => Command::Client(Request::Shutdown),
        "restart" => {
            Command::Client(Request::Restart { worker: if arg.is_some() { Some(number("restart")?) } else { None } })
        }
        "scale" => Command::Client(Request::Scale { count: number("scale")? }),
        "logs" => Command::Client(Request::Logs { lines, follow }),
        other => return Err(format!("unknown command {other:?} (see `warden --help`)")),
    };
    if arg.is_some() && !matches!(command, Command::Client(Request::Restart { .. } | Request::Scale { .. })) {
        return Err(format!("{cmd} takes no argument"));
    }
    Ok(Args { command, config, socket, json, no_wait })
}

pub async fn run_client(req: Request, socket: PathBuf, json: bool, table_only: bool, no_wait: bool) -> i32 {
    let mut out = std::io::stdout();
    match control::call(&socket, &req, &mut out).await {
        Err(e) => {
            eprintln!("warden: {e}");
            2
        }
        Ok(None) => 0,
        Ok(Some(resp)) => {
            if let Some(st) = &resp.status {
                if json {
                    println!("{}", serde_json::to_string_pretty(st).unwrap_or_default());
                } else {
                    print!("{}", render_status(st, table_only));
                }
            } else if let Some(m) = &resp.message {
                if resp.ok { println!("{m}") } else { eprintln!("warden: {m}") }
            }
            match (resp.ok, resp.seq) {
                (true, Some(seq)) if !no_wait => wait_for_rollout(&socket, seq).await,
                (ok, _) => {
                    if ok {
                        0
                    } else {
                        1
                    }
                }
            }
        }
    }
}

/// Follow a rollout until it finishes; exit code 0 = succeeded.
async fn wait_for_rollout(socket: &std::path::Path, seq: u64) -> i32 {
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
            match w.healthy {
                Some(true) => "ok",
                Some(false) => "FAILING",
                None => "-",
            },
            w.last_exit.as_deref().unwrap_or("-"),
        );
    }
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

    #[test]
    fn commands() {
        assert_eq!(p("start -c /etc/w.toml").unwrap().command, Command::Start);
        assert_eq!(p("start -c /etc/w.toml").unwrap().config, PathBuf::from("/etc/w.toml"));
        assert_eq!(p("restart 2").unwrap().command, Command::Client(Request::Restart { worker: Some(2) }));
        assert_eq!(p("restart").unwrap().command, Command::Client(Request::Restart { worker: None }));
        assert_eq!(p("scale 8").unwrap().command, Command::Client(Request::Scale { count: 8 }));
        assert_eq!(p("logs -n 5 -f").unwrap().command, Command::Client(Request::Logs { lines: 5, follow: true }));
        assert!(p("scale").is_err());
        assert!(p("scale x").is_err());
        assert!(p("status 3").is_err());
        assert!(p("frobnicate").is_err());
        assert!(p("status --bogus").is_err());
        assert_eq!(p("").unwrap().command, Command::Help);
        assert!(p("status --json").unwrap().json);
        assert_eq!(p("safe-reload").unwrap().command, Command::Client(Request::Reload { safe: true }));
        assert_eq!(p("reload --no-wait").unwrap().command, Command::Client(Request::Reload { safe: false }));
        assert!(p("reload --no-wait").unwrap().no_wait);
    }

    #[test]
    fn formatting() {
        assert_eq!(duration(5), "5s");
        assert_eq!(duration(125), "2m05s");
        assert_eq!(duration(2 * 3600 + 14 * 60), "2h14m");
        assert_eq!(duration(90_000), "1d01h");
        assert_eq!(bytes(64 * 1024 * 1024), "64.0 MB");
    }
}

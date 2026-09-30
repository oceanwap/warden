//! Many apps on one host, PM2-style (`warden list`, `warden restart api`).
//!
//! There is no daemon: each app has its own supervisor (run by systemd as
//! `warden@<app>`, or started in the background by `warden start`). The CLI
//! finds apps through their configs in the config directory and their
//! control sockets in the runtime directory, and talks to each one directly.
//! Nothing here runs on the request path or is needed by a running app.

use crate::cli::{self, Action, Args, ScaleArg, StartOpts};
use crate::config::{self, Config};
use crate::control::{self, Request, Response, Status};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long `list` waits for one app before showing it as unreachable.
const STATUS_TIMEOUT: Duration = Duration::from_secs(1);
/// Any other request (not a rollout: those are followed separately).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    pub name: String,
    pub namespace: String,
    /// Config file, when known.
    pub config: Option<PathBuf>,
    pub socket: PathBuf,
    /// Why the config could not be loaded, if it couldn't.
    pub problem: Option<String>,
}

/// The apps a command can act on.
pub struct Ctx {
    pub apps: Vec<App>,
    /// `-c` / `--socket` / `$WARDEN_CONFIG`: one app, as in single-app use.
    pub single: bool,
}

// ------------------------------------------------------------------ places

fn home() -> Option<PathBuf> {
    std::env::var_os("WARDEN_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn is_root() -> bool {
    // SAFETY: geteuid never fails.
    unsafe { libc::geteuid() == 0 }
}

fn user_dir(xdg: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(xdg).filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("warden"),
        None => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
            home.join(fallback).join("warden")
        }
    }
}

/// App configs: `$WARDEN_HOME`, `/etc/warden` for root, else `~/.config/warden`.
pub fn config_dir() -> PathBuf {
    if let Some(h) = home() {
        return h;
    }
    if is_root() { PathBuf::from("/etc/warden") } else { user_dir("XDG_CONFIG_HOME", ".config") }
}

/// Saved state (`warden save`) and background supervisors' log files.
pub fn state_dir() -> PathBuf {
    if let Some(h) = home() {
        return h.join("state");
    }
    if is_root() { PathBuf::from("/var/lib/warden") } else { user_dir("XDG_STATE_HOME", ".local/state") }
}

fn dump_path() -> PathBuf {
    state_dir().join("dump.json")
}

fn log_path(name: &str) -> PathBuf {
    state_dir().join("logs").join(format!("{name}.log"))
}

// --------------------------------------------------------------- discovery

fn app_from_config(path: &Path) -> App {
    match Config::load(path) {
        Ok(c) => App {
            name: c.app.name.clone(),
            namespace: c.app.namespace.clone().unwrap_or_else(|| "default".into()),
            config: Some(path.to_path_buf()),
            socket: c.socket_path(),
            problem: None,
        },
        Err(e) => {
            let socket = config::socket_path_lenient(path);
            let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            App {
                socket: socket.unwrap_or_else(|| config::app_runtime_dir(&name).join("control.sock")),
                name,
                namespace: "default".into(),
                config: Some(path.to_path_buf()),
                problem: Some(e),
            }
        }
    }
}

/// Every app on this host: configs in the config directory, plus running
/// supervisors found in the runtime directory (started with `-c` elsewhere).
pub fn discover() -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    let dir = config_dir();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut paths: Vec<PathBuf> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml") && p.is_file())
            .collect();
        paths.sort();
        for p in paths {
            let app = app_from_config(&p);
            if !apps.iter().any(|a| a.name == app.name) {
                apps.push(app);
            }
        }
    }
    if let Ok(rd) = std::fs::read_dir(config::runtime_dir()) {
        let mut found: Vec<App> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let sock = e.path().join("control.sock");
                let name = e.file_name().to_string_lossy().to_string();
                (sock.exists() && !apps.iter().any(|a| a.name == name)).then(|| App {
                    name,
                    namespace: "default".into(),
                    config: None,
                    socket: sock,
                    problem: None,
                })
            })
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        apps.extend(found);
    }
    apps
}

/// Single-app context when the user named a config or socket; otherwise
/// every app on the host (falling back to ./warden.toml when none exist).
pub fn context(args: &Args) -> Ctx {
    if let Some(sock) = &args.socket {
        let name = sock
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "app".into());
        let mut app = args.config.as_deref().map(app_from_config).unwrap_or(App {
            name,
            namespace: "default".into(),
            config: None,
            socket: sock.clone(),
            problem: None,
        });
        app.socket = sock.clone();
        return Ctx { apps: vec![app], single: true };
    }
    if let Some(c) = &args.config {
        return Ctx { apps: vec![app_from_config(c)], single: true };
    }
    let apps = discover();
    let local = Path::new("warden.toml");
    if apps.is_empty() && local.is_file() {
        return Ctx { apps: vec![app_from_config(local)], single: true };
    }
    Ctx { apps, single: false }
}

// ------------------------------------------------------------------ targets

#[derive(Debug, Clone, PartialEq)]
pub struct Sel {
    pub app: App,
    pub worker: Option<usize>,
}

/// `all`, an app name, a namespace, `app:N`; in a single-app context also
/// `N` or `:N` for a worker. `need`: a mutating command must name its target
/// when there are several apps.
pub fn resolve(ctx: &Ctx, target: Option<&str>, need: bool) -> Result<Vec<Sel>, String> {
    let all = |w: Option<usize>| ctx.apps.iter().map(|a| Sel { app: a.clone(), worker: w }).collect::<Vec<_>>();
    let Some(t) = target else {
        if ctx.single || !need || ctx.apps.len() == 1 && ctx.apps[0].config.is_none() {
            return Ok(all(None));
        }
        return Err(no_target_error(ctx));
    };
    if ctx.apps.is_empty() {
        return Err(no_apps_error());
    }
    if t == "all" {
        return Ok(all(None));
    }
    let (name, worker) = match t.rsplit_once(':') {
        Some((n, w)) => {
            let w: usize = w.parse().map_err(|_| format!("{t:?}: the part after ':' must be a worker number"))?;
            (n, Some(w))
        }
        None => (t, None),
    };
    if ctx.single {
        let app = &ctx.apps[0];
        if name.is_empty() || name == app.name {
            return Ok(vec![Sel { app: app.clone(), worker }]);
        }
        if let Ok(n) = name.parse::<usize>() {
            return Ok(vec![Sel { app: app.clone(), worker: Some(n) }]);
        }
        return Err(format!("{name:?} is not this app ({}); the config names one app", app.name));
    }
    if let Some(app) = ctx.apps.iter().find(|a| a.name == name) {
        return Ok(vec![Sel { app: app.clone(), worker }]);
    }
    let in_ns: Vec<Sel> =
        ctx.apps.iter().filter(|a| a.namespace == name).map(|a| Sel { app: a.clone(), worker: None }).collect();
    if !in_ns.is_empty() {
        if worker.is_some() {
            return Err(format!("{t:?}: a worker number needs an app, not a namespace"));
        }
        return Ok(in_ns);
    }
    if name.parse::<usize>().is_ok() {
        return Err(format!(
            "{t:?}: Warden names apps, not numeric ids like PM2. Use the app name (`warden restart api`) \
             or one worker (`warden restart api:2`); `warden list` shows both"
        ));
    }
    Err(format!("no app or namespace named {name:?}; `warden list` shows what is on this host"))
}

fn no_apps_error() -> String {
    format!(
        "no apps found in {} or {}. Start one with `warden start server.js --name api`, \
         or pass a config with -c",
        config_dir().display(),
        config::runtime_dir().display()
    )
}

fn no_target_error(ctx: &Ctx) -> String {
    if ctx.apps.is_empty() {
        return no_apps_error();
    }
    let names: Vec<&str> = ctx.apps.iter().map(|a| a.name.as_str()).collect();
    format!("which app? name one ({}) or `all`", names.join(", "))
}

// ------------------------------------------------------------------ calling

async fn call_with(app: &App, req: &Request, timeout: Duration) -> Result<Response, String> {
    let mut sink = std::io::sink();
    match tokio::time::timeout(timeout, control::call(&app.socket, req, &mut sink)).await {
        Ok(Ok(Some(r))) => Ok(r),
        Ok(Ok(None)) => Err("no response".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("no answer within {} s (is the supervisor stopped or overloaded?)", timeout.as_secs())),
    }
}

async fn status_of(app: &App) -> Result<Status, String> {
    let r = call_with(app, &Request::Status, STATUS_TIMEOUT).await?;
    r.status.ok_or_else(|| r.message.unwrap_or_else(|| "no status".into()))
}

fn reachable(app: &App) -> bool {
    std::os::unix::net::UnixStream::connect(&app.socket).is_ok()
}

/// Status of every app, queried in parallel.
pub async fn statuses(apps: &[App]) -> Vec<(App, Result<Status, String>)> {
    let mut set = tokio::task::JoinSet::new();
    for (i, app) in apps.iter().cloned().enumerate() {
        set.spawn(async move {
            let st = if app.socket.exists() { status_of(&app).await } else { Err("not running".into()) };
            (i, app, st)
        });
    }
    let mut out = Vec::with_capacity(apps.len());
    while let Some(r) = set.join_next().await {
        if let Ok(v) = r {
            out.push(v);
        }
    }
    out.sort_by_key(|(i, ..)| *i);
    out.into_iter().map(|(_, a, s)| (a, s)).collect()
}

// ----------------------------------------------------------------- commands

/// Commands that act on running apps.
pub async fn act(args: &Args, action: &Action) -> i32 {
    let ctx = context(args);
    let need = !matches!(action, Action::List | Action::Logs { .. } | Action::LogLevel(_));
    let sels = match resolve(&ctx, args.target.as_deref(), need) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warden: {e}");
            return 2;
        }
    };
    match action {
        Action::List => list(&sels, args, ctx.single).await,
        Action::Describe => describe(&sels, args).await,
        Action::Env { show_secrets } => env(&sels, *show_secrets).await,
        Action::Config { show_secrets } => show_config(&sels, *show_secrets).await,
        Action::Logs { lines, follow, history, query } => {
            logs(&ctx, &sels, *lines, *follow, *history, query, args.json).await
        }
        Action::Scale(s) => scale(&ctx, &sels, s, args).await,
        _ => ops(&ctx, &sels, action, args).await,
    }
}

async fn list(sels: &[Sel], args: &Args, single: bool) -> i32 {
    let apps: Vec<App> = sels.iter().map(|s| s.app.clone()).collect();
    let all = statuses(&apps).await;
    if single {
        // One app (-c): its status object, as `warden status --json` always printed.
        return match &all[..] {
            [(_, Ok(st))] => {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(st).unwrap_or_default());
                } else {
                    print!("{}", cli::render_status(st, args.table_only));
                }
                0
            }
            [(app, Err(e))] => {
                eprintln!("warden: cannot reach warden at {} ({e}). Is it running?", app.socket.display());
                2
            }
            _ => 2,
        };
    }
    if args.json {
        let v: Vec<serde_json::Value> = all
            .iter()
            .map(|(a, st)| {
                serde_json::json!({
                    "app": a.name,
                    "namespace": a.namespace,
                    "config": a.config,
                    "socket": a.socket,
                    "problem": a.problem,
                    "status": st.as_ref().ok(),
                    "error": st.as_ref().err(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    if all.len() == 1 && sels.first().is_some_and(|s| s.worker.is_none()) {
        if let (_, Ok(st)) = &all[0] {
            // One app: the detailed view, as before.
            print!("{}", cli::render_status(st, args.table_only));
            return 0;
        }
    }
    // Offline apps are a state to show, not an error (as with `pm2 list`).
    print!("{}", cli::render_list(&all));
    0
}

async fn describe(sels: &[Sel], args: &Args) -> i32 {
    let mut code = 0;
    for (i, s) in sels.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let st = status_of(&s.app).await;
        let info = call_with(&s.app, &Request::Config { show_secrets: false }, REQUEST_TIMEOUT).await;
        match (st, info) {
            (Ok(st), Ok(info)) => {
                if args.json {
                    let v = serde_json::json!({"status": st, "info": info.info});
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                } else {
                    print!("{}", cli::render_describe(&st, info.info.as_ref().unwrap_or(&serde_json::Value::Null)));
                }
            }
            (Err(e), _) | (_, Err(e)) => {
                code = 2;
                eprintln!("warden: {}: not reachable ({e})", s.app.name);
                if let Some(c) = &s.app.config {
                    eprintln!("  config: {}", c.display());
                    eprintln!("  start it with `warden start {}`", s.app.name);
                }
                if let Some(p) = &s.app.problem {
                    eprintln!("  config problem: {p}");
                }
            }
        }
    }
    code
}

async fn env(sels: &[Sel], show_secrets: bool) -> i32 {
    let mut code = 0;
    for s in sels {
        match call_with(&s.app, &Request::Config { show_secrets }, REQUEST_TIMEOUT).await {
            Ok(r) => {
                if sels.len() > 1 {
                    println!("# {}", s.app.name);
                }
                let info = r.info.unwrap_or_default();
                if let Some(env) = info.pointer("/config/app/env").and_then(|e| e.as_object()) {
                    for (k, v) in env {
                        println!("{k}={}", v.as_str().unwrap_or_default());
                    }
                }
                println!(
                    "# also set by Warden for each worker: PORT, WARDEN_APP, WARDEN_WORKER_ID, WARDEN_WORKER_COUNT, \
                     WARDEN_MODE{}",
                    if show_secrets { "" } else { "  (values hidden: --show-secrets)" }
                );
            }
            Err(e) => {
                code = 2;
                eprintln!("warden: {}: {e}", s.app.name);
            }
        }
    }
    code
}

async fn show_config(sels: &[Sel], show_secrets: bool) -> i32 {
    let mut code = 0;
    for s in sels {
        match call_with(&s.app, &Request::Config { show_secrets }, REQUEST_TIMEOUT).await {
            Ok(r) => println!("{}", serde_json::to_string_pretty(&r.info.unwrap_or_default()).unwrap_or_default()),
            Err(e) => {
                code = 2;
                eprintln!("warden: {}: {e}", s.app.name);
            }
        }
    }
    code
}

async fn scale(ctx: &Ctx, sels: &[Sel], s: &ScaleArg, args: &Args) -> i32 {
    if sels.len() != 1 {
        eprintln!("warden: scale one app at a time");
        return 2;
    }
    let app = &sels[0].app;
    let count = match s {
        ScaleArg::To(n) => *n,
        ScaleArg::By(d) => match status_of(app).await {
            Ok(st) => {
                let n = st.workers_configured as i64 + d;
                if n < 1 {
                    eprintln!("warden: {}: cannot go below 1 worker (now {})", app.name, st.workers_configured);
                    return 1;
                }
                n as usize
            }
            Err(e) => {
                eprintln!("warden: {}: {e}", app.name);
                return 2;
            }
        },
    };
    one_op(ctx, app, Request::Scale { count }, args).await
}

/// Send one request and report; follow a rollout when one starts.
async fn one_op(ctx: &Ctx, app: &App, req: Request, args: &Args) -> i32 {
    let prefix = if ctx.single { String::new() } else { format!("{}: ", app.name) };
    match call_with(app, &req, REQUEST_TIMEOUT).await {
        Err(e) => {
            eprintln!("warden: {prefix}{e}");
            2
        }
        Ok(r) => {
            if let Some(m) = r.message.as_deref().filter(|m| !m.is_empty()) {
                if r.ok { println!("{prefix}{m}") } else { eprintln!("warden: {prefix}{m}") }
            }
            match (r.ok, r.seq) {
                (true, Some(seq)) if !args.no_wait => cli::wait_for_rollout(&app.socket, seq).await,
                (ok, _) => i32::from(!ok),
            }
        }
    }
}

async fn ops(ctx: &Ctx, sels: &[Sel], action: &Action, args: &Args) -> i32 {
    let mut worst = 0;
    for s in sels {
        let w = s.worker;
        let req = match action {
            Action::Stop | Action::Shutdown | Action::Flush | Action::Reload { .. } if w.is_some() => {
                eprintln!(
                    "warden: {}:{} - this command works on whole apps; `warden restart {}:{}` replaces one worker",
                    s.app.name,
                    w.unwrap_or(0),
                    s.app.name,
                    w.unwrap_or(0)
                );
                return 2;
            }
            Action::Stop => Request::Stop,
            Action::Shutdown => Request::Shutdown,
            Action::Restart { hard } => Request::Restart { worker: w, hard: *hard },
            Action::Reload { safe } => Request::Reload { safe: *safe },
            Action::Reset => Request::Reset { worker: w },
            Action::Flush => Request::Flush,
            Action::Signal(sig) => Request::Signal { signal: sig.clone(), worker: w },
            Action::LogLevel(level) => Request::LogLevel { level: *level },
            _ => return 2,
        };
        if !ctx.single && !s.app.socket.exists() {
            eprintln!("warden: {}: not running (start it with `warden start {}`)", s.app.name, s.app.name);
            worst = worst.max(2);
            continue;
        }
        let code = one_op(ctx, &s.app, req, args).await;
        worst = worst.max(code);
        // Deploys across several apps stop at the first failure.
        if code == 1 && matches!(action, Action::Reload { .. } | Action::Restart { hard: false }) && sels.len() > 1 {
            let rest: Vec<&str> =
                sels.iter().skip_while(|x| x.app.name != s.app.name).skip(1).map(|x| x.app.name.as_str()).collect();
            if !rest.is_empty() {
                eprintln!("warden: stopped here; not reloaded: {}", rest.join(", "));
            }
            break;
        }
    }
    worst
}

// --------------------------------------------------------------------- logs

async fn logs(
    ctx: &Ctx,
    sels: &[Sel],
    lines: Option<usize>,
    follow: Option<bool>,
    history: bool,
    query: &crate::logview::Query,
    json: bool,
) -> i32 {
    if history {
        return logs_history(sels, lines, query, json).await;
    }
    // Like PM2: stream by default on a terminal; print and exit when piped.
    // SAFETY: isatty has no preconditions.
    let follow = follow.unwrap_or_else(|| unsafe { libc::isatty(1) == 1 });
    let lines = lines.unwrap_or(15);
    // Text / time / level filters run here; ask for more so N survive them.
    let fetch = if query.is_filtering() { 4000 } else { lines };
    let req = |w: Option<usize>, n: usize, follow: bool| Request::Logs {
        lines: n,
        follow,
        worker: w.map(|n| n.to_string()).or_else(|| query.worker.clone()),
        events: query.events || query.level.is_some(),
        stream: query.stream.clone(),
    };
    let multi = sels.len() > 1;
    let width = sels.iter().map(|s| s.app.name.len()).max().unwrap_or(0);
    let format = |app: &str, l: &str| -> String {
        if json {
            crate::logview::to_json(app, l)
        } else if multi {
            format!("{app:<width$} | {l}")
        } else {
            l.to_string()
        }
    };
    let mut out = crate::logview::PipeOut::new();
    let mut recent: Vec<(String, String)> = Vec::new();
    let mut live = Vec::new();
    let mut worst = 0;
    for s in sels {
        if multi && !s.app.socket.exists() {
            continue;
        }
        let mut buf: Vec<u8> = Vec::new();
        match control::call(&s.app.socket, &req(s.worker, fetch, false), &mut buf).await {
            Ok(_) => {
                let text = String::from_utf8_lossy(&buf).to_string();
                let kept: Vec<&str> = text.lines().filter(|l| query.matches(l)).collect();
                for l in &kept[kept.len().saturating_sub(lines)..] {
                    recent.push((l.get(..24).unwrap_or("").to_string(), format(&s.app.name, l)));
                }
                live.push(s.clone());
            }
            Err(e) => {
                let prefix = if ctx.single { String::new() } else { format!("{}: ", s.app.name) };
                eprintln!("warden: {prefix}{e}");
                worst = 2;
            }
        }
    }
    recent.sort_by(|a, b| a.0.cmp(&b.0));
    for (_, l) in &recent {
        if !out.line(l) {
            return 0;
        }
    }
    out.flush();
    if !follow || live.is_empty() {
        return worst;
    }
    let mut set = tokio::task::JoinSet::new();
    for s in live {
        let r = req(s.worker, 0, true);
        let (q, app) = (query.clone(), s.app.name.clone());
        let prefix = if multi { format!("{app:<width$} | ") } else { String::new() };
        set.spawn(async move {
            let mut w = Filtered { prefix, app, json, query: q, buf: Vec::new(), out: crate::logview::PipeOut::new() };
            let _ = control::call(&s.app.socket, &r, &mut w).await;
        });
    }
    while set.join_next().await.is_some() {}
    worst
}

/// `warden logs --history` / `warden search`: the app's log files (rotated
/// and gzipped ones too, oldest first) or journald.
async fn logs_history(sels: &[Sel], lines: Option<usize>, query: &crate::logview::Query, json: bool) -> i32 {
    use crate::logview::{file_chain, for_each_line, journal_lines};
    let multi = sels.len() > 1;
    let width = sels.iter().map(|s| s.app.name.len()).max().unwrap_or(0);
    let mut out = crate::logview::PipeOut::new();
    let mut worst = 0;
    for s in sels {
        let app = &s.app;
        let mut q = query.clone();
        if let Some(w) = s.worker {
            q.worker = Some(w.to_string());
        }
        // Where this app logs: from the running supervisor, else its config.
        let (cfg, unit) = match call_with(app, &Request::Config { show_secrets: false }, REQUEST_TIMEOUT).await {
            Ok(r) => {
                let info = r.info.unwrap_or_default();
                let cfg: Option<Config> = serde_json::from_value(info["config"].clone()).ok();
                let unit = info["unit"].as_str().map(String::from);
                let bg = info["log_file"].as_str().map(PathBuf::from);
                (cfg.map(|c| (c, bg)), unit)
            }
            Err(_) => {
                (app.config.as_ref().and_then(|p| Config::load(p).ok()).map(|c| (c, None)), systemd_unit_for(app))
            }
        };
        let Some((cfg, running_log)) = cfg else {
            eprintln!("warden: {}: cannot read its config to find its log files", app.name);
            worst = 2;
            continue;
        };
        let l = &cfg.logging;
        let background = Some(log_path(&app.name)).filter(|p| p.exists());
        // (path, raw): raw = the app's own lines (out/err files), not Warden-framed.
        let mut sources: Vec<(PathBuf, bool)> = Vec::new();
        match q.stream.as_deref() {
            Some("stderr") if l.err_file.is_some() => sources.push((l.err_file.clone().unwrap_or_default(), true)),
            Some("stdout") if l.out_file.is_some() => sources.push((l.out_file.clone().unwrap_or_default(), true)),
            _ => {
                if let Some(f) = l.file.clone().or(running_log).or(background) {
                    sources.push((f, false));
                } else {
                    for f in [l.out_file.clone(), l.err_file.clone()].into_iter().flatten() {
                        sources.push((f, true));
                    }
                }
            }
        }
        let emit_prefix = |line: &str| -> String {
            if json {
                crate::logview::to_json(&app.name, line)
            } else if multi {
                format!("{:<width$} | {line}", app.name)
            } else {
                line.to_string()
            }
        };
        // Keep only the last N when asked; otherwise stream everything.
        let mut tail: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut push = |line: String, out: &mut crate::logview::PipeOut| -> bool {
            match lines {
                Some(n) => {
                    tail.push_back(line);
                    if tail.len() > n {
                        tail.pop_front();
                    }
                    true
                }
                None => out.line(&line),
            }
        };
        if sources.is_empty() {
            match unit.as_deref() {
                Some(u) => {
                    let res = journal_lines(u, &q, &mut |line| {
                        if q.matches(line) { push(emit_prefix(line), &mut out) } else { true }
                    });
                    if let Err(e) = res {
                        eprintln!("warden: {}: {e}", app.name);
                        worst = 2;
                    }
                }
                None => {
                    eprintln!(
                        "warden: {}: no log files to read. Set [logging] file (or out_file / err_file) to keep \
                         history; `warden logs {}` shows the recent lines in memory",
                        app.name, app.name
                    );
                    worst = worst.max(1);
                    continue;
                }
            }
        }
        for (path, raw) in &sources {
            let chain = file_chain(path);
            if chain.is_empty() {
                eprintln!("warden: {}: {} does not exist yet", app.name, path.display());
                worst = worst.max(1);
            }
            for f in chain {
                let res = for_each_line(&f, &mut |line| {
                    let keep = if *raw { q.matches_raw(line) } else { q.matches(line) };
                    if keep { push(emit_prefix(line), &mut out) } else { true }
                });
                match res {
                    Ok(true) => {}
                    Ok(false) => return 0, // the reader went away (| head)
                    Err(e) => {
                        eprintln!("warden: {}: {e}", app.name);
                        worst = worst.max(1);
                    }
                }
            }
        }
        for line in tail {
            if !out.line(&line) {
                return 0;
            }
        }
    }
    out.flush();
    worst
}

/// Follow mode: filter each line, prefix it, and stop when stdout closes.
struct Filtered {
    prefix: String,
    app: String,
    json: bool,
    query: crate::logview::Query,
    buf: Vec<u8>,
    out: crate::logview::PipeOut,
}

impl std::io::Write for Filtered {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(data);
        while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
            let raw: Vec<u8> = self.buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&raw[..raw.len() - 1]).to_string();
            if !self.query.matches(&line) {
                continue;
            }
            let text =
                if self.json { crate::logview::to_json(&self.app, &line) } else { format!("{}{line}", self.prefix) };
            if !self.out.line(&text) {
                return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stdout closed"));
            }
            self.out.flush();
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn systemctl() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WARDEN_SYSTEMCTL").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if !is_root() || !Path::new("/run/systemd/system").is_dir() {
        return None;
    }
    ["/usr/bin/systemctl", "/bin/systemctl"].iter().map(PathBuf::from).find(|p| p.exists())
}

fn unit_dir() -> PathBuf {
    std::env::var_os("WARDEN_UNIT_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/systemd/system"))
}

fn unit_installed() -> bool {
    unit_dir().join("warden@.service").exists()
        || Path::new("/lib/systemd/system/warden@.service").exists()
        || Path::new("/usr/lib/systemd/system/warden@.service").exists()
}

fn run_systemctl(args: &[&str]) -> Result<(), String> {
    let Some(bin) = systemctl() else { return Err("systemd is not available here".into()) };
    let out = std::process::Command::new(&bin)
        .args(args)
        .output()
        .map_err(|e| format!("running {} {}: {e}", bin.display(), args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`systemctl {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("no output")
        ))
    }
}

/// The unit that `warden@.service` would run for this app, if systemd can.
fn systemd_unit_for(app: &App) -> Option<String> {
    systemctl()?;
    let cfg = app.config.as_ref()?;
    let expected = unit_config_path(&app.name);
    (unit_installed() && same_file(cfg, &expected)).then(|| format!("warden@{}.service", app.name))
}

/// `warden@.service` reads `/etc/warden/<app>.toml`.
fn unit_config_path(name: &str) -> PathBuf {
    if let Some(h) = home() {
        return h.join(format!("{name}.toml"));
    }
    PathBuf::from(format!("/etc/warden/{name}.toml"))
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// `warden start <app | config.toml | script>`.
pub async fn start(args: &Args, what: &str, opts: &StartOpts) -> i32 {
    let ctx = context(args);
    // 1. Apps we know (name, namespace, all).
    if let Ok(sels) = resolve(&ctx, Some(what), true) {
        let mut worst = 0;
        for s in sels {
            worst = worst.max(start_app(&ctx, &s.app).await);
        }
        return worst;
    }
    let path = Path::new(what);
    // 2. A config file.
    if path.extension().is_some_and(|x| x == "toml") && path.is_file() {
        let app = app_from_config(path);
        if let Some(p) = &app.problem {
            eprintln!("warden: {p}");
            return 1;
        }
        return start_app(&ctx, &app).await;
    }
    // 3. PM2 ecosystem files are imported once.
    let lower = what.to_ascii_lowercase();
    if lower.contains("ecosystem") || lower.ends_with(".config.js") || lower.ends_with(".config.cjs") {
        eprintln!(
            "warden: {what} looks like a PM2 ecosystem file. Import it once with \
             `warden pm2-migrate --from {what}`, then use `warden start <app>`"
        );
        return 2;
    }
    // 4. Anything else PM2 would run: a script, a program, a command line.
    let what_kind = match launch_kind(what) {
        Some(k) => k,
        None => {
            eprintln!(
                "warden: no app, config file, script or program named {what:?}; `warden list` shows the apps on \
                 this host. To run a command line, quote it: warden start \"python3 worker.py\" --name worker"
            );
            return 2;
        }
    };
    match quick_config(&what_kind, opts) {
        Ok((name, text)) => {
            let file = config_dir().join(format!("{name}.toml"));
            if file.exists() {
                eprintln!(
                    "warden: app {name:?} already exists ({}). Use `warden start {name}`, or `warden delete {name}` \
                     first to replace it",
                    file.display()
                );
                return 1;
            }
            if let Err(e) = write_private(&file, &text, 0o644) {
                eprintln!("warden: {e}");
                return 1;
            }
            println!("{name}: wrote {} (edit it for health checks, limits and more)", file.display());
            start_app(&ctx, &app_from_config(&file)).await
        }
        Err(e) => {
            eprintln!("warden: {e}");
            2
        }
    }
}

/// `warden serve <dir> [port]`: an app whose workers are Warden's static server.
pub async fn serve(args: &Args, dir: &Path, port: u16, o: &StartOpts) -> i32 {
    let ctx = context(args);
    let root = match std::fs::canonicalize(dir) {
        Ok(r) if r.is_dir() => r,
        Ok(r) => {
            eprintln!("warden: {} is not a directory", r.display());
            return 2;
        }
        Err(e) => {
            eprintln!("warden: {}: {e}", dir.display());
            return 2;
        }
    };
    let default_name = root.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "static".into());
    let name = sanitize_name(&o.name.clone().unwrap_or(default_name));
    let count = match o.instances.as_deref() {
        None => "1".to_string(),
        Some("max") | Some("0") => "\"max\"".into(),
        Some("-1") | Some("max-1") => "\"max-1\"".into(),
        Some(n) => match n.parse::<usize>() {
            Ok(n) => n.to_string(),
            Err(_) => {
                eprintln!("warden: -i {n:?}: expected a number or \"max\"");
                return 2;
            }
        },
    };
    let mut t = format!(
        "# Written by `warden serve`. Every setting: warden.example.toml\n\n[app]\nname = {}\nport = {port}\n",
        toml_str(&name)
    );
    if let Some(ns) = &o.namespace {
        t += &format!("namespace = {}\n", toml_str(ns));
    }
    t += &format!("\n[workers]\ncount = {count}\n\n[static]\nroot = {}\n", toml_str(&root.display().to_string()));
    if o.spa {
        t += "spa = true\n";
    }
    if o.listing {
        t += "listing = true\n";
    }
    if let Some(a) = &o.basic_auth {
        t += &format!("basic_auth = {}\n", toml_str(a));
    }
    if let Err(e) = Config::parse(&t) {
        eprintln!("warden: the generated config is invalid: {e}");
        return 2;
    }
    let file = config_dir().join(format!("{name}.toml"));
    if file.exists() {
        eprintln!(
            "warden: app {name:?} already exists ({}). Use `warden start {name}`, or `warden delete {name}` first",
            file.display()
        );
        return 1;
    }
    // Basic auth credentials live in it: owner-only.
    let mode = if o.basic_auth.is_some() { 0o600 } else { 0o644 };
    if let Err(e) = write_private(&file, &t, mode) {
        eprintln!("warden: {e}");
        return 1;
    }
    println!("{name}: serving {} on port {port} (config {})", root.display(), file.display());
    start_app(&ctx, &app_from_config(&file)).await
}

/// What `warden start <what>` runs when `what` is not an app or a config.
#[derive(Debug, Clone, PartialEq)]
pub enum Launch {
    /// A file: run with an interpreter picked by extension, or directly.
    Script(PathBuf),
    /// A program on PATH (`npm`, `python3`, `redis-server`).
    Program(String),
    /// A command line, run with `sh -c`.
    Shell(String),
}

pub fn launch_kind(what: &str) -> Option<Launch> {
    let p = Path::new(what);
    if p.is_file() {
        return Some(Launch::Script(p.to_path_buf()));
    }
    if what.chars().any(char::is_whitespace) {
        return Some(Launch::Shell(what.to_string()));
    }
    if !what.contains('/') && which(what) {
        return Some(Launch::Program(what.to_string()));
    }
    None
}

/// Interpreter for a script by its extension; `None` = run it directly.
fn interpreter_for(ext: &str) -> Result<Option<String>, String> {
    Ok(Some(
        match ext {
            "ts" | "tsx" | "mts" | "cts" => "bun",
            "js" | "mjs" | "cjs" | "jsx" => {
                if which("node") {
                    "node"
                } else {
                    "bun"
                }
            }
            "py" => "python3",
            "sh" => "sh",
            "bash" => "bash",
            "rb" => "ruby",
            "pl" => "perl",
            "php" => "php",
            "lua" => "lua",
            _ => return Ok(None),
        }
        .to_string(),
    ))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

/// Shell text that needs `sh` itself (so no `exec` prefix).
fn shell_syntax(cmd: &str) -> bool {
    ["&&", "||", ";", "|", "&", ">", "<", "$(", "`", "\n", "cd "].iter().any(|t| cmd.contains(t))
}

fn sanitize_name(s: &str) -> String {
    let n: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '-' }).collect();
    let n = n.trim_matches('-').to_string();
    if n.is_empty() || n == "all" { "app".into() } else { n }
}

async fn start_app(ctx: &Ctx, app: &App) -> i32 {
    let prefix = format!("{}: ", app.name);
    if reachable(app) {
        return match call_with(app, &Request::Start, REQUEST_TIMEOUT).await {
            Ok(r) if r.ok => {
                println!("{prefix}{}", r.message.unwrap_or_default());
                wait_ready(app, Duration::from_secs(30)).await
            }
            Ok(r) => {
                eprintln!("warden: {prefix}{}", r.message.unwrap_or_default());
                1
            }
            Err(e) => {
                eprintln!("warden: {prefix}{e}");
                2
            }
        };
    }
    if let Some(p) = &app.problem {
        eprintln!("warden: {prefix}{p}");
        return 1;
    }
    let Some(cfg) = app.config.clone() else {
        eprintln!("warden: {prefix}not running and no config file is known for it");
        return 2;
    };
    let _ = ctx;
    if let Some(unit) = systemd_unit_for(app) {
        if let Err(e) = run_systemctl(&["start", &unit]) {
            eprintln!("warden: {prefix}{e}\n  see `journalctl -u {unit} -n 50`");
            return 1;
        }
        println!("{prefix}started {unit}");
        return wait_ready(app, Duration::from_secs(60)).await;
    }
    match spawn_background(&app.name, &cfg) {
        Ok(mut child) => {
            let log = log_path(&app.name);
            println!("{prefix}supervisor started in the background (pid {}), log {}", child.id(), log.display());
            // Wait for the control socket, watching for an early exit.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(15) {
                if let Ok(Some(st)) = child.try_wait() {
                    eprintln!("warden: {prefix}the supervisor exited ({st}) while starting; last log lines:");
                    for l in tail(&log, 15) {
                        eprintln!("  {l}");
                    }
                    return 1;
                }
                if reachable(app) {
                    return wait_ready(app, Duration::from_secs(60)).await;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            eprintln!(
                "warden: {prefix}the supervisor did not open its control socket within 15 s; see {}",
                log.display()
            );
            1
        }
        Err(e) => {
            eprintln!("warden: {prefix}{e}");
            1
        }
    }
}

/// Wait until every worker is ready (or the app is stopped), then print one line.
async fn wait_ready(app: &App, limit: Duration) -> i32 {
    let t0 = Instant::now();
    let mut last = None;
    while t0.elapsed() < limit {
        if let Ok(st) = status_of(app).await {
            if st.stopped || (st.workers_ready >= st.workers_configured && st.workers_configured > 0) {
                println!(
                    "{}: {} ({}/{} workers ready)",
                    app.name,
                    if st.stopped { "stopped (restored from `warden save`)" } else { "online" },
                    st.workers_ready,
                    st.workers_configured
                );
                return 0;
            }
            if st.workers.iter().any(|w| w.state == "FAILED") {
                eprintln!("warden: {}: a worker is FAILED; `warden describe {}` shows why", app.name, app.name);
                return 1;
            }
            last = Some(st);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match last {
        Some(st) => eprintln!(
            "warden: {}: only {}/{} workers ready after {} s; `warden logs {}` shows why",
            app.name,
            st.workers_ready,
            st.workers_configured,
            limit.as_secs(),
            app.name
        ),
        None => eprintln!("warden: {}: not answering; see {}", app.name, log_path(&app.name).display()),
    }
    1
}

/// Run a supervisor detached from this terminal, logging to a rotated file.
fn spawn_background(name: &str, cfg: &Path) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the warden binary: {e}"))?;
    let log = log_path(name);
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    let err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("opening {}: {e}", log.display()))?;
    let cfg = std::fs::canonicalize(cfg).unwrap_or_else(|_| cfg.to_path_buf());
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["start", "-c"])
        .arg(&cfg)
        .env("WARDEN_LOG_FILE", &log)
        .env("WARDEN_LOG_STDOUT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err);
    // SAFETY: setsid is async-signal-safe; it detaches from our terminal and
    // process group so Ctrl-C here or closing the shell does not reach it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().map_err(|e| format!("starting the supervisor: {e}"))
}

fn tail(path: &Path, n: usize) -> Vec<String> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

fn write_private(path: &Path, text: &str, mode: u32) -> Result<(), String> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&tmp)
        .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    f.write_all(text.as_bytes()).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    f.sync_all().map_err(|e| format!("writing {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("writing {}: {e}", path.display()))
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").map(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file())).unwrap_or(false)
}

fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// The config `warden start <script | program | "command line"> [flags]`
/// writes. Flags follow PM2's names.
pub fn quick_config(what: &Launch, o: &StartOpts) -> Result<(String, String), String> {
    let cwd_now = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    let (default_name, command, mut args, dir) = match what {
        Launch::Script(script) => {
            let abs = std::fs::canonicalize(script).map_err(|e| format!("{}: {e}", script.display()))?;
            let dir = abs.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"));
            let stem = abs.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "app".into());
            let ext = abs.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_default();
            let interpreter = match o.interpreter.as_deref() {
                Some("none") => None,
                Some(i) => Some(i.to_string()),
                None => interpreter_for(&ext)?,
            };
            let file = abs.display().to_string();
            match interpreter {
                Some(i) => {
                    let mut a = o.interpreter_args.clone();
                    a.push(file);
                    (stem, i, a, dir)
                }
                None if is_executable(&abs) => (stem, file, vec![], dir),
                None => {
                    return Err(format!(
                        "don't know how to run {}: it is not executable and .{ext} has no default interpreter; \
                         pass --interpreter <program>",
                        script.display()
                    ));
                }
            }
        }
        Launch::Program(p) => (p.clone(), p.clone(), vec![], cwd_now.clone()),
        Launch::Shell(cmd) => {
            let first = cmd.split_whitespace().next().unwrap_or("app");
            let base = Path::new(first).file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let line = if shell_syntax(cmd) { cmd.clone() } else { format!("exec {cmd}") };
            (base, "sh".to_string(), vec!["-c".to_string(), line], cwd_now.clone())
        }
    };
    args.extend(o.script_args.iter().cloned());
    let name = sanitize_name(&o.name.clone().unwrap_or(default_name));
    let count = match o.instances.as_deref() {
        None => "1".to_string(),
        Some("max") | Some("0") => "\"max\"".into(),
        Some("-1") | Some("max-1") => "\"max-1\"".into(),
        Some(n) => {
            n.parse::<usize>().map(|n| n.to_string()).map_err(|_| format!("-i {n:?}: expected a number or \"max\""))?
        }
    };
    let js = crate::config::is_bun(&command) || crate::config::is_node(&command);
    let shim = o.shim.unwrap_or(js);

    let mut t = String::new();
    let shown = match what {
        Launch::Script(p) => p.display().to_string(),
        Launch::Program(p) => p.clone(),
        Launch::Shell(c) => format!("\"{c}\""),
    };
    t += &format!("# Written by `warden start {shown}`. Every setting: warden.example.toml\n\n[app]\n");
    t += &format!("name = {}\n", toml_str(&name));
    if let Some(ns) = &o.namespace {
        t += &format!("namespace = {}\n", toml_str(ns));
    }
    t += &format!("command = {}\n", toml_str(&command));
    let quoted: Vec<String> = args.iter().map(|a| toml_str(a)).collect();
    t += &format!("args = [{}]\n", quoted.join(", "));
    t += &format!("working_directory = {}\n", toml_str(&o.cwd.clone().unwrap_or(dir).display().to_string()));
    if let Some(p) = o.port {
        t += &format!("port = {p}\n");
    }
    if o.shim.is_some() {
        t += &format!("shim = {shim}\n");
    }
    if !o.env.is_empty() {
        t += "\n[app.env]\n";
        for (k, v) in &o.env {
            t += &format!("{} = {}\n", toml_str(k), toml_str(v));
        }
    }
    t += &format!("\n[workers]\ncount = {count}\n");
    if o.wait_ready {
        t += "wait_ready = true\n";
    }
    if let Some(ms) = o.listen_timeout_ms {
        t += &format!("ready_timeout = {}\n", ms.div_ceil(1000).max(1));
    }
    let mut restart = String::new();
    if o.autorestart == Some(false) {
        restart += "enabled = false\n";
    }
    if let Some(ms) = o.restart_delay_ms {
        restart += &format!("backoff_initial = {}\n", ms.max(1));
        restart += &format!("backoff_max = {}\n", (ms.max(1) * 16).max(10_000));
    }
    if let Some(n) = o.max_restarts {
        restart += &format!("max_restarts = {n}\n");
    }
    if let Some(c) = &o.cron {
        restart += &format!("schedule = {}\n", toml_str(c));
    }
    if !o.stop_exit_codes.is_empty() {
        let codes: Vec<String> = o.stop_exit_codes.iter().map(|c| c.to_string()).collect();
        restart += &format!("stop_exit_codes = [{}]\n", codes.join(", "));
    }
    if !restart.is_empty() {
        t += &format!("\n[restart]\n{restart}");
    }
    let mut shutdown = String::new();
    if let Some(sig) = &o.kill_signal {
        shutdown += &format!("signal = {}\n", toml_str(sig));
    }
    if let Some(ms) = o.kill_timeout_ms {
        shutdown += &format!("grace_period = {}\n", ms.div_ceil(1000).max(1));
    }
    if !shutdown.is_empty() {
        t += &format!("\n[shutdown]\n{shutdown}");
    }
    if let Some(mb) = o.max_memory_mb {
        t += &format!("\n[limits]\nmax_memory = {mb}\n");
    }
    let abs = |p: &PathBuf| if p.is_absolute() { p.clone() } else { cwd_now.join(p) };
    let mut logging = String::new();
    if let Some(f) = &o.out_file {
        logging += &format!("out_file = {}\n", toml_str(&abs(f).display().to_string()));
    }
    if let Some(f) = &o.err_file {
        logging += &format!("err_file = {}\n", toml_str(&abs(f).display().to_string()));
    }
    if let Some(f) = &o.log_file {
        logging += &format!("file = {}\n", toml_str(&abs(f).display().to_string()));
    }
    if o.time {
        logging += "file_timestamps = true\n";
    }
    if (o.out_file.is_some() || o.err_file.is_some()) && !o.merge_logs && count != "1" {
        logging += "per_worker_files = true\n";
    }
    if !logging.is_empty() {
        t += &format!("\n[logging]\n{logging}");
    }
    Config::parse(&t).map_err(|e| format!("the generated config is invalid: {e}\n{t}"))?;
    Ok((name, t))
}

/// Stop the supervisor itself (systemd unit, or a background one).
async fn stop_supervisor(app: &App, st: Option<&Status>, disable: bool) -> Result<String, String> {
    if let Some(unit) = st.and_then(|s| s.unit.clone()).or_else(|| systemd_unit_for(app)) {
        let verb = if disable { vec!["disable", "--now"] } else { vec!["stop"] };
        let mut a = verb.clone();
        a.push(&unit);
        run_systemctl(&a)?;
        return Ok(format!("{} {unit}", if disable { "disabled and stopped" } else { "stopped" }));
    }
    if !reachable(app) {
        return Ok("was not running".into());
    }
    match call_with(app, &Request::Shutdown, REQUEST_TIMEOUT).await {
        Ok(r) if !r.ok => return Err(r.message.unwrap_or_default()),
        Ok(_) => {}
        // It may exit before its reply is written; the wait below decides.
        Err(e) => crate::debug!("shutdown request ended early", app = app.name, error = e),
    }
    let t0 = Instant::now();
    while reachable(app) {
        if t0.elapsed() > Duration::from_secs(120) {
            return Err("still running after 120 s".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok("stopped".into())
}

pub async fn delete(args: &Args, target: &str) -> i32 {
    let ctx = context(args);
    let sels = match resolve(&ctx, Some(target), true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warden: {e}");
            return 2;
        }
    };
    let mut worst = 0;
    for s in sels {
        let st = status_of(&s.app).await.ok();
        match stop_supervisor(&s.app, st.as_ref(), true).await {
            Ok(m) => println!("{}: {m}", s.app.name),
            Err(e) => {
                eprintln!("warden: {}: {e}", s.app.name);
                worst = 1;
                continue;
            }
        }
        if let Some(cfg) = s.app.config.as_ref().filter(|c| c.parent() == Some(config_dir().as_path())) {
            let dest = config_dir().join("deleted").join(cfg.file_name().unwrap_or_default());
            let moved = std::fs::create_dir_all(dest.parent().unwrap_or(Path::new("/")))
                .and_then(|_| std::fs::rename(cfg, &dest));
            match moved {
                Ok(()) => println!("{}: config moved to {}", s.app.name, dest.display()),
                Err(e) => {
                    eprintln!("warden: {}: could not move {}: {e}", s.app.name, cfg.display());
                    worst = 1;
                }
            }
        }
    }
    worst
}

pub async fn kill(args: &Args) -> i32 {
    let ctx = context(args);
    let sels = match resolve(&ctx, args.target.as_deref().or(Some("all")), false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warden: {e}");
            return 2;
        }
    };
    let running: Vec<&Sel> = sels.iter().filter(|s| s.app.socket.exists()).collect();
    if running.is_empty() {
        println!("no app is running");
        return 0;
    }
    // SAFETY: isatty has no preconditions.
    if !args.yes && unsafe { libc::isatty(0) == 1 } {
        let names: Vec<&str> = running.iter().map(|s| s.app.name.as_str()).collect();
        eprint!("Stop {} app(s): {}? [y/N] ", names.len(), names.join(", "));
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("nothing stopped");
            return 1;
        }
    }
    let mut worst = 0;
    for s in running {
        let st = status_of(&s.app).await.ok();
        match stop_supervisor(&s.app, st.as_ref(), false).await {
            Ok(m) => println!("{}: {m}", s.app.name),
            Err(e) => {
                eprintln!("warden: {}: {e}", s.app.name);
                worst = 1;
            }
        }
    }
    worst
}

// ------------------------------------------------------- save / resurrect

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct Saved {
    pub name: String,
    pub config: PathBuf,
    pub workers: usize,
    pub stopped: bool,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Dump {
    version: u32,
    saved_at: String,
    apps: Vec<Saved>,
}

fn read_dump() -> Result<Option<Dump>, String> {
    let p = dump_path();
    match std::fs::read_to_string(&p) {
        Ok(t) => serde_json::from_str(&t).map(Some).map_err(|e| format!("{}: {e}", p.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", p.display())),
    }
}

/// What `warden save` recorded for this app, if it was saved with this config.
/// The supervisor applies it at start, so a reboot restores `scale` and
/// `stop` like `pm2 resurrect` does.
pub fn saved_state(name: &str, cfg: Option<&Path>) -> Option<Saved> {
    let dump = match read_dump() {
        Ok(d) => d?,
        Err(e) => {
            crate::warn!(
                "ignoring the saved state: it could not be read",
                error = e,
                hint = "run `warden save` again to rewrite it"
            );
            return None;
        }
    };
    let cfg = cfg?;
    dump.apps.into_iter().find(|s| s.name == name && same_file(&s.config, cfg))
}

pub async fn save(args: &Args) -> i32 {
    let ctx = context(args);
    let all = statuses(&ctx.apps).await;
    let mut saved = Vec::new();
    for (app, st) in &all {
        let Ok(st) = st else { continue };
        let Some(cfg) = app.config.clone().or_else(|| st.config_path.clone().map(PathBuf::from)) else {
            eprintln!("warden: {}: running without a known config file; not saved", app.name);
            continue;
        };
        let cfg = std::fs::canonicalize(&cfg).unwrap_or(cfg);
        saved.push(Saved { name: app.name.clone(), config: cfg, workers: st.workers_configured, stopped: st.stopped });
    }
    let dump = Dump { version: 1, saved_at: crate::logging::timestamp_now(), apps: saved.clone() };
    let text = serde_json::to_string_pretty(&dump).unwrap_or_default();
    if let Err(e) = write_private(&dump_path(), &text, 0o600) {
        eprintln!("warden: {e}");
        return 1;
    }
    for s in &saved {
        let app = ctx.apps.iter().find(|a| a.name == s.name).cloned();
        let unit = app.as_ref().and_then(systemd_unit_for);
        match unit {
            Some(u) => match run_systemctl(&["enable", &u]) {
                Ok(()) => println!("{}: saved ({} workers{}); {u} enabled at boot", s.name, s.workers, stopped_note(s)),
                Err(e) => eprintln!("warden: {}: saved, but {e}", s.name),
            },
            None => println!("{}: saved ({} workers{})", s.name, s.workers, stopped_note(s)),
        }
    }
    println!("saved {} app(s) to {}", saved.len(), dump_path().display());
    if systemctl().is_none() && !saved.is_empty() {
        println!("after a reboot, run `warden resurrect` (or `warden startup` to have systemd do it)");
    }
    0
}

fn stopped_note(s: &Saved) -> &'static str {
    if s.stopped { ", stopped" } else { "" }
}

pub async fn resurrect(args: &Args) -> i32 {
    let dump = match read_dump() {
        Ok(Some(d)) => d,
        Ok(None) => {
            eprintln!("warden: nothing saved yet ({} does not exist); run `warden save` first", dump_path().display());
            return 1;
        }
        Err(e) => {
            eprintln!("warden: {e}");
            return 1;
        }
    };
    let ctx = context(args);
    let mut worst = 0;
    for s in &dump.apps {
        if !s.config.is_file() {
            eprintln!("warden: {}: config {} no longer exists; skipped", s.name, s.config.display());
            worst = 1;
            continue;
        }
        let app = app_from_config(&s.config);
        if reachable(&app) {
            println!("{}: already running", s.name);
            continue;
        }
        worst = worst.max(start_app(&ctx, &app).await);
    }
    worst
}

// ---------------------------------------------------------- startup

const UNIT_TEMPLATE: &str = include_str!("../contrib/warden@.service");
const SYSCTL_CONF: &str = include_str!("../contrib/99-warden.conf");

fn sysctl_dir() -> PathBuf {
    std::env::var_os("WARDEN_SYSCTL_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/etc/sysctl.d"))
}

/// Write `text` to `path` unless it already has it; say what happened.
fn install_file(path: &Path, text: &str) -> Result<&'static str, String> {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        return Ok("unchanged");
    }
    write_private(path, text, 0o644)?;
    Ok("written")
}

pub async fn startup(args: &Args) -> i32 {
    if systemctl().is_none() {
        eprintln!(
            "warden: startup needs systemd and root (sudo warden startup). Without systemd, run \
             `warden resurrect` from your init system or container entrypoint at boot"
        );
        return 2;
    }
    let exe =
        std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "/usr/local/bin/warden".into());
    let unit = UNIT_TEMPLATE.replace("/usr/local/bin/warden", &exe);
    let unit_path = unit_dir().join("warden@.service");
    let sysctl_path = sysctl_dir().join("99-warden.conf");
    for (path, text) in [(&unit_path, unit.as_str()), (&sysctl_path, SYSCTL_CONF)] {
        match install_file(path, text) {
            Ok(what) => println!("{}: {what}", path.display()),
            Err(e) => {
                eprintln!("warden: {e}");
                return 1;
            }
        }
    }
    if std::env::var_os("WARDEN_SYSCTL_DIR").is_none() {
        match std::process::Command::new("sysctl").args(["-q", "-p"]).arg(&sysctl_path).status() {
            Ok(s) if s.success() => println!("applied {}", sysctl_path.display()),
            _ => eprintln!("warden: could not apply {} now; it applies at the next boot", sysctl_path.display()),
        }
    }
    if let Err(e) = run_systemctl(&["daemon-reload"]) {
        eprintln!("warden: {e}");
        return 1;
    }
    let ctx = context(args);
    let saved: Vec<String> = match read_dump() {
        Ok(Some(d)) => d.apps.into_iter().map(|s| s.name).collect(),
        _ => ctx.apps.iter().map(|a| a.name.clone()).collect(),
    };
    let mut worst = 0;
    for name in saved {
        let Some(app) = ctx.apps.iter().find(|a| a.name == name) else { continue };
        let expected = unit_config_path(&name);
        if !app.config.as_ref().is_some_and(|c| same_file(c, &expected)) {
            eprintln!(
                "warden: {name}: the unit reads {}; move or link the config there to run it under systemd",
                expected.display()
            );
            worst = 1;
            continue;
        }
        let unit = format!("warden@{name}.service");
        match run_systemctl(&["enable", &unit]) {
            Ok(()) => println!("{name}: {unit} enabled at boot"),
            Err(e) => {
                eprintln!("warden: {name}: {e}");
                worst = 1;
            }
        }
        if let Ok(st) = status_of(app).await {
            if st.unit.is_none() {
                println!(
                    "{name}: running outside systemd now; it moves under systemd at the next boot, or now with \
                     `warden kill {name} --yes && systemctl start {unit}`"
                );
            }
        }
    }
    worst
}

pub async fn unstartup(args: &Args) -> i32 {
    if systemctl().is_none() {
        eprintln!("warden: unstartup needs systemd and root");
        return 2;
    }
    let ctx = context(args);
    let mut worst = 0;
    for app in &ctx.apps {
        let unit = format!("warden@{}.service", app.name);
        match run_systemctl(&["disable", &unit]) {
            Ok(()) => println!("{}: {unit} disabled (still running until stopped)", app.name),
            Err(e) => {
                eprintln!("warden: {}: {e}", app.name);
                worst = 1;
            }
        }
    }
    let unit_path = unit_dir().join("warden@.service");
    match std::fs::remove_file(&unit_path) {
        Ok(()) => println!("{}: removed", unit_path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("warden: {}: {e}", unit_path.display());
            worst = 1;
        }
    }
    let _ = run_systemctl(&["daemon-reload"]);
    worst
}

// -------------------------------------------------------------------- top

pub async fn top(args: &Args) -> i32 {
    // SAFETY: isatty has no preconditions.
    let tty = unsafe { libc::isatty(1) == 1 };
    loop {
        let ctx = context(args);
        let apps: Vec<App> = match resolve(&ctx, args.target.as_deref(), false) {
            Ok(s) => s.into_iter().map(|s| s.app).collect(),
            Err(e) => {
                eprintln!("warden: {e}");
                return 2;
            }
        };
        let all = statuses(&apps).await;
        let mut out = String::new();
        if tty {
            out += "\x1b[2J\x1b[H";
        }
        out += &format!("warden top - {} (Ctrl-C to quit)\n\n", crate::logging::timestamp_now());
        out += &cli::render_list(&all);
        print!("{out}");
        if !tty {
            return 0;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, ns: &str) -> App {
        App {
            name: name.into(),
            namespace: ns.into(),
            config: Some(PathBuf::from(format!("/etc/warden/{name}.toml"))),
            socket: PathBuf::from(format!("/run/warden/{name}/control.sock")),
            problem: None,
        }
    }

    fn ctx() -> Ctx {
        Ctx { apps: vec![app("api", "backend"), app("web", "backend"), app("queue", "default")], single: false }
    }

    #[test]
    fn targets() {
        let c = ctx();
        let names = |v: Vec<Sel>| v.into_iter().map(|s| (s.app.name, s.worker)).collect::<Vec<_>>();
        assert_eq!(names(resolve(&c, Some("api"), true).unwrap()), vec![("api".into(), None)]);
        assert_eq!(names(resolve(&c, Some("api:2"), true).unwrap()), vec![("api".into(), Some(2))]);
        assert_eq!(resolve(&c, Some("backend"), true).unwrap().len(), 2);
        assert_eq!(resolve(&c, Some("all"), true).unwrap().len(), 3);
        assert_eq!(resolve(&c, None, false).unwrap().len(), 3);
        assert!(resolve(&c, None, true).unwrap_err().contains("which app?"));
        assert!(resolve(&c, Some("3"), true).unwrap_err().contains("names apps, not numeric ids"));
        assert!(resolve(&c, Some("nope"), true).unwrap_err().contains("no app or namespace"));
        assert!(resolve(&c, Some("backend:1"), true).unwrap_err().contains("needs an app"));
        assert!(resolve(&c, Some("api:x"), true).is_err());
    }

    #[test]
    fn single_app_targets() {
        let c = Ctx { apps: vec![app("api", "default")], single: true };
        let one = |t: Option<&str>| resolve(&c, t, true).map(|v| v[0].worker);
        assert_eq!(one(None), Ok(None));
        assert_eq!(one(Some("2")), Ok(Some(2)));
        assert_eq!(one(Some(":2")), Ok(Some(2)));
        assert_eq!(one(Some("api:3")), Ok(Some(3)));
        assert!(one(Some("web")).is_err());
    }

    #[test]
    fn quick_config_from_a_script() {
        let dir = std::env::temp_dir().join(format!("warden-quick-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.ts");
        std::fs::write(&script, "").unwrap();
        let o = StartOpts {
            name: Some("api".into()),
            instances: Some("4".into()),
            port: Some(3000),
            env: vec![("NODE_ENV".into(), "production".into())],
            max_memory_mb: Some(300),
            script_args: vec!["--verbose".into()],
            ..Default::default()
        };
        let (name, text) = quick_config(&Launch::Script(script.clone()), &o).unwrap();
        assert_eq!(name, "api");
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.app.command, "bun");
        assert_eq!(c.app.args.last().map(String::as_str), Some("--verbose"));
        assert_eq!((c.workers.count, c.app.port, c.limits.max_memory), (4, Some(3000), 300));
        assert_eq!(c.app.env.get("NODE_ENV").map(String::as_str), Some("production"));
        let bad = StartOpts { instances: Some("lots".into()), ..Default::default() };
        assert!(quick_config(&Launch::Script(script.clone()), &bad).is_err());
        // A command line and a program.
        let (name, text) =
            quick_config(&Launch::Shell("python3 -m http.server 8000".into()), &StartOpts::default()).unwrap();
        assert_eq!(name, "python3");
        let c = Config::parse(&text).unwrap();
        assert_eq!(
            (c.app.command.as_str(), c.app.args.clone()),
            ("sh", vec!["-c".to_string(), "exec python3 -m http.server 8000".to_string()])
        );
        assert!(!c.shim_enabled());
        let (_, text) = quick_config(&Launch::Shell("cd /tmp && ./run".into()), &StartOpts::default()).unwrap();
        assert!(text.contains("\"cd /tmp && ./run\""), "no exec for shell syntax: {text}");
        let o = StartOpts {
            script_args: vec!["start".into()],
            kill_signal: Some("SIGINT".into()),
            kill_timeout_ms: Some(1600),
            cron: Some("0 3 * * *".into()),
            stop_exit_codes: vec![0],
            autorestart: Some(false),
            out_file: Some("/var/log/web-out.log".into()),
            instances: Some("2".into()),
            ..Default::default()
        };
        let (name, text) = quick_config(&Launch::Program("npm".into()), &o).unwrap();
        assert_eq!(name, "npm");
        let c = Config::parse(&text).unwrap();
        assert_eq!(c.app.args, vec!["start".to_string()]);
        assert_eq!((c.shutdown.signal.as_str(), c.shutdown.grace_period), ("SIGINT", 2));
        assert_eq!(c.restart.schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(c.restart.stop_exit_codes, vec![0]);
        assert!(!c.restart.enabled);
        assert!(c.logging.per_worker_files);
        // A file with an unknown extension that isn't executable.
        let data = dir.join("notes.txt");
        std::fs::write(&data, "").unwrap();
        assert!(quick_config(&Launch::Script(data), &StartOpts::default()).unwrap_err().contains("--interpreter"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Many apps on one host, PM2-style (`warden list`, `warden restart api`).
//!
//! Each app has its own supervisor (run by systemd as `warden@<app>`, or
//! started in the background by `warden start`). The CLI finds apps through
//! their configs in the config directory and their control sockets in the
//! runtime directory, and talks to each one directly. The optional wardend
//! (`src/daemon`) only restarts background supervisors that die and serves
//! live events; nothing here depends on it, runs on the request path, or is
//! needed by a running app.

use crate::cli::{self, Action, Args, ScaleArg, StartOpts};
use crate::config::{self, Config};
use crate::control::{self, Request, Response, Status};
use crate::table::Fmt;
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
    /// The number `warden list` shows and `warden start 3` takes (`ids.rs`).
    /// `None`: not one of this host's configured apps (a supervisor found
    /// running from a config elsewhere, or an app given with `-c`).
    pub id: Option<u32>,
}

/// The apps a command can act on.
pub struct Ctx {
    pub apps: Vec<App>,
    /// `-c` / `--socket` / `$WARDEN_CONFIG`: one app, as in single-app use.
    pub single: bool,
    /// `--no-wait`: `start` returns once the supervisor answers, without
    /// waiting for the workers to be ready.
    pub no_wait: bool,
    /// The app ids could not be kept (`ids::Ids::warning`); `list` says so.
    pub ids_warning: Option<String>,
}

// ------------------------------------------------------------------ places

fn home() -> Option<PathBuf> {
    std::env::var_os("WARDEN_HOME").filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn is_root() -> bool {
    crate::sys::is_root()
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

/// A background supervisor's log file (`warden start` and wardend).
pub(crate) fn log_path(name: &str) -> PathBuf {
    state_dir().join("logs").join(format!("{name}.log"))
}

// --------------------------------------------------------------- discovery

pub(crate) fn app_from_config(path: &Path) -> App {
    match Config::load(path) {
        Ok(c) => App {
            name: c.app.name.clone(),
            namespace: c.app.namespace.clone().unwrap_or_else(|| "default".into()),
            config: Some(path.to_path_buf()),
            socket: c.socket_path(),
            problem: None,
            id: None,
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
                id: None,
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
            // wardend's own file (alert rules), not an app.
            .filter(|p| p.file_name().is_none_or(|n| n != crate::daemon::alerts::FILE_NAME))
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
                    id: None,
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
            id: None,
        });
        app.socket = sock.clone();
        return single_ctx(app, args);
    }
    if let Some(c) = &args.config {
        return single_ctx(app_from_config(c), args);
    }
    let mut apps = discover();
    let local = Path::new("warden.toml");
    if apps.is_empty() && local.is_file() {
        return single_ctx(app_from_config(local), args);
    }
    let ids_warning = number_apps(&mut apps);
    Ctx { apps, single: false, no_wait: args.no_wait, ids_warning }
}

/// Number what is new, in the order the configs sort, and set every app's id.
/// Only the config directory's apps get one: a supervisor found running from a
/// config somewhere else is named, not numbered. Returns what is wrong with
/// the saved ids, if anything (`list` says so).
fn number_apps(apps: &mut [App]) -> Option<String> {
    let dir = config_dir();
    let names: Vec<&str> = apps
        .iter()
        .filter(|a| a.config.as_ref().is_some_and(|c| c.parent() == Some(dir.as_path())))
        .map(|a| a.name.as_str())
        .collect();
    let ids = crate::ids::assign(&state_dir(), &names);
    for a in apps.iter_mut() {
        a.id = ids.get(&a.name);
    }
    ids.warning
}

/// The apps a TARGET names on this host (ids and lists as everywhere), as
/// names: `warden events 0,api`.
pub fn resolve_names(target: &str) -> Result<Vec<String>, String> {
    let mut apps = discover();
    number_apps(&mut apps);
    let ctx = Ctx { apps, single: false, no_wait: false, ids_warning: None };
    let mut names: Vec<String> = Vec::new();
    for s in resolve(&ctx, Some(target), false)? {
        if !names.contains(&s.app.name) {
            names.push(s.app.name);
        }
    }
    Ok(names)
}

/// The apps in a selection, once each (`0,0:1` or `-c x.toml 0,1` select one
/// app several times).
fn unique_apps(sels: &[Sel]) -> Vec<App> {
    let mut apps: Vec<App> = Vec::new();
    for s in sels {
        if !apps.iter().any(|a| a.name == s.app.name) {
            apps.push(s.app.clone());
        }
    }
    apps
}

/// One app named by `-c`, a socket or `./warden.toml`: its number is shown
/// when it has one, but nothing is numbered, and a number in a target means a
/// worker.
fn single_ctx(mut app: App, args: &Args) -> Ctx {
    app.id = crate::ids::lookup(&state_dir(), &[app.name.as_str()]).get(&app.name).copied();
    Ctx { apps: vec![app], single: true, no_wait: args.no_wait, ids_warning: None }
}

// ------------------------------------------------------------------ targets

#[derive(Debug, Clone, PartialEq)]
pub struct Sel {
    pub app: App,
    pub worker: Option<usize>,
    /// `app:standby` (every hot standby) or `app:s2` (one, as `warden
    /// status` lists them): only `warden logs` takes these.
    pub standby: Option<String>,
}

/// What follows `app:` in a target: a worker number, or a hot standby.
fn parse_worker(t: &str, w: &str) -> Result<(Option<usize>, Option<String>), String> {
    if let Ok(n) = w.parse::<usize>() {
        return Ok((Some(n), None));
    }
    if control::is_standby(w) {
        return Ok((None, Some(w.to_string())));
    }
    Err(format!(
        "{t:?}: the part after ':' must be a worker number, `standby` (the hot standbys) or one standby's `s1`, `s2`… \
         (as `warden status` lists them)"
    ))
}

/// What a command's TARGET can be, comma-separated (`warden start 0,1,2`,
/// `warden restart api,web:1`); each item:
/// - `all`;
/// - an app name, a namespace, or an app's number (`warden list` shows them;
///   a name wins over a number when an app is called `3`);
/// - `app:N` (or `3:N`) for one worker of it;
/// - a range of numbers, `0-2` (the apps that have one: a gap is skipped).
///
/// In a single-app context (`-c`) a number is a worker: `2`, `:2`, `0,1`, `0-2`.
/// Hot standbys: `app:standby`, `app:s1` (`warden logs` only). `need`: a
/// mutating command must name its target when there are several apps.
/// Nothing is selected when any item is wrong: a typo never acts on half.
pub fn resolve(ctx: &Ctx, target: Option<&str>, need: bool) -> Result<Vec<Sel>, String> {
    let Some(t) = target else {
        if ctx.single || !need || ctx.apps.len() == 1 && ctx.apps[0].config.is_none() {
            return Ok(ctx.apps.iter().map(|a| Sel { app: a.clone(), worker: None, standby: None }).collect());
        }
        return Err(no_target_error(ctx));
    };
    if ctx.apps.is_empty() {
        return Err(no_apps_error());
    }
    let items: Vec<&str> = t.split(',').map(str::trim).filter(|i| !i.is_empty()).collect();
    if items.is_empty() {
        return Err(format!("{t:?}: no app named; give a name, an id, `all`, or a list like 0,1,2"));
    }
    let mut out: Vec<Sel> = Vec::new();
    for item in items {
        for sel in resolve_item(ctx, item)? {
            if !out.contains(&sel) {
                out.push(sel);
            }
        }
    }
    Ok(out)
}

/// `3-7` as (3, 7); `None` for anything else (a name like `v1-2` is not one).
fn id_range(t: &str) -> Option<(u32, u32)> {
    let (a, b) = t.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    (digits(a) && digits(b)).then_some(())?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// Largest range `0-N` accepted: a typo like `0-99999999` is an error, not a
/// long loop.
const MAX_RANGE: u32 = 4096;

fn resolve_item(ctx: &Ctx, t: &str) -> Result<Vec<Sel>, String> {
    let all = |w: Option<usize>| {
        ctx.apps.iter().map(|a| Sel { app: a.clone(), worker: w, standby: None }).collect::<Vec<_>>()
    };
    if t == "all" {
        return Ok(all(None));
    }
    // A range, unless it is an app's name (`1-2` is a legal name).
    if let Some((lo, hi)) = id_range(t).filter(|_| !ctx.apps.iter().any(|a| a.name == t)) {
        if lo > hi {
            return Err(format!("{t:?}: a range goes up, like {hi}-{lo}"));
        }
        if hi - lo >= MAX_RANGE {
            return Err(format!("{t:?}: that range is too long (at most {MAX_RANGE} ids)"));
        }
        if ctx.single {
            let app = &ctx.apps[0];
            return Ok((lo..=hi).map(|w| Sel { app: app.clone(), worker: Some(w as usize), standby: None }).collect());
        }
        let mut in_range: Vec<&App> =
            ctx.apps.iter().filter(|a| a.id.is_some_and(|i| (lo..=hi).contains(&i))).collect();
        in_range.sort_by_key(|a| a.id);
        if in_range.is_empty() {
            return Err(no_such_id(ctx, t));
        }
        return Ok(in_range.into_iter().map(|a| Sel { app: a.clone(), worker: None, standby: None }).collect());
    }
    let (name, worker, standby) = match t.rsplit_once(':') {
        Some((n, w)) => {
            let (w, sb) = parse_worker(t, w)?;
            (n, w, sb)
        }
        None => (t, None, None),
    };
    if ctx.single {
        let app = &ctx.apps[0];
        if name.is_empty() || name == app.name {
            return Ok(vec![Sel { app: app.clone(), worker, standby }]);
        }
        if let Ok(n) = name.parse::<usize>() {
            return Ok(vec![Sel { app: app.clone(), worker: Some(n), standby: None }]);
        }
        if control::is_standby(name) {
            return Ok(vec![Sel { app: app.clone(), worker: None, standby: Some(name.to_string()) }]);
        }
        return Err(format!("{name:?} is not this app ({}); the config names one app", app.name));
    }
    if let Some(app) = ctx.apps.iter().find(|a| a.name == name) {
        return Ok(vec![Sel { app: app.clone(), worker, standby }]);
    }
    let in_ns: Vec<Sel> = ctx
        .apps
        .iter()
        .filter(|a| a.namespace == name)
        .map(|a| Sel { app: a.clone(), worker: None, standby: None })
        .collect();
    if !in_ns.is_empty() {
        if worker.is_some() || standby.is_some() {
            return Err(format!("{t:?}: a worker number needs an app, not a namespace"));
        }
        return Ok(in_ns);
    }
    if let Ok(n) = name.parse::<u32>() {
        return match ctx.apps.iter().find(|a| a.id == Some(n)) {
            Some(app) => Ok(vec![Sel { app: app.clone(), worker, standby }]),
            None => Err(no_such_id(ctx, name)),
        };
    }
    Err(format!("no app or namespace named {name:?}; `warden list` shows what is on this host"))
}

/// No app has this id (or none in this range): say which ids exist.
fn no_such_id(ctx: &Ctx, what: &str) -> String {
    let ids: Vec<u32> = ctx.apps.iter().filter_map(|a| a.id).collect();
    if ids.is_empty() {
        return format!("no app has the id {what}; `warden list` shows the apps, which you can name");
    }
    format!("no app has the id {what} (the ids on this host: {}); `warden list` shows them", crate::ids::compact(&ids))
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

pub(crate) async fn status_of(app: &App) -> Result<Status, String> {
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

/// PM2 prints the app table after start, stop, restart and the like: what
/// the command left things as. So do we on a terminal, or anywhere with
/// `WARDEN_TABLE=1` (into a log, say); a script's output stays as it was.
fn table_after() -> bool {
    crate::sys::isatty(1) || std::env::var_os("WARDEN_TABLE").is_some_and(|v| !v.is_empty() && v != "0")
}

/// Is any worker between states (stopping, starting, draining an old process)?
fn in_transition(all: &[(App, Result<Status, String>)]) -> bool {
    all.iter().filter_map(|(_, s)| s.as_ref().ok()).any(|s| {
        s.workers
            .iter()
            .chain(&s.draining)
            .any(|w| matches!(w.state.as_str(), "STOPPING" | "STARTING" | crate::control::DRAINING))
    })
}

/// Print the table of every app now (see `table_after`). Not for `--json` or
/// a single app named with `-c`. `settle`: for commands that return before
/// the workers have finished (`stop` only asks them to), wait a few seconds
/// for them to, so the table shows `stopped` and not `STOPPING`, as PM2's does.
async fn show_apps(args: &Args, settle: bool) {
    if args.json || !table_after() {
        return;
    }
    let ctx = context(args);
    if ctx.single {
        return;
    }
    let deadline = Instant::now() + SETTLE_MAX;
    let mut all = statuses(&ctx.apps).await;
    while settle && in_transition(&all) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        all = statuses(&ctx.apps).await;
    }
    print!("{}", cli::render_list_with(&all, &Fmt::stdout()));
}

/// The longest `show_apps` waits for workers to finish stopping or starting.
const SETTLE_MAX: Duration = Duration::from_secs(6);

/// Commands that act on running apps.
pub async fn act(args: &Args, action: &Action) -> i32 {
    let code = act_inner(args, action).await;
    match action {
        Action::Stop | Action::Reset | Action::Scale(_) => show_apps(args, true).await,
        Action::Restart { .. } | Action::Reload { .. } => show_apps(args, false).await,
        _ => {}
    }
    code
}

async fn act_inner(args: &Args, action: &Action) -> i32 {
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
        Action::List => list(&sels, args, &ctx).await,
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

async fn list(sels: &[Sel], args: &Args, ctx: &Ctx) -> i32 {
    let apps = unique_apps(sels);
    let all = statuses(&apps).await;
    if ctx.single {
        // One app (-c): its status object, as `warden status --json` always printed.
        return match &all[..] {
            [(_, Ok(st))] => {
                if args.json {
                    println!("{}", serde_json::to_string_pretty(st).unwrap_or_default());
                } else {
                    print!("{}", cli::render_status_with(st, args.table_only, ctx.apps[0].id, &Fmt::stdout()));
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
                    "id": a.id,
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
            print!("{}", cli::render_status_with(st, args.table_only, all[0].0.id, &Fmt::stdout()));
            return 0;
        }
    }
    // Offline apps are a state to show, not an error (as with `pm2 list`).
    print!("{}", cli::render_list_with(&all, &Fmt::stdout()));
    if let Some(w) = &ctx.ids_warning {
        eprintln!("warden: {w}");
    }
    0
}

async fn describe(sels: &[Sel], args: &Args) -> i32 {
    let mut code = 0;
    // Once per app, whichever workers the target named.
    let once: Vec<Sel> = unique_apps(sels).into_iter().map(|app| Sel { app, worker: None, standby: None }).collect();
    for (i, s) in once.iter().enumerate() {
        if i > 0 {
            println!();
        }
        let st = status_of(&s.app).await;
        let info = call_with(&s.app, &Request::Config { show_secrets: false, worker: None }, REQUEST_TIMEOUT).await;
        match (st, info) {
            (Ok(st), Ok(info)) => {
                if args.json {
                    let v = serde_json::json!({"status": st, "info": info.info});
                    println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                } else {
                    let info = info.info.as_ref().unwrap_or(&serde_json::Value::Null);
                    print!("{}", cli::render_describe(&st, info, s.app.id, &Fmt::stdout()));
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

/// `warden env <app>[:N]`: the environment worker N (default 1) starts
/// with, on top of the supervisor's own: `env_file`, `[app] env`, then
/// Warden's variables, as `KEY=value` lines (later ones win) with `#`
/// comments saying where each group comes from.
async fn env(sels: &[Sel], show_secrets: bool) -> i32 {
    let mut code = 0;
    for s in sels {
        let req = Request::Config { show_secrets, worker: s.worker };
        match call_with(&s.app, &req, REQUEST_TIMEOUT).await {
            Ok(r) => {
                if sels.len() > 1 {
                    println!("# {}", s.app.name);
                }
                print!("{}", render_env(&r.info.unwrap_or_default(), show_secrets));
            }
            Err(e) => {
                code = 2;
                eprintln!("warden: {}: {e}", s.app.name);
            }
        }
    }
    code
}

/// The text of `warden env` from a `config` answer.
fn render_env(info: &serde_json::Value, show_secrets: bool) -> String {
    let mut o = String::new();
    let hidden = if show_secrets { "" } else { "; the app's values are hidden unless --show-secrets" };
    let Some(vars) = info["worker_env"].as_array() else {
        // An older supervisor: only the app's own variables.
        if let Some(env) = info.pointer("/config/app/env").and_then(|e| e.as_object()) {
            for (k, v) in env {
                o += &format!("{k}={}\n", v.as_str().unwrap_or_default());
            }
        }
        o += &format!("# also set by Warden for each worker: PORT, WARDEN_APP, WARDEN_WORKER_ID, …{hidden}\n");
        return o;
    };
    let of = info["worker_env_of"].as_str().unwrap_or("1");
    let who = if of == "host" { "the worker-mode host process".to_string() } else { format!("worker {of}") };
    o += &format!(
        "# The environment {who} starts with: the supervisor's own (PATH, HOME, …), then these, a later one \
         winning{hidden}.\n"
    );
    let env_file = info.pointer("/config/app/env_file").and_then(|f| f.as_str()).unwrap_or("env_file");
    let mut group = "";
    let mut seen: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for v in vars {
        let (Some(name), Some(value), Some(from)) = (v["name"].as_str(), v["value"].as_str(), v["from"].as_str())
        else {
            continue;
        };
        if from != group {
            group = from;
            o += &match from {
                "env_file" => format!("# from {env_file}\n"),
                "env" => "# from [app] env\n".to_string(),
                _ => "# set by Warden (README: Environment variables)\n".to_string(),
            };
        }
        let note = match seen.insert(name, from) {
            Some(earlier) if earlier != from => format!("   # overrides the value from {earlier}"),
            _ => String::new(),
        };
        o += &format!("{name}={value}{note}\n");
    }
    if of != "host" {
        o += "# per worker: WARDEN_WORKER_ID, the instance variable (NODE_APP_INSTANCE) and, with port_strategy = \
              \"offset\", PORT; `warden env <app>:N` shows worker N\n";
    }
    o
}

async fn show_config(sels: &[Sel], show_secrets: bool) -> i32 {
    let mut code = 0;
    for s in sels {
        match call_with(&s.app, &Request::Config { show_secrets, worker: None }, REQUEST_TIMEOUT).await {
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
    // Every selection is checked before the first request goes out: a wrong
    // item (`warden stop 0 1:2`) acts on nothing, not on the items before it.
    let mut jobs: Vec<(&Sel, Request)> = Vec::new();
    for s in sels {
        if let Some(sb) = &s.standby {
            let app = &s.app.name;
            eprintln!(
                "warden: {app}:{sb} - a hot standby takes no commands of its own: Warden replaces a failing one, a \
                 deploy replaces them all, `warden reset {app}` retries FAILED ones. Its log: `warden logs {app} \
                 --worker {sb}`"
            );
            return 2;
        }
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
        jobs.push((s, req));
    }
    let mut worst = 0;
    for (n, (s, req)) in jobs.iter().enumerate() {
        if !ctx.single && !s.app.socket.exists() {
            eprintln!("warden: {}: not running (start it with `warden start {}`)", s.app.name, s.app.name);
            worst = worst.max(2);
            continue;
        }
        let code = one_op(ctx, &s.app, req.clone(), args).await;
        worst = worst.max(code);
        // Deploys across several apps stop at the first failure.
        if code == 1 && matches!(action, Action::Reload { .. } | Action::Restart { hard: false }) && jobs.len() > 1 {
            let mut rest: Vec<&str> = Vec::new();
            for (later, _) in &jobs[n + 1..] {
                if later.app.name != s.app.name && !rest.contains(&later.app.name.as_str()) {
                    rest.push(&later.app.name);
                }
            }
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
    let follow = follow.unwrap_or_else(|| crate::sys::isatty(1));
    let lines = lines.unwrap_or(15);
    // Text / time / level filters run here; ask for more so N survive them.
    let fetch = if query.is_filtering() { 4000 } else { lines };
    let req = |s: &Sel, n: usize, follow: bool| Request::Logs {
        lines: n,
        follow,
        worker: s.standby.clone().or_else(|| s.worker.map(|n| n.to_string())).or_else(|| query.worker.clone()),
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
        match control::call(&s.app.socket, &req(s, fetch, false), &mut buf).await {
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
        let r = req(&s, 0, true);
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
    use crate::logview::{Source, file_chain, for_each_line, journal_lines};
    let multi = sels.len() > 1;
    let width = sels.iter().map(|s| s.app.name.len()).max().unwrap_or(0);
    let mut out = crate::logview::PipeOut::new();
    let mut worst = 0;
    for s in sels {
        let app = &s.app;
        let mut q = query.clone();
        if let Some(w) = s.standby.clone().or_else(|| s.worker.map(|n| n.to_string())) {
            q.worker = Some(w);
        }
        // Where this app logs: from the running supervisor, else its config.
        let (cfg, unit) =
            match call_with(app, &Request::Config { show_secrets: false, worker: None }, REQUEST_TIMEOUT).await {
                Ok(r) => {
                    let info = r.info.unwrap_or_default();
                    let cfg: Option<Config> = serde_json::from_value(info["config"].clone()).ok();
                    let pid = info["pid"].as_u64().map(|p| p as u32);
                    let unit = info["unit"].as_str().map(|u| (scope_of_running_unit(pid), u.to_string()));
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
        let setup = crate::logview::Setup {
            logging: l,
            framed: l.file.clone().or(running_log).or(background),
            journal: unit.is_some(),
            processes: if cfg.workers.mode == config::Mode::Process { cfg.workers.count } else { 0 },
        };
        let sources = match crate::logview::sources(&setup, &q) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("warden: {}: {e}", app.name);
                worst = worst.max(1);
                continue;
            }
        };
        // Lines from several files (per-worker files, Warden's log next to
        // them) say whose they are; one file's are printed as written.
        let label = sources.len() > 1;
        for src in &sources {
            let emit = |line: &str| -> String {
                let line = match src {
                    Source::Raw { worker, stream, .. } if json => {
                        return crate::logview::raw_to_json(&app.name, worker.as_deref(), stream, line);
                    }
                    Source::Raw { worker, stream, .. } if label => {
                        std::borrow::Cow::Owned(crate::logview::frame_raw(worker.as_deref(), stream, line))
                    }
                    _ if json => return crate::logview::to_json(&app.name, line),
                    _ => std::borrow::Cow::Borrowed(line),
                };
                if multi { format!("{:<width$} | {line}", app.name) } else { line.into_owned() }
            };
            // `--lines N`: the last N of each source (as `pm2 logs --lines`
            // shows each file's); otherwise stream everything.
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
            let (path, raw) = match src {
                Source::Journal => {
                    let Some((scope, u)) = &unit else { continue };
                    let res = journal_lines(u, *scope == Scope::User, &q, &mut |line| {
                        if q.matches(line) { push(emit(line), &mut out) } else { true }
                    });
                    if let Err(e) = res {
                        eprintln!("warden: {}: {e}", app.name);
                        worst = 2;
                    }
                    (None, false)
                }
                Source::Framed(p) => (Some(p), false),
                Source::Raw { path, .. } => (Some(path), true),
            };
            if let Some(path) = path {
                let chain = file_chain(path);
                if chain.is_empty() {
                    eprintln!("warden: {}: {} does not exist yet", app.name, path.display());
                    worst = worst.max(1);
                }
                for f in chain {
                    let res = for_each_line(&f, &mut |line| {
                        let keep = if raw { q.matches_raw(line) } else { q.matches(line) };
                        if keep { push(emit(line), &mut out) } else { true }
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

/// Which systemd manager: the system's (root, /etc/systemd/system) or the
/// user's own (`systemctl --user`, ~/.config/systemd/user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Scope {
    System,
    User,
}

impl Scope {
    /// What this user manages: root the system's units, anyone else their own.
    pub(crate) fn mine() -> Scope {
        if is_root() { Scope::System } else { Scope::User }
    }

    /// Where `warden startup` writes the units (`$WARDEN_UNIT_DIR` /
    /// `$WARDEN_USER_UNIT_DIR` for tests).
    pub(crate) fn unit_dir(self) -> PathBuf {
        match self {
            Scope::System => std::env::var_os("WARDEN_UNIT_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/etc/systemd/system")),
            Scope::User => match std::env::var_os("WARDEN_USER_UNIT_DIR").filter(|v| !v.is_empty()) {
                Some(d) => PathBuf::from(d),
                None => user_dir("XDG_CONFIG_HOME", ".config").with_file_name("systemd").join("user"),
            },
        }
    }

    /// `systemctl <this> …`: `--user` for the user's manager.
    pub(crate) fn flag(self) -> Option<&'static str> {
        match self {
            Scope::System => None,
            Scope::User => Some("--user"),
        }
    }

    /// `systemctl --user ` or nothing, for messages.
    pub(crate) fn shown(self) -> &'static str {
        match self {
            Scope::System => "",
            Scope::User => "--user ",
        }
    }

    /// A Warden unit file installed for this manager.
    pub(crate) fn has_unit(self, file: &str) -> bool {
        self.unit_dir().join(file).exists()
            || self == Scope::System
                && ["/lib/systemd/system", "/usr/lib/systemd/system"].iter().any(|d| Path::new(d).join(file).exists())
    }
}

/// systemctl, when systemd runs this host (`$WARDEN_SYSTEMCTL` for tests).
pub(crate) fn systemctl_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("WARDEN_SYSTEMCTL").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(p));
    }
    if !Path::new("/run/systemd/system").is_dir() {
        return None;
    }
    ["/usr/bin/systemctl", "/bin/systemctl"].iter().map(PathBuf::from).find(|p| p.exists())
}

/// Does the system manager (root's units) apply to us?
fn systemctl() -> Option<PathBuf> {
    systemctl_bin().filter(|_| is_root() || std::env::var_os("WARDEN_SYSTEMCTL").is_some())
}

/// `systemctl [--user] <args>`; the last line of its error output on failure.
pub(crate) fn run_systemctl(scope: Scope, args: &[&str]) -> Result<(), String> {
    let Some(bin) = systemctl_bin() else { return Err("systemd is not running on this host".into()) };
    let out = std::process::Command::new(&bin)
        .args(scope.flag())
        .args(args)
        .output()
        .map_err(|e| format!("running {} {}{}: {e}", bin.display(), scope.shown(), args.join(" ")))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "`systemctl {}{}` failed: {}",
            scope.shown(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("no output")
        ))
    }
}

/// `systemctl [--user] <args>`: its standard output, or the last line of
/// its error output.
pub(crate) fn systemctl_output(scope: Scope, args: &[&str]) -> Result<String, String> {
    let Some(bin) = systemctl_bin() else { return Err("systemd is not running on this host".into()) };
    let out = std::process::Command::new(&bin)
        .args(scope.flag())
        .args(args)
        .output()
        .map_err(|e| format!("running {} {}{}: {e}", bin.display(), scope.shown(), args.join(" ")))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "`systemctl {}{}` failed: {}",
            scope.shown(),
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim().lines().last().unwrap_or("no output")
        ))
    }
}

/// `systemctl [--user] is-active --quiet <unit>`.
pub(crate) fn unit_active(scope: Scope, unit: &str) -> bool {
    run_systemctl(scope, &["is-active", "--quiet", unit]).is_ok()
}

/// The unit that `warden@.service` runs for this app (system units for
/// root, the user's own units for anyone else), if systemd can.
pub(crate) fn systemd_unit_for(app: &App) -> Option<(Scope, String)> {
    let scope = Scope::mine();
    let usable = if scope == Scope::System { systemctl() } else { systemctl_bin() };
    usable?;
    let cfg = app.config.as_ref()?;
    let expected = unit_config_path(&app.name);
    (scope.has_unit("warden@.service") && same_file(cfg, &expected))
        .then(|| (scope, format!("warden@{}.service", app.name)))
}

/// `warden@.service` reads `<config dir>/<app>.toml` (`/etc/warden` for root).
pub(crate) fn unit_config_path(name: &str) -> PathBuf {
    config_dir().join(format!("{name}.toml"))
}

/// Which manager runs the supervisor `pid` that reports a unit: the user's
/// when its cgroup is inside a `user@<uid>.service`, else the system's.
/// Without its cgroup (no pid, not Linux): the user's when we are not root
/// and user units are installed. Not from the unit files alone: `warden
/// unstartup` removes them while the apps keep running, and a `systemctl
/// stop` sent to the wrong manager then fails.
fn scope_of_running_unit(pid: Option<u32>) -> Scope {
    if let Some(cg) = pid.and_then(|p| std::fs::read_to_string(format!("/proc/{p}/cgroup")).ok()) {
        return scope_from_cgroup(&cg);
    }
    if !is_root() && Scope::User.has_unit("warden@.service") { Scope::User } else { Scope::System }
}

/// `/proc/<pid>/cgroup` of a process in a unit: is it the user manager's?
fn scope_from_cgroup(cgroup: &str) -> Scope {
    let user = cgroup
        .lines()
        .filter_map(|l| l.splitn(3, ':').nth(2))
        .any(|path| path.split('/').any(|c| c.starts_with("user@") && c.ends_with(".service")));
    if user { Scope::User } else { Scope::System }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

/// Does `what` read as app ids or a list of apps (`3`, `0-2`, `0,1,api`) rather
/// than a script, a path or a command line?
fn is_selector_list(what: &str) -> bool {
    let ids = what.split(',').all(|i| {
        let i = i.trim();
        i.parse::<u32>().is_ok() || id_range(i).is_some()
    });
    let list = what.contains(',') && !what.contains(char::is_whitespace) && !Path::new(what).exists();
    ids || list
}

/// `warden start <app | id | config.toml | script>`.
pub async fn start(args: &Args, what: &str, opts: &StartOpts) -> i32 {
    let code = start_inner(args, what, opts).await;
    show_apps(args, false).await;
    code
}

async fn start_inner(args: &Args, what: &str, opts: &StartOpts) -> i32 {
    let ctx = context(args);
    // 1. Apps we know: a name, an id, a namespace, `all`, or a list of them.
    match resolve(&ctx, Some(what), true) {
        Ok(sels) => {
            let mut worst = 0;
            let mut started: Vec<&str> = Vec::new();
            for s in &sels {
                // `0,0:1` or a namespace plus a member: once is enough.
                if started.contains(&s.app.name.as_str()) {
                    continue;
                }
                started.push(&s.app.name);
                worst = worst.max(start_app(&ctx, &s.app, OnFailedStart::Stop).await);
            }
            return worst;
        }
        // Meant as app ids or a list of apps, not a script or a command line:
        // say what is wrong with it rather than trying it as a program.
        Err(e) if is_selector_list(what) => {
            eprintln!("warden: {e}");
            if what.split(',').any(|i| i.parse::<u32>().is_err() && id_range(i).is_none()) {
                eprintln!(
                    "  (to run a program with arguments, quote the command line: warden start \"node server.js\" --name api)"
                );
            }
            return 2;
        }
        Err(_) => {}
    }
    let path = Path::new(what);
    // 2. A config file.
    if path.extension().is_some_and(|x| x == "toml") && path.is_file() {
        let app = app_from_config(path);
        if let Some(p) = &app.problem {
            eprintln!("warden: {p}");
            return 1;
        }
        return start_app(&ctx, &app, OnFailedStart::Stop).await;
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
            // Numbered now, so ids follow the order apps were created.
            let mut app = app_from_config(&file);
            app.id = crate::ids::register(&state_dir(), &name);
            start_app(&ctx, &app, OnFailedStart::Stop).await
        }
        Err(e) => {
            eprintln!("warden: {e}");
            2
        }
    }
}

/// `warden serve <dir> [port]`: an app whose workers are Warden's static server.
pub async fn serve(args: &Args, dir: &Path, port: u16, o: &StartOpts) -> i32 {
    let code = serve_inner(args, dir, port, o).await;
    show_apps(args, false).await;
    code
}

async fn serve_inner(args: &Args, dir: &Path, port: u16, o: &StartOpts) -> i32 {
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
    let mut app = app_from_config(&file);
    app.id = crate::ids::register(&state_dir(), &name);
    start_app(&ctx, &app, OnFailedStart::Stop).await
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

/// What `warden start` does when every worker crashed before one was ready.
#[derive(Debug, Clone, Copy, PartialEq)]
enum OnFailedStart {
    /// Report it and stop the workers: an interactive `warden start`.
    Stop,
    /// Report it and leave the restart policy at work: `warden resurrect`
    /// at boot, where a dependency may still be coming up.
    Report,
}

async fn start_app(ctx: &Ctx, app: &App, on_fail: OnFailedStart) -> i32 {
    let prefix = format!("{}: ", app.name);
    let log = format!("see {}", log_path(&app.name).display());
    if reachable(app) {
        return match call_with(app, &Request::Start, REQUEST_TIMEOUT).await {
            Ok(r) if r.ok => {
                println!("{prefix}{}", r.message.unwrap_or_default());
                ready_or_not(ctx, app, on_fail, &log).await
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
    if let Some((scope, unit)) = systemd_unit_for(app) {
        let journal = format!("see `journalctl {}-u {unit} -n 50`", scope.shown());
        if ctx.no_wait {
            if let Err(e) = run_systemctl(scope, &["start", "--no-block", &unit]) {
                eprintln!("warden: {prefix}{e}\n  {journal}");
                return 1;
            }
            return ready_or_not(ctx, app, on_fail, &journal).await;
        }
        println!("{prefix}starting {unit}");
        // `systemctl start` returns once the unit is ready (Type=notify:
        // every worker ready, or the workers stopped). Watch the app
        // meanwhile, so a start that fails is reported (and stopped) at
        // once rather than after the unit's TimeoutStartSec.
        let job = {
            let unit = unit.clone();
            tokio::task::spawn_blocking(move || run_systemctl(scope, &["start", &unit]))
        };
        return tokio::select! {
            r = job => match r {
                Ok(Ok(())) => wait_ready(app, on_fail, &journal).await,
                Ok(Err(e)) => {
                    eprintln!("warden: {prefix}{e}\n  {journal}");
                    1
                }
                Err(e) => {
                    eprintln!("warden: {prefix}running systemctl failed ({e})\n  {journal}");
                    1
                }
            },
            code = wait_ready(app, on_fail, &journal) => code,
        };
    }
    match spawn_background(&app.name, &cfg) {
        Ok(mut child) => {
            let log = log_path(&app.name);
            println!("{prefix}supervisor started in the background (pid {}), log {}", child.id(), log.display());
            // wardend restarts it if it dies (not needed under systemd).
            crate::daemon::client::autostart().await;
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
                    return ready_or_not(ctx, app, on_fail, &format!("see {}", log.display())).await;
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

/// `wait_ready`, unless `--no-wait` asked to return as soon as the supervisor
/// answers (starting many apps at once; `warden list` shows how they came up).
/// `log`: where the supervisor's own log is, for when it does not answer.
async fn ready_or_not(ctx: &Ctx, app: &App, on_fail: OnFailedStart, log: &str) -> i32 {
    if ctx.no_wait {
        println!("{}: starting (not waiting for readiness: `warden list` shows it)", app.name);
        return 0;
    }
    wait_ready(app, on_fail, log).await
}

/// How long `warden start` waits for the first worker, and then for the
/// rest: the app's `workers.ready_timeout` (after which Warden kills a
/// worker that isn't listening, a crash) plus room for a restart.
fn ready_wait(app: &App) -> Duration {
    let ready = app.config.as_ref().and_then(|p| Config::load(p).ok()).map_or(30, |c| c.workers.ready_timeout);
    Duration::from_secs(ready.saturating_add(15))
}

/// Wait until every worker is ready (or the app is stopped), then print one
/// line. Fails fast when the supervisor says every worker crashed before
/// one was ready (`Status.start_failed`): reports why, with the app's last
/// error output, and with `OnFailedStart::Stop` stops the workers. The
/// wait is bounded: `ready_wait` for the first worker, as long again for
/// the rest once one is ready.
async fn wait_ready(app: &App, on_fail: OnFailedStart, log: &str) -> i32 {
    let bound = ready_wait(app);
    let mut deadline = Instant::now() + bound;
    let mut first_ready = false;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(st) = status_of(app).await {
            if let Some(reason) = st.start_failed.clone() {
                return failed_start(app, &st, &reason, on_fail).await;
            }
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
            if st.workers_ready > 0 && !first_ready {
                // The app can start: give the others as long again.
                first_ready = true;
                deadline = Instant::now() + bound;
            }
            last = Some(st);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    match last {
        Some(st) if st.workers_ready == 0 => {
            eprintln!(
                "warden: {}: no worker ready after {} s, and not every one has crashed yet",
                app.name,
                bound.as_secs()
            );
            print_last_output(app).await;
            eprintln!(
                "  hint: the app is still starting: `warden list` shows its progress, `warden logs {}` its output; \
                 a slow boot needs a higher [workers] ready_timeout",
                app.name
            );
        }
        Some(st) => eprintln!(
            "warden: {}: only {}/{} workers ready after {} s; `warden logs {}` shows why",
            app.name,
            st.workers_ready,
            st.workers_configured,
            bound.as_secs(),
            app.name
        ),
        None => eprintln!("warden: {}: not answering; {log}", app.name),
    }
    1
}

/// `warden start` of an app that can't start: every worker crashed before
/// one was ready. What happened, why (the last crash and the app's last
/// error output), what Warden did, and what to do; exit 1.
///
/// `OnFailedStart::Stop` stops the workers (the supervisor stays up): the
/// app stays listed, as `errored` with its last exit, nothing restarts in
/// the background, and `warden start` tries again, as `pm2 start` leaves
/// an app that keeps crashing `errored`. FAILED with its cooldown retry
/// would go on restarting every few minutes behind the operator's back.
async fn failed_start(app: &App, st: &Status, reason: &str, on_fail: OnFailedStart) -> i32 {
    let name = &app.name;
    let stopped = on_fail == OnFailedStart::Stop
        && match call_with(app, &Request::Stop, REQUEST_TIMEOUT).await {
            Ok(r) => r.ok || st.stopped,
            Err(_) => false,
        };
    eprintln!(
        "warden: {name}: failed to start: {} crashed before one was ready ({reason})",
        if st.workers_configured == 1 {
            "its worker".to_string()
        } else {
            format!("all {} workers", st.workers_configured)
        }
    );
    print_last_output(app).await;
    let port = app.config.as_ref().and_then(|p| Config::load(p).ok()).and_then(|c| c.app.port);
    let fix = match port {
        Some(p) if reason == "not ready in time" => format!(
            "it never listened on port {p} within [workers] ready_timeout: is another program on it (`ss -ltnp \
             'sport = :{p}'`)? does the app listen on process.env.PORT?"
        ),
        _ if reason.starts_with("spawn failed") => {
            "the command could not be run: check [app] command, args and working_directory (`warden check`)".into()
        }
        _ => format!("fix the error above (all of it: `warden logs {name} --err`)"),
    };
    let then = if stopped {
        format!(
            "Its workers are stopped: {name} stays listed (errored) and nothing restarts it; `warden start {name}` \
             tries again"
        )
    } else {
        format!("Warden keeps restarting it with backoff; `warden stop {name}` stops it")
    };
    eprintln!("  hint: {fix}. {then}");
    1
}

/// The app's last error output (stderr; its stdout if it wrote none), each
/// line once: workers that all fail print the same lines.
async fn print_last_output(app: &App) {
    for stream in ["stderr", "stdout"] {
        let req = Request::Logs { lines: 200, follow: false, worker: None, events: false, stream: Some(stream.into()) };
        let mut buf: Vec<u8> = Vec::new();
        if tokio::time::timeout(REQUEST_TIMEOUT, control::call(&app.socket, &req, &mut buf)).await.is_err() {
            return;
        }
        let lines = last_unique_output(&String::from_utf8_lossy(&buf), 15);
        if !lines.is_empty() {
            eprintln!("  its last {}:", if stream == "stderr" { "error output" } else { "output" });
            for l in lines {
                eprintln!("    {l}");
            }
            return;
        }
    }
    eprintln!("  (no output from it in `warden logs {}`)", app.name);
}

/// The text of the last `n` distinct output lines (`<ts> OUT   worker=1
/// stderr: text` → `text`), in the order first written.
fn last_unique_output(text: &str, n: usize) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let unique: Vec<String> = text
        .lines()
        .filter_map(|l| l.split_once(" OUT   worker=")?.1.split_once(": ").map(|(_, t)| t.to_string()))
        .filter(|t| !t.trim().is_empty() && seen.insert(t.clone()))
        .collect();
    unique[unique.len().saturating_sub(n)..].to_vec()
}

/// Run a supervisor detached from this terminal, logging to a rotated file.
pub(crate) fn spawn_background(name: &str, cfg: &Path) -> Result<std::process::Child, String> {
    spawn_background_as(name, cfg, None)
}

/// The environment and working directory a supervisor was started with.
///
/// Trust: wardend restarts a dead supervisor with this environment, as its
/// own user. It only adopts supervisors whose control socket sits in its
/// runtime directory, which `control::ensure_private_dir` guarantees only
/// that user can write (owned by it, not a symlink, no group/other write).
/// So no other user can plant a supervisor, or an environment, for wardend
/// to run; keep that check in front of any new way of finding supervisors.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Origin {
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub cwd: Option<PathBuf>,
}

impl Origin {
    /// Read from /proc: what `pid` was started with (same user, or root).
    pub(crate) fn of(pid: u32) -> Option<Origin> {
        use std::os::unix::ffi::OsStrExt;
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let env: Vec<_> = raw
            .split(|b| *b == 0)
            .filter_map(|kv| {
                let i = kv.iter().position(|b| *b == b'=')?;
                let (k, v) = (std::ffi::OsStr::from_bytes(&kv[..i]), std::ffi::OsStr::from_bytes(&kv[i + 1..]));
                Some((k.to_os_string(), v.to_os_string()))
            })
            .collect();
        // A zombie shows an empty environment: better ours than none.
        if env.is_empty() {
            return None;
        }
        Some(Origin { env, cwd: std::fs::read_link(format!("/proc/{pid}/cwd")).ok() })
    }
}

/// `spawn_background`, but with `origin`'s environment and working
/// directory instead of ours: wardend restarts a supervisor exactly as
/// `warden start` started it (the same PATH to bun or node, the same
/// variables), not with wardend's own.
pub(crate) fn spawn_background_as(
    name: &str,
    cfg: &Path,
    origin: Option<&Origin>,
) -> Result<std::process::Child, String> {
    let cfg = std::fs::canonicalize(cfg).unwrap_or_else(|_| cfg.to_path_buf());
    let args = [std::ffi::OsStr::new("start"), std::ffi::OsStr::new("-c"), cfg.as_os_str()];
    spawn_detached(&args, &log_path(name), Some((crate::events::LAUNCH_ENV, "background")), origin)
        .map_err(|e| format!("starting the supervisor: {e}"))
}

/// `warden daemon [--resurrect]` (wardend) in the background, logging to
/// `<state dir>/logs/wardend.log`.
pub(crate) fn spawn_daemon(resurrect: bool) -> Result<std::process::Child, String> {
    let mut args = vec![std::ffi::OsStr::new("daemon")];
    if resurrect {
        args.push(std::ffi::OsStr::new("--resurrect"));
    }
    spawn_detached(&args, &crate::daemon::log_path(), None, None)
}

/// What systemd sets for the unit it runs. A process we start in the
/// background is not that unit: with `INVOCATION_ID` it would report the
/// unit as its own (and `warden kill` would stop the unit), with
/// `JOURNAL_STREAM` it would log for journald into a file.
const SYSTEMD_ENV: [&str; 8] = [
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "NOTIFY_SOCKET",
    "WATCHDOG_USEC",
    "WATCHDOG_PID",
    "LISTEN_FDS",
    "LISTEN_PID",
    "LISTEN_FDNAMES",
];

/// This binary. After an in-place upgrade Linux reports the old inode as
/// "<path> (deleted)"; the path itself now holds the new binary.
pub(crate) fn own_exe() -> Result<PathBuf, String> {
    let p = std::env::current_exe().map_err(|e| format!("cannot find the warden binary: {e}"))?;
    Ok(match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => p,
    })
}

/// Run `warden <args>` in its own session (no terminal, own process
/// group: Ctrl-C here or closing the shell does not reach it), stdout and
/// stderr to `log`. No parent-death signal: it outlives us. With `origin`:
/// its environment and working directory instead of ours.
fn spawn_detached(
    args: &[&std::ffi::OsStr],
    log: &Path,
    env: Option<(&str, &str)>,
    origin: Option<&Origin>,
) -> Result<std::process::Child, String> {
    use std::os::unix::process::CommandExt;
    let exe = own_exe()?;
    if let Some(d) = log.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("creating {}: {e}", d.display()))?;
    }
    let err = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("opening {}: {e}", log.display()))?;
    let mut cmd = std::process::Command::new(exe);
    if let Some(o) = origin {
        cmd.env_clear().envs(o.env.iter().map(|(k, v)| (k, v)));
        if let Some(cwd) = o.cwd.as_ref().filter(|d| d.is_dir()) {
            cmd.current_dir(cwd);
        }
    }
    cmd.args(args)
        .env("WARDEN_LOG_FILE", log)
        .env("WARDEN_LOG_STDOUT", "0")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(err);
    for k in SYSTEMD_ENV {
        cmd.env_remove(k);
    }
    if let Some((k, v)) = env {
        cmd.env(k, v);
    }
    // SAFETY: the closure only calls setsid (async-signal-safe): it detaches
    // from our terminal and process group, so Ctrl-C here or closing the
    // shell does not reach the child.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(crate::sys::child_new_session);
    }
    cmd.spawn().map_err(|e| e.to_string())
}

/// The last `n` lines of a file (read from its last 64 KB only).
pub(crate) fn tail(path: &Path, n: usize) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    const WINDOW: u64 = 64 * 1024;
    let Ok(mut f) = std::fs::File::open(path) else { return Vec::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(WINDOW);
    let mut buf = Vec::new();
    if f.seek(SeekFrom::Start(from)).is_err() || f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0); // probably cut in the middle
    }
    lines[lines.len().saturating_sub(n)..].iter().map(|s| s.to_string()).collect()
}

pub(crate) fn write_private(path: &Path, text: &str, mode: u32) -> Result<(), String> {
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
    let running_unit = st.and_then(|s| s.unit.clone().map(|u| (scope_of_running_unit(Some(s.pid)), u)));
    // A supervisor that answers and runs outside systemd (started in the
    // background before `warden startup` installed a unit for it) is not
    // the unit: `systemctl stop` would succeed on the inactive unit and
    // leave it running. Stop it directly; `delete` also disables the unit.
    let outside_systemd = st.is_some_and(|s| s.unit.is_none());
    let installed = if outside_systemd { None } else { systemd_unit_for(app) };
    if let Some((scope, unit)) = running_unit.or(installed) {
        let verb = if disable { vec!["disable", "--now"] } else { vec!["stop"] };
        let mut a = verb.clone();
        a.push(&unit);
        run_systemctl(scope, &a)?;
        return Ok(format!("{} {unit}", if disable { "disabled and stopped" } else { "stopped" }));
    }
    let also_disable = if disable && outside_systemd { systemd_unit_for(app) } else { None };
    let stopped = shutdown_directly(app).await?;
    match also_disable {
        Some((scope, unit)) => {
            run_systemctl(scope, &["disable", &unit])?;
            Ok(format!("{stopped}; {unit} disabled"))
        }
        None => Ok(stopped),
    }
}

/// Ask a supervisor to shut down and wait until its socket is gone.
async fn shutdown_directly(app: &App) -> Result<String, String> {
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
    let code = delete_inner(args, target).await;
    show_apps(args, true).await;
    code
}

async fn delete_inner(args: &Args, target: &str) -> i32 {
    let ctx = context(args);
    let sels = match resolve(&ctx, Some(target), true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warden: {e}");
            return 2;
        }
    };
    let mut worst = 0;
    // Once per app: `delete 0 0:1` must not try to move the config twice.
    for app in unique_apps(&sels) {
        let s = Sel { app, worker: None, standby: None };
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
                Ok(()) => {
                    println!("{}: config moved to {}", s.app.name, dest.display());
                    // Its number is free again (a later `warden list` won't show it).
                    if let Err(e) = crate::ids::forget(&state_dir(), &s.app.name) {
                        eprintln!("warden: {}: its id stays reserved: {e}", s.app.name);
                    }
                }
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
    // Everything (no target, or `all`): the apps, then wardend.
    let everything = !ctx.single && args.target.as_deref().is_none_or(|t| t == "all");
    let sels = match resolve(&ctx, args.target.as_deref().or(Some("all")), false) {
        Ok(s) => s,
        Err(_) if everything && ctx.apps.is_empty() => Vec::new(),
        Err(e) => {
            eprintln!("warden: {e}");
            return 2;
        }
    };
    let running: Vec<&Sel> = sels.iter().filter(|s| s.app.socket.exists()).collect();
    if running.is_empty() {
        println!("no app is running");
        return if everything { stop_wardend().await } else { 0 };
    }
    if !args.yes && crate::sys::isatty(0) {
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
    if everything {
        worst = worst.max(stop_wardend().await);
    }
    worst
}

/// `warden kill` (everything): wardend goes last, once the apps are down.
async fn stop_wardend() -> i32 {
    match crate::daemon::client::stop_daemon().await {
        Ok(Some(what)) => {
            println!("wardend: {what}");
            0
        }
        Ok(None) => 0,
        Err(e) => {
            eprintln!("warden: wardend: {e}");
            1
        }
    }
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

/// Names of the apps `warden save` remembered.
pub fn saved_names() -> Vec<String> {
    read_dump().ok().flatten().map(|d| d.apps.into_iter().map(|a| a.name).collect()).unwrap_or_default()
}

/// What `warden save` recorded (`None`: nothing saved yet).
pub(crate) fn saved_apps() -> Result<Option<Vec<Saved>>, String> {
    Ok(read_dump()?.map(|d| d.apps))
}

pub(crate) fn dump_file() -> PathBuf {
    dump_path()
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
            Some((scope, u)) => match run_systemctl(scope, &["enable", &u]) {
                Ok(()) => println!("{}: saved ({} workers{}); {u} enabled at boot", s.name, s.workers, stopped_note(s)),
                Err(e) => eprintln!("warden: {}: saved, but {e}", s.name),
            },
            None => println!("{}: saved ({} workers{})", s.name, s.workers, stopped_note(s)),
        }
    }
    println!("saved {} app(s) to {}", saved.len(), dump_path().display());
    if !saved.is_empty() && crate::startup::installed().is_none() {
        println!(
            "nothing starts them after a reboot yet: `warden startup` sets that up (systemd or launchd), or run \
             `warden resurrect` from your init system"
        );
    }
    0
}

fn stopped_note(s: &Saved) -> &'static str {
    if s.stopped { ", stopped" } else { "" }
}

pub async fn resurrect(args: &Args) -> i32 {
    let code = resurrect_inner(args).await;
    show_apps(args, false).await;
    code
}

async fn resurrect_inner(args: &Args) -> i32 {
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
        worst = worst.max(start_app(&ctx, &app, OnFailedStart::Report).await);
    }
    worst
}

// -------------------------------------------------------------------- top

pub async fn top(args: &Args) -> i32 {
    let tty = crate::sys::isatty(1);
    loop {
        let ctx = context(args);
        let apps: Vec<App> = match resolve(&ctx, args.target.as_deref(), false) {
            Ok(s) => unique_apps(&s),
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
        out += &cli::render_list_with(&all, &if tty { Fmt::stdout() } else { Fmt::PLAIN });
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
            id: None,
        }
    }

    fn numbered(name: &str, ns: &str, id: u32) -> App {
        App { id: Some(id), ..app(name, ns) }
    }

    /// api 0, web 1, queue 2: a host numbered the way `ids::assign` does.
    fn ctx() -> Ctx {
        Ctx {
            apps: vec![numbered("api", "backend", 0), numbered("web", "backend", 1), numbered("queue", "default", 2)],
            single: false,
            no_wait: false,
            ids_warning: None,
        }
    }

    /// "api", "web:1" … from a list of selections.
    fn show(v: Vec<Sel>) -> Vec<String> {
        v.into_iter()
            .map(|s| match (s.worker, s.standby) {
                (Some(w), _) => format!("{}:{w}", s.app.name),
                (None, Some(sb)) => format!("{}:{sb}", s.app.name),
                _ => s.app.name,
            })
            .collect()
    }

    #[test]
    fn ids_lists_and_ranges_pick_apps() {
        let c = ctx();
        let pick = |t: &str| resolve(&c, Some(t), true).map(show);
        // `warden start 0,1,2`, in the order given, and a single id.
        assert_eq!(pick("0,1,2").unwrap(), ["api", "web", "queue"]);
        assert_eq!(pick("2,0").unwrap(), ["queue", "api"]);
        assert_eq!(pick("1").unwrap(), ["web"]);
        // Names, namespaces, workers and ids mix; spaces around commas are fine.
        assert_eq!(pick("queue, 0:1").unwrap(), ["queue", "api:1"]);
        assert_eq!(pick("backend,2").unwrap(), ["api", "web", "queue"]);
        // The same app twice (or through its namespace) is picked once.
        assert_eq!(pick("0,api,backend").unwrap(), ["api", "web"]);
        assert_eq!(pick("0,0").unwrap(), ["api"]);
        // Ranges; a trailing comma is harmless.
        assert_eq!(pick("0-1").unwrap(), ["api", "web"]);
        assert_eq!(pick("1-2,").unwrap(), ["web", "queue"]);
        assert_eq!(pick("0-99").unwrap(), ["api", "web", "queue"], "ids that do not exist inside a range are skipped");
        // Wrong in any item: nothing is picked (no half-done `restart`).
        let e = pick("0,7").unwrap_err();
        assert!(e.contains("no app has the id 7") && e.contains("0-2"), "{e}");
        assert!(pick("0,nope").unwrap_err().contains("no app or namespace named \"nope\""));
        assert!(pick("7-9").unwrap_err().contains("no app has the id 7-9"));
        assert!(pick("5-2").unwrap_err().contains("a range goes up"));
        assert!(pick("0-99999").unwrap_err().contains("too long"));
        assert!(pick(",").unwrap_err().contains("no app named"));
    }

    #[test]
    fn a_name_wins_over_the_same_number_and_numbers_need_ids() {
        // An app called "1" (a legal name) is picked by `1`, not the app with id 1.
        let mut c = ctx();
        c.apps.push(numbered("1", "default", 3));
        assert_eq!(show(resolve(&c, Some("1"), true).unwrap()), ["1"]);
        assert_eq!(show(resolve(&c, Some("3"), true).unwrap()), ["1"], "id 3 is the app named 1");
        // A name that looks like a range is a name when an app has it.
        c.apps.push(numbered("2-3", "default", 4));
        assert_eq!(show(resolve(&c, Some("2-3"), true).unwrap()), ["2-3"]);
        // Apps with no number (found running from a config elsewhere) are named only.
        let c = Ctx { apps: vec![app("a", "default"), numbered("b", "default", 0)], ..ctx() };
        assert_eq!(show(resolve(&c, Some("0"), true).unwrap()), ["b"]);
        let none = Ctx { apps: vec![app("a", "default")], ..ctx() };
        assert!(resolve(&none, Some("0"), true).unwrap_err().contains("which you can name"));
    }

    #[test]
    fn numbers_mean_workers_in_a_single_app_context() {
        let c = Ctx { apps: vec![app("api", "default")], single: true, no_wait: false, ids_warning: None };
        let pick = |t: &str| resolve(&c, Some(t), true).map(show);
        assert_eq!(pick("1").unwrap(), ["api:1"]);
        assert_eq!(pick("0,2").unwrap(), ["api:0", "api:2"]);
        assert_eq!(pick("0-2").unwrap(), ["api:0", "api:1", "api:2"]);
        assert_eq!(pick("api,3").unwrap(), ["api", "api:3"]);
        assert!(pick("web").is_err());
    }

    /// Naming one app several ways (workers, an id and a name, `-c` with
    /// worker numbers) lists and describes it once.
    #[test]
    fn an_app_picked_twice_is_listed_once() {
        let c = ctx();
        let sels = resolve(&c, Some("0,api:1,api:2,web"), true).unwrap();
        assert_eq!(sels.len(), 4);
        let names: Vec<String> = unique_apps(&sels).into_iter().map(|a| a.name).collect();
        assert_eq!(names, ["api", "web"]);
        let single = Ctx { apps: vec![app("api", "default")], single: true, no_wait: false, ids_warning: None };
        let sels = resolve(&single, Some("0,1"), true).unwrap();
        assert_eq!((sels.len(), unique_apps(&sels).len()), (2, 1));
    }

    #[test]
    fn what_start_treats_as_ids_or_a_list_of_apps() {
        for yes in ["3", "0,1,2", "0-2", "api,web", "api,2", "0,"] {
            assert!(is_selector_list(yes), "{yes}");
        }
        // A script, a path, a command line: still tried as a program.
        for no in ["server.js", "./a.js", "python3 -m http.server", "node -e a,b", "api", "my-app"] {
            assert!(!is_selector_list(no), "{no}");
        }
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
        assert!(resolve(&c, Some("3"), true).unwrap_err().contains("no app has the id 3"));
        assert!(resolve(&c, Some("nope"), true).unwrap_err().contains("no app or namespace"));
        assert!(resolve(&c, Some("backend:1"), true).unwrap_err().contains("needs an app"));
        assert!(resolve(&c, Some("api:x"), true).is_err());
        // Hot standbys, as the standby hints name them.
        let standby =
            |t: &str| resolve(&c, Some(t), false).map(|v| (v[0].app.name.clone(), v[0].worker, v[0].standby.clone()));
        assert_eq!(standby("api:standby"), Ok(("api".into(), None, Some("standby".into()))));
        assert_eq!(standby("api:s2"), Ok(("api".into(), None, Some("s2".into()))));
        assert!(standby("api:s0").unwrap_err().contains("`standby`"));
        assert!(standby("backend:standby").unwrap_err().contains("needs an app"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[cfg(target_os = "linux")] // /proc
    fn origin_is_read_from_proc() {
        let me = Origin::of(std::process::id()).expect("our own environment");
        let path = me.env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.clone());
        assert_eq!(path, std::env::var_os("PATH"));
        assert_eq!(me.cwd, std::env::current_dir().ok());
        assert_eq!(Origin::of(u32::MAX / 2), None, "no such process");
    }

    /// What a failed `warden start` prints of the app's output: each line
    /// once (every worker prints the same error), the last ones.
    #[test]
    fn failed_start_shows_each_output_line_once() {
        let text = "2026-10-01T10:00:00.000Z OUT   worker=1 stderr: error: Cannot find package 'x'\n\
                    2026-10-01T10:00:00.001Z OUT   worker=2 stderr: error: Cannot find package 'x'\n\
                    2026-10-01T10:00:00.002Z OUT   worker=1 stderr: Bun v1.3.13 (Linux x64)\n\
                    2026-10-01T10:00:00.003Z OUT   worker=1 stderr: \n\
                    2026-10-01T10:00:00.004Z OUT   worker=2 stderr: Bun v1.3.13 (Linux x64)\n\
                    2026-10-01T10:00:00.005Z INFO  not output\n";
        assert_eq!(last_unique_output(text, 15), vec!["error: Cannot find package 'x'", "Bun v1.3.13 (Linux x64)"]);
        assert_eq!(last_unique_output(text, 1), vec!["Bun v1.3.13 (Linux x64)"]);
        assert!(last_unique_output("", 15).is_empty());
    }

    #[test]
    fn env_output_groups_and_marks_overrides() {
        let info = serde_json::json!({
            "config": {"app": {"env_file": "/etc/warden/api.env"}},
            "worker_env_of": "2",
            "worker_env": [
                {"name": "DB", "value": "(hidden, 6 chars)", "from": "env_file"},
                {"name": "PORT", "value": "9", "from": "env"},
                {"name": "WARDEN_WORKER_ID", "value": "2", "from": "warden"},
                {"name": "PORT", "value": "3001", "from": "warden"},
            ],
        });
        let text = render_env(&info, false);
        assert!(text.starts_with("# The environment worker 2 starts with"), "{text}");
        assert!(
            text.contains("# from /etc/warden/api.env\nDB=(hidden, 6 chars)\n# from [app] env\nPORT=9\n"),
            "{text}"
        );
        assert!(text.contains("PORT=3001   # overrides the value from env\n"), "{text}");
        assert!(text.contains("unless --show-secrets"), "{text}");
        // Every value line is KEY=value: `grep ^PORT=` works.
        assert!(text.lines().filter(|l| !l.starts_with('#')).all(|l| l.contains('=')), "{text}");
        // An older supervisor: its app variables only.
        let old = serde_json::json!({"config": {"app": {"env": {"A": "b"}}}});
        assert!(render_env(&old, true).starts_with("A=b\n# also set by Warden"));
    }

    #[test]
    fn a_running_units_manager_comes_from_its_cgroup() {
        let user = "0::/user.slice/user-1001.slice/user@1001.service/app.slice/app-warden.slice/warden@api.service\n";
        assert_eq!(scope_from_cgroup(user), Scope::User);
        let system = "0::/system.slice/system-warden.slice/warden@api.service\n";
        assert_eq!(scope_from_cgroup(system), Scope::System);
        // cgroup v1: the name=systemd hierarchy has the same path.
        let v1 =
            "4:memory:/user.slice\n1:name=systemd:/user.slice/user-1000.slice/user@1000.service/warden@api.service\n";
        assert_eq!(scope_from_cgroup(v1), Scope::User);
        // A login session's scope is not a user manager's unit.
        assert_eq!(scope_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"), Scope::System);
    }

    #[test]
    fn tail_reads_the_last_lines() {
        let f = std::env::temp_dir().join(format!("warden-tail-{}", std::process::id()));
        let text: String = (0..20_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&f, text).unwrap();
        assert_eq!(tail(&f, 2), vec!["line 19998".to_string(), "line 19999".to_string()]);
        assert_eq!(tail(&f, 100_000).len(), 64 * 1024 / 11, "only the last 64 KB, first partial line dropped");
        let _ = std::fs::remove_file(&f);
        assert!(tail(&f, 3).is_empty());
    }

    #[test]
    fn single_app_targets() {
        let c = Ctx { apps: vec![app("api", "default")], single: true, no_wait: false, ids_warning: None };
        let one = |t: Option<&str>| resolve(&c, t, true).map(|v| v[0].worker);
        assert_eq!(one(None), Ok(None));
        assert_eq!(one(Some("2")), Ok(Some(2)));
        assert_eq!(one(Some(":2")), Ok(Some(2)));
        assert_eq!(one(Some("api:3")), Ok(Some(3)));
        assert!(one(Some("web")).is_err());
        // `warden logs -c app.toml --worker standby` is the target ":standby".
        let standby = |t: &str| resolve(&c, Some(t), false).map(|v| (v[0].worker, v[0].standby.clone()));
        assert_eq!(standby(":standby"), Ok((None, Some("standby".into()))));
        assert_eq!(standby("api:s1"), Ok((None, Some("s1".into()))));
        assert_eq!(standby("s3"), Ok((None, Some("s3".into()))));
        assert!(standby(":sx").is_err());
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

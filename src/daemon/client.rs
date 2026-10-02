//! The CLI side of wardend: starting it (`warden start`, `resurrect`, the supervisors when it
//! dies), stopping it (`warden kill`), the internal `warden wardend [--background | status]`, and
//! `warden events`.

use super::watcher::{self, WatchMsg};
use super::{Frame, log_path, socket_path};
use crate::events::{AppEntry, AppState, DaemonReply, DaemonRequest, Event, SupervisorEvent, WorkerEvent};
use crate::fleet;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, watch};

/// One request, one reply.
async fn request(path: &Path, req: &DaemonRequest, limit: Duration) -> Result<DaemonReply, String> {
    let exchange = async {
        let s = UnixStream::connect(path)
            .await
            .map_err(|e| format!("wardend is not running (no answer on {}: {e})", path.display()))?;
        let (r, mut w) = s.into_split();
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        w.write_all(line.as_bytes()).await.map_err(|e| e.to_string())?;
        let mut answer = String::new();
        BufReader::new(r).read_line(&mut answer).await.map_err(|e| e.to_string())?;
        serde_json::from_str::<DaemonReply>(answer.trim()).map_err(|e| format!("bad answer from wardend: {e}"))
    };
    tokio::time::timeout(limit, exchange)
        .await
        .map_err(|_| format!("wardend did not answer within {} s", limit.as_secs()))?
}

/// wardend's pid, if it answers on `path`.
pub(crate) async fn hello_pid(path: &Path) -> Option<u32> {
    if !path.exists() {
        return None;
    }
    match request(path, &DaemonRequest::Hello, Duration::from_secs(2)).await {
        Ok(DaemonReply { hello: Some(Event::Hello { pid, .. }), .. }) => Some(pid),
        _ => None,
    }
}

/// `warden wardend --background [--resurrect]`.
pub async fn start_background(resurrect: bool) -> i32 {
    let path = socket_path();
    if let Some(pid) = hello_pid(&path).await {
        println!("wardend is already running (pid {pid})");
        return 0;
    }
    let mut child = match fleet::spawn_daemon(resurrect) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warden: cannot start wardend: {e}");
            return 1;
        }
    };
    let log = log_path();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if let Ok(Some(st)) = child.try_wait() {
            eprintln!("warden: wardend exited ({st}) while starting; last log lines:");
            for l in fleet::tail(&log, 10) {
                eprintln!("  {l}");
            }
            return 1;
        }
        if hello_pid(&path).await.is_some() {
            println!("wardend started in the background (pid {}), log {}", child.id(), log.display());
            return 0;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("warden: wardend did not answer on {} within 10 s; see {}", path.display(), log.display());
    1
}

/// `WARDEN_NO_DAEMON=1`: never start wardend (tests, containers that run none).
pub(crate) fn disabled() -> bool {
    std::env::var("WARDEN_NO_DAEMON").is_ok_and(|v| v == "1")
}

/// Called by `warden start` and `warden resurrect`: wardend is always on, like PM2's
/// daemon, so start it unless it runs already or `WARDEN_NO_DAEMON=1`.
pub(crate) async fn autostart() {
    // Apps start together (`resurrect`, `start a b c`), and each asks: one at a time, so the
    // first starts wardend and the others find it answering.
    static ONE_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    let _turn = ONE_AT_A_TIME.lock().await;
    if disabled() {
        return;
    }
    if hello_pid(&socket_path()).await.is_some() {
        return;
    }
    // systemd or launchd runs it (`warden startup`): a second one started by hand would
    // only fight it for the socket, so the manager starts it. (After `warden kill` it
    // is stopped and stays so until the next login or boot: `resurrect` must not
    // leave the apps without it.)
    if crate::startup::wardend_managed() {
        let started = tokio::task::spawn_blocking(crate::startup::start_wardend_managed)
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
        match started {
            Ok(what) => {
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(10) {
                    if let Some(pid) = hello_pid(&socket_path()).await {
                        println!("wardend started ({what}, pid {pid}): it restarts supervisors that die");
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                eprintln!(
                    "warden: {what}, but wardend does not answer yet (log {}); `warden doctor` says what it sees",
                    log_path().display()
                );
            }
            Err(e) => eprintln!(
                "warden: wardend is set up as a service (`warden startup`) but is not running, so nothing restarts \
                 this supervisor if it dies; starting it failed ({e}). `warden startup` sets it up again"
            ),
        }
        return;
    }
    let mut child = match fleet::spawn_daemon(false) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warden: could not start wardend ({e}); the app runs without it");
            return;
        }
    };
    // Wait until it answers: whatever comes next (the apps, `warden events`, a second command)
    // finds it there, and a wardend that cannot start says so here, not later.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if let Ok(Some(st)) = child.try_wait() {
            // Another one may have won the race for the socket: then it answers.
            if hello_pid(&socket_path()).await.is_none() {
                eprintln!(
                    "warden: wardend exited ({st}) while starting (log {}); the app runs without it",
                    log_path().display()
                );
            }
            return;
        }
        if hello_pid(&socket_path()).await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    println!(
        "wardend started in the background (pid {}): it restarts supervisors that die, and they bring it \
         back if it dies; `warden kill` stops it, WARDEN_NO_DAEMON=1 skips it",
        child.id()
    );
}

/// Stop wardend; `Ok(None)` when it was not running, else what was done.
/// Apps keep running. Under systemd (`Restart=always`) stopping the process
/// would only restart it: the unit is stopped instead.
pub(crate) async fn stop_daemon() -> Result<Option<String>, String> {
    if fleet::systemctl_bin().is_some() {
        for scope in [fleet::Scope::System, fleet::Scope::User] {
            if !scope.has_unit("wardend.service") || !fleet::unit_active(scope, "wardend.service") {
                continue;
            }
            return match fleet::run_systemctl(scope, &["stop", "wardend.service"]) {
                Ok(()) => Ok(Some(
                    "stopped wardend.service (systemd starts it again at boot; `warden unstartup` removes it)".into(),
                )),
                Err(e) => Err(format!(
                    "wardend runs as the systemd unit wardend.service, which would start it again if only its \
                     process stopped, and stopping the unit failed ({e}). Fix: `sudo systemctl {}stop wardend`",
                    scope.shown()
                )),
            };
        }
    }
    let Some(pid) = stop_daemon_process().await? else { return Ok(None) };
    let note = if crate::startup::wardend_managed() {
        " (launchd starts it again at the next login or boot; `warden unstartup` removes it)"
    } else {
        ""
    };
    Ok(Some(format!("stopped (pid {pid}){note}")))
}

/// Ask the running wardend process to exit; its pid, `None` if none runs.
pub(crate) async fn stop_daemon_process() -> Result<Option<u32>, String> {
    let path = socket_path();
    let Some(pid) = hello_pid(&path).await else { return Ok(None) };
    let r = request(&path, &DaemonRequest::Shutdown, Duration::from_secs(5)).await?;
    if !r.ok {
        return Err(r.message.unwrap_or_else(|| "wardend refused to stop".into()));
    }
    // It removes its socket just before it exits.
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(5) {
        if !path.exists() || std::os::unix::net::UnixStream::connect(&path).is_err() {
            return Ok(Some(pid));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Err(format!("wardend (pid {pid}) is still answering 5 s after `shutdown`"))
}

/// `warden wardend status [--json]`: exit 1 when wardend is not running.
pub async fn status(json: bool) -> i32 {
    let path = socket_path();
    let hello = match request(&path, &DaemonRequest::Hello, Duration::from_secs(2)).await {
        Ok(r) => r.hello,
        Err(_) => {
            eprintln!("wardend is not running (no answer on {}); `warden resurrect` starts it", path.display());
            return 1;
        }
    };
    let apps = match request(&path, &DaemonRequest::Apps, Duration::from_secs(5)).await {
        Ok(r) => r.apps.unwrap_or_default(),
        Err(e) => {
            eprintln!("warden: {e}");
            return 1;
        }
    };
    if json {
        let v = serde_json::json!({ "hello": hello, "apps": apps });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return 0;
    }
    if let Some(Event::Hello { pid, version, protocol, .. }) = hello {
        println!("wardend: pid {pid}, version {version}, protocol {protocol}, socket {}", path.display());
    }
    if apps.is_empty() {
        println!("no apps yet: `warden start server.js --name api`");
        return 0;
    }
    use crate::table::{Cell, DIM, GREEN, KEY, NAME, RED, YELLOW};
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    let ids = crate::ids::lookup(&fleet::state_dir(), &names);
    let rows: Vec<Vec<Cell>> = apps
        .iter()
        .map(|a| {
            let state = state_name(a.state);
            let style = match state {
                "running" | "ok" => GREEN,
                "stopped" | "offline" => DIM,
                "lost" | "failed" | "dead" | "crashed" => RED,
                _ => YELLOW,
            };
            vec![
                Cell::styled(ids.get(&a.name).map_or("-".to_string(), |i| i.to_string()), KEY),
                Cell::styled(a.name.clone(), NAME),
                Cell::styled(state, style),
                Cell::plain(a.supervised_by.clone()),
                Cell::plain(a.supervisor_pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())),
                Cell::plain(a.supervisor_restarts.to_string()),
                Cell::plain(a.problem.clone().unwrap_or_else(|| "-".into())),
            ]
        })
        .collect();
    let header = ["id", "name", "state", "supervised by", "pid", "↺", "problem"];
    print!("{}", crate::table::boxed(Some(&header), &rows, &crate::table::Fmt::stdout(), Some(6)));
    0
}

// ------------------------------------------------------------------ alerts

/// `warden check -c wardend.toml`, and `warden doctor`: validate the alert rules
/// (`<config dir>/wardend.toml`, or FILE). Exit 1 on any problem.
pub fn check(file: Option<std::path::PathBuf>) -> i32 {
    use super::alerts;
    let (path, explicit) = match file {
        Some(p) => (p, true),
        None => (alerts::path(), false),
    };
    let cfg = match alerts::load(&path) {
        Ok(Some(c)) => c,
        Ok(None) if explicit => {
            eprintln!("warden: {} does not exist", path.display());
            return 1;
        }
        Ok(None) => {
            println!(
                "{}: not there, so wardend sends no alerts (an example: `[[alert]]` in docs/protocol.md, \"Alerts\")",
                path.display()
            );
            return 0;
        }
        Err(problems) => {
            let n = problems.len();
            eprintln!("warden: {} has {n} problem{}:", path.display(), if n == 1 { "" } else { "s" });
            for p in &problems {
                eprintln!("  - {p}");
            }
            eprintln!(
                "A running wardend keeps the rules it has until the file is fixed, then reads it again by itself."
            );
            return 1;
        }
    };
    let n = cfg.rules.len();
    println!("{}: ok, {n} alert rule{}", path.display(), if n == 1 { "" } else { "s" });
    let known: Vec<String> = fleet::discover().into_iter().map(|a| a.name).collect();
    let mut notes = cfg.notes.clone();
    for r in &cfg.rules {
        let kinds = r.kinds();
        let on = if kinds.len() == alerts::AlertKind::ALL.len() {
            "all events".to_string()
        } else {
            kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")
        };
        let apps = if r.apps.is_empty() { "every app".to_string() } else { r.apps.join(", ") };
        println!(
            "  {}: {on} of {apps} → {} (min_interval {})",
            r.name,
            r.target.shown(),
            alerts::show_duration(r.min_interval)
        );
        for a in r.apps.iter().filter(|a| !known.contains(a)) {
            notes.push(format!("{}: there is no app {a:?} on this host (yet)", r.name));
        }
    }
    println!(
        "  crash loop: {} worker crashes within {}",
        cfg.crash_loop_crashes,
        alerts::show_duration(cfg.crash_loop_window)
    );
    for note in notes {
        println!("warning: {note}");
    }
    0
}

// ------------------------------------------------------------------ events

/// `warden events [app] [--json] [--logs] [--interval MS]`: from wardend
/// when it runs, else straight from the apps' control sockets.
pub fn events(
    rt: &tokio::runtime::Runtime,
    target: Option<String>,
    json: bool,
    logs: bool,
    interval: Option<u64>,
) -> i32 {
    let local = tokio::task::LocalSet::new();
    rt.block_on(local.run_until(events_main(target, json, logs, interval)))
}

async fn events_main(target: Option<String>, json: bool, logs: bool, interval: Option<u64>) -> i32 {
    let apps = fleet::discover();
    // Apps by name, id, namespace or a list of them (`0,api`); nothing (or
    // `all`): every app.
    let names: Vec<String> = match target.as_deref() {
        None | Some("all") => Vec::new(),
        Some(t) => match fleet::resolve_names(t) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warden: {e}");
                return 2;
            }
        },
    };
    let mut out = Printer::new(json);
    if let Ok(s) = UnixStream::connect(socket_path()).await {
        match from_daemon(s, &names, logs, interval, &mut out).await {
            Some(code) => return code,
            None => eprintln!("warden: wardend does not answer; reading events from the apps directly"),
        }
    }
    let apps: Vec<fleet::App> = apps
        .into_iter()
        .filter(|a| names.is_empty() || names.contains(&a.name))
        .filter(|a| a.socket.exists())
        .collect();
    direct(apps, logs, interval, &mut out).await
}

/// `None`: wardend did not answer at all (fall back to the apps).
async fn from_daemon(
    s: UnixStream,
    names: &[String],
    logs: bool,
    interval: Option<u64>,
    out: &mut Printer,
) -> Option<i32> {
    let (r, mut w) = s.into_split();
    let req = DaemonRequest::Subscribe { interval_ms: interval, logs, apps: names.to_vec() };
    let mut line = serde_json::to_string(&req).ok()?;
    line.push('\n');
    tokio::time::timeout(Duration::from_secs(5), w.write_all(line.as_bytes())).await.ok()?.ok()?;
    let mut reader = BufReader::new(r);
    let mut first = true;
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        line.clear();
        let read = async {
            if first {
                tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut line))
                    .await
                    .unwrap_or_else(|_| Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer")))
            } else {
                reader.read_line(&mut line).await
            }
        };
        let n = tokio::select! {
            _ = &mut ctrl_c => return Some(0),
            n = read => n,
        };
        match n {
            Ok(0) | Err(_) if first => return None,
            Ok(0) | Err(_) => {
                eprintln!(
                    "warden: wardend closed the stream (it exited?); `warden events` reads the apps directly without it"
                );
                return Some(1);
            }
            Ok(_) => {}
        }
        first = false;
        if !out.line(&line) {
            return Some(0); // stdout closed
        }
        if line.starts_with(r#"{"type":"bye""#) {
            return Some(0);
        }
    }
}

/// No wardend: watch each running app's socket (stream, or polling an
/// older supervisor) until they have all exited.
async fn direct(apps: Vec<fleet::App>, logs: bool, interval: Option<u64>, out: &mut Printer) -> i32 {
    if apps.is_empty() {
        eprintln!("warden: no running app to watch (and wardend is not running)");
        return 1;
    }
    let (tx, mut rx) = mpsc::channel(64);
    let bus: super::Bus = broadcast::channel(super::BUS_CAPACITY).0;
    let logs_bus: super::Bus = broadcast::channel(super::BUS_CAPACITY).0;
    let mut frames = bus.subscribe();
    let mut log_frames = logs_bus.subscribe();
    let (_logs_tx, logs_rx) = watch::channel(logs);
    for (i, a) in apps.iter().enumerate() {
        let spec = watcher::Spec {
            app: a.name.as_str().into(),
            socket: a.socket.clone(),
            epoch: i as u64,
            pid: None,
            interval: crate::events::interval(interval),
        };
        tokio::task::spawn_local(watcher::run(spec, tx.clone(), bus.clone(), logs_bus.clone(), logs_rx.clone()));
    }
    drop(tx); // `rx` ends when every watcher has
    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        let ok = tokio::select! {
            // Queued events first: nothing is lost when the last app exits.
            biased;
            _ = &mut ctrl_c => return 0,
            f = frames.recv() => out.frame(f),
            f = log_frames.recv() => out.frame(f),
            m = rx.recv() => match m {
                Some(m) => out.watch_msg(m),
                None => return 0,
            },
        };
        if !ok {
            return 0;
        }
    }
}

// ----------------------------------------------------------------- printing

fn state_name(s: AppState) -> &'static str {
    match s {
        AppState::Running => "running",
        AppState::Stopped => "stopped",
        AppState::Starting => "starting",
        AppState::Unreachable => "unreachable",
        AppState::GaveUp => "gave_up",
        AppState::NotStarted => "not_started",
    }
}

fn worker_event_name(e: WorkerEvent) -> &'static str {
    match e {
        WorkerEvent::Starting => "starting",
        WorkerEvent::Ready => "ready",
        WorkerEvent::Unhealthy => "unhealthy",
        WorkerEvent::Hung => "hung",
        WorkerEvent::Crashed => "crashed",
        WorkerEvent::Exited => "exited",
        WorkerEvent::Restarting => "restarting",
        WorkerEvent::Failed => "failed",
        WorkerEvent::Stopping => "stopping",
        WorkerEvent::Stopped => "stopped",
    }
}

fn supervisor_event_name(e: SupervisorEvent) -> &'static str {
    match e {
        SupervisorEvent::Found => "found",
        SupervisorEvent::Started => "started",
        SupervisorEvent::Exited => "exited",
        SupervisorEvent::Died => "died",
        SupervisorEvent::Restarting => "restarting",
        SupervisorEvent::GaveUp => "gave up",
        SupervisorEvent::Unresponsive => "unresponsive",
        SupervisorEvent::Responsive => "responsive",
    }
}

/// `12:00:01`, local time.
fn clock(at_ms: u64) -> String {
    match crate::sys::localtime((at_ms / 1000) as i64) {
        Some(tm) => format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec),
        None => "--:--:--".into(),
    }
}

/// `12:00:09 api worker 2 crashed pid=4230 exit code 3`: a worker event as
/// `warden events` prints it. A hot standby is `worker s1` (the pool as a
/// whole `worker standby`), as `warden status` and the log lines name it.
fn worker_line(ev: &Event) -> Option<String> {
    let Event::Worker { app, worker, standby, event, pid, detail, at_ms } = ev else { return None };
    let who = crate::events::worker_name(*worker, *standby);
    let mut t = format!("{} {app} worker {who} {}", clock(*at_ms), worker_event_name(*event));
    if let Some(p) = pid {
        t += &format!(" pid={p}");
    }
    if let Some(d) = detail {
        t += &format!(" {d}");
    }
    Some(t)
}

/// One line per event for people; the raw NDJSON with `--json`.
pub(crate) struct Printer {
    out: crate::logview::PipeOut,
    json: bool,
    /// What the last printed status of each app showed.
    shown: HashMap<String, String>,
    /// Each app's last (state, pid) from `apps` events.
    states: HashMap<String, (AppState, Option<u32>)>,
}

impl Printer {
    pub fn new(json: bool) -> Printer {
        Printer { out: crate::logview::PipeOut::new(), json, shown: HashMap::new(), states: HashMap::new() }
    }

    fn print(&mut self, text: &str) -> bool {
        let ok = self.out.line(text);
        self.out.flush();
        ok && !self.out.closed
    }

    /// A raw event line. False once stdout is closed.
    pub fn line(&mut self, raw: &str) -> bool {
        if self.json {
            return self.print(raw.trim_end());
        }
        match serde_json::from_str::<Event>(raw.trim_end()) {
            Ok(ev) => self.event(&ev),
            Err(_) => true, // a newer wardend's event type: skip
        }
    }

    fn frame(&mut self, f: Result<Frame, broadcast::error::RecvError>) -> bool {
        match f {
            Ok(f) => self.line(&f.line),
            Err(broadcast::error::RecvError::Lagged(dropped)) => self.emit(&Event::Lagged { app: None, dropped }),
            Err(broadcast::error::RecvError::Closed) => true,
        }
    }

    /// Our own event (direct mode): as JSON or as text.
    fn emit(&mut self, ev: &Event) -> bool {
        if self.json {
            let s = serde_json::to_string(ev).unwrap_or_default();
            return self.print(&s);
        }
        self.event(ev)
    }

    fn watch_msg(&mut self, m: WatchMsg) -> bool {
        let sup = |app: &Arc<str>, event, pid, detail| Event::Supervisor {
            app: app.to_string(),
            event,
            pid,
            detail,
            at_ms: crate::events::now_ms(),
        };
        match m {
            WatchMsg::Attached { app, pid, streaming, .. } => {
                // A note, not an event: its status line is the event.
                if !streaming {
                    eprintln!(
                        "warden: {app} (pid {pid}) does not stream events (an older version?): polling its status, \
                         so only status changes show"
                    );
                }
                true
            }
            WatchMsg::Gone { app, pid, on_purpose, detail, .. } => {
                let e = if on_purpose { SupervisorEvent::Exited } else { SupervisorEvent::Died };
                self.emit(&sup(&app, e, Some(pid), Some(detail)))
            }
            WatchMsg::Unresponsive { app, pid, detail, .. } => {
                self.emit(&sup(&app, SupervisorEvent::Unresponsive, pid, Some(detail)))
            }
            WatchMsg::Responsive { app, .. } => self.emit(&sup(&app, SupervisorEvent::Responsive, None, None)),
            WatchMsg::Lost { app, detail, .. } => {
                eprintln!("warden: {app}: not reachable ({detail})");
                true
            }
            WatchMsg::Ended { app, panic, .. } => {
                eprintln!("warden: {app}: watching failed ({panic})");
                true
            }
            // The event's own line was printed from the bus.
            WatchMsg::Status { .. } | WatchMsg::RestartDue { .. } | WatchMsg::Event { .. } => true,
        }
    }

    fn event(&mut self, ev: &Event) -> bool {
        let now = crate::events::now_ms();
        let text = match ev {
            Event::Hello { app: None, pid, version, .. } => {
                format!("{} wardend pid={pid} version={version} (Ctrl-C to stop)", clock(now))
            }
            Event::Hello { app: Some(app), pid, .. } => format!("{} {app} connected pid={pid}", clock(now)),
            Event::Status { app, status: s } => {
                let mut counts: Vec<(&str, usize)> = Vec::new();
                for w in &s.workers {
                    match counts.iter_mut().find(|(k, _)| *k == w.state) {
                        Some((_, n)) => *n += 1,
                        None => counts.push((&w.state, 1)),
                    }
                }
                let mut t = format!("{}/{} workers ready", s.workers_ready, s.workers_configured);
                if counts.len() > 1 || counts.first().is_some_and(|(k, _)| *k != "RUNNING") {
                    let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
                    t += &format!(" ({})", parts.join(", "));
                }
                if s.stopped {
                    t += ", stopped";
                }
                if s.shutting_down {
                    t += ", shutting down";
                }
                // Only what changed: status comes every second.
                if self.shown.get(app).is_some_and(|prev| *prev == t) {
                    return true;
                }
                self.shown.insert(app.clone(), t.clone());
                format!("{} {app} {t}", clock(now))
            }
            ev @ Event::Worker { .. } => worker_line(ev).unwrap_or_default(),
            Event::Rollout { app, rollout: r } => {
                format!("{} {app} {} {}/{}: {}", clock(now), r.kind, r.done, r.total, r.phase)
            }
            Event::RolloutDone { app, outcome: o } => format!(
                "{} {app} {} {}: {} ({:.1} s)",
                clock(now),
                o.kind,
                if o.ok { "done" } else { "FAILED" },
                o.message,
                o.duration_secs
            ),
            Event::Log { app, line } => format!("{app} | {line}"),
            Event::Lagged { app, dropped } => format!(
                "{} {}: {dropped} events skipped (this output is slower than the events)",
                clock(now),
                app.as_deref().unwrap_or("events")
            ),
            Event::Apps { apps } => return self.apps(apps),
            Event::Supervisor { app, event, pid, detail, at_ms } => {
                let mut t = format!("{} {app} supervisor {}", clock(*at_ms), supervisor_event_name(*event));
                if let Some(p) = pid {
                    t += &format!(" pid={p}");
                }
                if let Some(d) = detail {
                    t += &format!(" {d}");
                }
                t
            }
            Event::Host { .. } => return true, // every second: `--json` has it
            Event::Bye { app: None, reason } => format!("{} wardend exiting ({reason})", clock(now)),
            Event::Bye { app: Some(app), reason } => format!("{} {app} supervisor exiting ({reason})", clock(now)),
        };
        self.print(&text)
    }

    /// One line per app whose state (or supervisor) changed.
    fn apps(&mut self, apps: &[AppEntry]) -> bool {
        let now = clock(crate::events::now_ms());
        for a in apps {
            let key = (a.state, a.supervisor_pid);
            if self.states.get(&a.name) == Some(&key) {
                continue;
            }
            self.states.insert(a.name.clone(), key);
            let mut t = format!("{now} {} {} (supervised by {}", a.name, state_name(a.state), a.supervised_by);
            if let Some(p) = a.supervisor_pid {
                t += &format!(", pid {p}");
            }
            if a.supervisor_restarts > 0 {
                t += &format!(", {} restarts", a.supervisor_restarts);
            }
            t += ")";
            if let Some(p) = &a.problem {
                t += &format!(": {p}");
            }
            if !self.print(&t) {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_wire_format() {
        for s in [
            AppState::Running,
            AppState::Stopped,
            AppState::Starting,
            AppState::Unreachable,
            AppState::GaveUp,
            AppState::NotStarted,
        ] {
            assert_eq!(serde_json::to_value(s).unwrap(), state_name(s));
        }
        for e in [WorkerEvent::Crashed, WorkerEvent::Ready, WorkerEvent::Stopped, WorkerEvent::Failed] {
            assert_eq!(serde_json::to_value(e).unwrap(), worker_event_name(e));
        }
        assert_eq!(serde_json::to_value(SupervisorEvent::Died).unwrap(), supervisor_event_name(SupervisorEvent::Died));
    }

    /// `warden events` names hot standbys `s1`, `s2`… (and the pool
    /// `standby`), as `warden status` and the log lines do, not worker 0;
    /// an older supervisor's standby event (no `standby`) still prints.
    #[test]
    fn standby_events_print_as_s1() {
        let line = |json: &str| {
            let ev: Event = serde_json::from_str(json).unwrap();
            let t = worker_line(&ev).unwrap();
            t.split_once(' ').map(|(_, rest)| rest.to_string()).unwrap_or(t) // without the clock
        };
        assert_eq!(
            line(
                r#"{"type":"worker","app":"api","worker":0,"standby":1,"event":"ready","pid":7,"detail":"startup_ms=80 role=standby","at_ms":1}"#
            ),
            "api worker s1 ready pid=7 startup_ms=80 role=standby"
        );
        assert_eq!(
            line(
                r#"{"type":"worker","app":"api","worker":0,"standby":2,"event":"crashed","pid":8,"detail":"exit code 4 (standby)","at_ms":1}"#
            ),
            "api worker s2 crashed pid=8 exit code 4 (standby)"
        );
        assert_eq!(
            line(
                r#"{"type":"worker","app":"api","worker":0,"standby":0,"event":"restarting","detail":"in_ms=200 role=standby","at_ms":1}"#
            ),
            "api worker standby restarting in_ms=200 role=standby"
        );
        assert_eq!(
            line(
                r#"{"type":"worker","app":"api","worker":2,"event":"starting","pid":7,"detail":"promoted from standby s1","at_ms":1}"#
            ),
            "api worker 2 starting pid=7 promoted from standby s1"
        );
        assert_eq!(
            line(r#"{"type":"worker","app":"api","worker":0,"event":"stopping","pid":9,"at_ms":1}"#),
            "api worker 0 stopping pid=9",
            "an older supervisor"
        );
        assert_eq!(worker_line(&Event::Lagged { app: None, dropped: 1 }), None);
    }
}

//! wardend (`warden daemon`): the optional daemon (docs/protocol.md,
//! "Daemon socket" and "Second-level supervision"). It
//!
//! - serves one socket (`<runtime dir>/wardend.sock`) where the CLI and the
//!   GUI get every app and live events, pushed as they happen;
//! - watches every supervisor it finds and restarts the ones `warden start`
//!   (or wardend) launched in the background when they die, with backoff;
//! - is never on the request path: apps never depend on it, stopping or
//!   killing it stops no app, and it sets no parent-death signal on anything.
//!
//! One thread, one `LocalSet`. Events go out through a bounded broadcast of
//! ready-made JSON lines (`Frame`): a supervisor's line is forwarded as it
//! was sent, wardend's own events are serialized once, and every client
//! writes the same bytes. A slow client lags or is dropped, never waited for.

pub mod client;
mod host;
mod policy;
mod watcher;

use crate::config;
use crate::control::{self, Request, Status};
use crate::events::{self, AppEntry, AppState, DaemonReply, DaemonRequest, Event, SupervisorEvent};
use crate::fleet;
use policy::{Phase, Policy, Verdict};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use watcher::WatchMsg;

/// wardend's socket, in the runtime directory next to the apps' directories.
pub fn socket_path() -> PathBuf {
    warden_protocol::paths::wardend_socket(&config::runtime_dir())
}

/// Where `warden daemon --background` logs.
pub fn log_path() -> PathBuf {
    fleet::state_dir().join("logs").join("wardend.log")
}

/// How often the config and runtime directories are scanned for apps.
const DISCOVER_EVERY: Duration = Duration::from_secs(2);
/// Each supervisor's status cadence (`subscribe` interval, or polling).
const WATCH_INTERVAL: Duration = Duration::from_secs(1);
/// Events a client may fall behind by before it gets `lagged`.
const BUS_CAPACITY: usize = 1024;
/// Longest request line.
const MAX_REQUEST: u64 = 64 * 1024;
/// Forwarded requests other than `logs` / `subscribe`.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);
/// At most one `unresponsive` warning per app this often.
const UNRESPONSIVE_LOG_EVERY: Duration = Duration::from_secs(60);

/// One event line, ready to write.
#[derive(Clone, Debug)]
pub(crate) struct Frame {
    /// The app it is about, for `subscribe`'s `apps` filter.
    pub app: Option<Arc<str>>,
    pub kind: Kind,
    /// JSON, with its trailing newline.
    pub line: Arc<str>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Status,
    Log,
    Host,
    Bye,
    Other,
}

pub(crate) type Bus = broadcast::Sender<Frame>;

/// Serialize `ev` onto `bus`, only when someone listens.
pub(crate) fn emit(bus: &Bus, app: Option<&Arc<str>>, kind: Kind, ev: &Event) {
    if bus.receiver_count() == 0 {
        return;
    }
    if let Ok(mut s) = serde_json::to_string(ev) {
        s.push('\n');
        let _ = bus.send(Frame { app: app.cloned(), kind, line: s.into() });
    }
}

/// A `supervisor` event.
fn sup_event(bus: &Bus, app: &Arc<str>, event: SupervisorEvent, pid: Option<u32>, detail: Option<String>) {
    if bus.receiver_count() == 0 {
        return;
    }
    let ev = Event::Supervisor { app: app.to_string(), event, pid, detail, at_ms: events::now_ms() };
    emit(bus, Some(app), Kind::Other, &ev);
}

// -------------------------------------------------------------- the apps

/// What wardend knows about one app.
struct Rec {
    name: Arc<str>,
    namespace: String,
    config: Option<PathBuf>,
    socket: PathBuf,
    /// Its config does not load (from discovery).
    problem: Option<String>,
    /// Found by the last discovery.
    seen: bool,
    phase: Phase,
    /// Tags the current watcher; messages from older ones are ignored.
    epoch: u64,
    /// Tags the pending restart timer (separate: an attempt to attach to a
    /// stale socket during the backoff must not cancel the restart).
    timer_epoch: u64,
    /// A watcher is running (attaching or attached).
    watcher: Option<tokio::task::AbortHandle>,
    pid: Option<u32>,
    status: Option<Box<Status>>,
    /// `Status.launched`, or `background` when wardend started it.
    launched: Option<String>,
    /// The environment and working directory of a background supervisor,
    /// to restart it the way it was started.
    origin: Option<fleet::Origin>,
    up_since: Option<Instant>,
    /// Restarts by wardend.
    restarts: u32,
    backoff: policy::Backoff,
    unresponsive: bool,
    unresponsive_logged: Option<Instant>,
    /// Why it is not running, or what wardend is about to do.
    note: Option<String>,
}

impl Rec {
    fn new(app: &fleet::App) -> Rec {
        Rec {
            name: app.name.as_str().into(),
            namespace: app.namespace.clone(),
            config: app.config.clone(),
            socket: app.socket.clone(),
            problem: app.problem.clone(),
            seen: true,
            phase: Phase::Idle,
            epoch: 0,
            timer_epoch: 0,
            watcher: None,
            pid: None,
            status: None,
            launched: None,
            origin: None,
            up_since: None,
            restarts: 0,
            backoff: policy::Backoff::default(),
            unresponsive: false,
            unresponsive_logged: None,
            note: None,
        }
    }

    fn state(&self) -> AppState {
        policy::state(self.phase, self.status.as_deref(), self.unresponsive)
    }

    fn running(&self) -> bool {
        matches!(self.phase, Phase::Watching | Phase::Spawned)
    }

    fn entry(&self) -> AppEntry {
        let state = self.state();
        let name = &self.name;
        let problem = match state {
            AppState::Running => self
                .problem
                .as_ref()
                .map(|p| format!("config problem (the running app keeps the config it loaded): {p}")),
            AppState::Stopped if self.phase == Phase::Exited => {
                Some(format!("stopped on request; `warden start {name}` starts it"))
            }
            AppState::Stopped if self.status.as_ref().is_some_and(|s| s.shutting_down) => {
                Some("shutting down on request".into())
            }
            AppState::Stopped => Some(format!("workers stopped; `warden start {name}` starts them")),
            AppState::Unreachable => Some(match self.pid {
                Some(pid) => format!(
                    "supervisor pid {pid} does not answer on {}; `cat /proc/{pid}/stack` or `gdb -p {pid}` shows \
                     where it is stuck (wardend never kills it: its workers are probably still serving)",
                    self.socket.display()
                ),
                None => format!("no answer on {}", self.socket.display()),
            }),
            AppState::NotStarted => match &self.problem {
                Some(p) => Some(format!("config problem: {p}")),
                None => self.note.clone().or_else(|| Some(format!("not running; `warden start {name}` starts it"))),
            },
            AppState::Starting | AppState::GaveUp => self.note.clone(),
        };
        AppEntry {
            name: name.to_string(),
            namespace: self.namespace.clone(),
            config: self.config.as_ref().map(|p| p.display().to_string()),
            socket: self.socket.display().to_string(),
            state,
            supervised_by: policy::supervised_by(self.launched.as_deref()).into(),
            supervisor_pid: if self.running() { self.pid } else { None },
            supervisor_restarts: self.restarts,
            status: if self.phase == Phase::Watching { self.status.clone() } else { None },
            problem,
        }
    }
}

struct Core {
    apps: BTreeMap<Arc<str>, Rec>,
    policy: Policy,
    next_epoch: u64,
    tx: mpsc::Sender<WatchMsg>,
    bus: Bus,
    logs_bus: Bus,
    logs: watch::Receiver<bool>,
    /// Supervisors wardend started: its children, reaped here.
    children: Vec<std::process::Child>,
    /// Hash of every app's (name, state, pid, restarts): an `apps` event
    /// goes out when it changes.
    last_apps: u64,
}

impl Core {
    fn epoch(&mut self) -> u64 {
        self.next_epoch += 1;
        self.next_epoch
    }

    fn entries(&self) -> Vec<AppEntry> {
        self.apps.values().map(Rec::entry).collect()
    }

    /// Emit `apps` if the set of apps or any app's state changed.
    fn apps_changed(&mut self) {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        for r in self.apps.values() {
            (&*r.name, r.state() as u8, r.running().then_some(r.pid), r.restarts).hash(&mut h);
        }
        let sig = h.finish();
        if sig != self.last_apps {
            self.last_apps = sig;
            if self.bus.receiver_count() > 0 {
                emit(&self.bus, None, Kind::Other, &Event::Apps { apps: self.entries() });
            }
        }
    }

    /// Merge what discovery found; start watching running supervisors.
    fn merge(&mut self, found: Vec<fleet::App>) {
        for r in self.apps.values_mut() {
            r.seen = false;
        }
        for app in found {
            let r = self.apps.entry(app.name.as_str().into()).or_insert_with(|| Rec::new(&app));
            r.namespace = app.namespace;
            r.config = app.config;
            r.socket = app.socket;
            r.problem = app.problem;
            r.seen = true;
        }
        // Gone from both directories and not running: forget it.
        self.apps.retain(|_, r| {
            let keep = r.seen || r.running() || r.phase == Phase::Backoff || r.watcher.is_some();
            if !keep {
                if let Some(w) = r.watcher.take() {
                    w.abort();
                }
            }
            keep
        });
        // A socket nobody watches: someone started it (or a stale file).
        let attach: Vec<Arc<str>> = self
            .apps
            .values()
            .filter(|r| r.watcher.is_none() && r.phase != Phase::Spawned && r.socket.exists())
            .map(|r| r.name.clone())
            .collect();
        for name in attach {
            self.watch(&name, None);
        }
    }

    /// (Re)start the watcher of `name`; `pid` when wardend started it.
    fn watch(&mut self, name: &Arc<str>, pid: Option<u32>) {
        let epoch = self.epoch();
        let (tx, bus, logs_bus, logs) = (self.tx.clone(), self.bus.clone(), self.logs_bus.clone(), self.logs.clone());
        let Some(r) = self.apps.get_mut(name) else { return };
        if let Some(w) = r.watcher.take() {
            w.abort();
        }
        r.epoch = epoch;
        let spec =
            watcher::Spec { app: r.name.clone(), socket: r.socket.clone(), epoch, pid, interval: WATCH_INTERVAL };
        let app = r.name.clone();
        let task = tokio::task::spawn_local(async move {
            let on_panic = tx.clone();
            if let Err(panic) = crate::guard::catch_unwind(watcher::run(spec, tx, bus, logs_bus, logs)).await {
                let _ = on_panic.send(WatchMsg::Ended { app, epoch, panic }).await;
            }
        });
        r.watcher = Some(task.abort_handle());
    }

    fn reap(&mut self) {
        self.children.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    }

    /// Reap `pid` if it is our child; how it ended.
    fn reap_pid(&mut self, pid: u32) -> Option<String> {
        use std::os::unix::process::ExitStatusExt;
        let mut how = None;
        self.children.retain_mut(|c| {
            if c.id() != pid {
                return true;
            }
            let Ok(Some(st)) = c.try_wait() else { return true };
            how = Some(match (st.code(), st.signal()) {
                (Some(code), _) => format!("exit code {code}"),
                (None, Some(s)) => format!("signal {}", crate::signals::name(s)),
                _ => st.to_string(),
            });
            false
        });
        how
    }

    fn on_watch(&mut self, m: WatchMsg) {
        let (app, epoch) = m.key();
        let app = app.clone();
        let bus = &self.bus;
        let Some(r) = self.apps.get_mut(&app) else { return };
        let current = match m {
            WatchMsg::RestartDue { .. } => r.timer_epoch == epoch,
            _ => r.epoch == epoch,
        };
        if !current {
            return; // a replaced watcher, or a cancelled restart
        }
        match m {
            WatchMsg::Attached { pid, streaming, status, .. } => {
                let was = r.phase;
                r.phase = Phase::Watching;
                r.timer_epoch = 0; // a restart that was pending is moot
                r.pid = Some(pid);
                if !status.launched.is_empty() {
                    r.launched = Some(status.launched.clone());
                }
                if r.launched.as_deref() == Some("background") {
                    if let Some(o) = fleet::Origin::of(pid) {
                        r.origin = Some(o);
                    }
                }
                if was != Phase::Spawned {
                    r.up_since = Instant::now().checked_sub(Duration::from_secs(status.uptime_secs));
                    // Started by someone else (`warden start`, systemd): a fresh history.
                    r.backoff.clear();
                }
                r.status = Some(status);
                r.unresponsive = false;
                r.note = None;
                let how = if streaming { "event stream" } else { "status polling (an older supervisor)" };
                let launched = r.launched.clone().unwrap_or_else(|| "unknown".into());
                if was == Phase::Spawned {
                    crate::debug!("supervisor answers", app = app, pid = pid, watched_by = how);
                } else {
                    crate::info!(
                        "watching a running supervisor",
                        app = app,
                        pid = pid,
                        launched = launched,
                        watched_by = how
                    );
                    let detail = format!("launched: {launched}; watched by {how}");
                    sup_event(bus, &app, SupervisorEvent::Found, Some(pid), Some(detail));
                }
            }
            WatchMsg::Status { status, .. } => {
                let before = r.state();
                r.pid = Some(status.pid);
                if !status.launched.is_empty() && r.launched.as_deref() != Some(status.launched.as_str()) {
                    r.launched = Some(status.launched.clone());
                }
                r.status = Some(status);
                if r.state() == before {
                    return; // the common case: nothing for `apps`
                }
            }
            WatchMsg::Gone { pid, on_purpose, detail, .. } => self.on_gone(&app, pid, on_purpose, detail),
            WatchMsg::Lost { detail, .. } => {
                r.watcher = None;
                crate::debug!("nothing answers on the socket", app = app, socket = r.socket.display(), error = detail);
            }
            WatchMsg::Unresponsive { pid, detail, .. } => {
                r.unresponsive = true;
                let pid = pid.or(r.pid);
                let shown = pid.map(|p| p.to_string()).unwrap_or_else(|| "?".into());
                if r.unresponsive_logged.is_none_or(|t| t.elapsed() >= UNRESPONSIVE_LOG_EVERY) {
                    r.unresponsive_logged = Some(Instant::now());
                    crate::warn!(
                        "supervisor is unresponsive; not killing it (its workers are probably still serving)",
                        app = app,
                        pid = shown,
                        error = detail,
                        hint = format!(
                            "see where it is stuck with `cat /proc/{shown}/stack` or `gdb -p {shown}`; restart it \
                             yourself if it stays stuck"
                        ),
                    );
                }
                sup_event(bus, &app, SupervisorEvent::Unresponsive, pid, Some(detail));
            }
            WatchMsg::Responsive { .. } => {
                r.unresponsive = false;
                crate::info!("supervisor answers again", app = app);
                sup_event(bus, &app, SupervisorEvent::Responsive, r.pid, None);
            }
            WatchMsg::RestartDue { .. } => self.restart_due(&app),
            WatchMsg::Ended { panic, .. } => {
                r.watcher = None;
                if r.phase == Phase::Spawned {
                    // Discovery attaches to it again once its socket answers.
                    r.phase = Phase::Idle;
                }
                crate::error!(
                    "watching a supervisor failed; wardend will watch it again",
                    app = app,
                    panic = panic,
                    hint = "this is a wardend bug: please report it with the log lines above",
                );
            }
        }
        self.apps_changed();
    }

    fn on_gone(&mut self, app: &Arc<str>, pid: u32, on_purpose: bool, detail: String) {
        let how = self.reap_pid(pid);
        let timer_epoch = self.epoch();
        let now = Instant::now();
        let policy = self.policy;
        let (bus, tx) = (&self.bus, &self.tx);
        let Some(r) = self.apps.get_mut(app) else { return };
        r.watcher = None;
        r.unresponsive = false;
        let detail = match how {
            Some(h) => format!("{detail}; {h}"),
            None => detail,
        };
        if on_purpose {
            r.phase = Phase::Exited;
            r.note = None;
            crate::info!("supervisor exited on request", app = app, pid = pid);
            sup_event(bus, app, SupervisorEvent::Exited, Some(pid), Some(detail));
            return;
        }
        let uptime = r.up_since.map(|t| now.saturating_duration_since(t));
        sup_event(bus, app, SupervisorEvent::Died, Some(pid), Some(detail.clone()));
        let launched = r.launched.clone().unwrap_or_default();
        match launched.as_str() {
            "background" => match r.backoff.on_death(&policy, now, uptime) {
                Verdict::Restart(delay) => {
                    let delay_ms = delay.as_millis() as u64;
                    r.phase = Phase::Backoff;
                    r.timer_epoch = timer_epoch;
                    r.note = Some(format!("died ({detail}); wardend restarts it in {} s", delay_ms.div_ceil(1000)));
                    crate::warn!(
                        "supervisor died; restarting it",
                        app = app,
                        pid = pid,
                        delay_ms = delay_ms,
                        error = detail,
                        hint = format!(
                            "its log shows why: {} (or `warden logs {app} --history`)",
                            fleet::log_path(app).display()
                        ),
                    );
                    let (tx, name) = (tx.clone(), app.clone());
                    tokio::task::spawn_local(async move {
                        tokio::time::sleep(delay).await;
                        let _ = tx.send(WatchMsg::RestartDue { app: name, epoch: timer_epoch }).await;
                    });
                    sup_event(bus, app, SupervisorEvent::Restarting, Some(pid), Some(format!("after {delay_ms} ms")));
                }
                Verdict::GiveUp { deaths } => {
                    r.phase = Phase::GaveUp;
                    let log = fleet::log_path(app);
                    let window = policy.give_up_window.as_secs();
                    r.note = Some(format!(
                        "died {deaths} times in {window} s; wardend stopped restarting it. Fix the error in {} \
                         (`warden logs {app} --history`), then run `warden start {app}`",
                        log.display()
                    ));
                    let last = fleet::tail(&log, 20).join("\n");
                    crate::error!(
                        "supervisor keeps dying; wardend gave up restarting it",
                        app = app,
                        pid = pid,
                        deaths = deaths,
                        window_s = window,
                        error = detail,
                        log = log.display(),
                        last_lines = last,
                        hint = format!("fix the error shown in its last log lines, then run `warden start {app}`"),
                    );
                    let detail = format!("{deaths} deaths in {window} s");
                    sup_event(bus, app, SupervisorEvent::GaveUp, Some(pid), Some(detail));
                }
            },
            "systemd" => {
                r.phase = Phase::Idle;
                r.note = Some(format!("its supervisor died ({detail}); systemd restarts it"));
                crate::warn!(
                    "supervisor died; systemd restarts it, not wardend",
                    app = app,
                    pid = pid,
                    error = detail,
                    hint = format!("`journalctl -u warden@{app} -n 50` shows why"),
                );
            }
            other => {
                let launched = if other.is_empty() { "unknown" } else { other };
                r.phase = Phase::Idle;
                r.note = Some(format!(
                    "its supervisor died ({detail}); it was not started in the background, so wardend does not \
                     restart it: `warden start {app}` does"
                ));
                crate::warn!(
                    "supervisor died; not restarting it (it was not started in the background)",
                    app = app,
                    pid = pid,
                    launched = launched,
                    error = detail,
                    hint = format!("start it with `warden start {app}` so wardend restarts it next time"),
                );
            }
        }
    }

    fn restart_due(&mut self, app: &Arc<str>) {
        let Some(r) = self.apps.get_mut(app) else { return };
        if r.phase != Phase::Backoff {
            return;
        }
        r.timer_epoch = 0;
        let cfg = r.config.clone().or_else(|| r.status.as_ref().and_then(|s| s.config_path.clone()).map(PathBuf::from));
        let Some(cfg) = cfg.filter(|c| c.is_file()) else {
            r.phase = Phase::Idle;
            r.note = Some("its config file is gone, so wardend cannot restart it".into());
            crate::error!(
                "cannot restart a dead supervisor: its config file is gone",
                app = app,
                hint =
                    format!("put the config back in {}, then run `warden start {app}`", fleet::config_dir().display()),
            );
            return;
        };
        if let Err(e) = self.spawn(app, &cfg, true) {
            if let Some(r) = self.apps.get_mut(app) {
                r.phase = Phase::Idle;
                r.note = Some(format!("wardend could not restart it: {e}"));
            }
        }
    }

    /// Start `app`'s supervisor in the background and watch it.
    fn spawn(&mut self, app: &Arc<str>, cfg: &std::path::Path, restart: bool) -> Result<u32, String> {
        // As it was started (its PATH, variables and directory), else as wardend.
        let origin = self.apps.get(app).and_then(|r| r.origin.as_ref());
        let child = match fleet::spawn_background_as(app, cfg, origin) {
            Ok(c) => c,
            Err(e) => {
                crate::error!(
                    "could not start a supervisor",
                    app = app,
                    config = cfg.display(),
                    error = e,
                    hint = "is the warden binary still in place? fix the error, then run `warden start <app>`",
                );
                return Err(e);
            }
        };
        let pid = child.id();
        self.children.push(child);
        let log = fleet::log_path(app);
        let Some(r) = self.apps.get_mut(app) else { return Ok(pid) };
        r.phase = Phase::Spawned;
        r.pid = Some(pid);
        r.status = None;
        r.launched = Some("background".into());
        r.up_since = Some(Instant::now());
        r.unresponsive = false;
        r.note = Some(format!("starting (pid {pid}), log {}", log.display()));
        if restart {
            r.restarts += 1;
        }
        crate::info!("supervisor started", app = app, pid = pid, log = log.display(), restart = restart);
        sup_event(&self.bus, app, SupervisorEvent::Started, Some(pid), Some(format!("log {}", log.display())));
        self.watch(app, Some(pid));
        Ok(pid)
    }

    /// The `start` request. `answering`: its socket answered `status` just now.
    fn start(&mut self, name: &str, answering: Option<u32>) -> DaemonReply {
        let Some(r) = self.apps.get_mut(name) else {
            return reply_err(format!(
                "no app named {name:?}; configs are read from {}",
                fleet::config_dir().display()
            ));
        };
        let app = r.name.clone();
        match (r.phase, answering) {
            (_, Some(pid)) => {
                if r.watcher.is_none() {
                    self.watch(&app, None);
                }
                return reply_ok(format!("already running (pid {pid})"));
            }
            (Phase::Spawned, None) => {
                return reply_ok(format!("already starting (pid {})", r.pid.unwrap_or(0)));
            }
            _ => {}
        }
        if let Some(p) = &r.problem {
            return reply_err(format!("its config does not load: {p}"));
        }
        let Some(cfg) = r.config.clone() else {
            return reply_err("no config file is known for it".to_string());
        };
        r.backoff.clear();
        let msg = match self.spawn(&app, &cfg, false) {
            Ok(pid) => {
                reply_ok(format!("started in the background (pid {pid}), log {}", fleet::log_path(&app).display()))
            }
            Err(e) => reply_err(e),
        };
        self.apps_changed();
        msg
    }
}

fn reply_ok(msg: String) -> DaemonReply {
    DaemonReply { ok: true, message: Some(msg), ..Default::default() }
}

fn reply_err(msg: String) -> DaemonReply {
    DaemonReply { ok: false, message: Some(msg), ..Default::default() }
}

// ------------------------------------------------------------ the daemon

struct Daemon {
    core: RefCell<Core>,
    bus: Bus,
    logs_bus: Bus,
    logs_wanted: watch::Sender<bool>,
    log_clients: Cell<usize>,
    shutdown: Notify,
}

/// Counts a client that wants log lines; supervisors send them only while
/// at least one does.
struct WantsLogs(Rc<Daemon>);

impl WantsLogs {
    fn new(d: &Rc<Daemon>) -> WantsLogs {
        let n = d.log_clients.get() + 1;
        d.log_clients.set(n);
        if n == 1 {
            d.logs_wanted.send_replace(true);
        }
        WantsLogs(d.clone())
    }
}

impl Drop for WantsLogs {
    fn drop(&mut self) {
        let n = self.0.log_clients.get().saturating_sub(1);
        self.0.log_clients.set(n);
        if n == 0 {
            self.0.logs_wanted.send_replace(false);
        }
    }
}

impl Daemon {
    fn discover(&self) {
        let found = fleet::discover();
        let mut core = self.core.borrow_mut();
        core.merge(found);
        core.reap();
        core.apps_changed();
    }

    async fn start(&self, name: &str) -> DaemonReply {
        let found = fleet::discover();
        let app = found.iter().find(|a| a.name == name).cloned();
        let socket = {
            let mut core = self.core.borrow_mut();
            core.merge(found);
            core.apps.get(name).map(|r| r.socket.clone())
        };
        let answering = match socket {
            Some(s) if s.exists() => watcher::poll_status(s).await.ok().map(|st| st.pid),
            _ => None,
        };
        // An app with a systemd unit is started by systemd, as `warden start` does.
        if answering.is_none() {
            if let Some((scope, unit)) = app.as_ref().and_then(fleet::systemd_unit_for) {
                return start_unit(scope, &unit).await;
            }
        }
        self.core.borrow_mut().start(name, answering)
    }

    /// `--resurrect`: start every app `warden save` recorded that is not
    /// running, in the background, watched like any other. Once per boot:
    /// when launchd (KeepAlive) restarts a crashed wardend, apps the user
    /// stopped since boot stay stopped.
    async fn resurrect(&self) {
        let marker = config::runtime_dir().join("resurrected");
        let boot = boot_id();
        if already_resurrected(boot.as_deref(), std::fs::read_to_string(&marker).ok().as_deref()) {
            crate::info!(
                "saved apps were already resurrected this boot; not starting them again",
                marker = marker.display(),
                hint = "wardend restarted after a crash; apps stopped since boot stay stopped. `warden resurrect` starts the saved apps now",
            );
            return;
        }
        self.resurrect_saved().await;
        if let Some(b) = boot {
            if let Err(e) = std::fs::write(&marker, b) {
                crate::warn!(
                    "cannot record that the saved apps were resurrected",
                    marker = marker.display(),
                    error = e,
                    hint = "if wardend restarts before the next boot it resurrects them again; check the runtime directory's permissions",
                );
            }
        }
    }

    async fn resurrect_saved(&self) {
        let saved = match fleet::saved_apps() {
            Ok(Some(s)) if !s.is_empty() => s,
            Ok(_) => {
                crate::info!(
                    "nothing to resurrect: no app is saved",
                    hint = "`warden save` records the running apps; wardend --resurrect starts them when it starts",
                );
                return;
            }
            Err(e) => {
                crate::error!(
                    "cannot read the saved apps; resurrecting none",
                    error = e,
                    hint = "run `warden save` again to rewrite it (`warden start <app>` starts one meanwhile)",
                );
                return;
            }
        };
        for s in saved {
            if !s.config.is_file() {
                crate::warn!(
                    "a saved app's config is gone; not starting it",
                    app = s.name,
                    config = s.config.display(),
                    hint = "`warden save` again to forget it",
                );
                continue;
            }
            let app = fleet::app_from_config(&s.config);
            if let Some(p) = &app.problem {
                crate::error!(
                    "a saved app's config does not load; not starting it",
                    app = app.name,
                    error = p,
                    hint = format!("fix {}, then `warden start {}`", s.config.display(), app.name),
                );
                continue;
            }
            if app.socket.exists() && watcher::poll_status(app.socket.clone()).await.is_ok() {
                crate::debug!("saved app already running", app = app.name);
                continue;
            }
            let mut core = self.core.borrow_mut();
            let name: Arc<str> = app.name.as_str().into();
            core.apps.entry(name.clone()).or_insert_with(|| Rec::new(&app));
            if let Ok(pid) = core.spawn(&name, &s.config, false) {
                crate::info!("resurrected a saved app", app = name, pid = pid);
            }
            core.apps_changed();
        }
    }
}

/// Identifies this boot: the kernel's boot id on Linux, the boot time
/// elsewhere. None if unknown (then every start resurrects, as before).
fn boot_id() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_string())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("/usr/sbin/sysctl").args(["-n", "kern.boottime"]).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

fn already_resurrected(boot: Option<&str>, marker: Option<&str>) -> bool {
    matches!((boot, marker), (Some(b), Some(m)) if !b.is_empty() && m.trim() == b)
}

/// `systemctl [--user] start --no-block <unit>`: queued, not waited for (a
/// Type=notify start can take a minute; wardend's one thread must not wait).
async fn start_unit(scope: fleet::Scope, unit: &str) -> DaemonReply {
    let Some(bin) = fleet::systemctl_bin() else { return reply_err("systemd is not running on this host".into()) };
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(scope.flag()).args(["start", "--no-block", unit]).stdin(std::process::Stdio::null());
    match tokio::time::timeout(Duration::from_secs(10), cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => reply_ok(format!(
            "starting {unit} (systemd runs it; `systemctl {}status {unit}` shows how it goes)",
            scope.shown()
        )),
        Ok(Ok(out)) => {
            let err = String::from_utf8_lossy(&out.stderr);
            let why = err.trim().lines().last().unwrap_or("no output").to_string();
            reply_err(format!("`systemctl {}start {unit}` failed: {why}", scope.shown()))
        }
        Ok(Err(e)) => reply_err(format!("running systemctl: {e}")),
        Err(_) => reply_err(format!("`systemctl {}start {unit}` did not return within 10 s", scope.shown())),
    }
}

/// `warden daemon`: run wardend in the foreground until SIGTERM, SIGINT or
/// a `shutdown` request. `resurrect`: first start the saved apps that are
/// not running (launchd, containers; never under systemd, where each app
/// has its own unit).
pub fn main(rt: &tokio::runtime::Runtime, resurrect: bool) -> i32 {
    crate::logging::init(config::Level::Info, None, crate::logging::Files::default());
    crate::guard::install_panic_hook();
    let local = tokio::task::LocalSet::new();
    let run = std::panic::AssertUnwindSafe(|| rt.block_on(local.run_until(run(resurrect))));
    let code = match std::panic::catch_unwind(run) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            crate::error!(
                "wardend could not start",
                error = e,
                hint = "fix the problem named in `error`; apps are not affected (they never depend on wardend)",
            );
            1
        }
        Err(_) => {
            crate::error!(
                "wardend panicked; exiting (every app keeps running)",
                hint = "this is a wardend bug: please report it with the log lines above",
            );
            101
        }
    };
    crate::logging::flush(Duration::from_secs(1));
    code
}

async fn run(resurrect: bool) -> Result<(), String> {
    use tokio::signal::unix::{SignalKind, signal};
    let path = socket_path();
    if let Some(pid) = client::hello_pid(&path).await {
        return Err(format!(
            "wardend is already running (pid {pid}) on {}; `warden daemon status` shows it",
            path.display()
        ));
    }
    let listener = control::bind(&path).await?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| format!("installing signal handlers: {e}"))?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| format!("installing signal handlers: {e}"))?;

    let (tx, mut rx) = mpsc::channel(256);
    let bus = broadcast::channel(BUS_CAPACITY).0;
    let logs_bus = broadcast::channel(BUS_CAPACITY).0;
    let (logs_wanted, logs) = watch::channel(false);
    let core = Core {
        apps: BTreeMap::new(),
        policy: policy::from_env(),
        next_epoch: 0,
        tx,
        bus: bus.clone(),
        logs_bus: logs_bus.clone(),
        logs,
        children: Vec::new(),
        last_apps: 0,
    };
    let d = Rc::new(Daemon {
        core: RefCell::new(core),
        bus,
        logs_bus,
        logs_wanted,
        log_clients: Cell::new(0),
        shutdown: Notify::new(),
    });
    crate::info!(
        "wardend started",
        pid = std::process::id(),
        socket = path.display(),
        configs = fleet::config_dir().display(),
        hint = "`warden events` shows live events; stopping wardend stops no app",
    );
    crate::systemd::notify("READY=1\nSTATUS=watching apps");
    let watchdog = crate::systemd::watchdog_requested();
    crate::guard::spawn_essential("wardend socket", serve(d.clone(), listener));
    crate::guard::spawn_essential("wardend host metrics", host_loop(d.clone()));
    if resurrect {
        d.discover();
        d.resurrect().await;
    }

    let mut discover = tokio::time::interval(DISCOVER_EVERY);
    discover.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let reason = loop {
        tokio::select! {
            Some(m) = rx.recv() => d.core.borrow_mut().on_watch(m),
            _ = discover.tick() => {
                d.discover();
                if watchdog {
                    crate::systemd::notify("WATCHDOG=1");
                }
            }
            _ = term.recv() => break "SIGTERM",
            _ = int.recv() => break "SIGINT",
            _ = d.shutdown.notified() => break "shutdown request",
        }
    };
    crate::info!("wardend exiting; every app keeps running", reason = reason);
    crate::systemd::notify("STOPPING=1");
    emit(&d.bus, None, Kind::Bye, &Event::Bye { app: None, reason: reason.into() });
    // Let clients write the `bye` (each write is bounded by its own timeout).
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(&path);
    Ok(())
}

async fn host_loop(d: Rc<Daemon>) {
    let mut sampler = host::Sampler::default();
    let mut tick = tokio::time::interval(WATCH_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if d.bus.receiver_count() == 0 {
            sampler.reset();
            continue;
        }
        if let Some(ev) = sampler.sample() {
            emit(&d.bus, None, Kind::Host, &ev);
        }
    }
}

// ---------------------------------------------------------------- clients

async fn serve(d: Rc<Daemon>, listener: UnixListener) {
    let slots = Arc::new(tokio::sync::Semaphore::new(control::MAX_CONNECTIONS));
    let streams = Arc::new(tokio::sync::Semaphore::new(control::MAX_STREAMS));
    let mut last_refusal_log: Option<Instant> = None;
    let mut last_accept_log: Option<Instant> = None;
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                if last_accept_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                    crate::error!(
                        "wardend's socket cannot accept connections; retrying",
                        error = e,
                        hint = "wardend may be out of file descriptors: check LimitNOFILE and `ls /proc/<wardend pid>/fd | wc -l`",
                    );
                    last_accept_log = Some(Instant::now());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            if last_refusal_log.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
                crate::warn!(
                    "too many connections to wardend; refusing new ones",
                    limit = control::MAX_CONNECTIONS,
                    hint = "something opens wardend's socket in a loop or leaves `warden events` sessions open",
                );
                last_refusal_log = Some(Instant::now());
            }
            crate::guard::spawn_request("wardend refusal", async move {
                let (_, mut w) = stream.into_split();
                let msg = format!("too many connections to wardend (limit {}); try again", control::MAX_CONNECTIONS);
                let _ = reply(&mut w, &reply_err(msg)).await;
            });
            continue;
        };
        let d = d.clone();
        let slot = control::Slot::new(permit, streams.clone());
        crate::guard::spawn_request("wardend request", async move {
            if let Err(e) = handle(d, stream, slot).await {
                crate::debug!("wardend connection ended with an error", error = e);
            }
        });
    }
}

/// Write `bytes`, giving up (and dropping the client) after 5 s.
async fn send(w: &mut OwnedWriteHalf, bytes: &[u8]) -> std::io::Result<()> {
    match tokio::time::timeout(control::REQUEST_TIMEOUT, w.write_all(bytes)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "client did not read for 5 s")),
    }
}

async fn reply(w: &mut OwnedWriteHalf, r: &DaemonReply) -> std::io::Result<()> {
    let mut s = serde_json::to_string(r).unwrap_or_else(|_| "{\"ok\":false}".into());
    s.push('\n');
    send(w, s.as_bytes()).await
}

async fn handle(d: Rc<Daemon>, stream: UnixStream, mut slot: control::Slot) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    match tokio::time::timeout(control::REQUEST_TIMEOUT, (&mut reader).take(MAX_REQUEST).read_line(&mut line)).await {
        Ok(res) => {
            res?;
        }
        Err(_) => {
            let msg = format!("no request received within {} s", control::REQUEST_TIMEOUT.as_secs());
            return reply(&mut w, &reply_err(msg)).await;
        }
    }
    let req: DaemonRequest = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("bad request: {e} (is the `warden` CLI the same version as wardend?)");
            return reply(&mut w, &reply_err(msg)).await;
        }
    };
    match req {
        DaemonRequest::Hello => {
            let hello = hello_event();
            reply(&mut w, &DaemonReply { ok: true, hello: Some(hello), ..Default::default() }).await
        }
        DaemonRequest::Apps => {
            let apps = d.core.borrow().entries();
            reply(&mut w, &DaemonReply { ok: true, apps: Some(apps), ..Default::default() }).await
        }
        DaemonRequest::Subscribe { interval_ms, logs, apps } => {
            if let Err(msg) = slot.become_stream("subscribe") {
                return reply(&mut w, &reply_err(msg)).await;
            }
            subscribe(d, reader, w, events::interval(interval_ms), logs, apps).await
        }
        DaemonRequest::Start { app } => {
            let r = d.start(&app).await;
            reply(&mut w, &r).await
        }
        DaemonRequest::App { app, request } => {
            if matches!(request, control::Request::Logs { follow: true, .. } | control::Request::Subscribe { .. }) {
                if let Err(msg) = slot.become_stream("forwarded stream") {
                    return reply(&mut w, &reply_err(msg)).await;
                }
            }
            forward(d, &app, request, reader, w).await
        }
        DaemonRequest::Shutdown => {
            let r = reply(&mut w, &reply_ok("wardend is exiting; every app keeps running".into())).await;
            d.shutdown.notify_one();
            r
        }
    }
}

fn hello_event() -> Event {
    Event::Hello {
        protocol: events::PROTOCOL,
        app: None,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
    }
}

/// `subscribe`: `hello`, the `apps` snapshot, one `status` per running app,
/// then events as they happen.
async fn subscribe(
    d: Rc<Daemon>,
    mut reader: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
    interval: Duration,
    logs: bool,
    apps: Vec<String>,
) -> std::io::Result<()> {
    // Subscribe first, so nothing falls between the snapshot and the stream.
    let mut rx = d.bus.subscribe();
    let mut log_rx = logs.then(|| d.logs_bus.subscribe());
    let _wants_logs = logs.then(|| WantsLogs::new(&d));
    let wanted = |app: Option<&str>| apps.is_empty() || app.is_none_or(|a| apps.iter().any(|x| x == a));
    let mut snapshot = String::new();
    {
        let core = d.core.borrow();
        let mut push = |ev: &Event| {
            if let Ok(s) = serde_json::to_string(ev) {
                snapshot.push_str(&s);
                snapshot.push('\n');
            }
        };
        push(&hello_event());
        push(&Event::Apps { apps: core.entries() });
        for r in core.apps.values().filter(|r| r.phase == Phase::Watching && wanted(Some(&*r.name))) {
            if let Some(st) = &r.status {
                push(&Event::Status { app: r.name.to_string(), status: st.clone() });
            }
        }
    }
    send(&mut w, snapshot.as_bytes()).await?;
    drop(snapshot);
    // Supervisors report every second; a longer interval thins status and
    // host events out for this client.
    let thin = interval > WATCH_INTERVAL;
    let mut last_status: HashMap<Arc<str>, Instant> = HashMap::new();
    let mut last_host: Option<Instant> = None;
    let mut scratch = [0u8; 256];
    loop {
        let got = tokio::select! {
            r = rx.recv() => r,
            r = async {
                match log_rx.as_mut() {
                    Some(l) => l.recv().await,
                    None => std::future::pending().await,
                }
            } => r,
            // The client sends nothing more; a read returning means it left.
            n = reader.read(&mut scratch) => match n {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => continue,
            },
        };
        match got {
            Ok(f) => {
                if !wanted(f.app.as_deref()) {
                    continue;
                }
                if thin {
                    let now = Instant::now();
                    let last = match (f.kind, &f.app) {
                        (Kind::Status, Some(app)) => last_status.get(app).copied(),
                        (Kind::Host, _) => last_host,
                        _ => None,
                    };
                    // A little slack: ticks are not exactly one interval apart.
                    if last.is_some_and(|t| now.duration_since(t) + Duration::from_millis(100) < interval) {
                        continue;
                    }
                    match (f.kind, &f.app) {
                        (Kind::Status, Some(app)) => {
                            last_status.insert(app.clone(), now);
                        }
                        (Kind::Host, _) => last_host = Some(now),
                        _ => {}
                    }
                }
                send(&mut w, f.line.as_bytes()).await?;
                if f.kind == Kind::Bye {
                    return Ok(());
                }
            }
            Err(broadcast::error::RecvError::Lagged(dropped)) => {
                let mut s = serde_json::to_string(&Event::Lagged { app: None, dropped }).unwrap_or_default();
                s.push('\n');
                send(&mut w, s.as_bytes()).await?;
            }
            Err(broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

/// `app`: pass a request to one supervisor. `logs` and `subscribe` answer
/// with a stream, piped through as it comes.
async fn forward(
    d: Rc<Daemon>,
    app: &str,
    req: Request,
    mut reader: BufReader<OwnedReadHalf>,
    mut w: OwnedWriteHalf,
) -> std::io::Result<()> {
    let socket = d.core.borrow().apps.get(app).map(|r| r.socket.clone());
    let socket = match socket {
        Some(s) => s,
        None => match fleet::discover().into_iter().find(|a| a.name == app) {
            Some(a) => a.socket,
            None => {
                let msg = format!("no app named {app:?}; `warden list` shows the apps on this host");
                return reply(&mut w, &reply_err(msg)).await;
            }
        },
    };
    let unreachable =
        |e: String| reply_err(format!("cannot reach {app}'s supervisor ({e}); `warden start {app}` starts it"));
    if matches!(req, Request::Logs { .. } | Request::Subscribe { .. }) {
        let up = match tokio::time::timeout(control::REQUEST_TIMEOUT, UnixStream::connect(&socket)).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return reply(&mut w, &unreachable(e.to_string())).await,
            Err(_) => return reply(&mut w, &unreachable("no answer within 5 s".into())).await,
        };
        let (mut up_r, mut up_w) = up.into_split();
        let mut line = serde_json::to_vec(&req).unwrap_or_default();
        line.push(b'\n');
        match tokio::time::timeout(control::REQUEST_TIMEOUT, up_w.write_all(&line)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return reply(&mut w, &unreachable(e.to_string())).await,
            Err(_) => return reply(&mut w, &unreachable("it did not read the request".into())).await,
        }
        let mut buf = vec![0u8; 8192];
        let mut scratch = [0u8; 64];
        loop {
            tokio::select! {
                n = up_r.read(&mut buf) => match n {
                    Ok(0) | Err(_) => return Ok(()),
                    Ok(n) => send(&mut w, &buf[..n]).await?,
                },
                n = reader.read(&mut scratch) => match n {
                    Ok(0) | Err(_) => return Ok(()),
                    Ok(_) => {}
                },
            }
        }
    }
    let mut sink = std::io::sink();
    let r = match tokio::time::timeout(FORWARD_TIMEOUT, control::call(&socket, &req, &mut sink)).await {
        Ok(Ok(Some(resp))) => DaemonReply { ok: resp.ok, response: Some(resp), ..Default::default() },
        Ok(Ok(None)) => unreachable("no response".into()),
        Ok(Err(e)) => unreachable(e),
        Err(_) => reply_err(format!("{app}'s supervisor did not answer within {} s", FORWARD_TIMEOUT.as_secs())),
    };
    reply(&mut w, &r).await
}

#[cfg(test)]
mod tests {
    use super::already_resurrected;

    #[test]
    fn resurrect_runs_once_per_boot() {
        assert!(!already_resurrected(Some("b1"), None), "first start this boot");
        assert!(already_resurrected(Some("b1"), Some("b1\n")), "restart in the same boot");
        assert!(!already_resurrected(Some("b2"), Some("b1")), "after a reboot");
        assert!(!already_resurrected(None, Some("b1")), "boot unknown: resurrect, as before");
        assert!(!already_resurrected(Some(""), Some("")));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn boot_id_is_stable_within_a_boot() {
        let a = super::boot_id().expect("Linux has a boot id");
        assert_eq!(a.len(), 36, "{a}");
        assert_eq!(super::boot_id().as_deref(), Some(a.as_str()));
    }
}

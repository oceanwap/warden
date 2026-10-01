//! What the GUI knows about the host, built from wardend's events only
//! (docs/protocol.md): no I/O here, so every event → state step is tested.

use crate::format;
use crate::ring::Ring;
use std::collections::BTreeMap;
use warden_protocol::control::{RolloutOutcome, RolloutStatus, Status};
use warden_protocol::events::{AppEntry, AppState, Event};

/// Event lines kept per app, and for the whole host.
pub const FEED_CAP: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub struct HostMetrics {
    pub cpu_percent: f64,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub load: [f64; 3],
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    pub pid: u32,
    pub version: String,
    pub protocol: u32,
}

/// One app: wardend's entry, its latest status, and its recent events.
#[derive(Debug, Clone)]
pub struct App {
    pub entry: AppEntry,
    /// The newest status seen (`status` events, or the entry's).
    pub status: Option<Box<Status>>,
    /// The rollout in progress, from `rollout` events and each status.
    pub rollout: Option<RolloutStatus>,
    pub last_outcome: Option<RolloutOutcome>,
    pub feed: Ring<String>,
    /// What the last status line in the feed said (only changes are shown).
    shown_status: Option<String>,
    /// (state, supervisor pid) of the last `apps` line in the feed.
    shown_state: Option<(AppState, Option<u32>)>,
}

impl App {
    fn new(entry: AppEntry) -> App {
        App {
            entry,
            status: None,
            rollout: None,
            last_outcome: None,
            feed: Ring::new(FEED_CAP),
            shown_status: None,
            shown_state: None,
        }
    }

    pub fn name(&self) -> &str {
        &self.entry.name
    }

    /// (ready, configured) workers; `None` without a status.
    pub fn workers(&self) -> Option<(usize, usize)> {
        self.status.as_ref().map(|s| (s.workers_ready, s.workers_configured))
    }

    /// CPU % of the app's processes (workers, hot standbys, and the Bun host
    /// in worker mode); `None` when nothing reports it (no /proc: macOS).
    pub fn cpu_percent(&self) -> Option<f64> {
        let s = self.status.as_ref()?;
        let parts = s
            .workers
            .iter()
            .chain(&s.standbys)
            .map(|w| w.cpu_percent)
            .chain([s.host.as_ref().and_then(|h| h.cpu_percent)]);
        sum_some(parts)
    }

    /// Resident memory of the app: workers, hot standbys (what they cost),
    /// the worker-mode host, and the supervisor.
    pub fn rss_bytes(&self) -> Option<u64> {
        let s = self.status.as_ref()?;
        let parts = s
            .workers
            .iter()
            .chain(&s.standbys)
            .map(|w| w.rss_bytes)
            .chain([s.host.as_ref().and_then(|h| h.rss_bytes), s.supervisor_rss_bytes]);
        let mut total = None;
        for b in parts.flatten() {
            total = Some(total.unwrap_or(0) + b);
        }
        total
    }

    /// The state people read: wardend's, refined by the status.
    pub fn state_label(&self) -> String {
        match (&self.entry.state, &self.status) {
            (AppState::Running, Some(s)) if s.shutting_down => "shutting down".into(),
            (AppState::Running, Some(s)) if s.stopped => "stopped (idle)".into(),
            (AppState::Running, Some(s)) if s.rollout.is_some() || s.reloading => "rolling".into(),
            (AppState::Running, Some(s)) if s.workers_ready < s.workers_configured => "degraded".into(),
            (state, _) => state.as_str().replace('_', " "),
        }
    }

    /// Something is wrong: a problem text, or fewer workers ready than configured.
    pub fn is_unwell(&self) -> bool {
        match self.entry.state {
            AppState::Running => {
                self.status.as_ref().is_some_and(|s| s.workers_ready < s.workers_configured && !s.stopped)
            }
            AppState::Unreachable | AppState::GaveUp => true,
            AppState::Starting | AppState::Stopped | AppState::NotStarted => false,
        }
    }

    /// Its supervisor runs and answers: app requests (reload, scale, …) reach it.
    pub fn supervisor_up(&self) -> bool {
        self.entry.supervisor_pid.is_some() && matches!(self.entry.state, AppState::Running | AppState::Stopped)
    }

    fn line(&mut self, text: String, global: &mut Ring<String>) {
        global.push(text.clone());
        self.feed.push(text);
    }
}

fn sum_some(parts: impl Iterator<Item = Option<f64>>) -> Option<f64> {
    let mut total = None;
    for p in parts.flatten() {
        total = Some(total.unwrap_or(0.0) + p);
    }
    total
}

/// Everything the window shows about the host.
#[derive(Debug)]
pub struct Model {
    pub apps: BTreeMap<String, App>,
    pub host: Option<HostMetrics>,
    pub daemon: Option<DaemonInfo>,
    /// Every event line, all apps and wardend's own.
    pub feed: Ring<String>,
    /// wardend said `bye` (it exits on purpose); the reason.
    pub bye: Option<String>,
    /// Events this window never saw (`lagged`, and batches trimmed by the client).
    pub events_skipped: u64,
    /// Lines from a newer wardend this GUI does not understand.
    pub unknown_events: u64,
}

impl Default for Model {
    fn default() -> Model {
        Model {
            apps: BTreeMap::new(),
            host: None,
            daemon: None,
            feed: Ring::new(FEED_CAP),
            bye: None,
            events_skipped: 0,
            unknown_events: 0,
        }
    }
}

/// Names in the order people expect: `app2` before `app10`.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let n = x.iter().take_while(|c| c.is_ascii_digit()).count();
                let m = y.iter().take_while(|c| c.is_ascii_digit()).count();
                let (dx, dy) = (&x[..n], &y[..m]);
                // Longer (without leading zeros) is larger.
                let tx = dx.iter().position(|&c| c != b'0').map_or(&dx[n..], |i| &dx[i..]);
                let ty = dy.iter().position(|&c| c != b'0').map_or(&dy[m..], |i| &dy[i..]);
                let o = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
                x = &x[n..];
                y = &y[m..];
            }
            (Some(c), Some(d)) => {
                let o = c.to_ascii_lowercase().cmp(&d.to_ascii_lowercase());
                if o != std::cmp::Ordering::Equal {
                    return o;
                }
                x = &x[1..];
                y = &y[1..];
            }
        }
    }
}

impl Model {
    /// The apps, in natural name order.
    pub fn sorted(&self) -> Vec<&App> {
        let mut v: Vec<&App> = self.apps.values().collect();
        v.sort_by(|a, b| natural_cmp(a.name(), b.name()));
        v
    }

    /// The connection is gone: what wardend said about itself no longer holds.
    /// Apps are kept (shown as last seen) until the next snapshot replaces them.
    pub fn disconnected(&mut self) {
        self.daemon = None;
        self.host = None;
    }

    /// Apply one event. `now_ms`: the clock for event lines without a time of their own.
    pub fn apply(&mut self, ev: Event, now_ms: u64) {
        let line = format::event_line(&ev, now_ms);
        match ev {
            Event::Hello { app: None, pid, version, protocol } => {
                self.daemon = Some(DaemonInfo { pid, version, protocol });
                self.bye = None;
                self.feed.extend(line);
            }
            Event::Apps { apps } => self.set_apps(apps, now_ms),
            Event::Status { app, status } => {
                let Some(a) = self.apps.get_mut(&app) else { return };
                a.rollout = status.rollout.clone();
                if status.last_rollout.is_some() {
                    a.last_outcome = status.last_rollout.clone();
                }
                let summary = format::status_summary(&status);
                if a.shown_status.as_deref() != Some(summary.as_str()) {
                    a.line(format!("{} {app} {summary}", format::clock(now_ms)), &mut self.feed);
                    a.shown_status = Some(summary);
                }
                a.status = Some(status);
            }
            Event::Rollout { app, rollout } => {
                if let Some(a) = self.apps.get_mut(&app) {
                    a.rollout = Some(rollout);
                    a.line(line.unwrap_or_default(), &mut self.feed);
                }
            }
            Event::RolloutDone { app, outcome } => {
                if let Some(a) = self.apps.get_mut(&app) {
                    a.rollout = None;
                    a.last_outcome = Some(outcome);
                    a.line(line.unwrap_or_default(), &mut self.feed);
                }
            }
            Event::Worker { app, .. }
            | Event::Supervisor { app, .. }
            | Event::Hello { app: Some(app), .. }
            | Event::Bye { app: Some(app), .. } => match (self.apps.get_mut(&app), line) {
                (Some(a), Some(l)) => a.line(l, &mut self.feed),
                (None, Some(l)) => self.feed.push(l),
                _ => {}
            },
            Event::Lagged { app, dropped } => {
                self.events_skipped += dropped;
                match (app.and_then(|a| self.apps.get_mut(&a)), line) {
                    (Some(a), Some(l)) => a.line(l, &mut self.feed),
                    (None, Some(l)) => self.feed.push(l),
                    _ => {}
                }
            }
            Event::Host { cpu_percent, mem_used_bytes, mem_total_bytes, load, at_ms } => {
                self.host = Some(HostMetrics { cpu_percent, mem_used_bytes, mem_total_bytes, load, at_ms });
            }
            Event::Bye { app: None, reason } => {
                self.bye = Some(reason);
                self.feed.extend(line);
            }
            // The logs pane has its own stream.
            Event::Log { .. } => {}
        }
    }

    /// The `apps` snapshot: the set of apps and each one's state.
    fn set_apps(&mut self, entries: Vec<AppEntry>, now_ms: u64) {
        self.apps.retain(|name, _| entries.iter().any(|e| e.name == *name));
        for entry in entries {
            let name = entry.name.clone();
            let a = self.apps.entry(name).or_insert_with(|| App::new(entry.clone()));
            let key = (entry.state, entry.supervisor_pid);
            if a.shown_state != Some(key) {
                a.shown_state = Some(key);
                a.line(format!("{} {}", format::clock(now_ms), format::app_state(&entry)), &mut self.feed);
            }
            // wardend sends a status only while it watches the supervisor:
            // without one, the workers shown would be stale.
            a.status = entry.status.clone();
            if let Some(s) = &a.status {
                a.rollout = s.rollout.clone();
                if s.last_rollout.is_some() {
                    a.last_outcome = s.last_rollout.clone();
                }
            } else {
                a.rollout = None;
            }
            a.entry = entry;
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use warden_protocol::control::WorkerStatus;
    use warden_protocol::events::{SupervisorEvent, WorkerEvent};

    pub(crate) fn worker(id: usize, rss: Option<u64>, cpu: Option<f64>) -> WorkerStatus {
        WorkerStatus {
            id,
            state: "RUNNING".into(),
            pid: Some(1000 + id as u32),
            uptime_secs: Some(60),
            restarts: 0,
            crashes: 0,
            rss_bytes: rss,
            cpu_seconds: None,
            cpu_percent: cpu,
            last_exit: None,
            healthy: Some(true),
            loop_delay: None,
        }
    }

    pub(crate) fn status(app: &str, n: usize) -> Status {
        Status {
            app: app.into(),
            namespace: "default".into(),
            mode: "process".into(),
            config_path: None,
            unit: None,
            launched: "background".into(),
            stopped: false,
            log_file: None,
            version: "0.1.0".into(),
            pid: 42,
            uptime_secs: 60,
            workers_configured: n,
            workers_ready: n,
            healthy: Some(true),
            supervisor_rss_bytes: Some(4 << 20),
            host: None,
            reloading: false,
            shutting_down: false,
            health_suspended: false,
            log_lines_dropped: 0,
            rollout: None,
            last_rollout: None,
            workers: (1..=n).map(|i| worker(i, Some(40 << 20), Some(1.5))).collect(),
            release: None,
            standbys: vec![],
            start_failed: None,
        }
    }

    pub(crate) fn entry(name: &str, state: AppState, status: Option<Status>) -> AppEntry {
        AppEntry {
            name: name.into(),
            namespace: "default".into(),
            config: Some(format!("/etc/warden/{name}.toml")),
            socket: format!("/run/warden/{name}/control.sock"),
            state,
            supervised_by: "wardend".into(),
            supervisor_pid: if state == AppState::Running { Some(42) } else { None },
            supervisor_restarts: 0,
            status: status.map(Box::new),
            problem: None,
        }
    }

    fn apps(list: Vec<AppEntry>) -> Event {
        Event::Apps { apps: list }
    }

    #[test]
    fn hello_apps_and_status_build_the_model() {
        let mut m = Model::default();
        m.apply(Event::Hello { protocol: 1, app: None, pid: 7, version: "0.1.0".into() }, 0);
        assert_eq!(m.daemon, Some(DaemonInfo { pid: 7, version: "0.1.0".into(), protocol: 1 }));
        m.apply(
            apps(vec![
                entry("api", AppState::Running, Some(status("api", 2))),
                entry("web", AppState::NotStarted, None),
            ]),
            0,
        );
        assert_eq!(m.apps.keys().collect::<Vec<_>>(), ["api", "web"]);
        let api = &m.apps["api"];
        assert_eq!(api.workers(), Some((2, 2)));
        assert_eq!(api.rss_bytes(), Some(84 << 20));
        assert_eq!(api.cpu_percent(), Some(3.0));
        assert_eq!(api.state_label(), "running");
        assert!(api.feed.back().unwrap().ends_with("api running (supervised by wardend, pid 42)"), "{:?}", api.feed);
        assert_eq!(m.apps["web"].state_label(), "not started");
        assert!(m.apps["web"].workers().is_none() && m.apps["web"].cpu_percent().is_none());

        // A status: only a changed summary makes a feed line.
        let mut s = status("api", 2);
        s.workers_ready = 1;
        m.apply(Event::Status { app: "api".into(), status: Box::new(s.clone()) }, 0);
        m.apply(Event::Status { app: "api".into(), status: Box::new(s) }, 0);
        let api = &m.apps["api"];
        assert_eq!(api.workers(), Some((1, 2)));
        assert_eq!(api.state_label(), "degraded");
        assert!(api.is_unwell());
        let lines: Vec<_> = api.feed.iter().filter(|l| l.contains("workers ready")).collect();
        assert_eq!(lines.len(), 1, "the same summary twice is one line: {lines:?}");
        // A status for an app not in the snapshot is ignored.
        m.apply(Event::Status { app: "ghost".into(), status: Box::new(status("ghost", 1)) }, 0);
        assert!(!m.apps.contains_key("ghost"));
    }

    /// Hot standbys (`Status.standbys`) cost memory and CPU, so they count
    /// there; they are not workers, so not in "ready".
    #[test]
    fn standbys_count_in_resources_not_in_workers() {
        let mut m = Model::default();
        let mut s = status("api", 2);
        let mut sb = worker(1, Some(40 << 20), Some(0.5));
        sb.state = "STANDBY".into();
        s.standbys.push(sb);
        m.apply(apps(vec![entry("api", AppState::Running, Some(s))]), 0);
        let api = &m.apps["api"];
        assert_eq!(api.workers(), Some((2, 2)));
        assert_eq!(api.rss_bytes(), Some(124 << 20));
        assert_eq!(api.cpu_percent(), Some(3.5));
        assert_eq!(api.state_label(), "running");
    }

    #[test]
    fn apps_snapshot_replaces_the_set_and_drops_stale_status() {
        let mut m = Model::default();
        m.apply(
            apps(vec![entry("api", AppState::Running, Some(status("api", 1))), entry("old", AppState::Running, None)]),
            0,
        );
        m.apply(apps(vec![entry("api", AppState::Stopped, None)]), 0);
        assert_eq!(m.apps.keys().collect::<Vec<_>>(), ["api"]);
        let api = &m.apps["api"];
        assert!(api.status.is_none(), "no status from wardend: no stale workers");
        assert_eq!(api.state_label(), "stopped");
        assert!(!api.supervisor_up());
        // The same state again: no new line.
        let n = api.feed.len();
        m.apply(apps(vec![entry("api", AppState::Stopped, None)]), 0);
        assert_eq!(m.apps["api"].feed.len(), n);
    }

    #[test]
    fn rollouts_are_followed_until_done() {
        let mut m = Model::default();
        m.apply(apps(vec![entry("api", AppState::Running, Some(status("api", 2)))]), 0);
        let r = RolloutStatus {
            seq: 3,
            kind: "reload".into(),
            phase: "starting".into(),
            done: 0,
            total: 2,
            elapsed_secs: 0,
        };
        m.apply(Event::Rollout { app: "api".into(), rollout: r.clone() }, 0);
        assert_eq!(m.apps["api"].rollout.as_ref().map(|r| r.done), Some(0));
        assert_eq!(m.apps["api"].state_label(), "running", "the entry's status has no rollout yet");
        let mut s = status("api", 2);
        s.rollout = Some(RolloutStatus { done: 1, ..r });
        m.apply(Event::Status { app: "api".into(), status: Box::new(s) }, 0);
        assert_eq!(m.apps["api"].rollout.as_ref().map(|r| r.done), Some(1));
        assert_eq!(m.apps["api"].state_label(), "rolling");
        let o = RolloutOutcome {
            seq: 3,
            kind: "reload".into(),
            ok: true,
            message: "2 workers replaced".into(),
            duration_secs: 1.5,
        };
        m.apply(Event::RolloutDone { app: "api".into(), outcome: o.clone() }, 0);
        assert!(m.apps["api"].rollout.is_none());
        assert_eq!(m.apps["api"].last_outcome, Some(o));
        assert!(m.apps["api"].feed.back().unwrap().contains("api reload done: 2 workers replaced (1.5 s)"));
    }

    #[test]
    fn worker_supervisor_lagged_host_and_bye_events() {
        let mut m = Model::default();
        m.apply(apps(vec![entry("api", AppState::Running, None)]), 0);
        m.apply(
            Event::Worker {
                app: "api".into(),
                worker: 1,
                event: WorkerEvent::Crashed,
                pid: Some(9),
                detail: Some("exit code 1".into()),
                at_ms: 0,
            },
            0,
        );
        m.apply(
            Event::Supervisor {
                app: "api".into(),
                event: SupervisorEvent::Died,
                pid: Some(42),
                detail: None,
                at_ms: 0,
            },
            0,
        );
        m.apply(
            Event::Supervisor { app: "other".into(), event: SupervisorEvent::Found, pid: None, detail: None, at_ms: 0 },
            0,
        );
        m.apply(Event::Lagged { app: None, dropped: 5 }, 0);
        let feed: Vec<&String> = m.apps["api"].feed.iter().collect();
        assert!(feed.iter().any(|l| l.ends_with("api worker 1 crashed pid=9 exit code 1")), "{feed:?}");
        assert!(feed.iter().any(|l| l.ends_with("api supervisor died pid=42")), "{feed:?}");
        assert!(m.feed.iter().any(|l| l.ends_with("other supervisor found")), "unknown apps go to the host feed");
        assert_eq!(m.events_skipped, 5);
        m.apply(
            Event::Host { cpu_percent: 12.5, mem_used_bytes: 1, mem_total_bytes: 2, load: [0.5, 0.4, 0.3], at_ms: 1 },
            0,
        );
        assert_eq!(m.host.as_ref().map(|h| h.cpu_percent), Some(12.5));
        m.apply(Event::Bye { app: None, reason: "SIGTERM".into() }, 0);
        assert_eq!(m.bye.as_deref(), Some("SIGTERM"));
        m.disconnected();
        assert!(m.daemon.is_none() && m.host.is_none() && m.apps.contains_key("api"));
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["app10", "app2", "App1", "api", "app02", "b", "app1"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["api", "App1", "app1", "app02", "app2", "app10", "b"]);
        let mut m = Model::default();
        m.apply(apps(vec![entry("w10", AppState::Running, None), entry("w9", AppState::Running, None)]), 0);
        assert_eq!(m.sorted().iter().map(|a| a.name()).collect::<Vec<_>>(), ["w9", "w10"]);
    }

    #[test]
    fn feeds_are_bounded() {
        let mut m = Model::default();
        m.apply(apps(vec![entry("api", AppState::Running, None)]), 0);
        for i in 0..(FEED_CAP * 2) {
            m.apply(
                Event::Worker {
                    app: "api".into(),
                    worker: i,
                    event: WorkerEvent::Ready,
                    pid: None,
                    detail: None,
                    at_ms: 0,
                },
                0,
            );
        }
        assert_eq!(m.apps["api"].feed.len(), FEED_CAP);
        assert_eq!(m.feed.len(), FEED_CAP);
        assert!(m.apps["api"].feed.back().unwrap().contains(&format!("worker {} ready", FEED_CAP * 2 - 1)));
    }
}

//! Live events: what supervisors push to subscribers (`subscribe` on the app
//! control socket) and what `wardend` pushes to the CLI and the GUI, and the
//! requests wardend's socket takes. The wire format is documented in
//! docs/protocol.md; keep the two in step.

use crate::control::{Request, Response, RolloutOutcome, RolloutStatus, Status};
use serde::{Deserialize, Serialize};

/// Wire protocol version; goes up only for incompatible changes.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Hello {
        protocol: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        pid: u32,
        version: String,
    },
    Status {
        app: String,
        status: Box<Status>,
    },
    Worker {
        app: String,
        worker: usize,
        event: WorkerEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        at_ms: u64,
    },
    Rollout {
        app: String,
        rollout: RolloutStatus,
    },
    RolloutDone {
        app: String,
        outcome: RolloutOutcome,
    },
    Log {
        app: String,
        line: String,
    },
    Lagged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        dropped: u64,
    },
    Apps {
        apps: Vec<AppEntry>,
    },
    Supervisor {
        app: String,
        event: SupervisorEvent,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pid: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        at_ms: u64,
    },
    Host {
        cpu_percent: f64,
        mem_used_bytes: u64,
        mem_total_bytes: u64,
        /// 1, 5 and 15 minute load averages.
        load: [f64; 3],
        at_ms: u64,
    },
    Bye {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerEvent {
    Starting,
    Ready,
    Unhealthy,
    Hung,
    Crashed,
    /// Exited on its own with a clean status.
    Exited,
    Restarting,
    /// Too many restarts: left down until `failed_cooldown` or `warden reset`.
    Failed,
    Stopping,
    Stopped,
}

impl WorkerEvent {
    /// The wire name (`crashed`).
    pub fn as_str(self) -> &'static str {
        match self {
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
}

/// The exit reason of a worker the kernel's OOM killer killed (it died of
/// SIGKILL and the cgroup's `oom_kill` count rose): the supervisor writes it
/// at the start of the `crashed` event's `detail` and of
/// `WorkerStatus.last_exit`, maybe followed by more (` (standby)`), and
/// wardend's `oom` alert matches it anywhere in the detail
/// (docs/protocol.md, "The OOM marker").
pub const OOM_KILLED: &str = "killed by the kernel OOM killer (out of memory)";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SupervisorEvent {
    Found,
    Started,
    Exited,
    Died,
    Restarting,
    GaveUp,
    Unresponsive,
    Responsive,
}

impl SupervisorEvent {
    /// The wire name (`gave_up`).
    pub fn as_str(self) -> &'static str {
        match self {
            SupervisorEvent::Found => "found",
            SupervisorEvent::Started => "started",
            SupervisorEvent::Exited => "exited",
            SupervisorEvent::Died => "died",
            SupervisorEvent::Restarting => "restarting",
            SupervisorEvent::GaveUp => "gave_up",
            SupervisorEvent::Unresponsive => "unresponsive",
            SupervisorEvent::Responsive => "responsive",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AppState {
    Running,
    Stopped,
    Starting,
    Unreachable,
    GaveUp,
    NotStarted,
}

impl AppState {
    /// The wire name (`not_started`).
    pub fn as_str(self) -> &'static str {
        match self {
            AppState::Running => "running",
            AppState::Stopped => "stopped",
            AppState::Starting => "starting",
            AppState::Unreachable => "unreachable",
            AppState::GaveUp => "gave_up",
            AppState::NotStarted => "not_started",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AppEntry {
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default)]
    pub config: Option<String>,
    pub socket: String,
    pub state: AppState,
    /// `systemd`, `wardend` or `terminal`: who restarts a dead supervisor.
    pub supervised_by: String,
    #[serde(default)]
    pub supervisor_pid: Option<u32>,
    #[serde(default)]
    pub supervisor_restarts: u32,
    #[serde(default)]
    pub status: Option<Box<Status>>,
    /// Why it is not running, with the fix.
    #[serde(default)]
    pub problem: Option<String>,
}

/// Requests on the `wardend` socket.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum DaemonRequest {
    Hello,
    Apps,
    Subscribe {
        #[serde(default)]
        interval_ms: Option<u64>,
        #[serde(default)]
        logs: bool,
        /// Only these apps; empty: all.
        #[serde(default)]
        apps: Vec<String>,
    },
    Start {
        app: String,
    },
    /// Forward a request to one app's supervisor.
    App {
        app: String,
        request: Request,
    },
    Shutdown,
    /// The resource history (CPU, memory, workers, restarts) of the last
    /// 24 h, in `history`. `app`: that app only (`""`: the host only);
    /// none: every app. The host's series always come.
    History {
        #[serde(default)]
        app: Option<String>,
        /// From this Unix time in ms (default, and at most: 24 h ago).
        #[serde(default)]
        since_ms: Option<u64>,
        /// Seconds per point (default 10, wardend's sampling period).
        #[serde(default)]
        step_s: Option<u32>,
    },
    /// Read `wardend.toml` (the alert rules) again, as SIGHUP does. On an
    /// error the previous rules stay and the reply says what is wrong.
    Reload,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DaemonReply {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hello: Option<Event>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apps: Option<Vec<AppEntry>>,
    /// `app`: the supervisor's answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<Response>,
    /// `history`: the series.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<ResourceHistory>,
}

// ---------------------------------------------------------------- history

/// The answer to `history` (docs/protocol.md, "Resource history"): series
/// on one time grid. Point `i` covers `[start_ms + i * step_s * 1000, +
/// step_s)`; `null` where nothing was sampled (wardend not running, the
/// app not watched). Each array has `points` entries.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ResourceHistory {
    #[serde(default)]
    pub start_ms: u64,
    #[serde(default)]
    pub step_s: u32,
    #[serde(default)]
    pub points: u32,
    #[serde(default)]
    pub host: HostHistory,
    #[serde(default)]
    pub apps: Vec<AppHistory>,
}

/// The host: CPU % of all CPUs (mean per point), memory used (max), the
/// 1-minute load average (mean).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct HostHistory {
    #[serde(default)]
    pub cpu_percent: Vec<Option<f32>>,
    #[serde(default)]
    pub mem_used_bytes: Vec<Option<u64>>,
    /// The newest total, in bytes.
    #[serde(default)]
    pub mem_total_bytes: Option<u64>,
    #[serde(default)]
    pub load1: Vec<Option<f32>>,
}

/// One app: CPU % of one core, summed over its processes (mean per point),
/// resident memory with the supervisor's (max), workers ready (min) and
/// configured (max), and restarts (sum: how many happened in that point).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AppHistory {
    pub app: String,
    #[serde(default)]
    pub cpu_percent: Vec<Option<f32>>,
    #[serde(default)]
    pub rss_bytes: Vec<Option<u64>>,
    #[serde(default)]
    pub workers_ready: Vec<Option<u32>>,
    #[serde(default)]
    pub workers_configured: Vec<Option<u32>>,
    #[serde(default)]
    pub restarts: Vec<Option<u32>>,
}

/// What the history samples from one `status`: wardend and the GUI (which
/// appends live statuses to the series it fetched) count the same way.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Usage {
    /// CPU % of one core: the workers and the worker-mode host. `None`
    /// when nothing reports it (no /proc: macOS).
    pub cpu_percent: Option<f64>,
    /// Resident memory: the workers, the worker-mode host and the supervisor.
    pub rss_bytes: Option<u64>,
    /// Restarts so far as this supervisor counts them (its workers', and
    /// the worker-mode host's).
    pub restarts: u64,
}

impl Usage {
    pub fn of(s: &Status) -> Usage {
        let mut u = Usage::default();
        let host = s.host.as_ref();
        for c in s.workers.iter().map(|w| w.cpu_percent).chain([host.and_then(|h| h.cpu_percent)]).flatten() {
            u.cpu_percent = Some(u.cpu_percent.unwrap_or(0.0) + c);
        }
        let rss = s.workers.iter().map(|w| w.rss_bytes).chain([host.and_then(|h| h.rss_bytes), s.supervisor_rss_bytes]);
        for b in rss.flatten() {
            u.rss_bytes = Some(u.rss_bytes.unwrap_or(0).saturating_add(b));
        }
        u.restarts = s.workers.iter().map(|w| w.restarts).chain(host.map(|h| h.restarts)).fold(0, u64::saturating_add);
        u
    }
}

/// Restarts between two statuses of the same supervisor (`pid`): what the
/// counters grew by. A new supervisor (its counters start at 0) counts
/// everything it has; counters that shrank (a worker scaled away) count 0.
pub fn restarts_since(prev: Option<(u32, u64)>, pid: u32, total: u64) -> u64 {
    match prev {
        Some((p, before)) if p == pid => total.saturating_sub(before),
        Some(_) => total,
        // The first status seen: what came before is not news.
        None => 0,
    }
}

/// Default and bounds for `interval_ms`.
pub fn interval(ms: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_millis(ms.unwrap_or(1000).clamp(250, 60_000))
}

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_matches_the_protocol_doc() {
        let ev = Event::Worker {
            app: "api".into(),
            worker: 2,
            event: WorkerEvent::Crashed,
            pid: Some(42),
            detail: Some("exit code 1".into()),
            at_ms: 7,
        };
        assert_eq!(
            serde_json::to_string(&ev).unwrap(),
            r#"{"type":"worker","app":"api","worker":2,"event":"crashed","pid":42,"detail":"exit code 1","at_ms":7}"#
        );
        let bye = Event::Bye { app: None, reason: "shutdown".into() };
        assert_eq!(serde_json::to_string(&bye).unwrap(), r#"{"type":"bye","reason":"shutdown"}"#);
        let done: Event = serde_json::from_str(r#"{"type":"lagged","dropped":3}"#).unwrap();
        assert_eq!(done, Event::Lagged { app: None, dropped: 3 });
        let sup: Event =
            serde_json::from_str(r#"{"type":"supervisor","app":"api","event":"gave_up","at_ms":1}"#).unwrap();
        assert!(matches!(sup, Event::Supervisor { event: SupervisorEvent::GaveUp, .. }));
    }

    #[test]
    fn daemon_requests_parse_as_documented() {
        let r: DaemonRequest =
            serde_json::from_str(r#"{"cmd":"app","app":"api","request":{"cmd":"reload","safe":true}}"#).unwrap();
        assert_eq!(r, DaemonRequest::App { app: "api".into(), request: Request::Reload { safe: true } });
        let s: DaemonRequest = serde_json::from_str(r#"{"cmd":"subscribe"}"#).unwrap();
        assert_eq!(s, DaemonRequest::Subscribe { interval_ms: None, logs: false, apps: vec![] });
        let sub: Request = serde_json::from_str(r#"{"cmd":"subscribe","interval_ms":500}"#).unwrap();
        assert_eq!(sub, Request::Subscribe { interval_ms: Some(500), logs: false });
        let h: DaemonRequest = serde_json::from_str(r#"{"cmd":"history"}"#).unwrap();
        assert_eq!(h, DaemonRequest::History { app: None, since_ms: None, step_s: None });
        let h: DaemonRequest = serde_json::from_str(r#"{"cmd":"history","app":"api","step_s":60}"#).unwrap();
        assert_eq!(h, DaemonRequest::History { app: Some("api".into()), since_ms: None, step_s: Some(60) });
        assert_eq!(serde_json::from_str::<DaemonRequest>(r#"{"cmd":"reload"}"#).unwrap(), DaemonRequest::Reload);
    }

    #[test]
    fn history_replies_parse_as_documented() {
        let r: DaemonReply = serde_json::from_str(
            r#"{"ok":true,"history":{"start_ms":1000,"step_s":60,"points":2,
                "host":{"cpu_percent":[1.5,null],"mem_used_bytes":[10,null],"mem_total_bytes":20,"load1":[0.5,null]},
                "apps":[{"app":"api","cpu_percent":[null,3.0],"rss_bytes":[null,4096],"workers_ready":[null,2],
                         "workers_configured":[null,2],"restarts":[null,1]}]}}"#,
        )
        .unwrap();
        let h = r.history.unwrap();
        assert_eq!((h.start_ms, h.step_s, h.points), (1000, 60, 2));
        assert_eq!(h.host.cpu_percent, [Some(1.5), None]);
        assert_eq!(h.apps[0].rss_bytes, [None, Some(4096)]);
        // An older wardend's reply has no history; a newer one's may have more fields.
        let old: DaemonReply = serde_json::from_str(r#"{"ok":true,"history":{"later":1}}"#).unwrap();
        assert_eq!(old.history, Some(ResourceHistory::default()));
        assert!(!serde_json::to_string(&DaemonReply::default()).unwrap().contains("history"));
    }

    #[test]
    fn usage_sums_the_processes_of_a_status() {
        let s: Status = serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": 7, "uptime_secs": 1, "workers_configured": 2, "workers_ready": 2,
            "healthy": null, "supervisor_rss_bytes": 100, "host": null, "reloading": false, "shutting_down": false,
            "workers": [
                {"id": 1, "state": "RUNNING", "pid": 8, "uptime_secs": 1, "restarts": 2, "crashes": 2, "rss_bytes": 1000,
                 "cpu_seconds": null, "cpu_percent": 1.5, "last_exit": null},
                {"id": 2, "state": "RUNNING", "pid": 9, "uptime_secs": 1, "restarts": 1, "crashes": 1, "rss_bytes": 2000,
                 "cpu_seconds": null, "cpu_percent": null, "last_exit": null}
            ]
        }))
        .unwrap();
        assert_eq!(Usage::of(&s), Usage { cpu_percent: Some(1.5), rss_bytes: Some(3100), restarts: 3 });
        assert_eq!(restarts_since(None, 7, 3), 0, "the first status is the baseline");
        assert_eq!(restarts_since(Some((7, 1)), 7, 3), 2);
        assert_eq!(restarts_since(Some((7, 5)), 7, 3), 0, "a worker scaled away");
        assert_eq!(restarts_since(Some((6, 9)), 7, 3), 3, "a new supervisor counts from 0");
    }

    #[test]
    fn app_entries_and_host_events_parse_as_documented() {
        let apps: Event = serde_json::from_str(
            r#"{"type":"apps","apps":[{"name":"api","socket":"/run/warden/api/control.sock","state":"gave_up",
                "supervised_by":"wardend","problem":"died 10 times; `warden start api`"}]}"#,
        )
        .unwrap();
        let Event::Apps { apps } = apps else { panic!("not apps") };
        assert_eq!((apps[0].state, apps[0].namespace.as_str(), apps[0].supervisor_restarts), (AppState::GaveUp, "", 0));
        let host: Event = serde_json::from_str(
            r#"{"type":"host","cpu_percent":12.5,"mem_used_bytes":1,"mem_total_bytes":2,"load":[0.1,0.2,0.3],"at_ms":9}"#,
        )
        .unwrap();
        assert!(matches!(host, Event::Host { load: [_, _, l15], .. } if l15 == 0.3));
    }

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
            assert_eq!(serde_json::to_value(s).unwrap(), s.as_str());
        }
        for e in [
            WorkerEvent::Starting,
            WorkerEvent::Ready,
            WorkerEvent::Unhealthy,
            WorkerEvent::Hung,
            WorkerEvent::Crashed,
            WorkerEvent::Exited,
            WorkerEvent::Restarting,
            WorkerEvent::Failed,
            WorkerEvent::Stopping,
            WorkerEvent::Stopped,
        ] {
            assert_eq!(serde_json::to_value(e).unwrap(), e.as_str());
        }
        for e in [
            SupervisorEvent::Found,
            SupervisorEvent::Started,
            SupervisorEvent::Exited,
            SupervisorEvent::Died,
            SupervisorEvent::Restarting,
            SupervisorEvent::GaveUp,
            SupervisorEvent::Unresponsive,
            SupervisorEvent::Responsive,
        ] {
            assert_eq!(serde_json::to_value(e).unwrap(), e.as_str());
        }
    }

    #[test]
    fn interval_is_clamped() {
        assert_eq!(interval(None).as_millis(), 1000);
        assert_eq!(interval(Some(1)).as_millis(), 250);
        assert_eq!(interval(Some(u64::MAX)).as_millis(), 60_000);
    }
}

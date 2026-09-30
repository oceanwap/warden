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

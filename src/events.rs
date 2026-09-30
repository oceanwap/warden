//! Live events: what supervisors push to subscribers (`subscribe` on the app
//! control socket) and what `wardend` pushes to the CLI and the GUI.
//! The wire format is documented in docs/protocol.md; keep the two in step.
//!
//! Inside a supervisor, `emit` hands an event to every subscriber through a
//! bounded broadcast. With no subscriber it costs one atomic load, so emit
//! calls can sit on every state change.

use crate::control::{Request, RolloutOutcome, RolloutStatus, Status};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;

/// Wire protocol version; goes up only for incompatible changes.
pub const PROTOCOL: u32 = 1;

/// Set by `warden start` (and `wardend`) on supervisors they start in the
/// background, so `Status.launched` can say who restarts a dead supervisor.
pub const LAUNCH_ENV: &str = "WARDEN_LAUNCH";

/// Events a slow subscriber may fall behind by before it gets `lagged`.
const CAPACITY: usize = 1024;

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
    pub response: Option<crate::control::Response>,
}

/// Default and bounds for `interval_ms`.
pub fn interval(ms: Option<u64>) -> std::time::Duration {
    std::time::Duration::from_millis(ms.unwrap_or(1000).clamp(250, 60_000))
}

/// Unix time in milliseconds.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

// ------------------------------------------------------------ in-process bus

fn bus() -> &'static broadcast::Sender<Event> {
    static BUS: OnceLock<broadcast::Sender<Event>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(CAPACITY).0)
}

/// Set by `subscribe`, cleared by `active` once the last receiver is gone, so
/// that `active` without subscribers is one atomic load (tokio's
/// `receiver_count` takes the channel's lock).
static MAYBE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Receive every event emitted from now on.
pub fn subscribe() -> broadcast::Receiver<Event> {
    let rx = bus().subscribe();
    MAYBE_ACTIVE.store(true, Ordering::SeqCst);
    rx
}

/// Is anyone listening? Check before building an expensive event.
pub fn active() -> bool {
    if !MAYBE_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }
    if bus().receiver_count() > 0 {
        return true;
    }
    MAYBE_ACTIVE.store(false, Ordering::SeqCst);
    // A subscriber that came in meanwhile must not be missed.
    let again = bus().receiver_count() > 0;
    if again {
        MAYBE_ACTIVE.store(true, Ordering::SeqCst);
    }
    again
}

/// Hand an event to every subscriber (none: dropped at once).
pub fn emit(ev: Event) {
    if active() {
        // Err only means the last subscriber left meanwhile: nothing to do.
        let _ = bus().send(ev);
    }
}

/// A worker changed state.
pub fn worker(app: &str, worker: usize, event: WorkerEvent, pid: Option<u32>, detail: Option<String>) {
    if active() {
        emit(Event::Worker { app: app.to_string(), worker, event, pid, detail, at_ms: now_ms() });
    }
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
    fn interval_is_clamped() {
        assert_eq!(interval(None).as_millis(), 1000);
        assert_eq!(interval(Some(1)).as_millis(), 250);
        assert_eq!(interval(Some(u64::MAX)).as_millis(), 60_000);
    }

    #[tokio::test]
    async fn emit_reaches_subscribers_and_costs_nothing_without_them() {
        // Other tests may subscribe concurrently; only check our own receiver.
        let mut rx = subscribe();
        assert!(active());
        worker("bus-test", 1, WorkerEvent::Ready, Some(1), None);
        loop {
            match rx.recv().await.unwrap() {
                Event::Worker { app, event, .. } if app == "bus-test" => {
                    assert_eq!(event, WorkerEvent::Ready);
                    break;
                }
                _ => continue,
            }
        }
    }
}

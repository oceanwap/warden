//! The app control socket (`<runtime dir>/<app>/control.sock`): one JSON
//! request line, one JSON response line, except `logs` (log lines) and
//! `subscribe` (events). wardend forwards the same requests (`DaemonRequest::App`).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    /// Stop all workers; the supervisor stays up.
    Stop,
    /// Stop all workers and exit the supervisor.
    Shutdown,
    /// Replace workers one at a time through the health gates (all, or one);
    /// `hard`: stop every worker, then start them again (PM2's `restart`).
    Restart {
        worker: Option<usize>,
        #[serde(default)]
        hard: bool,
    },
    /// Rolling restart: each new worker must pass the gates before the old one
    /// is drained. `safe`: preflight, canary soak with rollback, pauses.
    Reload {
        #[serde(default)]
        safe: bool,
    },
    Scale {
        count: usize,
    },
    /// Start the workers of a stopped app.
    Start,
    /// Clear restart counters and FAILED state (one worker or all), and
    /// start FAILED workers now.
    Reset {
        #[serde(default)]
        worker: Option<usize>,
    },
    /// Send a signal to the workers (or one worker): `SIGUSR2`, `USR2`, `12`.
    Signal {
        signal: String,
        #[serde(default)]
        worker: Option<usize>,
    },
    /// The effective config and paths, with env values hidden unless
    /// `show_secrets`.
    Config {
        #[serde(default)]
        show_secrets: bool,
    },
    /// Empty the in-memory log buffers and truncate the log file.
    Flush,
    Logs {
        lines: usize,
        follow: bool,
        /// Only lines about this worker (its output and Warden's events for it).
        #[serde(default)]
        worker: Option<String>,
        /// Only Warden's own events, no worker output.
        #[serde(default)]
        events: bool,
        /// Only worker output on this stream: "stdout" or "stderr".
        #[serde(default)]
        stream: Option<String>,
    },
    /// Show or change the log level at runtime (not saved to the config).
    #[serde(rename = "log-level")]
    LogLevel {
        #[serde(default)]
        level: Option<crate::Level>,
    },
    /// Stream live events (docs/protocol.md): a status snapshot, worker and
    /// rollout events as they happen, a status every `interval_ms`.
    Subscribe {
        #[serde(default)]
        interval_ms: Option<u64>,
        /// Also every log line, as `log` events.
        #[serde(default)]
        logs: bool,
    },
}

/// Most control connections served at once; more are refused with a message.
pub const MAX_CONNECTIONS: usize = 64;
/// Long-lived streams (`subscribe`, `logs -f`) served at once. They have
/// their own budget and give back their request slot, so viewers (a GUI
/// reconnecting in a loop, many `warden events`) can never use up the slots
/// `status`, `stop` or `restart` need: the operator can always reach a
/// supervisor, however many clients watch it.
pub const MAX_STREAMS: usize = 32;
/// A client must send its request line within this time, and a streaming
/// client that stops reading for this long is disconnected.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// Rollout started by this request; poll `status.last_rollout` for its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
    /// `config`: the effective config and paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(msg: impl Into<String>) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: None, info: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, message: Some(msg.into()), status: None, seq: None, info: None }
    }
    pub fn started(msg: impl Into<String>, seq: u64) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: Some(seq), info: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub app: String,
    #[serde(default)]
    pub namespace: String,
    pub mode: String,
    /// The config file this supervisor was started with.
    #[serde(default)]
    pub config_path: Option<String>,
    /// systemd unit running this supervisor (`warden@api.service`), if any.
    #[serde(default)]
    pub unit: Option<String>,
    /// How this supervisor was started: `systemd`, `background` (`warden
    /// start` or `wardend`: `wardend` restarts it if it dies) or `terminal`.
    #[serde(default)]
    pub launched: String,
    /// `warden stop`: workers stopped on request, supervisor idle.
    #[serde(default)]
    pub stopped: bool,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub version: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub workers_configured: usize,
    pub workers_ready: usize,
    pub healthy: Option<bool>,
    pub supervisor_rss_bytes: Option<u64>,
    /// Worker mode: the Bun process hosting the Workers.
    pub host: Option<HostStatus>,
    pub reloading: bool,
    pub shutting_down: bool,
    /// DS2: replacements held because most workers fail health at once.
    #[serde(default)]
    pub health_suspended: bool,
    /// Log lines dropped because stdout could not keep up (CP5).
    #[serde(default)]
    pub log_lines_dropped: u64,
    /// Rollout in progress (reload, safe-reload, restart N, recycling).
    #[serde(default)]
    pub rollout: Option<RolloutStatus>,
    #[serde(default)]
    pub last_rollout: Option<RolloutOutcome>,
    pub workers: Vec<WorkerStatus>,
    /// `[app] pin_release`: the real path workers start in (the target of a
    /// `current` symlink when the pin was taken). Crash restarts reuse it;
    /// reload, safe-reload and restart move it. None when not pinned.
    #[serde(default)]
    pub release: Option<String>,
    /// Hot standbys (`[workers] standby`, process mode): started, not
    /// listening; one takes the slot of a worker that dies. Absent (empty)
    /// without standbys; their `restarts`, `crashes` and `last_exit` are
    /// the pool's. Not in `workers`, so older clients don't see them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standbys: Vec<WorkerStatus>,
    /// Old processes a rollout replaced that are still draining (closing
    /// WebSockets and SSE streams, finishing requests) until they exit:
    /// `id` is the worker whose old process it is (0: worker mode's host),
    /// `state` [`DRAINING`]. They take no new connections; their
    /// replacements already serve. Absent (empty) when none; not in
    /// `workers`, so older clients don't see them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub draining: Vec<WorkerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutStatus {
    pub seq: u64,
    pub kind: String,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutOutcome {
    pub seq: u64,
    pub kind: String,
    pub ok: bool,
    pub message: String,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub restarts: u64,
}

/// `Status.standbys[].state` of a standby that can take over a crashed worker.
pub const STANDBY: &str = "STANDBY";
/// `Status.standbys[].state` of a standby still starting (initializing, or
/// passing its health gates).
pub const WARMING: &str = "WARMING";
/// `Status.draining[].state`: an old process replaced by a rollout, draining.
pub const DRAINING: &str = "DRAINING";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkerStatus {
    /// The worker number (1..=count). In `Status.standbys`: the standby's
    /// place in the list (1..=standby), not a worker number. In
    /// `Status.draining`: the worker whose old process it is.
    pub id: usize,
    /// `STARTING`, `RUNNING`, `STOPPING`, `STOPPED`, `CRASHED`,
    /// `RESTARTING`, `FAILED`. Standbys: [`WARMING`], [`STANDBY`],
    /// `STOPPING`, and `RESTARTING` / `FAILED` / `STOPPED` for a missing one.
    /// Draining old processes: [`DRAINING`].
    pub state: String,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restarts: u64,
    pub crashes: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_seconds: Option<f64>,
    pub cpu_percent: Option<f64>,
    pub last_exit: Option<String>,
    /// Per-worker health verdict (private-socket checks).
    #[serde(default)]
    pub healthy: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"restart","worker":2}"#).unwrap(),
            Request::Restart { worker: Some(2), hard: false }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"scale","count":3}"#).unwrap(),
            Request::Scale { count: 3 }
        );
        // Older clients send logs without the filter fields.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"logs","lines":5,"follow":false}"#).unwrap(),
            Request::Logs { lines: 5, follow: false, worker: None, events: false, stream: None }
        );
        assert_eq!(
            serde_json::to_string(&Request::LogLevel { level: Some(crate::Level::Debug) }).unwrap(),
            r#"{"cmd":"log-level","level":"debug"}"#
        );
    }

    /// `standbys` is additive both ways: an older supervisor's status (none)
    /// parses, and none is sent without standbys; they never mix with workers.
    #[test]
    fn standbys_are_additive() {
        let old = r#"{"app":"api","mode":"process","pid":1,"uptime_secs":1,"workers_configured":1,"workers_ready":1,
            "healthy":null,"supervisor_rss_bytes":null,"host":null,"reloading":false,"shutting_down":false,"workers":[]}"#;
        let mut s: Status = serde_json::from_str(old).unwrap();
        assert!(s.standbys.is_empty());
        assert!(!serde_json::to_string(&s).unwrap().contains("standbys"), "not sent without standbys");
        s.standbys.push(WorkerStatus {
            id: 1,
            state: STANDBY.into(),
            pid: Some(7),
            uptime_secs: Some(3),
            restarts: 0,
            crashes: 0,
            rss_bytes: None,
            cpu_seconds: None,
            cpu_percent: None,
            last_exit: None,
            healthy: None,
        });
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["standbys"][0]["state"], "STANDBY");
        assert_eq!(v["workers"].as_array().map(Vec::len), Some(0));
        let back: Status = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }

    /// `draining` is additive the same way: absent from an older
    /// supervisor's status and when nothing drains; its rows are old
    /// processes, never counted as workers.
    #[test]
    fn draining_is_additive() {
        let old = r#"{"app":"api","mode":"process","pid":1,"uptime_secs":1,"workers_configured":2,"workers_ready":2,
            "healthy":null,"supervisor_rss_bytes":null,"host":null,"reloading":true,"shutting_down":false,"workers":[]}"#;
        let mut s: Status = serde_json::from_str(old).unwrap();
        assert!(s.draining.is_empty());
        assert!(!serde_json::to_string(&s).unwrap().contains("draining"), "not sent when nothing drains");
        s.draining.push(WorkerStatus {
            id: 2,
            state: DRAINING.into(),
            pid: Some(8),
            uptime_secs: Some(60),
            restarts: 0,
            crashes: 0,
            rss_bytes: Some(40 << 20),
            cpu_seconds: None,
            cpu_percent: None,
            last_exit: None,
            healthy: None,
        });
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!((v["draining"][0]["id"].as_u64(), v["draining"][0]["state"].as_str()), (Some(2), Some("DRAINING")));
        let back: Status = serde_json::from_value(v).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn response_wire_format() {
        assert_eq!(serde_json::to_string(&Response::ok("done")).unwrap(), r#"{"ok":true,"message":"done"}"#);
        assert_eq!(
            serde_json::to_string(&Response::started("rolling", 4)).unwrap(),
            r#"{"ok":true,"message":"rolling","seq":4}"#
        );
        let r: Response = serde_json::from_str(r#"{"ok":false,"message":"no such worker"}"#).unwrap();
        assert!(!r.ok && r.message.as_deref() == Some("no such worker") && r.status.is_none());
    }

    #[test]
    fn status_from_an_older_supervisor_has_no_release() {
        let old = r#"{"app":"api","mode":"process","pid":1,"uptime_secs":0,"workers_configured":1,
            "workers_ready":1,"healthy":null,"supervisor_rss_bytes":null,"host":null,"reloading":false,
            "shutting_down":false,"workers":[]}"#;
        let s: Status = serde_json::from_str(old).unwrap();
        assert_eq!(s.release, None);
        let with = old.replace(r#""workers":[]"#, r#""workers":[],"release":"/srv/api/releases/v2""#);
        let s: Status = serde_json::from_str(&with).unwrap();
        assert_eq!(s.release.as_deref(), Some("/srv/api/releases/v2"));
    }
}

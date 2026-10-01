//! Worker slots and their lifecycle states.
//!
//! ```text
//! STARTING ─ready─► RUNNING ─stop─► STOPPING ─► STOPPED
//!    │                 │
//!    └──exit──► CRASHED ─► RESTARTING (backoff) ─► STARTING
//!                  └── too many restarts ─► FAILED
//! ```
//!
//! A slot is one supervised unit: in process mode one slot per worker process;
//! in worker mode a single slot holding the Bun host process, whose Workers
//! (threads) are tracked as `ThreadInfo`s on the instance.
//!
//! Hot standbys (`[workers] standby`, process mode) are instances outside the
//! slots (slot 0, `Role::Standby`): started, not listening. When a worker
//! dies one is promoted into its slot (`Role::Current`).

use crate::process::Handle;
use crate::restart::Tracker;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    Stopping,
    Stopped,
    Crashed,
    Restarting,
    Failed,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "STARTING",
            State::Running => "RUNNING",
            State::Stopping => "STOPPING",
            State::Stopped => "STOPPED",
            State::Crashed => "CRASHED",
            State::Restarting => "RESTARTING",
            State::Failed => "FAILED",
        }
    }
}

pub struct Slot {
    pub id: usize,
    pub state: State,
    /// Instance currently serving (or starting) for this slot.
    pub current: Option<u64>,
    pub tracker: Tracker,
    pub restarts: u64,
    pub crashes: u64,
    /// Bumped to invalidate pending restart timers.
    pub token: u64,
    pub last_exit: Option<String>,
    /// Scaled down: remove once its process has exited.
    pub removing: bool,
    /// When the slot became FAILED (for the cooldown retry).
    pub failed_at: Option<Instant>,
}

impl Slot {
    pub fn new(id: usize) -> Self {
        Slot {
            id,
            state: State::Stopped,
            current: None,
            tracker: Tracker::default(),
            restarts: 0,
            crashes: 0,
            token: 0,
            last_exit: None,
            removing: false,
            failed_at: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Serving (or starting to serve) for its slot.
    Current,
    /// Started next to the current instance during a reload / recovery.
    Replacement,
    /// Replaced; draining and exiting.
    Retiring,
    /// A hot standby: initialized, its listen deferred, outside the slots
    /// (`slot` is 0) until promoted.
    Standby,
}

/// The slot number of standbys (process-mode slots start at 1).
pub const STANDBY_SLOT: usize = 0;

/// A standby's way to "available": initialized, then the rollout gates a
/// new worker must pass (health checks, verify_command).
#[derive(Debug, Default, Clone)]
pub struct StandbyGates {
    /// Reported `standby_ready` (its listen is deferred).
    pub ready_at: Option<Instant>,
    /// Consecutive health checks passed / failed (`reload.health_passes`).
    pub passes: u32,
    pub fails: u32,
    /// A check or verify_command is running.
    pub checking: bool,
    pub verified: bool,
    /// Passed every gate: may be promoted.
    pub available: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadInfo {
    pub listening: bool,
    pub crashed: bool,
    pub crashes: u64,
    pub last_exit: Option<String>,
}

pub struct Instance {
    pub slot: usize,
    pub handle: Handle,
    pub started: Instant,
    pub ready_at: Option<Instant>,
    pub role: Role,
    /// We asked it to stop; its exit is not a crash.
    pub stopping: bool,
    /// Killed for missing `ready_timeout`.
    pub timed_out: bool,
    /// Stopped because it failed rollout gates with nothing to roll back to:
    /// once it has exited, restart the slot through crash handling.
    pub restart_on_exit: bool,
    /// Process mode: shim reported a listening server.
    pub listening: HashSet<u16>,
    /// Worker mode: Workers inside this host, by worker id.
    pub threads: BTreeMap<usize, ThreadInfo>,
    /// Last CPU sample for `status` (time, cpu seconds).
    pub cpu_prev: Option<(Instant, f64)>,
    /// Private health sockets reported by the shim, by worker id.
    pub sockets: BTreeMap<usize, PathBuf>,
    /// Last heartbeat per worker id (watchdog); armed by the first heartbeat.
    pub heartbeats: BTreeMap<usize, Instant>,
    /// Killed by the watchdog.
    pub hung: bool,
    /// Per-worker health: consecutive failures, verdict, check in flight.
    pub health_fails: u32,
    pub healthy: Option<bool>,
    pub health_inflight: bool,
    /// Consecutive memory samples over `limits.max_memory`.
    pub mem_strikes: u8,
    /// `limits.max_lifetime` (with jitter) deadline.
    pub recycle_at: Option<Instant>,
    /// `Role::Standby`: its gates.
    pub standby: Option<StandbyGates>,
    /// A standby promoted into a slot: when (until it listens, then too).
    pub promoted_at: Option<Instant>,
}

impl Instance {
    pub fn new(slot: usize, handle: Handle, role: Role) -> Self {
        Instance {
            standby: (role == Role::Standby).then(StandbyGates::default),
            promoted_at: None,
            slot,
            handle,
            started: Instant::now(),
            ready_at: None,
            role,
            stopping: false,
            timed_out: false,
            restart_on_exit: false,
            listening: HashSet::new(),
            threads: BTreeMap::new(),
            cpu_prev: None,
            sockets: BTreeMap::new(),
            heartbeats: BTreeMap::new(),
            hung: false,
            health_fails: 0,
            healthy: None,
            health_inflight: false,
            mem_strikes: 0,
            recycle_at: None,
        }
    }

    pub fn threads_listening(&self) -> usize {
        self.threads.values().filter(|t| t.listening && !t.crashed).count()
    }
}

/// Human-readable exit description.
pub fn describe_exit(code: Option<i32>, signal: Option<i32>) -> String {
    match (code, signal) {
        (Some(c), _) => format!("exit code {c}"),
        (None, Some(s)) => format!("signal {s} ({})", signal_name(s)),
        _ => "unknown exit".into(),
    }
}

fn signal_name(s: i32) -> &'static str {
    match s {
        libc::SIGKILL => "SIGKILL",
        libc::SIGTERM => "SIGTERM",
        libc::SIGINT => "SIGINT",
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGABRT => "SIGABRT",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGHUP => "SIGHUP",
        _ => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exits() {
        assert_eq!(describe_exit(Some(1), None), "exit code 1");
        assert_eq!(describe_exit(None, Some(9)), "signal 9 (SIGKILL)");
    }
}

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

/// The name of standby number `n` (`s1`, `s2`…): its row in `warden
/// status`, its `worker=` on log lines, `warden logs --worker s1`.
pub fn standby_label(n: usize) -> String {
    format!("s{n}")
}

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
    /// Its instance number (`[app] instance_var`) until promoted: past the
    /// workers' when it started, and no other standby's.
    pub instance: usize,
}

/// One worker's event-loop delay, from its heartbeats.
#[derive(Debug, Clone, Copy)]
pub struct LoopState {
    pub last: crate::control::LoopDelay,
    pub at: Instant,
    /// Heartbeats in a row with p99 at or above the warning threshold.
    pub high: u32,
    /// When the last "event loop delay is high" warning was logged.
    pub warned: Option<Instant>,
}

impl LoopState {
    pub fn new(d: crate::control::LoopDelay, now: Instant) -> Self {
        LoopState { last: d, at: now, high: 0, warned: None }
    }

    /// The last figure, while it is recent: a worker whose heartbeats
    /// stopped has none (the watchdog deals with it).
    pub fn current(&self, now: Instant) -> Option<crate::control::LoopDelay> {
        (now.saturating_duration_since(self.at) <= LOOP_DELAY_FRESH).then_some(self.last)
    }

    /// Take one heartbeat's figure. True when `[watchdog] loop_delay_warn`
    /// (`warn_ms`, 0 = off) should warn now: p99 at or above it for
    /// `LOOP_WARN_AFTER` heartbeats in a row, at most once per
    /// `LOOP_WARN_EVERY` for this worker.
    pub fn observe(&mut self, d: crate::control::LoopDelay, now: Instant, warn_ms: f64) -> bool {
        self.last = d;
        self.at = now;
        if warn_ms <= 0.0 || d.p99_ms < warn_ms {
            self.high = 0;
            return false;
        }
        self.high = self.high.saturating_add(1);
        let due = self.warned.is_none_or(|t| now.saturating_duration_since(t) >= LOOP_WARN_EVERY);
        if self.high >= crate::config::LOOP_WARN_AFTER && due {
            self.warned = Some(now);
            return true;
        }
        false
    }
}

/// How long a heartbeat's event-loop delay is shown: a few heartbeats.
pub const LOOP_DELAY_FRESH: std::time::Duration = std::time::Duration::from_secs(5);
/// One "event loop delay is high" warning per worker per this long.
pub const LOOP_WARN_EVERY: std::time::Duration = std::time::Duration::from_secs(600);

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
    /// The event-loop delay each worker id's last heartbeat carried, and how
    /// many heartbeats in a row it has been above `[watchdog] loop_delay_warn`.
    pub loop_delay: BTreeMap<usize, LoopState>,
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
    /// Started as a standby: its number in the pool (1..=standby; `sN` in
    /// `warden status` and on its log lines), kept after its promotion.
    pub standby_number: Option<usize>,
    /// A standby promoted into a slot: when (until it listens, then too).
    pub promoted_at: Option<Instant>,
    /// `Role::Standby`: the release (pinned working directory) it started
    /// in; it is promoted only while that is still the workers' release.
    pub release: Option<PathBuf>,
    /// The OOM kill counter of the cgroup it runs in (`exit::OomTracker`):
    /// Warden's own until it is ready, then its cgroup's (a worker may run
    /// in one of its own); `None`: no readable counter.
    pub oom_counter: Option<PathBuf>,
}

impl Instance {
    pub fn new(slot: usize, handle: Handle, role: Role) -> Self {
        Instance {
            standby: (role == Role::Standby).then(StandbyGates::default),
            standby_number: None,
            promoted_at: None,
            release: None,
            oom_counter: None,
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
            loop_delay: BTreeMap::new(),
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

    /// `loop_delay_warn`: one warning after LOOP_WARN_AFTER high heartbeats
    /// in a row, then none for LOOP_WARN_EVERY; a low one starts the count
    /// again; 0 turns it off; an old figure is not shown.
    #[test]
    fn loop_delay_warning_is_rate_limited() {
        use crate::control::LoopDelay;
        let (low, high) = (
            LoopDelay { p50_ms: 1.0, p99_ms: 5.0, max_ms: 9.0 },
            LoopDelay { p50_ms: 100.0, p99_ms: 600.0, max_ms: 900.0 },
        );
        let t0 = Instant::now();
        let mut s = LoopState::new(low, t0);
        let at = |i: u64| t0 + std::time::Duration::from_secs(i);
        let after = crate::config::LOOP_WARN_AFTER as u64;
        for i in 1..after {
            assert!(!s.observe(high, at(i), 500.0), "heartbeat {i}");
        }
        assert!(!s.observe(low, at(after), 500.0), "a low one resets the count");
        let every = LOOP_WARN_EVERY.as_secs();
        let warned: Vec<u64> =
            (after + 1..=2 * after + 2 * every + 1).filter(|i| s.observe(high, at(*i), 500.0)).collect();
        assert_eq!(warned, vec![2 * after, 2 * after + every, 2 * after + 2 * every], "once per {every} s");
        let mut off = LoopState::new(high, t0);
        assert!((1..100).all(|i| !off.observe(high, at(i), 0.0)), "0 = off");
        assert_eq!(off.current(at(99)), Some(high));
        assert_eq!(off.current(at(99) + LOOP_DELAY_FRESH + std::time::Duration::from_millis(1)), None);
    }

    #[test]
    fn exits() {
        assert_eq!(describe_exit(Some(1), None), "exit code 1");
        assert_eq!(describe_exit(None, Some(9)), "signal 9 (SIGKILL)");
    }
}

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
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
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

/// Responses counted a second at a time, for a minute: the rate over the
/// last ten seconds, the last minute, and everything since it began.
#[derive(Debug, Clone)]
pub struct Window {
    slots: [crate::control::Responses; 60],
    /// The second (counted from the supervisor's start) of the newest slot.
    at: u64,
    total: crate::control::Responses,
}

impl Default for Window {
    fn default() -> Self {
        Window { slots: [crate::control::Responses::default(); 60], at: 0, total: Default::default() }
    }
}

impl Window {
    /// Move to second `sec`: the slots of the seconds since `at` start empty.
    fn advance(&mut self, sec: u64) {
        if sec <= self.at {
            return;
        }
        for s in (self.at + 1..=sec).rev().take(60) {
            self.slots[(s % 60) as usize] = Default::default();
        }
        self.at = sec;
    }

    /// Count `d`, made in second `sec`.
    pub fn add(&mut self, sec: u64, d: &crate::control::Responses) {
        self.advance(sec);
        // A second already gone by (out of order) goes into the newest one.
        self.slots[(self.at % 60) as usize].add(d);
        self.total.add(d);
    }

    /// The figures as of second `sec`.
    pub fn stats(&self, sec: u64) -> crate::control::RequestStats {
        let mut minute = crate::control::Responses::default();
        let mut ten = 0;
        for back in 0..60u64 {
            let Some(s) = sec.checked_sub(back) else { break };
            if s > self.at || self.at - s >= 60 {
                continue;
            }
            let slot = &self.slots[(s % 60) as usize];
            minute.add(slot);
            if back < 10 {
                ten += slot.total();
            }
        }
        crate::control::RequestStats { rate: ten as f64 / 10.0, minute, total: self.total }
    }
}

/// A worker's response counts: the last cumulative figure of each of its
/// reporters (a Worker thread in worker mode), and its window.
#[derive(Debug, Clone, Default)]
pub struct Requests {
    pub last: BTreeMap<usize, crate::control::Responses>,
    pub window: Window,
}

/// What a worker was seen listening on, for `[watchdog] port_lost`: the
/// kernel's view of its process tree, looked at every two seconds.
#[derive(Debug, Clone, Default)]
pub struct ListenWatch {
    /// The TCP listening sockets (inodes) last found in its tree; empty where
    /// the OS gives no inodes (then the tree is walked each time).
    pub inodes: Vec<u64>,
    /// Their ports, for the log line.
    pub ports: Vec<u16>,
    /// When it was last seen listening: `None` until it first is, and a
    /// worker that never listened is not watched.
    pub seen_at: Option<Instant>,
    /// Since when it has been seen listening on nothing.
    pub lost_since: Option<Instant>,
    /// Not seen listening yet: when to walk its tree again (more seldom as
    /// it ages: a worker without ports would be walked for nothing).
    pub next_walk: Option<Instant>,
}

#[derive(Debug, Clone, Default)]
pub struct ThreadInfo {
    pub listening: bool,
    pub crashed: bool,
    pub crashes: u64,
    pub last_exit: Option<String>,
}

pub struct Instance {
    /// Takes its connections from Warden (`crate::handoff`), not the port.
    pub handoff: bool,
    /// ...as a bare descriptor (`crate::handoff::Kind::Fd`).
    pub adopt: bool,
    /// The address the app asked to listen on, with the handoff.
    pub handoff_host: Option<String>,
    /// What the keeper was last told of it (`crate::keeper::Meta`).
    pub kept_meta: Option<crate::keeper::Meta>,
    pub slot: usize,
    pub handle: Handle,
    pub started: Instant,
    pub ready_at: Option<Instant>,
    /// Set once it is ready: the poll for its port (`watch_readiness`) stops
    /// then, or when the instance is gone (it holds only a weak reference).
    pub ready_flag: Arc<AtomicBool>,
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
    /// `[watchdog] port_lost`: what its process tree was last seen listening on.
    pub listen: ListenWatch,
    /// Stopped for having stopped listening: its exit is restarted as a crash.
    pub port_lost: bool,
    /// The responses it sent, by status, when it reports them (heartbeats).
    pub requests: Option<Requests>,
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
            handoff: false,
            adopt: false,
            handoff_host: None,
            kept_meta: None,
            standby: (role == Role::Standby).then(StandbyGates::default),
            standby_number: None,
            promoted_at: None,
            release: None,
            oom_counter: None,
            slot,
            handle,
            started: Instant::now(),
            ready_at: None,
            ready_flag: Arc::default(),
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
            listen: ListenWatch::default(),
            port_lost: false,
            requests: None,
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

    /// The window counts a second at a time: the rate is the last ten
    /// seconds, the minute the last sixty, and the total everything; seconds
    /// without a heartbeat count nothing, and a minute later it is all gone
    /// but the total.
    #[test]
    fn a_window_keeps_a_minute_of_seconds() {
        use crate::control::Responses;
        let ok = |n| Responses { ok: n, ..Responses::default() };
        let mut w = Window::default();
        assert_eq!(w.stats(0).rate, 0.0);
        for sec in 100..110 {
            w.add(sec, &ok(5));
        }
        w.add(110, &Responses { server_error: 2, ..Responses::default() });
        let s = w.stats(110);
        assert_eq!((s.rate, s.minute.ok, s.minute.server_error, s.total.total()), (4.7, 50, 2, 52));
        // Ten quiet seconds later: no rate, the minute is still there.
        let s = w.stats(120);
        assert_eq!((s.rate, s.minute.total()), (0.0, 52));
        // A minute after the last count: only the total.
        let s = w.stats(171);
        assert_eq!((s.minute.total(), s.total.total()), (0, 52));
        // Counting again after a gap of more than a minute starts from empty slots.
        w.add(300, &ok(1));
        let s = w.stats(300);
        assert_eq!((s.minute.total(), s.total.total(), s.rate), (1, 53, 0.1));
        // A late count (an older second) goes into the newest one.
        w.add(250, &ok(1));
        assert_eq!(w.stats(300).minute.total(), 2);
    }

    #[test]
    fn exits() {
        assert_eq!(describe_exit(Some(1), None), "exit code 1");
        assert_eq!(describe_exit(None, Some(9)), "signal 9 (SIGKILL)");
    }
}

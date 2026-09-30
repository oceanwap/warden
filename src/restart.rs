//! Crash-loop protection: exponential backoff plus a cap on restarts per window.
//!
//! The first crash after a healthy run restarts at once (a one-off crash
//! should cost only the app's startup time); each further crash in a row
//! waits `backoff_initial`, doubling up to `backoff_max`.

use crate::config;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Policy {
    pub enabled: bool,
    pub max_restarts: u32,
    pub window: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl From<&config::Restart> for Policy {
    fn from(r: &config::Restart) -> Self {
        Policy {
            enabled: r.enabled,
            max_restarts: r.max_restarts,
            window: Duration::from_secs(r.restart_window),
            backoff_initial: Duration::from_millis(r.backoff_initial),
            backoff_max: Duration::from_millis(r.backoff_max),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    RestartAfter(Duration),
    /// Restarts disabled, or too many restarts inside the window.
    GiveUp,
}

#[derive(Debug, Default, Clone)]
pub struct Tracker {
    history: VecDeque<Instant>,
    consecutive: u32,
}

impl Tracker {
    /// Record a crash of a worker that had been up for `uptime` and decide what to do.
    pub fn on_crash(&mut self, p: &Policy, now: Instant, uptime: Duration) -> Decision {
        if !p.enabled {
            return Decision::GiveUp;
        }
        // A worker that stayed up for a whole window was healthy: start backoff over.
        if uptime >= p.window {
            self.consecutive = 0;
        }
        while self.history.front().is_some_and(|t| now.duration_since(*t) >= p.window) {
            self.history.pop_front();
        }
        if self.history.len() >= p.max_restarts as usize {
            return Decision::GiveUp;
        }
        self.consecutive += 1;
        self.history.push_back(now);
        if self.consecutive == 1 {
            return Decision::RestartAfter(Duration::ZERO);
        }
        Decision::RestartAfter(backoff(p, self.consecutive - 1))
    }

    pub fn reset(&mut self) {
        self.history.clear();
        self.consecutive = 0;
    }

    pub fn restarts_in_window(&self) -> usize {
        self.history.len()
    }
}

/// `now + d` that cannot panic: values are bounded by config validation, and
/// anything that would still overflow lands a year out instead.
pub fn later(now: Instant, d: Duration) -> Instant {
    now.checked_add(d).unwrap_or_else(|| now + Duration::from_secs(365 * 86_400))
}

/// `initial * 2^(n-1)`, capped at `max`.
pub fn backoff(p: &Policy, n: u32) -> Duration {
    let shift = n.saturating_sub(1).min(31);
    let ms = (p.backoff_initial.as_millis() as u64).saturating_mul(1u64 << shift);
    Duration::from_millis(ms).min(p.backoff_max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> Policy {
        Policy {
            enabled: true,
            max_restarts: 4,
            window: Duration::from_secs(60),
            backoff_initial: Duration::from_millis(100),
            backoff_max: Duration::from_millis(1000),
        }
    }

    #[test]
    fn exponential_and_capped() {
        let p = policy();
        let got: Vec<u64> = (1..=6).map(|n| backoff(&p, n).as_millis() as u64).collect();
        assert_eq!(got, vec![100, 200, 400, 800, 1000, 1000]);
        assert_eq!(backoff(&p, 200), p.backoff_max);
    }

    #[test]
    fn gives_up_after_max_restarts_in_window() {
        let p = policy();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let short = Duration::from_millis(10);
        for i in 0..4 {
            assert!(matches!(t.on_crash(&p, t0 + Duration::from_secs(i), short), Decision::RestartAfter(_)));
        }
        assert_eq!(t.on_crash(&p, t0 + Duration::from_secs(5), short), Decision::GiveUp);
    }

    #[test]
    fn window_slides() {
        let p = policy();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let short = Duration::from_millis(10);
        for i in 0..4 {
            t.on_crash(&p, t0 + Duration::from_secs(i), short);
        }
        // 61 s after the first crash, one slot has freed up.
        assert!(matches!(t.on_crash(&p, t0 + Duration::from_secs(61), short), Decision::RestartAfter(_)));
    }

    #[test]
    fn long_uptime_resets_backoff() {
        let p = policy();
        let mut t = Tracker::default();
        let t0 = Instant::now();
        let short = Duration::from_millis(10);
        assert_eq!(t.on_crash(&p, t0, short), Decision::RestartAfter(Duration::ZERO), "first crash: at once");
        assert_eq!(t.on_crash(&p, t0, short), Decision::RestartAfter(Duration::from_millis(100)));
        assert_eq!(t.on_crash(&p, t0, short), Decision::RestartAfter(Duration::from_millis(200)));
        let later = t0 + Duration::from_secs(200);
        assert_eq!(t.on_crash(&p, later, Duration::from_secs(120)), Decision::RestartAfter(Duration::ZERO));
    }

    #[test]
    fn later_never_panics() {
        let now = Instant::now();
        assert!(later(now, Duration::MAX) > now);
        assert_eq!(later(now, Duration::from_secs(5)), now + Duration::from_secs(5));
    }

    #[test]
    fn disabled_never_restarts() {
        let mut p = policy();
        p.enabled = false;
        assert_eq!(Tracker::default().on_crash(&p, Instant::now(), Duration::ZERO), Decision::GiveUp);
    }
}

//! wardend's restart policy and how an app's state is derived: plain
//! functions of time and observations, so they are tested here without
//! processes or sockets.

use crate::control::Status;
use crate::events::AppState;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Policy {
    /// First restart delay; it doubles with each death, up to `max`.
    pub initial: Duration,
    pub max: Duration,
    /// Up this long: the next death starts from `initial` again.
    pub reset_after: Duration,
    /// This many deaths within `give_up_window`: stop restarting.
    pub give_up_deaths: usize,
    pub give_up_window: Duration,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
            reset_after: Duration::from_secs(300),
            give_up_deaths: 10,
            give_up_window: Duration::from_secs(600),
        }
    }
}

/// The policy. Debug builds only (like `WARDEN_FAULT`): tests shorten it
/// with `WARDEN_DAEMON_POLICY=initial_ms=100,max_ms=400,reset_ms=60000,deaths=3,window_ms=60000`.
pub(crate) fn from_env() -> Policy {
    #[cfg(debug_assertions)]
    if let Ok(spec) = std::env::var("WARDEN_DAEMON_POLICY") {
        return parse_overrides(&spec, Policy::default());
    }
    Policy::default()
}

#[cfg(any(debug_assertions, test))]
fn parse_overrides(spec: &str, mut p: Policy) -> Policy {
    for item in spec.split(',') {
        let Some((k, v)) = item.split_once('=') else { continue };
        let Ok(n) = v.trim().parse::<u64>() else { continue };
        let ms = Duration::from_millis(n);
        match k.trim() {
            "initial_ms" => p.initial = ms,
            "max_ms" => p.max = ms,
            "reset_ms" => p.reset_after = ms,
            "deaths" => p.give_up_deaths = usize::try_from(n).unwrap_or(usize::MAX).max(1),
            "window_ms" => p.give_up_window = ms,
            _ => {}
        }
    }
    p
}

/// What to do about a supervisor that died.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Verdict {
    Restart(Duration),
    GiveUp { deaths: usize },
}

/// One app's death history.
#[derive(Debug, Default)]
pub(crate) struct Backoff {
    /// Delay before the next restart; `None`: `initial`.
    next: Option<Duration>,
    /// Deaths within the give-up window, oldest first.
    deaths: VecDeque<Instant>,
}

impl Backoff {
    /// It died at `now` after being up for `uptime` (if known).
    pub fn on_death(&mut self, p: &Policy, now: Instant, uptime: Option<Duration>) -> Verdict {
        while self.deaths.front().is_some_and(|t| now.saturating_duration_since(*t) >= p.give_up_window) {
            self.deaths.pop_front();
        }
        self.deaths.push_back(now);
        while self.deaths.len() > p.give_up_deaths {
            self.deaths.pop_front();
        }
        if self.deaths.len() >= p.give_up_deaths {
            return Verdict::GiveUp { deaths: self.deaths.len() };
        }
        if uptime.is_some_and(|u| u >= p.reset_after) {
            self.next = None;
        }
        let delay = self.next.unwrap_or(p.initial).min(p.max);
        self.next = Some(delay.saturating_mul(2).min(p.max));
        Verdict::Restart(delay)
    }

    /// A human started it again (`warden start`): a fresh history.
    pub fn clear(&mut self) {
        *self = Backoff::default();
    }
}

/// Where wardend is with one app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Not running (never seen, or died with nobody for wardend to restart).
    Idle,
    /// A running supervisor, watched.
    Watching,
    /// wardend started it and waits for its control socket.
    Spawned,
    /// It died; wardend starts it again after the backoff.
    Backoff,
    /// It exited on purpose (`bye`, `warden kill`, `warden delete`).
    Exited,
    /// It died too often; wardend stopped restarting it.
    GaveUp,
}

/// The state an `AppEntry` shows.
pub(crate) fn state(phase: Phase, status: Option<&Status>, unresponsive: bool) -> AppState {
    match phase {
        Phase::GaveUp => AppState::GaveUp,
        Phase::Spawned | Phase::Backoff => AppState::Starting,
        Phase::Exited => AppState::Stopped,
        Phase::Idle => AppState::NotStarted,
        Phase::Watching if unresponsive => AppState::Unreachable,
        Phase::Watching => match status {
            Some(s) if s.stopped || s.shutting_down => AppState::Stopped,
            _ => AppState::Running,
        },
    }
}

/// Who restarts a dead supervisor, from `Status.launched`.
pub(crate) fn supervised_by(launched: Option<&str>) -> &'static str {
    match launched {
        Some("systemd") => "systemd",
        Some("terminal") => "terminal",
        _ => "wardend",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn delays(b: &mut Backoff, p: &Policy, t0: Instant, n: usize, every: Duration) -> Vec<u64> {
        (0..n)
            .map(|i| match b.on_death(p, t0 + every * i as u32, Some(secs(1))) {
                Verdict::Restart(d) => d.as_secs(),
                Verdict::GiveUp { .. } => 0,
            })
            .collect()
    }

    #[test]
    fn backoff_doubles_up_to_a_minute_then_resets_after_five_minutes_up() {
        // Deaths an hour apart never reach the give-up window.
        let p = Policy::default();
        let (mut b, t0) = (Backoff::default(), Instant::now());
        assert_eq!(delays(&mut b, &p, t0, 9, secs(3600)), vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
        // Five minutes up: back to 1 s, then doubling again.
        let later = t0 + secs(100_000);
        assert_eq!(b.on_death(&p, later, Some(secs(300))), Verdict::Restart(secs(1)));
        assert_eq!(b.on_death(&p, later + secs(3600), Some(secs(299))), Verdict::Restart(secs(2)));
        // An unknown uptime does not reset it.
        assert_eq!(b.on_death(&p, later + secs(7200), None), Verdict::Restart(secs(4)));
        b.clear();
        assert_eq!(b.on_death(&p, later + secs(9000), None), Verdict::Restart(secs(1)));
    }

    #[test]
    fn gives_up_after_ten_deaths_in_ten_minutes_only() {
        let p = Policy::default();
        let t0 = Instant::now();
        // Ten deaths in nine minutes: the tenth gives up.
        let mut b = Backoff::default();
        for i in 0..9 {
            assert!(matches!(b.on_death(&p, t0 + secs(60 * i), None), Verdict::Restart(_)), "death {i}");
        }
        assert_eq!(b.on_death(&p, t0 + secs(540), None), Verdict::GiveUp { deaths: 10 });
        // Deaths 70 s apart: never ten within any ten minutes.
        let mut b = Backoff::default();
        for i in 0..40 {
            assert!(matches!(b.on_death(&p, t0 + secs(70 * i), None), Verdict::Restart(_)), "death {i}");
        }
        // `warden start` clears the history.
        let mut b = Backoff::default();
        for i in 0..9 {
            b.on_death(&p, t0 + secs(i), None);
        }
        b.clear();
        assert!(matches!(b.on_death(&p, t0 + secs(10), None), Verdict::Restart(_)));
    }

    #[test]
    fn test_overrides_parse() {
        let p =
            parse_overrides("initial_ms=100,max_ms=400,deaths=3,window_ms=60000,bogus=1,reset_ms=x", Policy::default());
        assert_eq!(p.initial, Duration::from_millis(100));
        assert_eq!(p.max, Duration::from_millis(400));
        assert_eq!((p.give_up_deaths, p.give_up_window), (3, secs(60)));
        assert_eq!(p.reset_after, secs(300), "unparsable values are ignored");
        assert_eq!(parse_overrides("deaths=0", Policy::default()).give_up_deaths, 1);
        let mut b = Backoff::default();
        let t0 = Instant::now();
        let v: Vec<Verdict> = (0..3).map(|i| b.on_death(&p, t0 + secs(i), None)).collect();
        assert_eq!(
            v,
            vec![
                Verdict::Restart(Duration::from_millis(100)),
                Verdict::Restart(Duration::from_millis(200)),
                Verdict::GiveUp { deaths: 3 }
            ]
        );
    }

    pub(crate) fn status(stopped: bool, shutting_down: bool) -> Status {
        Status {
            app: "api".into(),
            namespace: "default".into(),
            mode: "process".into(),
            config_path: None,
            unit: None,
            launched: "background".into(),
            stopped,
            log_file: None,
            version: "0".into(),
            pid: 42,
            uptime_secs: 1,
            workers_configured: 1,
            workers_ready: 1,
            healthy: None,
            supervisor_rss_bytes: None,
            host: None,
            reloading: false,
            shutting_down,
            health_suspended: false,
            log_lines_dropped: 0,
            rollout: None,
            last_rollout: None,
            workers: vec![],
            release: None,
            standbys: vec![],
        }
    }

    #[test]
    fn app_state_derivation() {
        let up = status(false, false);
        assert_eq!(state(Phase::Watching, Some(&up), false), AppState::Running);
        assert_eq!(state(Phase::Watching, None, false), AppState::Running);
        assert_eq!(state(Phase::Watching, Some(&up), true), AppState::Unreachable);
        assert_eq!(state(Phase::Watching, Some(&status(true, false)), false), AppState::Stopped);
        assert_eq!(state(Phase::Watching, Some(&status(false, true)), false), AppState::Stopped);
        assert_eq!(state(Phase::Spawned, None, false), AppState::Starting);
        assert_eq!(state(Phase::Backoff, Some(&up), false), AppState::Starting);
        assert_eq!(state(Phase::Exited, Some(&up), false), AppState::Stopped);
        assert_eq!(state(Phase::GaveUp, None, true), AppState::GaveUp);
        assert_eq!(state(Phase::Idle, None, false), AppState::NotStarted);
        assert_eq!(supervised_by(Some("systemd")), "systemd");
        assert_eq!(supervised_by(Some("terminal")), "terminal");
        assert_eq!(supervised_by(Some("background")), "wardend");
        assert_eq!(supervised_by(None), "wardend");
    }
}

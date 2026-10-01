//! Text for people: event lines as `warden events` prints them, and the
//! numbers of `warden status` (sizes, durations, CPU).

use warden_protocol::control::Status;
use warden_protocol::events::{AppEntry, Event, SupervisorEvent};

/// `12:00:01`, local time (as `warden events` prints it).
pub fn clock(at_ms: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(at_ms.min(i64::MAX as u64) as i64).single() {
        Some(t) => t.format("%H:%M:%S").to_string(),
        None => "--:--:--".into(),
    }
}

/// `40.1 MB`, `1.25 GB` (as `warden status`).
pub fn bytes(b: u64) -> String {
    let mb = b as f64 / (1024.0 * 1024.0);
    if mb >= 1024.0 { format!("{:.2} GB", mb / 1024.0) } else { format!("{mb:.1} MB") }
}

/// `45s`, `3m07s`, `2h05m`, `4d03h` (as `warden status`).
pub fn duration(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, (secs % 86_400) / 3600, (secs % 3600) / 60, secs % 60);
    match (d, h, m) {
        (0, 0, 0) => format!("{s}s"),
        (0, 0, _) => format!("{m}m{s:02}s"),
        (0, _, _) => format!("{h}h{m:02}m"),
        _ => format!("{d}d{h:02}h"),
    }
}

pub fn percent(p: f64) -> String {
    format!("{p:.1}%")
}

/// `0.41ms`, `12.3ms`, `250ms`, `1.20s` (as `warden list` shows event-loop delay).
pub fn millis(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.2}ms")
    } else if ms < 100.0 {
        format!("{ms:.1}ms")
    } else if ms < 1000.0 {
        format!("{ms:.0}ms")
    } else {
        format!("{:.2}s", ms / 1000.0)
    }
}

pub fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> String) -> String {
    v.map(f).unwrap_or_else(|| "-".into())
}

/// A worker's health column: `ok`, `FAIL` or `-` (no health checks).
pub fn health(h: Option<bool>) -> &'static str {
    match h {
        Some(true) => "ok",
        Some(false) => "FAIL",
        None => "-",
    }
}

/// `warden events` spells `gave_up` as two words.
pub fn supervisor_event(e: SupervisorEvent) -> &'static str {
    match e {
        SupervisorEvent::GaveUp => "gave up",
        other => other.as_str(),
    }
}

/// `1/2 workers ready (1 RUNNING, 1 STARTING), stopped`: what `warden
/// events` prints for a status (it prints it only when this text changes).
pub fn status_summary(s: &Status) -> String {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for w in &s.workers {
        match counts.iter_mut().find(|(k, _)| *k == w.state) {
            Some((_, n)) => *n += 1,
            None => counts.push((&w.state, 1)),
        }
    }
    let mut t = format!("{}/{} workers ready", s.workers_ready, s.workers_configured);
    if counts.len() > 1 || counts.first().is_some_and(|(k, _)| *k != "RUNNING") {
        let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
        t += &format!(" ({})", parts.join(", "));
    }
    if let Some(reason) = &s.start_failed {
        // Every worker crashed before one was ready (`warden list`: errored).
        t += &format!(", failed to start ({reason})");
    }
    if s.stopped {
        t += ", stopped";
    }
    if s.shutting_down {
        t += ", shutting down";
    }
    t
}

/// `api running (supervised by wardend, pid 4208, 2 restarts): <problem>`,
/// without the clock.
pub fn app_state(a: &AppEntry) -> String {
    let mut t = format!("{} {} (supervised by {}", a.name, a.state.as_str(), a.supervised_by);
    if let Some(p) = a.supervisor_pid {
        t += &format!(", pid {p}");
    }
    if a.supervisor_restarts > 0 {
        t += &format!(", {} restarts", a.supervisor_restarts);
    }
    t += ")";
    if let Some(p) = &a.problem {
        t += &format!(": {p}");
    }
    t
}

/// One line for an event, as `warden events` prints it; `None` for the
/// events it does not print as such (`status`, `apps`, `host`, `log`).
/// `now_ms`: the clock for events that carry no time of their own.
pub fn event_line(ev: &Event, now_ms: u64) -> Option<String> {
    let now = clock(now_ms);
    Some(match ev {
        Event::Hello { app: None, pid, version, .. } => format!("{now} wardend pid={pid} version={version}"),
        Event::Hello { app: Some(app), pid, .. } => format!("{now} {app} connected pid={pid}"),
        Event::Worker { app, worker, event, pid, detail, at_ms } => {
            let mut t = format!("{} {app} worker {worker} {}", clock(*at_ms), event.as_str());
            if let Some(p) = pid {
                t += &format!(" pid={p}");
            }
            if let Some(d) = detail {
                t += &format!(" {d}");
            }
            t
        }
        Event::Rollout { app, rollout: r } => format!("{now} {app} {} {}/{}: {}", r.kind, r.done, r.total, r.phase),
        Event::RolloutDone { app, outcome: o } => format!(
            "{now} {app} {} {}: {} ({:.1} s)",
            o.kind,
            if o.ok { "done" } else { "FAILED" },
            o.message,
            o.duration_secs
        ),
        Event::Lagged { app, dropped } => format!(
            "{now} {}: {dropped} events skipped (this window is slower than the events)",
            app.as_deref().unwrap_or("events")
        ),
        Event::Supervisor { app, event, pid, detail, at_ms } => {
            let mut t = format!("{} {app} supervisor {}", clock(*at_ms), supervisor_event(*event));
            if let Some(p) = pid {
                t += &format!(" pid={p}");
            }
            if let Some(d) = detail {
                t += &format!(" {d}");
            }
            t
        }
        Event::Bye { app: None, reason } => format!("{now} wardend exiting ({reason})"),
        Event::Bye { app: Some(app), reason } => format!("{now} {app} supervisor exiting ({reason})"),
        Event::Status { .. } | Event::Apps { .. } | Event::Host { .. } | Event::Log { .. } => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_protocol::control::{RolloutOutcome, RolloutStatus, WorkerStatus};
    use warden_protocol::events::{AppState, WorkerEvent};

    /// The clock part is local time: compare what follows it.
    fn after_clock(s: &str) -> &str {
        &s[9..]
    }

    #[test]
    fn numbers_match_warden_status() {
        assert_eq!(bytes(64 * 1024 * 1024), "64.0 MB");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.50 GB");
        assert_eq!(duration(45), "45s");
        assert_eq!(duration(187), "3m07s");
        assert_eq!(duration(7500), "2h05m");
        assert_eq!(duration(4 * 86_400 + 3 * 3600), "4d03h");
        assert_eq!(percent(12.345), "12.3%");
        assert_eq!(
            (millis(0.414), millis(12.34), millis(250.4), millis(1200.0)),
            ("0.41ms".to_string(), "12.3ms".to_string(), "250ms".to_string(), "1.20s".to_string())
        );
        assert_eq!(opt(None::<u64>, bytes), "-");
        assert_eq!((health(Some(true)), health(Some(false)), health(None)), ("ok", "FAIL", "-"));
        assert_eq!(clock(0).len(), 8);
    }

    #[test]
    fn event_lines_match_warden_events() {
        let w = Event::Worker {
            app: "api".into(),
            worker: 2,
            event: WorkerEvent::Crashed,
            pid: Some(4230),
            detail: Some("exit code 3".into()),
            at_ms: 1,
        };
        assert_eq!(after_clock(&event_line(&w, 0).unwrap()), "api worker 2 crashed pid=4230 exit code 3");
        let r = Event::Rollout {
            app: "api".into(),
            rollout: RolloutStatus {
                seq: 1,
                kind: "reload".into(),
                phase: "replacing worker 1".into(),
                done: 0,
                total: 2,
                elapsed_secs: 0,
            },
        };
        assert_eq!(after_clock(&event_line(&r, 0).unwrap()), "api reload 0/2: replacing worker 1");
        let d = Event::RolloutDone {
            app: "api".into(),
            outcome: RolloutOutcome {
                seq: 1,
                kind: "reload".into(),
                ok: false,
                message: "rolled back".into(),
                duration_secs: 2.04,
            },
        };
        assert_eq!(after_clock(&event_line(&d, 0).unwrap()), "api reload FAILED: rolled back (2.0 s)");
        let s = Event::Supervisor {
            app: "api".into(),
            event: SupervisorEvent::GaveUp,
            pid: None,
            detail: Some("died 10 times".into()),
            at_ms: 1,
        };
        assert_eq!(after_clock(&event_line(&s, 0).unwrap()), "api supervisor gave up died 10 times");
        let h = Event::Hello { protocol: 1, app: None, pid: 7, version: "0.1.0".into() };
        assert_eq!(after_clock(&event_line(&h, 0).unwrap()), "wardend pid=7 version=0.1.0");
        let bye = Event::Bye { app: None, reason: "SIGTERM".into() };
        assert_eq!(after_clock(&event_line(&bye, 0).unwrap()), "wardend exiting (SIGTERM)");
        assert!(event_line(&Event::Log { app: "api".into(), line: "x".into() }, 0).is_none());
    }

    #[test]
    fn status_and_state_summaries() {
        let worker = |state: &str| WorkerStatus {
            id: 1,
            state: state.into(),
            pid: None,
            uptime_secs: None,
            restarts: 0,
            crashes: 0,
            rss_bytes: None,
            cpu_seconds: None,
            cpu_percent: None,
            last_exit: None,
            healthy: None,
            loop_delay: None,
        };
        let mut s = crate::model::tests::status("api", 2);
        assert_eq!(status_summary(&s), "2/2 workers ready");
        s.workers = vec![worker("RUNNING"), worker("STARTING")];
        s.workers_ready = 1;
        s.stopped = true;
        assert_eq!(status_summary(&s), "1/2 workers ready (1 RUNNING, 1 STARTING), stopped");
        s.start_failed = Some("exit code 3".into());
        assert!(status_summary(&s).ends_with(", failed to start (exit code 3), stopped"));
        s.start_failed = None;
        let a = AppEntry {
            name: "api".into(),
            namespace: String::new(),
            config: None,
            socket: "/s".into(),
            state: AppState::GaveUp,
            supervised_by: "wardend".into(),
            supervisor_pid: None,
            supervisor_restarts: 10,
            status: None,
            problem: Some("`warden start api` clears it".into()),
        };
        assert_eq!(app_state(&a), "api gave_up (supervised by wardend, 10 restarts): `warden start api` clears it");
    }
}

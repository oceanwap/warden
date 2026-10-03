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

/// Responses per second: `0`, `0.4/s`, `12.3/s`, `1234/s` (as `warden list`).
pub fn per_second(r: f64) -> String {
    if r == 0.0 {
        "0/s".into()
    } else if r < 100.0 {
        format!("{r:.1}/s")
    } else {
        format!("{r:.0}/s")
    }
}

/// The errors of a minute, worst first: `3 5xx · 4 4xx`, `4 404s`, or `None`.
pub fn errors(m: &warden_protocol::control::Responses) -> Option<String> {
    let mut parts = Vec::new();
    if m.server_error > 0 {
        parts.push(format!("{} 5xx", m.server_error));
    }
    if m.client_error > 0 {
        // A static site's 4xx are nearly all 404s: say so when they all are.
        parts.push(if m.not_found == m.client_error {
            format!("{} 404{}", m.not_found, if m.not_found == 1 { "" } else { "s" })
        } else {
            format!("{} 4xx", m.client_error)
        });
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
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
        Event::Worker { app, worker, standby, event, pid, detail, at_ms } => {
            // A hot standby is `s1` (the pool `standby`), as `warden status` names it.
            let who = warden_protocol::events::worker_name(*worker, *standby);
            let mut t = format!("{} {app} worker {who} {}", clock(*at_ms), event.as_str());
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

/// How an event line reads, for its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Good,
    Warn,
    Bad,
}

/// A feed line taken apart: its clock (`12:00:01`) and its text, without the
/// app's name when `app` is the one the list is of (`warden events` prints
/// it because it mixes apps; one app's list does not need it on every line).
pub fn split_event<'a>(line: &'a str, app: &str) -> (Option<&'a str>, &'a str) {
    let (time, rest) = match line.split_once(' ') {
        Some((t, r)) if t.len() == 8 && t.as_bytes().get(2) == Some(&b':') && t.as_bytes().get(5) == Some(&b':') => {
            (Some(t), r)
        }
        _ => (None, line),
    };
    let rest = match rest.strip_prefix(app) {
        Some(r) if !app.is_empty() && r.starts_with(' ') => r.trim_start(),
        _ => rest,
    };
    (time, rest)
}

/// What an event line is about, by its words: a crash is bad, a restart or a
/// stop is a warning, a ready worker is good.
pub fn severity(text: &str) -> Severity {
    let mut worst = Severity::Info;
    for word in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let s = match word.to_ascii_lowercase().as_str() {
            "crashed" | "failed" | "unreachable" | "died" | "killed" | "oom" | "gave_up" => Severity::Bad,
            "restarting" | "starting" | "stopped" | "stopping" | "exiting" | "skipped" | "draining" | "lagged" => {
                Severity::Warn
            }
            "ready" | "running" | "done" | "connected" => Severity::Good,
            _ => Severity::Info,
        };
        worst = match (worst, s) {
            (Severity::Bad, _) | (_, Severity::Bad) => Severity::Bad,
            (Severity::Warn, _) | (_, Severity::Warn) => Severity::Warn,
            (Severity::Good, _) | (_, Severity::Good) => Severity::Good,
            _ => Severity::Info,
        };
    }
    // "gave up" is two words in `warden events`.
    if text.contains("gave up") { Severity::Bad } else { worst }
}

/// What a worker listens on, for a table cell: `3000, 9229, +1 socket`
/// (a port on `0.0.0.0` and `::` is one).
pub fn ports_cell(listening: &[warden_protocol::control::Listener]) -> String {
    use warden_protocol::control::Listener;
    let mut ports: Vec<u16> = listening
        .iter()
        .filter_map(|l| match l {
            Listener::Tcp { port, .. } => Some(*port),
            _ => None,
        })
        .collect();
    ports.sort_unstable();
    ports.dedup();
    let sockets = listening.iter().filter(|l| matches!(l, Listener::Unix { .. })).count();
    let mut parts: Vec<String> = ports.iter().map(u16::to_string).collect();
    match sockets {
        0 => {}
        1 => parts.push("+1 socket".into()),
        n => parts.push(format!("+{n} sockets")),
    }
    if parts.is_empty() { "-".into() } else { parts.join(", ") }
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
    fn a_feed_line_loses_its_clock_and_its_own_app_name() {
        assert_eq!(
            split_event("12:00:01 api worker 2 crashed pid=4230", "api"),
            (Some("12:00:01"), "worker 2 crashed pid=4230")
        );
        // Another app's name stays (the host feed mixes apps); a name that merely starts the same does too.
        assert_eq!(split_event("12:00:01 web worker 1 ready", "api").1, "web worker 1 ready");
        assert_eq!(split_event("12:00:01 api2 worker 1 ready", "api").1, "api2 worker 1 ready");
        assert_eq!(split_event("no clock here", "api"), (None, "no clock here"));
        assert_eq!(split_event("12:00:01", "api"), (None, "12:00:01"));
        assert_eq!(split_event("12:00:01 api", "api").1, "api");
    }

    #[test]
    fn severity_by_the_words_of_the_line() {
        assert_eq!(severity("worker 2 crashed pid=4230 exit code 3"), Severity::Bad);
        assert_eq!(severity("supervisor gave up died 10 times in 10 minutes"), Severity::Bad);
        assert_eq!(severity("reload FAILED: new worker exited"), Severity::Bad);
        assert_eq!(severity("worker 2 restarting in_ms=0"), Severity::Warn);
        assert_eq!(severity("3/4 workers ready (3 RUNNING, 1 STARTING)"), Severity::Warn);
        assert_eq!(severity("worker 2 ready pid=4262 startup_ms=31"), Severity::Good);
        assert_eq!(severity("running (supervised by wardend, pid 4208)"), Severity::Good);
        assert_eq!(severity("reload 2/4: replacing worker 3"), Severity::Info);
        assert_eq!(severity(""), Severity::Info);
    }

    #[test]
    fn what_a_worker_listens_on_in_a_cell() {
        use warden_protocol::control::Listener;
        let tcp = |addr: &str, port| Listener::Tcp { addr: addr.into(), port };
        assert_eq!(ports_cell(&[]), "-");
        assert_eq!(ports_cell(&[tcp("0.0.0.0", 3000), tcp("::", 3000), tcp("127.0.0.1", 9229)]), "3000, 9229");
        let sock = |p: &str| Listener::Unix { path: p.into() };
        assert_eq!(ports_cell(&[sock("/tmp/a.sock")]), "+1 socket");
        assert_eq!(ports_cell(&[tcp("::", 80), sock("/a"), sock("/b")]), "80, +2 sockets");
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
            standby: None,
            event: WorkerEvent::Crashed,
            pid: Some(4230),
            detail: Some("exit code 3".into()),
            at_ms: 1,
        };
        assert_eq!(after_clock(&event_line(&w, 0).unwrap()), "api worker 2 crashed pid=4230 exit code 3");
        // Hot standbys are `s1`, `s2`… (the pool `standby`), not worker 0.
        let sb = |standby, event, detail: &str| Event::Worker {
            app: "api".into(),
            worker: 0,
            standby: Some(standby),
            event,
            pid: None,
            detail: Some(detail.into()),
            at_ms: 1,
        };
        let line = |ev: &Event| after_clock(&event_line(ev, 0).unwrap()).to_string();
        assert_eq!(
            line(&sb(1, WorkerEvent::Ready, "startup_ms=80 role=standby")),
            "api worker s1 ready startup_ms=80 role=standby"
        );
        assert_eq!(
            line(&sb(0, WorkerEvent::Failed, "too many standby restarts")),
            "api worker standby failed too many standby restarts"
        );
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
            listening: Vec::new(),
            requests: None,
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

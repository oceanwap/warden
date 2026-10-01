//! Turning what the soak saw into verdicts: every failed request and every
//! long-lived connection's ending is matched with the fault it happened
//! during, the samples are checked for trends, the logs are read for panics
//! and hint-less warnings. Then a summary table and a JSON file.

use super::faults::Fault;
use super::fleet::Fleet;
use super::load::{Client, Ended, ErrClass, Failure, Session};
use super::monitor::{Latency, ProcSample};
use super::{Shared, Unplanned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;

/// A broken invariant: what, when (seconds into the run), which app.
#[derive(Debug, Clone)]
pub struct Violation {
    pub invariant: &'static str,
    pub t: Option<f64>,
    pub app: Option<String>,
    pub what: String,
}

/// Seconds around a fault's window in which its allowances apply: a
/// failure is recorded when the client gives up, a little after the cause.
const SLACK_BEFORE: f64 = 0.5;
const SLACK_AFTER: f64 = 2.0;

pub fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let i = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    sorted.get(i.min(sorted.len() - 1)).copied()
}

fn sorted(mut v: Vec<f64>) -> Vec<f64> {
    v.sort_by(|a, b| a.total_cmp(b));
    v
}

fn active<'a>(faults: &'a [Fault], app: &str, t: f64) -> Vec<&'a Fault> {
    faults
        .iter()
        .filter(|f| f.skipped.is_none())
        .filter(|f| f.app.as_deref().is_none_or(|a| a == app))
        .filter(|f| t >= f.start - SLACK_BEFORE && t <= f.end + SLACK_AFTER)
        .collect()
}

/// Ok(the allowance that covers it) or Err(why it is a violation).
pub fn judge_failure(
    f: &Failure,
    faults: &[Fault],
    un: &Unplanned,
    migrate_req: bool,
    static_app: bool,
) -> Result<&'static str, String> {
    if f.who.as_deref().is_some_and(|w| un.covers(w)) {
        return Ok("in flight on a killed worker");
    }
    let during = active(faults, &f.app, f.t);
    let lost_conn = matches!(f.class, ErrClass::Reset | ErrClass::Eof);
    for r in &during {
        if r.allow.down && (matches!(f.class, ErrClass::Refused) || lost_conn) {
            return Ok("app down by design (supervisor killed, worker-mode host killed, restart --hard, crash loop)");
        }
        if r.allow.killed && f.fresh && lost_conn {
            return Ok("queued on a killed worker's listener");
        }
        // The static server's answers carry no pid: a lost keep-alive
        // request during a kill is assumed to be on the killed worker.
        if r.allow.killed && static_app && f.who.is_none() && lost_conn {
            return Ok("in flight on a killed worker (static: worker unknown)");
        }
        if r.allow.planned && !migrate_req && f.fresh && f.class == ErrClass::Reset {
            return Ok("reset while queued on a closing listener (tcp_migrate_req = 0)");
        }
    }
    let ctx = if during.is_empty() {
        "outside any fault".to_string()
    } else {
        format!("during {}", during.iter().map(|r| format!("#{} {}", r.n, r.kind)).collect::<Vec<_>>().join(", "))
    };
    Err(format!(
        "{} {} request failed ({}{}{}) {ctx}: {}",
        f.app,
        f.client.name(),
        f.class.name(),
        if f.fresh { ", new connection" } else { "" },
        f.who.as_ref().map(|w| format!(", worker {w}")).unwrap_or_default(),
        f.detail
    ))
}

pub fn judge_session(s: &Session, un: &Unplanned) -> Result<&'static str, String> {
    match &s.ended {
        Ended::Open => Ok("open at the end"),
        Ended::WsClose(1001) => Ok("clean: close 1001"),
        Ended::SseEnd { complete: true } => Ok("clean: end of stream"),
        _ if un.covers(&s.who) => Ok("unplanned: its worker was killed"),
        other => Err(format!(
            "{} {} on worker {} (open {:.1}s -> {:.1}s) ended {other:?}, not with 1001 / a clean end, and its worker was not killed",
            s.app, s.path, s.who, s.start, s.end
        )),
    }
}

// ------------------------------------------------------------------ trends

pub struct Trend {
    pub name: String,
    pub role: &'static str,
    pub pid: u32,
    pub samples: usize,
    pub span_s: f64,
    pub fds: (usize, usize),
    pub rss_kb: (u64, u64),
    pub verdict: Result<&'static str, String>,
}

/// Per process, from samples taken while the fleet was quiet: the lowest
/// value in the first third against the lowest in the last third (the
/// lowest filters out what a rollout holds for a moment). Growth beyond
/// max(4 fds, 10 %) or max(4 MB, 25 %) is a leak.
pub fn trends(samples: &[ProcSample]) -> Vec<Trend> {
    let mut by: BTreeMap<(String, u32), Vec<&ProcSample>> = BTreeMap::new();
    for s in samples.iter().filter(|s| s.quiet) {
        by.entry((s.name.clone(), s.pid)).or_default().push(s);
    }
    let mut out = Vec::new();
    for ((name, pid), v) in by {
        let span = v.last().map(|l| l.t).unwrap_or(0.0) - v.first().map(|f| f.t).unwrap_or(0.0);
        let role = v.first().map(|s| s.role).unwrap_or("?");
        let third = (v.len() / 3).max(1);
        let (first, last) = (&v[..third], &v[v.len() - third..]);
        let fds = (first.iter().map(|s| s.fds).min().unwrap_or(0), last.iter().map(|s| s.fds).min().unwrap_or(0));
        let rss = (first.iter().map(|s| s.rss_kb).min().unwrap_or(0), last.iter().map(|s| s.rss_kb).min().unwrap_or(0));
        let verdict = if v.len() < 9 || span < 120.0 {
            Ok("too short to judge")
        } else if fds.1 > fds.0 + (fds.0 / 10).max(4) {
            Err(format!("{role} {name} pid {pid}: open fds grew {} -> {} over {span:.0}s", fds.0, fds.1))
        } else if rss.1 > rss.0 + (rss.0 / 4).max(4096) {
            Err(format!("{role} {name} pid {pid}: RSS grew {} -> {} KB over {span:.0}s", rss.0, rss.1))
        } else {
            Ok("flat")
        };
        out.push(Trend { name, role, pid, samples: v.len(), span_s: span, fds, rss_kb: rss, verdict });
    }
    out
}

// -------------------------------------------------------------------- logs

/// "2026-10-01T03:38:49.348Z" -> milliseconds since the epoch.
pub fn parse_ts(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (h, mi, se) = (n(11..13)?, n(14..16)?, n(17..19)?);
    let ms = if b.get(19) == Some(&b'.') { n(20..23).unwrap_or(0) } else { 0 };
    // Days from civil (Howard Hinnant).
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (mo + if mo > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + se * 1000 + ms)
}

pub struct LogFindings {
    /// Message -> (count, first line), for WARN/ERROR lines without `hint=`.
    pub no_hint: BTreeMap<String, (usize, String)>,
    pub panics: Vec<String>,
    pub lines: usize,
    pub warn_error: usize,
}

/// A Warden log line: (timestamp, level, the rest).
fn parse_line(l: &str) -> Option<(&str, &str, &str)> {
    let (ts, rest) = l.split_once(' ')?;
    parse_ts(ts)?;
    let (level, msg) = rest.split_once(' ')?;
    Some((ts, level, msg.trim_start()))
}

/// The message of a log line: the text before its first `key=`.
fn message_of(rest: &str) -> String {
    let mut words = Vec::new();
    for w in rest.split(' ') {
        if w.contains('=')
            && w.split('=')
                .next()
                .is_some_and(|k| !k.is_empty() && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            break;
        }
        words.push(w);
    }
    words.join(" ")
}

/// Read every supervisor's and wardend's log in `dir`.
pub fn scan_logs(dir: &Path) -> LogFindings {
    let mut out = LogFindings { no_hint: BTreeMap::new(), panics: Vec::new(), lines: 0, warn_error: 0 };
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    let mut files: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for p in files {
        let file = p.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default();
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        for l in text.lines() {
            out.lines += 1;
            let Some((_, level, rest)) = parse_line(l) else {
                // Stray stderr: the panic hook writes "… ERROR warden panicked at …".
                if l.contains("panicked at") {
                    out.panics.push(format!("{file}: {l}"));
                }
                continue;
            };
            // Worker output: only Warden's own static workers' lines count.
            let (level, rest) = if level == "OUT" {
                let Some(inner) = rest.split_once(": ").map(|x| x.1) else { continue };
                match parse_line(inner) {
                    Some((_, lv, r)) if file.starts_with("site") => (lv, r),
                    _ => continue,
                }
            } else {
                (level, rest)
            };
            if rest.contains("panicked at") || rest.contains("essential task failed") {
                out.panics.push(format!("{file}: {}", l.chars().take(400).collect::<String>()));
            }
            if level == "WARN" || level == "ERROR" {
                out.warn_error += 1;
                if !rest.contains(" hint=") && !rest.starts_with("hint=") {
                    let e = out.no_hint.entry(format!("{level} {}", message_of(rest))).or_insert((0, String::new()));
                    if e.0 == 0 {
                        e.1 = format!("{file}: {}", l.chars().take(300).collect::<String>());
                    }
                    e.0 += 1;
                }
            }
        }
    }
    out
}

/// Warden's lines (not worker output) of `app`'s log within `t ± 3 s`.
pub fn excerpt(dir: &Path, app: &str, wall_ms: i64) -> Vec<String> {
    let path = dir.join(format!("{app}.log"));
    let Ok(text) = std::fs::read_to_string(&path) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.contains(" OUT "))
        .filter(|l| l.split(' ').next().and_then(parse_ts).is_some_and(|ms| (ms - wall_ms).abs() <= 3000))
        .take(14)
        .map(|l| l.chars().take(300).collect())
        .collect()
}

// ------------------------------------------------------------------ report

pub struct Inputs<'a> {
    pub sh: &'a Shared,
    pub fleet: &'a Fleet,
    pub faults: &'a [Fault],
    pub seed: u64,
    pub minutes: f64,
    pub migrate_req: Option<String>,
    pub namespace: bool,
    pub duration_s: f64,
    pub end_problems: Vec<Violation>,
    pub alerts: BTreeMap<String, usize>,
}

pub struct Report {
    pub text: String,
    pub json: Value,
    pub violations: Vec<Violation>,
}

fn table(rows: &[Vec<String>]) -> String {
    let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
    let w: Vec<usize> =
        (0..cols).map(|c| rows.iter().map(|r| r.get(c).map_or(0, |s| s.chars().count())).max().unwrap_or(0)).collect();
    let mut s = String::new();
    for (i, r) in rows.iter().enumerate() {
        let line: Vec<String> = r.iter().enumerate().map(|(c, x)| format!("{x:<width$}", width = w[c])).collect();
        s += "  ";
        s += line.join("  ").trim_end();
        s += "\n";
        if i == 0 {
            s += "  ";
            s += &w.iter().map(|n| "-".repeat(*n)).collect::<Vec<_>>().join("  ");
            s += "\n";
        }
    }
    s
}

/// Per app and client: (answered, allowed failures by reason, violations).
type ReqCounts = (u64, BTreeMap<&'static str, u64>, u64);
/// Per fault kind: (injected, recovered, recovery ms, operation ms, skipped).
type KindStats = (usize, usize, Vec<f64>, Vec<f64>, usize);

fn ms(v: Option<f64>) -> String {
    v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "-".into())
}

pub fn build(i: Inputs) -> Report {
    let sh = i.sh;
    let (rec, mon, un, frozen) = sh.snapshot();
    let migrate_on = i.migrate_req.as_deref() == Some("1");
    let mut violations: Vec<Violation> = Vec::new();
    let statics: Vec<&str> = i.fleet.apps.iter().filter(|a| a.static_site).map(|a| a.name).collect();

    // Requests.
    let mut per: BTreeMap<(String, Client), ReqCounts> = BTreeMap::new();
    for ((app, c), n) in &rec.ok {
        per.entry((app.clone(), *c)).or_default().0 += n;
    }
    let mut per_fault: BTreeMap<&'static str, (u64, u64)> = BTreeMap::new();
    let mut failure_json = Vec::new();
    for f in &rec.failures {
        let verdict = judge_failure(f, i.faults, &un, migrate_on, statics.contains(&f.app.as_str()));
        let e = per.entry((f.app.clone(), f.client)).or_default();
        let kinds: Vec<&'static str> = active(i.faults, &f.app, f.t).iter().map(|r| r.kind).collect();
        match &verdict {
            Ok(why) => {
                *e.1.entry(why).or_default() += 1;
                for k in &kinds {
                    per_fault.entry(k).or_default().0 += 1;
                }
            }
            Err(why) => {
                e.2 += 1;
                for k in &kinds {
                    per_fault.entry(k).or_default().1 += 1;
                }
                if kinds.is_empty() {
                    per_fault.entry("(none)").or_default().1 += 1;
                }
                violations.push(Violation {
                    invariant: "requests",
                    t: Some(f.t),
                    app: Some(f.app.clone()),
                    what: why.clone(),
                });
            }
        }
        if failure_json.len() < 2000 {
            failure_json.push(json!({
                "t": (f.t * 1000.0).round() / 1000.0, "app": f.app, "client": f.client.name(), "class": f.class.name(),
                "worker": f.who, "fresh": f.fresh, "detail": f.detail,
                "verdict": match &verdict { Ok(w) => json!({"allowed": w}), Err(e) => json!({"violation": e}) },
            }));
        }
    }

    // Long-lived connections.
    let mut ll: BTreeMap<(String, String), [u64; 4]> = BTreeMap::new(); // sessions, clean, unplanned, bad
    for s in &rec.sessions {
        let e = ll.entry((s.app.clone(), s.path.clone())).or_default();
        e[0] += 1;
        match judge_session(s, &un) {
            Ok(w) if w.starts_with("clean") => e[1] += 1,
            Ok(w) if w.starts_with("unplanned") => e[2] += 1,
            Ok(_) => {}
            Err(why) => {
                e[3] += 1;
                violations.push(Violation {
                    invariant: "long-lived",
                    t: Some(s.end),
                    app: Some(s.app.clone()),
                    what: why,
                });
            }
        }
    }

    // Fault-specific problems, and recovery.
    let mut kinds: BTreeMap<&'static str, KindStats> = BTreeMap::new();
    for f in i.faults {
        let e = kinds.entry(f.kind).or_default();
        if f.skipped.is_some() {
            e.4 += 1;
            continue;
        }
        e.0 += 1;
        if let Some(r) = f.recovery {
            e.1 += 1;
            e.2.push(r * 1000.0);
        }
        if let Some(op) = f.op_ms {
            e.3.push(op);
        }
        for p in &f.problems {
            let invariant = if p.contains("did not recover") { "recovery" } else { "fault checks" };
            violations.push(Violation {
                invariant,
                t: Some(f.end),
                app: f.app.clone(),
                what: format!("#{} {} ({}): {p}", f.n, f.kind, f.detail),
            });
        }
    }

    // CLI latency, outside the windows when a supervisor was frozen on
    // purpose (`list` waits up to 1 s for an app that does not answer).
    let in_frozen = |l: &Latency| frozen.iter().any(|(a, b)| l.t <= *b + 0.2 && l.t + l.ms / 1000.0 >= *a);
    let mut lat: BTreeMap<&'static str, (Vec<f64>, usize, usize)> = BTreeMap::new();
    for l in &mon.latency {
        let e = lat.entry(l.cmd).or_default();
        if in_frozen(l) {
            e.1 += 1;
        } else {
            e.0.push(l.ms);
            if !l.ok {
                e.2 += 1;
            }
        }
    }
    for (cmd, (v, _, failed)) in &lat {
        let v = sorted(v.clone());
        if let Some(p99) = percentile(&v, 99.0).filter(|p| *p > 100.0) {
            violations.push(Violation {
                invariant: "cli latency",
                t: None,
                app: None,
                what: format!("`warden {cmd}` p99 {p99:.0} ms > 100 ms ({} samples)", v.len()),
            });
        }
        if *failed > 0 {
            violations.push(Violation {
                invariant: "cli latency",
                t: None,
                app: None,
                what: format!("`warden {cmd}` failed {failed} time(s) outside a frozen supervisor"),
            });
        }
    }

    // Processes.
    for (t, p) in &mon.problems {
        let invariant = if p.starts_with("zombie") { "zombies" } else { "orphans" };
        violations.push(Violation { invariant, t: Some(*t), app: None, what: p.clone() });
    }
    let trends = trends(&mon.procs);
    for tr in &trends {
        if let Err(e) = &tr.verdict {
            let invariant = if e.contains("fds") { "fd leak" } else { "rss growth" };
            violations.push(Violation { invariant, t: None, app: Some(tr.name.clone()), what: e.clone() });
        }
    }

    // Logs.
    let logdir = i.fleet.home.join("state/logs");
    let logs = scan_logs(&logdir);
    for p in &logs.panics {
        violations.push(Violation { invariant: "panics", t: None, app: None, what: p.clone() });
    }
    for (msg, (n, example)) in &logs.no_hint {
        violations.push(Violation {
            invariant: "hints",
            t: None,
            app: None,
            what: format!("{n} x `{msg}` without hint=, e.g. {example}"),
        });
    }
    violations.extend(i.end_problems.iter().cloned());

    // ---------------------------------------------------------- text
    let mut text = format!(
        "\nwarden chaos soak: seed {}, {:.1} min of faults ({:.0} s in all), tcp_migrate_req = {}, {}\n",
        i.seed,
        i.minutes,
        i.duration_s,
        i.migrate_req.as_deref().unwrap_or("unknown"),
        if i.namespace { "own pid namespace" } else { "no pid namespace" }
    );
    text += "\nFaults (recovery: from the end of the injection to every app ready again)\n";
    let mut rows = vec![
        [
            "fault",
            "injected",
            "skipped",
            "recovered",
            "p50 ms",
            "p99 ms",
            "max ms",
            "op p50 ms",
            "req allowed",
            "req violations",
        ]
        .map(String::from)
        .to_vec(),
    ];
    let mut kinds_json = serde_json::Map::new();
    for (k, (n, rec_n, r, op, skipped)) in &kinds {
        let r = sorted(r.clone());
        let op = sorted(op.clone());
        let pf = per_fault.get(k).copied().unwrap_or((0, 0));
        rows.push(vec![
            k.to_string(),
            n.to_string(),
            skipped.to_string(),
            format!("{rec_n}/{n}"),
            ms(percentile(&r, 50.0)),
            ms(percentile(&r, 99.0)),
            ms(r.last().copied()),
            ms(percentile(&op, 50.0)),
            pf.0.to_string(),
            pf.1.to_string(),
        ]);
        kinds_json.insert(
            k.to_string(),
            json!({"injected": n, "skipped": skipped, "recovered": rec_n, "recovery_ms_p50": percentile(&r, 50.0),
                   "recovery_ms_p99": percentile(&r, 99.0), "recovery_ms_max": r.last(), "op_ms_p50": percentile(&op, 50.0),
                   "requests_allowed": pf.0, "requests_violations": pf.1}),
        );
    }
    text += &table(&rows);

    text += "\nRequests (allowed: lost only where the fault allows it)\n";
    let mut rows = vec![["app", "client", "ok", "allowed", "violations", "allowed because"].map(String::from).to_vec()];
    let mut req_json = Vec::new();
    for ((app, c), (ok, allowed, bad)) in &per {
        let n_allowed: u64 = allowed.values().sum();
        let why = allowed.iter().map(|(w, n)| format!("{n} {w}")).collect::<Vec<_>>().join("; ");
        rows.push(vec![app.clone(), c.name().into(), ok.to_string(), n_allowed.to_string(), bad.to_string(), why]);
        req_json.push(json!({"app": app, "client": c.name(), "ok": ok, "allowed": allowed, "violations": bad}));
    }
    text += &table(&rows);

    text += "\nWebSocket and SSE connections (clean: close 1001 or a complete last event)\n";
    let mut rows = vec![["app", "path", "sessions", "clean", "unplanned", "violations"].map(String::from).to_vec()];
    let mut ll_json = Vec::new();
    for ((app, path), [n, clean, unplanned, bad]) in &ll {
        rows.push(vec![
            app.clone(),
            path.clone(),
            n.to_string(),
            clean.to_string(),
            unplanned.to_string(),
            bad.to_string(),
        ]);
        ll_json.push(
            json!({"app": app, "path": path, "sessions": n, "clean": clean, "unplanned": unplanned, "violations": bad}),
        );
    }
    text += &table(&rows);

    text += "\nCLI latency (excluded: while a supervisor was frozen on purpose)\n";
    let mut rows = vec![["command", "samples", "excluded", "p50 ms", "p99 ms", "max ms"].map(String::from).to_vec()];
    let mut lat_json = serde_json::Map::new();
    for (cmd, (v, excl, failed)) in &lat {
        let v = sorted(v.clone());
        rows.push(vec![
            format!("warden {cmd}"),
            v.len().to_string(),
            excl.to_string(),
            format!("{:.1}", percentile(&v, 50.0).unwrap_or(0.0)),
            format!("{:.1}", percentile(&v, 99.0).unwrap_or(0.0)),
            format!("{:.1}", v.last().copied().unwrap_or(0.0)),
        ]);
        lat_json.insert(
            cmd.to_string(),
            json!({"samples": v.len(), "excluded": excl, "failed": failed, "p50_ms": percentile(&v, 50.0),
                   "p99_ms": percentile(&v, 99.0), "max_ms": v.last()}),
        );
    }
    text += &table(&rows);

    text += "\nSupervisors and wardend (quiet samples: lowest of the first third -> lowest of the last third)\n";
    let mut rows = vec![["process", "pid", "samples", "span s", "fds", "RSS KB", "verdict"].map(String::from).to_vec()];
    let mut tr_json = Vec::new();
    for t in &trends {
        let verdict = match &t.verdict {
            Ok(v) => v.to_string(),
            Err(_) => "GROWING".into(),
        };
        rows.push(vec![
            format!("{} {}", t.role, t.name),
            t.pid.to_string(),
            t.samples.to_string(),
            format!("{:.0}", t.span_s),
            format!("{} -> {}", t.fds.0, t.fds.1),
            format!("{} -> {}", t.rss_kb.0, t.rss_kb.1),
            verdict.clone(),
        ]);
        tr_json.push(json!({"process": format!("{} {}", t.role, t.name), "pid": t.pid, "samples": t.samples,
            "span_s": t.span_s, "fds": [t.fds.0, t.fds.1], "rss_kb": [t.rss_kb.0, t.rss_kb.1], "verdict": verdict}));
    }
    text += &table(&rows);

    let names = [
        ("requests", "No request fails except within the allowances"),
        ("recovery", "Every app recovers to all-ready within the bound"),
        ("long-lived", "WebSocket/SSE clients see 1001 or a clean end on planned restarts"),
        ("zombies", "No zombies"),
        ("orphans", "No orphaned workers past the drain"),
        ("fd leak", "Supervisor and wardend fds: no upward trend"),
        ("rss growth", "Supervisor and wardend RSS: no upward trend"),
        ("panics", "No panic lines in the logs"),
        ("hints", "Every WARN/ERROR line has a hint="),
        ("cli latency", "`warden list` and `status` answer within 100 ms (p99)"),
        ("fault checks", "Fault-specific checks (exit codes, pinning, watchdog, freezes)"),
        ("cleanup", "`warden kill --yes` leaves no process behind"),
    ];
    text += "\nInvariants\n";
    let mut rows = vec![["invariant", "result", "violations"].map(String::from).to_vec()];
    let mut inv_json = serde_json::Map::new();
    for (key, title) in names {
        let n = violations.iter().filter(|v| v.invariant == key).count();
        rows.push(vec![title.to_string(), if n == 0 { "PASS".into() } else { "FAIL".into() }, n.to_string()]);
        inv_json.insert(key.to_string(), json!({"title": title, "pass": n == 0, "violations": n}));
    }
    text += &table(&rows);
    text += &format!(
        "\n  logs read: {} lines, {} WARN/ERROR; zombies seen (all reaped in time unless listed): {}; alerts delivered: {:?}\n",
        logs.lines, logs.warn_error, mon.zombies_seen, i.alerts
    );

    let faults_json: Vec<Value> = i
        .faults
        .iter()
        .map(|f| {
            json!({"n": f.n, "kind": f.kind, "app": f.app, "detail": f.detail, "start_s": f.start, "end_s": f.end,
                   "recovery_ms": f.recovery.map(|r| (r * 1000.0).round()), "op_ms": f.op_ms.map(f64::round),
                   "skipped": f.skipped, "problems": f.problems})
        })
        .collect();
    let json = json!({
        "seed": i.seed,
        "minutes": i.minutes,
        "duration_s": i.duration_s,
        "tcp_migrate_req": i.migrate_req,
        "pid_namespace": i.namespace,
        "machine": crate::machine(),
        "fault_kinds": kinds_json,
        "faults": faults_json,
        "requests": req_json,
        "failures": failure_json,
        "long_lived": ll_json,
        "cli_latency": lat_json,
        "processes": tr_json,
        "invariants": inv_json,
        "alerts": i.alerts,
        "violations": violations.iter().map(|v| json!({"invariant": v.invariant, "t": v.t, "app": v.app, "what": v.what})).collect::<Vec<_>>(),
    });
    Report { text, json, violations }
}

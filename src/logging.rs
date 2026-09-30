//! Line-oriented, journald-friendly logging.
//!
//! `2026-09-30T12:00:01.123Z INFO worker ready worker=1 pid=4242`
//!
//! Under journald (`JOURNAL_STREAM` set) timestamps are dropped and each
//! supervisor line gets a `<N>` syslog priority prefix, so `journalctl -p warning`
//! works. Worker output is passed through with a `worker=N` prefix. The last
//! lines are kept in memory for `warden logs`.
//!
//! Writing never blocks supervision (CP5): lines go to a writer thread
//! through one queue with separate bounds for Warden's events (4096 lines)
//! and worker output (8192 lines or 4 MB). If stdout can't keep up
//! (journald stalled, a slow pipe), worker output fills its budget and is
//! dropped first, counted; Warden's events keep their own budget. Drops are
//! reported in the log once stdout recovers, in `warden status` and as a
//! metric.

use crate::config::Level;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

/// Lines kept in memory for `warden logs`: Warden's events and worker output
/// in separate rings, so a chatty worker can't push the events out.
const RING: usize = 2000;
/// The output ring also stops growing past this many bytes (lines can be 16 KB).
const RING_BYTES: usize = 2 * 1024 * 1024;
const EVENT_QUEUE: usize = 4096;
const OUTPUT_QUEUE: usize = 8192;
/// Worker output waiting for stdout is also capped by size.
const OUTPUT_QUEUE_BYTES: usize = 4 * 1024 * 1024;

/// One FIFO to the writer thread keeps lines in order and wakes it at once;
/// the bounds are enforced with counters before a line is queued.
enum Queued {
    Event(String),
    Output(String),
}

struct Writer {
    tx: Sender<Queued>,
}

struct Logger {
    level: AtomicU8,
    timestamps: bool,
    journald: bool,
    rings: Mutex<Rings>,
    tx: broadcast::Sender<String>,
    writer: Option<Writer>,
    /// Lines queued but not yet written (for `flush`).
    pending: AtomicUsize,
    /// Warden events queued for the writer.
    events_queued: AtomicUsize,
    /// Worker output lines queued for the writer.
    output_queued: AtomicUsize,
    /// Bytes of worker output queued for the writer.
    output_bytes: AtomicUsize,
    dropped_output: AtomicU64,
    dropped_events: AtomicU64,
}

#[derive(Default)]
struct Rings {
    seq: u64,
    events: Ring,
    output: Ring,
}

/// Lines with a global sequence number, so the two rings can be merged.
#[derive(Default)]
struct Ring {
    lines: VecDeque<(u64, String)>,
    bytes: usize,
}

impl Ring {
    fn push(&mut self, seq: u64, line: String) {
        self.bytes += line.len();
        self.lines.push_back((seq, line));
        while self.lines.len() > RING || (self.bytes > RING_BYTES && self.lines.len() > 1) {
            if let Some((_, old)) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(level: Level, timestamps: Option<bool>) {
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let (tx, _) = broadcast::channel(256);
    let (wtx, wrx) = channel::<Queued>();
    let ok = LOGGER
        .set(Logger {
            level: AtomicU8::new(level as u8),
            timestamps: timestamps.unwrap_or(!journald),
            journald,
            rings: Mutex::new(Rings::default()),
            tx,
            writer: Some(Writer { tx: wtx }),
            pending: AtomicUsize::new(0),
            events_queued: AtomicUsize::new(0),
            output_queued: AtomicUsize::new(0),
            output_bytes: AtomicUsize::new(0),
            dropped_output: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
        })
        .is_ok();
    if ok {
        let spawned = std::thread::Builder::new().name("warden-log".into()).spawn(move || write_loop(wrx));
        if let Err(e) = spawned {
            eprintln!("warden: cannot start the log writer thread ({e}); logging directly");
        }
    }
}

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| {
        let (tx, _) = broadcast::channel(16);
        Logger {
            level: AtomicU8::new(Level::Info as u8),
            timestamps: true,
            journald: false,
            rings: Mutex::new(Rings::default()),
            tx,
            writer: None,
            pending: AtomicUsize::new(0),
            events_queued: AtomicUsize::new(0),
            output_queued: AtomicUsize::new(0),
            output_bytes: AtomicUsize::new(0),
            dropped_output: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
        }
    })
}

fn current_level(l: &Logger) -> Level {
    match l.level.load(Ordering::Relaxed) {
        0 => Level::Debug,
        1 => Level::Info,
        2 => Level::Warn,
        _ => Level::Error,
    }
}

/// Change the level at runtime (`warden log-level debug`).
pub fn set_level(level: Level) {
    logger().level.store(level as u8, Ordering::Relaxed);
}

pub fn level() -> Level {
    current_level(logger())
}

/// (worker output lines, supervisor events) dropped because stdout was too slow.
pub fn dropped() -> (u64, u64) {
    let l = logger();
    (l.dropped_output.load(Ordering::Relaxed), l.dropped_events.load(Ordering::Relaxed))
}

/// Supervisor event. `fields` are appended as `key=value`.
pub fn event(level: Level, msg: &str, fields: &[(&str, &dyn std::fmt::Display)]) {
    let l = logger();
    if level < current_level(l) {
        return;
    }
    let mut line = String::with_capacity(96);
    let _ = write!(line, "{:<5} {msg}", level_name(level));
    for (k, v) in fields {
        let v = v.to_string();
        if v.is_empty() || v.contains(char::is_whitespace) || v.contains('"') || v.contains('=') {
            let _ = write!(line, " {k}={v:?}");
        } else {
            let _ = write!(line, " {k}={v}");
        }
    }
    emit(l, line, Some(level));
}

/// A line of worker stdout/stderr.
pub fn worker_output(worker: &str, stream: &str, text: &str) {
    let l = logger();
    emit(l, format!("OUT   worker={worker} {stream}: {text}"), None);
}

fn emit(l: &Logger, body: String, level: Option<Level>) {
    let stamped = format!("{} {body}", timestamp_now());
    let printed = if l.journald {
        match level {
            Some(lv) => format!("<{}>{body}", syslog_priority(lv)),
            None => body,
        }
    } else if l.timestamps {
        stamped.clone()
    } else {
        body
    };
    match &l.writer {
        Some(w) if level.is_some() => {
            if l.events_queued.load(Ordering::Relaxed) >= EVENT_QUEUE {
                l.dropped_events.fetch_add(1, Ordering::Relaxed);
            } else {
                l.events_queued.fetch_add(1, Ordering::Relaxed);
                l.pending.fetch_add(1, Ordering::Relaxed);
                if w.tx.send(Queued::Event(printed)).is_err() {
                    // Writer thread gone (only if it panicked): count and move on.
                    l.events_queued.fetch_sub(1, Ordering::Relaxed);
                    l.pending.fetch_sub(1, Ordering::Relaxed);
                    l.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        Some(w) => {
            let len = printed.len();
            if l.output_bytes.load(Ordering::Relaxed) + len > OUTPUT_QUEUE_BYTES
                || l.output_queued.load(Ordering::Relaxed) >= OUTPUT_QUEUE
            {
                l.dropped_output.fetch_add(1, Ordering::Relaxed);
            } else {
                l.output_queued.fetch_add(1, Ordering::Relaxed);
                l.output_bytes.fetch_add(len, Ordering::Relaxed);
                l.pending.fetch_add(1, Ordering::Relaxed);
                if w.tx.send(Queued::Output(printed)).is_err() {
                    l.output_queued.fetch_sub(1, Ordering::Relaxed);
                    l.output_bytes.fetch_sub(len, Ordering::Relaxed);
                    l.pending.fetch_sub(1, Ordering::Relaxed);
                    l.dropped_output.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        None => write_line(&printed),
    }
    {
        let mut rings = l.rings.lock().unwrap_or_else(|e| e.into_inner());
        rings.seq += 1;
        let seq = rings.seq;
        let ring = if level.is_some() { &mut rings.events } else { &mut rings.output };
        ring.push(seq, stamped.clone());
    }
    // No subscriber (`warden logs -f`) is the normal case, not an error.
    let _ = l.tx.send(stamped);
}

fn write_line(line: &str) {
    let mut out = std::io::stdout().lock();
    // A closed or failing stdout must not take Warden down; there is nowhere
    // left to report it, so the line is lost (it is still in `warden logs`).
    let _ = out.write_all(line.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// The writer thread: lines in the order they were logged. After a period
/// of drops, one line says how many and why.
fn write_loop(rx: Receiver<Queued>) {
    let l = logger();
    let mut reported = (0u64, 0u64);
    let mut last_report = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Queued::Event(line)) => {
                write_line(&line);
                l.events_queued.fetch_sub(1, Ordering::Relaxed);
                l.pending.fetch_sub(1, Ordering::Relaxed);
            }
            Ok(Queued::Output(line)) => {
                write_line(&line);
                l.output_queued.fetch_sub(1, Ordering::Relaxed);
                l.output_bytes.fetch_sub(line.len(), Ordering::Relaxed);
                l.pending.fetch_sub(1, Ordering::Relaxed);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        let now = (l.dropped_output.load(Ordering::Relaxed), l.dropped_events.load(Ordering::Relaxed));
        if now != reported && last_report.elapsed() >= Duration::from_secs(5) {
            // Goes through the queue like any event, so it is written once
            // stdout drains and also lands in `warden logs`.
            event(
                Level::Warn,
                "log lines dropped because stdout could not keep up",
                &[
                    ("worker_output", &(now.0 - reported.0)),
                    ("events", &(now.1 - reported.1)),
                    ("hint", &"check the log consumer (journald, the pipe reader); `warden status` shows totals"),
                ],
            );
            reported = now;
            last_report = Instant::now();
        }
    }
}

/// Wait (up to `timeout`) for queued lines to be written. Called before exit.
pub fn flush(timeout: Duration) {
    let l = logger();
    let t0 = Instant::now();
    while l.pending.load(Ordering::Relaxed) > 0 && t0.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The last `n` lines kept in memory that pass `keep` (oldest first).
pub fn recent_matching(n: usize, keep: &dyn Fn(&str) -> bool) -> Vec<String> {
    let rings = logger().rings.lock().unwrap_or_else(|e| e.into_inner());
    merge_newest(&rings.events, &rings.output, n, keep)
}

/// The newest `n` lines of both rings that pass `keep`, oldest first.
fn merge_newest(a: &Ring, b: &Ring, n: usize, keep: &dyn Fn(&str) -> bool) -> Vec<String> {
    let mut a = a.lines.iter().rev().filter(|(_, l)| keep(l)).peekable();
    let mut b = b.lines.iter().rev().filter(|(_, l)| keep(l)).peekable();
    let mut out = Vec::with_capacity(n.min(RING * 2));
    while out.len() < n {
        let next = match (a.peek(), b.peek()) {
            (Some((sa, _)), Some((sb, _))) => {
                if sa > sb {
                    a.next()
                } else {
                    b.next()
                }
            }
            (Some(_), None) => a.next(),
            (None, Some(_)) => b.next(),
            (None, None) => break,
        };
        if let Some((_, l)) = next {
            out.push(l.clone());
        }
    }
    out.reverse();
    out
}

pub fn subscribe() -> broadcast::Receiver<String> {
    logger().tx.subscribe()
}

fn level_name(l: Level) -> &'static str {
    match l {
        Level::Debug => "DEBUG",
        Level::Info => "INFO",
        Level::Warn => "WARN",
        Level::Error => "ERROR",
    }
}

fn syslog_priority(l: Level) -> u8 {
    match l {
        Level::Debug => 7,
        Level::Info => 6,
        Level::Warn => 4,
        Level::Error => 3,
    }
}

pub fn timestamp_now() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format_rfc3339(d.as_secs() as i64, d.subsec_millis())
}

/// RFC 3339 UTC with milliseconds, without pulling in a date crate.
pub fn format_rfc3339(secs: i64, millis: u32) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[macro_export]
macro_rules! log_event {
    ($lvl:ident, $msg:expr $(, $k:ident = $v:expr)* $(,)?) => {
        $crate::logging::event($crate::config::Level::$lvl, &$msg, &[$((stringify!($k), &$v as &dyn std::fmt::Display)),*])
    };
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log_event!(Info, $($t)*) }; }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log_event!(Warn, $($t)*) }; }
#[macro_export]
macro_rules! error { ($($t:tt)*) => { $crate::log_event!(Error, $($t)*) }; }
#[macro_export]
macro_rules! debug { ($($t:tt)*) => { $crate::log_event!(Debug, $($t)*) }; }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_is_bounded_by_lines_and_bytes() {
        let mut r = Ring::default();
        for i in 0..RING + 10 {
            r.push(i as u64, format!("line {i}"));
        }
        assert_eq!(r.lines.len(), RING);
        assert_eq!(r.lines.front().map(|(_, l)| l.as_str()), Some("line 10"));
        let mut r = Ring::default();
        for i in 0..300 {
            r.push(i, "x".repeat(16 * 1024));
        }
        assert!(r.bytes <= RING_BYTES, "{} bytes kept", r.bytes);
        assert_eq!(r.bytes, r.lines.iter().map(|(_, l)| l.len()).sum::<usize>());
        let mut r = Ring::default();
        r.push(1, "y".repeat(RING_BYTES + 1));
        assert_eq!(r.lines.len(), 1, "one oversized line is still kept");
    }

    #[test]
    fn rings_merge_in_order_and_filter() {
        let (mut ev, mut out) = (Ring::default(), Ring::default());
        for seq in 1..=10u64 {
            let ring = if seq % 3 == 0 { &mut ev } else { &mut out };
            ring.push(seq, format!("l{seq}"));
        }
        let all = |_: &str| true;
        assert_eq!(merge_newest(&ev, &out, 4, &all), vec!["l7", "l8", "l9", "l10"]);
        assert_eq!(merge_newest(&ev, &out, 100, &all).len(), 10);
        let odd = |l: &str| l.ends_with(['1', '3', '5', '7', '9']);
        assert_eq!(merge_newest(&ev, &out, 3, &odd), vec!["l5", "l7", "l9"]);
    }

    #[test]
    fn rfc3339() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_790_769_601, 5), "2026-09-30T12:00:01.005Z");
        assert_eq!(format_rfc3339(951_782_400, 999), "2000-02-29T00:00:00.999Z");
    }
}

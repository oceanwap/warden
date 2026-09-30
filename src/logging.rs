//! Line-oriented, journald-friendly logging.
//!
//! `2026-09-30T12:00:01.123Z INFO worker ready worker=1 pid=4242`
//!
//! Under journald (`JOURNAL_STREAM` set) timestamps are dropped and each
//! supervisor line gets a `<N>` syslog priority prefix, so `journalctl -p warning`
//! works. Worker output is passed through with a `worker=N` prefix. The last
//! lines are kept in memory for `warden logs`.

use crate::config::Level;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;

const RING: usize = 1000;

struct Logger {
    level: Level,
    timestamps: bool,
    journald: bool,
    ring: Mutex<VecDeque<String>>,
    tx: broadcast::Sender<String>,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

pub fn init(level: Level, timestamps: Option<bool>) {
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    let (tx, _) = broadcast::channel(1024);
    let _ = LOGGER.set(Logger {
        level,
        timestamps: timestamps.unwrap_or(!journald),
        journald,
        ring: Mutex::new(VecDeque::with_capacity(RING)),
        tx,
    });
}

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| {
        let (tx, _) = broadcast::channel(16);
        Logger { level: Level::Info, timestamps: true, journald: false, ring: Mutex::new(VecDeque::new()), tx }
    })
}

/// Supervisor event. `fields` are appended as `key=value`.
pub fn event(level: Level, msg: &str, fields: &[(&str, &dyn std::fmt::Display)]) {
    let l = logger();
    if level < l.level {
        return;
    }
    let mut line = String::with_capacity(96);
    let _ = write!(line, "{:<5} {msg}", level_name(level));
    for (k, v) in fields {
        let v = v.to_string();
        if v.is_empty() || v.contains(char::is_whitespace) || v.contains('"') {
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
    {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(printed.as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }
    let mut ring = l.ring.lock().unwrap_or_else(|e| e.into_inner());
    if ring.len() == RING {
        ring.pop_front();
    }
    ring.push_back(stamped.clone());
    drop(ring);
    let _ = l.tx.send(stamped);
}

pub fn recent(n: usize) -> Vec<String> {
    let ring = logger().ring.lock().unwrap_or_else(|e| e.into_inner());
    let skip = ring.len().saturating_sub(n);
    ring.iter().skip(skip).cloned().collect()
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
    fn rfc3339() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_790_769_601, 5), "2026-09-30T12:00:01.005Z");
        assert_eq!(format_rfc3339(951_782_400, 999), "2000-02-29T00:00:00.999Z");
    }
}

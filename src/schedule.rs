//! Cron schedules for `[restart] schedule` (PM2's `cron_restart`), in the
//! server's local time. Five fields (minute hour day-of-month month
//! day-of-week) with `*`, lists, ranges, steps and names, plus @hourly,
//! @daily, @weekly, @monthly and @yearly. As in cron, when both day fields
//! are restricted a day matches either of them.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
}

const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

fn value(s: &str, names: &[&str], base: u32) -> Result<u32, String> {
    if let Ok(n) = s.parse::<u32>() {
        return Ok(n);
    }
    let l = s.to_ascii_lowercase();
    names
        .iter()
        .position(|n| *n == l)
        .map(|i| i as u32 + base)
        .ok_or_else(|| format!("{s:?} is not a number{}", if names.is_empty() { "" } else { " or a name" }))
}

/// One field into a bit set of allowed values in `lo..=hi`.
fn field(spec: &str, lo: u32, hi: u32, names: &[&str], base: u32) -> Result<(u64, bool), String> {
    let mut bits = 0u64;
    let any = spec == "*" || spec == "?";
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let s: u32 = s.parse().map_err(|_| format!("step {s:?} is not a number"))?;
                if s == 0 {
                    return Err("a step of 0 never matches".into());
                }
                (r, s)
            }
            None => (part, 1),
        };
        let (a, b) = if range == "*" || range == "?" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (value(a, names, base)?, value(b, names, base)?)
        } else {
            let a = value(range, names, base)?;
            (a, if step > 1 { hi } else { a })
        };
        if a < lo || b > hi || a > b {
            return Err(format!("{part:?} is outside {lo}-{hi}"));
        }
        let mut v = a;
        while v <= b {
            bits |= 1 << v;
            v += step;
        }
    }
    Ok((bits, any))
}

impl Cron {
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let expr = match expr.trim() {
            "@yearly" | "@annually" => "0 0 1 1 *",
            "@monthly" => "0 0 1 * *",
            "@weekly" => "0 0 * * 0",
            "@daily" | "@midnight" => "0 0 * * *",
            "@hourly" => "0 * * * *",
            e => e,
        };
        let f: Vec<&str> = expr.split_whitespace().collect();
        if f.len() != 5 {
            return Err(format!("expected 5 fields (minute hour day month weekday), got {}", f.len()));
        }
        let (minute, _) = field(f[0], 0, 59, &[], 0).map_err(|e| format!("minute: {e}"))?;
        let (hour, _) = field(f[1], 0, 23, &[], 0).map_err(|e| format!("hour: {e}"))?;
        let (dom, dom_any) = field(f[2], 1, 31, &[], 0).map_err(|e| format!("day of month: {e}"))?;
        let (month, _) = field(f[3], 1, 12, &MONTHS, 1).map_err(|e| format!("month: {e}"))?;
        let (mut dow, dow_any) = field(f[4], 0, 7, &DAYS, 0).map_err(|e| format!("day of week: {e}"))?;
        if dow & (1 << 7) != 0 {
            dow |= 1; // 7 is Sunday too
        }
        let c = Cron { minute, hour, dom, month, dow, dom_any, dow_any };
        if c.next_after(SystemTime::now()).is_none() {
            return Err("never fires (no such date in the next 5 years)".into());
        }
        Ok(c)
    }

    fn day_matches(&self, mday: u32, wday: u32) -> bool {
        let d = self.dom & (1 << mday) != 0;
        let w = self.dow & (1 << wday) != 0;
        match (self.dom_any, self.dow_any) {
            (true, true) => true,
            (true, false) => w,
            (false, true) => d,
            (false, false) => d || w,
        }
    }

    /// The first matching minute strictly after `t`, in local time.
    pub fn next_after(&self, t: SystemTime) -> Option<SystemTime> {
        let secs = t.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
        let mut m = secs / 60 + 1; // next whole minute
        let end = m + 5 * 366 * 24 * 60;
        while m < end {
            let tm = local(m * 60)?;
            let (min, hour, mday, mon, wday) =
                (tm.tm_min as u32, tm.tm_hour as u32, tm.tm_mday as u32, tm.tm_mon as u32 + 1, tm.tm_wday as u32);
            if self.month & (1 << mon) == 0 || !self.day_matches(mday, wday) {
                // Skip to the next local midnight (at least one minute).
                m += ((24 - hour as i64) * 60 - min as i64).max(1);
                continue;
            }
            if self.hour & (1 << hour) == 0 {
                m += (60 - min as i64).max(1);
                continue;
            }
            if self.minute & (1 << min) == 0 {
                m += 1;
                continue;
            }
            return Some(UNIX_EPOCH + Duration::from_secs((m * 60) as u64));
        }
        None
    }
}

fn local(secs: i64) -> Option<libc::tm> {
    crate::sys::localtime(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    #[test]
    fn parses_and_rejects() {
        assert!(Cron::parse("0 3 * * *").is_ok());
        assert!(Cron::parse("*/15 9-17 * * mon-fri").is_ok());
        assert!(Cron::parse("0 0 1,15 jan,jul *").is_ok());
        assert!(Cron::parse("@daily").is_ok());
        assert!(Cron::parse("0 3 * *").unwrap_err().contains("5 fields"));
        assert!(Cron::parse("60 * * * *").unwrap_err().contains("minute"));
        assert!(Cron::parse("0 0 31 2 *").unwrap_err().contains("never fires"));
        assert!(Cron::parse("*/0 * * * *").is_err());
        assert!(Cron::parse("0 0 * * funday").is_err());
    }

    #[test]
    fn next_fire_times() {
        // Local time is what matters; in tests compare against localtime fields.
        let c = Cron::parse("30 3 * * *").unwrap();
        let n = c.next_after(at(1_790_769_601)).unwrap();
        let tm = local(n.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64).unwrap();
        assert_eq!((tm.tm_hour, tm.tm_min), (3, 30));
        assert!(n > at(1_790_769_601) && n <= at(1_790_769_601 + 86_400));
        // Strictly after: firing exactly at a match moves to the next day.
        let again = c.next_after(n).unwrap();
        assert_eq!(again.duration_since(n).unwrap().as_secs(), 86_400);
        let every = Cron::parse("*/15 * * * *").unwrap();
        let n = every.next_after(at(1_790_769_601)).unwrap();
        let tm = local(n.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64).unwrap();
        assert_eq!(tm.tm_min % 15, 0);
        let sunday = Cron::parse("0 0 * * 7").unwrap();
        let n = sunday.next_after(at(1_790_769_601)).unwrap();
        assert_eq!(local(n.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64).unwrap().tm_wday, 0);
    }
}

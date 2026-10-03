//! Small texts and numbers written without allocating: decimal and
//! hexadecimal into a buffer, ETags and dates on the stack, the clock a
//! response's `Date` comes from, and the date and Basic-auth text formats
//! read back.

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

/// Appends `s` to `out`.
#[inline]
pub(super) fn put(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(s.as_bytes());
}

/// `n` in decimal, in the tail of `buf`.
pub(super) fn dec(mut n: u64, buf: &mut [u8; 20]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            return &buf[i..];
        }
    }
}

/// `n` in lowercase hexadecimal (like `{:x}`), in the tail of `buf`.
pub(super) fn hex(mut n: u64, buf: &mut [u8; 16]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b"0123456789abcdef"[(n & 15) as usize];
        n >>= 4;
        if n == 0 {
            return &buf[i..];
        }
    }
}

/// Appends `n` in decimal to `out`.
pub(super) fn put_dec(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(dec(n, &mut [0; 20]));
}

/// A short ASCII text kept on the stack: an ETag or a date, which every
/// request makes and none needs the heap for.
pub(super) struct Short {
    buf: [u8; 64],
    len: usize,
}

impl Short {
    pub(super) fn new() -> Short {
        Short { buf: [0; 64], len: 0 }
    }

    pub(super) fn push(&mut self, b: &[u8]) -> &mut Short {
        // 64 bytes hold the longest text made here (an ETag is at most 51, a
        // date 45); a longer one is cut short rather than overrun.
        let n = b.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
        self
    }

    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// The weak ETag of a file: `W/"<size>-<mtime>"`, with `-<encoding>` for a
/// precompressed variant, all in hexadecimal. The modification time is its
/// seconds, and `.<nanoseconds>` when it has any: two edits of the same size
/// within one second are two versions, and a browser holding the first must
/// not be told it is current (editors and build tools leave times with a
/// fraction; an archive that keeps only seconds, as tar's default format
/// does, leaves whole ones). No inode: the tag is the same on every host
/// that has the same files with the same times.
pub(super) fn etag(len: u64, mtime: u64, nanos: u32, encoding: Option<&str>) -> Short {
    let mut e = Short::new();
    e.push(b"W/\"").push(hex(len, &mut [0; 16])).push(b"-").push(hex(mtime, &mut [0; 16]));
    if nanos != 0 {
        e.push(b".").push(hex(u64::from(nanos), &mut [0; 16]));
    }
    if let Some(enc) = encoding {
        e.push(b"-").push(enc.as_bytes());
    }
    e.push(b"\"");
    e
}

const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// IMF-fixdate: `Wed, 30 Sep 2026 12:00:01 GMT`.
pub(super) fn http_date_short(secs: u64) -> Short {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (y, m, d) = crate::logging::civil_from_days(days);
    let two = |n: u64| [b'0' + (n / 10) as u8, b'0' + (n % 10) as u8];
    let mut s = Short::new();
    s.push(DAYS[(days.rem_euclid(7)) as usize].as_bytes())
        .push(b", ")
        .push(&two(u64::from(d)))
        .push(b" ")
        .push(MONTHS[(m - 1) as usize].as_bytes())
        .push(b" ")
        .push(dec(y.unsigned_abs(), &mut [0; 20]))
        .push(b" ")
        .push(&two(rem / 3600))
        .push(b":")
        .push(&two((rem % 3600) / 60))
        .push(b":")
        .push(&two(rem % 60))
        .push(b" GMT");
    s
}

/// An HTTP date is this long, whatever the day (`Sun, 06 Nov 1994 08:49:37 GMT`).
pub(super) const DATE_LEN: usize = 29;

/// The last day an HTTP date can say (9999-12-31): the clock never gets near.
const LAST_SECOND: u64 = 253_402_300_799;

/// The time a response is made at: its `Date`, and the second it is of.
#[derive(Clone, Copy)]
pub(super) struct Now {
    pub(super) secs: u64,
    date: [u8; DATE_LEN],
}

impl Now {
    /// The `Date` header's value.
    pub(super) fn date(&self) -> &str {
        std::str::from_utf8(&self.date).unwrap_or("")
    }

    /// The same bytes, to be written over a date already in a buffer.
    pub(super) fn date_bytes(&self) -> &[u8; DATE_LEN] {
        &self.date
    }

    /// The second `secs` (since 1970; the last an HTTP date can say at most).
    pub(super) fn of(secs: u64) -> Now {
        let secs = secs.min(LAST_SECOND);
        let mut date = [b' '; DATE_LEN];
        let text = http_date_short(secs);
        let n = text.as_str().len().min(DATE_LEN);
        date[..n].copy_from_slice(&text.as_str().as_bytes()[..n]);
        Now { secs, date }
    }
}

/// The wall clock now. Reading it costs a vDSO call (no system call); the
/// text is made once a second, so a request pays a copy of 29 bytes.
pub(super) fn now() -> Now {
    thread_local! {
        static LAST: Cell<(u64, [u8; DATE_LEN])> = const { Cell::new((u64::MAX, [b' '; DATE_LEN])) };
    }
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()).min(LAST_SECOND);
    LAST.with(|last| {
        let (at, date) = last.get();
        if at == secs {
            return Now { secs, date };
        }
        let now = Now::of(secs);
        last.set((secs, now.date));
        now
    })
}

pub(super) fn parse_http_date(s: &str) -> Option<u64> {
    let p: Vec<&str> = s.split_whitespace().collect();
    if p.len() != 6 || p[5] != "GMT" {
        return None;
    }
    let d: i64 = p[1].parse().ok()?;
    let m = MONTHS.iter().position(|x| *x == p[2])? as i64 + 1;
    let y: i64 = p[3].parse().ok()?;
    let hms: Vec<i64> = p[4].split(':').filter_map(|x| x.parse().ok()).collect();
    if hms.len() != 3 {
        return None;
    }
    // Header text comes from the client: bound every field so the arithmetic
    // below cannot overflow (an HTTP date has a four-digit year).
    let bounded = (1970..=9999).contains(&y)
        && (1..=31).contains(&d)
        && (0..24).contains(&hms[0])
        && (0..60).contains(&hms[1])
        && (0..=60).contains(&hms[2]);
    if !bounded {
        return None;
    }
    // Days from civil (Howard Hinnant).
    let (y2, m2) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * m2 + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hms[0] * 3600 + hms[1] * 60 + hms[2];
    (secs >= 0).then_some(secs as u64)
}

pub(super) fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_base64_are_read_and_written() {
        assert_eq!(http_date_short(0).as_str(), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(http_date_short(1_790_769_601).as_str(), "Wed, 30 Sep 2026 12:00:01 GMT");
        assert_eq!(parse_http_date("Wed, 30 Sep 2026 12:00:01 GMT"), Some(1_790_769_601));
        assert_eq!(parse_http_date("nonsense"), None);
        // A client chooses these numbers: nothing may overflow or wrap.
        for bad in [
            "Wed, 30 Sep 9223372036854775807 12:00:01 GMT",
            "Wed, 30 Sep 2026 12:00:99999999999999999 GMT",
            "Wed, 9223372036854775807 Sep 2026 12:00:01 GMT",
            "Wed, 30 Sep 1969 12:00:01 GMT",
            "Wed, 32 Sep 2026 12:00:01 GMT",
            "Wed, 30 Sep 2026 24:00:01 GMT",
            "Wed, 30 Sep -2026 12:00:01 GMT",
        ] {
            assert_eq!(parse_http_date(bad), None, "{bad}");
        }
        assert_eq!(base64(b"user:pass"), "dXNlcjpwYXNz");
        assert_eq!(base64(b"ab"), "YWI=");
    }

    #[test]
    fn an_etag_tells_edits_within_one_second_apart() {
        let plain = etag(10, 1_790_769_601, 0, None);
        assert_eq!(plain.as_str(), "W/\"a-6abcf9c1\"", "no fraction: as before");
        let a = etag(10, 1_790_769_601, 250_000_000, None);
        let b = etag(10, 1_790_769_601, 750_000_000, Some("br"));
        assert_eq!(a.as_str(), "W/\"a-6abcf9c1.ee6b280\"");
        assert_eq!(b.as_str(), "W/\"a-6abcf9c1.2cb41780-br\"");
        assert_ne!(a.as_str(), plain.as_str());
        // The longest: 64-bit size and time, all nanoseconds, the longest encoding.
        let longest = etag(u64::MAX, u64::MAX, 999_999_999, Some("gzip"));
        assert!(longest.as_str().ends_with("-gzip\""), "{}", longest.as_str());
        assert!(longest.as_str().len() <= 64);
    }

    #[test]
    fn the_clock_makes_a_date_of_the_right_length_that_follows_the_second() {
        let a = now();
        assert_eq!(a.date().len(), DATE_LEN);
        assert_eq!(parse_http_date(a.date()), Some(a.secs), "the text is the second");
        assert_eq!(a.date_bytes(), a.date().as_bytes());
        // In the same second it is the same text; across seconds it moves.
        let mut seen = std::collections::BTreeSet::new();
        let t0 = std::time::Instant::now();
        while t0.elapsed() < std::time::Duration::from_millis(2300) {
            let n = now();
            assert_eq!(parse_http_date(n.date()), Some(n.secs));
            seen.insert(n.secs);
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(seen.len() >= 2, "{seen:?}");
        // Every date this clock can make has the same length.
        for secs in [0, 1, 86_399, 951_782_400, 1_790_769_601, LAST_SECOND] {
            assert_eq!(http_date_short(secs).as_str().len(), DATE_LEN, "{secs}");
        }
    }

    /// The text helpers that replaced `format!` and `Path` calls give what
    /// those gave.
    #[test]
    fn the_allocation_free_helpers_agree_with_what_they_replaced() {
        // Dates and ETags, over small, ordinary and huge values.
        let old_date = |secs: u64| {
            let days = (secs / 86_400) as i64;
            let rem = secs % 86_400;
            let (y, m, d) = crate::logging::civil_from_days(days);
            format!(
                "{}, {d:02} {} {y} {:02}:{:02}:{:02} GMT",
                DAYS[(days.rem_euclid(7)) as usize],
                MONTHS[(m - 1) as usize],
                rem / 3600,
                (rem % 3600) / 60,
                rem % 60
            )
        };
        let mut x = 0x2545_f491_4f6c_dd1du64;
        let mut values = vec![0, 1, 59, 86_399, 86_400, 1_790_769_601, u64::from(u32::MAX), u64::MAX / 2000, u64::MAX];
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            values.push(x >> (x % 40));
        }
        for v in values {
            // (The year of the largest values has more digits than any real date.)
            assert_eq!(http_date_short(v).as_str(), old_date(v), "date of {v}");
            for enc in [None, Some("br"), Some("gzip")] {
                let old = format!("W/\"{v:x}-{:x}{}\"", v / 3, enc.map(|e| format!("-{e}")).unwrap_or_default());
                assert_eq!(etag(v, v / 3, 0, enc).as_str(), old);
            }
            let mut out = Vec::new();
            put_dec(&mut out, v);
            assert_eq!(out, v.to_string().into_bytes());
        }
    }
}

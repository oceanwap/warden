//! Small texts and numbers written without allocating: decimal and
//! hexadecimal into a buffer, ETags and dates on the stack, and the date and
//! Basic-auth text formats read back.

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
        // 64 bytes hold the longest text made here (an ETag is at most 42, a
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
/// precompressed variant, all in hexadecimal.
pub(super) fn etag(len: u64, mtime: u64, encoding: Option<&str>) -> Short {
    let mut e = Short::new();
    e.push(b"W/\"").push(hex(len, &mut [0; 16])).push(b"-").push(hex(mtime, &mut [0; 16]));
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
                assert_eq!(etag(v, v / 3, enc).as_str(), old);
            }
            let mut out = Vec::new();
            put_dec(&mut out, v);
            assert_eq!(out, v.to_string().into_bytes());
        }
    }
}

//! Reading logs after the fact: `warden logs <app> --history` and
//! `warden search <text>`. Reads the app's log files, rotated and gzipped
//! ones included (oldest first), or journald when it runs under systemd;
//! filters by text, time, level, worker and stream; prints plain lines for
//! pipes (`| grep`, `| head`) or JSON lines for `jq`.

use crate::config::Level;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `2026-09-30T12:00:01.123Z` — every framed line starts with this.
const TS: usize = 24;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// Every one must appear.
    pub grep: Vec<String>,
    /// None may appear.
    pub exclude: Vec<String>,
    pub ignore_case: bool,
    /// RFC 3339 UTC prefixes, compared with each line's timestamp.
    pub since: Option<String>,
    pub until: Option<String>,
    /// Warden events at this level or above (worker output excluded).
    pub level: Option<Level>,
    pub events: bool,
    pub stream: Option<String>,
    pub worker: Option<String>,
}

impl Query {
    pub fn is_filtering(&self) -> bool {
        !self.grep.is_empty()
            || !self.exclude.is_empty()
            || self.since.is_some()
            || self.until.is_some()
            || self.level.is_some()
    }

    fn text_matches(&self, line: &str) -> bool {
        let has = |needle: &str| {
            if self.ignore_case { line.to_lowercase().contains(&needle.to_lowercase()) } else { line.contains(needle) }
        };
        self.grep.iter().all(|g| has(g)) && !self.exclude.iter().any(|x| has(x))
    }

    fn time_matches(&self, line: &str) -> bool {
        let Some(ts) = timestamp_of(line) else { return true };
        self.since.as_deref().is_none_or(|s| ts >= s) && self.until.as_deref().is_none_or(|u| ts < u)
    }

    /// A line framed by Warden (`<ts> LEVEL message` / `<ts> OUT   worker=…`).
    pub fn matches(&self, line: &str) -> bool {
        if !self.text_matches(line) || !self.time_matches(line) {
            return false;
        }
        if !crate::control::log_filter(line, self.worker.as_deref(), self.events || self.level.is_some()) {
            return false;
        }
        if !crate::control::stream_filter(line, self.stream.as_deref()) {
            return false;
        }
        match self.level {
            None => true,
            Some(min) => level_of(line).is_some_and(|l| l >= min),
        }
    }

    /// A line as the app wrote it (out/err files): only text and time apply.
    pub fn matches_raw(&self, line: &str) -> bool {
        self.text_matches(line) && self.time_matches(line)
    }
}

fn timestamp_of(line: &str) -> Option<&str> {
    let ts = line.get(..TS)?;
    let b = ts.as_bytes();
    (b[4] == b'-' && b[10] == b'T' && b[23] == b'Z').then_some(ts)
}

fn level_of(line: &str) -> Option<Level> {
    let word = line.get(TS + 1..)?.split_whitespace().next()?;
    Some(match word {
        "DEBUG" => Level::Debug,
        "INFO" => Level::Info,
        "WARN" => Level::Warn,
        "ERROR" => Level::Error,
        _ => return None,
    })
}

/// `2h`, `30m`, `90s`, `3d` ago, or a UTC date/time (`2026-09-30`,
/// `2026-09-30T12:00`, `2026-09-30 12:00:05`), as a comparable prefix.
pub fn parse_time(s: &str) -> Result<String, String> {
    let t = s.trim();
    let rel = t
        .strip_suffix('s')
        .map(|n| (n, 1))
        .or_else(|| t.strip_suffix('m').map(|n| (n, 60)))
        .or_else(|| t.strip_suffix('h').map(|n| (n, 3600)))
        .or_else(|| t.strip_suffix('d').map(|n| (n, 86_400)));
    if let Some((n, mult)) = rel {
        if let Ok(n) = n.parse::<u64>() {
            let then = SystemTime::now()
                .checked_sub(Duration::from_secs(n.saturating_mul(mult)))
                .unwrap_or(UNIX_EPOCH)
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            return Ok(crate::logging::format_rfc3339(then.as_secs() as i64, then.subsec_millis()));
        }
    }
    let norm = t.replace(' ', "T").trim_end_matches('Z').to_string();
    let valid = norm.len() >= 10
        && norm.as_bytes().get(4) == Some(&b'-')
        && norm.as_bytes().get(7) == Some(&b'-')
        && norm.chars().all(|c| c.is_ascii_digit() || "-T:.".contains(c));
    if !valid {
        return Err(format!("{s:?}: expected a time like 2h, 30m, 3d, 2026-09-30 or 2026-09-30T12:00 (UTC)"));
    }
    // Complete to full precision so the string compares like the timestamps.
    let full = "0000-00-00T00:00:00.000";
    let mut out = norm.clone();
    if out.len() < full.len() {
        out.push_str(&full[out.len()..]);
    }
    if out.len() == 10 {
        out.push_str("T00:00:00.000");
    }
    out.truncate(full.len());
    out.push('Z');
    Ok(out)
}

/// Rotated siblings of `path` (numbered, dated, gzipped), oldest first, then
/// `path` itself.
pub fn file_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        let prefix = format!("{}.", name.to_string_lossy());
        if let Ok(rd) = std::fs::read_dir(dir) {
            let mut rotated: Vec<(SystemTime, PathBuf)> = rd
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().to_string();
                    n.starts_with(&prefix) && !n.ends_with(".tmp")
                })
                .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                .collect();
            rotated.sort();
            chain.extend(rotated.into_iter().map(|(_, p)| p));
        }
    }
    if path.exists() {
        chain.push(path.to_path_buf());
    }
    chain
}

/// Call `each` for every line of `path` (gzip or plain); stop when it
/// returns false.
pub fn for_each_line(path: &Path, each: &mut dyn FnMut(&str) -> bool) -> Result<bool, String> {
    let f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let reader: Box<dyn BufRead> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(std::io::BufReader::new(flate2::read::MultiGzDecoder::new(f)))
    } else {
        Box::new(std::io::BufReader::new(f))
    };
    for line in reader.split(b'\n') {
        let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
        if !each(&String::from_utf8_lossy(&line)) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// journald under systemd: the same framed lines as a log file
/// (`<ts> <message>`), built from `journalctl -o json`.
pub fn journal_lines(unit: &str, q: &Query, each: &mut dyn FnMut(&str) -> bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new("journalctl");
    cmd.args(["-u", unit, "-o", "json", "--no-pager", "-q"]);
    let to_journal = |t: &str| format!("{} UTC", t.trim_end_matches('Z').replace('T', " ").get(..19).unwrap_or(""));
    if let Some(s) = &q.since {
        cmd.arg(format!("--since={}", to_journal(s)));
    }
    if let Some(u) = &q.until {
        cmd.arg(format!("--until={}", to_journal(u)));
    }
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("running journalctl: {e}"))?;
    let Some(out) = child.stdout.take() else { return Err("journalctl gave no output".into()) };
    for line in std::io::BufReader::new(out).lines() {
        let Ok(line) = line else { break };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
        let usec: i64 = v["__REALTIME_TIMESTAMP"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0);
        let msg = match &v["MESSAGE"] {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(bytes) => {
                String::from_utf8_lossy(&bytes.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect::<Vec<_>>())
                    .to_string()
            }
            _ => continue,
        };
        let ts = crate::logging::format_rfc3339(usec / 1_000_000, ((usec % 1_000_000) / 1000) as u32);
        if !each(&format!("{ts} {msg}")) {
            let _ = child.kill();
            break;
        }
    }
    let _ = child.wait();
    Ok(())
}

/// A framed line as one JSON object for `jq`.
pub fn to_json(app: &str, line: &str) -> String {
    let mut o = serde_json::Map::new();
    o.insert("app".into(), app.into());
    let Some(ts) = timestamp_of(line) else {
        o.insert("message".into(), line.into());
        return serde_json::Value::Object(o).to_string();
    };
    o.insert("time".into(), ts.into());
    let rest = line.get(TS + 1..).unwrap_or("");
    if let Some(out) = rest.strip_prefix("OUT   worker=") {
        let (worker, rest) = out.split_once(' ').unwrap_or((out, ""));
        let (stream, text) = rest.split_once(": ").unwrap_or((rest, ""));
        o.insert("kind".into(), "output".into());
        o.insert("worker".into(), worker.into());
        o.insert("stream".into(), stream.into());
        o.insert("message".into(), text.into());
    } else {
        let (level, rest) = rest.split_once(' ').unwrap_or((rest, ""));
        let (msg, fields) = split_fields(rest.trim_start());
        o.insert("kind".into(), "event".into());
        o.insert("level".into(), level.to_ascii_lowercase().into());
        o.insert("message".into(), msg.into());
        let map: serde_json::Map<String, serde_json::Value> =
            fields.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
        o.insert("fields".into(), serde_json::Value::Object(map));
    }
    serde_json::Value::Object(o).to_string()
}

/// `worker ready worker=1 reason="exit code 1"` → ("worker ready", fields).
fn split_fields(s: &str) -> (String, Vec<(String, String)>) {
    // The message ends where the first ` key=` begins.
    let bytes = s.as_bytes();
    let mut cut = s.len();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b' ' {
            let key_end = s[i + 1..].find('=').map(|e| i + 1 + e);
            if let Some(e) = key_end {
                let key = &s[i + 1..e];
                if !key.is_empty() && key.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()) {
                    cut = i;
                    break;
                }
            }
        }
        i += 1;
    }
    let msg = s[..cut].to_string();
    let mut fields = Vec::new();
    let mut rest = s[cut..].trim_start();
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].to_string();
        let after = &rest[eq + 1..];
        let (val, next) = if let Some(q) = after.strip_prefix('"') {
            // Rust Debug quoting: \" and \\ escapes.
            let mut v = String::new();
            let mut chars = q.char_indices();
            let mut end = q.len();
            while let Some((j, c)) = chars.next() {
                match c {
                    '\\' => {
                        if let Some((_, n)) = chars.next() {
                            v.push(match n {
                                'n' => '\n',
                                't' => '\t',
                                other => other,
                            });
                        }
                    }
                    '"' => {
                        end = j + 1;
                        break;
                    }
                    c => v.push(c),
                }
            }
            (v, &q[end.min(q.len())..])
        } else {
            let end = after.find(' ').unwrap_or(after.len());
            (after[..end].to_string(), &after[end..])
        };
        fields.push((key, val));
        rest = next.trim_start();
    }
    (msg, fields)
}

/// Stdout for pipes: a closed pipe (`| head`) ends the command quietly.
pub struct PipeOut {
    out: std::io::BufWriter<std::io::Stdout>,
    pub closed: bool,
}

impl PipeOut {
    pub fn new() -> Self {
        PipeOut { out: std::io::BufWriter::new(std::io::stdout()), closed: false }
    }

    /// False once the reader has gone away.
    pub fn line(&mut self, s: &str) -> bool {
        if self.closed {
            return false;
        }
        if self.out.write_all(s.as_bytes()).and_then(|_| self.out.write_all(b"\n")).is_err() {
            self.closed = true;
        }
        !self.closed
    }

    pub fn flush(&mut self) {
        if self.out.flush().is_err() {
            self.closed = true;
        }
    }
}

impl Drop for PipeOut {
    fn drop(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EV: &str = "2026-09-30T12:00:01.123Z WARN  worker crashed worker=2 pid=41 reason=\"exit code 1\" uptime_s=3";
    const OUT: &str = "2026-09-30T12:00:02.000Z OUT   worker=1 stderr: Error: ECONNREFUSED db:5432";

    #[test]
    fn filters() {
        let q = Query { grep: vec!["econnrefused".into()], ignore_case: true, ..Default::default() };
        assert!(q.matches(OUT) && !q.matches(EV));
        let q = Query { level: Some(Level::Warn), ..Default::default() };
        assert!(q.matches(EV) && !q.matches(OUT), "--level shows Warden events only");
        let q = Query { since: Some("2026-09-30T12:00:01.500Z".into()), ..Default::default() };
        assert!(!q.matches(EV) && q.matches(OUT));
        let q = Query { until: Some("2026-09-30T12:00:01.500Z".into()), ..Default::default() };
        assert!(q.matches(EV) && !q.matches(OUT));
        let q = Query { exclude: vec!["crashed".into()], ..Default::default() };
        assert!(!q.matches(EV) && q.matches(OUT));
        let q = Query { stream: Some("stderr".into()), ..Default::default() };
        assert!(q.matches(OUT) && !q.matches(EV));
        let q = Query { worker: Some("2".into()), ..Default::default() };
        assert!(q.matches(EV) && !q.matches(OUT));
        // Raw app lines: text only; time only when they carry a timestamp.
        let q =
            Query { grep: vec!["job".into()], since: Some("2026-01-01T00:00:00.000Z".into()), ..Default::default() };
        assert!(q.matches_raw("job 42 done"));
    }

    #[test]
    fn times() {
        assert_eq!(parse_time("2026-09-30").unwrap(), "2026-09-30T00:00:00.000Z");
        assert_eq!(parse_time("2026-09-30 12:30").unwrap(), "2026-09-30T12:30:00.000Z");
        assert_eq!(parse_time("2026-09-30T12:30:05Z").unwrap(), "2026-09-30T12:30:05.000Z");
        let h = parse_time("2h").unwrap();
        assert!(h.len() == 24 && h.ends_with('Z'));
        assert!(parse_time("yesterday").is_err());
        assert!(parse_time("5x").is_err());
    }

    #[test]
    fn json_lines() {
        let v: serde_json::Value = serde_json::from_str(&to_json("api", EV)).unwrap();
        assert_eq!(v["level"], "warn");
        assert_eq!(v["message"], "worker crashed");
        assert_eq!(v["fields"]["reason"], "exit code 1");
        assert_eq!(v["fields"]["uptime_s"], "3");
        let v: serde_json::Value = serde_json::from_str(&to_json("api", OUT)).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["worker"].as_str(), v["stream"].as_str()),
            (Some("output"), Some("1"), Some("stderr"))
        );
        assert_eq!(v["message"], "Error: ECONNREFUSED db:5432");
        let v: serde_json::Value = serde_json::from_str(&to_json("api", "plain app line")).unwrap();
        assert_eq!(v["message"], "plain app line");
    }

    #[test]
    fn reads_rotated_and_gzipped_files_in_order() {
        let dir = std::env::temp_dir().join(format!("warden-logview-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("app.log");
        let gz = std::fs::File::create(dir.join("app.log.2.gz")).unwrap();
        let mut enc = flate2::write::GzEncoder::new(gz, flate2::Compression::default());
        enc.write_all(b"one\ntwo\n").unwrap();
        enc.finish().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("app.log.1"), "three\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&p, "four\n").unwrap();
        let mut seen = Vec::new();
        for f in file_chain(&p) {
            for_each_line(&f, &mut |l| {
                seen.push(l.to_string());
                true
            })
            .unwrap();
        }
        assert_eq!(seen, vec!["one", "two", "three", "four"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

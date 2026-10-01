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

/// One place `warden logs --history` reads, in the order they are read.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Lines framed by Warden (`<ts> LEVEL …`, `<ts> OUT   worker=…`):
    /// `[logging] file` or a background supervisor's log, rotated ones too.
    Framed(PathBuf),
    /// journald (the app runs as a systemd unit and logs nowhere else).
    Journal,
    /// Output as a worker wrote it: an out/err file and its rotated ones.
    /// `worker`: whose (per-worker files); None for a file all share.
    Raw { path: PathBuf, worker: Option<String>, stream: &'static str },
}

/// What `--history` knows about where an app logs.
pub struct Setup<'a> {
    pub logging: &'a crate::config::Logging,
    /// `[logging] file`, else the background supervisor's log, if any.
    pub framed: Option<PathBuf>,
    /// It runs as a systemd unit: journald has Warden's own lines.
    pub journal: bool,
    /// `[workers] count` in process mode, else 0 (one shared file is one
    /// worker's only with a single worker process).
    pub processes: usize,
}

/// Where `warden logs --history` finds the lines `q` asks for, in reading
/// order: Warden's own log first, then each worker's files (by worker
/// number, then standbys, then the worker-mode host; stdout before
/// stderr). Each file comes with its rotated and gzipped ones, oldest
/// first (`file_chain`). Err: nothing configured can answer, and why.
///
/// - Capture mode: `[logging] file` holds everything, each line tagged
///   with its worker and stream, so it answers alone. `--out`/`--err`
///   read the out/err files when set (as the app wrote them).
/// - Direct mode: worker output is only in the out/err files; Warden's
///   file (or journald) has its events. Both are read.
/// - Per-worker files (`out-2.log`) are found on disk, so a worker that
///   is gone (scaled down, an earlier run) is still read; `--worker N`
///   reads only worker N's.
pub fn sources(s: &Setup, q: &Query) -> Result<Vec<Source>, String> {
    use crate::config::WorkerOutput;
    let l = s.logging;
    let capture = l.worker_output == WorkerOutput::Capture;
    let direct = l.worker_output == WorkerOutput::Direct;
    // Warden's own lines: its file, else journald.
    let events: Vec<Source> = match &s.framed {
        Some(f) => vec![Source::Framed(f.clone())],
        None if s.journal => vec![Source::Journal],
        None => Vec::new(),
    };
    // Warden's log answers questions about worker output only in capture mode.
    let framed_output = capture && s.framed.is_some();
    let no_files = || {
        "no log files to read: set [logging] file (or out_file / err_file) to keep history; `warden logs` shows \
         the recent lines in memory"
            .to_string()
    };
    if q.events || q.level.is_some() {
        return if events.is_empty() { Err(no_files()) } else { Ok(events) };
    }
    let worker = q.worker.as_deref();
    let raw = |stream: &'static str| -> Option<Result<Vec<Source>, String>> {
        let base = match stream {
            "stdout" => l.out_file.as_ref()?,
            _ => l.err_file.as_ref()?,
        };
        if l.per_worker_files {
            let workers: Vec<Source> = crate::logging::worker_labels(base)
                .into_iter()
                .filter(|w| worker.is_none_or(|want| want == w || (want == "standby" && crate::control::is_standby(w))))
                .map(|w| Source::Raw { path: crate::logging::worker_file(base, true, &w), worker: Some(w), stream })
                .collect();
            return Some(Ok(workers));
        }
        match worker {
            Some(w) if !(s.processes == 1 && w == "1") => Some(Err(format!(
                "{} is shared by every worker and its lines don't say whose they are, so --worker {w} can't pick \
                 them out. Set [logging] per_worker_files = true{}",
                base.display(),
                if direct { "" } else { ", or [logging] file (each of its lines names the worker)" }
            ))),
            _ => Some(Ok(vec![Source::Raw { path: base.clone(), worker: None, stream }])),
        }
    };
    if let Some(stream) = q.stream.as_deref() {
        let stream: &'static str = if stream == "stderr" { "stderr" } else { "stdout" };
        return match raw(stream) {
            Some(Ok(v)) if !v.is_empty() => Ok(v),
            _ if framed_output => Ok(events),
            Some(Err(e)) => Err(e),
            Some(Ok(_)) => Err(match worker {
                Some(w) => format!("no {stream} file of worker {w} yet"),
                None => format!("no per-worker {stream} files yet"),
            }),
            None if direct && stream == "stderr" => Err(format!(
                "stderr goes into out_file together with stdout (no err_file is set), so it can't be read on its \
                 own; drop --err to read both, or set [logging] err_file to keep stderr apart{}",
                l.out_file.as_ref().map(|o| format!(" (out_file: {})", o.display())).unwrap_or_default()
            )),
            None if capture && !events.is_empty() => Ok(events),
            None => Err(format!(
                "no {stream} log file: set [logging] {} (or [logging] file) to keep history",
                if stream == "stderr" { "err_file" } else { "out_file" }
            )),
        };
    }
    if framed_output {
        return Ok(events);
    }
    // Direct mode (or files without Warden's log): Warden's events, then
    // each worker's out and err files.
    let (out, err) = (raw("stdout").transpose()?.unwrap_or_default(), raw("stderr").transpose()?.unwrap_or_default());
    let mut files: Vec<Source> = out.into_iter().chain(err).collect();
    let order = |src: &Source| match src {
        Source::Raw { worker, stream, .. } => {
            (worker.as_deref().map(crate::logging::label_order), u8::from(*stream == "stderr"))
        }
        _ => (None, 0),
    };
    // Stable: a shared file keeps its place; per worker, stdout then stderr.
    files.sort_by_key(|src| order(src));
    // Events from journald next to a capture mode's files would repeat
    // the output journald has too; only direct mode adds them.
    let events = if capture && s.framed.is_none() { Vec::new() } else { events };
    let all: Vec<Source> = events.into_iter().chain(files).collect();
    if all.is_empty() {
        return match worker {
            Some(w) if l.per_worker_files && (l.out_file.is_some() || l.err_file.is_some()) => {
                Err(format!("no log file of worker {w} yet"))
            }
            _ if s.journal => Ok(vec![Source::Journal]),
            _ => Err(no_files()),
        };
    }
    Ok(all)
}

/// A line of an out/err file, shown among other files' lines: it says
/// whose it is, framed like captured output. A line written with
/// `file_timestamps` (`<ts>: text`) keeps its time; others have none. A
/// file all workers share (`worker` None) names only the stream.
pub fn frame_raw(worker: Option<&str>, stream: &str, line: &str) -> String {
    let Some(worker) = worker else { return format!("{stream}: {line}") };
    match timestamp_of(line).and_then(|ts| Some((ts, line.get(TS..)?.strip_prefix(": ")?))) {
        Some((ts, text)) => format!("{ts} OUT   worker={worker} {stream}: {text}"),
        None => format!("worker={worker} {stream}: {line}"),
    }
}

/// Rotated siblings of `path` (numbered, dated, gzipped), oldest first, then
/// `path` itself.
pub fn file_chain(path: &Path) -> Vec<PathBuf> {
    let mut chain = Vec::new();
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        let prefix = format!("{}.", name.to_string_lossy());
        if let Ok(rd) = std::fs::read_dir(dir) {
            let names: Vec<String> = rd
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with(&prefix) && !n.ends_with(".tmp"))
                .collect();
            // (modified, rotation number if numbered, path)
            let mut rotated: Vec<(SystemTime, Option<u64>, PathBuf)> = names
                .iter()
                // Being compressed right now: `x` and the finished `x.gz`
                // both exist for a moment. Read the plain one, once.
                .filter(|n| !n.strip_suffix(".gz").is_some_and(|plain| names.iter().any(|m| m == plain)))
                .filter_map(|n| {
                    let p = dir.join(n);
                    let rest = &n[prefix.len()..];
                    let number = rest.strip_suffix(".gz").unwrap_or(rest).parse::<u64>().ok();
                    Some((std::fs::metadata(&p).ok()?.modified().ok()?, number, p))
                })
                .collect();
            // Numbered files (`app.log.3` is older than `app.log.1`) are
            // ordered by their number: rotations in one burst can share a
            // modification time (coarse timestamps on many kernels). Dated
            // ones by time, ties by number, then name.
            if rotated.iter().all(|r| r.1.is_some()) {
                rotated.sort_by_key(|r| std::cmp::Reverse(r.1));
            } else {
                rotated.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(a.2.cmp(&b.2)));
            }
            chain.extend(rotated.into_iter().map(|(_, _, p)| p));
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

/// The last `n` lines of `path` (read from at most `max_bytes` before its
/// end; a line cut at that start is dropped, a last line without `\n`
/// kept) and the file's modification time. Reads only what is in the page
/// cache (`WouldBlock` otherwise), so the supervisor can serve `warden
/// logs` from `worker_output = "direct"` files without waiting for a disk.
pub fn tail_lines(path: &Path, n: usize, max_bytes: u64) -> std::io::Result<(SystemTime, Vec<String>)> {
    use std::os::fd::AsFd;
    let f = std::fs::File::open(path)?;
    let meta = f.metadata()?;
    let mtime = meta.modified().unwrap_or(UNIX_EPOCH);
    if !meta.is_file() || n == 0 {
        return Ok((mtime, Vec::new()));
    }
    // One byte more than asked: a newline there means the first line is whole.
    let start = if meta.len() > max_bytes { meta.len() - max_bytes - 1 } else { 0 };
    let mut buf = vec![0u8; (meta.len() - start) as usize];
    let mut done = 0;
    while done < buf.len() {
        let at = start + done as u64;
        let k = match crate::sys::pread_nowait(f.as_fd(), &mut buf[done..], at) {
            Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
                std::os::unix::fs::FileExt::read_at(&f, &mut buf[done..], at)?
            }
            r => r?,
        };
        if k == 0 {
            break; // truncated meanwhile
        }
        done += k;
    }
    buf.truncate(done);
    let mut text = &buf[..];
    if start > 0 {
        text = match text.iter().position(|&b| b == b'\n') {
            Some(i) => &text[i + 1..],
            None => &[],
        };
    }
    let text = text.strip_suffix(b"\n").unwrap_or(text);
    if text.is_empty() {
        return Ok((mtime, Vec::new()));
    }
    let mut lines: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    let lines = lines.split_off(lines.len().saturating_sub(n));
    let lines =
        lines.iter().map(|l| String::from_utf8_lossy(l.strip_suffix(b"\r").unwrap_or(l)).into_owned()).collect();
    Ok((mtime, lines))
}

/// journald under systemd: the same framed lines as a log file
/// (`<ts> <message>`), built from `journalctl -o json`.
/// `user`: a unit of the user's own systemd manager (`warden startup --user`).
pub fn journal_lines(unit: &str, user: bool, q: &Query, each: &mut dyn FnMut(&str) -> bool) -> Result<(), String> {
    let mut cmd = std::process::Command::new("journalctl");
    if user {
        cmd.arg("--user");
    }
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

/// A line of a worker's out/err file as one JSON object: `kind` output,
/// `worker` when the file is one worker's, `stream`, and `time` when the
/// line was written with `file_timestamps`.
pub fn raw_to_json(app: &str, worker: Option<&str>, stream: &str, line: &str) -> String {
    let mut o = serde_json::Map::new();
    o.insert("app".into(), app.into());
    let text = match timestamp_of(line).and_then(|ts| Some((ts, line.get(TS..)?.strip_prefix(": ")?))) {
        Some((ts, text)) => {
            o.insert("time".into(), ts.into());
            text
        }
        None => line,
    };
    o.insert("kind".into(), "output".into());
    if let Some(w) = worker {
        o.insert("worker".into(), w.into());
    }
    o.insert("stream".into(), stream.into());
    o.insert("message".into(), text.into());
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
    fn tails_of_files() {
        let dir = std::env::temp_dir().join(format!("warden-tail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("out.log");
        let tail = |n, max| tail_lines(&p, n, max).map(|(_, l)| l);
        std::fs::write(&p, "").unwrap();
        assert!(tail(5, 1024).unwrap().is_empty());
        std::fs::write(&p, "one\ntwo\r\nthree\nno newline").unwrap();
        let _ = std::fs::read(&p); // in the page cache
        assert_eq!(tail(2, 1024).unwrap(), vec!["three", "no newline"]);
        assert_eq!(tail(10, 1024).unwrap(), vec!["one", "two", "three", "no newline"]);
        assert!(tail(0, 1024).unwrap().is_empty());
        std::fs::write(&p, "one\ntwo\nthree\n").unwrap();
        assert_eq!(tail(10, 1024).unwrap(), vec!["one", "two", "three"]);
        // Reading from the middle: the cut first line is dropped.
        assert_eq!(tail(10, 8).unwrap(), vec!["three"]);
        assert_eq!(tail(10, 6).unwrap(), vec!["three"], "starts right after a newline: whole");
        assert_eq!(tail(10, 5).unwrap(), Vec::<String>::new());
        assert_eq!(tail_lines(&dir.join("missing"), 5, 64).unwrap_err().kind(), std::io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn numbered_files_are_ordered_by_number_even_with_equal_times() {
        let dir = std::env::temp_dir().join(format!("warden-logview-num-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("app.log");
        let same = std::time::SystemTime::now() - Duration::from_secs(60);
        for (i, text) in [(12, "a\n"), (2, "c\n"), (11, "b\n")] {
            let f = dir.join(format!("app.log.{i}"));
            std::fs::write(&f, text).unwrap();
            std::fs::File::options().write(true).open(&f).unwrap().set_modified(same).unwrap();
        }
        std::fs::write(&p, "d\n").unwrap();
        let mut seen = Vec::new();
        for f in file_chain(&p) {
            for_each_line(&f, &mut |l| {
                seen.push(l.to_string());
                true
            })
            .unwrap();
        }
        assert_eq!(seen, vec!["a", "b", "c", "d"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Which files `--history` reads for each setup and query: per-worker
    /// files are found on disk (gone workers too), in worker order, and
    /// direct mode reads Warden's events next to the workers' files.
    #[test]
    fn history_sources() {
        use crate::config::{Logging, WorkerOutput};
        let dir = std::env::temp_dir().join(format!("warden-sources-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["out-1.log", "out-2.log", "out-10.log.1.gz", "out-s1.log", "err-1.log", "err-2.log.3"] {
            std::fs::write(dir.join(f), "x\n").unwrap();
        }
        let (out, err, warden) = (dir.join("out.log"), dir.join("err.log"), dir.join("warden.log"));
        let raw = |f: &str, w: Option<&str>, stream: &'static str| Source::Raw {
            path: dir.join(f),
            worker: w.map(String::from),
            stream,
        };
        let per = Logging {
            out_file: Some(out.clone()),
            err_file: Some(err.clone()),
            per_worker_files: true,
            ..Logging::default()
        };
        let direct = Logging { worker_output: WorkerOutput::Direct, ..per.clone() };
        let setup =
            |l, framed: Option<&PathBuf>| Setup { logging: l, framed: framed.cloned(), journal: false, processes: 2 };
        let q = |stream: Option<&str>, worker: Option<&str>| Query {
            stream: stream.map(String::from),
            worker: worker.map(String::from),
            ..Default::default()
        };

        // Direct mode: Warden's events, then each worker's out and err.
        assert_eq!(
            sources(&setup(&direct, Some(&warden)), &q(None, None)).unwrap(),
            vec![
                Source::Framed(warden.clone()),
                raw("out-1.log", Some("1"), "stdout"),
                raw("err-1.log", Some("1"), "stderr"),
                raw("out-2.log", Some("2"), "stdout"),
                raw("err-2.log", Some("2"), "stderr"),
                raw("out-10.log", Some("10"), "stdout"),
                raw("out-s1.log", Some("s1"), "stdout"),
            ]
        );
        // --out, --err, --worker.
        assert_eq!(
            sources(&setup(&direct, Some(&warden)), &q(Some("stdout"), None)).unwrap(),
            vec![
                raw("out-1.log", Some("1"), "stdout"),
                raw("out-2.log", Some("2"), "stdout"),
                raw("out-10.log", Some("10"), "stdout"),
                raw("out-s1.log", Some("s1"), "stdout"),
            ]
        );
        assert_eq!(
            sources(&setup(&direct, None), &q(Some("stderr"), Some("2"))).unwrap(),
            vec![raw("err-2.log", Some("2"), "stderr")]
        );
        assert_eq!(
            sources(&setup(&direct, Some(&warden)), &q(None, Some("10"))).unwrap(),
            vec![Source::Framed(warden.clone()), raw("out-10.log", Some("10"), "stdout")]
        );
        assert_eq!(
            sources(&setup(&direct, None), &q(Some("stdout"), Some("standby"))).unwrap(),
            vec![raw("out-s1.log", Some("s1"), "stdout")]
        );
        assert!(sources(&setup(&direct, None), &q(Some("stdout"), Some("7"))).unwrap_err().contains("worker 7"));
        // Events only: Warden's log.
        let events = Query { level: Some(Level::Warn), ..Default::default() };
        assert_eq!(sources(&setup(&direct, Some(&warden)), &events).unwrap(), vec![Source::Framed(warden.clone())]);

        // Capture mode: Warden's file has everything, tagged; --out reads the out files.
        assert_eq!(
            sources(&setup(&per, Some(&warden)), &q(None, Some("2"))).unwrap(),
            vec![Source::Framed(warden.clone())]
        );
        assert_eq!(
            sources(&setup(&per, Some(&warden)), &q(Some("stderr"), Some("1"))).unwrap(),
            vec![raw("err-1.log", Some("1"), "stderr")]
        );
        // ...and without it, the files, by worker.
        assert_eq!(
            sources(&setup(&per, None), &q(None, Some("1"))).unwrap(),
            vec![raw("out-1.log", Some("1"), "stdout"), raw("err-1.log", Some("1"), "stderr")]
        );

        // One shared file: whole, but it can't answer --worker (unless one worker).
        let shared = Logging { per_worker_files: false, ..per.clone() };
        assert_eq!(
            sources(&setup(&shared, None), &q(Some("stdout"), None)).unwrap(),
            vec![Source::Raw { path: out.clone(), worker: None, stream: "stdout" }]
        );
        let e = sources(&setup(&shared, None), &q(Some("stdout"), Some("2"))).unwrap_err();
        assert!(e.contains("per_worker_files") && e.contains("--worker 2"), "{e}");
        assert_eq!(
            sources(&setup(&shared, Some(&warden)), &q(Some("stdout"), Some("2"))).unwrap(),
            vec![Source::Framed(warden.clone())],
            "capture mode: Warden's file tags each line with its worker"
        );
        let one = Setup { logging: &shared, framed: None, journal: false, processes: 1 };
        assert_eq!(sources(&one, &q(Some("stdout"), Some("1"))).unwrap().len(), 1);

        // Direct mode without err_file: stderr is in out_file.
        let mixed = Logging { err_file: None, ..direct.clone() };
        let e = sources(&setup(&mixed, None), &q(Some("stderr"), None)).unwrap_err();
        assert!(e.contains("err_file"), "{e}");
        // Nothing configured: journald under systemd, else a hint.
        let none = Logging::default();
        let journal = Setup { logging: &none, framed: None, journal: true, processes: 1 };
        assert_eq!(sources(&journal, &q(None, None)).unwrap(), vec![Source::Journal]);
        assert!(sources(&setup(&none, None), &q(None, None)).unwrap_err().contains("[logging] file"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn raw_lines_say_whose_they_are() {
        assert_eq!(frame_raw(Some("2"), "stderr", "boom"), "worker=2 stderr: boom");
        assert_eq!(
            frame_raw(Some("2"), "stdout", "2026-09-30T12:00:01.123Z: hello"),
            "2026-09-30T12:00:01.123Z OUT   worker=2 stdout: hello"
        );
        assert_eq!(frame_raw(None, "stdout", "hello"), "stdout: hello");
        let q = Query { worker: Some("2".into()), stream: Some("stdout".into()), ..Default::default() };
        assert!(q.matches(&frame_raw(Some("2"), "stdout", "2026-09-30T12:00:01.123Z: hello")));
        let v: serde_json::Value = serde_json::from_str(&raw_to_json("api", Some("2"), "stderr", "boom")).unwrap();
        assert_eq!(
            (v["kind"].as_str(), v["worker"].as_str(), v["stream"].as_str()),
            (Some("output"), Some("2"), Some("stderr"))
        );
        assert_eq!((v["message"].as_str(), v.get("time")), (Some("boom"), None));
        let v: serde_json::Value =
            serde_json::from_str(&raw_to_json("api", None, "stdout", "2026-09-30T12:00:01.123Z: hi")).unwrap();
        assert_eq!((v["time"].as_str(), v["message"].as_str()), (Some("2026-09-30T12:00:01.123Z"), Some("hi")));
        assert!(v.get("worker").is_none());
    }

    #[test]
    fn per_worker_chains_keep_their_rotation_order() {
        // `out-1.log.` must not take `out-10.log.1`; each worker's chain is
        // its own rotated files, oldest first, numbered and dated, .gz too.
        let dir = std::env::temp_dir().join(format!("warden-logview-per-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let gz = |name: &str, text: &[u8]| {
            let f = std::fs::File::create(dir.join(name)).unwrap();
            let mut enc = flate2::write::GzEncoder::new(f, flate2::Compression::default());
            enc.write_all(text).unwrap();
            enc.finish().unwrap();
        };
        gz("out-1.log.2.gz", b"1a\n");
        std::fs::write(dir.join("out-1.log.1"), "1b\n").unwrap();
        std::fs::write(dir.join("out-1.log"), "1c\n").unwrap();
        std::fs::write(dir.join("out-10.log.1"), "10a\n").unwrap();
        std::fs::write(dir.join("out-10.log"), "10b\n").unwrap();
        let read = |p: &Path| {
            let mut seen = Vec::new();
            for f in file_chain(p) {
                for_each_line(&f, &mut |l| {
                    seen.push(l.to_string());
                    true
                })
                .unwrap();
            }
            seen
        };
        assert_eq!(read(&dir.join("out-1.log")), vec!["1a", "1b", "1c"]);
        assert_eq!(read(&dir.join("out-10.log")), vec!["10a", "10b"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_being_compressed_is_read_once() {
        // Between the rename of `x.gz` and the removal of `x`, both exist.
        let dir = std::env::temp_dir().join(format!("warden-logview-gz-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("app.log");
        std::fs::write(dir.join("app.log.1"), "old\n").unwrap();
        let gz = std::fs::File::create(dir.join("app.log.1.gz")).unwrap();
        let mut enc = flate2::write::GzEncoder::new(gz, flate2::Compression::default());
        enc.write_all(b"old\n").unwrap();
        enc.finish().unwrap();
        std::fs::write(&p, "new\n").unwrap();
        let mut seen = Vec::new();
        for f in file_chain(&p) {
            for_each_line(&f, &mut |l| {
                seen.push(l.to_string());
                true
            })
            .unwrap();
        }
        assert_eq!(seen, vec!["old", "new"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

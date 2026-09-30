//! Line-oriented, journald-friendly logging.
//!
//! `2026-09-30T12:00:01.123Z INFO worker ready worker=1 pid=4242`
//!
//! Under journald (`JOURNAL_STREAM` set) timestamps are dropped and each
//! supervisor line gets a `<N>` syslog priority prefix, so `journalctl -p warning`
//! works. Worker output is passed through with a `worker=N` prefix. The last
//! lines are kept in memory for `warden logs`. Optionally the log is also
//! written to a file, rotated by size (`[logging] file`).
//!
//! Writing never blocks supervision (CP5): lines go to a writer thread
//! through one queue with separate bounds for Warden's events (4096 lines)
//! and worker output (8192 lines or 4 MB). If stdout can't keep up
//! (journald stalled, a slow pipe), worker output fills its budget and is
//! dropped first, counted; Warden's events keep their own budget. Drops are
//! reported in the log once stdout recovers, in `warden status` and as a
//! metric.
//!
//! Cost per line on the event loop: one formatted allocation shared (`Arc`)
//! by the queue, the in-memory ring and `logs -f` followers; sink formatting
//! and writes happen on the writer thread, batched.

use crate::config::Level;
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
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
/// `2026-09-30T12:00:01.123Z ` — every stored line starts with this.
const TS_LEN: usize = 25;

pub type Line = Arc<str>;

/// One FIFO to the writer thread keeps lines in order and wakes it at once;
/// the bounds are enforced with counters before a line is queued.
enum Queued {
    Event(Level, Line),
    Output(Batch),
}

/// Worker output lines from one read of one worker's pipe: same worker,
/// stream and timestamp, so they travel (and are counted, stored and
/// written) together instead of one queue message and lock per line.
struct Batch {
    worker: Arc<str>,
    stream: &'static str,
    /// Where the app's text starts in each line (the prefix is shared).
    text_at: usize,
    lines: Vec<Line>,
    bytes: usize,
}

/// Builds a [`Batch`]: `OutputBatch::new`, `push` each line, then
/// [`worker_output_batch`].
pub struct OutputBatch {
    prefix: String,
    batch: Batch,
}

impl OutputBatch {
    /// Lines `worker` wrote to `stream` ("stdout"/"stderr") just now.
    pub fn new(worker: &str, stream: &'static str) -> Self {
        let mut prefix = String::with_capacity(TS_LEN + 24 + worker.len());
        push_timestamp_now(&mut prefix);
        for part in [" OUT   worker=", worker, " ", stream, ": "] {
            prefix.push_str(part);
        }
        let batch = Batch { worker: worker.into(), stream, text_at: prefix.len(), lines: Vec::new(), bytes: 0 };
        OutputBatch { prefix, batch }
    }

    pub fn push(&mut self, text: &str) {
        let mut line = String::with_capacity(self.prefix.len() + text.len());
        line.push_str(&self.prefix);
        line.push_str(text);
        self.batch.bytes += line.len();
        self.batch.lines.push(line.into());
    }

    pub fn len(&self) -> usize {
        self.batch.lines.len()
    }

    pub fn bytes(&self) -> usize {
        self.batch.bytes
    }
}

/// For `max_lines_per_sec = 0` (keep everything): wait until the writer's
/// queue has room for `lines`/`bytes`, at most `max`. Meanwhile the worker's
/// pipe fills and its writes block, which slows a flooding app down to what
/// the log sinks take instead of dropping lines. `max` bounds the wait so a
/// stuck sink (stdout nobody reads) slows apps but can't freeze them.
pub async fn room_for(lines: usize, bytes: usize, max: Duration) {
    let l = logger();
    if l.writer.is_none() {
        return;
    }
    let t0 = Instant::now();
    loop {
        let (q, b) = (l.output_queued.load(Ordering::Relaxed), l.output_bytes.load(Ordering::Relaxed));
        if q == 0 || (q + lines <= OUTPUT_QUEUE && b + bytes <= OUTPUT_QUEUE_BYTES) || t0.elapsed() >= max {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

struct Writer {
    tx: Sender<Queued>,
}

/// Where the log goes besides memory; owned by the writer thread.
pub struct Sinks {
    pub stdout: bool,
    pub timestamps: bool,
    pub journald: bool,
    /// Everything: Warden's events and worker output.
    pub file: Option<FileSink>,
    /// Worker stdout / stderr as the app wrote it (PM2's out_file / error_file).
    pub out: Option<StreamFiles>,
    pub err: Option<StreamFiles>,
    /// Prefix lines in `out` / `err` with a timestamp (PM2's `time`).
    pub stream_timestamps: bool,
}

/// Log files settings (from `[logging]`).
#[derive(Debug, Clone, Default)]
pub struct Files {
    pub file: Option<PathBuf>,
    pub out_file: Option<PathBuf>,
    pub err_file: Option<PathBuf>,
    /// One file per worker (`out-1.log`), like PM2 without `merge_logs`.
    pub per_worker: bool,
    pub timestamps: bool,
    pub rotate: RotatePolicy,
}

/// When and how log files rotate (`[logging.rotate]`).
#[derive(Debug, Clone)]
pub struct RotatePolicy {
    /// Bytes; 0 = never by size.
    pub max_size: u64,
    pub keep: u32,
    pub interval: Option<crate::schedule::Cron>,
    pub compress: bool,
    pub date_suffix: bool,
    pub max_age: Option<Duration>,
}

impl Default for RotatePolicy {
    fn default() -> Self {
        RotatePolicy::from(&crate::config::Rotate::default())
    }
}

impl From<&crate::config::Rotate> for RotatePolicy {
    fn from(r: &crate::config::Rotate) -> Self {
        RotatePolicy {
            max_size: r.max_size,
            keep: r.keep,
            interval: r.interval.as_deref().and_then(|e| crate::schedule::Cron::parse(e).ok()),
            compress: r.compress,
            date_suffix: r.date_suffix,
            max_age: (r.max_age_days > 0).then(|| Duration::from_secs(r.max_age_days * 86_400)),
        }
    }
}

/// Worker output of one stream: one file, or one per worker.
pub struct StreamFiles {
    base: PathBuf,
    per_worker: bool,
    policy: RotatePolicy,
    files: std::collections::HashMap<String, FileSink>,
}

impl StreamFiles {
    fn new(base: PathBuf, per_worker: bool, policy: RotatePolicy) -> Self {
        StreamFiles { base, per_worker, policy, files: std::collections::HashMap::new() }
    }

    /// `out.log` → `out-2.log` for worker 2.
    fn path_for(&self, worker: &str) -> PathBuf {
        if !self.per_worker {
            return self.base.clone();
        }
        let stem = self.base.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let name = match self.base.extension() {
            Some(e) => format!("{stem}-{worker}.{}", e.to_string_lossy()),
            None => format!("{stem}-{worker}"),
        };
        self.base.with_file_name(name)
    }

    /// The file for `worker`'s lines (the shared one unless per-worker).
    fn sink(&mut self, worker: &str) -> &mut FileSink {
        let key = if self.per_worker { worker } else { "" };
        let path = self.path_for(worker);
        let policy = &self.policy;
        self.files.entry(key.to_string()).or_insert_with(|| FileSink::new(path, policy.clone()))
    }
}

/// `<ts> OUT   worker=<w> <stream>: <text>` → (w, stream, text). The
/// writer gets these from the batch; this is the format `warden logs`
/// readers rely on.
#[cfg(test)]
fn split_output(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.get(TS_LEN..)?.strip_prefix("OUT   worker=")?;
    let (worker, rest) = rest.split_once(' ')?;
    let (stream, text) = rest.split_once(": ")?;
    Some((worker, stream, text))
}

struct Logger {
    level: AtomicU8,
    rings: Mutex<Rings>,
    tx: broadcast::Sender<Line>,
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
    file_path: Option<PathBuf>,
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
    lines: VecDeque<(u64, Line)>,
    bytes: usize,
}

impl Ring {
    fn push(&mut self, seq: u64, line: Line) {
        self.bytes += line.len();
        self.lines.push_back((seq, line));
        while self.lines.len() > RING || (self.bytes > RING_BYTES && self.lines.len() > 1) {
            if let Some((_, old)) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }
    fn clear(&mut self) {
        self.lines.clear();
        self.bytes = 0;
    }
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// Start logging. `files`: also write to these files, rotated per `rotate`,
/// keeping `keep` old files. `$WARDEN_LOG_FILE` overrides the file and
/// `WARDEN_LOG_STDOUT=0` turns stdout off (both set by `warden start <app>`
/// when it runs a supervisor in the background).
pub fn init(level: Level, timestamps: Option<bool>, mut files: Files) {
    let journald = std::env::var_os("JOURNAL_STREAM").is_some();
    // A background supervisor (`warden start <app>`) logs to a file of its
    // own, unless the config already names one.
    if files.file.is_none() {
        files.file = std::env::var_os("WARDEN_LOG_FILE").filter(|p| !p.is_empty()).map(PathBuf::from);
    }
    let sinks = Sinks {
        stdout: std::env::var("WARDEN_LOG_STDOUT").map(|v| v != "0").unwrap_or(true),
        timestamps: timestamps.unwrap_or(!journald),
        journald,
        file: files.file.clone().map(|p| FileSink::new(p, files.rotate.clone())),
        out: files.out_file.clone().map(|p| StreamFiles::new(p, files.per_worker, files.rotate.clone())),
        err: files.err_file.clone().map(|p| StreamFiles::new(p, files.per_worker, files.rotate.clone())),
        stream_timestamps: files.timestamps,
    };
    let file = files.file;
    let (tx, _) = broadcast::channel(256);
    let (wtx, wrx) = channel::<Queued>();
    let ok = LOGGER
        .set(Logger {
            level: AtomicU8::new(level as u8),
            rings: Mutex::new(Rings::default()),
            tx,
            writer: Some(Writer { tx: wtx }),
            pending: AtomicUsize::new(0),
            events_queued: AtomicUsize::new(0),
            output_queued: AtomicUsize::new(0),
            output_bytes: AtomicUsize::new(0),
            dropped_output: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            file_path: file,
        })
        .is_ok();
    if ok {
        let spawned = std::thread::Builder::new().name("warden-log".into()).spawn(move || write_loop(wrx, sinks));
        if let Err(e) = spawned {
            eprintln!("warden: cannot start the log writer thread ({e}); log lines are kept in memory only");
        }
    }
}

fn logger() -> &'static Logger {
    LOGGER.get_or_init(|| {
        let (tx, _) = broadcast::channel(16);
        Logger {
            level: AtomicU8::new(Level::Info as u8),
            rings: Mutex::new(Rings::default()),
            tx,
            writer: None,
            pending: AtomicUsize::new(0),
            events_queued: AtomicUsize::new(0),
            output_queued: AtomicUsize::new(0),
            output_bytes: AtomicUsize::new(0),
            dropped_output: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            file_path: None,
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

/// The log file, if one is written (`[logging] file`).
pub fn file_path() -> Option<PathBuf> {
    logger().file_path.clone()
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
    let mut line = String::with_capacity(TS_LEN + 96);
    push_timestamp_now(&mut line);
    let _ = write!(line, " {:<5} {msg}", level_name(level));
    for (k, v) in fields {
        let v = v.to_string();
        if v.is_empty() || v.contains(char::is_whitespace) || v.contains('"') || v.contains('=') {
            let _ = write!(line, " {k}={v:?}");
        } else {
            let _ = write!(line, " {k}={v}");
        }
    }
    emit(l, line.into(), Some(level));
}

/// Worker output lines: into memory (for `warden logs`), to followers,
/// and queued for the writer thread, as far as the queue bounds allow
/// (lines past them are counted as dropped, never waited for).
pub fn worker_output_batch(b: OutputBatch) {
    let l = logger();
    let mut batch = b.batch;
    if batch.lines.is_empty() {
        return;
    }
    {
        let mut rings = l.rings.lock().unwrap_or_else(|e| e.into_inner());
        for line in &batch.lines {
            rings.seq += 1;
            let seq = rings.seq;
            rings.output.push(seq, line.clone());
        }
    }
    if l.tx.receiver_count() > 0 {
        for line in &batch.lines {
            let _ = l.tx.send(line.clone());
        }
    }
    let Some(w) = &l.writer else {
        // Before `init` (CLI commands, tests): plain stdout.
        let mut out = std::io::stdout().lock();
        for line in &batch.lines {
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
        }
        return;
    };
    // Keep the lines that fit under both bounds; count the rest. An empty
    // queue takes the whole batch: one read (64 KB of short lines can be
    // more than OUTPUT_QUEUE lines) is always small in bytes.
    let (queued, qbytes) = (l.output_queued.load(Ordering::Relaxed), l.output_bytes.load(Ordering::Relaxed));
    let (mut fit, mut bytes) = (0, 0);
    if queued == 0 && batch.bytes <= OUTPUT_QUEUE_BYTES {
        (fit, bytes) = (batch.lines.len(), batch.bytes);
    } else {
        for line in &batch.lines {
            if queued + fit >= OUTPUT_QUEUE || qbytes + bytes + line.len() > OUTPUT_QUEUE_BYTES {
                break;
            }
            fit += 1;
            bytes += line.len();
        }
    }
    let dropped = batch.lines.len() - fit;
    if dropped > 0 {
        l.dropped_output.fetch_add(dropped as u64, Ordering::Relaxed);
        batch.lines.truncate(fit);
        batch.bytes = bytes;
    }
    if fit == 0 {
        return;
    }
    l.output_queued.fetch_add(fit, Ordering::Relaxed);
    l.output_bytes.fetch_add(bytes, Ordering::Relaxed);
    l.pending.fetch_add(fit, Ordering::Relaxed);
    if w.tx.send(Queued::Output(batch)).is_err() {
        // Writer thread gone (only if it panicked): count and move on.
        l.output_queued.fetch_sub(fit, Ordering::Relaxed);
        l.output_bytes.fetch_sub(bytes, Ordering::Relaxed);
        l.pending.fetch_sub(fit, Ordering::Relaxed);
        l.dropped_output.fetch_add(fit as u64, Ordering::Relaxed);
    }
}

fn emit(l: &Logger, line: Line, level: Option<Level>) {
    match (&l.writer, level) {
        (Some(w), Some(lv)) => {
            if l.events_queued.load(Ordering::Relaxed) >= EVENT_QUEUE {
                l.dropped_events.fetch_add(1, Ordering::Relaxed);
            } else {
                l.events_queued.fetch_add(1, Ordering::Relaxed);
                l.pending.fetch_add(1, Ordering::Relaxed);
                if w.tx.send(Queued::Event(lv, line.clone())).is_err() {
                    // Writer thread gone (only if it panicked): count and move on.
                    l.events_queued.fetch_sub(1, Ordering::Relaxed);
                    l.pending.fetch_sub(1, Ordering::Relaxed);
                    l.dropped_events.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        // Worker output goes through `worker_output_batch`.
        (Some(_), None) => {}
        // Before `init` (CLI commands, tests): plain stdout.
        (None, _) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(line.as_bytes());
            let _ = out.write_all(b"\n");
        }
    }
    {
        let mut rings = l.rings.lock().unwrap_or_else(|e| e.into_inner());
        rings.seq += 1;
        let seq = rings.seq;
        let ring = if level.is_some() { &mut rings.events } else { &mut rings.output };
        ring.push(seq, line.clone());
    }
    // No subscriber (`warden logs -f`) is the normal case, not an error.
    let _ = l.tx.send(line);
}

/// Empty the in-memory buffers (`warden flush`); files are truncated by the
/// writer thread.
pub fn clear() {
    let l = logger();
    {
        let mut rings = l.rings.lock().unwrap_or_else(|e| e.into_inner());
        rings.events.clear();
        rings.output.clear();
    }
    if let Some(p) = &l.file_path {
        // Truncating in place keeps the writer's O_APPEND handle valid.
        if let Err(e) = std::fs::OpenOptions::new().write(true).truncate(true).open(p) {
            crate::warn!("could not truncate the log file", path = p.display(), error = e);
        }
    }
}

/// A log file rotated by size and/or on a schedule. Numbered
/// (`app.log.1` newest … `app.log.N`) or dated (`app.log.2026-09-30T00-00-00`),
/// optionally gzipped; old ones are pruned by count and age.
pub struct FileSink {
    path: PathBuf,
    file: Option<std::fs::File>,
    /// Lines not yet written: one write(2) per batch instead of two per
    /// line. `flush` empties it; the writer thread flushes after every
    /// batch, and rotation and drop flush first.
    buf: Vec<u8>,
    /// Bytes in the file plus those in `buf`.
    size: u64,
    policy: RotatePolicy,
    next_rotation: Option<SystemTime>,
    /// Last time an error was reported (at most once a minute).
    last_error: Option<Instant>,
    /// The previous rotation's compression, finished before names shift again.
    compressing: Option<std::thread::JoinHandle<()>>,
}

impl FileSink {
    pub fn new(path: PathBuf, policy: RotatePolicy) -> Self {
        let next_rotation = policy.interval.as_ref().and_then(|c| c.next_after(SystemTime::now()));
        FileSink {
            path,
            file: None,
            buf: Vec::new(),
            size: 0,
            policy,
            next_rotation,
            last_error: None,
            compressing: None,
        }
    }

    fn open(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = std::fs::OpenOptions::new().create(true).append(true).mode(0o640).open(&self.path)?;
        self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(f);
        Ok(())
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(suffix);
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.write_buf()?;
        self.file = None;
        if let Some(h) = self.compressing.take() {
            let _ = h.join();
        }
        let p = &self.policy;
        let rotated = if p.keep == 0 && !p.date_suffix {
            std::fs::remove_file(&self.path).or_else(ignore_missing)?;
            None
        } else if p.date_suffix {
            let stamp = timestamp_now().get(..19).unwrap_or("").replace(':', "-");
            let mut dest = self.sibling(&format!(".{stamp}"));
            let mut n = 1;
            while dest.exists() || self.sibling(&format!(".{stamp}.gz")).exists() && n == 1 {
                dest = self.sibling(&format!(".{stamp}-{n}"));
                n += 1;
            }
            std::fs::rename(&self.path, &dest).or_else(ignore_missing)?;
            Some(dest)
        } else {
            let numbered = |i: u32, gz: bool| self.sibling(&format!(".{i}{}", if gz { ".gz" } else { "" }));
            for gz in [false, true] {
                std::fs::remove_file(numbered(p.keep, gz)).or_else(ignore_missing)?;
                for i in (1..p.keep).rev() {
                    std::fs::rename(numbered(i, gz), numbered(i + 1, gz)).or_else(ignore_missing)?;
                }
            }
            let dest = numbered(1, false);
            std::fs::rename(&self.path, &dest).or_else(ignore_missing)?;
            Some(dest)
        };
        self.prune();
        if let (Some(done), true) = (rotated, self.policy.compress) {
            // Off the writer thread: a 10 MB file takes ~100 ms to compress.
            self.compressing = std::thread::Builder::new()
                .name("warden-gzip".into())
                .spawn(move || {
                    if let Err(e) = gzip_file(&done) {
                        event(
                            Level::Warn,
                            "could not compress a rotated log file; it stays uncompressed",
                            &[("file", &done.display()), ("error", &e)],
                        );
                    }
                })
                .ok();
        }
        self.open()
    }

    /// Rotated files beyond `keep`, or older than `max_age`, are deleted.
    fn prune(&self) {
        let (Some(dir), Some(name)) = (self.path.parent(), self.path.file_name()) else { return };
        let prefix = format!("{}.", name.to_string_lossy());
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut old: Vec<(SystemTime, PathBuf)> = rd
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .filter(|e| !e.file_name().to_string_lossy().contains(".tmp"))
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        old.sort_by_key(|o| std::cmp::Reverse(o.0)); // newest first
        let now = SystemTime::now();
        for (i, (mtime, path)) in old.iter().enumerate() {
            let too_many = i >= self.policy.keep as usize;
            let too_old = self.policy.max_age.is_some_and(|age| now.duration_since(*mtime).is_ok_and(|d| d > age));
            if too_many || too_old {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// Append one line; rotates first when the size or the schedule says so.
    pub fn write(&mut self, line: &[u8]) -> Result<(), String> {
        self.write_parts(&[line])
    }

    /// Append one line made of `parts`.
    pub fn write_parts(&mut self, parts: &[&[u8]]) -> Result<(), String> {
        let needed = parts.iter().map(|p| p.len() as u64).sum::<u64>() + 1;
        let res = (|| -> std::io::Result<()> {
            if self.file.is_none() {
                self.open()?;
            }
            let due = self.next_rotation.is_some_and(|t| SystemTime::now() >= t);
            if due {
                self.next_rotation = self.policy.interval.as_ref().and_then(|c| c.next_after(SystemTime::now()));
                if self.size > 0 {
                    self.rotate()?;
                }
            } else if self.policy.max_size > 0 && self.size > 0 && self.size + needed > self.policy.max_size {
                // `warden flush` may have truncated it: trust the file, not the count.
                self.write_buf()?;
                self.size = self.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(self.size);
                if self.size > 0 && self.size + needed > self.policy.max_size {
                    self.rotate()?;
                }
            }
            if self.file.is_some() {
                for p in parts {
                    self.buf.extend_from_slice(p);
                }
                self.buf.push(b'\n');
                self.size += needed;
                if self.buf.len() >= FILE_BUFFER {
                    self.write_buf()?;
                }
            }
            Ok(())
        })();
        res.map_err(|e| {
            self.file = None;
            format!("{}: {e}", self.path.display())
        })
    }

    /// Write out buffered lines.
    pub fn flush(&mut self) -> Result<(), String> {
        self.write_buf().map_err(|e| {
            self.file = None;
            format!("{}: {e}", self.path.display())
        })
    }

    fn write_buf(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let res = match self.file.as_mut() {
            Some(f) => f.write_all(&self.buf),
            None => Ok(()),
        };
        // Written or not, these lines are done: a failing disk must not make
        // the buffer grow without bound.
        self.buf.clear();
        if self.buf.capacity() > 4 * FILE_BUFFER {
            self.buf.shrink_to(FILE_BUFFER);
        }
        res
    }

    fn should_report(&mut self) -> bool {
        let due = self.last_error.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
        if due {
            self.last_error = Some(Instant::now());
        }
        due
    }
}

impl Drop for FileSink {
    fn drop(&mut self) {
        let _ = self.write_buf();
    }
}

/// Buffered log bytes per file before a write(2), within one batch.
const FILE_BUFFER: usize = 64 * 1024;

/// `file` → `file.gz` (written to a temp name first), then `file` removed.
fn gzip_file(path: &std::path::Path) -> std::io::Result<()> {
    let mut gz_name = path.as_os_str().to_owned();
    gz_name.push(".gz");
    let dest = PathBuf::from(gz_name);
    let mut tmp_name = dest.clone().into_os_string();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        let mut input = std::fs::File::open(path)?;
        let out = std::fs::File::create(&tmp)?;
        let mut enc = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        std::io::copy(&mut input, &mut enc)?;
        enc.finish()?.sync_all()?;
    }
    std::fs::rename(&tmp, &dest)?;
    std::fs::remove_file(path)
}

fn ignore_missing(e: std::io::Error) -> std::io::Result<()> {
    if e.kind() == std::io::ErrorKind::NotFound { Ok(()) } else { Err(e) }
}

/// Format for stdout: journald gets `<priority>` and no timestamp.
fn stdout_form<'a>(
    journald: bool,
    timestamps: bool,
    level: Option<Level>,
    line: &'a str,
    buf: &'a mut String,
) -> &'a str {
    let body = line.get(TS_LEN..).unwrap_or(line);
    if journald {
        match level {
            Some(lv) => {
                buf.clear();
                let _ = write!(buf, "<{}>{body}", syslog_priority(lv));
                buf
            }
            None => body,
        }
    } else if timestamps {
        line
    } else {
        body
    }
}

/// A line to stdout in its sink's form. A closed or failing stdout must
/// not take Warden down; the line is still in memory for `warden logs`.
fn to_stdout(out: &mut impl Write, form: (bool, bool), level: Option<Level>, line: &str, scratch: &mut String) {
    let text = stdout_form(form.0, form.1, level, line, scratch);
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
}

/// A write error, kept for one report after the batch (at most one a
/// minute per file).
fn note_error(f: &mut FileSink, res: Result<(), String>, error: &mut Option<String>) {
    if let Err(e) = res {
        if f.should_report() {
            *error = Some(e);
        }
    }
}

/// The writer thread: lines in the order they were logged, written in
/// batches (one flush per burst). After a period of drops, one line says
/// how many and why.
fn write_loop(rx: Receiver<Queued>, sinks: Sinks) {
    let Sinks {
        stdout: use_stdout,
        timestamps,
        journald,
        mut file,
        out: mut out_files,
        err: mut err_files,
        stream_timestamps,
    } = sinks;
    let form = (journald, timestamps);
    let l = logger();
    let mut reported = (0u64, 0u64);
    let mut last_report = Instant::now();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::with_capacity(64 * 1024, stdout.lock());
    let mut scratch = String::new();
    let mut file_error: Option<String> = None;
    loop {
        let first = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(q) => Some(q),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        // What is already queued is written before one flush (at most 256
        // messages, so drop reports still go out under a constant stream).
        for q in first.into_iter().chain(std::iter::from_fn(|| rx.try_recv().ok())).take(256) {
            match q {
                Queued::Event(lv, line) => {
                    if use_stdout {
                        to_stdout(&mut out, form, Some(lv), &line, &mut scratch);
                    }
                    if let Some(f) = file.as_mut() {
                        let res = f.write(line.as_bytes());
                        note_error(f, res, &mut file_error);
                    }
                    l.events_queued.fetch_sub(1, Ordering::Relaxed);
                    l.pending.fetch_sub(1, Ordering::Relaxed);
                }
                Queued::Output(batch) => {
                    // The out/err file for this worker, looked up once per batch.
                    let files = if batch.stream == "stderr" { err_files.as_mut() } else { out_files.as_mut() };
                    let mut target = files.map(|t| t.sink(&batch.worker));
                    for line in &batch.lines {
                        if use_stdout {
                            to_stdout(&mut out, form, None, line, &mut scratch);
                        }
                        if let Some(f) = file.as_mut() {
                            let res = f.write(line.as_bytes());
                            note_error(f, res, &mut file_error);
                        }
                        if let Some(t) = target.as_deref_mut() {
                            let text = line.get(batch.text_at..).unwrap_or("").as_bytes();
                            let res = if stream_timestamps {
                                let stamp = line.get(..TS_LEN - 1).unwrap_or("").as_bytes();
                                t.write_parts(&[stamp, b": ", text])
                            } else {
                                t.write_parts(&[text])
                            };
                            note_error(t, res, &mut file_error);
                        }
                    }
                    let n = batch.lines.len();
                    l.output_queued.fetch_sub(n, Ordering::Relaxed);
                    l.output_bytes.fetch_sub(batch.bytes, Ordering::Relaxed);
                    l.pending.fetch_sub(n, Ordering::Relaxed);
                }
            }
        }
        let _ = out.flush();
        if let Some(f) = file.as_mut() {
            let res = f.flush();
            note_error(f, res, &mut file_error);
        }
        for t in [out_files.as_mut(), err_files.as_mut()].into_iter().flatten() {
            for f in t.files.values_mut() {
                let res = f.flush();
                note_error(f, res, &mut file_error);
            }
        }
        if let Some(e) = file_error.take() {
            event(
                Level::Error,
                "cannot write the log file; lines go to stdout and memory only",
                &[("error", &e), ("hint", &"check [logging] file: the directory must exist and be writable")],
            );
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
pub fn recent_matching(n: usize, keep: &dyn Fn(&str) -> bool) -> Vec<Line> {
    let rings = logger().rings.lock().unwrap_or_else(|e| e.into_inner());
    merge_newest(&rings.events, &rings.output, n, keep)
}

/// The newest `n` lines of both rings that pass `keep`, oldest first.
fn merge_newest(a: &Ring, b: &Ring, n: usize, keep: &dyn Fn(&str) -> bool) -> Vec<Line> {
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

pub fn subscribe() -> broadcast::Receiver<Line> {
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
    let mut s = String::with_capacity(TS_LEN);
    push_timestamp_now(&mut s);
    s
}

fn push_timestamp_now(out: &mut String) {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    push_rfc3339(out, d.as_secs() as i64, d.subsec_millis());
}

/// RFC 3339 UTC with milliseconds, without pulling in a date crate.
pub fn format_rfc3339(secs: i64, millis: u32) -> String {
    let mut s = String::with_capacity(TS_LEN);
    push_rfc3339(&mut s, secs, millis);
    s
}

fn push_rfc3339(out: &mut String, secs: i64, millis: u32) {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let _ = write!(out, "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z", rem / 3600, (rem % 3600) / 60, rem % 60);
}

// Howard Hinnant's days-to-civil algorithm.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
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
    use std::path::Path;

    #[test]
    fn ring_is_bounded_by_lines_and_bytes() {
        let mut r = Ring::default();
        for i in 0..RING + 10 {
            r.push(i as u64, format!("line {i}").into());
        }
        assert_eq!(r.lines.len(), RING);
        assert_eq!(r.lines.front().map(|(_, l)| &**l), Some("line 10"));
        let mut r = Ring::default();
        for i in 0..300 {
            r.push(i, "x".repeat(16 * 1024).into());
        }
        assert!(r.bytes <= RING_BYTES, "{} bytes kept", r.bytes);
        assert_eq!(r.bytes, r.lines.iter().map(|(_, l)| l.len()).sum::<usize>());
        let mut r = Ring::default();
        r.push(1, "y".repeat(RING_BYTES + 1).into());
        assert_eq!(r.lines.len(), 1, "one oversized line is still kept");
    }

    #[test]
    fn rings_merge_in_order_and_filter() {
        let (mut ev, mut out) = (Ring::default(), Ring::default());
        for seq in 1..=10u64 {
            let ring = if seq % 3 == 0 { &mut ev } else { &mut out };
            ring.push(seq, format!("l{seq}").into());
        }
        let all = |_: &str| true;
        let strs = |v: Vec<Line>| v.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(strs(merge_newest(&ev, &out, 4, &all)), vec!["l7", "l8", "l9", "l10"]);
        assert_eq!(merge_newest(&ev, &out, 100, &all).len(), 10);
        let odd = |l: &str| l.ends_with(['1', '3', '5', '7', '9']);
        assert_eq!(strs(merge_newest(&ev, &out, 3, &odd)), vec!["l5", "l7", "l9"]);
    }

    #[test]
    fn file_sink_rotates_by_size() {
        let dir = std::env::temp_dir().join(format!("warden-logfile-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("app.log");
        let policy = RotatePolicy { max_size: 1024, keep: 2, ..RotatePolicy::default() };
        let mut f = FileSink::new(path.clone(), policy.clone());
        let line = [b'x'; 99];
        for _ in 0..40 {
            f.write(&line).unwrap();
        }
        f.flush().unwrap();
        let size = |p: &Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        assert!(size(&path) <= 1024, "current file {} bytes", size(&path));
        assert!(size(&dir.join("app.log.1")) > 900 && size(&dir.join("app.log.2")) > 900);
        assert!(!dir.join("app.log.3").exists(), "keeps only 2 rotated files");
        // A truncated file (warden flush) is not rotated early.
        std::fs::OpenOptions::new().write(true).truncate(true).open(&path).unwrap();
        f.write(&line).unwrap();
        f.flush().unwrap();
        assert_eq!(size(&path), 100);
        // An unwritable path reports an error instead of panicking.
        let mut bad = FileSink::new(PathBuf::from("/proc/warden-nope/app.log"), policy);
        assert!(bad.write(b"x").unwrap_err().contains("/proc/warden-nope/app.log"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_compress_date_and_age() {
        let dir = std::env::temp_dir().join(format!("warden-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let line = [b'x'; 499];
        // Compressed, numbered.
        let path = dir.join("c.log");
        let mut f =
            FileSink::new(path.clone(), RotatePolicy { max_size: 1024, keep: 3, compress: true, ..Default::default() });
        for _ in 0..12 {
            f.write(&line).unwrap();
        }
        let t0 = Instant::now();
        while (dir.join("c.log.1").exists() || !dir.join("c.log.1.gz").exists())
            && t0.elapsed() < Duration::from_secs(5)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dir.join("c.log.1.gz").exists(), "{:?}", std::fs::read_dir(&dir).unwrap().collect::<Vec<_>>());
        let mut text = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(std::fs::File::open(dir.join("c.log.1.gz")).unwrap()),
            &mut text,
        )
        .unwrap();
        assert_eq!(text.len(), 1000, "two 500-byte lines per rotated file");
        assert!(!dir.join("c.log.4.gz").exists() && !dir.join("c.log.4").exists(), "keep = 3");
        // Dated names, pruned by count.
        let path = dir.join("d.log");
        let mut f = FileSink::new(
            path.clone(),
            RotatePolicy { max_size: 1024, keep: 2, date_suffix: true, ..Default::default() },
        );
        for _ in 0..12 {
            f.write(&line).unwrap();
        }
        let dated: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
            .filter(|n| n.starts_with("d.log."))
            .collect();
        assert_eq!(dated.len(), 2, "{dated:?}");
        assert!(dated.iter().all(|n| n.starts_with("d.log.20")), "{dated:?}");
        // Size 0 = never by size.
        let path = dir.join("n.log");
        let mut f = FileSink::new(path.clone(), RotatePolicy { max_size: 0, ..Default::default() });
        for _ in 0..12 {
            f.write(&line).unwrap();
        }
        f.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 6000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worker_output_files() {
        let line = "2026-09-30T12:00:01.123Z OUT   worker=2 stderr: boom: bad thing";
        assert_eq!(split_output(line), Some(("2", "stderr", "boom: bad thing")));
        assert_eq!(split_output("2026-09-30T12:00:01.123Z INFO  worker ready worker=1"), None);
        // Batches build exactly that format, with the text where they say.
        let mut b = OutputBatch::new("3", "stdout");
        b.push("hello: world");
        b.push("");
        let batch = &b.batch;
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.bytes, batch.lines.iter().map(|l| l.len()).sum::<usize>());
        for (line, text) in batch.lines.iter().zip(["hello: world", ""]) {
            assert_eq!(split_output(line), Some(("3", "stdout", text)));
            assert_eq!(&line[batch.text_at..], text);
            assert_eq!(line.as_bytes()[TS_LEN - 1], b' ');
        }
        let dir = std::env::temp_dir().join(format!("warden-streams-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut per = StreamFiles::new(dir.join("api-out.log"), true, RotatePolicy::default());
        per.sink("1").write_parts(&[b"a"]).unwrap();
        per.sink("2").write_parts(&[b"b"]).unwrap();
        per.files.values_mut().for_each(|f| f.flush().unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("api-out-1.log")).unwrap(), "a\n");
        assert_eq!(std::fs::read_to_string(dir.join("api-out-2.log")).unwrap(), "b\n");
        let mut merged = StreamFiles::new(dir.join("all.log"), false, RotatePolicy::default());
        merged.sink("1").write_parts(&[b"a"]).unwrap();
        merged.sink("2").write_parts(&[b"b".as_slice(), b"", b""]).unwrap();
        assert_eq!(merged.files.len(), 1, "one shared file");
        merged.files.values_mut().for_each(|f| f.flush().unwrap());
        assert_eq!(std::fs::read_to_string(dir.join("all.log")).unwrap(), "a\nb\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn journald_and_plain_forms() {
        let line = "2026-09-30T12:00:01.123Z WARN  thing happened";
        let mut buf = String::new();
        assert_eq!(stdout_form(false, true, Some(Level::Warn), line, &mut buf), line);
        assert_eq!(stdout_form(false, false, Some(Level::Warn), line, &mut buf), "WARN  thing happened");
        assert_eq!(stdout_form(true, false, Some(Level::Warn), line, &mut buf), "<4>WARN  thing happened");
        assert_eq!(stdout_form(true, true, None, line, &mut buf), "WARN  thing happened");
    }

    #[test]
    fn rfc3339() {
        assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(format_rfc3339(1_790_769_601, 5), "2026-09-30T12:00:01.005Z");
        assert_eq!(format_rfc3339(951_782_400, 999), "2000-02-29T00:00:00.999Z");
    }
}

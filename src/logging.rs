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
//!
//! `worker_output = "direct"` bypasses all of that for worker output: the
//! bytes go from each worker's pipe into its out/err file with splice(2) on
//! an output thread, rotated here at line boundaries (see the section
//! "worker_output = direct" below). Warden's own events are unchanged.

use crate::config::Level;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::io::Write;
use std::path::PathBuf;
use std::rc::{Rc, Weak};
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
        Self::at(worker, stream, SystemTime::now())
    }

    /// Lines `worker` wrote to `stream`, stamped with `time`.
    pub fn at(worker: &str, stream: &'static str, time: SystemTime) -> Self {
        let mut prefix = String::with_capacity(TS_LEN + 24 + worker.len());
        let d = time.duration_since(UNIX_EPOCH).unwrap_or_default();
        push_rfc3339(&mut prefix, d.as_secs() as i64, d.subsec_millis());
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
    files: HashMap<String, FileSink>,
}

impl StreamFiles {
    fn new(base: PathBuf, per_worker: bool, policy: RotatePolicy) -> Self {
        StreamFiles { base, per_worker, policy, files: HashMap::new() }
    }

    /// The file for `worker`'s lines (the shared one unless per-worker).
    fn sink(&mut self, worker: &str) -> &mut FileSink {
        let key = if self.per_worker { worker } else { "" };
        let path = worker_file(&self.base, self.per_worker, worker);
        let policy = &self.policy;
        self.files.entry(key.to_string()).or_insert_with(|| FileSink::new(path, policy.clone()))
    }
}

/// `out.log` → `out-2.log` for worker 2 when `per_worker`, else `base`.
pub fn worker_file(base: &std::path::Path, per_worker: bool, worker: &str) -> PathBuf {
    if !per_worker {
        return base.to_path_buf();
    }
    let stem = base.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    let name = match base.extension() {
        Some(e) => format!("{stem}-{worker}.{}", e.to_string_lossy()),
        None => format!("{stem}-{worker}"),
    };
    base.with_file_name(name)
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
    emit(l, format_event(level, msg, fields).into(), Some(level));
}

/// `<ts> LEVEL msg key=value…`, as stored and written.
fn format_event(level: Level, msg: &str, fields: &[(&str, &dyn std::fmt::Display)]) -> String {
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
    line
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
    truncate_direct_files();
}

/// Moves a full log file out of the way: numbered (`app.log.1` newest …
/// `app.log.N`) or dated (`app.log.2026-09-30T00-00-00`), optionally
/// gzipped in the background; old ones are pruned by count and age. Shared
/// by the writer thread's files ([`FileSink`]) and the files workers write
/// directly ([`DirectFile`]), so both name, keep and compress alike.
struct Rotator {
    path: PathBuf,
    policy: RotatePolicy,
    next_rotation: Option<SystemTime>,
    /// The previous rotation's compression, finished before names shift again.
    compressing: Option<std::thread::JoinHandle<()>>,
}

impl Rotator {
    fn new(path: PathBuf, policy: RotatePolicy) -> Self {
        let next_rotation = policy.interval.as_ref().and_then(|c| c.next_after(SystemTime::now()));
        Rotator { path, policy, next_rotation, compressing: None }
    }

    /// The schedule (`interval`) says rotate now; the next time is set.
    /// No clock read without a schedule.
    fn schedule_due(&mut self) -> bool {
        let due = self.next_rotation.is_some_and(|t| SystemTime::now() >= t);
        if due {
            self.next_rotation = self.policy.interval.as_ref().and_then(|c| c.next_after(SystemTime::now()));
        }
        due
    }

    fn sibling(&self, suffix: &str) -> PathBuf {
        let mut s = self.path.clone().into_os_string();
        s.push(suffix);
        PathBuf::from(s)
    }

    /// Rename (or delete, with `keep = 0`) the file at `path`; whoever
    /// writes it opens a new one at `path` afterwards. A descriptor still
    /// open on the old file keeps pointing at it under its new name.
    fn rotate(&mut self) -> std::io::Result<()> {
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
            // Off the writing thread: a 10 MB file takes ~100 ms to compress.
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
        Ok(())
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
}

/// A log file rotated by size and/or on a schedule (see [`Rotator`]),
/// written line by line by the writer thread.
pub struct FileSink {
    rot: Rotator,
    file: Option<std::fs::File>,
    /// Lines not yet written: one write(2) per batch instead of two per
    /// line. `flush` empties it; the writer thread flushes after every
    /// batch, and rotation and drop flush first.
    buf: Vec<u8>,
    /// Bytes in the file plus those in `buf`.
    size: u64,
    /// Last time an error was reported (at most once a minute).
    last_error: Option<Instant>,
}

impl FileSink {
    pub fn new(path: PathBuf, policy: RotatePolicy) -> Self {
        FileSink { rot: Rotator::new(path, policy), file: None, buf: Vec::new(), size: 0, last_error: None }
    }

    fn open(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(dir) = self.rot.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let f = std::fs::OpenOptions::new().create(true).append(true).mode(0o640).open(&self.rot.path)?;
        self.size = f.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(f);
        Ok(())
    }

    fn rotate(&mut self) -> std::io::Result<()> {
        self.write_buf()?;
        self.file = None;
        self.rot.rotate()?;
        self.open()
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
            let max = self.rot.policy.max_size;
            if self.rot.schedule_due() {
                if self.size > 0 {
                    self.rotate()?;
                }
            } else if max > 0 && self.size > 0 && self.size + needed > max {
                // `warden flush` may have truncated it: trust the file, not the count.
                self.write_buf()?;
                self.size = self.file.as_ref().and_then(|f| f.metadata().ok()).map(|m| m.len()).unwrap_or(self.size);
                if self.size > 0 && self.size + needed > max {
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
            format!("{}: {e}", self.rot.path.display())
        })
    }

    /// Write out buffered lines.
    pub fn flush(&mut self) -> Result<(), String> {
        self.write_buf().map_err(|e| {
            self.file = None;
            format!("{}: {e}", self.rot.path.display())
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

// ------------------------------------------------ worker_output = "direct"
//
// Worker bytes go from the worker's pipe to its out/err file with
// splice(2): no parsing and no copy through Warden's memory. The pipes are
// pumped on one output thread (`warden-output`, started on first use), so
// a slow or stalled disk holds up worker output (the worker's writes block,
// as if it wrote the file itself) but never supervision.
//
// splice can't write to O_APPEND files, so each file tracks its write
// offset, re-read from the file's size before every write: a truncation by
// someone else (`copytruncate`, `: > file`) or another appender is followed
// like O_APPEND would, never overwritten or turned into a hole. Rotation
// happens at a line boundary: splices stop at the size limit; then one
// chunk is read (not spliced), the line that reached the limit (or was
// open when the schedule fired) ends the old file and the rest begins the
// new one. Every stream writing one path
// shares one `DirectFile` (offset, rotation), and a stream only writes
// after the file's last line is finished (or its writer is gone, or had
// LINE_GRACE to finish it): two workers' partial lines never meet.

/// Where direct-mode worker output goes (`[logging] out_file`/`err_file`).
#[derive(Debug, Clone)]
pub struct DirectFiles {
    pub out: PathBuf,
    /// None: stderr goes into `out` too, through the same pipe (`2>&1`).
    pub err: Option<PathBuf>,
    pub per_worker: bool,
    pub policy: RotatePolicy,
}

impl DirectFiles {
    /// `worker`'s stdout file and, when separate, its stderr file.
    pub fn paths(&self, worker: &str) -> (PathBuf, Option<PathBuf>) {
        let out = worker_file(&self.out, self.per_worker, worker);
        let err = self.err.as_deref().map(|e| worker_file(e, self.per_worker, worker)).filter(|e| *e != out);
        (out, err)
    }
}

/// Most bytes one splice moves (a pipe holds 64 KB unless enlarged).
const DIRECT_CHUNK: usize = 1 << 20;
/// Userspace buffer for the read/write fallback, rotation and `logs -f`.
const DIRECT_BUF: usize = 64 * 1024;
/// How long a writer waits for another writer of the same file to finish
/// the line it is in the middle of.
const LINE_GRACE: Duration = Duration::from_millis(100);
/// A failed rotation is retried after this (the file grows meanwhile).
const ROTATE_RETRY: Duration = Duration::from_secs(60);

/// Gets a copy of the bytes a step moved, for `warden logs -f` (None when
/// nobody follows: then nothing is read back).
pub type Echo<'a> = Option<&'a mut dyn FnMut(&[u8])>;

/// What one [`DirectWriter::step`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Bytes taken from the pipe.
    Moved(usize),
    /// The pipe is empty and closed: the worker (and whatever it started) is gone.
    Eof,
    /// Another writer of the file is in the middle of a line: try again after this.
    Wait(Duration),
}

thread_local! {
    /// Direct files open on this thread by path; every writer of a path
    /// shares one (offset, rotation, whose turn it is).
    static DIRECT_OPEN: RefCell<HashMap<PathBuf, Weak<RefCell<DirectFile>>>> =
        RefCell::new(HashMap::new());
    /// Buffer for the fallback copy, rotation and `logs -f`; allocated on first use.
    static DIRECT_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// A direct file `warden logs` shows the tail of: (path, worker, stream).
/// `worker` is "*" once several workers have written the same path.
type Known = (PathBuf, String, &'static str);
/// Every direct file opened so far (it stays listed after its worker exits).
static DIRECT_KNOWN: Mutex<Vec<Known>> = Mutex::new(Vec::new());
/// Paths whose filesystem refused splice: warned once, then copied.
static DIRECT_NO_SPLICE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
/// Direct streams still being pumped (`flush` waits for them at exit, so
/// the last lines of exited workers are drained into their files).
static DIRECT_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static DIRECT_WRITER_ID: AtomicU64 = AtomicU64::new(1);

fn with_scratch<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    DIRECT_SCRATCH.with(|b| {
        let mut b = b.borrow_mut();
        if b.len() < DIRECT_BUF {
            b.resize(DIRECT_BUF, 0);
        }
        f(&mut b)
    })
}

/// Read one chunk from `pipe` and drop it (the file can't take it).
fn discard_chunk(pipe: &std::fs::File) -> std::io::Result<usize> {
    use std::io::Read;
    with_scratch(|buf| {
        let mut r = pipe;
        r.read(buf)
    })
}

/// Counts one direct stream as active until dropped.
pub struct DirectActive(());

impl DirectActive {
    pub fn begin() -> Self {
        DIRECT_ACTIVE.fetch_add(1, Ordering::Relaxed);
        DirectActive(())
    }
}

impl Drop for DirectActive {
    fn drop(&mut self) {
        DIRECT_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// One stream's handle on its direct file.
pub struct DirectWriter {
    file: Rc<RefCell<DirectFile>>,
    id: u64,
}

impl DirectWriter {
    /// The file at `path` for `worker`'s `stream`, shared with every other
    /// writer of that path on this thread. If it can't be opened, that is
    /// logged, and output is discarded until it can (retried every second).
    pub fn open(path: PathBuf, policy: RotatePolicy, worker: &str, stream: &'static str) -> DirectWriter {
        {
            let mut known = DIRECT_KNOWN.lock().unwrap_or_else(|e| e.into_inner());
            match known.iter_mut().find(|k| k.0 == path) {
                Some(k) if k.1 != worker => k.1 = "*".into(),
                Some(_) => {}
                None => known.push((path.clone(), worker.to_string(), stream)),
            }
        }
        let file = DIRECT_OPEN.with(|open| {
            let mut open = open.borrow_mut();
            open.retain(|_, f| f.strong_count() > 0);
            if let Some(f) = open.get(&path).and_then(Weak::upgrade) {
                return f;
            }
            let f = Rc::new(RefCell::new(DirectFile::new(path.clone(), policy, worker, stream)));
            open.insert(path, Rc::downgrade(&f));
            f
        });
        DirectWriter { file, id: DIRECT_WRITER_ID.fetch_add(1, Ordering::Relaxed) }
    }

    /// Move the next chunk of `pipe` (non-blocking) into the file.
    /// `WouldBlock`: the pipe is empty. `echo` gets a copy of the bytes
    /// (pass it only while someone follows `warden logs -f`).
    pub fn step(&self, pipe: &std::fs::File, echo: Echo<'_>) -> std::io::Result<Step> {
        self.file.borrow_mut().step(self.id, pipe, echo)
    }
}

impl Drop for DirectWriter {
    fn drop(&mut self) {
        if let Ok(mut f) = self.file.try_borrow_mut() {
            if f.last_writer == Some(self.id) {
                f.last_writer_gone = true;
            }
        }
    }
}

/// One direct-mode file: descriptor, write offset and rotation, shared by
/// every stream that writes it.
pub struct DirectFile {
    rot: Rotator,
    file: Option<std::fs::File>,
    /// Opened read-write: its last byte and `logs -f` can be read back.
    readable: bool,
    /// A regular file: offsets, splice and rotation apply. Otherwise (a
    /// FIFO, a terminal, /dev/null) plain writes, never rotated.
    regular: bool,
    /// Where the next byte goes: the file's size, re-read before each write.
    offset: u64,
    /// splice works here; false after EINVAL/ENOSYS/EPERM, and off Linux.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only read by the Linux fast path
    splice: bool,
    /// The schedule asked for a rotation: done at the next line boundary.
    rotate_due: bool,
    /// A rotation failed: none before this.
    rotate_retry: Option<Instant>,
    /// The file could not be opened: next attempt.
    next_open: Option<Instant>,
    last_writer: Option<u64>,
    last_writer_gone: bool,
    last_write: Instant,
    /// Writes are failing: bytes are being discarded.
    failing: bool,
    /// Bytes discarded since the last report.
    discarded: u64,
    last_report: Option<Instant>,
    worker: String,
    stream: &'static str,
}

impl DirectFile {
    fn new(path: PathBuf, policy: RotatePolicy, worker: &str, stream: &'static str) -> Self {
        let no_splice = DIRECT_NO_SPLICE.lock().unwrap_or_else(|e| e.into_inner()).contains(&path);
        let mut f = DirectFile {
            rot: Rotator::new(path, policy),
            file: None,
            readable: false,
            regular: false,
            offset: 0,
            splice: cfg!(target_os = "linux") && !no_splice,
            rotate_due: false,
            rotate_retry: None,
            next_open: None,
            last_writer: None,
            last_writer_gone: false,
            last_write: Instant::now(),
            failing: false,
            discarded: 0,
            last_report: None,
            worker: worker.to_string(),
            stream,
        };
        f.reopen();
        f
    }

    /// O_CREAT, never O_APPEND (splice refuses it), close-on-exec (std),
    /// read-write when allowed (for the last byte and `logs -f`).
    fn open(&mut self) -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = &self.rot.path;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).mode(0o640);
        let (f, readable) = match opts.clone().read(true).open(path) {
            Ok(f) => (f, true),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => (opts.open(path)?, false),
            Err(e) => return Err(e),
        };
        let m = f.metadata()?;
        self.regular = m.is_file();
        self.offset = if self.regular { m.len() } else { 0 };
        self.file = Some(f);
        self.readable = readable;
        Ok(())
    }

    /// Open the file unless the last attempt failed less than a second ago.
    fn reopen(&mut self) -> bool {
        if self.next_open.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        match self.open() {
            Ok(()) => {
                self.next_open = None;
                true
            }
            Err(e) => {
                self.next_open = Some(Instant::now() + Duration::from_secs(1));
                self.failing = true;
                if self.should_report() {
                    crate::error!(
                        "cannot open the worker output file; the worker's output is discarded until it can",
                        file = self.rot.path.display(),
                        worker = self.worker,
                        stream = self.stream,
                        error = e,
                        discarded_bytes = std::mem::take(&mut self.discarded),
                        hint = "make sure the directory exists (or can be created) and is writable by the user Warden runs as; Warden retries every second",
                    );
                }
                false
            }
        }
    }

    fn step(&mut self, me: u64, pipe: &std::fs::File, echo: Echo<'_>) -> std::io::Result<Step> {
        self.sync_offset();
        if self.file.is_none() && !self.reopen() {
            let n = discard_chunk(pipe)?;
            self.discarded += n as u64;
            return Ok(if n == 0 { Step::Eof } else { Step::Moved(n) });
        }
        if self.last_writer != Some(me) {
            if let Some(wait) = self.take_turn() {
                return Ok(Step::Wait(wait));
            }
            self.last_writer = Some(me);
            self.last_writer_gone = false;
        }
        let n = if self.rotation_due() { self.rotate_at_line(pipe, echo)? } else { self.transfer(pipe, echo)? };
        self.last_write = Instant::now();
        if !self.failing && self.discarded > 0 {
            crate::info!(
                "worker output file is writable again",
                file = self.rot.path.display(),
                worker = self.worker,
                discarded_bytes = std::mem::take(&mut self.discarded),
            );
        }
        Ok(if n == 0 { Step::Eof } else { Step::Moved(n) })
    }

    /// Follow the file's real size: truncated elsewhere (`copytruncate`,
    /// `warden flush` from another process) or appended to by someone
    /// else, the next write goes to its end. Deleted: a new file.
    fn sync_offset(&mut self) {
        use std::os::unix::fs::MetadataExt;
        let (Some(f), true) = (&self.file, self.regular) else { return };
        match f.metadata() {
            Ok(m) if m.nlink() == 0 => {
                self.file = None;
                self.next_open = None;
            }
            Ok(m) => self.offset = m.len(),
            Err(_) => {}
        }
    }

    /// Another writer wrote last. If the file ends inside its line, it gets
    /// LINE_GRACE from its last write to finish; after that, or if it is
    /// gone, the line is ended here, so two writers never share a line.
    fn take_turn(&mut self) -> Option<Duration> {
        if !self.regular || self.offset == 0 || !self.ends_mid_line() {
            return None;
        }
        let since = self.last_write.elapsed();
        if self.last_writer.is_some() && !self.last_writer_gone && since < LINE_GRACE {
            return Some((LINE_GRACE - since).min(Duration::from_millis(5)));
        }
        let _ = self.write_bytes(b"\n");
        None
    }

    /// Not empty, and the last byte written ends a line.
    fn ends_at_line_end(&self) -> bool {
        use std::os::unix::fs::FileExt;
        let (Some(f), true) = (&self.file, self.readable) else { return false };
        let mut b = [0u8; 1];
        self.offset > 0 && matches!(f.read_at(&mut b, self.offset - 1), Ok(1)) && b[0] == b'\n'
    }

    fn ends_mid_line(&self) -> bool {
        use std::os::unix::fs::FileExt;
        let (Some(f), true) = (&self.file, self.readable) else { return false };
        let mut b = [0u8; 1];
        matches!(f.read_at(&mut b, self.offset - 1), Ok(1)) && b[0] != b'\n'
    }

    fn rotation_due(&mut self) -> bool {
        if !self.regular {
            return false;
        }
        if self.rot.schedule_due() && self.offset > 0 {
            self.rotate_due = true;
        }
        if let Some(t) = self.rotate_retry {
            if Instant::now() < t {
                return false;
            }
            self.rotate_retry = None;
        }
        if self.offset == 0 {
            return false;
        }
        let max = self.rot.policy.max_size;
        self.rotate_due || (max > 0 && self.offset >= max)
    }

    /// The fast path: splice up to the size limit (or a chunk).
    fn transfer(&mut self, pipe: &std::fs::File, echo: Echo<'_>) -> std::io::Result<usize> {
        let max = self.rot.policy.max_size;
        let room = match self.regular && max > 0 && self.offset < max {
            true => (max - self.offset).min(DIRECT_CHUNK as u64) as usize,
            false => DIRECT_CHUNK,
        };
        // Off Linux `splice` is always false: the copy below.
        #[cfg(target_os = "linux")]
        if self.splice && self.regular {
            use std::os::fd::AsFd;
            let start = self.offset;
            let Some(f) = self.file.as_ref() else { return discard_chunk(pipe) };
            match crate::sys::splice(pipe.as_fd(), f.as_fd(), Some(&mut self.offset), room) {
                Ok(n) => {
                    self.failing = false;
                    if let (Some(echo), true) = (echo, n > 0) {
                        self.echo_back(start, n, echo);
                    }
                    return Ok(n);
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted) => {
                    return Err(e);
                }
                Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::ENOSYS | libc::EPERM)) => {
                    self.splice = false;
                    DIRECT_NO_SPLICE.lock().unwrap_or_else(|e| e.into_inner()).push(self.rot.path.clone());
                    crate::warn!(
                        "cannot splice into the worker output file; copying through Warden instead",
                        file = self.rot.path.display(),
                        reason = e,
                        hint = "output still works, only slower (one copy through Warden); a local filesystem such as ext4, xfs or tmpfs takes splice",
                    );
                }
                // The data is still in the pipe: drop one chunk so the
                // worker never blocks on a file that can't take it.
                Err(e) => {
                    let n = discard_chunk(pipe)?;
                    self.write_failed(&e, n);
                    return Ok(n);
                }
            }
        }
        self.copy(pipe, room, echo)
    }

    /// The fallback: read(2) into a buffer, write it out.
    fn copy(&mut self, pipe: &std::fs::File, room: usize, echo: Echo<'_>) -> std::io::Result<usize> {
        use std::io::Read;
        with_scratch(|buf| {
            let mut r = pipe;
            let n = r.read(&mut buf[..room.clamp(1, DIRECT_BUF)])?;
            if n > 0 {
                match self.write_bytes(&buf[..n]) {
                    Ok(()) => {
                        if let Some(echo) = echo {
                            echo(&buf[..n]);
                        }
                    }
                    Err(e) => self.write_failed(&e, n),
                }
            }
            Ok(n)
        })
    }

    /// Rotation, exact and at a line boundary: one chunk is read (not
    /// spliced) and cut where the line that reaches the limit ends; that
    /// much ends the old file and the rest begins the new one (cut again if
    /// it fills that one too). Without such a newline all of it goes to the
    /// current file and the rotation waits for the next chunk.
    fn rotate_at_line(&mut self, pipe: &std::fs::File, echo: Echo<'_>) -> std::io::Result<usize> {
        use std::io::Read;
        with_scratch(|buf| {
            let mut r = pipe;
            let n = r.read(buf)?;
            let mut rest = &buf[..n];
            while !rest.is_empty() {
                // What this file still takes: nothing once due (limit or schedule).
                let max = self.rot.policy.max_size;
                let room = match (self.rotate_due, max) {
                    (true, _) => 0,
                    (false, 0) => usize::MAX,
                    (false, max) => max.saturating_sub(self.offset).try_into().unwrap_or(usize::MAX),
                };
                // Full, and its last line is complete (a splice can stop
                // exactly on a newline at the limit): rotate before writing,
                // or the next whole line would land in this file too.
                if room == 0 && self.rotate_retry.is_none() && self.ends_at_line_end() {
                    if let Err(e) = self.rotate_now() {
                        self.rotate_retry = Some(Instant::now() + ROTATE_RETRY);
                        crate::error!(
                            "cannot rotate the worker output file; it keeps growing",
                            file = self.rot.path.display(),
                            error = e,
                            hint =
                                "Warden must be able to rename and create files in that directory; retried in a minute",
                        );
                    }
                    continue;
                }
                // The line that reaches the limit is the file's last.
                let from = room.saturating_sub(1);
                let end = match rest.get(from..).and_then(|r| r.iter().position(|&b| b == b'\n')) {
                    Some(i) if room <= rest.len() && self.rotate_retry.is_none() => from + i + 1,
                    // Room for all of it, or the line goes on: rotate once it ends.
                    _ => {
                        self.write_or_drop(rest);
                        // A line that never ends (megabytes without `\n`)
                        // would grow the file without bound: past the limit
                        // plus max(limit, 1 MiB), cut it mid-line.
                        let hard = max.saturating_add(max.max(LONG_LINE_SLACK));
                        if max > 0 && self.offset >= hard && self.rotate_retry.is_none() {
                            long_line_warning(&self.rot.path, max);
                            if let Err(e) = self.rotate_now() {
                                self.rotate_retry = Some(Instant::now() + ROTATE_RETRY);
                                crate::error!(
                                    "cannot rotate the worker output file; it keeps growing",
                                    file = self.rot.path.display(),
                                    error = e,
                                    hint = "Warden must be able to rename and create files in that directory; retried in a minute",
                                );
                            }
                        }
                        break;
                    }
                };
                let (head, tail) = rest.split_at(end);
                self.write_or_drop(head);
                if let Err(e) = self.rotate_now() {
                    self.rotate_retry = Some(Instant::now() + ROTATE_RETRY);
                    crate::error!(
                        "cannot rotate the worker output file; it keeps growing",
                        file = self.rot.path.display(),
                        error = e,
                        hint = "Warden must be able to rename and create files in that directory; retried in a minute",
                    );
                }
                rest = tail;
            }
            if let (Some(echo), true) = (echo, n > 0) {
                echo(&buf[..n]);
            }
            Ok(n)
        })
    }

    fn rotate_now(&mut self) -> std::io::Result<()> {
        self.rotate_due = false;
        self.rot.rotate()?;
        // The descriptor still points at the rotated file; the new one
        // starts empty. The turn stays with this writer: the rest of its
        // chunk (maybe a partial line) begins the new file.
        self.file = None;
        self.rotate_retry = None;
        self.open()
    }

    fn write_or_drop(&mut self, b: &[u8]) {
        if let Err(e) = self.write_bytes(b) {
            self.write_failed(&e, b.len());
        }
    }

    fn write_bytes(&mut self, b: &[u8]) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let Some(f) = self.file.as_ref() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotFound, "the file is not open"));
        };
        if self.regular {
            f.write_all_at(b, self.offset)?;
            self.offset += b.len() as u64;
        } else {
            let mut w = f;
            w.write_all(b)?;
        }
        self.failing = false;
        Ok(())
    }

    fn write_failed(&mut self, e: &std::io::Error, lost: usize) {
        self.failing = true;
        self.discarded += lost as u64;
        if self.should_report() {
            crate::error!(
                "cannot write the worker output file; its output is discarded until writes work again",
                file = self.rot.path.display(),
                worker = self.worker,
                stream = self.stream,
                error = e,
                discarded_bytes = std::mem::take(&mut self.discarded),
                hint = "free disk space (`df -h`) or fix the file's permissions; the worker keeps running and writing resumes by itself",
            );
        }
    }

    /// `logs -f`: the bytes just spliced, read back from the page cache.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn echo_back(&self, start: u64, n: usize, echo: &mut dyn FnMut(&[u8])) {
        use std::os::unix::fs::FileExt;
        let (Some(f), true) = (&self.file, self.readable) else { return };
        let skip = n.saturating_sub(DIRECT_BUF);
        with_scratch(|buf| {
            if let Ok(k) = f.read_at(&mut buf[..n - skip], start + skip as u64) {
                echo(&buf[..k]);
            }
        });
    }

    /// `warden flush`: empty the file; the next byte goes to offset 0.
    fn truncate(&mut self) {
        let (Some(f), true) = (&self.file, self.regular) else { return };
        match f.set_len(0) {
            Ok(()) => {
                self.offset = 0;
                self.rotate_due = false;
                self.last_writer = None;
            }
            Err(e) => crate::warn!(
                "could not truncate the worker output file; it keeps its content",
                file = self.rot.path.display(),
                error = e,
                hint = "check the file's permissions",
            ),
        }
    }

    fn should_report(&mut self) -> bool {
        let due = self.last_report.is_none_or(|t| t.elapsed() >= Duration::from_secs(60));
        if due {
            self.last_report = Some(Instant::now());
        }
        due
    }
}

/// Work for the output thread.
enum DirectMsg {
    /// Called on the thread, inside its LocalSet: spawns a pump.
    Run(Box<dyn FnOnce() + Send>),
    /// `warden flush`: truncate every open direct file, then say so.
    Truncate(Sender<()>),
}

static DIRECT_THREAD: OnceLock<Option<tokio::sync::mpsc::UnboundedSender<DirectMsg>>> = OnceLock::new();

/// Run `start` on the output thread, where direct-mode files are written
/// (`start` spawns its task with `spawn_local`). If that thread can't run,
/// `start` runs here, on the caller's LocalSet.
pub fn on_output_thread(start: Box<dyn FnOnce() + Send>) {
    let start = match output_thread() {
        Some(tx) => match tx.send(DirectMsg::Run(start)) {
            Ok(()) => return,
            Err(tokio::sync::mpsc::error::SendError(DirectMsg::Run(start))) => start,
            Err(_) => return,
        },
        None => start,
    };
    start();
}

fn output_thread() -> Option<&'static tokio::sync::mpsc::UnboundedSender<DirectMsg>> {
    DIRECT_THREAD
        .get_or_init(|| {
            let started = tokio::runtime::Builder::new_current_thread().enable_all().build().and_then(|rt| {
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DirectMsg>();
                std::thread::Builder::new().name("warden-output".into()).spawn(move || {
                    let local = tokio::task::LocalSet::new();
                    local.block_on(&rt, async move {
                        while let Some(msg) = rx.recv().await {
                            match msg {
                                DirectMsg::Run(start) => start(),
                                DirectMsg::Truncate(done) => {
                                    truncate_direct_here();
                                    let _ = done.send(());
                                }
                            }
                        }
                    });
                })?;
                Ok(tx)
            });
            match started {
                Ok(tx) => Some(tx),
                Err(e) => {
                    crate::error!(
                        "cannot start the thread that writes worker output files; writing them on the main thread",
                        error = e,
                        hint = "a slow disk can now delay supervision; check the process and memory limits (ulimit -u)",
                    );
                    None
                }
            }
        })
        .as_ref()
}

fn truncate_direct_here() {
    let files: Vec<_> = DIRECT_OPEN.with(|open| open.borrow().values().filter_map(Weak::upgrade).collect());
    for f in files {
        if let Ok(mut f) = f.try_borrow_mut() {
            f.truncate();
        }
    }
}

/// `warden flush` for direct files: truncated on the thread that writes
/// them, between two writes, so no write lands past the new end. Waits a
/// moment so the command returns after the fact.
fn truncate_direct_files() {
    truncate_direct_here();
    if let Some(Some(tx)) = DIRECT_THREAD.get() {
        let (done, wait) = channel();
        if tx.send(DirectMsg::Truncate(done)).is_ok() && wait.recv_timeout(Duration::from_millis(250)).is_err() {
            crate::warn!(
                "worker output files are still being truncated",
                hint = "the disk is slow; the truncation finishes in the background",
            );
        }
    }
}

/// Someone is reading `warden logs` (and may follow it).
pub fn following() -> bool {
    logger().tx.receiver_count() > 0
}

/// Direct-mode output for `warden logs -f` only: not kept, not written
/// (it is in the file already).
pub fn follow_only(b: OutputBatch) {
    let l = logger();
    for line in b.batch.lines {
        let _ = l.tx.send(line);
    }
}

/// Bytes read from the end of each direct file for `warden logs`.
const DIRECT_TAIL_MIN: u64 = 64 * 1024;
const DIRECT_TAIL_MAX: u64 = 256 * 1024;

/// The last `n` lines of a direct file as `warden logs` shows worker
/// output, stamped with the file's last write (lines carry no time of
/// their own). Only the page cache is read, so the event loop never waits
/// for the disk; otherwise one line says where the output is.
/// How far past `max_size` a direct-mode file may grow waiting for a line
/// to end (at least; `max_size` itself if that is larger).
const LONG_LINE_SLACK: u64 = 1 << 20;

/// At most one warning a minute: an app that writes endless lines would
/// otherwise log one per rotation.
fn long_line_warning(path: &std::path::Path, max: u64) {
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = crate::sys::monotonic_usec();
    let last = LAST.load(std::sync::atomic::Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 60_000_000 {
        return;
    }
    LAST.store(now, std::sync::atomic::Ordering::Relaxed);
    crate::warn!(
        "a worker wrote a line far longer than the log file limit; splitting it across rotated files",
        file = path.display(),
        max_size = max,
        hint = "the line continues at the start of the next file; if the app really writes such lines, raise [logging.rotate] max_size",
    );
}

fn direct_tail(k: &Known, n: usize) -> Vec<Line> {
    let (path, worker, stream) = k;
    let bytes = (n as u64).saturating_mul(1024).clamp(DIRECT_TAIL_MIN, DIRECT_TAIL_MAX);
    match crate::logview::tail_lines(path, n, bytes) {
        Ok((mtime, text)) => {
            let mut b = OutputBatch::at(worker, stream, mtime);
            for t in &text {
                b.push(t);
            }
            b.batch.lines
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => vec![
            format_event(
                Level::Info,
                "worker output not shown: its file is not in memory right now",
                &[("file", &path.display()), ("reason", &e), ("hint", &"read the file itself (tail, less)")],
            )
            .into(),
        ],
    }
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

/// Wait (up to `timeout`) for queued lines to be written, and for direct
/// worker output to be drained from the pipes of workers that have exited.
/// Called before exit.
pub fn flush(timeout: Duration) {
    let l = logger();
    let t0 = Instant::now();
    while (l.pending.load(Ordering::Relaxed) > 0 || DIRECT_ACTIVE.load(Ordering::Relaxed) > 0) && t0.elapsed() < timeout
    {
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The last `n` lines kept in memory that pass `keep` (oldest first). With
/// `worker_output = "direct"`, worker output is not in memory: the tails
/// of its files are read instead and merged in by time.
pub fn recent_matching(n: usize, keep: &dyn Fn(&str) -> bool) -> Vec<Line> {
    let mut lines = {
        let rings = logger().rings.lock().unwrap_or_else(|e| e.into_inner());
        merge_newest(&rings.events, &rings.output, n, keep)
    };
    let known = DIRECT_KNOWN.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if known.is_empty() || n == 0 {
        return lines;
    }
    for k in &known {
        lines.extend(direct_tail(k, n).into_iter().filter(|l| keep(l)));
    }
    // Stable: events keep their order, and so do each file's lines.
    lines.sort_by(|a, b| a.get(..TS_LEN).cmp(&b.get(..TS_LEN)));
    let cut = lines.len().saturating_sub(n);
    lines.drain(..cut);
    lines
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

    // -------------------------------------------- worker_output = "direct"

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warden-direct-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A pipe whose read end is non-blocking, as the output thread has it.
    fn nb_pipe() -> (std::fs::File, std::fs::File) {
        let (r, w) = crate::sys::pipe_cloexec().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_io().build().unwrap();
        let _in_rt = rt.enter();
        let r = tokio::net::unix::pipe::Receiver::from_owned_fd(r).unwrap().into_nonblocking_fd().unwrap();
        (std::fs::File::from(r), std::fs::File::from(w))
    }

    /// A pipe whose write end a thread fills with `data`, then closes.
    fn feed(data: Vec<u8>) -> (std::fs::File, std::thread::JoinHandle<()>) {
        let (r, mut w) = nb_pipe();
        let t = std::thread::spawn(move || w.write_all(&data).unwrap());
        (r, t)
    }

    /// Step until the pipe is empty (`Ok(None)`: EOF) or the file says wait.
    fn pump(w: &DirectWriter, pipe: &std::fs::File, until_eof: bool) -> Option<Step> {
        loop {
            match w.step(pipe, None) {
                Ok(Step::Moved(_)) => {}
                Ok(Step::Eof) => return None,
                Ok(wait @ Step::Wait(_)) => return Some(wait),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && until_eof => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Some(Step::Moved(0)),
                Err(e) => panic!("{e}"),
            }
        }
    }

    /// Lines of every length (empty, short, CRLF, over a pipe's 64 KB),
    /// ending with one that has no newline.
    fn varied_output(lines: usize) -> Vec<u8> {
        let mut data = Vec::new();
        let mut seed = 99u64;
        for i in 0..lines {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let len = match (seed >> 33) % 10 {
                0 => 0,
                1 => 70_000 + (seed >> 40) as usize % 5000,
                _ => (seed >> 40) as usize % 300,
            };
            data.extend(format!("{i} ").bytes());
            data.extend((0..len).map(|j| b'a' + (j % 26) as u8));
            data.extend_from_slice(if i % 7 == 0 { b"\r\n" } else { b"\n" });
        }
        data.extend_from_slice(b"last line, no newline");
        data
    }

    /// Rotated files oldest first, then the current one.
    fn chain(path: &std::path::Path) -> Vec<PathBuf> {
        let mut rotated: Vec<(u32, PathBuf)> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                let n = p.file_name()?.to_str()?.strip_prefix("out.log.")?.parse().ok()?;
                Some((n, p))
            })
            .collect();
        rotated.sort_by_key(|r| std::cmp::Reverse(r.0));
        rotated.into_iter().map(|r| r.1).chain([path.to_path_buf()]).collect()
    }

    #[test]
    fn direct_files_rotate_exactly_at_line_boundaries() {
        // Spliced, then the read/write fallback: the same bytes either way.
        for splice in [true, false] {
            let dir = scratch_dir(&format!("rotate-{splice}"));
            let path = dir.join("out.log");
            let policy = RotatePolicy { max_size: 4096, keep: 1000, ..RotatePolicy::default() };
            let w = DirectWriter::open(path.clone(), policy, "1", "stdout");
            w.file.borrow_mut().splice &= splice;
            let data = varied_output(600);
            let (pipe, t) = feed(data.clone());
            assert_eq!(pump(&w, &pipe, true), None);
            t.join().unwrap();
            let files = chain(&path);
            assert!(files.len() > 20, "{} files", files.len());
            let mut all = Vec::new();
            for (i, f) in files.iter().enumerate() {
                let bytes = std::fs::read(f).unwrap();
                if i + 1 < files.len() {
                    assert!(bytes.ends_with(b"\n"), "{} does not end a line", f.display());
                    assert!(bytes.len() >= 4096, "{} rotated early: {} bytes", f.display(), bytes.len());
                    // ...and late by no more than the line that reached the limit.
                    let last_line = bytes[..bytes.len() - 1].iter().rposition(|&b| b == b'\n').map_or(0, |p| p + 1);
                    assert!(last_line < 4096, "{} rotated late: {last_line} bytes before its last line", f.display());
                }
                all.extend(bytes);
            }
            assert!(all == data, "splice={splice}: the files together are not what was written");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn direct_files_rotate_when_a_line_ends_exactly_at_the_limit() {
        // The first 4096 bytes end with a newline: that file is complete,
        // and the next line starts the new one (found under load, when a
        // splice happened to stop exactly there).
        for splice in [true, false] {
            let dir = scratch_dir(&format!("exact-{splice}"));
            let path = dir.join("out.log");
            let policy = RotatePolicy { max_size: 4096, keep: 10, ..RotatePolicy::default() };
            let w = DirectWriter::open(path.clone(), policy, "1", "stdout");
            w.file.borrow_mut().splice &= splice;
            let mut data = vec![b'a'; 4095];
            data.extend_from_slice(b"\nnext line\n");
            let (pipe, t) = feed(data.clone());
            assert_eq!(pump(&w, &pipe, true), None);
            t.join().unwrap();
            let files = chain(&path);
            assert_eq!(files.len(), 2, "splice={splice}: {files:?}");
            assert_eq!(std::fs::read(&files[0]).unwrap().len(), 4096, "splice={splice}");
            assert_eq!(std::fs::read(&files[1]).unwrap(), b"next line\n", "splice={splice}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn direct_files_cut_a_line_that_never_ends() {
        // Review R4: 3 MiB without a newline and a 4 KiB limit: rotation
        // may wait for a line end, but not forever.
        let dir = scratch_dir("endless");
        let path = dir.join("out.log");
        let policy = RotatePolicy { max_size: 4096, keep: 100, ..RotatePolicy::default() };
        let w = DirectWriter::open(path.clone(), policy, "1", "stdout");
        let mut data = b"short line\n".to_vec();
        data.extend((0..3 << 20).map(|j| b'a' + (j % 26) as u8));
        let (pipe, t) = feed(data.clone());
        assert_eq!(pump(&w, &pipe, true), None);
        t.join().unwrap();
        let files = chain(&path);
        assert!(files.len() >= 3, "{} files", files.len());
        let mut all = Vec::new();
        for f in &files {
            let bytes = std::fs::read(f).unwrap();
            let bound = 4096 + (1 << 20) + DIRECT_CHUNK as u64;
            assert!((bytes.len() as u64) <= bound, "{} holds {} bytes", f.display(), bytes.len());
            all.extend(bytes);
        }
        assert!(all == data, "the files together are not what was written");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_files_follow_truncation_and_flush() {
        let dir = scratch_dir("truncate");
        let path = dir.join("out.log");
        let policy = RotatePolicy { max_size: 0, ..RotatePolicy::default() };
        let w = DirectWriter::open(path.clone(), policy, "1", "stdout");
        let (pipe, t) = feed(b"first\n".to_vec());
        pump(&w, &pipe, true);
        t.join().unwrap();
        // Truncated by someone else (copytruncate): no hole of zeros.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        let (pipe, t) = feed(b"second\n".to_vec());
        pump(&w, &pipe, true);
        t.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"second\n");
        // `warden flush`.
        truncate_direct_here();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        let (pipe, t) = feed(b"third\n".to_vec());
        pump(&w, &pipe, true);
        t.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"third\n");
        // Deleted: a new file at the path.
        std::fs::remove_file(&path).unwrap();
        let (pipe, t) = feed(b"fourth\n".to_vec());
        pump(&w, &pipe, true);
        t.join().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"fourth\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_files_rotate_on_schedule_at_a_line_boundary() {
        let dir = scratch_dir("schedule");
        let path = dir.join("out.log");
        let policy = RotatePolicy { max_size: 0, keep: 3, ..RotatePolicy::default() };
        let w = DirectWriter::open(path.clone(), policy, "1", "stdout");
        let (r, mut wr) = nb_pipe();
        wr.write_all(b"abc\ndef").unwrap();
        pump(&w, &r, false);
        // The schedule fires while a line is unfinished: it waits for its end.
        w.file.borrow_mut().rot.next_rotation = Some(UNIX_EPOCH);
        wr.write_all(b"gh").unwrap();
        pump(&w, &r, false);
        assert!(!dir.join("out.log.1").exists(), "no newline yet: no rotation");
        wr.write_all(b"i\njk\n").unwrap();
        pump(&w, &r, false);
        assert_eq!(std::fs::read_to_string(dir.join("out.log.1")).unwrap(), "abc\ndefghi\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "jk\n");
        // Once: the next writes stay in the new file.
        wr.write_all(b"lm\n").unwrap();
        pump(&w, &r, false);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "jk\nlm\n");
        assert!(!dir.join("out.log.2").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_writers_of_one_file_never_share_a_line() {
        let dir = scratch_dir("turns");
        let path = dir.join("out.log");
        let policy = RotatePolicy { max_size: 0, ..RotatePolicy::default() };
        let a = DirectWriter::open(path.clone(), policy.clone(), "1", "stdout");
        let b = DirectWriter::open(path.clone(), policy, "1", "stdout");
        assert!(Rc::ptr_eq(&a.file, &b.file), "one shared file");
        let ((ar, mut aw), (br, mut bw)) = (nb_pipe(), nb_pipe());
        // A is mid-line: B waits for it to finish.
        aw.write_all(b"a starts").unwrap();
        pump(&a, &ar, false);
        bw.write_all(b"b line\n").unwrap();
        assert!(matches!(pump(&b, &br, false), Some(Step::Wait(_))));
        aw.write_all(b" and ends\n").unwrap();
        pump(&a, &ar, false);
        pump(&b, &br, false);
        // A stays mid-line past the grace period: B ends the line itself.
        aw.write_all(b"a stalls").unwrap();
        pump(&a, &ar, false);
        bw.write_all(b"b again\n").unwrap();
        let t0 = Instant::now();
        while let Some(Step::Wait(d)) = pump(&b, &br, false) {
            std::thread::sleep(d);
        }
        assert!(t0.elapsed() >= LINE_GRACE - Duration::from_millis(10));
        // A goes away mid-line: B doesn't wait at all.
        pump(&a, &ar, false); // nothing new
        aw.write_all(b"a dies").unwrap();
        pump(&a, &ar, false);
        drop(a);
        bw.write_all(b"b last\n").unwrap();
        assert_eq!(pump(&b, &br, false), Some(Step::Moved(0)));
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "a starts and ends\nb line\na stalls\nb again\na dies\nb last\n");
        drop((aw, bw));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_output_is_drained_even_when_the_file_is_unusable() {
        let dir = scratch_dir("unusable");
        // A directory where the file should be: open fails, bytes are discarded, never blocking.
        let path = dir.join("taken");
        std::fs::create_dir_all(&path).unwrap();
        let w = DirectWriter::open(path.clone(), RotatePolicy::default(), "1", "stdout");
        let (pipe, t) = feed(vec![b'x'; 300_000]);
        assert_eq!(pump(&w, &pipe, true), None, "EOF reached: everything was read");
        t.join().unwrap();
        assert_eq!(w.file.borrow().discarded, 300_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn direct_paths_and_tails() {
        let files = DirectFiles {
            out: "/l/out.log".into(),
            err: Some("/l/err.log".into()),
            per_worker: true,
            policy: RotatePolicy::default(),
        };
        assert_eq!(files.paths("2"), ("/l/out-2.log".into(), Some("/l/err-2.log".into())));
        let merged = DirectFiles { err: None, per_worker: false, ..files.clone() };
        assert_eq!(merged.paths("2"), ("/l/out.log".into(), None));
        let same = DirectFiles { err: Some("/l/out.log".into()), ..files };
        assert_eq!(same.paths("3"), ("/l/out-3.log".into(), None), "same file: one pipe");
        // `warden logs`: the file's last lines, framed like captured output.
        let dir = scratch_dir("tail");
        let path = dir.join("out.log");
        std::fs::write(&path, "one\ntwo\nthree").unwrap();
        let lines = direct_tail(&(path.clone(), "1".into(), "stdout"), 2);
        let shown: Vec<_> = lines.iter().map(|l| split_output(l)).collect();
        assert_eq!(shown, vec![Some(("1", "stdout", "two")), Some(("1", "stdout", "three"))]);
        assert!(direct_tail(&(dir.join("missing"), "1".into(), "stdout"), 2).is_empty());
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

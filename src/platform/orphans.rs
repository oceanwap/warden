//! Workers a killed supervisor left behind, and what a new supervisor does
//! about them, for an OS with no parent-death signal (macOS).
//!
//! On Linux a worker gets SIGTERM when its supervisor dies (`PR_SET_PDEATHSIG`).
//! macOS has nothing like it: a supervisor killed with SIGKILL (or that
//! crashed, or that launchd killed in the middle of a shutdown) leaves its
//! workers running, still holding the app's port, and the next supervisor of
//! the app starts a second set next to them. So each supervisor keeps a small
//! record of its own workers, and the next supervisor of the app stops the
//! ones that are still running before it starts new ones.
//!
//! The record, `<state dir>/orphans/<app>.<supervisor pid>.json`, lists the
//! supervisor and each worker as a pid with the process's start time
//! ([`Member`]). It is rewritten, atomically (a temporary file renamed over
//! it), whenever the workers change, and removed when the supervisor stops its
//! workers and exits. One file per supervisor, so two supervisors (of two
//! apps, or of one app by mistake) never overwrite each other's.
//!
//! At its start a supervisor reads the records of its app, and a worker is
//! stopped only if every one of these holds:
//!
//! - the record is from this boot (after a reboot nothing recorded exists);
//! - its supervisor is not running: no process has that pid, or the process
//!   that has it started at another time (the pid was reused);
//! - a process with the worker's pid exists, and started at the recorded
//!   time (a pid that was reused is never touched: it is somebody else);
//! - that process's parent is not the recorded supervisor.
//!
//! The worker's process group gets the app's stop signal, as at a normal
//! shutdown, and SIGKILL once `shutdown.grace_period` has passed. When the
//! worker is no longer the leader of its group (it moved itself to another),
//! only the worker is signalled: that group is not Warden's to stop.
//!
//! What this cannot do, and the docs (`docs/platforms.md`) say:
//!
//! - it runs at the next start of the app, not when the supervisor dies: until
//!   then the orphans keep serving, holding the port and their memory;
//! - a child of a worker that outlived the worker, in a group whose leader
//!   was already gone before the next start, is not found (the group's number
//!   may belong to another group by then, and nothing proves it does not);
//! - a worker spawned in the instant before the record was rewritten is not
//!   in it;
//! - between reading a process and signalling it there is a gap in which a
//!   process could exit and its pid be given to another; it takes the whole
//!   pid space being used up within microseconds.
//!
//! All of the logic is here and independent of the OS: [`Procs`] is how it
//! asks about and signals processes, the tests give it a table of fake ones,
//! and the one OS-specific question, [`Platform::proc_identity`], has an
//! implementation on Linux as well, so the tests also run it against real
//! processes.

use super::ProcIdentity;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// The format of the record; a record of a later one is left alone.
const VERSION: u32 = 1;

/// How often the sweep looks again at the processes it signalled.
const POLL: Duration = Duration::from_millis(50);

/// How long after SIGKILL the sweep waits for a process to be gone.
const KILL_WAIT: Duration = Duration::from_secs(3);

/// A temporary file of a write that never finished is removed after this.
const TMP_MAX_AGE: Duration = Duration::from_secs(60);

/// A record is a few hundred bytes (a pid, a start time and a label per
/// worker); a file bigger than this is not one, and is not read.
const MAX_RECORD: u64 = 1 << 20;

/// `Status.rollout.kind` while a supervisor waits for a killed one's workers
/// to stop: `warden status` and `warden list` show it like a rollout in
/// progress, and `warden start` waits for it to end.
pub const SWEEP_KIND: &str = "sweep";

/// The longest a sweep takes: the stop signal, `grace` for the workers to
/// obey it, SIGKILL, and a wait for them to be gone.
pub fn longest(grace: Duration) -> Duration {
    grace + KILL_WAIT
}

/// How far a sweep has got, shared with the supervisor that runs it so that
/// `status` can say.
#[derive(Debug, Default)]
pub struct Progress {
    total: AtomicUsize,
    done: AtomicUsize,
}

impl Progress {
    /// `(done, total)`: the workers found, and how many of them are gone.
    pub fn counts(&self) -> (usize, usize) {
        (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed))
    }
}

/// A process of Warden's: the supervisor, or a worker (`label`: `1`, `s1`,
/// `host`, as in the logs).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub pid: u32,
    /// `ProcIdentity::start`: what shows the pid is still this process.
    pub start: u64,
    #[serde(default)]
    pub label: String,
}

/// What one supervisor wrote down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub v: u32,
    pub app: String,
    /// `platform::boot_id()` when it was written, when the OS has one.
    #[serde(default)]
    pub boot: Option<String>,
    /// Unix time in milliseconds, for people (`warden doctor`).
    #[serde(default)]
    pub written_ms: u64,
    pub supervisor: Member,
    pub workers: Vec<Member>,
}

// ---------------------------------------------------------------- processes

/// How the sweep asks about and signals processes.
pub trait Procs {
    /// `Platform::proc_identity`: `None` for no such process and for a zombie.
    fn identity(&self, pid: u32) -> Option<ProcIdentity>;
    /// Does any process belong to group `pgid`?
    fn group_exists(&self, pgid: u32) -> bool;
    /// `sig` to the process, or to every process of the group of that number.
    fn signal(&self, pid: u32, group: bool, sig: i32) -> io::Result<()>;
}

/// The real processes of this OS.
pub struct Os;

impl Procs for Os {
    fn identity(&self, pid: u32) -> Option<ProcIdentity> {
        super::proc_identity(pid)
    }

    fn group_exists(&self, pgid: u32) -> bool {
        let Ok(g) = i32::try_from(pgid) else { return false };
        if g <= 1 {
            return false;
        }
        // Signal 0 only asks; EPERM says there is a group, but not ours.
        match crate::sys::kill(-g, 0) {
            Ok(()) => true,
            Err(e) => e.raw_os_error() == Some(libc::EPERM),
        }
    }

    fn signal(&self, pid: u32, group: bool, sig: i32) -> io::Result<()> {
        let p = i32::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
        // Never pid 1, nor (through a negative pid) pid -1: a record is a file.
        if p <= 1 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("refusing to signal pid {pid}")));
        }
        crate::sys::kill(if group { -p } else { p }, sig)
    }
}

// ----------------------------------------------------------------- the plan

/// A worker of a dead supervisor that is still running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub member: Member,
    /// It leads its own process group: stop the group (the worker and what it
    /// started), not just the worker.
    pub group: bool,
}

/// What a record says about the processes now.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Workers to stop.
    pub orphans: Vec<Orphan>,
    /// The recorded supervisor is running: the workers are its, not orphans.
    pub live_supervisor: Option<Member>,
    /// Workers whose pid now belongs to another process: not touched.
    pub reused: Vec<Member>,
    /// Workers that are gone.
    pub gone: Vec<Member>,
    /// Workers whose parent is still the recorded supervisor, though nothing
    /// shows that supervisor running: not touched.
    pub still_parented: Vec<Member>,
    /// The record is from before the last reboot: nothing in it exists.
    pub from_an_earlier_boot: bool,
}

/// Who is asking: what the sweep must never signal.
pub struct Ctx<'a> {
    pub procs: &'a dyn Procs,
    /// `platform::boot_id()`.
    pub boot: Option<&'a str>,
    /// The asking process.
    pub me: u32,
}

/// Which of the record's workers are orphans (see the module doc for the
/// rule). Only asks; signals nothing.
pub fn plan(rec: &Record, cx: &Ctx<'_>) -> Plan {
    let mut plan = Plan::default();
    if let (Some(then), Some(now)) = (rec.boot.as_deref(), cx.boot) {
        if then != now {
            plan.from_an_earlier_boot = true;
            return plan;
        }
    }
    let sup = &rec.supervisor;
    if cx.procs.identity(sup.pid).is_some_and(|id| id.start == sup.start) {
        plan.live_supervisor = Some(sup.clone());
        return plan;
    }
    let my_group = cx.procs.identity(cx.me).map(|id| id.pgid);
    for w in &rec.workers {
        // Never init, never this process: a corrupt record is not a licence.
        if w.pid <= 1 || w.pid == cx.me {
            plan.gone.push(w.clone());
            continue;
        }
        match cx.procs.identity(w.pid) {
            None => plan.gone.push(w.clone()),
            Some(id) if id.start != w.start => plan.reused.push(w.clone()),
            Some(id) if id.ppid == sup.pid => plan.still_parented.push(w.clone()),
            Some(id) => {
                let leads_a_group = id.pgid == w.pid && my_group != Some(w.pid);
                plan.orphans.push(Orphan { member: w.clone(), group: leads_a_group });
            }
        }
    }
    plan
}

// ------------------------------------------------------------------ stopping

/// How the stop of one orphan ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    Running,
    /// Gone after the stop signal.
    Stopped,
    /// Gone after SIGKILL.
    Killed,
    /// Its pid is another process's now (it exited, and the number was taken).
    Reused,
    /// Still there after SIGKILL and `KILL_WAIT`.
    Survived,
}

/// How it ended, for the log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Outcome {
    pub stopped: Vec<Member>,
    pub killed: Vec<Member>,
    pub reused: Vec<Member>,
    pub survived: Vec<Member>,
}

/// The stop of a set of orphans: the stop signal to each at once, then, once
/// `grace` has passed, SIGKILL to those still there, as a normal shutdown
/// does. A state machine driven by [`Stopper::poll`], so tests need no clock.
pub struct Stopper<'a> {
    procs: &'a dyn Procs,
    grace: Duration,
    started: Instant,
    killed_at: Option<Instant>,
    targets: Vec<(Orphan, Fate)>,
}

/// What is left of an orphan.
enum Left {
    /// There still is: its group number now, if it leads one.
    There {
        group: bool,
    },
    Gone,
    Reused,
}

impl<'a> Stopper<'a> {
    /// Sends `stop_signal` to every orphan.
    pub fn new(procs: &'a dyn Procs, orphans: Vec<Orphan>, stop_signal: i32, grace: Duration, now: Instant) -> Self {
        let mut s = Stopper { procs, grace, started: now, killed_at: None, targets: Vec::new() };
        for o in orphans {
            // An orphan that changed since the plan (gone, its pid taken) is
            // seen by the first `poll`: look again right before signalling.
            match s.left(&o) {
                Left::There { group } => {
                    s.signal(&o, group, stop_signal);
                    s.targets.push((o, Fate::Running));
                }
                Left::Gone => s.targets.push((o, Fate::Stopped)),
                Left::Reused => s.targets.push((o, Fate::Reused)),
            }
        }
        s
    }

    /// Is the orphan still there, and is that still the process Warden
    /// started? A group whose leader is gone counts while it has members: its
    /// number cannot go to another process while it has, and it was seen with
    /// its leader moments ago.
    fn left(&self, o: &Orphan) -> Left {
        match self.procs.identity(o.member.pid) {
            Some(id) if id.start == o.member.start => Left::There { group: o.group && id.pgid == o.member.pid },
            Some(_) => Left::Reused,
            None if o.group && self.procs.group_exists(o.member.pid) => Left::There { group: true },
            None => Left::Gone,
        }
    }

    fn signal(&self, o: &Orphan, group: bool, sig: i32) {
        // A process that is gone is what was wanted; anything else (EPERM) is
        // seen as it staying there.
        let _ = self.procs.signal(o.member.pid, group, sig);
    }

    /// Look at the orphans: `None` while any may still be stopped, the
    /// outcome when none is running. Sends SIGKILL when `grace` has passed.
    pub fn poll(&mut self, now: Instant) -> Option<Outcome> {
        let mut still = Vec::new();
        for i in 0..self.targets.len() {
            if self.targets[i].1 != Fate::Running {
                continue;
            }
            match self.left(&self.targets[i].0) {
                Left::Gone => {
                    self.targets[i].1 = if self.killed_at.is_some() { Fate::Killed } else { Fate::Stopped };
                }
                Left::Reused => self.targets[i].1 = Fate::Reused,
                Left::There { group } => still.push((i, group)),
            }
        }
        if still.is_empty() {
            return Some(self.outcome());
        }
        match self.killed_at {
            None if now.saturating_duration_since(self.started) >= self.grace => {
                for (i, group) in &still {
                    self.signal(&self.targets[*i].0, *group, libc::SIGKILL);
                }
                self.killed_at = Some(now);
            }
            Some(at) if now.saturating_duration_since(at) >= KILL_WAIT => {
                for (i, _) in still {
                    self.targets[i].1 = Fate::Survived;
                }
                return Some(self.outcome());
            }
            _ => {}
        }
        None
    }

    fn outcome(&self) -> Outcome {
        let mut out = Outcome::default();
        for (o, fate) in &self.targets {
            let list = match fate {
                Fate::Stopped => &mut out.stopped,
                Fate::Killed => &mut out.killed,
                Fate::Reused => &mut out.reused,
                Fate::Survived | Fate::Running => &mut out.survived,
            };
            list.push(o.member.clone());
        }
        out
    }

    /// `(done, total)`: how many orphans are no longer running.
    fn counts(&self) -> (usize, usize) {
        (self.targets.iter().filter(|(_, fate)| *fate != Fate::Running).count(), self.targets.len())
    }

    /// Wait for the end, looking every `POLL`, and telling `progress` how many
    /// are gone as they go.
    pub async fn finish(mut self, progress: &Progress) -> Outcome {
        loop {
            let out = self.poll(Instant::now());
            let (done, total) = self.counts();
            progress.total.store(total, Ordering::Relaxed);
            progress.done.store(done, Ordering::Relaxed);
            if let Some(out) = out {
                return out;
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

// ---------------------------------------------------------------- the files

/// Where the records are kept, in the state directory.
pub fn dir(state_dir: &Path) -> PathBuf {
    state_dir.join("orphans")
}

pub fn file_name(app: &str, supervisor: u32) -> String {
    format!("{app}.{supervisor}.json")
}

/// `(app, supervisor pid)` of a record's file name: `api.v2.123.json` is
/// app `api.v2`, supervisor 123.
fn parse_file_name(name: &str) -> Option<(&str, u32)> {
    let (app, pid) = name.strip_suffix(".json")?.rsplit_once('.')?;
    (!app.is_empty()).then_some(())?;
    Some((app, pid.parse().ok()?))
}

/// A record on disk, or why it could not be used.
#[derive(Debug)]
pub struct Found {
    pub path: PathBuf,
    pub record: Result<Record, Unusable>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Unusable {
    /// Not a record (cut short, not JSON, the wrong shape): it is garbage.
    Garbled(String),
    /// A record of a later Warden: left alone.
    Newer(u32),
}

/// Parse a record; the app must be the file's.
fn parse(text: &str, app: &str) -> Result<Record, Unusable> {
    #[derive(Deserialize)]
    struct Version {
        v: u32,
    }
    let v = serde_json::from_str::<Version>(text).map_err(|e| Unusable::Garbled(e.to_string()))?.v;
    if v > VERSION {
        return Err(Unusable::Newer(v));
    }
    let rec: Record = serde_json::from_str(text).map_err(|e| Unusable::Garbled(e.to_string()))?;
    if rec.app != app {
        return Err(Unusable::Garbled(format!("it is for app {:?}, not {app:?}", rec.app)));
    }
    if rec.supervisor.pid <= 1 {
        return Err(Unusable::Garbled(format!("supervisor pid {}", rec.supervisor.pid)));
    }
    Ok(rec)
}

/// The records of `app` in `dir` (every app's when `None`), by file name.
/// Temporary files of writes that never finished and are old are removed.
pub fn read_all(dir: &Path, app: Option<&str>) -> Vec<Found> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut found = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let Some((file_app, _)) = parse_file_name(&name) else {
            remove_old_temporary(&path, &name, app);
            continue;
        };
        if app.is_some_and(|a| a != file_app) {
            continue;
        }
        let record = match read_record(&path) {
            Ok(text) => parse(&text, file_app),
            Err(why) => Err(Unusable::Garbled(why)),
        };
        found.push(Found { path, record });
    }
    found.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// The text of a record file, if it can be one: a regular file (a link, or a
/// pipe that would block the read, is not) of at most `MAX_RECORD` bytes, read
/// no further than that.
fn read_record(path: &Path) -> Result<String, String> {
    use std::io::Read as _;
    let meta = std::fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    if meta.len() > MAX_RECORD {
        return Err(format!("it is {} bytes: a record is a few hundred", meta.len()));
    }
    let mut text = String::new();
    // The file may have grown since: never read more than the limit.
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_RECORD + 1).read_to_string(&mut text))
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_RECORD {
        return Err(format!("it is over {MAX_RECORD} bytes: a record is a few hundred"));
    }
    Ok(text)
}

/// `<app>.<pid>.tmp<pid>` older than `TMP_MAX_AGE`: a write cut short.
fn remove_old_temporary(path: &Path, name: &str, app: Option<&str>) {
    let Some((head, tail)) = name.rsplit_once(".tmp") else { return };
    if tail.is_empty() || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return;
    }
    if app.is_some_and(|a| !head.starts_with(&format!("{a}."))) {
        return;
    }
    let old = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age > TMP_MAX_AGE);
    if old {
        let _ = std::fs::remove_file(path);
    }
}

/// Write `rec` to `path` so that a reader sees the old file or the new one,
/// never half of one: into a file of its own, renamed over it. Not synced:
/// what it protects against is a killed process, and a machine that lost
/// power has no processes left to sweep.
fn write_atomic(path: &Path, rec: &Record) -> io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    let mut text = serde_json::to_vec(rec).map_err(io::Error::other)?;
    text.push(b'\n');
    // create_new: never through a link somebody left at this name.
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
    let written = f.write_all(&text).and_then(|()| f.flush());
    drop(f);
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

// ---------------------------------------------------------- the supervisor's

/// This supervisor's record: what it keeps up to date while it runs.
pub struct Registry {
    file: PathBuf,
    rec: Record,
    /// A worker's identity does not change: read once per pid.
    known: HashMap<u32, Member>,
    /// The last write failed, and was said.
    failing: bool,
}

impl Registry {
    fn new(dir: &Path, app: &str, me: Member, boot: Option<String>) -> Registry {
        let file = dir.join(file_name(app, me.pid));
        let rec =
            Record { v: VERSION, app: app.into(), boot, written_ms: now_ms(), supervisor: me, workers: Vec::new() };
        Registry { file, rec, known: HashMap::new(), failing: false }
    }

    /// A registry of this process for another module's tests: its record is
    /// `<dir>/<app>.<this pid>.json`.
    #[cfg(test)]
    pub(crate) fn for_test(dir: &Path, app: &str) -> Registry {
        let me = std::process::id();
        let start = super::proc_identity(me).map_or(0, |id| id.start);
        Registry::new(dir, app, Member { pid: me, start, label: String::new() }, super::boot_id())
    }

    /// The workers the record names now: `(pid, label)`, for tests.
    #[cfg(test)]
    pub(crate) fn workers(&self) -> Vec<(u32, String)> {
        self.rec.workers.iter().map(|m| (m.pid, m.label.clone())).collect()
    }

    /// The supervisor's workers are these (`pid`, label): write the record if
    /// they are not what it says. A process that cannot be read (it has
    /// already exited) is left out: it cannot be told from another later.
    pub fn note(&mut self, workers: &[(u32, String)]) {
        let mut now: Vec<Member> = Vec::with_capacity(workers.len());
        for (pid, label) in workers {
            let member = match self.known.get(pid) {
                Some(m) => Member { label: label.clone(), ..m.clone() },
                None => {
                    let Some(id) = super::proc_identity(*pid) else { continue };
                    Member { pid: *pid, start: id.start, label: label.clone() }
                }
            };
            self.known.insert(*pid, member.clone());
            now.push(member);
        }
        self.known.retain(|pid, _| workers.iter().any(|(p, _)| p == pid));
        now.sort_by_key(|m| m.pid);
        if now == self.rec.workers && self.file.exists() {
            return;
        }
        self.rec.workers = now;
        self.rec.written_ms = now_ms();
        self.write();
    }

    fn write(&mut self) {
        match write_atomic(&self.file, &self.rec) {
            Ok(()) => self.failing = false,
            Err(e) if !self.failing => {
                self.failing = true;
                crate::warn!(
                    "cannot record the workers; if this supervisor is killed, its workers will not be stopped when the app starts again",
                    file = self.file.display(),
                    error = e,
                    hint = "is the state directory writable, and the disk not full? `warden doctor` checks",
                );
            }
            Err(_) => {}
        }
    }

    /// The supervisor has stopped its workers and is exiting: nothing is left
    /// to sweep.
    pub fn close(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// What starting needs to know.
pub struct Settings<'a> {
    pub app: &'a str,
    /// `[shutdown] signal`, and `[shutdown] grace_period`: how workers are
    /// stopped at a normal shutdown.
    pub stop_signal: i32,
    pub grace: Duration,
    /// Where the records are (`dir(state_dir)`).
    pub dir: PathBuf,
    /// Told how many workers were found and how many are gone: what
    /// `status` shows while the sweep runs.
    pub progress: Arc<Progress>,
}

/// On this OS, does a supervisor keep and sweep records? macOS: yes.
/// Elsewhere (Linux, with its parent-death signal) no, so a debug build can
/// be told to behave like macOS (`WARDEN_TEST_MACOS_ORPHANS=1`: workers
/// outlive their supervisor, and the sweep is on), which is how Linux tests
/// run the whole thing against real processes.
pub fn enabled() -> bool {
    if super::current().capabilities().orphan_sweep {
        return true;
    }
    #[cfg(all(debug_assertions, target_os = "linux"))]
    if std::env::var_os("WARDEN_TEST_MACOS_ORPHANS").is_some() {
        crate::sys::test_workers_outlive_the_supervisor();
        return true;
    }
    false
}

/// A supervisor of `app` starting: stop the workers a dead supervisor of the
/// app left behind, then start recording this one's. `None` when this OS
/// does not need it, or the record cannot be kept (said in the log).
pub async fn start(s: Settings<'_>) -> Option<Registry> {
    if !enabled() {
        return None;
    }
    let me = std::process::id();
    let boot = super::boot_id();
    if let Err(e) = crate::control::ensure_private_dir(&s.dir) {
        crate::warn!(
            "cannot keep the record of the workers; if this supervisor is killed, its workers will not be stopped when the app starts again",
            error = e,
            hint = "the record directory must be a real directory of this user that no one else can write to",
        );
        return None;
    }
    sweep(&s, &Os, boot.as_deref(), me).await;
    let Some(id) = super::proc_identity(me) else {
        crate::warn!(
            "cannot read this process's start time; its workers will not be recorded",
            hint = "if this supervisor is killed, its workers are not stopped when the app starts again; `warden doctor` says what the OS adapter cannot read",
        );
        return None;
    };
    let mut reg = Registry::new(&s.dir, s.app, Member { pid: me, start: id.start, label: String::new() }, boot);
    reg.write();
    Some(reg)
}

/// Read the records of the app, stop what is orphaned, and drop the records
/// that have been dealt with.
pub async fn sweep(s: &Settings<'_>, procs: &dyn Procs, boot: Option<&str>, me: u32) {
    let cx = Ctx { procs, boot, me };
    let mut orphans: Vec<Orphan> = Vec::new();
    // Records to remove once their orphans are stopped: path, pids.
    let mut done: Vec<(PathBuf, Vec<u32>)> = Vec::new();
    // The supervisors the orphans had, for the log.
    let mut previous: Vec<u32> = Vec::new();
    for found in read_all(&s.dir, Some(s.app)) {
        let rec = match found.record {
            Ok(rec) => rec,
            Err(Unusable::Newer(v)) => {
                crate::warn!(
                    "a record of workers was written by a newer Warden; it is left alone",
                    file = found.path.display(),
                    version = v,
                    hint = "workers it lists, if still running, are not stopped here: use the newer `warden`, or kill them by hand",
                );
                continue;
            }
            Err(Unusable::Garbled(why)) => {
                crate::warn!(
                    "a record of workers could not be read; removed",
                    file = found.path.display(),
                    reason = why,
                    hint = "workers of a killed supervisor that it listed, if any, are not stopped: look for them with `ps` (`warden doctor` lists what it can)",
                );
                let _ = std::fs::remove_file(&found.path);
                continue;
            }
        };
        let p = plan(&rec, &cx);
        if let Some(sup) = &p.live_supervisor {
            // Another supervisor of this app is running (a second one, on
            // another control socket): its workers are its own.
            crate::warn!(
                "another supervisor of this app is running; its workers are left alone",
                supervisor = sup.pid,
                workers = rec.workers.len(),
                hint = "two supervisors of one app start two sets of workers: stop one of them",
            );
            continue;
        }
        for w in &p.reused {
            crate::debug!("a recorded worker's pid belongs to another process now; left alone", pid = w.pid);
        }
        for w in &p.still_parented {
            crate::debug!("a recorded worker still has the recorded supervisor as its parent; left alone", pid = w.pid);
        }
        if !p.orphans.is_empty() {
            previous.push(rec.supervisor.pid);
        }
        done.push((found.path, p.orphans.iter().map(|o| o.member.pid).collect()));
        orphans.extend(p.orphans);
    }
    let mut survivors: Vec<u32> = Vec::new();
    if !orphans.is_empty() {
        let describe = |list: &[Member]| {
            list.iter()
                .map(|m| if m.label.is_empty() { m.pid.to_string() } else { format!("{} (worker {})", m.pid, m.label) })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let members: Vec<Member> = orphans.iter().map(|o| o.member.clone()).collect();
        crate::warn!(
            "workers of a previous supervisor of this app are still running; stopping them before starting new ones",
            workers = describe(&members),
            previous_supervisor = previous.iter().map(u32::to_string).collect::<Vec<_>>().join(", "),
            hint = "that supervisor was killed (SIGKILL, a crash, or launchd's timeout) and macOS has no parent-death \
                    signal, so its workers kept running; `warden stop` or SIGTERM ends a supervisor and its workers \
                    together",
        );
        let outcome = Stopper::new(procs, orphans, s.stop_signal, s.grace, Instant::now()).finish(&s.progress).await;
        if !outcome.killed.is_empty() {
            crate::warn!(
                "workers left behind by a previous supervisor did not exit within the grace period; sent SIGKILL",
                workers = describe(&outcome.killed),
                grace_s = s.grace.as_secs(),
                hint = "the app ignored the stop signal or kept running work: close servers and DB pools on SIGTERM, \
                        or raise shutdown.grace_period",
            );
        }
        for m in &outcome.survived {
            survivors.push(m.pid);
            crate::error!(
                "could not stop a worker a previous supervisor left behind",
                worker = describe(std::slice::from_ref(m)),
                hint = format!("it may still hold the app's port: `kill -9 {}` stops it", m.pid),
            );
        }
        let stopped = outcome.stopped.len() + outcome.killed.len();
        if stopped > 0 {
            crate::info!(
                "stopped workers left behind by a previous supervisor",
                stopped = stopped,
                killed_after_grace = outcome.killed.len(),
            );
        }
    }
    // A record whose workers could not all be stopped stays for the next start.
    for (path, pids) in done {
        if !pids.iter().any(|p| survivors.contains(p)) {
            let _ = std::fs::remove_file(path);
        }
    }
}

// ------------------------------------------------------------------- doctor

/// The apps with workers a dead supervisor left running now, for `warden
/// doctor`: `(app, supervisor pid, orphans)`. Only reads.
pub fn running_orphans(dir: &Path, procs: &dyn Procs, boot: Option<&str>, me: u32) -> Vec<(String, u32, Vec<Orphan>)> {
    let cx = Ctx { procs, boot, me };
    read_all(dir, None)
        .into_iter()
        .filter_map(|f| f.record.ok())
        .filter_map(|rec| {
            let p = plan(&rec, &cx);
            (!p.orphans.is_empty()).then(|| (rec.app.clone(), rec.supervisor.pid, p.orphans))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    const TERM: i32 = libc::SIGTERM;
    const KILL: i32 = libc::SIGKILL;

    fn m(pid: u32, start: u64) -> Member {
        Member { pid, start, label: pid.to_string() }
    }

    fn id(start: u64, ppid: u32, pgid: u32) -> ProcIdentity {
        ProcIdentity { start, ppid, pgid }
    }

    /// How a fake process answers the stop signal.
    #[derive(Clone, Copy, PartialEq)]
    enum OnTerm {
        Exits,
        Ignores,
    }

    /// A table of fake processes that die when signalled, as told.
    #[derive(Default)]
    struct Fake {
        table: RefCell<BTreeMap<u32, (ProcIdentity, OnTerm)>>,
        /// Every signal sent: (pid, group, signal).
        sent: RefCell<Vec<(u32, bool, i32)>>,
        /// Pids that refuse to die (SIGKILL does nothing: uninterruptible).
        immortal: RefCell<Vec<u32>>,
    }

    impl Fake {
        fn add(&self, pid: u32, identity: ProcIdentity, on_term: OnTerm) {
            self.table.borrow_mut().insert(pid, (identity, on_term));
        }
        fn remove(&self, pid: u32) {
            self.table.borrow_mut().remove(&pid);
        }
        fn sent(&self) -> Vec<(u32, bool, i32)> {
            self.sent.borrow().clone()
        }
        fn alive(&self, pid: u32) -> bool {
            self.table.borrow().contains_key(&pid)
        }
    }

    impl Procs for Fake {
        fn identity(&self, pid: u32) -> Option<ProcIdentity> {
            self.table.borrow().get(&pid).map(|(i, _)| *i)
        }
        fn group_exists(&self, pgid: u32) -> bool {
            self.table.borrow().values().any(|(i, _)| i.pgid == pgid)
        }
        fn signal(&self, pid: u32, group: bool, sig: i32) -> io::Result<()> {
            self.sent.borrow_mut().push((pid, group, sig));
            let victims: Vec<u32> = self
                .table
                .borrow()
                .iter()
                .filter(|(p, (i, _))| if group { i.pgid == pid } else { **p == pid })
                .map(|(p, _)| *p)
                .collect();
            if victims.is_empty() {
                return Err(io::Error::from_raw_os_error(libc::ESRCH));
            }
            for v in victims {
                let on_term = self.table.borrow()[&v].1;
                let dies =
                    sig == KILL && !self.immortal.borrow().contains(&v) || sig == TERM && on_term == OnTerm::Exits;
                if dies {
                    self.table.borrow_mut().remove(&v);
                }
            }
            Ok(())
        }
    }

    fn record(sup: Member, workers: Vec<Member>) -> Record {
        Record { v: VERSION, app: "api".into(), boot: Some("boot-1".into()), written_ms: 1, supervisor: sup, workers }
    }

    fn cx(fake: &Fake) -> Ctx<'_> {
        Ctx { procs: fake, boot: Some("boot-1"), me: 900 }
    }

    // ------------------------------------------------------------- planning

    #[test]
    fn a_running_worker_of_a_dead_supervisor_is_an_orphan_and_leads_its_group() {
        let fake = Fake::default();
        fake.add(900, id(5000, 1, 900), OnTerm::Exits); // this process
        fake.add(101, id(1001, 1, 101), OnTerm::Exits); // worker 1: its own group, its parent is init now
        let rec = record(m(100, 1000), vec![m(101, 1001)]);
        let p = plan(&rec, &cx(&fake));
        assert_eq!(p.orphans, [Orphan { member: m(101, 1001), group: true }]);
        assert!(p.live_supervisor.is_none() && p.reused.is_empty() && p.gone.is_empty());
    }

    #[test]
    fn a_worker_that_left_its_process_group_is_signalled_alone() {
        let fake = Fake::default();
        // Its group is another's (pgid 555): not Warden's to stop.
        fake.add(101, id(1001, 1, 555), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert_eq!(p.orphans, [Orphan { member: m(101, 1001), group: false }]);
    }

    #[test]
    fn a_pid_that_belongs_to_another_process_now_is_never_touched() {
        let fake = Fake::default();
        // Pid 101 was a worker; the process there started later: somebody else's.
        fake.add(101, id(9999, 1, 101), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert!(p.orphans.is_empty());
        assert_eq!(p.reused, [m(101, 1001)]);
        // Not even a start time that is one tick off, either way.
        for start in [1000, 1002] {
            let fake = Fake::default();
            fake.add(101, id(start, 1, 101), OnTerm::Exits);
            assert!(plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake)).orphans.is_empty(), "{start}");
        }
    }

    #[test]
    fn nothing_is_an_orphan_while_its_supervisor_runs() {
        let fake = Fake::default();
        fake.add(100, id(1000, 1, 100), OnTerm::Exits); // the supervisor, alive, same start
        fake.add(101, id(1001, 100, 101), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert!(p.orphans.is_empty());
        assert_eq!(p.live_supervisor, Some(m(100, 1000)));
        // Even when the worker has been left by its parent: the supervisor decides.
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        assert!(plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake)).orphans.is_empty());
    }

    #[test]
    fn a_supervisor_pid_taken_by_another_process_is_a_dead_supervisor() {
        let fake = Fake::default();
        fake.add(100, id(7777, 1, 100), OnTerm::Exits); // pid 100 is somebody else's now
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert!(p.live_supervisor.is_none());
        assert_eq!(p.orphans.len(), 1);
    }

    #[test]
    fn a_worker_whose_parent_is_still_the_recorded_supervisor_is_left_alone() {
        // The supervisor cannot be read (another user's, say), but the worker
        // says it is still its child: it is not an orphan.
        let fake = Fake::default();
        fake.add(101, id(1001, 100, 101), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert!(p.orphans.is_empty());
        assert_eq!(p.still_parented, [m(101, 1001)]);
    }

    #[test]
    fn dead_workers_zombies_init_and_this_process_are_not_orphans() {
        let fake = Fake::default();
        // 102 is not in the table: gone or a zombie (the adapter says None for both).
        fake.add(900, id(5000, 1, 900), OnTerm::Exits);
        fake.add(1, id(1, 0, 1), OnTerm::Exits);
        let rec = record(m(100, 1000), vec![m(102, 1002), m(1, 1), m(900, 5000), m(0, 0)]);
        let p = plan(&rec, &cx(&fake));
        assert!(p.orphans.is_empty(), "{p:?}");
        assert_eq!(p.gone.len(), 4);
    }

    #[test]
    fn a_record_from_before_a_reboot_describes_nothing() {
        let fake = Fake::default();
        // The same pid and start time after a reboot: not the same process.
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        let mut rec = record(m(100, 1000), vec![m(101, 1001)]);
        rec.boot = Some("boot-0".into());
        let p = plan(&rec, &cx(&fake));
        assert!(p.from_an_earlier_boot && p.orphans.is_empty());
        // A boot id that cannot be read, on either side, is not a reason to ignore it.
        let unknown = Ctx { procs: &fake, boot: None, me: 900 };
        assert_eq!(plan(&rec, &unknown).orphans.len(), 1);
        rec.boot = None;
        assert_eq!(plan(&rec, &cx(&fake)).orphans.len(), 1);
    }

    #[test]
    fn a_worker_that_leads_the_group_this_process_is_in_is_never_signalled_as_a_group() {
        let fake = Fake::default();
        fake.add(900, id(5000, 1, 101), OnTerm::Exits); // this process is in group 101 (a corrupt record)
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        let p = plan(&record(m(100, 1000), vec![m(101, 1001)]), &cx(&fake));
        assert_eq!(p.orphans, [Orphan { member: m(101, 1001), group: false }]);
    }

    // ------------------------------------------------------------- stopping

    fn orphan(pid: u32, start: u64) -> Orphan {
        Orphan { member: m(pid, start), group: true }
    }

    #[test]
    fn workers_that_exit_on_the_stop_signal_are_not_killed() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        fake.add(102, id(1002, 1, 102), OnTerm::Exits);
        let t0 = Instant::now();
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001), orphan(102, 1002)], TERM, Duration::from_secs(30), t0);
        assert_eq!(fake.sent(), [(101, true, TERM), (102, true, TERM)], "the stop signal, to each group");
        let out = s.poll(t0 + POLL).expect("both are gone");
        assert_eq!((out.stopped.len(), out.killed.len(), out.survived.len()), (2, 0, 0));
        assert_eq!(fake.sent().len(), 2, "no SIGKILL");
    }

    #[test]
    fn the_stopper_counts_how_many_are_gone_as_they_go() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        fake.add(102, id(1002, 1, 102), OnTerm::Ignores);
        let t0 = Instant::now();
        let grace = Duration::from_secs(5);
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001), orphan(102, 1002)], TERM, grace, t0);
        assert_eq!(s.counts(), (0, 2), "both were told to stop; none has looked gone yet");
        assert!(s.poll(t0 + POLL).is_none());
        assert_eq!(s.counts(), (1, 2), "one obeyed, one did not");
        assert!(s.poll(t0 + grace + POLL).is_none() || !fake.alive(102));
        assert!(s.poll(t0 + grace + 2 * POLL).is_some());
        assert_eq!(s.counts(), (2, 2));
        // The shared progress a supervisor reads for `status` ends the same way.
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        let progress = Progress::default();
        assert_eq!(progress.counts(), (0, 0));
        let s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, grace, Instant::now());
        let out = rt().block_on(s.finish(&progress));
        assert_eq!(out.stopped.len(), 1);
        assert_eq!(progress.counts(), (1, 1));
    }

    #[test]
    fn the_configured_stop_signal_is_the_one_sent() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Ignores);
        let _ = Stopper::new(&fake, vec![orphan(101, 1001)], libc::SIGINT, Duration::from_secs(1), Instant::now());
        assert_eq!(fake.sent(), [(101, true, libc::SIGINT)]);
    }

    #[test]
    fn a_worker_that_ignores_the_stop_signal_is_killed_when_the_grace_period_is_over() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Ignores);
        let t0 = Instant::now();
        let grace = Duration::from_secs(5);
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, grace, t0);
        assert!(s.poll(t0 + Duration::from_secs(4)).is_none(), "still within the grace period");
        assert_eq!(fake.sent(), [(101, true, TERM)]);
        // Over: SIGKILL to the group, once.
        assert!(s.poll(t0 + grace).is_none() || !fake.alive(101));
        assert_eq!(fake.sent(), [(101, true, TERM), (101, true, KILL)]);
        let out = s.poll(t0 + grace + POLL).expect("gone");
        assert_eq!((out.stopped.len(), out.killed.len()), (0, 1));
        assert_eq!(fake.sent().len(), 2, "SIGKILL once");
    }

    #[test]
    fn a_worker_that_will_not_die_is_reported_after_the_wait() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Ignores);
        fake.immortal.borrow_mut().push(101);
        let t0 = Instant::now();
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, Duration::from_secs(1), t0);
        assert!(s.poll(t0 + Duration::from_secs(1)).is_none(), "killed, looking");
        assert!(s.poll(t0 + Duration::from_secs(3)).is_none(), "still inside the wait");
        let out = s.poll(t0 + Duration::from_secs(1) + KILL_WAIT).expect("gave up");
        assert_eq!(out.survived, [m(101, 1001)]);
    }

    #[test]
    fn a_pid_taken_by_another_process_while_waiting_is_never_killed() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Ignores);
        let t0 = Instant::now();
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, Duration::from_secs(2), t0);
        // It exited by itself and a stranger got its pid (and its own group).
        fake.remove(101);
        fake.add(101, id(8888, 1, 101), OnTerm::Exits);
        let out = s.poll(t0 + Duration::from_secs(3)).expect("nothing left to stop");
        assert_eq!(out.reused, [m(101, 1001)]);
        assert!(!fake.sent().contains(&(101, true, KILL)) && fake.alive(101), "{:?}", fake.sent());
    }

    #[test]
    fn a_pid_taken_before_the_stop_signal_is_not_signalled_at_all() {
        let fake = Fake::default();
        fake.add(101, id(8888, 1, 101), OnTerm::Exits); // changed since the plan
        let s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, Duration::from_secs(1), Instant::now());
        assert!(fake.sent().is_empty());
        drop(s);
    }

    #[test]
    fn stragglers_of_a_group_whose_leader_exited_are_killed_with_the_group() {
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Exits); // the worker exits on the stop signal...
        fake.add(150, id(1050, 101, 101), OnTerm::Ignores); // ...but its child, in its group, does not
        let t0 = Instant::now();
        let mut s = Stopper::new(&fake, vec![orphan(101, 1001)], TERM, Duration::from_secs(2), t0);
        assert!(!fake.alive(101) && fake.alive(150));
        assert!(s.poll(t0 + Duration::from_secs(1)).is_none(), "the group is not empty: waiting");
        assert!(s.poll(t0 + Duration::from_secs(2)).is_none() || !fake.alive(150));
        assert!(fake.sent().contains(&(101, true, KILL)), "{:?}", fake.sent());
        let out = s.poll(t0 + Duration::from_secs(3)).expect("empty");
        assert_eq!(out.killed.len(), 1, "it took SIGKILL for the straggler");
        assert!(!fake.alive(150));
    }

    #[test]
    fn a_worker_that_moved_to_another_group_is_stopped_alone_and_its_old_group_is_left() {
        let fake = Fake::default();
        // Recorded as a group leader (group: true at the plan), now in group 555 with a stranger.
        fake.add(101, id(1001, 1, 555), OnTerm::Exits);
        fake.add(600, id(2000, 1, 555), OnTerm::Ignores);
        let o = Orphan { member: m(101, 1001), group: true };
        let t0 = Instant::now();
        let mut s = Stopper::new(&fake, vec![o], TERM, Duration::from_secs(1), t0);
        assert_eq!(fake.sent(), [(101, false, TERM)], "the process, not group 101 (nor 555)");
        assert!(s.poll(t0 + POLL).is_some());
        assert!(fake.alive(600), "the stranger in group 555 was not touched");
    }

    // ---------------------------------------------------------------- files

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("warden-orphans-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_record_survives_a_round_trip_and_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("roundtrip");
        let rec = record(m(100, 1000), vec![m(101, 1001), Member { pid: 102, start: 1002, label: "s1".into() }]);
        let path = dir.join(file_name("api", 100));
        write_atomic(&path, &rec).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let found = read_all(&dir, Some("api"));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].record.as_ref().unwrap(), &rec);
        // No temporary file is left next to it.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_names_carry_the_app_and_the_supervisor() {
        assert_eq!(file_name("api", 123), "api.123.json");
        assert_eq!(parse_file_name("api.123.json"), Some(("api", 123)));
        assert_eq!(parse_file_name("api.v2.123.json"), Some(("api.v2", 123)), "dots in the app's name");
        assert_eq!(parse_file_name("api.json"), None);
        assert_eq!(parse_file_name("api.x.json"), None);
        assert_eq!(parse_file_name(".123.json"), None);
        assert_eq!(parse_file_name("api.123.tmp456"), None);
        assert_eq!(parse_file_name("..123.json"), Some((".", 123)), "an app called `.` stays a file name");
    }

    #[test]
    fn a_torn_garbled_or_foreign_record_is_unusable_not_a_licence_to_kill() {
        let good = serde_json::to_string(&record(m(100, 1000), vec![m(101, 1001)])).unwrap();
        assert!(parse(&good, "api").is_ok());
        // Cut short anywhere: never a record (a write that was not atomic would look like this).
        for cut in [0, 1, good.len() / 2, good.len() - 2, good.len() - 1] {
            assert!(
                matches!(parse(&good[..cut], "api"), Err(Unusable::Garbled(_))),
                "cut at {cut}: {:?}",
                parse(&good[..cut], "api")
            );
        }
        assert!(matches!(parse("[]", "api"), Err(Unusable::Garbled(_))));
        assert!(matches!(parse("{\"v\":1}", "api"), Err(Unusable::Garbled(_))), "missing fields");
        assert!(matches!(parse(&good, "web"), Err(Unusable::Garbled(_))), "another app's");
        let newer = good.replacen("\"v\":1", "\"v\":2", 1);
        assert_eq!(parse(&newer, "api"), Err(Unusable::Newer(2)));
        let init = serde_json::to_string(&record(m(1, 1), vec![])).unwrap();
        assert!(matches!(parse(&init, "api"), Err(Unusable::Garbled(_))), "a supervisor that is pid 1");
        // A newer record's other fields may be anything: only its version is read.
        assert_eq!(parse("{\"v\":9,\"workers\":\"?\"}", "api"), Err(Unusable::Newer(9)));
    }

    #[test]
    fn only_the_apps_own_records_are_read_and_old_temporary_files_go() {
        let dir = scratch("read");
        let write = |app: &str, sup: u32| {
            let mut r = record(m(sup, 1000), vec![]);
            r.app = app.into();
            write_atomic(&dir.join(file_name(app, sup)), &r).unwrap();
        };
        write("api", 100);
        write("api", 200);
        write("web", 300);
        write("api.v2", 400);
        std::fs::write(dir.join("api.100.json.bak"), "x").unwrap();
        // A temporary file of a write cut short: old ones go, a fresh one (a write in progress) stays.
        let old = dir.join("api.5.tmp5");
        let fresh = dir.join("api.6.tmp6");
        std::fs::write(&old, "{").unwrap();
        std::fs::write(&fresh, "{").unwrap();
        let t = std::time::SystemTime::now() - Duration::from_secs(3600);
        std::fs::File::options().write(true).open(&old).unwrap().set_modified(t).unwrap();
        let apps: Vec<u32> =
            read_all(&dir, Some("api")).iter().map(|f| f.record.as_ref().unwrap().supervisor.pid).collect();
        assert_eq!(apps, [100, 200], "api's, not web's or api.v2's");
        assert!(!old.exists() && fresh.exists());
        assert_eq!(read_all(&dir, None).len(), 4, "every app's");
        assert!(read_all(&dir.join("missing"), None).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn writes_replace_the_record_whole() {
        let dir = scratch("atomic");
        let path = dir.join(file_name("api", 100));
        let mut rec = record(m(100, 1000), vec![]);
        for n in 0..50u32 {
            rec.workers = (0..n).map(|i| m(200 + i, 2000 + u64::from(i))).collect();
            write_atomic(&path, &rec).unwrap();
            // A reader at any moment finds a whole record, the last one written.
            assert_eq!(parse(&std::fs::read_to_string(&path).unwrap(), "api").unwrap(), rec);
        }
        // A directory in the way of the rename fails, and leaves no temporary file.
        let blocked = dir.join(file_name("api", 101));
        std::fs::create_dir(&blocked).unwrap();
        assert!(write_atomic(&blocked, &rec).is_err());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "the record and the directory");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_too_big_to_be_a_record_is_garbage_and_is_not_read() {
        let dir = scratch("bounded");
        // A record is a few hundred bytes. 300 MB (sparse here: it costs no disk) was read whole,
        // 322 MB of memory, before it was found to be nothing.
        let big = dir.join(file_name("api", 100));
        std::fs::File::create(&big).unwrap().set_len(300 << 20).unwrap();
        let why = read_record(&big).unwrap_err();
        assert!(why.contains("bytes"), "{why}");
        let found = read_all(&dir, Some("api"));
        assert_eq!(found.len(), 1);
        assert!(matches!(&found[0].record, Err(Unusable::Garbled(w)) if w.contains("bytes")), "{found:?}");
        // The limit is on the size: a file of exactly `MAX_RECORD` bytes is read (and is no record),
        // and one a byte over is not.
        let edge = dir.join(file_name("api", 101));
        std::fs::File::create(&edge).unwrap().set_len(MAX_RECORD).unwrap();
        assert_eq!(read_record(&edge).map(|t| t.len() as u64), Ok(MAX_RECORD));
        std::fs::File::create(&edge).unwrap().set_len(MAX_RECORD + 1).unwrap();
        assert!(read_record(&edge).is_err());
        // A real record is far below it, whatever the number of workers a supervisor has.
        let workers: Vec<Member> = (0..1000).map(|i| m(200 + i, 5000 + u64::from(i))).collect();
        let rec = record(m(100, 1000), workers);
        let path = dir.join(file_name("api", 102));
        write_atomic(&path, &rec).unwrap();
        assert!(std::fs::metadata(&path).unwrap().len() < MAX_RECORD / 4, "1000 workers fit a quarter of it");
        assert_eq!(parse(&read_record(&path).unwrap(), "api").unwrap(), rec);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_link_or_a_pipe_in_the_record_directory_is_garbage_and_never_read() {
        let dir = scratch("special");
        // A link to something endless, and a pipe nobody writes to (a read of it would block for good).
        std::os::unix::fs::symlink("/dev/zero", dir.join(file_name("api", 100))).unwrap();
        let fifo = dir.join(file_name("api", 101));
        assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
        let t0 = Instant::now();
        let found = read_all(&dir, Some("api"));
        assert!(t0.elapsed() < Duration::from_secs(5), "returned at once");
        assert_eq!(found.len(), 2);
        for f in &found {
            assert!(matches!(&f.record, Err(Unusable::Garbled(w)) if w.contains("regular file")), "{f:?}");
        }
        // The sweep removes them like any other garbage (the link, not what it points at).
        rt().block_on(sweep(&settings(&dir, Duration::from_millis(100)), &Fake::default(), None, 900));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        assert!(Path::new("/dev/zero").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ the registry

    #[test]
    fn a_registry_records_the_workers_it_is_told_about_and_only_when_they_change() {
        let dir = scratch("registry");
        let me = std::process::id();
        let id_of_me = super::super::proc_identity(me).expect("this process");
        let mut reg = Registry::new(&dir, "api", Member { pid: me, start: id_of_me.start, label: String::new() }, None);
        reg.write();
        let read = || read_all(&dir, Some("api")).remove(0).record.unwrap();
        assert!(read().workers.is_empty());

        // Two real children: their identities are read.
        let mut a = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let mut b = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        reg.note(&[(b.id(), "2".into()), (a.id(), "1".into())]);
        let rec = read();
        assert_eq!(rec.workers.iter().map(|w| w.pid).collect::<Vec<_>>(), {
            let mut v = vec![a.id(), b.id()];
            v.sort_unstable();
            v
        });
        for w in &rec.workers {
            assert_eq!(super::super::proc_identity(w.pid).map(|i| i.start), Some(w.start));
        }
        let before = std::fs::metadata(&reg.file).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        reg.note(&[(a.id(), "1".into()), (b.id(), "2".into())]);
        assert_eq!(std::fs::metadata(&reg.file).unwrap().modified().unwrap(), before, "unchanged: not rewritten");

        // One goes: the record follows; a pid that cannot be read is not recorded.
        reg.note(&[(a.id(), "1".into()), (0x7fff_fff0, "9".into())]);
        assert_eq!(read().workers.iter().map(|w| w.pid).collect::<Vec<_>>(), [a.id()]);
        // The record is removed when the supervisor is done.
        reg.close();
        assert!(read_all(&dir, Some("api")).is_empty());
        let _ = (a.kill(), b.kill(), a.wait(), b.wait());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --------------------------------------------- real processes, end to end

    /// Start `cmd` in the process group `pgroup` (0: a group of its own) and
    /// have a thread collect it when it exits: a real orphan's parent is init,
    /// which does that at once, and the sweep treats an exited process that
    /// is still in the table as a member of its group.
    fn spawn_reaped(mut cmd: std::process::Command, pgroup: i32) -> u32 {
        use std::os::unix::process::CommandExt;
        let mut child = cmd.process_group(pgroup).stdin(std::process::Stdio::null()).spawn().unwrap();
        let pid = child.id();
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        pid
    }

    /// `exec sleep`, or a shell that ignores SIGTERM.
    fn worker(ignore_term: bool) -> u32 {
        let script = if ignore_term { "trap '' TERM; while :; do sleep 1; done" } else { "exec sleep 300" };
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        spawn_reaped(cmd, 0)
    }

    fn kill_quietly(pid: u32) {
        let _ = crate::sys::kill(pid as i32, libc::SIGKILL);
    }

    fn gone(pid: u32) -> bool {
        super::super::proc_identity(pid).is_none()
    }

    fn wait_until(what: &str, f: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn settings(dir: &Path, grace: Duration) -> Settings<'_> {
        Settings { app: "api", stop_signal: TERM, grace, dir: dir.to_path_buf(), progress: Arc::default() }
    }

    /// A record of this boot (the real one: the sweep ignores any other).
    fn record_now(sup: Member, workers: Vec<Member>) -> Record {
        Record { boot: super::super::boot_id(), ..record(sup, workers) }
    }

    /// Record a supervisor that is gone (a pid no process has) with these workers.
    fn dead_supervisor_record(dir: &Path, workers: &[u32]) {
        let members = workers
            .iter()
            .map(|pid| m(*pid, super::super::proc_identity(*pid).expect("a running worker").start))
            .collect();
        write_atomic(&dir.join(file_name("api", 0x7fff_ff00)), &record_now(m(0x7fff_ff00, 12345), members)).unwrap();
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap()
    }

    fn sweep_now(dir: &Path, grace: Duration, asking: u32) {
        let boot = super::super::boot_id();
        rt().block_on(sweep(&settings(dir, grace), &Os, boot.as_deref(), asking));
    }

    #[test]
    fn real_orphans_are_stopped_with_the_group_they_lead_and_the_record_goes() {
        if super::super::current().name() == "other" {
            return;
        }
        let dir = scratch("real");
        let leader = worker(false);
        // A second process in the worker's group: stopped with it.
        let mut sleep = std::process::Command::new("sleep");
        sleep.arg("300");
        let helper = spawn_reaped(sleep, leader as i32);
        wait_until("the helper in the worker's group", || {
            super::super::proc_identity(helper).is_some_and(|i| i.pgid == leader)
        });
        dead_supervisor_record(&dir, &[leader]);
        sweep_now(&dir, Duration::from_secs(10), std::process::id());
        wait_until("both gone", || gone(leader) && gone(helper));
        assert!(read_all(&dir, Some("api")).is_empty(), "the dealt-with record is removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_worker_that_ignores_sigterm_is_killed_after_the_grace_period() {
        if super::super::current().name() == "other" {
            return;
        }
        let dir = scratch("grace");
        let stubborn = worker(true);
        wait_until("the shell to ignore SIGTERM", || {
            // The trap is set once the shell runs `sleep`: it has a child.
            super::super::current().children(stubborn).is_some_and(|k| !k.is_empty())
        });
        dead_supervisor_record(&dir, &[stubborn]);
        let t0 = Instant::now();
        sweep_now(&dir, Duration::from_millis(600), std::process::id());
        let took = t0.elapsed();
        wait_until("it gone", || gone(stubborn));
        assert!(took >= Duration::from_millis(600), "SIGTERM was ignored, so the grace period was waited: {took:?}");
        assert!(took < Duration::from_secs(8), "and SIGKILL ended it then: {took:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_real_process_whose_pid_was_reused_is_left_running() {
        if super::super::current().name() == "other" {
            return;
        }
        let dir = scratch("reuse");
        let innocent = worker(false);
        wait_until("it to exist", || super::super::proc_identity(innocent).is_some());
        // The record remembers this pid with another start time: the worker that
        // had the pid is gone, and this process got the number later.
        let id = super::super::proc_identity(innocent).unwrap();
        let rec = record_now(m(0x7fff_ff00, 12345), vec![m(innocent, id.start + 1)]);
        write_atomic(&dir.join(file_name("api", 0x7fff_ff00)), &rec).unwrap();
        sweep_now(&dir, Duration::from_millis(100), std::process::id());
        assert_eq!(super::super::proc_identity(innocent), Some(id), "untouched");
        assert!(read_all(&dir, Some("api")).is_empty(), "the record is spent all the same");
        kill_quietly(innocent);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn real_workers_of_a_supervisor_that_is_running_are_left_alone() {
        if super::super::current().name() == "other" {
            return;
        }
        let dir = scratch("live");
        let w = worker(false);
        wait_until("it to exist", || super::super::proc_identity(w).is_some());
        // This test process is the supervisor: alive, with its real start time.
        let me = std::process::id();
        let mine = super::super::proc_identity(me).unwrap();
        let wid = super::super::proc_identity(w).unwrap();
        let rec = record_now(m(me, mine.start), vec![m(w, wid.start)]);
        write_atomic(&dir.join(file_name("api", me)), &rec).unwrap();
        sweep_now(&dir, Duration::from_millis(100), me + 1); // asked by some other process
        assert_eq!(super::super::proc_identity(w), Some(wid), "its supervisor lives: not an orphan");
        assert_eq!(read_all(&dir, Some("api")).len(), 1, "and its record stays");
        kill_quietly(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbled_record_is_removed_and_a_newer_one_is_kept() {
        let dir = scratch("garbled");
        std::fs::write(dir.join(file_name("api", 100)), "{\"v\":1,\"app\":\"api\"").unwrap();
        std::fs::write(dir.join(file_name("api", 200)), "{\"v\":7}").unwrap();
        rt().block_on(sweep(&settings(&dir, Duration::from_millis(100)), &Fake::default(), None, 900));
        let left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert_eq!(left, ["api.200.json"], "garbage gone, the newer Warden's kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_doctor_lists_running_orphans_without_touching_them() {
        let dir = scratch("doctor");
        let fake = Fake::default();
        fake.add(101, id(1001, 1, 101), OnTerm::Exits);
        let write = |app: &str, sup: u32, workers: Vec<Member>| {
            let mut r = record(m(sup, 1000), workers);
            r.app = app.into();
            write_atomic(&dir.join(file_name(app, sup)), &r).unwrap();
        };
        write("api", 100, vec![m(101, 1001)]);
        write("web", 300, vec![m(777, 7)]); // its worker is gone: nothing to report
        let found = running_orphans(&dir, &fake, Some("boot-1"), 900);
        assert_eq!(found.len(), 1);
        assert_eq!((found[0].0.as_str(), found[0].1, found[0].2.len()), ("api", 100, 1));
        assert!(fake.sent().is_empty() && fake.alive(101), "read only");
        assert_eq!(read_all(&dir, None).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

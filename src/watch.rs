//! Opt-in file watching (`[watch]`, PM2's `watch`): when the files of an app
//! change, the supervisor starts a gated rolling restart (docs/watch.md).
//!
//! It polls. A scan walks the watched paths (sorted, so two scans of an
//! unchanged tree agree on which files they saw) and records a fingerprint of
//! every file: inode, size, mtime and ctime. The next scan is compared with
//! it; the first difference names the file. Polling instead of inotify or
//! FSEvents is what works the same on every OS, on network and bind-mounted
//! filesystems and in containers, and it needs no per-directory watches that
//! a big tree would run out of; the price is a scan every `interval_ms` (a
//! `stat` per file), bounded by `max_files` and paced so that a slow scan
//! never takes more than a fifth of a core.
//!
//! A scan is a short blocking job on a worker thread, started again by an
//! async task after a pause; it checks a cancel flag as it goes, so
//! dropping the [`Handle`] ends it within a directory's worth of work. The
//! comparison and the debounce are a plain state machine ([`Watcher::apply`]
//! with the time passed in), tested without a clock or a thread.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Directories deeper than this are not entered (a bind-mount loop that
/// `symlink`s cannot explain; no source tree is this deep).
const MAX_DEPTH: usize = 64;

/// A pause between two scans is at least this many times the last scan's
/// duration, so a big tree on a slow disk costs at most a fifth of a core.
const PACE: u32 = 4;

/// Waiting for a change to settle looks at least this often.
const MIN_SETTLE: Duration = Duration::from_millis(50);

/// Files that have not stopped changing for this long are reported once.
const BUSY_AFTER: Duration = Duration::from_secs(30);

// ------------------------------------------------------------------- globs

/// One pattern of `watch.ignore`: `*` (anything but `/`), `?` (one character
/// but `/`), `**` (anything, `/` too; `**/` also matches no directory at
/// all), `\` to take the next character literally.
///
/// - without a `/`: matched against the name of every file and directory,
///   at any depth (`node_modules`, `*.log`);
/// - with a `/`: matched against the path from the working directory
///   (`src/generated`, `**/*.test.ts`), a leading `./` is optional;
/// - starting with `/`: an absolute path;
/// - ending in `/`: only matches directories.
#[derive(Debug, Clone, PartialEq)]
pub struct Glob {
    name_only: bool,
    absolute: bool,
    dir_only: bool,
    tokens: Vec<Tok>,
    fast: Fast,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Tok {
    Lit(u8),
    /// `?`
    One,
    /// `*`
    Star,
    /// `**`
    Stars,
    /// `**/`: any directories, or none
    Dirs,
}

/// Patterns that need no matching machinery.
#[derive(Debug, Clone, PartialEq)]
enum Fast {
    Exact(Vec<u8>),
    Prefix(Vec<u8>),
    Suffix(Vec<u8>),
    General,
}

impl Glob {
    pub fn new(pattern: &str) -> Result<Glob, String> {
        if pattern.is_empty() {
            return Err("an empty pattern matches nothing".into());
        }
        if pattern.len() > crate::config::Watch::MAX_PATTERN_LEN {
            return Err(format!("longer than {} bytes", crate::config::Watch::MAX_PATTERN_LEN));
        }
        if pattern.contains('\0') {
            return Err("contains a NUL byte".into());
        }
        if pattern.starts_with('!') {
            return Err("negation (`!`) is not supported: list what to skip, and narrow `paths` instead".into());
        }
        let mut p = pattern;
        while let Some(rest) = p.strip_prefix("./") {
            p = rest;
        }
        let absolute = p.starts_with('/');
        let dir_only = p.len() > 1 && p.ends_with('/');
        let p = if dir_only { p.trim_end_matches('/') } else { p };
        if p.is_empty() || p == "." || p == ".." || p == "/" {
            return Err("does not name anything to skip".into());
        }
        let name_only = !absolute && !p.contains('/');
        let bytes = p.as_bytes();
        let mut tokens = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' => {
                    let Some(&c) = bytes.get(i + 1) else { return Err("ends with a lone `\\`".into()) };
                    tokens.push(Tok::Lit(c));
                    i += 2;
                }
                b'?' => {
                    tokens.push(Tok::One);
                    i += 1;
                }
                b'*' => {
                    let mut n = 0;
                    while bytes.get(i + n) == Some(&b'*') {
                        n += 1;
                    }
                    i += n;
                    if n == 1 {
                        tokens.push(Tok::Star);
                    } else if !name_only && bytes.get(i) == Some(&b'/') {
                        tokens.push(Tok::Dirs);
                        i += 1;
                    } else {
                        tokens.push(Tok::Stars);
                    }
                }
                // `a//b` is `a/b`.
                b'/' if tokens.last() == Some(&Tok::Lit(b'/')) => i += 1,
                c => {
                    tokens.push(Tok::Lit(c));
                    i += 1;
                }
            }
        }
        let fast = Self::fast(&tokens, name_only);
        Ok(Glob { name_only, absolute, dir_only, tokens, fast })
    }

    fn fast(tokens: &[Tok], name_only: bool) -> Fast {
        let lits = |t: &[Tok]| -> Option<Vec<u8>> {
            t.iter().map(|t| if let Tok::Lit(c) = t { Some(*c) } else { None }).collect()
        };
        if let Some(l) = lits(tokens) {
            return Fast::Exact(l);
        }
        // A name has no `/`, so `*` and `**` are the same there.
        if name_only {
            if let [Tok::Star | Tok::Stars, rest @ ..] = tokens {
                if let Some(l) = lits(rest) {
                    return Fast::Suffix(l);
                }
            }
            if let [rest @ .., Tok::Star | Tok::Stars] = tokens {
                if let Some(l) = lits(rest) {
                    return Fast::Prefix(l);
                }
            }
        }
        Fast::General
    }

    /// Does the path rule apply to paths (as opposed to names)?
    fn is_path_rule(&self) -> bool {
        !self.name_only
    }

    fn matches(&self, name: &[u8], rel: &[u8], abs: &[u8], is_dir: bool) -> bool {
        if self.dir_only && !is_dir {
            return false;
        }
        let text = if self.name_only {
            name
        } else if self.absolute {
            abs
        } else {
            rel
        };
        match &self.fast {
            Fast::Exact(l) => text == l.as_slice(),
            Fast::Prefix(l) => text.starts_with(l),
            Fast::Suffix(l) => text.ends_with(l),
            Fast::General => tokens_match(&self.tokens, text),
        }
    }
}

/// Wildcard matching in time O(pattern × text) and space O(text): no input
/// makes it exponential.
fn tokens_match(tokens: &[Tok], text: &[u8]) -> bool {
    let n = text.len();
    // dp[j]: the tokens so far match text[..j].
    let mut dp = vec![false; n + 1];
    dp[0] = true;
    for tok in tokens {
        match *tok {
            Tok::Lit(c) => {
                for j in (1..=n).rev() {
                    dp[j] = dp[j - 1] && text[j - 1] == c;
                }
                dp[0] = false;
            }
            Tok::One => {
                for j in (1..=n).rev() {
                    dp[j] = dp[j - 1] && text[j - 1] != b'/';
                }
                dp[0] = false;
            }
            Tok::Star => {
                for j in 1..=n {
                    dp[j] = dp[j] || (dp[j - 1] && text[j - 1] != b'/');
                }
            }
            Tok::Stars => {
                for j in 1..=n {
                    dp[j] = dp[j] || dp[j - 1];
                }
            }
            Tok::Dirs => {
                // (.*/)? : empty, or anything up to a `/`.
                let mut seen = dp[0];
                for j in 1..=n {
                    let old = dp[j];
                    dp[j] = old || (text[j - 1] == b'/' && seen);
                    seen |= old;
                }
            }
        }
    }
    dp[n]
}

/// What is never watched: the patterns of `watch.ignore`, and absolute paths
/// Warden writes to itself (its log files and their rotations, its socket).
#[derive(Debug, Clone, Default)]
pub struct Ignore {
    names: Vec<Glob>,
    paths: Vec<Glob>,
    skip: Vec<Vec<u8>>,
}

impl Ignore {
    pub fn new(patterns: &[String], skip: &[PathBuf]) -> Result<Ignore, String> {
        let mut i = Ignore::default();
        for p in patterns {
            let g = Glob::new(p).map_err(|e| format!("{p:?}: {e}"))?;
            if g.is_path_rule() { i.paths.push(g) } else { i.names.push(g) }
        }
        i.skip = skip.iter().map(|p| p.as_os_str().as_bytes().to_vec()).filter(|p| !p.is_empty()).collect();
        Ok(i)
    }

    /// `abs` is the entry's absolute path, `rel` its path from the working
    /// directory (or, outside it, the absolute path without its first `/`).
    fn is_ignored(&self, name: &[u8], abs: &[u8], rel: &[u8], is_dir: bool) -> bool {
        self.names.iter().any(|g| g.matches(name, rel, abs, is_dir))
            || self.paths.iter().any(|g| g.matches(name, rel, abs, is_dir))
            || self.skip.iter().any(|s| {
                // `out.log` also covers `out.log.1` and `out.log.2026-09-30`, not `out.logger.js`.
                abs.starts_with(s) && matches!(abs.get(s.len()), None | Some(b'.') | Some(b'/'))
            })
    }

    fn needs_paths(&self) -> bool {
        !self.paths.is_empty() || !self.skip.is_empty()
    }
}

// ------------------------------------------------------------------- scan

/// What a file looked like at a scan. Everything but `size` and `mtime`
/// catches a file that was replaced (an editor's atomic save, `rsync`,
/// `cp -p`, `mv` over it) with one of the same size and modification time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Print {
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    link: bool,
}

impl Print {
    fn of(m: &fs::Metadata) -> Print {
        Print {
            ino: m.ino(),
            size: m.len(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
            link: m.file_type().is_symlink(),
        }
    }
}

/// One look at the watched paths.
#[derive(Debug, Default)]
pub struct Scanned {
    files: HashMap<PathBuf, Print>,
    /// Roots that could not be read and directories that could not be
    /// listed: what they held at the last scan is taken to be unchanged.
    lost: Vec<PathBuf>,
    missing_roots: Vec<PathBuf>,
    unreadable: Vec<PathBuf>,
    /// Stopped at `max_files` (or the work limit): the rest is not watched.
    truncated: bool,
    /// Stopped by the cancel flag: what it saw is partial and is not used.
    cancelled: bool,
}

struct Walk<'a> {
    ignore: &'a Ignore,
    cancel: &'a AtomicBool,
    max: usize,
    work_left: usize,
    entries: usize,
    /// `max_files` or the work limit was reached: nothing more is looked at.
    capped: bool,
    out: Scanned,
}

impl Walk<'_> {
    fn done(&self) -> bool {
        self.out.cancelled || self.capped
    }

    fn cap(&mut self) {
        self.capped = true;
        self.out.truncated = true;
    }

    /// `rel`: the path of `dir` from the working directory, with a `/` after it (empty for the directory itself).
    fn dir(&mut self, dir: &Path, rel: &mut Vec<u8>, depth: usize) {
        if self.cancel.load(Ordering::Relaxed) {
            self.out.cancelled = true;
        }
        if self.done() {
            return;
        }
        if depth > MAX_DEPTH {
            // This directory only: the rest of the tree is still watched.
            self.out.truncated = true;
            return;
        }
        let rd = match fs::read_dir(dir) {
            Ok(rd) => rd,
            // Gone since its parent was listed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(_) => {
                self.out.lost.push(dir.to_path_buf());
                self.out.unreadable.push(dir.to_path_buf());
                return;
            }
        };
        let mut items: Vec<(OsString, fs::FileType, fs::DirEntry)> = Vec::new();
        for ent in rd {
            let Ok(ent) = ent else {
                self.out.lost.push(dir.to_path_buf());
                break;
            };
            if self.work_left == 0 {
                self.cap();
                break;
            }
            self.work_left -= 1;
            // A file that vanished since the listing is simply not there.
            let Ok(ft) = ent.file_type() else { continue };
            let name = ent.file_name();
            let is_dir = ft.is_dir();
            if !self.ignored(dir, rel, &name, is_dir) {
                items.push((name, ft, ent));
            }
        }
        // The same files are seen in the same order by every scan, so a
        // tree cut short by `max_files` is cut at the same place.
        items.sort_by(|a, b| a.0.cmp(&b.0));
        for (name, ft, ent) in items {
            if self.done() {
                return;
            }
            self.entries += 1;
            if self.entries > self.max {
                self.cap();
                return;
            }
            let path = dir.join(&name);
            if ft.is_dir() {
                let keep = rel.len();
                rel.extend_from_slice(name.as_bytes());
                rel.push(b'/');
                self.dir(&path, rel, depth + 1);
                rel.truncate(keep);
            } else if ft.is_symlink() {
                // A link to a file stands for that file (its content is what
                // runs); a link to a directory is never followed (loops), and
                // a dangling one is the link itself.
                let meta = fs::metadata(&path).ok().filter(|m| m.is_file()).or_else(|| ent.metadata().ok());
                if let Some(m) = meta {
                    self.out.files.insert(path, Print::of(&m));
                }
            } else if ft.is_file() {
                if let Ok(m) = ent.metadata() {
                    self.out.files.insert(path, Print::of(&m));
                }
            }
            // Sockets, FIFOs and devices are not files of the app.
        }
    }

    fn ignored(&self, dir: &Path, rel: &[u8], name: &OsString, is_dir: bool) -> bool {
        let n = name.as_bytes();
        if !self.ignore.needs_paths() {
            return self.ignore.is_ignored(n, &[], &[], is_dir);
        }
        let abs = dir.join(name);
        let mut r = Vec::with_capacity(rel.len() + n.len());
        r.extend_from_slice(rel);
        r.extend_from_slice(n);
        self.ignore.is_ignored(n, abs.as_os_str().as_bytes(), &r, is_dir)
    }
}

// --------------------------------------------------------------- the watcher

/// What the watcher is told: where, what to skip, how patient to be.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// The working directory: relative `paths` and path patterns start here.
    pub base: PathBuf,
    pub paths: Vec<String>,
    pub ignore: Vec<String>,
    /// Absolute paths never watched (Warden's own log files and socket).
    pub skip: Vec<PathBuf>,
    pub debounce: Duration,
    pub interval: Duration,
    pub max_files: usize,
}

impl Spec {
    pub fn new(w: &crate::config::Watch, base: PathBuf, skip: Vec<PathBuf>) -> Spec {
        Spec {
            base: normalize(&base),
            paths: w.paths.clone(),
            ignore: w.ignore.clone(),
            skip,
            debounce: w.debounce(),
            interval: w.interval(),
            max_files: w.max_files.clamp(1, crate::config::Watch::MAX_FILES),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Created,
    Modified,
    Removed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Created => "created",
            Kind::Modified => "modified",
            Kind::Removed => "removed",
        }
    }
}

/// Files changed and then stayed as they were for the debounce time.
#[derive(Debug, Clone, PartialEq)]
pub struct Change {
    /// The first file (in path order) that differs, from the working directory.
    pub file: String,
    pub kind: Kind,
    /// How many files were created, changed or removed.
    pub count: usize,
}

impl Change {
    /// This change and a later one, as one.
    pub fn merge(mut self, later: Change) -> Change {
        self.count = self.count.saturating_add(later.count);
        self
    }
}

/// Things worth a log line, found while scanning.
#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// More files than `max_files`: the rest is not watched.
    Truncated {
        limit: usize,
    },
    /// A watched path does not exist (or cannot be read); watched again when it does.
    Missing(PathBuf),
    Back(PathBuf),
    /// A directory could not be listed (permissions): its files count as unchanged.
    Unreadable(PathBuf),
    /// Files keep changing: no restart yet.
    Busy {
        secs: u64,
        file: String,
    },
}

#[derive(Debug, Default)]
pub struct Poll {
    /// The first scan: how many files are watched.
    pub baseline: Option<usize>,
    pub change: Option<Change>,
    pub notes: Vec<Note>,
}

struct Pending {
    change: Change,
    /// The last scan that saw a difference.
    last: Instant,
    since: Instant,
    warned: bool,
}

pub struct Watcher {
    spec: Spec,
    ignore: Ignore,
    /// Each root with its path from the working directory (and a `/`).
    roots: Vec<(PathBuf, Vec<u8>)>,
    snap: Option<HashMap<PathBuf, Print>>,
    pending: Option<Pending>,
    missing: HashSet<PathBuf>,
    unreadable: HashSet<PathBuf>,
    truncated: bool,
}

impl Watcher {
    pub fn new(spec: &Spec) -> Result<Watcher, String> {
        let ignore = Ignore::new(&spec.ignore, &spec.skip)?;
        let mut roots: Vec<PathBuf> = spec.paths.iter().map(|p| normalize(&spec.base.join(p))).collect();
        roots.sort();
        roots.dedup();
        // `src` inside `.`: scanned once, as part of `.`.
        let all = roots.clone();
        roots.retain(|r| !all.iter().any(|o| o != r && r.starts_with(o)));
        let roots = roots
            .into_iter()
            .map(|r| {
                let mut rel = rel_of(&spec.base, &r).into_bytes();
                if !rel.is_empty() {
                    rel.push(b'/');
                }
                (r, rel)
            })
            .collect();
        Ok(Watcher {
            spec: spec.clone(),
            ignore,
            roots,
            snap: None,
            pending: None,
            missing: HashSet::new(),
            unreadable: HashSet::new(),
            truncated: false,
        })
    }

    /// One look at the files. Runs on a blocking thread; `cancel` ends it early.
    pub fn scan(&self, cancel: &AtomicBool) -> Scanned {
        // Work limit: ignored entries are listed too, so a directory of a
        // million ignored files is not free; it is bounded all the same.
        let work = self.spec.max_files.saturating_mul(16).saturating_add(100_000);
        let mut w = Walk {
            ignore: &self.ignore,
            cancel,
            max: self.spec.max_files,
            work_left: work,
            entries: 0,
            capped: false,
            out: Scanned::default(),
        };
        for (root, rel) in &self.roots {
            if w.done() {
                break;
            }
            // The root itself is followed if it is a symlink (a `current` release link).
            match fs::metadata(root) {
                Ok(m) if m.is_dir() => {
                    let mut rel = rel.clone();
                    w.dir(root, &mut rel, 0);
                }
                Ok(m) if m.is_file() => {
                    w.entries += 1;
                    w.out.files.insert(root.clone(), Print::of(&m));
                }
                Ok(_) => {}
                Err(_) => {
                    w.out.lost.push(root.clone());
                    w.out.missing_roots.push(root.clone());
                }
            }
        }
        w.out
    }

    /// Take a scan into account at time `now`: report a change once the
    /// files have been the same for the debounce time.
    pub fn apply(&mut self, mut s: Scanned, now: Instant) -> Poll {
        let mut poll = Poll::default();
        if s.cancelled {
            // Partial: comparing it would report everything it did not reach as removed.
            return poll;
        }
        for r in &s.missing_roots {
            if self.missing.insert(r.clone()) {
                poll.notes.push(Note::Missing(r.clone()));
            }
        }
        let back: Vec<PathBuf> = self.missing.iter().filter(|r| !s.missing_roots.contains(r)).cloned().collect();
        for r in back {
            self.missing.remove(&r);
            poll.notes.push(Note::Back(r));
        }
        for d in &s.unreadable {
            if self.unreadable.insert(d.clone()) {
                poll.notes.push(Note::Unreadable(d.clone()));
            }
        }
        if s.truncated && !self.truncated {
            poll.notes.push(Note::Truncated { limit: self.spec.max_files });
        }
        self.truncated = s.truncated;

        let Some(old) = self.snap.take() else {
            poll.baseline = Some(s.files.len());
            self.snap = Some(s.files);
            return poll;
        };
        // What sat in a directory (or under a root) that could not be read
        // now is what sat there before: not deleted, just not seen.
        if !s.lost.is_empty() {
            let lost: HashSet<&Path> = s.lost.iter().map(PathBuf::as_path).collect();
            for (p, print) in &old {
                if !s.files.contains_key(p) && p.ancestors().any(|a| lost.contains(a)) {
                    s.files.insert(p.clone(), *print);
                }
            }
        }
        let (count, first) = diff(&old, &s.files);
        self.snap = Some(s.files);
        if let Some((path, kind)) = first {
            let file = rel_of(&self.spec.base, &path);
            match &mut self.pending {
                Some(p) => {
                    p.change.count = p.change.count.saturating_add(count);
                    p.last = now;
                }
                None => {
                    self.pending =
                        Some(Pending { change: Change { file, kind, count }, last: now, since: now, warned: false });
                }
            }
        }
        let settled =
            self.pending.as_ref().is_some_and(|p| now.saturating_duration_since(p.last) >= self.spec.debounce);
        if settled {
            poll.change = self.pending.take().map(|p| p.change);
        } else if let Some(p) = &mut self.pending {
            if !p.warned && now.saturating_duration_since(p.since) >= BUSY_AFTER {
                p.warned = true;
                poll.notes.push(Note::Busy { secs: BUSY_AFTER.as_secs(), file: p.change.file.clone() });
            }
        }
        poll
    }

    /// How long until the next scan: while a change settles, the debounce
    /// time (or the interval, if that is shorter); else the interval.
    pub fn delay(&self) -> Duration {
        if self.pending.is_some() {
            self.spec.interval.min(self.spec.debounce.max(MIN_SETTLE))
        } else {
            self.spec.interval
        }
    }
}

/// How many files differ, and the first of them in path order.
fn diff(old: &HashMap<PathBuf, Print>, new: &HashMap<PathBuf, Print>) -> (usize, Option<(PathBuf, Kind)>) {
    let mut count = 0usize;
    let mut first: Option<(&PathBuf, Kind)> = None;
    fn earlier<'a>(first: &mut Option<(&'a PathBuf, Kind)>, p: &'a PathBuf, k: Kind) {
        if first.as_ref().is_none_or(|(f, _)| p < *f) {
            *first = Some((p, k));
        }
    }
    for (p, print) in new {
        match old.get(p) {
            None => {
                count += 1;
                earlier(&mut first, p, Kind::Created);
            }
            Some(o) if o != print => {
                count += 1;
                earlier(&mut first, p, Kind::Modified);
            }
            Some(_) => {}
        }
    }
    for p in old.keys() {
        if !new.contains_key(p) {
            count += 1;
            earlier(&mut first, p, Kind::Removed);
        }
    }
    (count, first.map(|(p, k)| (p.clone(), k)))
}

/// `path` without `.` and, where it can be told lexically, `..`.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            c => out.push(c.as_os_str()),
        }
    }
    out
}

/// `path` from `base` as a string with `/`; outside it, the absolute path.
/// Entries of a root outside the working directory are matched without their first `/`.
fn rel_of(base: &Path, path: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

// ---------------------------------------------------------------- throttle

/// A floor under the time between two watch restarts. The debounce already
/// waits for a build to finish; this is for what it cannot see: a worker
/// that writes into a watched directory every time it starts would restart
/// itself forever. The gap starts at `GAP` and doubles for every restart in a
/// row that began less than `RESET` after the one before, up to `GAP_MAX`;
/// a restart after a longer calm starts over.
#[derive(Debug, Default)]
pub struct Throttle {
    last: Option<Instant>,
    streak: u32,
}

impl Throttle {
    pub const GAP: Duration = Duration::from_secs(2);
    pub const GAP_MAX: Duration = Duration::from_secs(30);
    pub const RESET: Duration = Duration::from_secs(60);

    fn gap(&self) -> Duration {
        Self::GAP.saturating_mul(1u32 << self.streak.min(5)).min(Self::GAP_MAX)
    }

    /// How long a restart has to wait, if it has to.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        let ready = self.last? + self.gap();
        ready.checked_duration_since(now).filter(|d| !d.is_zero())
    }

    /// A watch restart starts at `now`.
    pub fn started(&mut self, now: Instant) {
        self.streak = match self.last {
            Some(l) if now.saturating_duration_since(l) < Self::RESET => self.streak.saturating_add(1),
            _ => 0,
        };
        self.last = Some(now);
    }

    /// Restarts in a row, each shortly after the one before (0 for the first).
    pub fn streak(&self) -> u32 {
        self.streak
    }
}

// -------------------------------------------------------------------- task

/// A running watcher. Dropping it stops it: the cancel flag ends a scan in
/// progress and the task is aborted, so nothing of it outlives its owner.
pub struct Handle {
    cancel: Arc<AtomicBool>,
    task: tokio::task::AbortHandle,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

/// Start watching. `send` hands a change to whoever restarts the app; it
/// returns false when nobody listens any more, which ends the task. Must be
/// called inside the supervisor's `LocalSet`.
pub fn spawn(spec: Spec, send: impl Fn(Change) -> bool + 'static) -> Result<Handle, String> {
    let watcher = Watcher::new(&spec)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let task = tokio::task::spawn_local(async move {
        let mut watcher = Some(watcher);
        let mut slow_logged = false;
        loop {
            let Some(w) = watcher.take() else { return };
            let cancel = flag.clone();
            let started = Instant::now();
            let job = tokio::task::spawn_blocking(move || {
                let scanned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| w.scan(&cancel)));
                (w, scanned)
            });
            let (mut w, scanned) = match job.await {
                Ok((w, Ok(s))) => (w, s),
                Ok((_, Err(p))) => {
                    crate::error!(
                        "file watching stopped: the scan panicked",
                        panic = crate::guard::panic_message(&*p),
                        hint = "this is a Warden bug: please report it; `warden reload` starts watching again",
                    );
                    return;
                }
                // The runtime is shutting down.
                Err(_) => return,
            };
            let took = started.elapsed();
            let poll = w.apply(scanned, Instant::now());
            log_notes(&poll, &w.spec, took, &mut slow_logged);
            if let Some(c) = poll.change {
                if !send(c) {
                    return;
                }
            }
            let wait = w.delay().max(took.saturating_mul(PACE));
            watcher = Some(w);
            tokio::time::sleep(wait).await;
        }
    });
    Ok(Handle { cancel, task: task.abort_handle() })
}

fn log_notes(poll: &Poll, spec: &Spec, took: Duration, slow_logged: &mut bool) {
    if let Some(files) = poll.baseline {
        crate::info!(
            "file watching started",
            files = files,
            first_scan_ms = took.as_millis(),
            interval_ms = spec.interval.as_millis(),
        );
    }
    // A scan that takes a good part of the interval is the tree's size talking.
    if !*slow_logged && took.saturating_mul(PACE) > spec.interval && poll.baseline.is_none() {
        *slow_logged = true;
        crate::warn!(
            "a scan of the watched files takes long: changes are noticed later than interval_ms",
            took_ms = took.as_millis(),
            interval_ms = spec.interval.as_millis(),
            hint = "scans are paced to use at most a fifth of a core; ignore big directories ([watch] ignore) or \
                    watch fewer paths",
        );
    }
    for n in &poll.notes {
        match n {
            Note::Truncated { limit } => crate::warn!(
                "the watched tree is bigger than watch.max_files (or deeper than 64 levels): the rest is not watched",
                max_files = limit,
                hint = "add build output, caches and data directories to [watch] ignore, or raise max_files (at most \
                        100000; every file costs memory and a stat per scan)",
            ),
            Note::Missing(p) => crate::warn!(
                "a watched path cannot be read; its files count as unchanged until it can",
                path = p.display(),
                hint = "create it, or fix [watch] paths",
            ),
            Note::Back(p) => crate::info!("a watched path can be read again", path = p.display()),
            Note::Unreadable(d) => crate::warn!(
                "a watched directory cannot be listed; its files count as unchanged",
                dir = d.display(),
                hint = "fix its permissions or add it to [watch] ignore",
            ),
            Note::Busy { secs, file } => crate::warn!(
                "watched files keep changing: waiting for them to settle before restarting",
                first_file = file,
                changing_for_s = secs,
                hint = "a build still running is fine; if something writes into a watched directory all the time, add \
                        it to [watch] ignore",
            ),
        }
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// A scratch directory under the system temp dir, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(tag: &str) -> Dir {
            static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::SeqCst);
            let p = std::env::temp_dir().join(format!("warden-watch-{tag}-{}-{n}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Dir(fs::canonicalize(&p).unwrap())
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        fn write(&self, rel: &str, text: &str) {
            let p = self.path(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, text).unwrap();
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn spec(dir: &Dir) -> Spec {
        Spec {
            base: dir.0.clone(),
            paths: vec![".".into()],
            ignore: ["node_modules", ".git", "*.log"].iter().map(|s| s.to_string()).collect(),
            skip: Vec::new(),
            debounce: Duration::from_millis(500),
            interval: Duration::from_millis(1000),
            max_files: 1000,
        }
    }

    static NEVER: AtomicBool = AtomicBool::new(false);

    /// A scan and its report, at a given moment.
    fn look(w: &mut Watcher, at: Instant) -> Poll {
        let s = w.scan(&NEVER);
        w.apply(s, at)
    }

    fn names(w: &Watcher) -> Vec<String> {
        let mut v: Vec<String> = w.snap.as_ref().unwrap().keys().map(|p| rel_of(&w.spec.base, p)).collect();
        v.sort();
        v
    }

    fn ignored(pattern: &str, name: &str, rel: &str, is_dir: bool) -> bool {
        let g = Glob::new(pattern).unwrap();
        g.matches(name.as_bytes(), rel.as_bytes(), format!("/work/{rel}").as_bytes(), is_dir)
    }

    #[test]
    fn globs_on_names() {
        assert!(ignored("node_modules", "node_modules", "a/b/node_modules", true));
        assert!(!ignored("node_modules", "node_modules2", "node_modules2", true));
        assert!(ignored("*.log", "app.log", "x/app.log", false));
        assert!(!ignored("*.log", "app.log.1", "app.log.1", false));
        assert!(ignored("*~", "main.ts~", "main.ts~", false));
        assert!(ignored(".#*", ".#main.ts", ".#main.ts", false));
        assert!(ignored("?.tmp", "a.tmp", "a.tmp", false) && !ignored("?.tmp", "ab.tmp", "ab.tmp", false));
        assert!(ignored("a*b*c", "aXXbYYc", "aXXbYYc", false) && !ignored("a*b*c", "aXXbYY", "aXXbYY", false));
        // A trailing slash: directories only.
        assert!(ignored("build/", "build", "build", true) && !ignored("build/", "build", "build", false));
        // `./` is not part of the pattern; escapes take the character literally.
        assert!(ignored("./dist", "dist", "dist", true));
        assert!(ignored(r"a\*b", "a*b", "a*b", false) && !ignored(r"a\*b", "aXb", "aXb", false));
        // A name pattern never looks at the path above it.
        assert!(!ignored("src", "main.ts", "src/main.ts", false));
    }

    #[test]
    fn globs_on_paths() {
        assert!(ignored("src/generated", "generated", "src/generated", true));
        assert!(!ignored("src/generated", "generated", "lib/src/generated", true), "anchored at the working dir");
        assert!(ignored("src/*.js", "a.js", "src/a.js", false) && !ignored("src/*.js", "a.js", "src/x/a.js", false));
        assert!(ignored("src/**", "a.js", "src/x/y/a.js", false));
        assert!(ignored("**/*.test.ts", "a.test.ts", "a.test.ts", false), "`**/` also means no directory");
        assert!(ignored("**/*.test.ts", "a.test.ts", "src/deep/a.test.ts", false));
        assert!(!ignored("**/*.test.ts", "a.ts", "src/a.ts", false));
        assert!(ignored("a/**/b", "b", "a/b", false) && ignored("a/**/b", "b", "a/x/y/b", false));
        assert!(!ignored("a/**/b", "b", "ab", false) && !ignored("a/**/b", "b", "xa/b", false));
        assert!(ignored("src/gen/", "gen", "src/gen", true) && !ignored("src/gen/", "gen", "src/gen", false));
        // `a//b` is `a/b`; absolute patterns see the absolute path.
        assert!(ignored("a//b", "b", "a/b", false));
        assert!(ignored("/work/out", "out", "out", true) && !ignored("/other/out", "out", "out", true));
        // Matching is polynomial: this would not finish with backtracking.
        let long = "a".repeat(3000);
        assert!(!ignored("*a*a*a*a*a*a*a*a*a*b", &long, &long, false));
    }

    #[test]
    fn bad_patterns_are_named() {
        for (p, why) in [
            ("", "empty"),
            ("!keep", "negation"),
            ("a\0b", "NUL"),
            (".", "name anything"),
            ("/", "name anything"),
            ("a\\", "lone"),
        ] {
            let e = Glob::new(p).unwrap_err();
            assert!(e.contains(why), "{p:?}: {e}");
        }
        assert!(Glob::new(&"x".repeat(513)).is_err());
        assert!(Glob::new(&"x".repeat(512)).is_ok());
    }

    #[test]
    fn scan_skips_ignored_names_and_directories_at_any_depth() {
        let d = Dir::new("ignore");
        d.write("src/app.ts", "1");
        d.write("src/deep/er/app.ts", "1");
        d.write("node_modules/dep/index.js", "1");
        d.write("src/node_modules/dep/index.js", "1");
        d.write(".git/HEAD", "1");
        d.write("run.log", "1");
        d.write("src/x/debug.log", "1");
        d.write("package.json", "{}");
        let mut w = Watcher::new(&spec(&d)).unwrap();
        let p = look(&mut w, Instant::now());
        assert_eq!(p.baseline, Some(3));
        assert_eq!(names(&w), ["package.json", "src/app.ts", "src/deep/er/app.ts"]);
    }

    #[test]
    fn path_rules_and_skipped_paths() {
        let d = Dir::new("pathrules");
        d.write("src/a.ts", "1");
        d.write("src/a.test.ts", "1");
        d.write("src/gen/x.ts", "1");
        d.write("lib/gen/y.ts", "1");
        d.write("out.txt", "1");
        d.write("out.txt.1", "1");
        d.write("out.txt.old/z", "1");
        d.write("out.txtx", "1");
        let mut s = spec(&d);
        s.ignore = vec!["src/gen".into(), "**/*.test.ts".into()];
        s.skip = vec![d.path("out.txt")];
        let mut w = Watcher::new(&s).unwrap();
        look(&mut w, Instant::now());
        assert_eq!(names(&w), ["lib/gen/y.ts", "out.txtx", "src/a.ts"]);
    }

    #[test]
    fn created_modified_removed_and_renamed_files() {
        let d = Dir::new("diff");
        d.write("a.ts", "one");
        d.write("b.ts", "two");
        d.write("c.ts", "three");
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        assert_eq!(look(&mut w, t).baseline, Some(3));
        assert_eq!(look(&mut w, t).change, None, "nothing changed");

        d.write("d.ts", "new");
        let c = look(&mut w, t).change.unwrap();
        assert_eq!((c.file.as_str(), c.kind, c.count), ("d.ts", Kind::Created, 1));

        fs::remove_file(d.path("d.ts")).unwrap();
        let c = look(&mut w, t).change.unwrap();
        assert_eq!((c.file.as_str(), c.kind, c.count), ("d.ts", Kind::Removed, 1));

        d.write("a.ts", "one, longer");
        let c = look(&mut w, t).change.unwrap();
        assert_eq!((c.file.as_str(), c.kind), ("a.ts", Kind::Modified));

        // A rename is a removal and a creation; the first name in path order is reported.
        fs::rename(d.path("b.ts"), d.path("z.ts")).unwrap();
        let c = look(&mut w, t).change.unwrap();
        assert_eq!((c.file.as_str(), c.kind, c.count), ("b.ts", Kind::Removed, 2));

        // A new directory with a file, then its removal.
        d.write("sub/new.ts", "x");
        assert_eq!(look(&mut w, t).change.unwrap().file, "sub/new.ts");
        fs::remove_dir_all(d.path("sub")).unwrap();
        assert_eq!(look(&mut w, t).change.unwrap().kind, Kind::Removed);
        assert_eq!(look(&mut w, t).change, None);
    }

    fn set_mtime(p: &Path, t: std::time::SystemTime) {
        fs::OpenOptions::new().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn a_touch_is_a_change_and_so_is_a_same_size_replacement() {
        let d = Dir::new("touch");
        d.write("a.ts", "same");
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        // mtime only: same size, same content.
        set_mtime(&d.path("a.ts"), std::time::SystemTime::now() + Duration::from_secs(5));
        assert_eq!(look(&mut w, t).change.map(|c| c.kind), Some(Kind::Modified));
        assert_eq!(look(&mut w, t).change, None);
        // Replaced by a file of the same size and the same mtime (rsync -t, cp -p, an atomic save).
        let before = fs::metadata(d.path("a.ts")).unwrap().modified().unwrap();
        d.write("a.new", "SAME");
        set_mtime(&d.path("a.new"), before);
        // Created by this write; take it into the baseline, then move it over a.ts.
        look(&mut w, t);
        fs::rename(d.path("a.new"), d.path("a.ts")).unwrap();
        let c = look(&mut w, t).change.unwrap();
        assert_eq!(c.count, 2, "a.new is gone and a.ts is another file: {c:?}");
        // Permissions change the ctime only.
        std::thread::sleep(Duration::from_millis(20));
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(d.path("a.ts"), fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some("a.ts".to_string()));
    }

    #[test]
    fn debounce_waits_for_the_files_to_stop_changing() {
        let d = Dir::new("debounce");
        d.write("a.ts", "1");
        let mut w = Watcher::new(&spec(&d)).unwrap(); // debounce 500 ms
        let t0 = Instant::now();
        look(&mut w, t0);
        assert_eq!(w.delay(), Duration::from_millis(1000));

        // A build: files keep changing, scan after scan.
        d.write("a.ts", "22");
        let p = look(&mut w, t0 + Duration::from_millis(1000));
        assert!(p.change.is_none(), "just changed: not yet");
        assert_eq!(w.delay(), Duration::from_millis(500), "looks again after the debounce time");
        d.write("b.ts", "built");
        assert!(look(&mut w, t0 + Duration::from_millis(1500)).change.is_none());
        d.write("a.ts", "333");
        assert!(look(&mut w, t0 + Duration::from_millis(2000)).change.is_none());
        // Quiet for less than the debounce time: still waiting.
        assert!(look(&mut w, t0 + Duration::from_millis(2300)).change.is_none());
        // Quiet for the debounce time: one change for the whole build.
        let c = look(&mut w, t0 + Duration::from_millis(2500)).change.unwrap();
        assert_eq!(c.count, 3, "a.ts, b.ts, a.ts again: {c:?}");
        assert_eq!(c.file, "a.ts");
        assert_eq!(w.delay(), Duration::from_millis(1000));
        assert!(look(&mut w, t0 + Duration::from_millis(9000)).change.is_none(), "reported once");
    }

    #[test]
    fn continuous_changes_never_fire_and_are_reported_once() {
        let d = Dir::new("busy");
        d.write("a.ts", "0");
        let mut w = Watcher::new(&spec(&d)).unwrap();
        let t0 = Instant::now();
        look(&mut w, t0);
        let mut busy = 0;
        for i in 1..=80u64 {
            d.write("a.ts", &"x".repeat(i as usize + 1));
            let p = look(&mut w, t0 + Duration::from_millis(400 * i));
            assert!(p.change.is_none(), "still changing at step {i}");
            busy += p.notes.iter().filter(|n| matches!(n, Note::Busy { .. })).count();
        }
        assert_eq!(busy, 1, "reported once after 30 s");
    }

    #[test]
    fn debounce_zero_reports_at_the_first_scan() {
        let d = Dir::new("nodebounce");
        d.write("a.ts", "1");
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        d.write("a.ts", "22");
        assert!(look(&mut w, t).change.is_some());
        assert_eq!(w.delay(), Duration::from_millis(1000));
    }

    #[test]
    fn the_file_cap_cuts_the_same_files_every_time() {
        let d = Dir::new("cap");
        for i in 0..40 {
            d.write(&format!("d{}/f{i:02}.ts", i % 4), "x");
        }
        let mut s = spec(&d);
        s.max_files = 10;
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        let p = look(&mut w, t);
        assert_eq!(p.notes, vec![Note::Truncated { limit: 10 }]);
        // Directories count too: d0 (1) + its files, in name order, until 10.
        assert_eq!(p.baseline, Some(9));
        let first = names(&w);
        assert!(first.iter().all(|n| n.starts_with("d0/")), "{first:?}");
        // Scan after scan: no phantom changes, and the warning is not repeated.
        for _ in 0..5 {
            let p = look(&mut w, t);
            assert!(p.change.is_none() && p.notes.is_empty(), "{p:?}");
        }
        assert_eq!(names(&w), first);
        // A file inside the watched part is seen; one beyond the cap is not.
        d.write("d3/zzz.ts", "x");
        assert!(look(&mut w, t).change.is_none(), "beyond the cap");
        d.write("d0/f00.ts", "changed");
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some("d0/f00.ts".into()));
    }

    #[test]
    fn symlinks_are_not_followed_into_loops() {
        let d = Dir::new("loop");
        d.write("src/a.ts", "1");
        std::os::unix::fs::symlink(&d.0, d.path("src/up")).unwrap(); // a loop
        std::os::unix::fs::symlink("src", d.path("alias")).unwrap(); // another view of src
        std::os::unix::fs::symlink("nowhere", d.path("dangling")).unwrap();
        std::os::unix::fs::symlink("src/a.ts", d.path("a-link.ts")).unwrap();
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        let p = look(&mut w, t);
        assert_eq!(names(&w), ["a-link.ts", "alias", "dangling", "src/a.ts", "src/up"], "{p:?}");
        // The content behind a link to a file counts as the link's.
        d.write("src/a.ts", "changed, longer");
        let c = look(&mut w, t).change.unwrap();
        assert_eq!(c.count, 2, "a.ts and the link to it: {c:?}");
        // Re-pointing a link to a directory is a change.
        fs::remove_file(d.path("alias")).unwrap();
        std::os::unix::fs::symlink("src/up", d.path("alias")).unwrap();
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some("alias".to_string()));
    }

    #[test]
    fn a_root_that_is_a_symlink_is_followed_and_a_release_flip_is_seen() {
        let d = Dir::new("current");
        d.write("releases/1/app.js", "v1");
        d.write("releases/2/app.js", "v2");
        std::os::unix::fs::symlink("releases/1", d.path("current")).unwrap();
        let mut s = spec(&d);
        s.paths = vec!["current".into()];
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        assert_eq!(look(&mut w, t).baseline, Some(1));
        fs::remove_file(d.path("current")).unwrap();
        std::os::unix::fs::symlink("releases/2", d.path("current")).unwrap();
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some("current/app.js".to_string()));
    }

    #[test]
    fn a_missing_root_is_not_a_mass_deletion() {
        let d = Dir::new("missing");
        d.write("app/a.ts", "1");
        d.write("app/b.ts", "1");
        let mut s = spec(&d);
        s.paths = vec!["app".into(), "extra".into()];
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        let p = look(&mut w, t);
        assert_eq!((p.baseline, p.notes), (Some(2), vec![Note::Missing(d.path("extra"))]));
        assert!(look(&mut w, t).notes.is_empty(), "said once");
        // The whole directory goes (rm -rf, a deploy in progress): not a change, not a crash.
        fs::remove_dir_all(d.path("app")).unwrap();
        let p = look(&mut w, t);
        assert!(p.change.is_none(), "{p:?}");
        assert_eq!(p.notes, vec![Note::Missing(d.path("app"))]);
        assert_eq!(w.snap.as_ref().unwrap().len(), 2, "what was there is remembered");
        // It comes back with other content: that is a change.
        d.write("app/a.ts", "1 and more");
        d.write("app/b.ts", "1");
        let p = look(&mut w, t);
        assert!(p.notes.contains(&Note::Back(d.path("app"))), "{p:?}");
        assert_eq!(p.change.map(|c| c.file), Some("app/a.ts".to_string()));
        // `extra` appears later.
        d.write("extra/new.ts", "1");
        let p = look(&mut w, t);
        assert_eq!(p.change.map(|c| (c.file, c.kind)), Some(("extra/new.ts".to_string(), Kind::Created)));
    }

    #[test]
    fn an_unreadable_directory_keeps_what_it_held() {
        // Built by hand: tests may run as root, which can read everything.
        let d = Dir::new("unreadable");
        d.write("a/x.ts", "1");
        d.write("b/y.ts", "1");
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        let mut scanned = w.scan(&NEVER);
        scanned.files.retain(|p, _| !p.starts_with(d.path("a")));
        scanned.lost.push(d.path("a"));
        scanned.unreadable.push(d.path("a"));
        let p = w.apply(scanned, t);
        assert!(p.change.is_none(), "files in a directory that cannot be listed are not deleted: {p:?}");
        assert_eq!(p.notes, vec![Note::Unreadable(d.path("a"))]);
        assert_eq!(names(&w), ["a/x.ts", "b/y.ts"]);
        // Said once.
        let mut again = w.scan(&NEVER);
        again.files.retain(|p, _| !p.starts_with(d.path("a")));
        again.lost.push(d.path("a"));
        again.unreadable.push(d.path("a"));
        assert!(w.apply(again, t).notes.is_empty());
        if crate::sys::euid() != 0 {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(d.path("a"), fs::Permissions::from_mode(0o000)).unwrap();
            let p = look(&mut w, t);
            fs::set_permissions(d.path("a"), fs::Permissions::from_mode(0o755)).unwrap();
            assert!(p.change.is_none(), "{p:?}");
        }
    }

    #[test]
    fn a_cancelled_scan_is_dropped_whole() {
        let d = Dir::new("cancel");
        for i in 0..20 {
            d.write(&format!("d{i}/f.ts"), "x");
        }
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        let flag = AtomicBool::new(true);
        let scanned = w.scan(&flag);
        assert!(scanned.cancelled && scanned.files.is_empty(), "stops before the first directory");
        let p = w.apply(scanned, t);
        assert!(p.change.is_none(), "everything it did not see is not 'removed'");
        assert_eq!(w.snap.as_ref().unwrap().len(), 20);
    }

    #[test]
    fn paths_can_be_files_and_overlaps_are_scanned_once() {
        let d = Dir::new("roots");
        d.write("src/a.ts", "1");
        d.write("package.json", "{}");
        d.write("other/b.ts", "1");
        let mut s = spec(&d);
        s.paths = vec!["src".into(), "package.json".into(), "./src/".into(), "src/a.ts".into(), "src".into()];
        s.debounce = Duration::ZERO;
        let w = Watcher::new(&s).unwrap();
        assert_eq!(w.roots.len(), 2, "{:?}", w.roots);
        let mut w = w;
        let t = Instant::now();
        assert_eq!(look(&mut w, t).baseline, Some(2));
        d.write("package.json", "{ }");
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some("package.json".into()));
        d.write("other/b.ts", "ignored: not watched");
        assert!(look(&mut w, t).change.is_none());
    }

    #[test]
    fn paths_outside_the_working_directory_use_their_absolute_names() {
        let d = Dir::new("outside");
        let other = Dir::new("outside-other");
        d.write("a.ts", "1");
        other.write("shared/lib.ts", "1");
        let mut s = spec(&d);
        s.paths = vec![".".into(), other.0.display().to_string()];
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        other.write("shared/lib.ts", "12");
        assert_eq!(look(&mut w, t).change.map(|c| c.file), Some(other.path("shared/lib.ts").display().to_string()));
    }

    #[test]
    fn deep_trees_stop_at_the_depth_limit() {
        let d = Dir::new("deep");
        let mut rel = String::new();
        for i in 0..(MAX_DEPTH + 6) {
            rel.push_str(&format!("d{i}/"));
        }
        d.write(&format!("{rel}f.ts"), "x");
        d.write("top.ts", "x");
        let mut w = Watcher::new(&spec(&d)).unwrap();
        let p = look(&mut w, Instant::now());
        assert_eq!(p.notes, vec![Note::Truncated { limit: 1000 }]);
        assert_eq!(names(&w), ["top.ts"]);
    }

    #[test]
    fn many_ignored_files_are_bounded_work() {
        let d = Dir::new("flood");
        for i in 0..300 {
            d.write(&format!("logs/{i}.log"), "x");
        }
        d.write("a.ts", "1");
        let mut s = spec(&d);
        s.max_files = 5; // work limit 16 × 5 + 100_000: this stays below it
        let mut w = Watcher::new(&s).unwrap();
        let p = look(&mut w, Instant::now());
        assert_eq!(names(&w), ["a.ts"], "{p:?}");
        assert_eq!(p.baseline, Some(1));
    }

    #[test]
    fn throttle_spaces_restarts_that_come_in_a_row() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut th = Throttle::default();
        assert_eq!(th.wait(at(0)), None, "the first is never held");
        th.started(at(0));
        assert_eq!((th.streak(), th.wait(at(0))), (0, Some(Duration::from_secs(2))));
        assert_eq!(th.wait(at(1)), Some(Duration::from_secs(1)));
        assert_eq!(th.wait(at(2)), None);
        // A second one soon after: gaps of 4, 8, 16, then 30 s at most.
        th.started(at(3));
        assert_eq!((th.streak(), th.wait(at(3))), (1, Some(Duration::from_secs(4))));
        th.started(at(8));
        assert_eq!((th.streak(), th.wait(at(8))), (2, Some(Duration::from_secs(8))));
        th.started(at(17));
        th.started(at(34));
        th.started(at(65));
        assert_eq!(th.streak(), 5);
        assert_eq!(th.wait(at(65)), Some(Duration::from_secs(30)));
        th.started(at(96));
        assert_eq!(th.wait(at(96)), Some(Duration::from_secs(30)), "never more than 30 s");
        // A calm of a minute and the next one is a first.
        th.started(at(96 + 60));
        assert_eq!((th.streak(), th.wait(at(96 + 60))), (0, Some(Duration::from_secs(2))));
    }

    #[test]
    fn spec_from_config_clamps_and_normalizes() {
        let w =
            crate::config::Watch { interval_ms: 1, max_files: usize::MAX, debounce_ms: u64::MAX, ..Default::default() };
        let s = Spec::new(&w, PathBuf::from("/srv/./app/../app/"), vec![]);
        assert_eq!(s.base, PathBuf::from("/srv/app"));
        assert_eq!(s.interval, Duration::from_millis(100));
        assert_eq!(s.max_files, 100_000);
        assert_eq!(s.debounce, Duration::from_millis(600_000));
    }

    /// The task end to end: a change is reported once, after the debounce
    /// time, and dropping the handle stops the scans.
    #[tokio::test(flavor = "current_thread")]
    async fn the_task_reports_changes_and_stops_when_dropped() {
        let d = Dir::new("task");
        d.write("a.ts", "1");
        let mut s = spec(&d);
        s.interval = Duration::from_millis(100);
        s.debounce = Duration::from_millis(200);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let h = spawn(s, move |c| tx.send(c).is_ok()).unwrap();
                tokio::time::sleep(Duration::from_millis(400)).await;
                assert!(rx.try_recv().is_err(), "nothing changed yet");
                let t = Instant::now();
                d.write("b.ts", "new");
                let c = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap().unwrap();
                assert_eq!((c.file.as_str(), c.kind), ("b.ts", Kind::Created));
                assert!(t.elapsed() >= Duration::from_millis(200), "waited for the debounce time: {:?}", t.elapsed());
                // Reported once.
                tokio::time::sleep(Duration::from_millis(500)).await;
                assert!(rx.try_recv().is_err());
                // Dropped: no more reports, however the files change.
                drop(h);
                d.write("c.ts", "1");
                tokio::time::sleep(Duration::from_millis(600)).await;
                assert!(rx.try_recv().is_err(), "a dropped watcher is silent");
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn the_task_ends_when_nobody_listens() {
        let d = Dir::new("task-end");
        d.write("a.ts", "1");
        let mut s = spec(&d);
        s.interval = Duration::from_millis(50);
        s.debounce = Duration::ZERO;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Change>();
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let h = spawn(s, move |c| tx.send(c).is_ok()).unwrap();
                drop(rx);
                tokio::time::sleep(Duration::from_millis(150)).await;
                d.write("b.ts", "x");
                tokio::time::sleep(Duration::from_millis(300)).await;
                assert!(h.task.is_finished(), "the send failed, so the task returned");
            })
            .await;
    }

    #[test]
    fn writes_are_visible_through_a_held_open_file() {
        // An editor that keeps writing to one file handle: size and mtime move.
        let d = Dir::new("append");
        let mut f = fs::File::create(d.path("a.ts")).unwrap();
        let mut s = spec(&d);
        s.debounce = Duration::ZERO;
        let mut w = Watcher::new(&s).unwrap();
        let t = Instant::now();
        look(&mut w, t);
        f.write_all(b"more").unwrap();
        f.flush().unwrap();
        assert_eq!(look(&mut w, t).change.map(|c| c.kind), Some(Kind::Modified));
    }
}

//! wardend's resource history on disk (docs/protocol.md, "Resource
//! history"): a snapshot of the rings in `<state dir>/wardend-history.bin`,
//! written every minute and when wardend exits on purpose, read back when
//! it starts, so a restart (crash, upgrade, reboot) keeps the day's charts.
//!
//! - **Atomic.** Written to `.wardend-history.bin.tmp` (created 0600, never
//!   over an existing file), fsynced, renamed over the snapshot, then the
//!   directory is fsynced. A crash at any point leaves the previous snapshot
//!   or the new one, never a mix. One save at a time.
//! - **Off the core thread where it can wait.** The snapshot is encoded
//!   straight into the temporary file through a 64 KiB buffer (no copy of
//!   the rings; ~1.5 MB for 10 apps with a full day, a few ms); the fsync
//!   and the rename, which can take long on a busy disk, run on a blocking
//!   thread. Nothing is written per sample, and nothing when nothing was
//!   committed since the last snapshot.
//! - **Format** (version 1, little-endian), the in-memory records as they are:
//!
//!   ```text
//!   magic "WDHIST\r\n", version u32, step_s u32, saved_at u64 (Unix s),
//!   samples u32 (≤ CAP), apps u32 (≤ MAX_APPS), runs u32 (≤ samples)
//!   runs × (start u32, len u32)        sample times: runs 10 s apart, increasing
//!   host series(samples)
//!   apps × (name_len u8, name, len u32 (≤ samples), series(len))   rings end with the newest time
//!   crc32 u32                          of every byte before it
//!   series(m) := pairs (absent u32, present u32, present × 16-byte record)
//!                covering m samples; a pair after the first has absent ≥ 1
//!   ```
//!
//!   Absent samples (an app not watched, a host without metrics) cost
//!   nothing but their pair. The largest file any wardend writes is
//!   `MAX_BYTES` (17.9 MB: 128 apps with a full day): a bigger one is
//!   refused before it is read.
//! - **Loading.** Samples older than 24 h (wardend was down) or from the
//!   future (the clock went back) are dropped, and apps with no sample left.
//!   A file that is truncated, corrupt (checksum), of another version, too
//!   big or not a history at all is reported (WARN with the fix), moved
//!   aside to `wardend-history.bin.bad`, and wardend starts with an empty
//!   history. What is not saved: the 10 s being sampled at the exit, and
//!   the restart baselines (the first status after a start is the baseline,
//!   as before: restarts while wardend was down are not counted).

use super::{AppSample, AppSeries, CAP, HostSample, MAX_APPS, MEM, PRESENT, Ring, STEP_S, Store};
use flate2::{CrcReader, CrcWriter};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The snapshot, in wardend's state directory.
pub(crate) const FILE: &str = "wardend-history.bin";
const MAGIC: [u8; 8] = *b"WDHIST\r\n";
/// The format version this wardend writes and reads.
pub(crate) const VERSION: u32 = 1;
/// How often a snapshot is written.
const SAVE_EVERY: Duration = Duration::from_secs(60);
/// A save still running after this is reported (a hung disk or mount).
const SAVE_HUNG: Duration = Duration::from_secs(300);
/// App names longer than this are not saved (socket path limits keep real
/// names far shorter).
const MAX_NAME: usize = 255;
const HEADER_BYTES: u64 = 8 + 4 + 4 + 8 + 4 + 4 + 4;
/// One record: an `AppSample` or a `HostSample`.
const RECORD: usize = 16;

/// The largest `series(m)` the encoder writes: `8 × pairs + 16 × present`,
/// and pairs ≤ absent + 1.
const fn series_max(m: usize) -> u64 {
    RECORD as u64 * m as u64 + 8
}

/// The largest snapshot any wardend writes: every app (`MAX_APPS`) with a
/// full day and the longest name, the times in runs of one.
pub(crate) const MAX_BYTES: u64 =
    HEADER_BYTES + 8 * CAP as u64 + series_max(CAP) + MAX_APPS as u64 * (1 + MAX_NAME as u64 + 4 + series_max(CAP)) + 4;

/// `<state dir>/wardend-history.bin`.
pub(crate) fn path() -> PathBuf {
    crate::fleet::state_dir().join(FILE)
}

/// Where a snapshot is written before the rename.
fn tmp_path(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{FILE}.tmp"))
}

/// Where a file that cannot be loaded is moved.
pub(crate) fn bad_path(path: &Path) -> PathBuf {
    path.with_file_name(format!("{FILE}.bad"))
}

/// How often a snapshot is written. Debug builds only (like
/// `WARDEN_DAEMON_POLICY`): tests shorten it with `WARDEN_HISTORY_SAVE_MS`.
pub(crate) fn save_every() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("WARDEN_HISTORY_SAVE_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        return Duration::from_millis(ms.max(100));
    }
    SAVE_EVERY
}

// ------------------------------------------------------------------ records

impl AppSample {
    const fn to_bytes(self) -> [u8; RECORD] {
        let (c, m, r, k, x, f) = (
            self.cpu.to_bits().to_le_bytes(),
            self.mem_kib.to_le_bytes(),
            self.ready.to_le_bytes(),
            self.configured.to_le_bytes(),
            self.restarts.to_le_bytes(),
            self.flags.to_le_bytes(),
        );
        [c[0], c[1], c[2], c[3], m[0], m[1], m[2], m[3], r[0], r[1], k[0], k[1], x[0], x[1], f[0], f[1]]
    }

    fn from_bytes(b: [u8; RECORD]) -> Result<AppSample, Bad> {
        let s = AppSample {
            cpu: f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            mem_kib: u32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            ready: u16::from_le_bytes([b[8], b[9]]),
            configured: u16::from_le_bytes([b[10], b[11]]),
            restarts: u16::from_le_bytes([b[12], b[13]]),
            flags: u16::from_le_bytes([b[14], b[15]]),
        };
        if s.flags & !(PRESENT | MEM) != 0 {
            return Err(Bad::Corrupt(format!("an app sample has unknown flags {:#x}", s.flags)));
        }
        Ok(s)
    }
}

impl HostSample {
    const fn to_bytes(self) -> [u8; RECORD] {
        let (c, l, u, t) = (
            self.cpu.to_bits().to_le_bytes(),
            self.load1.to_bits().to_le_bytes(),
            self.mem_used_kib.to_le_bytes(),
            self.mem_total_kib.to_le_bytes(),
        );
        [c[0], c[1], c[2], c[3], l[0], l[1], l[2], l[3], u[0], u[1], u[2], u[3], t[0], t[1], t[2], t[3]]
    }

    fn from_bytes(b: [u8; RECORD]) -> Result<HostSample, Bad> {
        Ok(HostSample {
            cpu: f32::from_bits(u32::from_le_bytes([b[0], b[1], b[2], b[3]])),
            load1: f32::from_bits(u32::from_le_bytes([b[4], b[5], b[6], b[7]])),
            mem_used_kib: u32::from_le_bytes([b[8], b[9], b[10], b[11]]),
            mem_total_kib: u32::from_le_bytes([b[12], b[13], b[14], b[15]]),
        })
    }
}

/// A record type of a series: its bytes, and the one that means "absent".
trait Record: Copy {
    const ABSENT_BYTES: [u8; RECORD];
    fn bytes(self) -> [u8; RECORD];
    fn parse(b: [u8; RECORD]) -> Result<Self, Bad>;
    fn absent() -> Self;
}

impl Record for AppSample {
    const ABSENT_BYTES: [u8; RECORD] = AppSample::ABSENT.to_bytes();
    fn bytes(self) -> [u8; RECORD] {
        self.to_bytes()
    }
    fn parse(b: [u8; RECORD]) -> Result<Self, Bad> {
        AppSample::from_bytes(b)
    }
    fn absent() -> Self {
        AppSample::ABSENT
    }
}

impl Record for HostSample {
    const ABSENT_BYTES: [u8; RECORD] = HostSample::ABSENT.to_bytes();
    fn bytes(self) -> [u8; RECORD] {
        self.to_bytes()
    }
    fn parse(b: [u8; RECORD]) -> Result<Self, Bad> {
        HostSample::from_bytes(b)
    }
    fn absent() -> Self {
        HostSample::ABSENT
    }
}

// ------------------------------------------------------------------ writing

/// Write `ring`'s last `m` items as `series(m)`.
fn write_series<T: Record, W: Write>(w: &mut W, ring: &Ring<T>, m: usize) -> io::Result<()> {
    let first = ring.len() - m;
    let absent = |i: usize| ring.get(first + i).bytes() == T::ABSENT_BYTES;
    let mut i = 0;
    while i < m {
        let a0 = i;
        while i < m && absent(i) {
            i += 1;
        }
        let p0 = i;
        while i < m && !absent(i) {
            i += 1;
        }
        // m ≤ CAP: both fit in u32.
        w.write_all(&((p0 - a0) as u32).to_le_bytes())?;
        w.write_all(&((i - p0) as u32).to_le_bytes())?;
        // 256 records per write.
        let mut buf = [0u8; 256 * RECORD];
        let mut at = 0;
        for j in p0..i {
            buf[at..at + RECORD].copy_from_slice(&ring.get(first + j).bytes());
            at += RECORD;
            if at == buf.len() {
                w.write_all(&buf)?;
                at = 0;
            }
        }
        w.write_all(&buf[..at])?;
    }
    Ok(())
}

impl Store {
    /// Write the snapshot to `w`; the number of bytes written.
    pub(crate) fn encode<W: Write>(&self, w: W, saved_at_s: u64) -> io::Result<u64> {
        let n = self.times.len();
        debug_assert_eq!(self.host.len(), n, "the host's ring and the times grow together");
        let n = n.min(self.host.len());
        let apps: Vec<(&Arc<str>, &AppSeries)> = self
            .apps
            .iter()
            .filter(|(name, a)| !name.is_empty() && name.len() <= MAX_NAME && !a.ring.buf.is_empty())
            .collect();
        // The runs of times 10 s apart.
        let first = self.times.len() - n;
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for i in 0..n {
            let t = self.times.get(first + i);
            match runs.last_mut() {
                Some((start, len)) if u64::from(*start) + u64::from(*len) * STEP_S == u64::from(t) => *len += 1,
                _ => runs.push((t, 1)),
            }
        }
        let mut w = CrcWriter::new(w);
        w.write_all(&MAGIC)?;
        w.write_all(&VERSION.to_le_bytes())?;
        w.write_all(&(STEP_S as u32).to_le_bytes())?;
        w.write_all(&saved_at_s.to_le_bytes())?;
        w.write_all(&(n as u32).to_le_bytes())?;
        w.write_all(&(apps.len() as u32).to_le_bytes())?;
        w.write_all(&(runs.len() as u32).to_le_bytes())?;
        for (start, len) in runs {
            w.write_all(&start.to_le_bytes())?;
            w.write_all(&len.to_le_bytes())?;
        }
        write_series(&mut w, &self.host, n)?;
        for (name, a) in apps {
            let len = a.ring.len().min(n);
            w.write_all(&[name.len() as u8])?;
            w.write_all(name.as_bytes())?;
            w.write_all(&(len as u32).to_le_bytes())?;
            write_series(&mut w, &a.ring, len)?;
        }
        let (sum, amount) = (w.crc().sum(), w.crc().amount());
        let mut inner = w.into_inner();
        inner.write_all(&sum.to_le_bytes())?;
        Ok(u64::from(amount) + 4)
    }
}

// ------------------------------------------------------------------ reading

/// Why a snapshot cannot be used.
#[derive(Debug)]
pub(crate) enum Bad {
    /// Reading it failed (an I/O error): left where it is.
    Io(io::Error),
    NotAFile,
    TooBig(u64),
    NotHistory,
    Version(u32),
    Truncated,
    Corrupt(String),
}

impl Bad {
    /// What is wrong, and how to fix it.
    fn explain(&self, file: &Path) -> (String, String) {
        let dir = file.parent().unwrap_or(file).display().to_string();
        match self {
            Bad::Io(e) => (
                format!("reading it failed: {e}"),
                "check the file's permissions and the disk (`dmesg`); wardend replaces it at its next save".into(),
            ),
            Bad::NotAFile => (
                "it is not a regular file".into(),
                format!("something else uses that name in {dir}; wardend writes its own file there from now on"),
            ),
            Bad::TooBig(bytes) => (
                format!("it is {bytes} bytes, more than any wardend writes ({MAX_BYTES} at most)"),
                "something other than wardend wrote it; nothing to do, wardend writes a new one".into(),
            ),
            Bad::NotHistory => (
                "it is not a wardend history file (unknown magic)".into(),
                "something other than wardend wrote it; nothing to do, wardend writes a new one".into(),
            ),
            Bad::Version(v) => (
                format!("it is in format version {v}, and this wardend reads version {VERSION}"),
                "a wardend of another version wrote it (a downgrade?); the history starts again in this \
                 version's format"
                    .into(),
            ),
            Bad::Truncated => (
                "it is truncated (cut short, e.g. by a full disk or an incomplete copy)".into(),
                format!(
                    "nothing to do: wardend writes a new one within a minute; check the free space (`df -h {dir}`)"
                ),
            ),
            Bad::Corrupt(why) => (
                format!("it is corrupt ({why})"),
                "nothing to do: wardend writes a new one within a minute; if it happens again, check the disk \
                 (`dmesg`) and report it with the moved file"
                    .into(),
            ),
        }
    }
}

/// A reader that keeps the CRC of what it read and calls an early end of
/// file `Truncated`.
struct Input<R: Read> {
    r: CrcReader<R>,
}

impl<R: Read> Input<R> {
    fn array<const N: usize>(&mut self) -> Result<[u8; N], Bad> {
        let mut b = [0u8; N];
        self.r.read_exact(&mut b).map_err(eof_is_truncated)?;
        Ok(b)
    }

    fn u32(&mut self) -> Result<u32, Bad> {
        self.array::<4>().map(u32::from_le_bytes)
    }

    fn u64(&mut self) -> Result<u64, Bad> {
        self.array::<8>().map(u64::from_le_bytes)
    }

    /// `series(m)`, as a Vec (oldest first).
    fn series<T: Record>(&mut self, m: usize, what: &str) -> Result<Vec<T>, Bad> {
        let mut v: Vec<T> = Vec::with_capacity(m);
        let mut pairs = 0u32;
        while v.len() < m {
            let (absent, present) = (self.u32()? as usize, self.u32()? as usize);
            if absent + present == 0 || (pairs > 0 && absent == 0) || absent + present > m - v.len() {
                return Err(Bad::Corrupt(format!("the runs of {what} do not add up")));
            }
            pairs += 1;
            v.extend(std::iter::repeat_n(T::absent(), absent));
            // 256 records per read.
            let mut buf = [0u8; 256 * RECORD];
            let mut left = present;
            while left > 0 {
                let k = left.min(256);
                self.r.read_exact(&mut buf[..k * RECORD]).map_err(eof_is_truncated)?;
                for rec in buf[..k * RECORD].chunks_exact(RECORD) {
                    let mut b = [0u8; RECORD];
                    b.copy_from_slice(rec);
                    v.push(T::parse(b)?);
                }
                left -= k;
            }
        }
        Ok(v)
    }
}

fn eof_is_truncated(e: io::Error) -> Bad {
    if e.kind() == io::ErrorKind::UnexpectedEof { Bad::Truncated } else { Bad::Io(e) }
}

/// What a load kept and dropped.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Loaded {
    pub samples: usize,
    pub apps: usize,
    /// Unix seconds of the oldest and newest sample kept.
    pub oldest_s: Option<u64>,
    pub newest_s: Option<u64>,
    pub saved_at_s: u64,
    /// Samples older than 24 h, and from the future.
    pub dropped_old: usize,
    pub dropped_future: usize,
    pub dropped_apps: usize,
}

impl Store {
    /// Read a snapshot written by `encode`, keeping the samples of the last
    /// 24 h before `now_s`.
    pub(crate) fn decode<R: Read>(r: R, now_s: u64) -> Result<(Store, Loaded), Bad> {
        let mut r = Input { r: CrcReader::new(r) };
        if r.array::<8>()? != MAGIC {
            return Err(Bad::NotHistory);
        }
        let version = r.u32()?;
        if version != VERSION {
            return Err(Bad::Version(version));
        }
        let step = r.u32()?;
        if u64::from(step) != STEP_S {
            return Err(Bad::Corrupt(format!("a sample every {step} s, not {STEP_S}")));
        }
        let saved_at_s = r.u64()?;
        let n = r.u32()? as usize;
        let n_apps = r.u32()? as usize;
        let n_runs = r.u32()? as usize;
        if n > CAP || n_apps > MAX_APPS || n_runs > n || (n > 0 && n_runs == 0) {
            return Err(Bad::Corrupt(format!("{n} samples of {n_apps} apps in {n_runs} runs")));
        }
        let mut times: Vec<u32> = Vec::with_capacity(n);
        for _ in 0..n_runs {
            let (start, len) = (r.u32()?, r.u32()? as usize);
            let follows = times.last().is_none_or(|last| start > *last);
            if len == 0 || len > n - times.len() || u64::from(start) % STEP_S != 0 || !follows {
                return Err(Bad::Corrupt("the sample times are out of order".into()));
            }
            for k in 0..len {
                let t = u64::from(start) + k as u64 * STEP_S;
                times.push(u32::try_from(t).map_err(|_| Bad::Corrupt("a sample time is out of range".into()))?);
            }
        }
        if times.len() != n {
            return Err(Bad::Corrupt("the sample times do not add up".into()));
        }
        let host: Vec<HostSample> = r.series(n, "the host's samples")?;
        let mut apps: Vec<(Arc<str>, Vec<AppSample>)> = Vec::with_capacity(n_apps);
        for _ in 0..n_apps {
            let [len] = r.array::<1>()?;
            let mut name = vec![0u8; usize::from(len)];
            r.r.read_exact(&mut name).map_err(eof_is_truncated)?;
            let name = String::from_utf8(name).map_err(|_| Bad::Corrupt("an app name is not UTF-8".into()))?;
            if name.is_empty() || apps.iter().any(|(a, _)| **a == *name) {
                return Err(Bad::Corrupt(format!("app name {name:?} is empty or twice")));
            }
            let m = r.u32()? as usize;
            if m > n {
                return Err(Bad::Corrupt(format!("app {name:?} has more samples than there are times")));
            }
            let samples = r.series(m, &format!("app {name:?}"))?;
            apps.push((name.into(), samples));
        }
        let sum = r.r.crc().sum();
        let stored = r.u32()?;
        if stored != sum {
            return Err(Bad::Corrupt(format!("checksum {stored:#010x}, expected {sum:#010x}")));
        }
        let mut more = [0u8; 1];
        match r.r.get_mut().read(&mut more) {
            Ok(0) => {}
            Ok(_) => return Err(Bad::Corrupt("there is data after the checksum".into())),
            Err(e) => return Err(Bad::Io(e)),
        }
        Ok(Store::assemble(times, host, apps, saved_at_s, now_s))
    }

    /// The store of what `decode` read: the last 24 h before `now_s`, no
    /// sample from the future.
    fn assemble(
        mut times: Vec<u32>,
        mut host: Vec<HostSample>,
        apps: Vec<(Arc<str>, Vec<AppSample>)>,
        saved_at_s: u64,
        now_s: u64,
    ) -> (Store, Loaded) {
        let n = times.len();
        let oldest = now_s.saturating_sub(CAP as u64 * STEP_S);
        let open = now_s - now_s % STEP_S;
        // Keep times[lo..hi]: times increase.
        let lo = times.partition_point(|t| u64::from(*t) < oldest);
        let hi = times.partition_point(|t| u64::from(*t) < open).max(lo);
        let kept = hi - lo;
        let mut info = Loaded {
            saved_at_s,
            dropped_old: lo,
            dropped_future: n - hi,
            oldest_s: times.get(lo).filter(|_| kept > 0).map(|t| u64::from(*t)),
            newest_s: hi.checked_sub(1).filter(|_| kept > 0).and_then(|i| times.get(i)).map(|t| u64::from(*t)),
            ..Loaded::default()
        };
        // In place: no second copy of the rings while loading.
        fn keep<T>(v: &mut Vec<T>, lo: usize, hi: usize) {
            v.truncate(hi);
            v.drain(..lo);
        }
        keep(&mut times, lo, hi);
        keep(&mut host, lo, hi);
        let mut store = Store {
            times: Ring { buf: times, head: 0 },
            host: Ring { buf: host, head: 0 },
            seq: kept as u64,
            ..Store::default()
        };
        let mut map = BTreeMap::new();
        for (name, mut ring) in apps {
            // Its ring covers times[n - len..n].
            let begin = n - ring.len();
            let from = lo.max(begin);
            if from >= hi {
                info.dropped_apps += 1;
                continue;
            }
            keep(&mut ring, from - begin, hi - begin);
            // `seq` of the commit of kept sample k is k + 1.
            let Some(last) = ring.iter().rposition(|s| s.flags & PRESENT != 0) else {
                info.dropped_apps += 1;
                continue;
            };
            let last_present = (from - lo + last + 1) as u64;
            map.insert(name, AppSeries { ring: Ring { buf: ring, head: 0 }, last_present, ..AppSeries::default() });
        }
        info.samples = kept;
        info.apps = map.len();
        store.apps = map;
        (store, info)
    }
}

// ------------------------------------------------------------------ loading

/// Read the snapshot at `path` (at start). Whatever happens, a store comes
/// back: the history on disk, or an empty one (said why in the log).
pub(crate) fn load(path: &Path, now_s: u64) -> Store {
    let t0 = Instant::now();
    let result = match std::fs::metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            crate::debug!("no resource history on disk yet", file = path.display());
            return Store::default();
        }
        Err(e) => Err(Bad::Io(e)),
        Ok(m) if !m.is_file() => Err(Bad::NotAFile),
        Ok(m) if m.len() > MAX_BYTES => Err(Bad::TooBig(m.len())),
        Ok(_) => match std::fs::File::open(path) {
            // `take`: a file that grows meanwhile is still read within the bound.
            Ok(f) => Store::decode(BufReader::with_capacity(64 << 10, f.take(MAX_BYTES + 1)), now_s),
            Err(e) => Err(Bad::Io(e)),
        },
    };
    match result {
        Ok((store, info)) => {
            let at =
                |s: Option<u64>| s.map(|s| crate::logging::format_rfc3339(s as i64, 0)).unwrap_or_else(|| "-".into());
            crate::info!(
                "resource history loaded",
                file = path.display(),
                samples = info.samples,
                apps = info.apps,
                oldest = at(info.oldest_s),
                newest = at(info.newest_s),
                dropped_old = info.dropped_old,
                load_ms = t0.elapsed().as_millis(),
            );
            if info.dropped_future > 0 {
                crate::warn!(
                    "the resource history on disk is newer than the clock; dropping the samples from the future",
                    file = path.display(),
                    dropped = info.dropped_future,
                    newest_on_disk = at(Some(info.saved_at_s)),
                    now = at(Some(now_s)),
                    hint = "check the system clock (`timedatectl`); the charts continue from now",
                );
            }
            store
        }
        Err(bad) => {
            let (reason, hint) = bad.explain(path);
            if let Bad::Io(_) = bad {
                crate::warn!(
                    "cannot read the resource history; starting with an empty one",
                    file = path.display(),
                    error = reason,
                    hint = hint,
                );
                return Store::default();
            }
            let aside = bad_path(path);
            let moved = match std::fs::rename(path, &aside) {
                Ok(()) => aside.display().to_string(),
                Err(e) => format!("not moved ({e}); the next save replaces it"),
            };
            crate::warn!(
                "cannot use the resource history on disk; starting with an empty one",
                file = path.display(),
                reason = reason,
                moved_to = moved,
                hint = hint,
            );
            Store::default()
        }
    }
}

// ------------------------------------------------------------------ saving

/// A snapshot in its temporary file, written but not yet synced and renamed.
pub(crate) struct Pending {
    file: std::fs::File,
    tmp: PathBuf,
    path: PathBuf,
    pub bytes: u64,
    pub samples: usize,
    pub apps: usize,
    pub encode_ms: u128,
}

impl Pending {
    /// fsync, rename over the snapshot, fsync the directory: on a blocking
    /// thread, since a busy disk can take seconds.
    pub async fn finish(self) -> Result<(), String> {
        let (file, tmp, path) = (self.file, self.tmp, self.path);
        let cleanup = tmp.clone();
        let r = tokio::task::spawn_blocking(move || -> Result<(), String> {
            file.sync_all().map_err(|e| format!("fsync of {}: {e}", tmp.display()))?;
            drop(file);
            std::fs::rename(&tmp, &path)
                .map_err(|e| format!("renaming {} to {}: {e}", tmp.display(), path.display()))?;
            // The rename is durable once the directory is synced; some
            // filesystems refuse to sync a directory, which changes nothing.
            if let Some(Ok(d)) = path.parent().map(std::fs::File::open) {
                let _ = d.sync_all();
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|e| Err(format!("the thread writing it failed: {e}")));
        if r.is_err() {
            let _ = std::fs::remove_file(&cleanup); // best effort; the next save removes it too
        }
        r
    }
}

#[derive(Default)]
struct SaveState {
    /// A snapshot is being synced and renamed, since then.
    busy: Cell<bool>,
    since: Cell<Option<Instant>>,
    /// A save running for `SAVE_HUNG` was reported.
    hung_logged: Cell<bool>,
    /// A failure was logged; the next success says so.
    failing: Cell<bool>,
    done: tokio::sync::Notify,
}

/// The running save: `busy` until dropped, whatever ends the task (a panic
/// included), so saves never stop for good.
struct Running(Rc<SaveState>);

impl Running {
    fn new(state: &Rc<SaveState>) -> Running {
        state.busy.set(true);
        state.since.set(Some(Instant::now()));
        Running(state.clone())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.busy.set(false);
        self.0.since.set(None);
        self.0.hung_logged.set(false);
        self.0.done.notify_waiters();
    }
}

impl SaveState {
    fn report(&self, path: &Path, r: Result<(), String>, p: Option<(u64, usize, u128)>) {
        match r {
            Ok(()) => {
                if self.failing.replace(false) {
                    crate::info!("resource history saved again", file = path.display());
                }
                if let Some((bytes, apps, ms)) = p {
                    crate::debug!(
                        "resource history saved",
                        file = path.display(),
                        bytes = bytes,
                        apps = apps,
                        encode_ms = ms
                    );
                }
            }
            Err(e) => {
                // Once until it works again: it is retried every minute.
                if !self.failing.replace(true) {
                    let dir = path.parent().unwrap_or(path).display().to_string();
                    crate::warn!(
                        "cannot save the resource history; it is kept in memory only until a save works",
                        file = path.display(),
                        error = e,
                        hint = format!(
                            "check that {dir} exists and wardend's user can write it, and that the disk has room \
                             (`df -h {dir}`); wardend retries every minute"
                        ),
                    );
                }
            }
        }
    }
}

/// Writes the snapshot: every `save_every()` while something changed, and
/// at a clean exit.
pub(crate) struct Saver {
    path: PathBuf,
    /// `seq` of the store the last snapshot holds.
    saved_seq: Option<u64>,
    state: Rc<SaveState>,
}

impl Saver {
    /// `store`: what was loaded from `path` (its `seq` is what the file holds).
    pub fn new(path: PathBuf, store: &Store) -> Saver {
        let saved_seq = (store.seq > 0).then_some(store.seq);
        Saver { path, saved_seq, state: Rc::default() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Encode `store` into the temporary file (here, on the core thread:
    /// buffered writes into the page cache).
    pub fn start(&mut self, store: &Store, now_s: u64) -> Result<Pending, String> {
        let t0 = Instant::now();
        let tmp = tmp_path(&self.path);
        let write = || -> Result<(std::fs::File, u64), String> {
            use std::os::unix::fs::OpenOptionsExt;
            if let Some(dir) = self.path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
            }
            match std::fs::remove_file(&tmp) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("removing the stale {}: {e}", tmp.display())),
            }
            // create_new: never through a file or symlink someone put there.
            let f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .map_err(|e| format!("creating {}: {e}", tmp.display()))?;
            let mut w = BufWriter::with_capacity(64 << 10, f);
            let bytes = store.encode(&mut w, now_s).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
            let f = w.into_inner().map_err(|e| format!("writing {}: {}", tmp.display(), e.error()))?;
            Ok((f, bytes))
        };
        match write() {
            Ok((file, bytes)) => {
                self.saved_seq = Some(store.seq);
                Ok(Pending {
                    file,
                    tmp,
                    path: self.path.clone(),
                    bytes,
                    samples: store.times.len(),
                    apps: store.apps.len(),
                    encode_ms: t0.elapsed().as_millis(),
                })
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp); // best effort: whatever was written of it
                Err(e)
            }
        }
    }

    /// The periodic save: nothing when nothing was committed since the last
    /// snapshot, or while the last one is still being synced.
    pub fn periodic(&mut self, store: &Store, now_s: u64) {
        if self.state.busy.get() {
            let running = self.state.since.get().map(|t| t.elapsed()).unwrap_or_default();
            if running >= SAVE_HUNG && !self.state.hung_logged.replace(true) {
                let dir = self.path.parent().unwrap_or(&self.path).display().to_string();
                crate::warn!(
                    "saving the resource history has not finished; no new snapshot until it does",
                    file = self.path.display(),
                    running_s = running.as_secs(),
                    hint = format!(
                        "fsync or rename in {dir} is stuck: a hung disk or network mount (`dmesg`); wardend keeps the \
                         history in memory meanwhile"
                    ),
                );
            }
            return;
        }
        if self.saved_seq == Some(store.seq) {
            return;
        }
        match self.start(store, now_s) {
            Ok(p) => {
                let running = Running::new(&self.state);
                let path = self.path.clone();
                let stats = (p.bytes, p.apps, p.encode_ms);
                crate::guard::spawn_request("wardend history save", async move {
                    let r = p.finish().await;
                    running.0.report(&path, r, Some(stats));
                    drop(running);
                });
            }
            Err(e) => self.state.report(&self.path, Err(e), None),
        }
    }

    /// Resolves once no save is running.
    pub fn idle(&self) -> impl Future<Output = ()> + 'static {
        let state = self.state.clone();
        async move {
            // Created before the check: `notify_waiters` reaches it from then on.
            let done = state.done.notified();
            if state.busy.get() {
                done.await;
            }
        }
    }

    #[cfg(test)]
    pub fn busy(&self) -> bool {
        self.state.busy.get()
    }

    /// Record the outcome of a save made with `start` and `Pending::finish`.
    pub fn report(&self, r: Result<(), String>) {
        self.state.report(&self.path, r, None);
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::status;
    use super::*;

    const T0: u64 = 1_790_000_000; // a multiple of 10

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wd-hist-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A store with 3 apps over 40 samples: `api` all along (with gaps and a
    /// sample without CPU), `web` from sample 10, `gone` only at first; the
    /// host with a gap. The clock is at `T0 + 40 * 10 + 5`.
    fn sample_store() -> (Store, u64) {
        let mut st = Store::default();
        let (api, web, gone): (Arc<str>, Arc<str>, Arc<str>) = ("api".into(), "web".into(), "gone".into());
        let mut now = T0;
        st.tick(now);
        for i in 0..40u64 {
            if i % 7 != 3 {
                let mut s = status(2, 1.0 + i as f64, (10 + i) << 20, i / 5);
                if i == 5 {
                    s.workers.iter_mut().for_each(|w| w.cpu_percent = None);
                }
                st.observe_app(&api, &s, (i / 20) as u32);
            }
            if i >= 10 {
                st.observe_app(&web, &status(1, 0.5, 5 << 20, 0), 0);
            }
            if i < 3 {
                st.observe_app(&gone, &status(2, 2.0, 1 << 20, 0), 0);
            }
            if i % 9 != 4 {
                st.observe_host(10.0 + i as f64, (1 + i) << 20, 8 << 30, 0.25 * i as f64);
            }
            now += STEP_S;
            st.tick(now);
        }
        (st, now + 5)
    }

    fn encoded(st: &Store, now: u64) -> Vec<u8> {
        let mut v = Vec::new();
        let n = st.encode(&mut v, now).unwrap();
        assert_eq!(n as usize, v.len());
        v
    }

    /// Everything `history` can answer, for every app and step.
    fn answers(st: &Store, now: u64) -> Vec<warden_protocol::events::ResourceHistory> {
        let mut out = Vec::new();
        for app in [None, Some("api"), Some("web"), Some("gone"), Some("")] {
            for step in [10, 20, 60, 3600] {
                out.push(st.query(app, Some(T0 * 1000 - 50_000), Some(step), now * 1000));
            }
        }
        out
    }

    fn same_rings(a: &Store, b: &Store) {
        let bits = |r: &Ring<AppSample>| (0..r.len()).map(|i| r.get(i).to_bytes()).collect::<Vec<_>>();
        let host = |r: &Ring<HostSample>| (0..r.len()).map(|i| r.get(i).to_bytes()).collect::<Vec<_>>();
        let times = |r: &Ring<u32>| (0..r.len()).map(|i| r.get(i)).collect::<Vec<_>>();
        assert_eq!(times(&a.times), times(&b.times));
        assert_eq!(host(&a.host), host(&b.host));
        assert_eq!(a.apps.keys().collect::<Vec<_>>(), b.apps.keys().collect::<Vec<_>>());
        for (k, x) in &a.apps {
            assert_eq!(bits(&x.ring), bits(&b.apps[k].ring), "{k}");
            assert_eq!(x.last_present, b.apps[k].last_present, "{k}");
        }
        assert_eq!(a.seq, b.seq);
    }

    #[test]
    fn a_snapshot_round_trips() {
        let (st, now) = sample_store();
        let bytes = encoded(&st, now);
        let (back, info) = Store::decode(&bytes[..], now).unwrap();
        same_rings(&st, &back);
        assert_eq!(answers(&st, now), answers(&back, now), "every query answers the same");
        assert_eq!(
            info,
            Loaded {
                samples: 40,
                apps: 3,
                oldest_s: Some(T0),
                newest_s: Some(T0 + 390),
                saved_at_s: now,
                ..Loaded::default()
            }
        );
        // Absent samples cost only their run (`gone` has 37 of 40), and the
        // 40 times are one run.
        let raw = HEADER_BYTES as usize
            + 8
            + 8
            + RECORD * 40
            + st.apps.iter().map(|(k, a)| 1 + k.len() + 4 + 8 + RECORD * a.ring.len()).sum::<usize>()
            + 4;
        assert!(bytes.len() < raw - 30 * RECORD, "{} bytes, {raw} without runs", bytes.len());
        // A loaded store goes on: the next commit lines up with the rest.
        let mut back = back;
        back.tick(now);
        back.observe_app(&Arc::from("web"), &status(1, 9.0, 1 << 20, 0), 0);
        back.tick(now + STEP_S);
        let h = back.query(Some("web"), Some((now - 20) * 1000), None, (now + STEP_S) * 1000);
        assert_eq!(h.apps[0].cpu_percent, [Some(1.0), Some(1.0), Some(18.0), None], "{h:?}");
        // An empty store too.
        let (empty, info) = Store::decode(&encoded(&Store::default(), T0)[..], T0).unwrap();
        assert_eq!((empty.times.len(), empty.apps.len(), info.samples), (0, 0, 0));
    }

    /// `apps` apps with a full day each, samples alternating present and
    /// absent (the most runs), names of `name_len` bytes; times `spacing` s
    /// apart, the newest just before `T0 + CAP * 10`.
    fn full_store(apps: usize, name_len: usize, spacing: u64) -> Store {
        let mut st = Store::default();
        let first = T0 + CAP as u64 * STEP_S - CAP as u64 * spacing;
        for i in 0..CAP {
            st.times.push((first + i as u64 * spacing) as u32);
            st.host.push(if i % 2 == 0 { HostSample::ABSENT } else { HostSample { cpu: 1.0, ..HostSample::ABSENT } });
        }
        st.seq = CAP as u64;
        let sample = AppSample { cpu: 1.5, mem_kib: 7, ready: 1, configured: 2, restarts: 0, flags: PRESENT | MEM };
        for a in 0..apps {
            let mut ring = Ring::default();
            for i in 0..CAP {
                ring.push(if i % 2 == 1 { AppSample::ABSENT } else { sample });
            }
            let name = format!("{a:03}{}", "x".repeat(name_len - 3));
            st.apps.insert(name.into(), AppSeries { ring, last_present: CAP as u64 - 1, ..AppSeries::default() });
        }
        st
    }

    #[test]
    fn full_rings_round_trip_within_the_size_bound() {
        assert_eq!(MAX_BYTES, 17_936_432, "docs/protocol.md states it");
        let now = T0 + CAP as u64 * STEP_S;
        // Every app with the longest name, and times 20 s apart (a run
        // each): the largest file there can be.
        let worst = full_store(MAX_APPS, MAX_NAME, 2 * STEP_S);
        let bytes = encoded(&worst, now);
        assert!(bytes.len() as u64 <= MAX_BYTES, "{} > {MAX_BYTES}", bytes.len());
        assert!(bytes.len() as u64 > MAX_BYTES * 3 / 5, "{}", bytes.len());
        // Two days of samples: the older half is dropped.
        let (_, info) = Store::decode(&bytes[..], now).unwrap();
        assert_eq!((info.samples, info.dropped_old, info.apps), (CAP / 2, CAP / 2, MAX_APPS));
        drop(bytes);
        // A full day of 10 apps comes back exactly.
        let st = full_store(10, 20, STEP_S);
        let bytes = encoded(&st, now);
        let (back, info) = Store::decode(&bytes[..], now).unwrap();
        assert_eq!((info.samples, info.apps), (CAP, 10));
        same_rings(&st, &back);
    }

    #[test]
    fn corruption_anywhere_is_caught() {
        let (st, now) = sample_store();
        let bytes = encoded(&st, now);
        for i in 0..bytes.len() {
            for bit in [0x01, 0x80] {
                let mut b = bytes.clone();
                b[i] ^= bit;
                match Store::decode(&b[..], now) {
                    Ok(_) => panic!("flipping bit {bit:#x} of byte {i} went unnoticed"),
                    Err(Bad::Io(e)) => panic!("byte {i}: {e}"),
                    Err(_) => {}
                }
            }
        }
        let mut b = bytes.clone();
        b.extend_from_slice(b"junk");
        assert!(matches!(Store::decode(&b[..], now), Err(Bad::Corrupt(why)) if why.contains("after the checksum")));
        let mid = bytes.len() / 2;
        let mut b = bytes.clone();
        b[mid] ^= 0x10;
        assert!(matches!(Store::decode(&b[..], now), Err(Bad::Corrupt(_)) | Err(Bad::Truncated)));
        let mut b = bytes;
        b[..8].copy_from_slice(b"NOTWDHST");
        assert!(matches!(Store::decode(&b[..], now), Err(Bad::NotHistory)));
    }

    #[test]
    fn every_truncation_is_caught() {
        let (st, now) = sample_store();
        let bytes = encoded(&st, now);
        for len in 0..bytes.len() {
            match Store::decode(&bytes[..len], now) {
                Err(Bad::Truncated) => {}
                other => panic!("cut at {len} of {}: {:?}", bytes.len(), other.map(|(_, i)| i)),
            }
        }
    }

    #[test]
    fn another_version_is_refused() {
        let (st, now) = sample_store();
        let mut bytes = encoded(&st, now);
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(Store::decode(&bytes[..], now), Err(Bad::Version(2))));
        bytes[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(matches!(Store::decode(&bytes[..], now), Err(Bad::Version(0))));
    }

    #[test]
    fn old_and_future_samples_are_dropped_and_apps_age_out() {
        let (st, now) = sample_store();
        let bytes = encoded(&st, now);
        // A day and 200 s later: the first 21 samples (T0..T0+200) are older
        // than 24 h. `gone` (samples 0..3) has none left.
        let later = T0 + CAP as u64 * STEP_S + 205;
        let (back, info) = Store::decode(&bytes[..], later).unwrap();
        assert_eq!((info.dropped_old, info.samples, info.dropped_future), (21, 19, 0));
        assert_eq!((info.apps, info.dropped_apps), (2, 1));
        assert!(!back.apps.contains_key("gone"));
        assert_eq!(back.times.get(0), (T0 + 210) as u32);
        assert_eq!(back.seq, 19);
        assert_eq!(back.apps["api"].ring.len(), 19);
        // api had a sample last at i = 39: the newest kept, seq 19.
        assert_eq!(back.apps["api"].last_present, 19);
        let h = back.query(Some("api"), None, None, later * 1000);
        assert_eq!(h.apps[0].cpu_percent.iter().flatten().count(), 19 - 3, "every 7th (from 3) is a gap");
        // Then it keeps ageing as samples come: api is forgotten a day after its last sample.
        let mut back = back;
        let mut t = later;
        for _ in 0..CAP - 19 {
            t += STEP_S;
            back.tick(t);
        }
        assert!(back.apps.contains_key("api"));
        for _ in 0..20 {
            t += STEP_S;
            back.tick(t);
        }
        assert!(!back.apps.contains_key("api"), "no sample for 24 h");

        // Two days later: nothing is left.
        let (back, info) = Store::decode(&bytes[..], T0 + 2 * 86_400).unwrap();
        assert_eq!((info.samples, info.apps, info.dropped_old, back.times.len()), (0, 0, 40, 0));
        assert_eq!((info.oldest_s, info.newest_s), (None, None));

        // The clock went back 100 s: the 10 newest samples (and the one now
        // being sampled) are from the future.
        let (back, info) = Store::decode(&bytes[..], T0 + 300).unwrap();
        assert_eq!((info.samples, info.dropped_future), (30, 10));
        assert_eq!(back.times.get(back.times.len() - 1), (T0 + 290) as u32);
        assert_eq!(back.apps["web"].ring.len(), 20, "web began at sample 10");
        assert_eq!(back.apps["web"].last_present, 30);
    }

    #[test]
    fn load_moves_a_bad_file_aside_and_starts_empty() {
        let dir = tmpdir("load");
        let path = dir.join(FILE);
        // No file: empty, nothing written.
        assert_eq!(load(&path, T0).times.len(), 0);
        assert!(!path.exists() && !bad_path(&path).exists());

        let (st, now) = sample_store();
        let good = encoded(&st, now);
        let check_bad = |contents: &[u8]| {
            std::fs::write(&path, contents).unwrap();
            let s = load(&path, now);
            assert_eq!((s.times.len(), s.apps.len()), (0, 0));
            assert!(!path.exists(), "moved aside");
            assert_eq!(std::fs::read(bad_path(&path)).unwrap(), contents);
        };
        check_bad(&good[..good.len() - 10]);
        let mut corrupt = good.clone();
        corrupt[100] ^= 4;
        check_bad(&corrupt);
        let mut v2 = good.clone();
        v2[8] = 2;
        check_bad(&v2);
        check_bad(b"{\"not\":\"history\"}");

        // Too big: refused unread (a sparse file).
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(MAX_BYTES + 1).unwrap();
        drop(f);
        let s = load(&path, now);
        assert_eq!(s.times.len(), 0);
        assert!(!path.exists() && std::fs::metadata(bad_path(&path)).unwrap().len() == MAX_BYTES + 1);
        // Not a file at all (a FIFO would block a read forever).
        std::fs::remove_file(bad_path(&path)).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(load(&path, now).times.len(), 0);
        assert!(!path.exists());
        let _ = std::fs::remove_dir(bad_path(&path));

        // A good file loads.
        std::fs::write(&path, &good).unwrap();
        let s = load(&path, now);
        same_rings(&st, &s);
        assert!(path.exists(), "a good file stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stuck_save_is_reported_once_and_a_dropped_one_frees_the_saver() {
        let dir = tmpdir("stuck");
        let (st, now) = sample_store();
        let mut saver = Saver::new(dir.join(FILE), &Store::default());
        // A save that ended any way at all (finished, failed, its task
        // dropped or panicked) lets the next one run.
        let running = Running::new(&saver.state);
        assert!(saver.busy());
        drop(running);
        assert!(!saver.busy() && saver.state.since.get().is_none());
        // One running for more than SAVE_HUNG: said once; nothing else starts.
        let running = Running::new(&saver.state);
        saver.state.since.set(Instant::now().checked_sub(SAVE_HUNG + Duration::from_secs(1)));
        saver.periodic(&st, now);
        assert!(saver.state.hung_logged.get() && !dir.join(FILE).exists());
        saver.periodic(&st, now);
        drop(running);
        assert!(!saver.state.hung_logged.get(), "reset for the next one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn saves_are_atomic_private_and_only_when_something_changed() {
        use std::os::unix::fs::PermissionsExt;
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let dir = tmpdir("save").join("state"); // created by the save
                let path = dir.join(FILE);
                let (mut st, now) = sample_store();
                // A stale temporary file from a crash is replaced.
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(tmp_path(&path), b"stale").unwrap();
                let mut saver = Saver::new(path.clone(), &Store::default());
                saver.periodic(&st, now);
                assert!(saver.busy());
                saver.idle().await;
                assert!(!saver.busy());
                let meta = std::fs::metadata(&path).unwrap();
                assert_eq!(meta.permissions().mode() & 0o777, 0o600);
                assert!(!tmp_path(&path).exists(), "renamed into place");
                same_rings(&st, &Store::decode(&std::fs::read(&path).unwrap()[..], now).unwrap().0);
                // Nothing committed since: nothing written.
                std::fs::remove_file(&path).unwrap();
                saver.periodic(&st, now + 1);
                assert!(!saver.busy() && !path.exists());
                // A commit later: written again.
                st.observe_host(1.0, 1, 2, 0.0);
                st.tick(now + 10);
                saver.periodic(&st, now + 10);
                saver.idle().await;
                assert_eq!(Store::decode(&std::fs::read(&path).unwrap()[..], now + 10).unwrap().1.samples, 41);
                // A failure is reported, and the file in place stays.
                let before = std::fs::read(&path).unwrap();
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
                st.tick(now + 20);
                let r = saver.start(&st, now + 20);
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
                // Root ignores directory permissions: only check when they apply.
                if let Err(e) = r {
                    assert!(e.contains(".wardend-history.bin.tmp"), "{e}");
                    assert_eq!(std::fs::read(&path).unwrap(), before);
                }
                let _ = std::fs::remove_dir_all(dir.parent().unwrap());
            })
            .await;
    }
}

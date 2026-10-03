//! The work of background compression, which a child process of the worker
//! does (`warden static-compress`, started by `compress.rs`): compress one
//! file into the store, or look over the store. One process for each job keeps
//! the worker a single thread, which is what makes its requests fast, and
//! keeps the memory of the compressors (tens of MB) and any failure of theirs
//! out of the server.
//!
//! The job comes in `WARDEN_COMPRESS_JOB` as JSON, and the process says what
//! came of it by its exit status and one line on stdout (`Outcome`).
//!
//! The compressions running at once, for all the workers of a site together,
//! are held to `compress_jobs` by lock files (`flock`): a job takes one of that
//! many `place-N` files in the store folder, and a process that dies gives its
//! place up. One copy is made once however many workers want it: the file being
//! written is locked too.
//!
//! A copy is only made of a version of the file that has been still for two
//! seconds and is the same before and after it was read; one that is not
//! smaller than the file is not kept.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::open::open_source;
use super::store::{Entry, Store, Version};

/// Files up to this size are compressed with brotli at its best level, which
/// takes about 1.5 s per MB; larger ones at level 9, which takes a tenth of
/// that and gives up some 8 % of the size.
const BEST_UP_TO: u64 = 1 << 20;

/// The size of a read, and so how often a job looks whether it is still wanted.
const CHUNK: usize = 64 << 10;

/// What a worker asks of the process.
#[derive(Serialize, Deserialize)]
pub(super) struct Spec {
    /// The worker's process id: when it is not the parent any more, nobody
    /// wants the result.
    pub parent: u32,
    pub root: PathBuf,
    pub store: PathBuf,
    pub cap: u64,
    pub task: Task,
}

#[derive(Serialize, Deserialize)]
pub(super) enum Task {
    /// Make the copies of this version of `rel`.
    Compress { rel: String, version: Version, min: u64, max: u64, jobs: usize },
    /// Count the copies, and remove the oldest while there are too many.
    Sweep,
}

/// What came of a job.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The copies are in the store (made now, or by someone else before);
    /// what was read, written, and removed of older versions.
    Done { read: u64, wrote: u64, freed: u64 },
    /// Nothing was made because the file does not shrink.
    NoGain,
    /// Nothing was made: another worker is at it, or the file is not
    /// compressed yet (too fresh) or not any more (outside the limits).
    Later,
    /// The file is not the version the job was for: the next request asks again.
    Changed,
    /// The store holds this many bytes of copies.
    Swept { total: u64 },
}

const EXIT_NO_GAIN: i32 = 3;
const EXIT_LATER: i32 = 4;
const EXIT_CHANGED: i32 = 5;

impl Outcome {
    /// The exit status and the stdout line of a process that ends with this.
    fn report(&self) -> (i32, String) {
        match self {
            Outcome::Done { read, wrote, freed } => (0, format!("done {read} {wrote} {freed}")),
            Outcome::Swept { total } => (0, format!("swept {total}")),
            Outcome::NoGain => (EXIT_NO_GAIN, String::new()),
            Outcome::Later => (EXIT_LATER, String::new()),
            Outcome::Changed => (EXIT_CHANGED, String::new()),
        }
    }

    /// What a process that exited with `code` and printed `out` came to; an
    /// error is the text to show.
    pub(super) fn parse(code: Option<i32>, out: &str, err: &str) -> Result<Outcome, String> {
        let words: Vec<&str> = out.split_whitespace().collect();
        let n = |i: usize| words.get(i).and_then(|w| w.parse::<u64>().ok());
        match (code, words.first().copied()) {
            (Some(0), Some("done")) => match (n(1), n(2), n(3)) {
                (Some(read), Some(wrote), Some(freed)) => Ok(Outcome::Done { read, wrote, freed }),
                _ => Err(format!("unexpected output {out:?}")),
            },
            (Some(0), Some("swept")) => {
                n(1).map(|total| Outcome::Swept { total }).ok_or_else(|| format!("unexpected output {out:?}"))
            }
            (Some(EXIT_NO_GAIN), _) => Ok(Outcome::NoGain),
            (Some(EXIT_LATER), _) => Ok(Outcome::Later),
            (Some(EXIT_CHANGED), _) => Ok(Outcome::Changed),
            (code, _) => {
                let said = err.trim();
                let how = code.map_or("a signal".to_string(), |c| format!("status {c}"));
                Err(if said.is_empty() {
                    format!("ended with {how}")
                } else {
                    said.lines().last().unwrap_or("").to_string()
                })
            }
        }
    }
}

/// Entry point of `warden static-compress`.
pub fn main() -> i32 {
    // Nothing the worker had open is wanted here (it holds the channel to the supervisor).
    crate::sys::close_inherited_fds();
    crate::sys::lower_priority();
    let spec: Result<Spec, String> = std::env::var("WARDEN_COMPRESS_JOB")
        .map_err(|e| e.to_string())
        .and_then(|j| serde_json::from_str(&j).map_err(|e| e.to_string()));
    let spec = match spec {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "warden static-compress: WARDEN_COMPRESS_JOB is missing or invalid ({e}); this command is run by the file server"
            );
            return 1;
        }
    };
    match Runner::new(&spec).and_then(|r| r.run(&spec.task)) {
        Ok(outcome) => {
            let (code, line) = outcome.report();
            if !line.is_empty() {
                println!("{line}");
            }
            code
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// A job's surroundings: the site, the store, and who wants the result.
pub(super) struct Runner {
    /// The root of the site, to open the files to compress.
    dir: OwnedFd,
    root: PathBuf,
    store: Store,
    parent: u32,
}

impl Runner {
    pub(super) fn new(spec: &Spec) -> std::io::Result<Runner> {
        let dir = OwnedFd::from(std::fs::File::open(&spec.root)?);
        let store = Store::open(&spec.store, &spec.root, spec.cap).map_err(std::io::Error::other)?;
        Ok(Runner { dir, root: spec.root.clone(), store, parent: spec.parent })
    }

    /// The worker that asked is gone: stop.
    fn abandoned(&self) -> bool {
        std::os::unix::process::parent_id() != self.parent
    }

    pub(super) fn run(&self, task: &Task) -> std::io::Result<Outcome> {
        if self.abandoned() {
            return Err(std::io::Error::other("the worker that asked for this is gone"));
        }
        match task {
            Task::Sweep => Ok(Outcome::Swept { total: self.store.sweep() }),
            Task::Compress { rel, version, min, max, jobs } => {
                let Some(_place) = self.place(*jobs) else {
                    return Err(std::io::Error::other("the worker that asked for this is gone"));
                };
                self.compress(rel, version, *min..=*max)
            }
        }
    }

    /// One of the `jobs` places for compressing at once: a lock file that is
    /// let go of when the guard is, or when this process ends. None when the
    /// worker is gone.
    fn place(&self, jobs: usize) -> Option<std::fs::File> {
        let mut round = 0u64;
        loop {
            if self.abandoned() {
                return None;
            }
            for i in 0..jobs.max(1) {
                let path = self.store.path.join(format!("place-{i}"));
                let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) else { continue };
                if crate::sys::try_lock_exclusive(&f).unwrap_or(false) {
                    return Some(f);
                }
            }
            // All taken. Look again soon, a little later each time.
            std::thread::sleep(Duration::from_millis(50 + round.min(10) * 20));
            round += 1;
        }
    }

    /// Make the copies of `version` of `rel`, if they are wanted and not there.
    fn compress(&self, rel: &str, version: &Version, sizes: std::ops::RangeInclusive<u64>) -> std::io::Result<Outcome> {
        let mut src = open_source(&self.dir, &self.root, rel)?;
        let v = Version::of(&src.meta);
        // Changed since the request that queued this: that request, or the
        // next, queues the version there is now.
        if v != *version {
            return Ok(Outcome::Changed);
        }
        if !src.meta.is_file() || !sizes.contains(&v.size()) || !v.quiet(SystemTime::now()) {
            return Ok(Outcome::Later);
        }
        let br = Entry::new(rel, &v, "br");
        let gz = Entry::new(rel, &v, "gz");
        let exists = |e: &Entry| self.store.path_of(e).exists();
        let nothing_to_do = || Outcome::Done { read: 0, wrote: 0, freed: 0 };
        if exists(&br) || exists(&gz) {
            return Ok(nothing_to_do()); // another worker was first
        }
        let folder = self.store.folder(&br)?;
        let (tmp_br, tmp_gz) = (
            folder.join(format!("{}br.tmp", br.version_prefix())),
            folder.join(format!("{}gz.tmp", br.version_prefix())),
        );
        // The brotli file doubles as the lock on this version.
        let br_file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&tmp_br)?;
        if !crate::sys::try_lock_exclusive(&br_file)? {
            return Ok(Outcome::Later);
        }
        let outcome = (|| {
            if exists(&br) || exists(&gz) {
                return Ok(nothing_to_do());
            }
            br_file.set_len(0)?;
            let (br_out, gz_out, read) = self.encode(&mut src.file, &v, br_file.try_clone()?, &tmp_gz)?;
            let mut wrote = 0;
            for (out, tmp, dest) in [(br_out, &tmp_br, &br), (gz_out, &tmp_gz, &gz)] {
                if out.written >= v.size() {
                    continue; // no gain: not kept
                }
                if let Some(t) = v.modified() {
                    out.file.set_modified(t)?;
                }
                out.file.sync_all()?;
                std::fs::rename(tmp, self.store.path_of(dest))?;
                wrote += out.written;
            }
            if wrote == 0 {
                return Ok(Outcome::NoGain);
            }
            let freed = self.store.prune(&br);
            Ok(Outcome::Done { read, wrote, freed })
        })();
        // Whatever was not put in its place goes.
        let _ = std::fs::remove_file(&tmp_br);
        let _ = std::fs::remove_file(&tmp_gz);
        outcome
    }

    /// Read `src` once, through both compressors, into the brotli file and a
    /// new gzip one at `tmp_gz`; check that the file did not change while it
    /// was read. Returns both outputs, and the bytes read.
    fn encode(
        &self,
        src: &mut std::fs::File,
        v: &Version,
        br_file: std::fs::File,
        tmp_gz: &Path,
    ) -> std::io::Result<(Counted, Counted, u64)> {
        let gz_file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(tmp_gz)?;
        let quality = if v.size() <= BEST_UP_TO { 11 } else { 9 };
        let mut br = brotli::CompressorWriter::new(Counted::new(br_file), CHUNK, quality, 22);
        let mut gz = flate2::write::GzEncoder::new(Counted::new(gz_file), flate2::Compression::new(9));
        let mut buf = vec![0u8; CHUNK];
        let mut read = 0u64;
        loop {
            if self.abandoned() {
                return Err(std::io::Error::other("the worker that asked for this is gone"));
            }
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            read += n as u64;
            br.write_all(&buf[..n])?;
            gz.write_all(&buf[..n])?;
        }
        let br_out = br.into_inner();
        let gz_out = gz.finish()?;
        if br_out.failed || gz_out.failed {
            return Err(std::io::Error::other("a copy could not be written"));
        }
        // The same file as when the job was queued, and the same as it was read.
        if read != v.size() || Version::of(&src.metadata()?) != *v {
            return Err(std::io::Error::other("the file changed while it was read"));
        }
        Ok((br_out, gz_out, read))
    }
}

/// A file written to that counts what it was given and remembers a failure
/// (a compressor that finishes by itself leaves no way to see one).
struct Counted {
    file: std::fs::File,
    written: u64,
    failed: bool,
}

impl Counted {
    fn new(file: std::fs::File) -> Counted {
        Counted { file, written: 0, failed: false }
    }
}

impl Write for Counted {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self.file.write(b) {
            Ok(n) => {
                self.written += n as u64;
                Ok(n)
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("warden-job-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("site")).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    fn runner(base: &Path) -> Runner {
        runner_of(base, std::os::unix::process::parent_id())
    }

    fn runner_of(base: &Path, parent: u32) -> Runner {
        let spec =
            Spec { parent, root: base.join("site"), store: base.join("copies"), cap: 256 << 20, task: Task::Sweep };
        Runner::new(&spec).unwrap()
    }

    fn text(n: usize) -> Vec<u8> {
        (0..n)
            .flat_map(|i| {
                format!(".rule-{i}{{color:#{:06x};margin:{}px}}\n", i * 7919 % 0xff_ffff, i % 40).into_bytes()
            })
            .collect()
    }

    fn noise(n: usize) -> Vec<u8> {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn version(r: &Runner, rel: &str) -> Version {
        Version::of(&open_source(&r.dir, &r.root, rel).unwrap().meta)
    }

    fn unbr(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        brotli::Decompressor::new(b, 4096).read_to_end(&mut out).unwrap();
        out
    }

    fn ungz(b: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(b).read_to_end(&mut out).unwrap();
        out
    }

    const ANY: std::ops::RangeInclusive<u64> = 1024..=8 << 20;

    /// A job makes both copies, which decode to the file and carry its
    /// modification time; a file that does not shrink, is too small, or was
    /// changed since the job was queued gets none.
    #[test]
    fn a_job_makes_copies_that_decode_to_the_file() {
        let base = tmp("copies");
        let site = base.join("site");
        let (css, bin) = (text(600), noise(5000));
        std::fs::write(site.join("a.css"), &css).unwrap();
        std::fs::write(site.join("noise.bin"), &bin).unwrap();
        std::fs::write(site.join("tiny.txt"), b"ten bytes!").unwrap();
        std::fs::write(site.join("moves.css"), text(300)).unwrap();
        let r = runner(&base);
        let (a, n, t, m) =
            (version(&r, "a.css"), version(&r, "noise.bin"), version(&r, "tiny.txt"), version(&r, "moves.css"));
        // Written a moment ago: the files are not quiet yet (and the job says so).
        assert_eq!(r.compress("a.css", &a, ANY).unwrap(), Outcome::Later);
        std::thread::sleep(Duration::from_millis(2100));

        let done = r.compress("a.css", &a, ANY).unwrap();
        let (br, gz) = (Entry::new("a.css", &a, "br"), Entry::new("a.css", &a, "gz"));
        let (br_path, gz_path) = (r.store.path_of(&br), r.store.path_of(&gz));
        let sizes = (std::fs::metadata(&br_path).unwrap().len(), std::fs::metadata(&gz_path).unwrap().len());
        assert_eq!(done, Outcome::Done { read: css.len() as u64, wrote: sizes.0 + sizes.1, freed: 0 });
        assert_eq!(unbr(&std::fs::read(&br_path).unwrap()), css);
        assert_eq!(ungz(&std::fs::read(&gz_path).unwrap()), css);
        let source_mtime = std::fs::metadata(site.join("a.css")).unwrap().modified().unwrap();
        for p in [&br_path, &gz_path] {
            let m = std::fs::metadata(p).unwrap();
            assert!(m.len() < css.len() as u64);
            assert_eq!(m.modified().unwrap(), source_mtime, "a copy carries the file's time");
        }
        // Nothing half-made is left, and a second job for the version does nothing.
        let folder = br_path.parent().unwrap();
        assert_eq!(std::fs::read_dir(folder).unwrap().count(), 2);
        let first = std::fs::metadata(&br_path).unwrap().modified().unwrap();
        assert_eq!(r.compress("a.css", &a, ANY).unwrap(), Outcome::Done { read: 0, wrote: 0, freed: 0 });
        assert_eq!(std::fs::metadata(&br_path).unwrap().modified().unwrap(), first);

        // Random bytes do not shrink: no copy.
        assert_eq!(r.compress("noise.bin", &n, ANY).unwrap(), Outcome::NoGain);
        assert!(
            r.store
                .path_of(&Entry::new("noise.bin", &n, "br"))
                .parent()
                .is_none_or(|d| std::fs::read_dir(d).unwrap().count() == 0)
        );
        // Too small for a copy, or too large.
        assert_eq!(r.compress("tiny.txt", &t, ANY).unwrap(), Outcome::Later);
        assert_eq!(r.compress("a.css", &a, 1024..=2000).unwrap(), Outcome::Later);
        assert!(!r.store.path_of(&Entry::new("tiny.txt", &t, "br")).exists());
        // Edited after the job was queued: the job is for a version that is gone.
        std::fs::write(site.join("moves.css"), text(310)).unwrap();
        assert_eq!(r.compress("moves.css", &m, ANY).unwrap(), Outcome::Changed);
        assert!(!r.store.path_of(&Entry::new("moves.css", &m, "br")).exists());
        // A file that is gone is a failure of the job.
        std::fs::remove_file(site.join("moves.css")).unwrap();
        assert!(r.compress("moves.css", &m, ANY).is_err());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A new version's copies replace the old ones, and the job says how much that freed.
    #[test]
    fn a_new_version_frees_the_old_copies() {
        let base = tmp("prune");
        let site = base.join("site");
        std::fs::write(site.join("a.css"), text(600)).unwrap();
        let r = runner(&base);
        std::thread::sleep(Duration::from_millis(2100));
        let v1 = version(&r, "a.css");
        let Outcome::Done { wrote: first, .. } = r.compress("a.css", &v1, ANY).unwrap() else { panic!() };
        std::fs::write(site.join("a.css"), text(700)).unwrap();
        std::thread::sleep(Duration::from_millis(2100));
        let v2 = version(&r, "a.css");
        let Outcome::Done { freed, .. } = r.compress("a.css", &v2, ANY).unwrap() else { panic!() };
        assert_eq!(freed, first);
        assert!(!r.store.path_of(&Entry::new("a.css", &v1, "br")).exists());
        assert!(r.store.path_of(&Entry::new("a.css", &v2, "br")).exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A file that is written to while it is read gives no copy.
    #[test]
    fn a_file_that_changes_while_it_is_read_gives_no_copy() {
        let base = tmp("changes");
        let site = base.join("site");
        std::fs::write(site.join("a.css"), text(600)).unwrap();
        let r = runner(&base);
        let v = version(&r, "a.css");
        let mut src = open_source(&r.dir, &r.root, "a.css").unwrap();
        // The version is no longer the file's by the time it has been read.
        let mut f = std::fs::OpenOptions::new().append(true).open(site.join("a.css")).unwrap();
        f.write_all(b"a{}").unwrap();
        let out = std::fs::File::create(base.join("t.br")).unwrap();
        let err = r.encode(&mut src.file, &v, out, &base.join("t.gz")).err().unwrap();
        assert!(err.to_string().contains("changed"), "{err}");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A job whose worker is gone does nothing (and a pass over the store is a job too).
    #[test]
    fn a_job_of_a_worker_that_is_gone_stops() {
        let base = tmp("gone");
        std::fs::write(base.join("site/a.css"), text(600)).unwrap();
        let r = runner_of(&base, 1_999_999_999);
        let v = version(&r, "a.css");
        let task = Task::Compress { rel: "a.css".into(), version: v, min: 1024, max: 8 << 20, jobs: 1 };
        assert!(r.run(&task).err().unwrap().to_string().contains("gone"));
        assert!(r.run(&Task::Sweep).is_err());
        assert!(
            std::fs::read_dir(base.join("copies")).unwrap().all(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("place"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// At most `compress_jobs` compressions run at once, however many
    /// processes there are: the places are lock files.
    #[test]
    fn only_as_many_compressions_run_at_once_as_there_are_places() {
        let base = tmp("places");
        let r = std::sync::Arc::new(runner(&base));
        let (first, second) = (r.place(2).unwrap(), r.place(2).unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        let waiting = {
            let r = r.clone();
            std::thread::spawn(move || {
                let p = r.place(2);
                tx.send(p.is_some()).unwrap();
                p
            })
        };
        assert!(rx.recv_timeout(Duration::from_millis(400)).is_err(), "a third place was given");
        drop(first);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(true));
        let third = waiting.join().unwrap();
        drop((second, third));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What a process says about its job is read back as it was meant.
    #[test]
    fn outcomes_survive_the_trip_through_a_process() {
        let all = [
            Outcome::Done { read: 10, wrote: 4, freed: 3 },
            Outcome::Done { read: 0, wrote: 0, freed: 0 },
            Outcome::Swept { total: 77 },
            Outcome::NoGain,
            Outcome::Later,
            Outcome::Changed,
        ];
        for o in all {
            let (code, line) = o.report();
            assert_eq!(Outcome::parse(Some(code), &line, ""), Ok(o));
        }
        // Anything else is a failure, and says why when it can.
        assert_eq!(Outcome::parse(Some(1), "", "boom\nthe file is gone\n"), Err("the file is gone".into()));
        assert_eq!(Outcome::parse(Some(1), "", ""), Err("ended with status 1".into()));
        assert_eq!(Outcome::parse(None, "", ""), Err("ended with a signal".into()));
        assert!(Outcome::parse(Some(0), "done x", "").is_err());
        assert!(Outcome::parse(Some(0), "", "").is_err());
    }
}

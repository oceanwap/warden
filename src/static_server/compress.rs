//! Compressing files in the background, the worker's side. A request that
//! finds no compressed copy of the file it serves sends the file as it is and
//! queues a job for it; the jobs run in child processes at the lowest priority
//! (`job.rs` does the work and says why a process), a few at a time, and the
//! next request is answered from the copies they leave in the store
//! (`store.rs` says where they are and how a request finds them).
//!
//! The worker itself stays one thread: a request costs the same with or
//! without this, nothing a compressor needs (its memory, its CPU, a failure) is
//! in the process that serves, and a job runs with a `tokio` task of the worker
//! waiting for it, not a thread.
//!
//! The work is bounded: the jobs queued (further requests do not queue more),
//! the processes of one worker, and, for all the workers of a site together,
//! the compressions running at once (`compress_jobs`; `job.rs`). A job that
//! ended without a copy is not tried again for a while (`Known`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::{Notify, watch};

use super::job::{Outcome, Spec, Task};
use super::store::{Store, Version};
use crate::config::Static;

/// Jobs waiting. Past this, requests for more files wait for their turn
/// (they are queued again by the next request).
const QUEUE_MAX: usize = 1024;

/// What is remembered of versions that have been dealt with: past this, the
/// remembered are forgotten (it costs a repeated attempt at worst).
const KNOWN_MAX: usize = 16_384;

/// How long a version is left alone after a job ended without a copy of it.
const AFTER_NO_GAIN: Duration = Duration::from_secs(3600);
const AFTER_FAILURE: Duration = Duration::from_secs(60);
const AFTER_CONTENTION: Duration = Duration::from_secs(10);
const AFTER_CHANGE: Duration = Duration::from_secs(1);

/// A job that takes longer than this (a file on a stuck network share) is
/// given up on and its process killed.
const JOB_LIMIT: Duration = Duration::from_secs(900);

/// What is known of a version.
#[derive(Clone, Copy)]
enum Known {
    /// A job for it is queued or running.
    Pending,
    /// No job for it until then (one just ended without making what was
    /// asked for, or made it already).
    Pause(Instant),
    /// No copy of it, and none to be made until then: it does not shrink, or
    /// has a sibling file of its own. Requests do not look for one.
    Skip(Instant),
}

impl Known {
    /// A job for the version may not be queued now.
    fn blocks_a_job(&self, now: Instant) -> bool {
        match self {
            Known::Pending => true,
            Known::Pause(t) | Known::Skip(t) => now < *t,
        }
    }
}

struct Job {
    rel: String,
    version: Version,
}

struct State {
    queue: VecDeque<Job>,
    known: HashMap<Version, Known>,
    /// Processes running a compression.
    running: usize,
    /// A pass over the store is running.
    sweeping: bool,
}

/// What a worker did, for the line it prints when it stops.
#[derive(Default)]
struct Stats {
    made: AtomicU64,
    read: AtomicU64,
    wrote: AtomicU64,
    failed: AtomicU64,
}

pub(super) struct Compressor {
    store: Store,
    /// The executable the jobs run (`warden static-compress`).
    exe: PathBuf,
    root: PathBuf,
    cap: u64,
    min: u64,
    max: u64,
    jobs: usize,
    state: Mutex<State>,
    /// A job was queued, or one ended and its place is free.
    wake: Notify,
    stop: watch::Sender<bool>,
    stats: Stats,
}

impl Compressor {
    /// Ready to compress for the site at `root`, keeping copies in
    /// `cfg.compress_dir`, with jobs run by `exe`. `run` does the work.
    pub(super) fn start(cfg: &Static, root: &Path, exe: PathBuf) -> Result<Arc<Compressor>, String> {
        let path = cfg.compress_dir.as_deref().ok_or("static.compress_dir is not set")?;
        let store = Store::open(path, root, cfg.compress_dir_size)?;
        let jobs = match cfg.compress_jobs {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()),
            n => n as usize,
        };
        Ok(Arc::new(Compressor {
            store,
            exe,
            root: root.to_path_buf(),
            cap: cfg.compress_dir_size,
            min: cfg.compress_min_file,
            max: cfg.compress_max_file,
            jobs,
            state: Mutex::new(State { queue: VecDeque::new(), known: HashMap::new(), running: 0, sweeping: false }),
            wake: Notify::new(),
            stop: watch::channel(false).0,
            stats: Stats::default(),
        }))
    }

    pub(super) fn store(&self) -> &Store {
        &self.store
    }

    pub(super) fn jobs(&self) -> usize {
        self.jobs
    }

    /// A file of this size is worth a copy.
    pub(super) fn wants(&self, size: u64) -> bool {
        (self.min..=self.max).contains(&size)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether it is worth looking for a copy of this version: not when it is
    /// known to have none and to get none (`Known::Skip`).
    pub(super) fn expects(&self, v: &Version) -> bool {
        match self.lock().known.get(v) {
            Some(Known::Skip(t)) => Instant::now() >= *t,
            _ => true,
        }
    }

    /// Leave this version alone for `secs`: its file has a compressed sibling
    /// of its own, say, so a copy is not wanted, and requests need not look
    /// for one.
    pub(super) fn leave(&self, v: Version, secs: u64) {
        let mut st = self.lock();
        Self::note(&mut st, v, Known::Skip(Instant::now() + Duration::from_secs(secs)));
    }

    fn note(st: &mut State, v: Version, k: Known) {
        if st.known.len() >= KNOWN_MAX {
            st.known.retain(|_, k| matches!(k, Known::Pending));
        }
        st.known.insert(v, k);
    }

    /// Queue a job for `rel`, unless there is one already or the queue is full.
    pub(super) fn submit(&self, rel: &str, v: Version) {
        // A file that was just written is not compressed yet: a later request will ask.
        if !v.quiet(SystemTime::now()) {
            return;
        }
        let mut st = self.lock();
        let busy = st.known.get(&v).is_some_and(|k| k.blocks_a_job(Instant::now()));
        if busy || st.queue.len() >= QUEUE_MAX {
            return;
        }
        Self::note(&mut st, v, Known::Pending);
        st.queue.push_back(Job { rel: rel.to_string(), version: v });
        drop(st);
        self.wake.notify_one();
    }

    /// The next job, if there is a place for it.
    fn take(&self) -> Option<Job> {
        let mut st = self.lock();
        if st.running >= self.jobs {
            return None;
        }
        let job = st.queue.pop_front()?;
        st.running += 1;
        Some(job)
    }

    /// Start the jobs as they are queued, until `stop`. Run as a task of the
    /// worker.
    pub(super) async fn run(self: Arc<Self>) {
        self.clone().sweep();
        let mut stopped = self.stop.subscribe();
        loop {
            let job = loop {
                if *stopped.borrow() {
                    return;
                }
                if let Some(job) = self.take() {
                    break job;
                }
                tokio::select! {
                    _ = self.wake.notified() => {}
                    _ = stopped.changed() => {}
                }
            };
            let me = self.clone();
            tokio::spawn(async move {
                let task = Task::Compress {
                    rel: job.rel.clone(),
                    version: job.version,
                    min: me.min,
                    max: me.max,
                    jobs: me.jobs,
                };
                let result = me.child(task).await;
                me.finished(job, result);
                me.wake.notify_one();
            });
        }
    }

    /// Run a pass over the store, unless one is running.
    fn sweep(self: Arc<Self>) {
        {
            let mut st = self.lock();
            if st.sweeping {
                return;
            }
            st.sweeping = true;
        }
        tokio::spawn(async move {
            match self.child(Task::Sweep).await {
                Ok(Outcome::Swept { total }) => self.store.recount(total),
                Ok(_) => {}
                Err(e) => self.complain("looking over the copies", &e),
            }
            self.lock().sweeping = false;
        });
    }

    /// Run `task` in a process, and say what came of it.
    async fn child(&self, task: Task) -> Result<Outcome, String> {
        let spec = Spec {
            parent: std::process::id(),
            root: self.root.clone(),
            store: self.store.path.clone(),
            cap: self.cap,
            task,
        };
        let json = serde_json::to_string(&spec).map_err(|e| e.to_string())?;
        let child = tokio::process::Command::new(&self.exe)
            .arg("static-compress")
            .env_clear()
            .env("WARDEN_COMPRESS_JOB", json)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropped (a stop, the limit) means killed.
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", self.exe.display()))?;
        let mut stopped = self.stop.subscribe();
        let out = tokio::select! {
            r = tokio::time::timeout(JOB_LIMIT, child.wait_with_output()) => match r {
                Ok(Ok(out)) => out,
                Ok(Err(e)) => return Err(e.to_string()),
                Err(_) => return Err(format!("took more than {} s", JOB_LIMIT.as_secs())),
            },
            _ = stopped.wait_for(|s| *s) => return Err("stopped".into()),
        };
        Outcome::parse(out.status.code(), &String::from_utf8_lossy(&out.stdout), &String::from_utf8_lossy(&out.stderr))
    }

    /// Say a few failures, then count them in silence.
    fn complain(&self, what: &str, why: &str) {
        if *self.stop.borrow() {
            return;
        }
        if self.stats.failed.fetch_add(1, Ordering::Relaxed) < 3 {
            eprintln!("warden serve-static: {what}: {why}");
        }
    }

    /// A job's process has ended.
    fn finished(self: &Arc<Self>, job: Job, result: Result<Outcome, String>) {
        let v = job.version;
        let now = Instant::now();
        let mut due = false;
        let known = match &result {
            Ok(Outcome::Done { read, wrote, freed }) => {
                self.stats.read.fetch_add(*read, Ordering::Relaxed);
                self.stats.wrote.fetch_add(*wrote, Ordering::Relaxed);
                if *wrote > 0 {
                    self.stats.made.fetch_add(1, Ordering::Relaxed);
                }
                due = (*wrote > 0 || *freed > 0) && self.store.account(*wrote, *freed);
                // Copies made: they are found by lookup. Nothing made: they were there
                // already, or not in the encoding that was asked for; once in a while is enough.
                (*wrote == 0).then_some(Known::Pause(now + AFTER_CONTENTION))
            }
            Ok(Outcome::NoGain) => Some(Known::Skip(now + AFTER_NO_GAIN)),
            Ok(Outcome::Later) => Some(Known::Pause(now + AFTER_CONTENTION)),
            Ok(Outcome::Changed) => Some(Known::Pause(now + AFTER_CHANGE)),
            Ok(Outcome::Swept { .. }) => None,
            Err(e) => {
                self.complain(&format!("compressing {}", job.rel), e);
                Some(Known::Pause(now + AFTER_FAILURE))
            }
        };
        {
            let mut st = self.lock();
            st.running -= 1;
            match known {
                None => {
                    st.known.remove(&v);
                }
                Some(k) => Self::note(&mut st, v, k),
            }
        }
        if due {
            self.clone().sweep();
        }
    }

    /// Stop the jobs (their processes are killed; what they were making is
    /// left for the next pass over the store to remove) and wait for them to
    /// be gone.
    pub(super) async fn stop(&self) {
        let _ = self.stop.send(true);
        for _ in 0..100 {
            if self.lock().running == 0 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// What was done, for the line printed when the worker stops.
    pub(super) fn report(&self) -> String {
        format!(
            "compressed {} files in the background ({} KB read, {} KB written; {} failed), {} KB of copies kept",
            self.stats.made.load(Ordering::Relaxed),
            self.stats.read.load(Ordering::Relaxed) >> 10,
            self.stats.wrote.load(Ordering::Relaxed) >> 10,
            self.stats.failed.load(Ordering::Relaxed),
            self.store.used() >> 10
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("warden-compress-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("site")).unwrap();
        std::fs::canonicalize(&p).unwrap()
    }

    /// A compressor whose jobs are `body`, a shell script (what a job does and says).
    fn start(base: &Path, jobs: u32, body: &str) -> Arc<Compressor> {
        let exe = base.join("job.sh");
        std::fs::write(&exe, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        compressor(base, jobs, exe)
    }

    fn compressor(base: &Path, jobs: u32, exe: PathBuf) -> Arc<Compressor> {
        let cfg: Static = serde_json::from_str(&format!(
            r#"{{"root": {:?}, "compress_dir": {:?}, "compress_jobs": {jobs}}}"#,
            base.join("site"),
            base.join("copies")
        ))
        .unwrap();
        Compressor::start(&cfg, &base.join("site"), exe).unwrap()
    }

    /// How long no job is queued for `v`, if that is so.
    fn paused_for(c: &Compressor, v: &Version) -> Option<Duration> {
        match c.lock().known.get(v) {
            Some(Known::Pause(t) | Known::Skip(t)) => Some(t.saturating_duration_since(Instant::now())),
            _ => None,
        }
    }

    async fn until(what: &str, mut f: impl FnMut() -> bool) {
        let t0 = Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(20), "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Requests queue jobs without bound on the queue or the versions
    /// remembered; a version is queued once.
    #[test]
    fn the_queue_and_the_versions_known_are_bounded() {
        let base = tmp("bounds");
        let c = start(&base, 2, "exit 0");
        assert_eq!(c.jobs(), 2);
        for n in 0..(QUEUE_MAX as u64 + 50) {
            c.submit("a.css", Version::fake(n));
        }
        assert_eq!(c.lock().queue.len(), QUEUE_MAX);
        // The same version is queued once, and is not expected to have a copy while it is.
        let before = c.lock().queue.len();
        c.submit("a.css", Version::fake(0));
        assert_eq!(c.lock().queue.len(), before);
        assert!(c.expects(&Version::fake(0)), "a copy may be made any moment: lookups go on");
        assert!(c.expects(&Version::fake(QUEUE_MAX as u64 + 7)), "never queued: nothing is known of it");
        // A file written a moment ago is not queued.
        let fresh = Version::of(&std::fs::metadata(&base).unwrap());
        c.submit("fresh", fresh);
        assert!(c.expects(&fresh));
        // Versions that have a sibling file are not looked for in the store, until the time is up.
        c.leave(Version::fake(9_000_000), 3600);
        assert!(!c.expects(&Version::fake(9_000_000)));
        c.leave(Version::fake(9_000_001), 0);
        assert!(c.expects(&Version::fake(9_000_001)));
        // What is remembered is bounded: over the limit, what is settled is
        // forgotten, and what is queued is not.
        for n in 0..(KNOWN_MAX as u64 + 10) {
            c.leave(Version::fake(10_000_000 + n), 3600);
        }
        assert!(c.lock().known.len() < KNOWN_MAX, "{}", c.lock().known.len());
        assert!(c.lock().known.contains_key(&Version::fake(0)), "and what is queued is kept in mind");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Never more processes than `compress_jobs`, however many jobs are
    /// queued; each job's result is counted, and so is the pass over the store
    /// made at the start.
    #[tokio::test(flavor = "current_thread")]
    async fn at_most_as_many_processes_as_jobs_and_every_job_is_run() {
        let base = tmp("pool");
        let log = base.join("log");
        // Passes over the store print `swept`; compressions take a moment and print `done`.
        let c = start(
            &base,
            2,
            &format!(
                "case \"$WARDEN_COMPRESS_JOB\" in *Sweep*) echo 'swept 5000'; exit 0;; esac\n\
                 echo $$ >> {log:?}; sleep 0.3; echo 'done 1000 300 100'"
            ),
        );
        tokio::spawn(c.clone().run());
        let versions: Vec<Version> = (0..6).map(Version::fake).collect();
        for v in &versions {
            c.submit("a.css", *v);
        }
        let mut most = 0;
        until("six jobs run", || {
            most = most.max(c.lock().running);
            c.stats.made.load(Ordering::Relaxed) == 6
        })
        .await;
        assert_eq!(most, 2, "two at a time: the limit, and used");
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 6);
        // Done: no longer pending, and the sizes are counted (5000 found, 6 x (300 - 100) since).
        assert!(versions.iter().all(|v| c.expects(v)));
        assert_eq!(
            c.report(),
            "compressed 6 files in the background (5 KB read, 1 KB written; 0 failed), 6 KB of copies kept"
        );
        c.stop().await;
        let _ = std::fs::remove_dir_all(&base);
    }

    /// What a job's process says decides how long its version is left alone.
    #[tokio::test(flavor = "current_thread")]
    async fn a_version_is_left_alone_for_as_long_as_the_job_says() {
        let base = tmp("pauses");
        // The job's version is in its spec; the script answers by its inode number.
        let c = start(
            &base,
            1,
            "case \"$WARDEN_COMPRESS_JOB\" in *Sweep*) echo 'swept 0'; exit 0;; esac\n\
             case \"$WARDEN_COMPRESS_JOB\" in\n\
               *'\"ino\":1,'*) exit 3;;\n\
               *'\"ino\":2,'*) exit 4;;\n\
               *'\"ino\":3,'*) exit 5;;\n\
               *'\"ino\":4,'*) echo 'it broke' >&2; exit 1;;
               *'\"ino\":6,'*) echo 'done 0 0 0';;\n\
               *) echo 'done 10 5 0';;\n\
             esac",
        );
        tokio::spawn(c.clone().run());
        let v: Vec<Version> = (1..=6).map(Version::fake).collect();
        for x in &v {
            c.submit("a.css", *x);
        }
        until("the jobs end", || {
            let st = c.lock();
            st.running == 0 && st.queue.is_empty() && v.iter().all(|x| !matches!(st.known.get(x), Some(Known::Pending)))
        })
        .await;
        let about = |d: Option<Duration>, secs: u64| {
            d.is_some_and(|d| d <= Duration::from_secs(secs) && d + Duration::from_secs(5) >= Duration::from_secs(secs))
        };
        assert!(about(paused_for(&c, &v[0]), 3600), "no gain: an hour");
        assert!(about(paused_for(&c, &v[1]), 10), "not now: ten seconds");
        assert!(paused_for(&c, &v[2]).is_some_and(|d| d <= Duration::from_secs(1)), "changed: a second");
        assert!(about(paused_for(&c, &v[3]), 60), "failed: a minute");
        assert!(paused_for(&c, &v[4]).is_none() && c.expects(&v[4]), "done: nothing to wait for");
        // Only a version that has no copy and gets none is not looked for in the store.
        assert!(!c.expects(&v[0]), "no gain");
        assert!(v[1..].iter().all(|x| c.expects(x)), "the others may have copies, or soon");
        // A job that found the copies there already, or made none, is not repeated at once,
        // and the copies that are there are still looked for.
        assert!(about(paused_for(&c, &v[5]), 10) && c.expects(&v[5]), "nothing made: ten seconds");
        assert_eq!(c.stats.failed.load(Ordering::Relaxed), 1);
        c.stop().await;
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A stop kills the jobs that are running.
    #[tokio::test(flavor = "current_thread")]
    async fn stopping_kills_the_jobs_that_are_running() {
        let base = tmp("stop");
        let pids = base.join("pids");
        let c = start(
            &base,
            1,
            &format!(
                "case \"$WARDEN_COMPRESS_JOB\" in *Sweep*) echo 'swept 0'; exit 0;; esac\necho $$ > {pids:?}; exec sleep 60"
            ),
        );
        tokio::spawn(c.clone().run());
        c.submit("a.css", Version::fake(1));
        until("the job started", || std::fs::read_to_string(&pids).is_ok_and(|s| !s.trim().is_empty())).await;
        let pid: i32 = std::fs::read_to_string(&pids).unwrap().trim().parse().unwrap();
        let t0 = Instant::now();
        c.stop().await;
        assert!(t0.elapsed() < Duration::from_secs(2));
        assert_eq!(c.lock().running, 0);
        // Signal 0 only asks whether the process exists.
        until("the process is gone", || crate::sys::kill(pid, 0).is_err()).await;
        // A stop is not a failure to report.
        assert_eq!(c.stats.failed.load(Ordering::Relaxed), 0);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An executable that is not there is a failure of the job, not of the worker.
    #[tokio::test(flavor = "current_thread")]
    async fn a_job_that_cannot_start_is_a_failure_and_nothing_more() {
        let base = tmp("nostart");
        let c = compressor(&base, 1, base.join("missing"));
        tokio::spawn(c.clone().run());
        c.submit("a.css", Version::fake(1));
        until("the job ends", || c.stats.failed.load(Ordering::Relaxed) >= 2 && c.lock().running == 0).await;
        assert!(paused_for(&c, &Version::fake(1)).is_some());
        c.stop().await;
        let _ = std::fs::remove_dir_all(&base);
    }
}

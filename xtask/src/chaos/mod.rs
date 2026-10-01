//! `cargo xtask chaos`: a chaos soak of a real fleet (docs/chaos.md).
//!
//! It starts a throwaway fleet (Bun and Node apps in process mode, one with
//! a hot standby and one with surge rollouts, a Bun worker-mode app, a
//! static site, an app with `worker_output = "direct"`, WebSocket and SSE
//! clients, wardend), puts steady client load on it, and for `--minutes`
//! injects faults picked by a seeded RNG: kills, freezes, rollouts on top of
//! rollouts, bad configs, release swaps, log floods, a full disk, a crash
//! loop. After each fault it waits for the whole fleet to be ready again.
//! Throughout, it checks the invariants (see `report.rs`), prints a summary,
//! writes JSON to `bench/results/`, and exits non-zero on any violation with
//! the seed and the log lines around it.
//!
//! As root on Linux it runs itself in its own pid and mount namespace
//! (`unshare`, with bash as the namespace's init): background supervisors
//! are orphans by design, and an init that reaps them (here bash) is what a
//! host or `docker run --init` provides; the log directory's tmpfs (for the
//! full-disk fault) disappears with the namespace.

mod cgroup;
mod faults;
mod fleet;
mod load;
mod monitor;
mod procfs;
mod report;

use fleet::{AppSpec, Fleet};
use load::{Client, ErrClass, Failure, Session};
use monitor::Mon;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub const USAGE: &str = "\
cargo xtask chaos [OPTIONS]

Runs a fleet (Bun and Node apps, worker mode, a static site, direct output,
WebSockets and SSE, wardend) in a throwaway WARDEN_HOME under steady client
load, injects random faults, and checks that nothing breaks beyond what is
documented. Exit 1 on any invariant violation, with the seed to reproduce it.

OPTIONS:
    --minutes N        How long faults are injected (default 10; fractions work)
    --seed S           RNG seed (default: from the clock; printed at start)
    --only FAULTS      Comma-separated subset of the faults below
    --bound S          Seconds every app gets to be ready again after a fault (default 60)
    --release          Build and run Warden in release mode (default: debug)
    --warden PATH      Use this warden binary instead of building one
    --no-namespace     Do not run in an own pid/mount namespace (no tmpfs: no disk-full fault)
    --keep             Keep the run directory (always kept when something failed)
    --out FILE         Where to write the JSON (default bench/results/chaos-<time>-seed<S>.json)
    -h, --help         This help

FAULTS: kill-worker, kill-standby, kill-supervisor, kill-wardend, stop-worker,
stop-supervisor, stop-wardend, reload, safe-reload, restart, restart-hard,
scale, overlap, bad-config, release-swap, throw-thread, log-flood, disk-full,
crash-loop, oom-kill, memory-recycle

NEEDS: Linux or macOS, bun, node >= 22.12. Linux, as root: the namespace, the
tmpfs (disk-full) and a memory cgroup (oom-kill). npm, once, for the NestJS
app's packages (bench/nest). What a host lacks is skipped, and the run says why.
";

// -------------------------------------------------------------------- rng

/// SplitMix64: small, seedable, good enough to pick faults.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// 0..n (0 when n is 0).
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { (self.next_u64() % n as u64) as usize }
    }
    pub fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * ((self.next_u64() >> 11) as f64 / (1u64 << 53) as f64)
    }
    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = self.below(i + 1);
            v.swap(i, j);
        }
    }
}

// ----------------------------------------------------------------- shared

/// Workers killed on purpose (by the harness, or hung so the watchdog
/// kills them): what they had in flight may be lost.
#[derive(Debug, Clone, Default)]
pub struct Unplanned {
    pids: HashSet<u32>,
    /// `<pid>:<thread>` of single Worker threads that died.
    threads: HashSet<String>,
}

impl Unplanned {
    pub fn covers(&self, who: &str) -> bool {
        self.threads.contains(who) || load::who_pid(who).is_some_and(|p| self.pids.contains(&p))
    }
}

#[derive(Debug, Clone, Default)]
pub struct Rec {
    pub ok: BTreeMap<(String, Client), u64>,
    pub failures: Vec<Failure>,
    pub sessions: Vec<Session>,
}

/// Failures kept for the report; past this only the counts grow.
const MAX_FAILURES: usize = 100_000;

/// The memory cgroup of the `oom` app: its supervisor and 2 Bun workers
/// (~150 MB with a rollout's extra worker) fit with room to spare; the
/// oom-kill fault makes one worker grow twice past it.
const OOM_LIMIT_MB: u64 = 400;

pub struct Shared {
    t0: Instant,
    pub wall0_ms: i64,
    stop: AtomicBool,
    /// No fault in progress and the fleet recovered: fd/RSS samples taken
    /// now count for the trend.
    pub quiet: AtomicBool,
    rec: Mutex<Rec>,
    mon: Mutex<Mon>,
    un: Mutex<Unplanned>,
    frozen: Mutex<Vec<(f64, f64)>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    fn new() -> Shared {
        let wall0_ms =
            SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
        Shared {
            t0: Instant::now(),
            wall0_ms,
            stop: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
            rec: Mutex::new(Rec::default()),
            mon: Mutex::new(Mon::default()),
            un: Mutex::new(Unplanned::default()),
            frozen: Mutex::new(Vec::new()),
        }
    }
    /// Seconds since the run started.
    pub fn now(&self) -> f64 {
        self.t0.elapsed().as_secs_f64()
    }
    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }
    pub fn ok(&self, app: &str, client: Client) {
        *lock(&self.rec).ok.entry((app.to_string(), client)).or_default() += 1;
    }
    pub fn fail(&self, app: &str, client: Client, class: ErrClass, who: Option<String>, fresh: bool, detail: String) {
        let f = Failure { t: self.now(), app: app.to_string(), client, class, who, fresh, detail };
        let mut r = lock(&self.rec);
        if r.failures.len() < MAX_FAILURES {
            r.failures.push(f);
        }
    }
    pub fn session(&self, s: Session) {
        lock(&self.rec).sessions.push(s);
    }
    pub fn with_mon(&self, f: impl FnOnce(&mut Mon)) {
        f(&mut lock(&self.mon));
    }
    pub fn doom_pid(&self, pid: u32) {
        lock(&self.un).pids.insert(pid);
    }
    pub fn doom_thread(&self, who: &str) {
        lock(&self.un).threads.insert(who.to_string());
    }
    /// A supervisor frozen from `start` to `end` (∞ while it still is).
    pub fn frozen(&self, start: f64, end: f64) {
        let mut v = lock(&self.frozen);
        match v.iter_mut().find(|w| w.0 == start) {
            Some(w) => w.1 = end,
            None => v.push((start, end)),
        }
    }
    pub fn snapshot(&self) -> (Rec, Mon, Unplanned, Vec<(f64, f64)>) {
        (lock(&self.rec).clone(), lock(&self.mon).clone(), lock(&self.un).clone(), lock(&self.frozen).clone())
    }
}

// ---------------------------------------------------------------- options

struct Opts {
    minutes: f64,
    seed: Option<u64>,
    only: Option<Vec<&'static str>>,
    bound: u64,
    release: bool,
    warden: Option<PathBuf>,
    no_ns: bool,
    keep: bool,
    out: Option<PathBuf>,
}

fn parse(args: &[String]) -> Result<Opts, String> {
    let mut o = Opts {
        minutes: 10.0,
        seed: None,
        only: None,
        bound: 60,
        release: false,
        warden: None,
        no_ns: false,
        keep: false,
        out: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| it.next().cloned().ok_or(format!("{a} needs {what}"));
        match a.as_str() {
            "--minutes" => {
                let v = val("a number of minutes")?;
                o.minutes = v
                    .parse()
                    .ok()
                    .filter(|m: &f64| *m > 0.0 && *m <= 24.0 * 60.0)
                    .ok_or(format!("--minutes {v:?}: a number of minutes"))?;
            }
            "--seed" => {
                let v = val("a number")?;
                o.seed = Some(v.parse().map_err(|_| format!("--seed {v:?} is not a number"))?);
            }
            "--bound" => {
                let v = val("seconds")?;
                o.bound = v.parse().ok().filter(|b| *b > 0).ok_or(format!("--bound {v:?}: seconds"))?;
            }
            "--only" => {
                let v = val("a list of faults")?;
                let mut kinds = Vec::new();
                for k in v.split(',').map(str::trim) {
                    let Some(known) = faults::KINDS.iter().find(|x| **x == k) else {
                        return Err(format!("unknown fault {k:?}; one of {}", faults::KINDS.join(", ")));
                    };
                    kinds.push(*known);
                }
                o.only = Some(kinds);
            }
            "--release" => o.release = true,
            "--warden" => o.warden = Some(PathBuf::from(val("a path")?)),
            "--out" => o.out = Some(PathBuf::from(val("a path")?)),
            "--no-namespace" => o.no_ns = true,
            "--keep" => o.keep = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    Ok(o)
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok_and(|o| o.status.success())
}

fn build(root: &Path, release: bool) -> Result<PathBuf, String> {
    let profile = if release { "release" } else { "debug" };
    eprintln!("chaos: building Warden ({profile})");
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut args = vec!["build", "--package", "warden"];
    if release {
        args.push("--release");
    }
    crate::run(Command::new(&cargo).args(&args).current_dir(root), &format!("cargo {}", args.join(" ")))?;
    let target = std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("target"));
    let bin = target.join(profile).join("warden");
    if bin.is_file() { Ok(bin) } else { Err(format!("{} was not built", bin.display())) }
}

/// What the binary under test is: "release", "debug", or its path.
fn build_label(bin: &Path) -> String {
    let parent = bin.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().to_string());
    match parent.as_deref() {
        Some("release") => "release".into(),
        Some("debug") => "debug".into(),
        _ => bin.display().to_string(),
    }
}

/// The NestJS app's packages in `bench/nest`: there, or installed now
/// (`npm ci`, as `cargo xtask bench` does). Err: why the app is left out.
fn nest_ready(dir: &Path) -> Result<(), String> {
    let core = dir.join("node_modules/@nestjs/core/package.json");
    if core.is_file() {
        return Ok(());
    }
    if !have("npm") {
        return Err(format!("{} is missing and npm is not installed (npm ci in bench/nest)", core.display()));
    }
    eprintln!("chaos: installing the NestJS app's packages (npm ci in bench/nest)");
    crate::run(Command::new("npm").args(["ci", "--no-audit", "--no-fund"]).current_dir(dir), "npm ci in bench/nest")?;
    if core.is_file() { Ok(()) } else { Err(format!("npm ci ran, but {} is still missing", core.display())) }
}

pub fn main(args: &[String], root: &Path) -> Result<(), String> {
    let o = parse(args)?;
    if !cfg!(any(target_os = "linux", target_os = "macos")) {
        return Err("the chaos soak runs on Linux (all of it) and macOS (without the Linux-only faults)".into());
    }
    if !have("bun") || !have("node") {
        return Err("the chaos soak needs bun and node (22.12 or newer) on PATH".into());
    }
    let in_ns = std::env::var_os("WARDEN_CHAOS_NS").is_some();
    if cfg!(target_os = "linux") && !in_ns && !o.no_ns && procfs::is_root() && have("unshare") && have("bash") {
        let bin = match &o.warden {
            Some(b) => b.clone(),
            None => build(root, o.release)?,
        };
        let exe = std::env::current_exe().map_err(|e| format!("finding the xtask binary: {e}"))?;
        // bash as the namespace's init reaps orphans; `"$@"; exit $?` keeps
        // it from exec-ing the harness in its place.
        let st = Command::new("unshare")
            .args(["--pid", "--fork", "--mount-proc", "--kill-child", "bash", "-c", "\"$@\"; exit $?", "bash"])
            .arg(exe)
            .arg("chaos")
            .args(args)
            .arg("--warden")
            .arg(&bin)
            .env("WARDEN_CHAOS_NS", "1")
            .status()
            .map_err(|e| format!("running unshare: {e}"))?;
        return if st.success() { Ok(()) } else { Err(format!("the chaos soak failed ({st}); the report is above")) };
    }
    let bin = match &o.warden {
        Some(b) => b.clone(),
        None => build(root, o.release)?,
    };
    run(&o, root, &bin, in_ns)
}

// -------------------------------------------------------------------- run

fn copy(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to).map(|_| ()).map_err(|e| format!("copying {} to {}: {e}", from.display(), to.display()))
}

fn mkdir(p: &Path) -> Result<(), String> {
    std::fs::create_dir_all(p).map_err(|e| format!("creating {}: {e}", p.display()))
}

fn write(p: &Path, text: &str) -> Result<(), String> {
    std::fs::write(p, text).map_err(|e| format!("writing {}: {e}", p.display()))
}

/// The run directory: apps, releases, a site, configs, the log tmpfs.
fn prepare(root: &Path, home: &Path, in_ns: bool) -> Result<bool, String> {
    let _ = std::fs::remove_dir_all(home);
    for d in ["apps", "run", "logs", "releases/v1", "releases/v2", "site/assets"] {
        mkdir(&home.join(d))?;
    }
    let chaos_app = root.join("bench/chaos/app.ts");
    copy(&chaos_app, &home.join("apps/app.ts"))?;
    copy(&chaos_app, &home.join("releases/v1/app.ts"))?;
    copy(&chaos_app, &home.join("releases/v2/app.ts"))?;
    for f in ["longlived.ts", "longlived_node.mjs"] {
        copy(&root.join("tests/fixtures").join(f), &home.join("apps").join(f))?;
    }
    std::os::unix::fs::symlink("v1", home.join("releases/current")).map_err(|e| format!("symlink: {e}"))?;
    let para = "<p>Warden chaos soak: a static page served from memory.</p>\n".repeat(30);
    write(&home.join("site/index.html"), &format!("<!doctype html><title>chaos</title>\n{para}"))?;
    write(&home.join("site/assets/app.js"), &"console.log('chaos soak asset');\n".repeat(900))?;
    // The log directory on a small tmpfs, so the disk can be filled. Only
    // in our own mount namespace: it goes away with it.
    let tmpfs = in_ns
        && Command::new("mount")
            .args(["-t", "tmpfs", "-o", "size=48m,mode=0755", "warden-chaos-logs"])
            .arg(home.join("logs"))
            .status()
            .is_ok_and(|s| s.success());
    Ok(tmpfs)
}

fn report_line(sh: &Shared, s: &str) {
    println!("[{:>6.1}s] {s}", sh.now());
}

fn run(o: &Opts, root: &Path, bin: &Path, in_ns: bool) -> Result<(), String> {
    let seed = o.seed.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64 % 1_000_000_000)
            .unwrap_or(1)
    });
    println!(
        "chaos: seed {seed} (reproduce with: cargo xtask chaos --seed {seed} --minutes {}{})",
        o.minutes,
        o.only.as_ref().map(|k| format!(" --only {}", k.join(","))).unwrap_or_default()
    );
    let migrate_req = procfs::sysctl("net/ipv4/tcp_migrate_req");
    println!("chaos: net.ipv4.tcp_migrate_req = {}", migrate_req.as_deref().unwrap_or("unknown"));
    let mut rng = Rng::new(seed);
    // Not macOS's per-user $TMPDIR (/var/folders/…/T/): the workers' health
    // sockets in <home>/run/<app>/ would pass the ~104-byte limit of a Unix
    // socket path, and Warden rejects such a config.
    let tmp = if cfg!(target_os = "linux") { std::env::temp_dir() } else { PathBuf::from("/tmp") };
    let home = tmp.join(format!("wc-{}-{}", std::process::id(), seed % 100_000));
    let tmpfs = prepare(root, &home, in_ns)?;
    let mut notes = Vec::new();
    if !tmpfs {
        notes.push("no tmpfs for the log directory (needs Linux, root and the namespace): disk-full is skipped".into());
    }
    // The optional apps: each needs something the host may not have.
    let nest_dir = root.join("bench/nest");
    let nest = match nest_ready(&nest_dir) {
        Ok(()) => true,
        Err(why) => {
            notes.push(format!("no NestJS app: {why}"));
            false
        }
    };
    let cgroup = if !cfg!(target_os = "linux") {
        notes.push("no memory cgroup on this OS: no `oom` app, oom-kill is skipped".into());
        None
    } else if !procfs::is_root() {
        notes.push("no memory cgroup without root: no `oom` app, oom-kill is skipped".into());
        None
    } else {
        let name = format!("warden-chaos-{}-{}", std::process::id(), seed % 100_000);
        match cgroup::make(&name, OOM_LIMIT_MB) {
            Ok(cg) => {
                println!("chaos: the `oom` app runs in {} ({} MB, no swap)", cg.dir.display(), cg.limit_mb);
                Some(cg)
            }
            Err(why) => {
                notes.push(format!("no memory cgroup could be made ({why}): no `oom` app, oom-kill is skipped"));
                None
            }
        }
    };
    let rss = cfg!(target_os = "linux");
    if !rss {
        notes.push("Warden reads workers' RSS from /proc (Linux): no `memhog` app, memory-recycle is skipped".into());
    }
    // README, Platforms: without a parent-death signal the workers of a
    // SIGKILLed supervisor keep running (and serving) next to the new ones.
    let pdeathsig = cfg!(target_os = "linux");
    if !pdeathsig {
        notes.push(
            "no parent-death signal on this OS (a killed supervisor's workers live on): kill-supervisor is skipped"
                .into(),
        );
    }
    // Node's reusePort (libuv) is Linux's (and some BSDs'): not macOS's.
    let node_shared_port = cfg!(target_os = "linux");
    if !node_shared_port {
        notes.push("Node cannot share a port on this OS (no reusePort in libuv): no `api-node` app".into());
    }
    for n in &notes {
        println!("chaos: note: {n}");
    }
    let opt = fleet::Optional { nest, oom: cgroup.is_some(), rss, node_shared_port };
    let specs: Vec<AppSpec> = fleet::specs(&opt)?;
    let logs = home.join("logs");
    let crash_flag = home.join("crash-flag");
    let alerts_file = home.join("alerts.jsonl");
    let mut good = BTreeMap::new();
    let paths = fleet::Paths { home: &home, logs: &logs, crash_flag: &crash_flag, nest: &nest_dir };
    for s in &specs {
        let text = fleet::config_text(s, &paths);
        write(&home.join(format!("{}.toml", s.name)), &text)?;
        good.insert(s.name.to_string(), text);
    }
    let env = vec![
        ("WARDEN_HOME".to_string(), home.display().to_string()),
        ("WARDEN_RUNTIME_DIR".to_string(), home.join("run").display().to_string()),
        ("WARDEN_NO_DAEMON".to_string(), "1".to_string()),
    ];
    let fleet = Arc::new(Fleet {
        bin: bin.to_path_buf(),
        home: home.clone(),
        logs,
        tmpfs,
        expected: Mutex::new(specs.iter().map(|s| (s.name.to_string(), s.count)).collect()),
        apps: specs,
        env,
        crash_flag,
        good_config: good,
        cgroup,
    });
    // wardend's alerts go to a file, one JSON object per line.
    let q = alerts_file.display().to_string().replace('\'', "");
    write(
        &home.join("wardend.toml"),
        &format!(
            "[[alert]]\non = [\"all\"]\ncommand = [\"sh\", \"-c\", \"cat >> '{q}'; echo >> '{q}'\"]\nmin_interval = \"1s\"\n"
        ),
    )?;
    let r = fleet.cli(&["daemon", "check"], Duration::from_secs(20));
    if !r.ok() {
        println!("chaos: note: wardend.toml rejected, running without alert rules: {}", r.brief());
        let _ = std::fs::remove_file(home.join("wardend.toml"));
    }
    println!("chaos: run directory {}", home.display());
    for s in &fleet.apps {
        let timeout = Duration::from_secs(90);
        let r = match (&fleet.cgroup, s.oom) {
            // Its supervisor inherits the cgroup from the CLI that starts it.
            (Some(cg), true) => fleet.cli_in_cgroup(cg, &["start", s.name], timeout),
            _ => fleet.cli(&["start", s.name], timeout),
        };
        if !r.ok() {
            return Err(format!("starting {}: {}", s.name, r.brief()));
        }
    }
    let r = fleet.cli(&["daemon", "--background"], Duration::from_secs(30));
    if !r.ok() {
        return Err(format!("starting wardend: {}", r.brief()));
    }
    if let Err(why) = fleet.wait_ready(Duration::from_secs(90)) {
        return Err(format!("the fleet never became ready: {}", why.join("; ")));
    }
    let sh = Arc::new(Shared::new());
    report_line(&sh, &format!("fleet ready: {} apps; starting the load", fleet.apps.len()));

    // Load and monitors.
    let mut threads = Vec::new();
    for s in &fleet.apps {
        if let Some((ka, nc)) = s.load {
            let (a, b) = (sh.clone(), sh.clone());
            let (app, port) = (s.name.to_string(), s.port);
            let app2 = app.clone();
            threads
                .push(std::thread::spawn(move || load::keepalive(a, app, port, ka.into(), Duration::from_millis(20))));
            threads
                .push(std::thread::spawn(move || load::newconn(b, app2, port, nc.into(), Duration::from_millis(40))));
        }
        for path in s.longlived {
            let (sh, app, port, path) = (sh.clone(), s.name.to_string(), s.port, path.to_string());
            threads.push(std::thread::spawn(move || load::longlived(sh, app, port, path)));
        }
    }
    {
        let (s1, f1) = (sh.clone(), fleet.clone());
        threads.push(std::thread::spawn(move || monitor::observer(s1, f1)));
        let (s2, f2) = (sh.clone(), fleet.clone());
        threads.push(std::thread::spawn(move || monitor::procmon(s2, f2, in_ns)));
    }
    sh.quiet.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(10));

    // Faults, one after another, in a shuffled deck: every kind comes up
    // once per round, in a random order.
    let kinds: Vec<&'static str> = faults::KINDS
        .iter()
        .copied()
        .filter(|k| o.only.as_ref().is_none_or(|only| only.contains(k)))
        .filter(|k| *k != "disk-full" || tmpfs)
        .filter(|k| *k != "oom-kill" || fleet.cgroup.is_some())
        .filter(|k| *k != "memory-recycle" || rss)
        .filter(|k| *k != "kill-supervisor" || pdeathsig)
        .collect();
    let bound = Duration::from_secs(o.bound);
    let run_for = Duration::from_secs_f64(o.minutes * 60.0);
    let t_faults = Instant::now();
    let mut done: Vec<faults::Fault> = Vec::new();
    let mut deck: Vec<&'static str> = Vec::new();
    while t_faults.elapsed() < run_for && !kinds.is_empty() {
        if deck.is_empty() {
            deck = kinds.clone();
            rng.shuffle(&mut deck);
        }
        let Some(kind) = deck.pop() else { break };
        sh.quiet.store(false, Ordering::Relaxed);
        let n = done.len() + 1;
        let f = {
            let mut cx = faults::Ctx { sh: &sh, fleet: &fleet, rng: &mut rng, bound };
            faults::run(kind, n, &mut cx)
        };
        let outcome = match (&f.skipped, f.recovery) {
            (Some(why), _) => format!("skipped: {why}"),
            (None, Some(r)) => format!("recovered in {:.0} ms", r * 1000.0),
            (None, None) => "NOT RECOVERED".to_string(),
        };
        report_line(&sh, &format!("#{n} {kind} {}: {} -> {outcome}", f.app.as_deref().unwrap_or("-"), f.detail));
        for p in &f.problems {
            report_line(&sh, &format!("    PROBLEM: {p}"));
        }
        let recovered = f.recovery.is_some() || f.skipped.is_some();
        done.push(f);
        std::thread::sleep(Duration::from_secs(1));
        sh.quiet.store(recovered, Ordering::Relaxed);
        let gap = rng.range(1.0, 5.0);
        std::thread::sleep(Duration::from_secs_f64(gap));
    }

    // The end: ready, quiet, load stopped, everything killed.
    let mut end_problems = Vec::new();
    if let Err(why) = fleet.wait_ready(bound) {
        end_problems.push(report::Violation {
            invariant: "recovery",
            t: Some(sh.now()),
            app: None,
            what: format!("the fleet was not ready at the end: {}", why.join("; ")),
        });
    }
    sh.quiet.store(true, Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(5));
    let duration_s = sh.now();
    report_line(&sh, "stopping the load");
    sh.stop.store(true, Ordering::Relaxed);
    for t in threads {
        let _ = t.join();
    }
    let alerts = read_alerts(&alerts_file);
    let r = fleet.cli(&["kill", "--yes"], Duration::from_secs(60));
    if !r.ok() {
        end_problems.push(report::Violation {
            invariant: "cleanup",
            t: None,
            app: None,
            what: format!("warden kill --yes: {}", r.brief()),
        });
    }
    let left = leftovers(&home, Duration::from_secs(fleet::GRACE_S + 10));
    if !left.is_empty() {
        end_problems.push(report::Violation {
            invariant: "cleanup",
            t: None,
            app: None,
            what: format!("still running after warden kill --yes: {}", left.join("; ")),
        });
    }
    if let Some(cg) = &fleet.cgroup
        && let Err(e) = cgroup::remove(cg)
    {
        end_problems.push(report::Violation { invariant: "cleanup", t: None, app: None, what: e });
    }

    let rep = report::build(report::Inputs {
        sh: &sh,
        fleet: &fleet,
        faults: &done,
        seed,
        minutes: o.minutes,
        migrate_req: migrate_req.clone(),
        namespace: in_ns,
        duration_s,
        end_problems,
        alerts,
        build: build_label(bin),
        notes,
    });
    print!("{}", rep.text);
    let out = match &o.out {
        Some(p) => p.clone(),
        None => {
            let stamp = crate::tool_version("date", &["-u", "+%Y%m%d-%H%M%S"]).unwrap_or_else(|| "now".into());
            root.join(format!("bench/results/chaos-{stamp}-seed{seed}.json"))
        }
    };
    let json = serde_json::to_string_pretty(&rep.json).unwrap_or_default();
    if let Some(dir) = out.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    write(&out, &json)?;
    // The copy kept in git: compact (the samples would take thousands of lines).
    let latest = root.join("bench/results/latest");
    if latest.is_dir() && o.out.is_none() {
        let _ = std::fs::write(latest.join("chaos.json"), serde_json::to_string(&rep.json).unwrap_or_default() + "\n");
    }
    println!("\nchaos: wrote {}", out.display());

    if rep.violations.is_empty() {
        if !o.keep {
            let _ = Command::new("umount").arg(home.join("logs")).status();
            let _ = std::fs::remove_dir_all(&home);
        }
        println!("chaos: PASS (seed {seed})");
        return Ok(());
    }
    let logdir = home.join("state/logs");
    println!("\nchaos: FAILED: {} invariant violation(s) with seed {seed}", rep.violations.len());
    println!(
        "chaos: reproduce with: cargo xtask chaos --seed {seed} --minutes {}{}",
        o.minutes,
        o.only.as_ref().map(|k| format!(" --only {}", k.join(","))).unwrap_or_default()
    );
    let mut shown: HashSet<String> = HashSet::new();
    for v in rep.violations.iter().take(40) {
        println!("\n  [{}] {}{}", v.invariant, v.t.map(|t| format!("at {t:.1}s: ")).unwrap_or_default(), v.what);
        if let (Some(t), Some(app)) = (v.t, &v.app) {
            // One excerpt per app and second is enough.
            if shown.insert(format!("{app}@{}", t as i64)) {
                for l in report::excerpt(&logdir, app, sh.wall0_ms + (t * 1000.0) as i64) {
                    println!("      {l}");
                }
            }
        }
    }
    if rep.violations.len() > 40 {
        println!("\n  … and {} more in the JSON", rep.violations.len() - 40);
    }
    println!("\nchaos: logs kept in {}", logdir.display());
    Err(format!("{} invariant violation(s); seed {seed}", rep.violations.len()))
}

/// The alerts in alerts.jsonl. Not line by line: alerts delivered at the
/// same moment (a crash_loop and an oom of the same death) run the rule's
/// `cat >> file; echo >> file` at once, and two objects can share a line.
pub fn alert_values(text: &str) -> Vec<serde_json::Value> {
    serde_json::Deserializer::from_str(text).into_iter::<serde_json::Value>().map_while(Result::ok).collect()
}

/// wardend's delivered alerts, counted by kind.
fn read_alerts(path: &Path) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for v in alert_values(&std::fs::read_to_string(path).unwrap_or_default()) {
        let kind = v["kind"].as_str().map(String::from).unwrap_or_else(|| "unparsed".into());
        *out.entry(kind).or_default() += 1;
    }
    out
}

/// Processes of this run (WARDEN_HOME in their environment) still alive
/// after `within`.
fn leftovers(home: &Path, within: Duration) -> Vec<String> {
    let entry = format!("WARDEN_HOME={}", home.display());
    let me = std::process::id();
    let t0 = Instant::now();
    loop {
        let left: Vec<String> = procfs::ours(&entry, me)
            .into_iter()
            .map(|p| format!("pid {p}: {}", procfs::cmdline(p).join(" ")))
            .collect();
        if left.is_empty() || t0.elapsed() > within {
            return left;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_seeded_and_spread() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        assert_eq!((0..5).map(|_| a.next_u64()).collect::<Vec<_>>(), (0..5).map(|_| b.next_u64()).collect::<Vec<_>>());
        let mut seen = [0usize; 4];
        for _ in 0..4000 {
            seen[a.below(4)] += 1;
        }
        assert!(seen.iter().all(|n| *n > 800), "{seen:?}");
        let x = a.range(1.0, 2.0);
        assert!((1.0..2.0).contains(&x));
        let mut v: Vec<u32> = (0..20).collect();
        a.shuffle(&mut v);
        v.sort();
        assert_eq!(v, (0..20).collect::<Vec<_>>());
    }

    #[test]
    fn options() {
        let a = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        let o = parse(&a("--minutes 2.5 --seed 42 --only kill-worker,reload")).unwrap();
        assert_eq!((o.minutes, o.seed), (2.5, Some(42)));
        assert_eq!(o.only.unwrap(), vec!["kill-worker", "reload"]);
        assert!(parse(&a("--only nope")).is_err());
        assert!(parse(&a("--minutes 0")).is_err());
        assert!(parse(&a("--bogus")).is_err());
    }
}

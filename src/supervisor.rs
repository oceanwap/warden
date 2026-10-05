//! The supervisor: one event loop owning all state (no locks). Child exits,
//! readiness, timers, signals, health results and CLI requests all arrive as
//! events and are handled in order.
//!
//! - `rollout.rs`: gated replacement of workers (reload, safe-reload with a
//!   canary, restart N, recycling), preflight, rollback.
//! - `upkeep.rs`: the 1 s maintenance tick (watchdog, per-worker health,
//!   memory / lifetime recycling, FAILED cooldown).
//! - `portwatch.rs`: `[watchdog] port_lost`, a worker that stopped listening.
//! - `standby.rs`: hot standbys (`[workers] standby`), promoted into the
//!   slot of a worker that died.
//! - `release.rs`: release pinning (`[app] pin_release`).

mod listening;
mod portwatch;
mod release;
mod requests;
#[cfg(test)]
mod rig;
mod rollout;
mod standby;
mod upkeep;

use crate::config::{Config, Mode, OnHealthFailure, PortStrategy};
use crate::control::{self, ControlMsg, HostStatus, Request, Response, Status, WorkerStatus};
use crate::events::{self, WorkerEvent};
use crate::process::{self, IpcMsg, ProcEvent};
use crate::restart::{Decision, Policy};
use crate::signals::Sig;
use crate::worker::{Instance, LoopState, Role, STANDBY_SLOT, Slot, State, ThreadInfo, describe_exit, standby_label};
use crate::{debug, error, info, metrics, networking, systemd, warn, watch};
use listening::ListenerCache;
use rollout::{Kind, Roll};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const SHIM_JS: &str = include_str!("../shim/warden-shim.mjs");
const HOST_JS: &str = include_str!("../shim/warden-host.mjs");
const HEARTBEAT_MS: u64 = 1000;

enum Event {
    Ready {
        inst: u64,
    },
    ReadyTimeout {
        inst: u64,
    },
    RestartDue {
        slot: usize,
        token: u64,
    },
    KillDue {
        inst: u64,
    },
    AppHealth(Result<u16, String>),
    WorkerHealth {
        inst: u64,
        result: Result<(), String>,
    },
    Tick,
    /// `[restart] schedule` fired (stale when `token` changed).
    Scheduled {
        token: u64,
    },
    /// `[watch]`: files changed and settled (stale when `token` is not the
    /// running watcher's).
    Watch {
        token: u64,
        change: watch::Change,
    },
    Gate(rollout::GateEvent),
    Snapshot(oneshot::Sender<Status>),
    /// Standby pool: refill after backoff (stale when `token` changed).
    StandbyDue {
        token: u64,
    },
    /// Standby gates: next health check due, a check's or verify_command's result.
    StandbyGateDue {
        inst: u64,
    },
    StandbyChecked {
        inst: u64,
        result: Result<(), String>,
    },
    StandbyVerified {
        inst: u64,
        result: Result<(), String>,
    },
    /// `[watchdog] port_lost`: what each watched worker was seen listening on.
    Ports(Vec<(u64, portwatch::Seen)>),
}

struct HealthState {
    healthy: Option<bool>,
    failures: u32,
}

pub struct Supervisor {
    cfg: Config,
    cfg_path: Option<PathBuf>,
    policy: Policy,
    count: usize,
    slots: BTreeMap<usize, Slot>,
    insts: HashMap<u64, Instance>,
    next_inst: u64,
    tx: mpsc::UnboundedSender<Event>,
    proc_tx: mpsc::UnboundedSender<ProcEvent>,
    roll: Option<Roll>,
    roll_seq: u64,
    last_rollout: Option<control::RolloutOutcome>,
    /// Workers waiting for a graceful replacement (health, memory, lifetime, hang).
    pending_replace: BTreeMap<usize, (String, bool)>,
    shutting_down: bool,
    /// `warden stop`: workers stopped, supervisor idle.
    stopped: bool,
    /// `warden restart` (all): start everything again once all have exited.
    start_after_stop: bool,
    app_health: HealthState,
    started: Instant,
    announced_ready: bool,
    runtime_dir: PathBuf,
    shim_path: Option<PathBuf>,
    host_path: Option<PathBuf>,
    force_kill: bool,
    /// `[watchdog] port_lost`: a look at the workers' sockets is running.
    port_look_inflight: bool,
    /// The responses of the app's workers, since the supervisor started
    /// (`None` until one reports them).
    requests: Option<crate::worker::Window>,
    /// The app's ports as the kernel sees them (`Status.ports`).
    ports: requests::PortCache,
    supervisor_cpu_prev: Option<(Instant, f64)>,
    /// What each worker process listens on (a walk of its process tree
    /// reads `/proc`, or asks libproc, for each: see `listening.rs`).
    listeners: ListenerCache,
    ticks: u64,
    /// Fleet-wide health failure: a dependency is probably down; replacements held.
    outage: bool,
    last_tick: Instant,
    /// systemd set WATCHDOG_USEC for us (WatchdogSec= in the unit).
    watchdog_enabled: bool,
    /// Invalidates the pending scheduled restart when the schedule changes.
    schedule_token: u64,
    /// `[watch]`: the settings the running watcher was started with, and the
    /// watcher itself (dropping it stops its scans). None while `[watch]` is
    /// off, and while the workers are stopped or shutting down.
    watch: Option<(watch::Spec, watch::Handle)>,
    /// Told apart from an earlier watcher's late events.
    watch_token: u64,
    /// The settings that could not start a watcher, or whose watcher died (not
    /// retried every tick: `warden reload` or a start of the workers tries again).
    watch_failed: Option<watch::Spec>,
    /// Said that `[watch]` cannot say where to look (once, not every tick).
    watch_blocked: bool,
    /// The directory Warden was started in, for what is relative (it may be
    /// deleted later: asking again would answer "/" or fail).
    launch_cwd: Option<PathBuf>,
    /// Files changed and the restart has not started yet (a rollout is in
    /// progress, or the gap after the last watch restart has not passed).
    watch_pending: Option<watch::Change>,
    /// The wait for `watch_pending` was logged.
    watch_held: bool,
    /// A restart that could not start (the release did not resolve) is tried
    /// again a few times: `(failures so far, not before)`.
    watch_retry: Option<(u8, Instant)>,
    watch_throttle: watch::Throttle,
    /// This binary, for `[static]` apps (their workers run `warden serve-static`).
    exe: PathBuf,
    /// Why we are shutting down, for the `bye` event.
    shutdown_reason: String,
    /// Rollout phase last published as a `rollout` event.
    rollout_published: Option<rollout::PhaseKey>,
    /// Hot standbys: the pool's restart policy state (members are in `insts`).
    pool: standby::Pool,
    /// `[app] pin_release`: the release workers start in.
    release: Option<release::Pin>,
    /// The OOM kill counters of the cgroups workers run in, to tell OOM
    /// kills from other SIGKILLs.
    oom: process::exit::OomTracker,
    /// Since the workers were last started (`start_all`) and until one is
    /// ready: the slots that crashed meanwhile. Every slot in it = the app
    /// can't start (`Status.start_failed`; `warden start` fails fast on it).
    start_attempt: Option<StartAttempt>,
    /// macOS, where a worker is not stopped when its supervisor is killed:
    /// this supervisor's record of its workers, which the next supervisor of
    /// the app reads to stop the ones left running (`platform::orphans`).
    orphans: Option<crate::platform::orphans::Registry>,
    /// The sweep of a killed supervisor's workers, while it runs: no worker is
    /// started here until it ends (they hold the port), and what `status`
    /// says meanwhile (`sweep_status`).
    sweep: Option<Sweeping>,
}

/// The sweep of a killed supervisor's workers, run by `run_local`'s loop.
type Sweep<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Option<crate::platform::orphans::Registry>> + 'a>>;

/// A sweep of the previous supervisor's workers in progress (`platform::orphans`).
struct Sweeping {
    started: Instant,
    progress: std::sync::Arc<crate::platform::orphans::Progress>,
    /// The most it takes: `shutdown.grace_period` and the wait after SIGKILL.
    longest: Duration,
}

/// How many times each worker has to crash before one is ready for the app
/// to count as unable to start. One crash can be a transient: a dependency
/// that wasn't up for a moment, a port still held by the old process. The
/// first restart is immediate, so a second crash follows at once for an app
/// that is really broken.
const START_FAIL_CRASHES: u32 = 2;

/// A start of every worker that no worker has survived to be ready yet.
#[derive(Debug, Default)]
struct StartAttempt {
    /// Crashes per slot since the start.
    crashes: std::collections::BTreeMap<usize, u32>,
    /// The last crash's reason (`exit code 1`, `not ready in time`).
    last_exit: Option<String>,
    /// The ERROR line saying so was logged (once per attempt).
    reported: bool,
}

impl StartAttempt {
    /// Every slot crashed `START_FAIL_CRASHES` times, none ever ready.
    fn every_slot_crashed(&self, ids: &[usize]) -> bool {
        !ids.is_empty() && ids.iter().all(|id| self.crashes.get(id).copied().unwrap_or(0) >= START_FAIL_CRASHES)
    }
}

pub async fn run(cfg: Config, cfg_path: Option<PathBuf>) -> Result<(), String> {
    let local = tokio::task::LocalSet::new();
    local.run_until(run_local(cfg, cfg_path)).await
}

async fn run_local(cfg: Config, cfg_path: Option<PathBuf>) -> Result<(), String> {
    // While our launcher is still our parent (see `own_unit`).
    let _ = systemd::own_unit();
    let socket = cfg.socket_path();
    let listener = control::bind(&socket).await?;
    let runtime_dir = socket.parent().unwrap_or(Path::new(".")).to_path_buf();
    let (shim_path, host_path) = write_js(&cfg, &runtime_dir)?;

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    let (proc_tx, mut proc_rx) = mpsc::unbounded_channel::<ProcEvent>();
    let (sig_tx, mut sig_rx) = mpsc::unbounded_channel::<Sig>();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<ControlMsg>();

    crate::signals::listen(sig_tx).map_err(|e| format!("installing signal handlers: {e}"))?;
    crate::guard::spawn_essential("control socket", control::serve(listener, ctl_tx));

    if let Some(addr) = &cfg.metrics.listen {
        let addr: std::net::SocketAddr = addr.parse().map_err(|e| format!("metrics.listen: {e}"))?;
        let tx = tx.clone();
        crate::guard::spawn_essential("metrics endpoint", async move {
            let snapshot = move || {
                let tx = tx.clone();
                async move {
                    let (s, r) = oneshot::channel();
                    tx.send(Event::Snapshot(s)).ok()?;
                    r.await.ok()
                }
            };
            if let Err(e) = metrics::serve(addr, snapshot).await {
                error!(
                    "metrics endpoint could not start; Warden keeps running without it",
                    addr = addr,
                    error = e,
                    hint = "is another process using that address? change [metrics] listen",
                );
            }
        });
    }

    if cfg.health.enabled && !cfg.health.url.is_empty() {
        let target = crate::health::parse_url(&cfg.health.url)?;
        let interval = Duration::from_secs(cfg.health.interval);
        let timeout = Duration::from_secs(cfg.health.timeout);
        let tx = tx.clone();
        crate::guard::spawn_essential("app health check", async move {
            let mut tick = tokio::time::interval(interval);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let r = crate::health::check(&target, timeout).await;
                if tx.send(Event::AppHealth(r)).is_err() {
                    return;
                }
            }
        });
    }

    {
        let tx = tx.clone();
        crate::guard::spawn_essential("tick", async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if tx.send(Event::Tick).is_err() {
                    return;
                }
            }
        });
    }

    // wardend restarts supervisors that die; this brings wardend back if it dies. Best effort:
    // nothing here waits for it, and an app never depends on it.
    tokio::task::spawn_local(crate::daemon::revive::watch());

    let mut sup = Supervisor::new(cfg, cfg_path, runtime_dir, (shim_path, host_path), tx, proc_tx);

    // `warden save` recorded a worker count / stopped state for this app:
    // honour it, as `pm2 resurrect` would after a reboot.
    let saved = crate::fleet::saved_state(&sup.cfg.app.name, sup.cfg_path.as_deref());
    if let Some(s) = saved.as_ref().filter(|s| s.workers != sup.count && (1..=1024).contains(&s.workers)) {
        info!(
            "using the worker count saved by `warden save`",
            config = sup.count,
            saved = s.workers,
            hint = "run `warden save` again after scaling to change it",
        );
        sup.count = s.workers;
    }
    info!(
        "starting application",
        app = sup.cfg.app.name,
        mode = mode_name(sup.cfg.workers.mode),
        workers = sup.count,
        pid = std::process::id(),
        control = socket.display(),
    );
    if sup.cfg.workers.standby > 0 {
        info!(
            "hot standbys enabled: started once the workers are ready",
            standby = sup.cfg.workers.standby,
            note = "each costs about one worker's memory",
        );
    }
    warn_if_no_migrate_req(&sup.cfg);
    warn_if_node_cannot_share_the_port(&sup.cfg);
    if !sup.cfg.any_worker_path() {
        info!("no health path configured: new workers are gated on listening only (set [health] path)");
    }
    match sup.oom.own() {
        Some(p) => debug!("OOM kills are told apart through the cgroup's counter", file = p.display()),
        None => debug!("no cgroup OOM kill counter found: a SIGKILL's sender can't be told apart from an OOM kill"),
    }
    // Workers a killed supervisor of this app left running are stopped before
    // new ones start (macOS: no parent-death signal); then this one's workers
    // are recorded as they start. On macOS the sweep runs in this loop, so
    // that signals and control requests are served meanwhile (it can take
    // `grace_period` and a few seconds when a worker ignores the stop signal),
    // and the workers start once it is over.
    let app = sup.cfg.app.name.clone();
    let progress = std::sync::Arc::new(crate::platform::orphans::Progress::default());
    let settings = crate::platform::orphans::Settings {
        app: &app,
        stop_signal: sup.cfg.stop_signal(),
        grace: sup.cfg.grace_period(),
        dir: crate::platform::orphans::dir(&crate::fleet::state_dir()),
        progress: progress.clone(),
    };
    let saved_stopped = saved.as_ref().is_some_and(|s| s.stopped);
    let mut sweeping: Option<Sweep<'_>> = None;
    if crate::platform::orphans::enabled() {
        let longest = crate::platform::orphans::longest(settings.grace);
        let mut sweep: Sweep<'_> = Box::pin(crate::platform::orphans::start(settings));
        // Nothing to stop (the usual case) ends at the first look: workers start
        // as ever, and no sweep is ever shown. Otherwise the loop carries on with it.
        match tokio::time::timeout(Duration::ZERO, &mut sweep).await {
            Ok(registry) => {
                sup.orphans = registry;
                sup.start_workers(saved_stopped);
            }
            Err(_) => {
                sup.sweep = Some(Sweeping { started: Instant::now(), progress, longest });
                sweeping = Some(sweep);
            }
        }
    } else {
        sup.start_workers(saved_stopped);
    }

    loop {
        tokio::select! {
            reg = async { sweeping.as_mut().expect("the precondition says it is there").await }, if sweeping.is_some() => {
                sweeping = None;
                sup.sweep_over(reg, saved_stopped);
            }
            Some(ev) = rx.recv() => sup.on_event(ev),
            Some(pe) = proc_rx.recv() => sup.on_proc(pe),
            Some(s) = sig_rx.recv() => sup.on_signal(s),
            Some((req, reply)) = ctl_rx.recv() => {
                let resp = sup.on_request(req);
                let _ = reply.send(resp);
            }
        }
        sup.publish_rollout();
        if sup.shutting_down && sup.insts.is_empty() {
            break;
        }
    }
    if sweeping.is_some() {
        // Told to stop before the sweep ended: nothing was started. What it
        // did not reach is found, by the records, at the next start.
        info!(
            "stopped while waiting for the previous supervisor's workers to stop; the next start of the app stops what is left"
        );
    }
    // Every worker has exited: nothing for a next supervisor to sweep.
    if let Some(o) = sup.orphans.as_mut() {
        o.close();
    }
    info!("stopped", app = sup.cfg.app.name);
    // From now on control requests are answered "shutting down" at once: a
    // subscriber waiting for a status must not hold up its `bye`.
    drop(ctl_rx);
    // Subscribers learn we exit on purpose (EOF without `bye` means a crash),
    // and control tasks deliver replies already sent (e.g. to the `shutdown`
    // that ended us) before the runtime goes away. Bounded: 50-200 ms.
    control::say_bye(&sup.cfg.app.name, &sup.shutdown_reason, Duration::from_millis(50), control::BYE_FLUSH).await;
    // Best effort: a leftover socket is detected and replaced at the next start.
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Write the embedded shim / host scripts into the runtime directory.
fn write_js(cfg: &Config, dir: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>), String> {
    if !cfg.shim_enabled() {
        return Ok((None, None));
    }
    control::ensure_private_dir(dir)?;
    let write = |name: String, body: &str| -> Result<PathBuf, String> {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt;
        let path = dir.join(name);
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        // create_new = O_CREAT|O_EXCL: never follows a planted symlink.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&tmp)
            .map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        f.write_all(body.as_bytes()).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("writing {}: {e}", path.display()))?;
        Ok(path)
    };
    let shim = write(format!("{}-shim.mjs", cfg.app.name), SHIM_JS)?;
    let host = match cfg.workers.mode {
        Mode::Worker => Some(write(format!("{}-host.mjs", cfg.app.name), HOST_JS)?),
        Mode::Process => None,
    };
    Ok((Some(shim), host))
}

/// With SO_REUSEPORT, connections still queued on a listener that closes are
/// reset unless the kernel migrates them (Linux ≥ 5.14). Measured: 10-15 resets
/// per worker-mode reload under load with 0, none with 1.
fn warn_if_no_migrate_req(cfg: &Config) {
    if cfg.workers.port_strategy != PortStrategy::Shared || cfg.app.port.is_none() {
        return;
    }
    if let Ok(v) = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_migrate_req") {
        if v.trim() == "0" {
            warn!(
                "net.ipv4.tcp_migrate_req is 0: a few queued connections may be reset when a worker stops",
                hint = "sysctl -w net.ipv4.tcp_migrate_req=1, and contrib/99-warden.conf to keep it after a reboot \
                        (`warden startup` installs it)",
            );
        }
    }
}

/// Node's `reusePort` exists only where the kernel spreads connections
/// (Linux, some BSDs): on macOS a second Node worker on the port fails with
/// EADDRINUSE, and so does a rollout's replacement next to the old worker.
fn warn_if_node_cannot_share_the_port(cfg: &Config) {
    let node_shares =
        cfg!(any(target_os = "linux", target_os = "freebsd", target_os = "dragonfly", target_os = "solaris"));
    if node_shares
        || !crate::config::is_node(&cfg.app.command)
        || cfg.workers.port_strategy != PortStrategy::Shared
        || cfg.app.port.is_none()
    {
        return;
    }
    warn!(
        "Node cannot share a port on this OS: only one worker can listen on it",
        workers = cfg.workers.count,
        hint = "Node's reusePort (libuv) exists only where the kernel spreads connections, such as Linux. Here run \
                [workers] count = 1, and replace it with `warden restart --hard` (a reload starts the new worker \
                next to the old one, which fails with EADDRINUSE); or set workers.port_strategy = \"offset\" \
                (one port per worker)",
    );
}

fn mode_name(m: Mode) -> &'static str {
    match m {
        Mode::Process => "process",
        Mode::Worker => "worker",
    }
}

impl Supervisor {
    fn new(
        cfg: Config,
        cfg_path: Option<PathBuf>,
        runtime_dir: PathBuf,
        (shim_path, host_path): (Option<PathBuf>, Option<PathBuf>),
        tx: mpsc::UnboundedSender<Event>,
        proc_tx: mpsc::UnboundedSender<ProcEvent>,
    ) -> Supervisor {
        Supervisor {
            policy: Policy::from(&cfg.restart),
            count: cfg.workers.count,
            slots: BTreeMap::new(),
            insts: HashMap::new(),
            next_inst: 1,
            tx,
            proc_tx,
            roll: None,
            roll_seq: 0,
            last_rollout: None,
            pending_replace: BTreeMap::new(),
            shutting_down: false,
            stopped: false,
            start_after_stop: false,
            app_health: HealthState { healthy: None, failures: 0 },
            started: Instant::now(),
            announced_ready: false,
            runtime_dir,
            shim_path,
            host_path,
            force_kill: false,
            port_look_inflight: false,
            requests: None,
            ports: requests::PortCache::default(),
            supervisor_cpu_prev: None,
            listeners: ListenerCache::default(),
            ticks: 0,
            outage: false,
            last_tick: Instant::now(),
            watchdog_enabled: systemd::watchdog_requested(),
            schedule_token: 0,
            watch: None,
            watch_token: 0,
            watch_failed: None,
            watch_blocked: false,
            launch_cwd: std::env::current_dir().ok(),
            watch_pending: None,
            watch_held: false,
            watch_retry: None,
            watch_throttle: watch::Throttle::default(),
            exe: own_exe(),
            shutdown_reason: "shutdown".into(),
            rollout_published: None,
            pool: standby::Pool::default(),
            release: None,
            oom: process::exit::OomTracker::new(),
            start_attempt: None,
            orphans: None,
            sweep: None,
            cfg,
            cfg_path,
        }
    }

    fn is_worker_mode(&self) -> bool {
        self.cfg.workers.mode == Mode::Worker
    }

    /// The processes this supervisor has now, for the record that lets the
    /// next supervisor stop them if this one is killed (`platform::orphans`;
    /// nothing where the OS stops workers with their supervisor).
    fn note_workers(&mut self) {
        if self.orphans.is_none() {
            return;
        }
        let workers: Vec<(u32, String)> = self.insts.values().map(|i| (i.handle.pid, self.inst_label(i))).collect();
        if let Some(o) = self.orphans.as_mut() {
            o.note(&workers);
        }
    }

    /// The first start: the schedule, and the workers, unless `warden save`
    /// recorded them stopped, or a `warden stop` came while the previous
    /// supervisor's workers were being stopped.
    fn start_workers(&mut self, saved_stopped: bool) {
        self.schedule_next();
        if saved_stopped || self.stopped {
            if saved_stopped {
                info!("workers stay stopped, as saved by `warden save`", hint = "`warden start <app>` starts them");
            } else {
                info!(
                    "workers stay stopped, as asked while the previous supervisor's workers were being stopped",
                    hint = "`warden start <app>` starts them"
                );
            }
            self.stopped = true;
            if !self.announced_ready {
                self.announced_ready = true;
                systemd::notify("READY=1\nSTATUS=workers stopped (saved state)");
            }
        } else {
            self.start_all();
        }
    }

    /// The sweep of the previous supervisor's workers has ended: record this
    /// one's from now on, and start them (not when a stop is under way).
    fn sweep_over(&mut self, registry: Option<crate::platform::orphans::Registry>, saved_stopped: bool) {
        self.orphans = registry;
        self.sweep = None;
        if self.shutting_down {
            return;
        }
        self.start_workers(saved_stopped);
    }

    /// What a status says while the sweep runs: a rollout of its own kind, so
    /// that `warden status`, `warden list`, the GUI and `warden start` show
    /// and wait for it like any other (`done`: workers gone, of `total`).
    fn sweep_status(&self) -> Option<control::RolloutStatus> {
        let s = self.sweep.as_ref()?;
        let (done, total) = s.progress.counts();
        Some(control::RolloutStatus {
            seq: 0,
            kind: crate::platform::orphans::SWEEP_KIND.into(),
            phase: format!("waiting for the previous supervisor's workers to stop (up to {} s)", s.longest.as_secs()),
            done,
            total,
            elapsed_secs: s.started.elapsed().as_secs(),
        })
    }

    /// What a request gets while the sweep runs, `None` to carry on as usual:
    /// no worker runs yet, and none may start until the old ones are gone, so
    /// what would start or change workers waits (`start` and `stop` are
    /// remembered for when the sweep ends).
    fn request_during_sweep(&mut self, req: &Request) -> Option<Response> {
        let longest = self.sweep.as_ref()?.longest.as_secs();
        match req {
            Request::Status | Request::Config { .. } | Request::Shutdown => None,
            Request::Logs { .. } | Request::LogLevel { .. } | Request::Flush | Request::Subscribe { .. } => None,
            Request::Start if self.shutting_down => None,
            Request::Stop if self.shutting_down => None,
            Request::Start => {
                self.stopped = false;
                Some(Response::ok("starting workers once the previous supervisor's workers have stopped"))
            }
            Request::Stop => {
                self.stopped = true;
                Some(Response::ok("workers stay stopped once the previous supervisor's workers have stopped"))
            }
            _ => Some(Response::err(format!(
                "still stopping the workers a killed supervisor of this app left behind (up to {longest} s); try again then"
            ))),
        }
    }

    fn slot_ids(&self) -> Vec<usize> {
        match self.cfg.workers.mode {
            Mode::Process => (1..=self.count).collect(),
            Mode::Worker => vec![1],
        }
    }

    /// `worker=` on log lines about a slot (`standby`: the pool as a whole).
    fn label(&self, slot: usize) -> String {
        if self.is_worker_mode() {
            "host".into()
        } else if slot == STANDBY_SLOT {
            "standby".into()
        } else {
            slot.to_string()
        }
    }

    /// `worker=` on log lines about one process: its slot's, or a standby's
    /// own (`s1`, `s2`… as `warden status` lists them).
    fn inst_label(&self, i: &Instance) -> String {
        match i.standby_number.filter(|_| i.role == Role::Standby) {
            Some(n) => standby_label(n),
            None => self.label(i.slot),
        }
    }

    /// `worker` in events: the slot in process mode; in worker mode 0 is the
    /// host process (its Workers are 1..=count, as in `status`).
    fn event_worker(&self, slot: usize) -> usize {
        if self.is_worker_mode() { 0 } else { slot }
    }

    /// A worker event for `slot` (see `emit`).
    fn emit_worker(&self, slot: usize, event: WorkerEvent, pid: Option<u32>, detail: impl FnOnce() -> Option<String>) {
        emit(&self.cfg.app.name, self.event_worker(slot), event, pid, detail);
    }

    /// An event about hot standby `number` (`s1`…; 0: the pool as a whole):
    /// worker 0, with the standby's number (`Event::Worker::standby`).
    fn emit_standby(
        &self,
        number: usize,
        event: WorkerEvent,
        pid: Option<u32>,
        detail: impl FnOnce() -> Option<String>,
    ) {
        emit_to(&self.cfg.app.name, (STANDBY_SLOT, Some(number)), event, pid, detail);
    }

    /// `(worker, standby)` of an event about process `i`: its slot's worker
    /// (`event_worker`), or a standby's own number until it is promoted.
    fn event_who(&self, i: &Instance) -> (usize, Option<usize>) {
        match i.standby_number.filter(|_| i.role == Role::Standby) {
            Some(n) => (STANDBY_SLOT, Some(n)),
            None => (self.event_worker(i.slot), None),
        }
    }

    /// A `rollout` event when a rollout started or its phase changed since
    /// the last one. Without subscribers: one atomic load.
    fn publish_rollout(&mut self) {
        if !events::active() {
            return;
        }
        let phase = self.rollout_phase();
        if phase == self.rollout_published {
            return;
        }
        self.rollout_published = phase;
        if let Some(rollout) = self.rollout_status() {
            events::emit(events::Event::Rollout { app: self.cfg.app.name.clone(), rollout });
        }
    }

    fn send_later(&self, after: Duration, ev: Event) {
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            tokio::time::sleep(after).await;
            let _ = tx.send(ev);
        });
    }

    // ---------------------------------------------------------------- start

    fn start_all(&mut self) {
        // A start (also `restart --hard`, `start` after `stop`) takes the release `current` points to now.
        if let Err(e) = self.pin_release() {
            warn!(
                "cannot pin the release",
                error = e,
                hint = "check working_directory and its `current` symlink; workers can't start until it resolves",
            );
        }
        // Ends with the first worker ready; until then crashes are counted
        // (a spawn that fails below is one too).
        self.start_attempt = Some(StartAttempt::default());
        for id in self.slot_ids() {
            let slot = self.slots.entry(id).or_insert_with(|| Slot::new(id));
            slot.tracker.reset();
            slot.failed_at = None;
            slot.token += 1;
            self.spawn_current(id);
        }
        // Standbys follow once the workers listen (`fill_pool` waits for them).
        self.fill_pool();
        // `[watch]` looks at the files from the moment the workers have read them
        // (a watcher that died is tried again with the workers).
        self.watch_failed = None;
        self.sync_watch();
    }

    /// Spawn the serving instance of a slot. On spawn failure the slot goes
    /// through normal crash handling (backoff, then FAILED).
    fn spawn_current(&mut self, slot_id: usize) -> Option<u64> {
        if self.slots.get(&slot_id).is_none_or(|s| s.removing) {
            return None;
        }
        match self.spawn_instance(slot_id, Role::Current) {
            Ok(inst) => {
                let s = self.slots.get_mut(&slot_id)?;
                s.current = Some(inst);
                s.state = State::Starting;
                Some(inst)
            }
            Err(e) => {
                error!(
                    "failed to start worker",
                    worker = self.label(slot_id),
                    command = self.cfg.app.command,
                    error = e
                );
                self.emit_worker(slot_id, WorkerEvent::Crashed, None, || Some(format!("spawn failed: {e}")));
                if let Some(s) = self.slots.get_mut(&slot_id) {
                    s.last_exit = Some(format!("spawn failed: {e}"));
                }
                self.on_slot_crash(slot_id, Duration::ZERO);
                None
            }
        }
    }

    fn spawn_instance(&mut self, slot_id: usize, role: Role) -> std::io::Result<u64> {
        if role != Role::Standby && self.slots.get(&slot_id).is_none_or(|s| s.removing) {
            return Err(std::io::Error::other(format!("worker {slot_id} no longer exists (scaled down)")));
        }
        let inst_id = self.next_inst;
        self.next_inst += 1;
        self.check_release();
        // A standby: its number in the pool, and its instance number.
        let standby = (role == Role::Standby).then(|| (self.free_standby_number(), self.free_standby_instance()));
        let spec = self.spec(slot_id, inst_id, standby);
        let handle = process::spawn(spec, inst_id, self.proc_tx.clone())?;
        let pid = handle.pid;
        let mut inst = Instance::new(slot_id, handle, role);
        // It starts in Warden's cgroup; `attach_oom` looks again once it is ready.
        inst.oom_counter = self.oom.own();
        let number = standby.map(|(n, _)| n);
        inst.standby_number = number;
        if let (Some(st), Some((_, instance))) = (inst.standby.as_mut(), standby) {
            st.instance = instance;
        }
        self.insts.insert(inst_id, inst);
        self.note_workers();
        if self.is_worker_mode() {
            info!("host starting", pid = pid, workers = self.count, role = role_name(role));
        } else if let Some(n) = number {
            info!("standby starting", worker = standby_label(n), pid = pid);
        } else {
            info!("worker starting", worker = slot_id, pid = pid, role = role_name(role));
        }
        let detail = || (role != Role::Current).then(|| format!("role={}", role_name(role)));
        match number {
            Some(n) => self.emit_standby(n, WorkerEvent::Starting, Some(pid), detail),
            None => self.emit_worker(slot_id, WorkerEvent::Starting, Some(pid), detail),
        }
        if role == Role::Standby {
            // Ready = initialized (`standby_ready`); it never listens before promotion.
            self.send_later(self.cfg.ready_timeout(), Event::ReadyTimeout { inst: inst_id });
        } else {
            self.watch_readiness(inst_id, pid, slot_id);
        }
        Ok(inst_id)
    }

    /// `standby`: the number and instance number of the standby this starts
    /// (`slot_id` is then 0).
    fn spec(&self, slot_id: usize, inst_id: u64, standby: Option<(usize, usize)>) -> process::Spec {
        let a = &self.cfg.app;
        let env = self.worker_env(slot_id, inst_id, standby).into_iter().map(|(k, v, _)| (k, v)).collect();
        let (program, args) = match self.cfg.workers.mode {
            // A standby (slot 0) runs the workers' command; the shim defers
            // its listen until promoted. In the pinned release, as workers
            // (`release.rs`).
            Mode::Process if self.cfg.static_files.is_some() && slot_id != STANDBY_SLOT => {
                (self.exe.display().to_string(), vec!["serve-static".to_string()])
            }
            Mode::Process => {
                let args: Vec<String> = a.args.iter().map(|x| self.pinned_arg(x)).collect();
                (self.pinned_arg(&a.command), with_preload(&a.command, &args, self.shim_path.as_deref()))
            }
            Mode::Worker => {
                let host = self.host_path.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
                (self.pinned_arg(&a.command), vec![host])
            }
        };
        process::Spec {
            program,
            args,
            cwd: self.worker_dir(),
            env,
            label: standby.map(|(n, _)| standby_label(n)).unwrap_or_else(|| self.label(slot_id)),
            output: process::Output::from_config(&self.cfg.logging),
            max_lines_per_sec: self.cfg.logging.max_lines_per_sec,
        }
    }

    /// The variables a worker process starts with on top of the supervisor's
    /// own environment, in the order applied (a later one wins): `env_file`,
    /// `[app] env`, then Warden's (docs/configuration.md, "Environment variables"), each
    /// with where it comes from. `slot_id` 0 with `standby`: a standby.
    /// Also what `warden env` prints, so the two can't drift apart.
    fn worker_env(
        &self,
        slot_id: usize,
        inst_id: u64,
        standby: Option<(usize, usize)>,
    ) -> Vec<(String, String, &'static str)> {
        let a = &self.cfg.app;
        let mut env: Vec<(String, String, &'static str)> = a
            .environment()
            .map(|(k, v)| (k.clone(), v.clone(), if a.env.contains_key(k) { "env" } else { "env_file" }))
            .collect();
        let mut add = |k: &str, v: String| env.push((k.to_string(), v, "warden"));
        add("WARDEN_APP", a.name.clone());
        add("WARDEN_MODE", mode_name(self.cfg.workers.mode).into());
        add("WARDEN_WORKER_COUNT", self.count.to_string());
        add("WARDEN_DRAIN_MS", self.cfg.shutdown.drain_ms.to_string());
        add("WARDEN_LONG_LIVED_MS", self.cfg.long_lived_timeout().as_millis().to_string());
        add("WARDEN_INSTANCE", inst_id.to_string());
        if self.cfg.private_sockets() {
            add("WARDEN_HEALTH_DIR", self.runtime_dir.display().to_string());
        }
        add("WARDEN_STOP_SIGNAL", crate::signals::name(self.cfg.stop_signal()));
        if self.cfg.workers.wait_ready {
            add("WARDEN_WAIT_READY", "1".into());
        }
        if !a.instance_var.is_empty() {
            add("WARDEN_INSTANCE_VAR", a.instance_var.clone());
        }
        // Always: the heartbeat carries the event-loop delay too; the
        // watchdog only acts on it with [watchdog] timeout > 0.
        add("WARDEN_HEARTBEAT_MS", HEARTBEAT_MS.to_string());
        if !self.cfg.metrics.requests {
            // The static server and the shim count responses unless told not to.
            add("WARDEN_REQUESTS", "0".into());
        }
        if self.cfg.workers.port_strategy == PortStrategy::Shared {
            add("WARDEN_REUSE_PORT", "1".into());
        }
        if let Some(p) = a.port {
            // Standbys (slot 0) need a shared port (config validation).
            add("PORT", networking::worker_port(p, self.cfg.workers.port_strategy, slot_id.max(1)).to_string());
        }
        match self.cfg.workers.mode {
            Mode::Process if slot_id == STANDBY_SLOT => {
                // The shim takes the slot's worker id and instance number
                // when promoted.
                add("WARDEN_WORKER_ID", "0".into());
                add("WARDEN_STANDBY", "1".into());
                if let (false, Some((_, instance))) = (a.instance_var.is_empty(), standby) {
                    // Past the workers' numbers and unique: code that runs
                    // only on one instance (cron on 0) does not run in a
                    // standby. A scale-up past it replaces the standby.
                    add(&a.instance_var, instance.to_string());
                }
            }
            Mode::Process => {
                add("WARDEN_WORKER_ID", slot_id.to_string());
                if !a.instance_var.is_empty() {
                    add(&a.instance_var, slot_id.saturating_sub(1).to_string());
                }
                if let Some(st) = &self.cfg.static_files {
                    let mut st = st.clone();
                    st.root = self.pinned_path(st.root);
                    // Where the background compression keeps its copies: one folder per app.
                    st.compress_dir
                        .get_or_insert_with(|| crate::fleet::state_dir().join("compress").join(&self.cfg.app.name));
                    add("WARDEN_STATIC", serde_json::to_string(&st).unwrap_or_default());
                }
            }
            Mode::Worker => {
                // Each Worker gets WARDEN_WORKER_ID and the instance
                // variable from the host and the shim.
                add("WARDEN_WORKERS", self.count.to_string());
                if let Some(shim) = &self.shim_path {
                    add("WARDEN_SHIM", shim.display().to_string());
                }
                add("WARDEN_ENTRY", self.entry_path().display().to_string());
            }
        }
        // Set by `process::spawn` for every child.
        add("WARDEN_IPC_FD", process::IPC_FD.to_string());
        env
    }

    /// `Status.cwd`: where the workers run. For a static site that is the folder it serves
    /// (`[static] root`, resolved against the working directory when relative), since a site
    /// made by `warden serve` has no `working_directory` of its own.
    fn shown_cwd(&self) -> Option<String> {
        let base = self.worker_dir().or_else(|| std::env::current_dir().ok());
        let dir = match &self.cfg.static_files {
            Some(site) => {
                let root = self.pinned_path(site.root.clone());
                if root.is_absolute() { Some(root) } else { base.map(|b| b.join(root)) }
            }
            None => base,
        };
        dir.map(|d| d.display().to_string())
    }

    fn entry_path(&self) -> PathBuf {
        let e = PathBuf::from(self.cfg.app.entry.as_deref().unwrap_or("index.js"));
        if e.is_absolute() {
            return self.pinned_path(e);
        }
        let base = self.worker_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        base.join(e)
    }

    /// How many listeners make an instance ready.
    fn expected_listeners(&self) -> usize {
        if self.is_worker_mode() { self.count } else { 1 }
    }

    /// Readiness: shim `listening` reports (handled in `on_ipc`) or, with a
    /// configured port, the process owning LISTEN sockets on it (Linux /proc).
    /// Without a port and in process mode, a worker is ready once spawned.
    fn watch_readiness(&self, inst: u64, pid: u32, slot_id: usize) {
        let tx = self.tx.clone();
        let deadline = self.cfg.ready_timeout();
        let expected = self.expected_listeners();
        let port = self.cfg.app.port.map(|p| networking::worker_port(p, self.cfg.workers.port_strategy, slot_id));
        let first_worker = slot_id == 1 && self.insts.len() == 1;

        self.send_later(deadline, Event::ReadyTimeout { inst });

        // `wait_ready`: only the app's `process.send('ready')` counts (on_ipc).
        let port = if self.cfg.workers.wait_ready { None } else { port };
        match port {
            None if self.cfg.workers.wait_ready => {}
            // No port: ready once it has stayed up `min_uptime` (a crash on
            // boot then counts as a failed start, not as a running worker).
            None if !self.is_worker_mode() => {
                self.send_later(Duration::from_millis(self.cfg.workers.min_uptime), Event::Ready { inst });
            }
            None => {} // worker mode: wait for every Worker's shim report
            Some(port) => {
                tokio::task::spawn_local(async move {
                    let start = Instant::now();
                    let mut delay = Duration::from_millis(20);
                    while start.elapsed() < deadline {
                        tokio::time::sleep(delay).await;
                        delay = (delay * 2).min(Duration::from_millis(250));
                        match networking::count_listeners(pid, port) {
                            Some(n) if n >= expected => break,
                            Some(_) => continue,
                            // No /proc: a connect probe is only meaningful for the first worker.
                            None if first_worker && networking::port_accepts(port) => break,
                            None => continue,
                        }
                    }
                    if start.elapsed() < deadline {
                        let _ = tx.send(Event::Ready { inst });
                    }
                });
            }
        }
    }

    // --------------------------------------------------------------- events

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Scheduled { token } => self.on_scheduled(token),
            Event::Ready { inst } => self.mark_ready(inst),
            Event::ReadyTimeout { inst } => {
                let timeout = self.cfg.workers.ready_timeout;
                let promote_timeout = standby::PROMOTE_TIMEOUT.min(self.cfg.ready_timeout());
                let label = self.insts.get(&inst).map(|i| self.inst_label(i));
                let app = self.cfg.app.name.clone();
                if let Some(i) = self.insts.get_mut(&inst) {
                    // A standby is ready once initialized; a promoted one has
                    // `promote_timeout` to listen (the spawn-time timer is stale).
                    let late = match (&i.standby, i.promoted_at) {
                        (Some(st), _) => st.ready_at.is_none(),
                        (None, Some(p)) => i.ready_at.is_none() && p.elapsed() >= promote_timeout,
                        (None, None) => i.ready_at.is_none(),
                    };
                    if late && !i.stopping {
                        if i.standby.is_some() {
                            let label = label.unwrap_or_default();
                            error!(
                                "standby not initialized in time; killing",
                                worker = label,
                                pid = i.handle.pid,
                                ready_timeout_s = timeout,
                                hint = format!(
                                    "a standby is ready when the app calls Bun.serve or listen() on app.port (the shim \
                                     holds that call back); its output is in `warden logs {app} --worker {label}`. \
                                     Raise workers.ready_timeout if startup is slow"
                                ),
                            );
                        } else if i.promoted_at.is_some() {
                            error!(
                                "promoted standby did not listen in time; killing",
                                worker = label.unwrap_or_default(),
                                pid = i.handle.pid,
                                standby = i.standby_number.map(standby_label).unwrap_or_default(),
                                timeout_ms = promote_timeout.as_millis(),
                                hint = "the slot restarts the normal way; if this repeats, set [workers] standby = 0 and report it",
                            );
                        } else {
                            error!(
                                "worker not ready in time; killing",
                                worker = label.unwrap_or_default(),
                                pid = i.handle.pid,
                                ready_timeout_s = timeout,
                                hint = "the app must listen on process.env.PORT (or call process.send('ready') with wait_ready) within workers.ready_timeout; `warden logs <app> --worker N` shows why it didn't; raise ready_timeout for slow boots",
                            );
                        }
                        i.timed_out = true;
                        i.handle.signal(libc::SIGKILL);
                    }
                }
            }
            Event::RestartDue { slot, token } => self.on_restart_due(slot, token),
            Event::KillDue { inst } => {
                if let Some(i) = self.insts.get(&inst) {
                    warn!(
                        "worker did not exit within grace period; sending SIGKILL",
                        worker = self.inst_label(i),
                        pid = i.handle.pid,
                        hint = "the app ignored the stop signal or kept running work: close servers and DB pools on SIGTERM (apps written for PM2 listen for SIGINT: set shutdown.signal = \"SIGINT\"), or raise shutdown.grace_period",
                    );
                    i.handle.signal(libc::SIGKILL);
                }
            }
            Event::AppHealth(r) => self.on_app_health(r),
            Event::Ports(seen) => self.on_ports(seen),
            Event::WorkerHealth { inst, result } => self.on_worker_health(inst, result),
            Event::Tick if self.sweep.is_some() => {
                // No worker runs yet, and none may start (a standby would): only
                // systemd's watchdog is fed, and the tick's clock kept, so the
                // first real tick does not report a blocked loop.
                self.last_tick = Instant::now();
                if self.watchdog_enabled {
                    systemd::notify("WATCHDOG=1");
                }
            }
            Event::Tick => {
                self.on_tick();
                // `[watch]` changed by a reload, workers stopped or started again.
                self.sync_watch();
                self.try_watch_restart();
            }
            Event::Watch { token, change } => self.on_watch(token, change),
            Event::Gate(g) => self.on_gate(g),
            Event::Snapshot(reply) => {
                let _ = reply.send(self.status());
            }
            Event::StandbyDue { token } => self.on_standby_due(token),
            Event::StandbyGateDue { inst } => self.standby_gates(inst),
            Event::StandbyChecked { inst, result } => self.on_standby_checked(inst, result),
            Event::StandbyVerified { inst, result } => self.on_standby_verified(inst, result),
        }
    }

    fn on_proc(&mut self, ev: ProcEvent) {
        match ev {
            ProcEvent::Exited { inst, code, signal, note, sent } => self.on_exit(inst, code, signal, note, sent),
            ProcEvent::Ipc { inst, msg } => self.on_ipc(inst, msg),
        }
    }

    fn on_ipc(&mut self, inst_id: u64, msg: IpcMsg) {
        let worker_mode = self.is_worker_mode();
        let expected = self.expected_listeners();
        let (port, strategy) = (self.cfg.app.port, self.cfg.workers.port_strategy);
        let Some(inst) = self.insts.get_mut(&inst_id) else { return };
        let expected_port = port.map(|p| networking::worker_port(p, strategy, inst.slot.max(1)));
        // Process mode: one worker per process, keyed by its slot now (a
        // promoted standby's last pre-promotion heartbeat may arrive after it
        // changed slots, still saying worker 0).
        let worker = if worker_mode { msg.worker.unwrap_or(0) } else { inst.slot };
        match msg.ev.as_str() {
            "heartbeat" => {
                let now = Instant::now();
                inst.heartbeats.insert(worker, now);
                if let Some(counts) = msg.requests() {
                    self.count_requests(inst_id, worker, counts);
                }
                let Some(inst) = self.insts.get_mut(&inst_id) else { return };
                let Some(d) = msg.loop_delay() else { return };
                let warn_ms = self.cfg.watchdog.loop_delay_warn * 1000.0;
                let state = inst.loop_delay.entry(worker).or_insert_with(|| LoopState::new(d, now));
                if state.observe(d, now, warn_ms) {
                    // The process (`s1`: a standby), or in worker mode the Worker thread.
                    let who = match inst.standby_number.filter(|_| inst.role == Role::Standby) {
                        _ if worker_mode => worker.to_string(),
                        Some(n) => standby_label(n),
                        None => inst.slot.to_string(),
                    };
                    warn!(
                        "worker event loop delay is high",
                        worker = who,
                        pid = inst.handle.pid,
                        p99_ms = d.p99_ms,
                        max_ms = d.max_ms,
                        for_s = state.high,
                        threshold_ms = warn_ms,
                        hint = "requests wait this long before their handler starts: synchronous work on the event \
                                loop (large JSON, sync fs or crypto, a CPU-heavy route) or a starved host (`warden \
                                top`, the host's load). Warden only reports it (the watchdog acts when heartbeats \
                                stop). Profile it (node --cpu-prof, bun --inspect), move heavy work to a Worker, or \
                                add workers; [watchdog] loop_delay_warn sets the threshold (0 = off)",
                    );
                }
            }
            "listening" => {
                // Other servers the app may start (metrics, admin) don't count.
                if expected_port.is_some() && msg.port.is_some() && msg.port != expected_port {
                    debug!("ignoring listener on another port", port = msg.port.unwrap_or(0));
                    return;
                }
                if inst.role == Role::Standby {
                    // Its listen should have been held back: it takes traffic.
                    let pid = inst.handle.pid;
                    self.disable_pool(format!("standby pid {pid} reported listening before its promotion"));
                    return;
                }
                let promoted = inst.promoted_at.is_some();
                if let Some(sock) = &msg.socket {
                    inst.sockets.insert(worker, PathBuf::from(sock));
                }
                let ready = if worker_mode {
                    let t = inst.threads.entry(worker).or_default();
                    let newly = !t.listening;
                    t.listening = true;
                    debug!("worker listening", worker = worker, port = msg.port.unwrap_or(0), pid = inst.handle.pid);
                    if newly {
                        let ms = inst.started.elapsed().as_millis();
                        emit(&self.cfg.app.name, worker, WorkerEvent::Ready, Some(inst.handle.pid), || {
                            Some(format!("startup_ms={ms}"))
                        });
                    }
                    inst.threads_listening() >= expected
                } else {
                    inst.listening.insert(msg.port.unwrap_or(0));
                    true
                };
                // A promoted standby was initialized before: listening is all
                // it has left to do, even with wait_ready.
                if ready && (!self.cfg.workers.wait_ready || promoted) {
                    self.mark_ready(inst_id);
                }
            }
            "standby_ready" => self.on_standby_ready(inst_id, msg.socket.clone()),
            "ready" => {
                if self.cfg.workers.wait_ready {
                    debug!("app reported ready", worker = worker, pid = inst.handle.pid);
                    self.mark_ready(inst_id);
                }
            }
            "exit" if worker_mode => {
                let why = describe_exit(msg.code, None);
                inst.heartbeats.remove(&worker);
                inst.sockets.remove(&worker);
                let t = inst.threads.entry(worker).or_insert_with(ThreadInfo::default);
                t.listening = false;
                t.last_exit = Some(why.clone());
                let unexpected = !msg.expected.unwrap_or(false) && !inst.stopping;
                if unexpected {
                    t.crashed = true;
                    t.crashes += 1;
                    let (slot, role, pid) = (inst.slot, inst.role, inst.handle.pid);
                    warn!(
                        "worker thread crashed",
                        worker = worker,
                        reason = why,
                        host_pid = pid,
                        hint = "an uncaught error or process.exit() in that Worker (the `worker thread error` line \
                                before it, or `warden logs <app>`); the host keeps serving with its other Workers \
                                while Warden starts a replacement host",
                    );
                    emit(&self.cfg.app.name, worker, WorkerEvent::Crashed, Some(pid), || Some(why.clone()));
                    if role == Role::Current && !self.shutting_down && !self.stopped {
                        self.on_thread_crash(slot, false);
                    } else if role == Role::Replacement && self.rollout_new_instance(slot) == Some(inst_id) {
                        self.fail_rollout(format!("Worker {worker} of the new host crashed ({why})"));
                    }
                }
            }
            "error" if worker_mode => {
                warn!(
                    "worker thread error",
                    worker = worker,
                    message = msg.message.unwrap_or_default(),
                    hint = "an uncaught exception in the app's code (message= has it); the Worker dies with it and \
                            Warden replaces the host. Fix the error, or catch it in the app",
                );
            }
            "draining" => debug!("draining", worker = worker),
            "long_lived_closed" => info!(
                "closed long-lived connections so their clients reconnect to new workers",
                worker = worker,
                pid = inst.handle.pid,
                websockets = msg.ws.unwrap_or(0),
                sse = msg.sse.unwrap_or(0),
            ),
            other => debug!("unknown IPC event", ev = other),
        }
    }

    fn mark_ready(&mut self, inst_id: u64) {
        let lifetime = self.cfg.limits.max_lifetime;
        // Ready by listening on its port (found open, or the shim's report).
        let port = self.cfg.app.port.filter(|_| !self.cfg.workers.wait_ready);
        let strategy = self.cfg.workers.port_strategy;
        let Some(inst) = self.insts.get_mut(&inst_id) else { return };
        // A standby is ready once initialized (`standby_ready`), not here.
        if inst.ready_at.is_some() || inst.stopping || inst.role == Role::Standby {
            return;
        }
        let now = Instant::now();
        inst.ready_at = Some(now);
        // It listened, so `[watchdog] port_lost` watches it from now on, even
        // if its server closes before the first look at its sockets.
        if port.is_some() || !inst.listening.is_empty() {
            inst.listen.seen_at = Some(now);
            inst.listen.ports = match port {
                Some(p) => vec![networking::worker_port(p, strategy, inst.slot.max(1))],
                None => inst.listening.iter().copied().filter(|p| *p != 0).collect(),
            };
        }
        if lifetime > 0 {
            inst.recycle_at =
                Some(crate::restart::later(now, upkeep::jittered(Duration::from_secs(lifetime), inst_id)));
        }
        let ms = inst.started.elapsed().as_millis();
        // Promoted standby: from the promotion to listening.
        let promote_ms = inst.promoted_at.map(|p| format!("{:.1}", p.elapsed().as_secs_f64() * 1000.0));
        let (slot_id, role, pid) = (inst.slot, inst.role, inst.handle.pid);
        // The standby it was (`--worker sN` shows its whole story).
        let was = inst.standby_number.map(standby_label).unwrap_or_default();
        match role {
            Role::Current => {
                // The app can start: a worker made it.
                self.start_attempt = None;
                if let Some(s) = self.slots.get_mut(&slot_id) {
                    s.state = State::Running;
                }
                if self.is_worker_mode() {
                    info!("host ready", pid = pid, workers = self.count, startup_ms = ms);
                } else if let Some(pms) = &promote_ms {
                    info!("worker promoted from standby", worker = slot_id, pid = pid, standby = was, promote_ms = pms);
                } else {
                    info!("worker ready", worker = slot_id, pid = pid, startup_ms = ms);
                }
                self.emit_worker(slot_id, WorkerEvent::Ready, Some(pid), || match &promote_ms {
                    Some(pms) => Some(format!("promoted from standby {was} promote_ms={pms}")),
                    None => Some(format!("startup_ms={ms}")),
                });
            }
            Role::Replacement => {
                if let Some(pms) = &promote_ms {
                    info!(
                        "standby promoted as the replacement; verifying it",
                        worker = self.label(slot_id),
                        pid = pid,
                        standby = was,
                        promote_ms = pms
                    );
                } else {
                    info!("replacement listening", worker = self.label(slot_id), pid = pid, startup_ms = ms);
                }
                self.emit_worker(slot_id, WorkerEvent::Ready, Some(pid), || match &promote_ms {
                    Some(pms) => Some(format!("promoted from standby {was} promote_ms={pms} role=replacement")),
                    None => Some(format!("startup_ms={ms} role=replacement")),
                });
            }
            Role::Retiring | Role::Standby => {}
        }
        self.attach_oom(inst_id);
        self.rollout_on_ready(inst_id);
        self.check_all_ready();
        // A worker listens: start the standbys that waited for it.
        self.fill_pool();
    }

    /// Where this process's OOM kills are counted: in Warden's cgroup, or a
    /// cgroup of its own it runs in once ready (a `command` that wraps the
    /// app in `systemd-run --scope`, a runtime that moves it). Warden makes
    /// no cgroups; this finds the one it is in.
    fn attach_oom(&mut self, inst_id: u64) {
        let Some(i) = self.insts.get(&inst_id) else { return };
        let (pid, label) = (i.handle.pid, self.inst_label(i));
        let counter = self.oom.attach(pid);
        if counter != self.oom.own() {
            match &counter {
                Some(file) => debug!(
                    "this process runs in a cgroup of its own: its OOM kills are counted there",
                    worker = label,
                    pid = pid,
                    file = file.display(),
                ),
                None => debug!(
                    "this process runs in a cgroup whose OOM kill count can't be read: its OOM kills read as SIGKILLs",
                    worker = label,
                    pid = pid,
                ),
            }
        }
        if let Some(i) = self.insts.get_mut(&inst_id) {
            i.oom_counter = counter;
        }
    }

    fn check_all_ready(&mut self) {
        let all = !self.slots.is_empty() && self.slots.values().all(|s| s.state == State::Running);
        if all && !self.announced_ready {
            self.announced_ready = true;
            info!("all workers ready", workers = self.count, startup_ms = self.started.elapsed().as_millis());
            systemd::notify("READY=1\nSTATUS=all workers ready");
        }
    }

    fn on_exit(
        &mut self,
        inst_id: u64,
        code: Option<i32>,
        signal: Option<i32>,
        note: Option<String>,
        sent: process::exit::Sent,
    ) {
        let Some(inst) = self.insts.remove(&inst_id) else { return };
        self.note_workers();
        for sock in inst.sockets.values() {
            let _ = std::fs::remove_file(sock);
        }
        // Who ended it: Warden, the OOM killer (just before), someone else, a
        // crash. Its cgroup's OOM kill is its own only if no other process of
        // Warden's there is dying of SIGKILL at this moment to share it.
        let counter = inst.oom_counter.clone();
        let (oom, insts) = (&mut self.oom, &self.insts);
        let verdict =
            oom.verdict(counter.as_deref(), process::exit::kill_signal(code, signal), sent, Instant::now(), || {
                insts.values().filter(|o| o.oom_counter == counter && o.handle.dying_of_sigkill()).count()
            });
        let cause = process::exit::classify(code, signal, sent, self.cfg.stop_signal(), verdict);
        // A `note` (Warden lost track of it) is the whole story.
        let hint = if note.is_some() { None } else { cause.hint(counter.is_some()) };
        // The WARN/ERROR line of a crash always says what to do, even when
        // the cause alone has nothing to add (an exit code, Warden's kill).
        let crash_hint = hint.unwrap_or(if inst.hung {
            "its event loop stopped (the `worker hung` line before it); Warden restarts it. Its last output is in \
             `warden logs <app> --worker N`"
        } else if inst.port_lost {
            "it stopped listening (the `worker stopped listening` line before it); Warden restarts it with backoff. \
             Its last output is in `warden logs <app> --worker N`"
        } else if inst.timed_out {
            "it did not listen on its port within workers.ready_timeout (the `not ready in time` line before it); \
             Warden restarts it with backoff"
        } else if note.is_some() {
            "Warden lost track of this process (the reason says how); it starts a new one"
        } else {
            cause.plain_hint()
        });
        let why = cause.short();
        let slot_id = inst.slot;
        let label = self.label(slot_id);
        let uptime = inst.started.elapsed();
        let is_current = self.slots.get(&slot_id).is_some_and(|s| s.current == Some(inst_id));
        let reason = if let Some(n) = note {
            n
        } else if inst.timed_out {
            "not ready in time".to_string()
        } else if inst.hung {
            format!("hung: no heartbeat for {}s ({why})", self.cfg.watchdog.timeout)
        } else if inst.port_lost {
            format!(
                "stopped listening: no listener on {} for {}s ({why})",
                portwatch::ports_text(&inst.listen.ports),
                self.cfg.watchdog.port_lost
            )
        } else {
            why.clone()
        };

        if inst.role == Role::Standby {
            self.on_standby_exit(&inst, why, reason.clone(), hint);
        } else if inst.restart_on_exit && is_current && !self.shutting_down && !self.stopped {
            // A failed rollout, not a crash of this slot: restart right away on
            // the restored config, without counting it towards the restart limit.
            warn!(
                "worker started by a rollout that failed has stopped; restarting it with the previous config",
                worker = label,
                pid = inst.handle.pid,
                hint = "nothing to undo: the rollout's failure is reported above; fix what it says and deploy again",
            );
            self.emit_worker(slot_id, WorkerEvent::Restarting, Some(inst.handle.pid), || {
                Some("its rollout failed; restarting now".into())
            });
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = None;
                s.state = State::Restarting;
                s.last_exit = Some("stopped: its rollout failed".into());
                s.restarts += 1;
                s.token += 1;
            }
            self.spawn_current(slot_id);
        } else if inst.stopping || inst.role == Role::Retiring {
            match hint {
                // Killed while draining: by the OOM killer, a crash, someone else.
                Some(h) => warn!("worker stopped", worker = label, pid = inst.handle.pid, reason = why, hint = h),
                None => info!("worker stopped", worker = label, pid = inst.handle.pid, reason = why),
            }
            self.emit_worker(slot_id, WorkerEvent::Stopped, Some(inst.handle.pid), || Some(why.clone()));
            if is_current {
                let mut remove = false;
                // Stopped after a start that failed: the crash says why, not the stop.
                let keep_crash = self.start_failed().is_some();
                if let Some(s) = self.slots.get_mut(&slot_id) {
                    s.current = None;
                    s.state = State::Stopped;
                    if !keep_crash {
                        s.last_exit = Some(why);
                    }
                    remove = s.removing;
                }
                if remove {
                    self.slots.remove(&slot_id);
                }
            }
        } else if inst.role == Role::Replacement {
            error!(
                "replacement exited before taking over",
                worker = label,
                pid = inst.handle.pid,
                reason = reason,
                hint = crash_hint
            );
            self.emit_worker(slot_id, WorkerEvent::Crashed, Some(inst.handle.pid), || {
                Some(format!("{reason} (replacement, before taking over)"))
            });
        } else if is_current
            && signal.is_none()
            && code.is_some_and(|c| self.cfg.restart.stop_exit_codes.contains(&c))
            && !inst.port_lost
            && !self.shutting_down
        {
            info!(
                "worker finished; not restarting",
                worker = label,
                pid = inst.handle.pid,
                reason = why,
                hint = "this exit code is listed in restart.stop_exit_codes; `warden restart` starts it again",
            );
            self.emit_worker(slot_id, WorkerEvent::Exited, Some(inst.handle.pid), || Some(why.clone()));
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = None;
                s.state = State::Stopped;
                s.last_exit = Some(format!("{why} (done)"));
                s.token += 1;
            }
        } else if is_current {
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = None;
                s.crashes += 1;
                s.state = if self.shutting_down || self.stopped { State::Stopped } else { State::Crashed };
                s.last_exit = Some(reason.clone());
            }
            if self.shutting_down || self.stopped {
                info!("worker exited", worker = label, pid = inst.handle.pid, reason = reason);
                self.emit_worker(slot_id, WorkerEvent::Stopped, Some(inst.handle.pid), || Some(reason.clone()));
            } else {
                warn!(
                    "worker crashed",
                    worker = label,
                    pid = inst.handle.pid,
                    reason = reason,
                    uptime_s = uptime.as_secs(),
                    hint = crash_hint
                );
                self.emit_worker(slot_id, WorkerEvent::Crashed, Some(inst.handle.pid), || Some(reason.clone()));
                self.on_slot_crash(slot_id, uptime);
            }
        }

        self.rollout_on_exit(inst_id, &reason);

        if self.stopped && self.start_after_stop && !self.shutting_down && self.insts.is_empty() {
            self.start_after_stop = false;
            self.stopped = false;
            info!("starting all workers");
            self.start_all();
        }
    }

    /// Worker mode: a Worker died (or hung) but the host lives on (degraded).
    /// Replace the host: after backoff for a crash, immediately for a hang
    /// (a hung Worker's listener black-holes connections until it is gone).
    fn on_thread_crash(&mut self, slot_id: usize, hung: bool) {
        if hung {
            self.request_replace(slot_id, "a Worker thread is hung".into(), true);
            return;
        }
        let pending = self.slots.get(&slot_id).is_some_and(|s| s.state == State::Restarting)
            || self.roll.as_ref().is_some_and(|r| r.kind == Kind::Recovery);
        if pending {
            return;
        }
        let uptime = self
            .slots
            .get(&slot_id)
            .and_then(|s| s.current)
            .and_then(|i| self.insts.get(&i))
            .map(|i| i.started.elapsed());
        if let Some(s) = self.slots.get_mut(&slot_id) {
            s.crashes += 1;
        }
        self.on_slot_crash(slot_id, uptime.unwrap_or_default());
    }

    /// A slot crashed while no worker has been ready since the workers were
    /// started. Once every slot has, the app can't start as it is: one ERROR
    /// line says so and why. The restart policy goes on as configured (a
    /// dependency may come back); an interactive `warden start` stops the
    /// workers instead (it sees `Status.start_failed`).
    fn note_start_crash(&mut self, slot_id: usize) {
        let last = self.slots.get(&slot_id).and_then(|s| s.last_exit.clone());
        let ids = self.slot_ids();
        let Some(a) = self.start_attempt.as_mut() else { return };
        *a.crashes.entry(slot_id).or_insert(0) += 1;
        if last.is_some() {
            a.last_exit = last;
        }
        if a.reported || !a.every_slot_crashed(&ids) {
            return;
        }
        a.reported = true;
        let reason = a.last_exit.clone().unwrap_or_default();
        let app = &self.cfg.app.name;
        let why = match self.cfg.app.port {
            Some(p) if reason == "not ready in time" => format!(
                "no worker listened on port {p} within workers.ready_timeout ({} s): another program on the port \
                 (`ss -ltnp 'sport = :{p}'`), an app that doesn't listen on process.env.PORT, or a slow boot (raise \
                 ready_timeout); its output: `warden logs {app}`",
                self.cfg.workers.ready_timeout
            ),
            _ => format!("the app's own error output says why: `warden logs {app} --err`"),
        };
        error!(
            "app cannot start: every worker crashed before it was ready",
            workers = ids.len(),
            crashes_each = START_FAIL_CRASHES,
            reason = reason,
            hint = format!(
                "{why}. Warden keeps restarting it with backoff (FAILED after restart.max_restarts in \
                 restart.restart_window, then retried after restart.failed_cooldown); fix the cause, then `warden \
                 restart {app}`, or `warden stop {app}` to stop trying"
            ),
        );
    }

    /// `Status.start_failed`: every worker crashed since the workers were
    /// started and none was ready; the last crash's reason.
    fn start_failed(&self) -> Option<String> {
        let a = self.start_attempt.as_ref()?;
        let ids = self.slot_ids();
        a.every_slot_crashed(&ids).then(|| a.last_exit.clone().unwrap_or_else(|| "crashed".into()))
    }

    fn on_slot_crash(&mut self, slot_id: usize, uptime: Duration) {
        if self.shutting_down || self.stopped {
            return;
        }
        if self.slots.get(&slot_id).is_some_and(|s| s.removing) {
            self.slots.remove(&slot_id);
            return;
        }
        // A replacement for this slot is already on its way (rollout): it
        // becomes the slot's worker now; if it then fails its gates it is
        // stopped and the slot restarted (see `fail_rollout`).
        let current_gone = self.slots.get(&slot_id).is_some_and(|s| s.current.is_none());
        if let Some(new) = self.rollout_new_instance(slot_id).filter(|_| current_gone) {
            let ready = self.insts.get(&new).is_some_and(|i| i.ready_at.is_some());
            if let Some(i) = self.insts.get_mut(&new) {
                i.role = Role::Current;
            }
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = Some(new);
                s.state = if ready { State::Running } else { State::Starting };
            }
            info!("old worker gone; its replacement takes over the slot", worker = self.label(slot_id));
            return;
        }
        self.note_start_crash(slot_id);
        let label = self.label(slot_id);
        let wid = self.event_worker(slot_id);
        let policy = self.policy.clone();
        let cooldown = self.cfg.restart.failed_cooldown;
        let Some(s) = self.slots.get_mut(&slot_id) else { return };
        match s.tracker.on_crash(&policy, Instant::now(), uptime) {
            Decision::RestartAfter(d) => {
                s.state = State::Restarting;
                s.token += 1;
                let (slot, token) = (slot_id, s.token);
                info!(
                    "worker restarting",
                    worker = label,
                    in_ms = d.as_millis(),
                    attempt = s.tracker.restarts_in_window()
                );
                emit(&self.cfg.app.name, wid, WorkerEvent::Restarting, None, || {
                    Some(format!("in_ms={}", d.as_millis()))
                });
                if d.is_zero() {
                    self.on_restart_due(slot, token);
                } else {
                    self.send_later(d, Event::RestartDue { slot, token });
                }
            }
            Decision::GiveUp => {
                s.state = State::Failed;
                s.failed_at = Some(Instant::now());
                if policy.enabled {
                    let retry = if cooldown > 0 { format!("retrying in {cooldown}s") } else { "not retrying".into() };
                    error!(
                        "worker failed: too many restarts",
                        worker = label,
                        max_restarts = policy.max_restarts,
                        window_s = policy.window.as_secs(),
                        next = retry,
                        hint = if self.cfg.workers.mode == Mode::Worker {
                            "fix the cause, then run `warden reload`".to_string()
                        } else {
                            format!("fix the cause, then run `warden restart {slot_id}`")
                        },
                    );
                    emit(&self.cfg.app.name, wid, WorkerEvent::Failed, None, || {
                        Some(format!(
                            "too many restarts ({} in {}s); {retry}",
                            policy.max_restarts,
                            policy.window.as_secs()
                        ))
                    });
                } else {
                    error!("worker exited and restarts are disabled", worker = label);
                    emit(&self.cfg.app.name, wid, WorkerEvent::Failed, None, || Some("restarts are disabled".into()));
                }
            }
        }
    }

    fn on_restart_due(&mut self, slot_id: usize, token: u64) {
        if self.shutting_down || self.stopped {
            return;
        }
        let Some(s) = self.slots.get_mut(&slot_id) else { return };
        if s.token != token || s.state != State::Restarting {
            return;
        }
        if s.current.is_some() {
            // Worker mode, degraded host: bring up a replacement next to it.
            if self.roll.is_some() {
                // Busy (e.g. a reload's preflight): try again shortly instead of dropping it.
                self.send_later(Duration::from_secs(1), Event::RestartDue { slot: slot_id, token });
            } else if let Err(e) =
                self.begin_rollout(Kind::Recovery, vec![slot_id], "Worker thread crashed".into(), false)
            {
                warn!("recovery could not start; retrying", worker = self.label(slot_id), reason = e);
                self.send_later(Duration::from_secs(1), Event::RestartDue { slot: slot_id, token });
            }
            return;
        }
        s.restarts += 1;
        // A hot standby takes the slot in milliseconds; else a cold start.
        if self.promote_standby(slot_id, Role::Current).is_some() {
            return;
        }
        self.spawn_current(slot_id);
    }

    // ------------------------------------------------------------- stopping

    fn stop_instance(&mut self, inst_id: u64) {
        let grace = self.cfg.grace_period();
        let stop_signal = self.cfg.stop_signal();
        let Some(i) = self.insts.get(&inst_id) else { return };
        let who = self.event_who(i);
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        if i.stopping {
            return;
        }
        i.stopping = true;
        i.handle.signal_group(stop_signal);
        if i.role == Role::Standby {
            // Never promoted now: end the shim's pending read of commands
            // (EOF), so nothing it runs on can hold up the exit.
            i.handle.close_input();
        }
        emit_to(&self.cfg.app.name, who, WorkerEvent::Stopping, Some(i.handle.pid), || {
            Some(format!("{} grace_s={}", crate::signals::name(stop_signal), grace.as_secs()))
        });
        if let Some(s) = self.slots.get_mut(&i.slot) {
            if s.current == Some(inst_id) {
                s.state = State::Stopping;
            }
        }
        self.send_later(grace, Event::KillDue { inst: inst_id });
    }

    fn kill_instance(&mut self, inst_id: u64) {
        let Some(who) = self.insts.get(&inst_id).map(|i| self.event_who(i)) else { return };
        if let Some(i) = self.insts.get_mut(&inst_id) {
            i.stopping = true;
            i.handle.signal(libc::SIGKILL);
            emit_to(&self.cfg.app.name, who, WorkerEvent::Stopping, Some(i.handle.pid), || Some("SIGKILL".into()));
        }
    }

    fn stop_all(&mut self) {
        self.abort_rollout("workers are being stopped");
        self.pending_replace.clear();
        let worker_mode = self.is_worker_mode();
        for s in self.slots.values_mut() {
            s.token += 1;
            if s.current.is_none() && s.state != State::Failed {
                // Waiting to restart (or crashed): nothing to stop, it just stays down.
                if s.state != State::Stopped {
                    let wid = if worker_mode { 0 } else { s.id };
                    emit(&self.cfg.app.name, wid, WorkerEvent::Stopped, None, || {
                        Some("stopped before its restart".into())
                    });
                }
                s.state = State::Stopped;
            }
        }
        let ids: Vec<u64> = self.insts.keys().copied().collect();
        for id in ids {
            self.stop_instance(id);
        }
    }

    fn begin_shutdown(&mut self, why: &str) {
        if self.shutting_down {
            if !self.force_kill {
                self.force_kill = true;
                warn!("second shutdown request: killing workers now");
                for i in self.insts.values() {
                    i.handle.signal(libc::SIGKILL);
                }
            }
            return;
        }
        info!("shutting down", reason = why, workers = self.insts.len(), grace_s = self.cfg.shutdown.grace_period);
        systemd::notify("STOPPING=1");
        self.shutting_down = true;
        self.shutdown_reason = why.to_string();
        self.start_after_stop = false;
        // No scan outlives the shutdown, and no restart starts during it.
        self.watch = None;
        self.watch_pending = None;
        self.stop_all();
    }

    fn on_signal(&mut self, s: Sig) {
        match s {
            Sig::Term | Sig::Int => self.begin_shutdown(s.name()),
            Sig::Hup => {
                let r = self.request_reload(false);
                if !r.ok {
                    warn!(
                        "SIGHUP ignored",
                        reason = r.message.unwrap_or_default(),
                        hint = "SIGHUP reloads the workers once they run and nothing else is under way; `warden reload <app>` says why when it cannot",
                    );
                }
            }
            Sig::Usr1 | Sig::Usr2 => {
                info!("forwarding signal to workers", signal = s.name());
                for i in self.insts.values() {
                    i.handle.signal(s.raw());
                }
            }
        }
    }

    // --------------------------------------------------------------- health

    /// App-level check through the shared port (`health.url`).
    fn on_app_health(&mut self, r: Result<u16, String>) {
        if !self.announced_ready || self.shutting_down || self.stopped {
            return;
        }
        let threshold = self.cfg.health.failure_threshold;
        match r {
            Ok(code) => {
                if self.app_health.healthy == Some(false) {
                    info!("application healthy again", status = code);
                }
                self.app_health.healthy = Some(true);
                self.app_health.failures = 0;
            }
            Err(e) => {
                self.app_health.failures += 1;
                warn!(
                    "app health check failed",
                    url = self.cfg.health.url,
                    error = e,
                    consecutive = self.app_health.failures
                );
                if self.app_health.failures == threshold {
                    self.app_health.healthy = Some(false);
                    error!("application unhealthy", failures = threshold);
                    if self.cfg.health.on_failure == OnHealthFailure::Reload {
                        let r = self.request_reload(false);
                        if !r.ok {
                            warn!("health-triggered reload skipped", reason = r.message.unwrap_or_default());
                        }
                    }
                }
            }
        }
    }

    /// Per-worker check over the worker's private socket (liveness path).
    fn on_worker_health(&mut self, inst_id: u64, result: Result<(), String>) {
        let threshold = self.cfg.health.failure_threshold;
        let on_failure = self.cfg.health.on_failure;
        let worker_mode = self.is_worker_mode();
        let Some(inst) = self.insts.get_mut(&inst_id) else { return };
        inst.health_inflight = false;
        if inst.role == Role::Standby {
            return self.on_standby_health(inst_id, result);
        }
        if inst.stopping || inst.role != Role::Current {
            return;
        }
        let (slot, pid) = (inst.slot, inst.handle.pid);
        let mut replace = None;
        match result {
            Ok(()) => {
                if inst.healthy == Some(false) {
                    info!("worker healthy again", worker = slot, pid = pid);
                }
                inst.healthy = Some(true);
                inst.health_fails = 0;
            }
            Err(e) => {
                inst.health_fails += 1;
                // First failure: WARN. Repeats stay at DEBUG until the
                // threshold turns it into one ERROR (no log storm, A5).
                if inst.health_fails == 1 {
                    warn!(
                        "worker health check failed",
                        worker = slot,
                        pid = pid,
                        error = e,
                        threshold = threshold,
                        hint = "Warden acts after `threshold` failures in a row ([health] failure_threshold)",
                    );
                } else {
                    debug!(
                        "worker health check failed again",
                        worker = slot,
                        pid = pid,
                        error = e,
                        consecutive = inst.health_fails
                    );
                }
                if inst.health_fails >= threshold {
                    if inst.healthy != Some(false) {
                        let action = match on_failure {
                            OnHealthFailure::Log => "logging only ([health] on_failure = \"log\")",
                            _ => "replacing it gracefully",
                        };
                        error!(
                            "worker unhealthy",
                            worker = slot,
                            pid = pid,
                            failures = inst.health_fails,
                            error = e,
                            action = action,
                        );
                        let (wid, fails) = (if worker_mode { 0 } else { slot }, inst.health_fails);
                        emit(&self.cfg.app.name, wid, WorkerEvent::Unhealthy, Some(pid), || {
                            Some(format!("failed {fails} health checks: {e}"))
                        });
                    }
                    inst.healthy = Some(false);
                    // Re-trigger every `threshold` failures in case a replacement failed.
                    if (inst.health_fails - threshold) % threshold == 0 && on_failure != OnHealthFailure::Log {
                        replace = Some(format!("failed {} health checks", inst.health_fails));
                    }
                }
            }
        }
        self.update_outage();
        if let Some(reason) = replace {
            if self.outage {
                debug!("replacement held: fleet-wide health failure", worker = slot);
            } else {
                self.request_replace(slot, reason, false);
            }
        }
    }

    /// DS2: when a large share of workers fail their checks at once, the cause
    /// is almost always a shared dependency (database, network), and replacing
    /// workers only adds reconnect pressure. Hold replacements until it clears.
    fn update_outage(&mut self) {
        let threshold = self.cfg.health.outage_threshold;
        let live: Vec<&Instance> =
            self.insts.values().filter(|i| i.role == Role::Current && !i.stopping && i.ready_at.is_some()).collect();
        let failing = live.iter().filter(|i| i.healthy == Some(false)).count();
        // One worker can't tell a dependency outage from a broken worker.
        let outage = threshold < 1.0 && live.len() >= 2 && failing as f64 >= threshold * live.len() as f64;
        if outage && !self.outage {
            error!(
                "dependency outage suspected: holding worker replacements",
                failing = failing,
                workers = live.len(),
                outage_threshold = threshold,
            );
        } else if !outage && self.outage {
            info!("fleet health recovered: replacements resumed", failing = failing, workers = live.len());
        }
        self.outage = outage;
    }

    // -------------------------------------------------------------- control

    /// Arm the next `[restart] schedule` run (PM2's `cron_restart`).
    fn schedule_next(&mut self) {
        self.schedule_token += 1;
        let Some(expr) = self.cfg.restart.schedule.clone() else { return };
        let cron = match crate::schedule::Cron::parse(&expr) {
            Ok(c) => c,
            Err(e) => {
                error!("restart.schedule is invalid; scheduled restarts are off", schedule = expr, error = e);
                return;
            }
        };
        let now = std::time::SystemTime::now();
        let Some(at) = cron.next_after(now) else { return };
        let wait = at.duration_since(now).unwrap_or_default();
        info!(
            "next scheduled restart",
            schedule = expr,
            at = crate::logging::format_rfc3339(
                at.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0),
                0
            ),
            in_s = wait.as_secs()
        );
        self.send_later(wait, Event::Scheduled { token: self.schedule_token });
    }

    fn on_scheduled(&mut self, token: u64) {
        if token != self.schedule_token {
            return;
        }
        if self.shutting_down {
            return;
        }
        if self.stopped {
            info!("scheduled restart skipped: workers are stopped");
        } else if self.roll.is_some() {
            // Busy: try again in a minute rather than skipping a day.
            info!("scheduled restart postponed: a rollout is in progress", retry_s = 60);
            self.send_later(Duration::from_secs(60), Event::Scheduled { token });
            return;
        } else {
            let ids: Vec<usize> = self.slots.values().filter(|s| !s.removing).map(|s| s.id).collect();
            match self.begin_rollout(Kind::Restart, ids, "scheduled".into(), false) {
                Ok(_) => info!(
                    "scheduled rolling restart started",
                    schedule = self.cfg.restart.schedule.clone().unwrap_or_default()
                ),
                Err(e) => warn!("scheduled restart could not start", reason = e),
            }
        }
        self.schedule_next();
    }

    // ---------------------------------------------------------------- watch

    /// What `[watch]` asks for now: None when it is off or the workers do not
    /// run (nothing to restart), an error when it cannot say where to look.
    fn watch_spec(&self) -> Result<Option<watch::Spec>, String> {
        let w = &self.cfg.watch;
        if !w.enabled || self.stopped || self.shutting_down {
            return Ok(None);
        }
        // Relative paths are relative to where Warden was started, as they are for the workers.
        let abs = |p: &Path, what: &str| -> Result<PathBuf, String> {
            if p.is_absolute() {
                return Ok(p.to_path_buf());
            }
            match &self.launch_cwd {
                Some(cwd) => Ok(cwd.join(p)),
                None => Err(format!(
                    "{what} {} is relative to the directory Warden was started in, which cannot be read (deleted?)",
                    p.display()
                )),
            }
        };
        let base = abs(self.cfg.app.working_directory.as_deref().unwrap_or(Path::new(".")), "the working directory")?;
        // What Warden writes itself is never a change to restart for: its log
        // files (and their rotations, and each worker's with per_worker_files),
        // its control socket, scripts and health sockets.
        let l = &self.cfg.logging;
        let mut skip: Vec<watch::Skip> = Vec::new();
        for p in [&l.file, &l.out_file, &l.err_file].into_iter().flatten() {
            skip.extend(skip_entries(abs(p, "the log file")?, l.per_worker_files));
        }
        skip.extend(skip_entries(abs(&self.cfg.socket_path(), "the control socket")?, false));
        let watched: Vec<PathBuf> =
            [base.clone()].into_iter().chain(w.paths.iter().map(|p| base.join(p))).map(|p| watch::clean(&p)).collect();
        // The runtime directory as written; by its real path when that differs
        // (a symlink); and by that real path as the watcher spells it, under a
        // watched directory as written: the walk does not resolve symlinks, and
        // a directory above both may be one (on macOS the temporary directory
        // is under /var, which is /private/var).
        let runtime = abs(&self.runtime_dir, "the runtime directory")?;
        let mut runtimes = vec![runtime.clone()];
        if let Ok(real) = std::fs::canonicalize(&runtime) {
            for dir in &watched {
                let Ok(real_dir) = std::fs::canonicalize(dir) else { continue };
                if let Ok(rest) = real.strip_prefix(&real_dir) {
                    runtimes.push(dir.join(rest));
                }
            }
            runtimes.push(real);
        }
        let mut seen = std::collections::HashSet::new();
        runtimes.retain(|p| seen.insert(watch::clean(p)));
        for runtime in runtimes {
            let runtime_clean = watch::clean(&runtime);
            if watched.iter().any(|p| p.starts_with(&runtime_clean)) {
                // The runtime directory is, or is above, what is watched (`socket =
                // "/tmp/app.sock"` with the app under /tmp): skipping it all would
                // skip the app. Only what Warden writes in it is skipped.
                let app = &self.cfg.app.name;
                for (start, end) in
                    [(format!("{app}-shim."), ""), (format!("{app}-host."), ""), (format!("{app}.h"), ".sock")]
                {
                    skip.push(watch::Skip::Prefix(runtime.join(start), end.into()));
                }
            } else {
                skip.push(watch::Skip::Path(runtime));
            }
        }
        Ok(Some(watch::Spec::new(w, base, skip)))
    }

    /// What a watcher that starts could not do, as a warning: with workers that
    /// cannot run side by side, a restart stops the old worker first, so a
    /// version that fails to start takes that worker down.
    fn watch_start_warning(&self) -> Option<String> {
        let why = self.cfg.no_overlap_reason()?;
        Some(format!(
            "a restart stops each worker before starting its replacement ({why}), so a version that fails to start \
             leaves that worker down until the files are fixed: there is no old worker to roll back to"
        ))
    }

    /// Start, replace or stop the watcher so that it matches `[watch]` (a
    /// reload may have changed it) and whether the workers run. A new
    /// watcher starts from the files as they are: what changed while the
    /// workers were stopped is what they start with.
    fn sync_watch(&mut self) {
        // A watcher that died (its panic is logged): nothing watches until
        // `warden reload`, a start of the workers or a change of `[watch]`.
        if let Some((spec, _)) = self.watch.take_if(|(_, h)| h.is_finished()) {
            error!(
                "file watching has stopped: changes to files restart nothing until it is started again",
                hint = "the error above says why; `warden reload` starts watching again",
            );
            self.watch_failed = Some(spec);
            self.watch_pending = None;
            self.watch_token += 1;
        }
        let want = match self.watch_spec() {
            Ok(want) => {
                self.watch_blocked = false;
                want
            }
            Err(e) => {
                if !self.watch_blocked {
                    self.watch_blocked = true;
                    error!(
                        "file watching cannot start: it does not know where the app's files are",
                        reason = e,
                        hint = "set [app] working_directory (an absolute path) and `warden reload`",
                    );
                }
                // Not watching "/" instead: the watcher goes until it can say where to look.
                if self.watch.take().is_some() {
                    self.watch_pending = None;
                    self.watch_token += 1;
                }
                return;
            }
        };
        if want.is_none() {
            // Off, or the workers are not running: a later start tries afresh.
            self.watch_failed = None;
        }
        if want.as_ref() == self.watch.as_ref().map(|(s, _)| s) || (want.is_some() && want == self.watch_failed) {
            return;
        }
        let was_watching = self.watch.is_some();
        self.watch = None;
        self.watch_failed = None;
        self.watch_pending = None;
        self.watch_held = false;
        self.watch_retry = None;
        self.watch_token += 1;
        let Some(spec) = want else { return };
        let (tx, token) = (self.tx.clone(), self.watch_token);
        match watch::spawn(spec.clone(), move |change| tx.send(Event::Watch { token, change }).is_ok()) {
            Ok(handle) => {
                info!(
                    if was_watching { "file watching settings changed" } else { "file watching on" },
                    paths = spec.paths.join(","),
                    base = spec.base.display(),
                    ignore = spec.ignore.len(),
                    debounce_ms = spec.debounce.as_millis(),
                    interval_ms = spec.interval.as_millis(),
                    max_files = spec.max_files,
                );
                if let Some(w) = self.watch_start_warning() {
                    warn!(
                        "file watching is on, but a failing restart cannot be rolled back",
                        reason = w,
                        hint = "let the workers share the port (Bun or Node through the shim, port_strategy = \"shared\") \
                                so a restart starts the new worker next to the old one",
                    );
                }
                self.watch = Some((spec, handle));
            }
            Err(e) => {
                error!(
                    "file watching could not start",
                    error = e,
                    hint = "fix [watch] (`warden check` finds it) and `warden reload`",
                );
                self.watch_failed = Some(spec);
            }
        }
    }

    fn on_watch(&mut self, token: u64, change: watch::Change) {
        if token != self.watch_token || self.shutting_down || self.stopped {
            return;
        }
        debug!("files changed", file = change.file, change = change.kind.as_str(), files = change.count);
        self.watch_pending = Some(match self.watch_pending.take() {
            Some(earlier) => earlier.merge(change),
            None => change,
        });
        self.try_watch_restart();
    }

    /// Tries for a restart that cannot begin, and the time between them.
    const WATCH_TRIES: u8 = 3;
    const WATCH_RETRY: Duration = Duration::from_secs(3);

    /// Start the rolling restart for changed files, once it can: not while a
    /// rollout runs (the change waits for it, and is not lost), and not
    /// within the throttle's gap of the last one. It is gated like any
    /// restart, so a release that does not start or pass its health checks
    /// is rolled back and the old workers keep serving until the next change.
    fn try_watch_restart(&mut self) {
        let Some(change) = &self.watch_pending else { return };
        if self.shutting_down || self.stopped {
            // The workers that start next read the files as they are.
            self.watch_pending = None;
            return;
        }
        let now = Instant::now();
        if self.roll.is_some() {
            if !self.watch_held {
                self.watch_held = true;
                info!(
                    "files changed: restart waits for the rollout in progress",
                    file = change.file,
                    files = change.count,
                );
            }
            return;
        }
        if self.watch_retry.is_some_and(|(_, at)| now < at) {
            return;
        }
        if let Some(wait) = self.watch_throttle.wait(now) {
            if !self.watch_held {
                self.watch_held = true;
                info!(
                    "files changed again soon after a watch restart: waiting before the next",
                    file = change.file,
                    files = change.count,
                    wait_ms = wait.as_millis(),
                    restarts_in_a_row = self.watch_throttle.streak() + 1,
                );
            }
            return;
        }
        let Some(change) = self.watch_pending.take() else { return };
        self.watch_held = false;
        // Like `warden restart`: a worker that gave up (FAILED, its backoff
        // running) is tried again; fixing the file that crashed it is the point.
        for s in self.slots.values_mut() {
            s.tracker.reset();
            s.failed_at = None;
        }
        let ids: Vec<usize> = self.slots.values().filter(|s| !s.removing).map(|s| s.id).collect();
        info!(
            "files changed: rolling restart",
            file = change.file,
            change = change.kind.as_str(),
            files = change.count,
        );
        match self.begin_rollout(Kind::Restart, ids, "watch".into(), false) {
            Ok(_) => {
                self.watch_retry = None;
                self.watch_throttle.started(now);
                if self.watch_throttle.streak() == 3 {
                    warn!(
                        "files keep changing right after each watch restart",
                        restarts_in_a_row = self.watch_throttle.streak() + 1,
                        last_file = change.file,
                        hint = "something may be writing into a watched directory (a database, a cache, uploads, a \
                                build output): add it to [watch] ignore. Restarts are spaced out meanwhile",
                    );
                }
            }
            // Nothing was touched (the release `current` points to did not
            // resolve: a deploy may be halfway). The change is not lost: it is
            // tried again, a few times, a few seconds apart.
            Err(e) => {
                let failures = self.watch_retry.map_or(0, |(n, _)| n) + 1;
                if failures < Self::WATCH_TRIES {
                    warn!(
                        "the restart for changed files could not start; trying again",
                        reason = e,
                        attempt = failures,
                        retry_in_s = Self::WATCH_RETRY.as_secs(),
                        hint = "a deploy may be halfway through swapping `current`; check working_directory if it \
                                keeps failing",
                    );
                    self.watch_retry = Some((failures, crate::restart::later(now, Self::WATCH_RETRY)));
                    self.watch_pending = Some(change);
                } else {
                    self.watch_retry = None;
                    warn!(
                        "the restart for changed files could not start; giving up until the next change",
                        reason = e,
                        hint = "`warden restart` starts it by hand once the cause is fixed",
                    );
                }
            }
        }
    }

    fn request_reload(&mut self, safe: bool) -> Response {
        if self.shutting_down {
            return Response::err("shutting down");
        }
        if self.sweep.is_some() {
            return Response::err(
                "still stopping the workers a killed supervisor of this app left behind; try again then",
            );
        }
        if self.stopped {
            return Response::err("workers are stopped; use `warden restart`");
        }
        // A watcher that died or could not start is tried again by the next tick.
        self.watch_failed = None;
        let ids: Vec<usize> = self.slots.values().filter(|s| !s.removing).map(|s| s.id).collect();
        let kind = if safe { Kind::SafeReload } else { Kind::Reload };
        match self.begin_rollout(kind, ids, String::new(), false) {
            Ok(seq) => Response::started(if safe { "safe-reload started" } else { "reload started" }, seq),
            Err(e) => Response::err(e),
        }
    }

    fn on_request(&mut self, req: Request) -> Response {
        if let Some(r) = self.request_during_sweep(&req) {
            return r;
        }
        match req {
            Request::Status => Response { status: Some(self.status()), ..Response::ok("") },
            Request::Start => {
                if self.shutting_down {
                    return Response::err("shutting down");
                }
                if !self.stopped {
                    return Response::ok("already running");
                }
                if !self.insts.is_empty() {
                    // Still stopping: start again once every worker has exited.
                    self.start_after_stop = true;
                    return Response::ok("starting workers once the last ones have stopped");
                }
                info!("starting all workers on request");
                self.stopped = false;
                self.start_all();
                Response::ok("starting workers")
            }
            Request::Reset { worker } => self.reset(worker),
            Request::Signal { signal, worker } => self.send_signal(&signal, worker),
            Request::Config { show_secrets, worker } => {
                Response { info: Some(self.info(show_secrets, worker)), ..Response::ok("") }
            }
            Request::Reload { safe } => self.request_reload(safe),
            Request::Stop => {
                if self.shutting_down {
                    return Response::err("shutting down");
                }
                // A pending `restart` (stop, then start) is cancelled either way.
                self.start_after_stop = false;
                if self.stopped {
                    return Response::ok("already stopped");
                }
                info!("stopping all workers (supervisor stays up)");
                self.stopped = true;
                // Under systemd (Type=notify) the unit is started all the
                // same: else it would wait for READY=1 until its timeout and
                // restart us, starting the workers again (a start that failed
                // and was stopped by `warden start`, for one).
                if !self.announced_ready {
                    self.announced_ready = true;
                    systemd::notify("READY=1\nSTATUS=workers stopped on request");
                }
                self.stop_all();
                Response::ok("stopping workers; `warden restart` starts them again")
            }
            Request::Shutdown => {
                self.begin_shutdown("shutdown request");
                Response::ok("shutting down")
            }
            Request::Restart { worker: None, hard } => {
                if self.shutting_down {
                    return Response::err("shutting down");
                }
                if self.insts.is_empty() {
                    info!("starting all workers (restart of a stopped app)");
                    self.stopped = false;
                    self.start_all();
                    return Response::ok("starting workers");
                }
                if hard || self.stopped {
                    info!("restarting all workers (stop, then start)");
                    self.stopped = true;
                    self.start_after_stop = true;
                    self.stop_all();
                    return Response::ok("restarting all workers at once (brief downtime)");
                }
                for s in self.slots.values_mut() {
                    s.tracker.reset();
                    s.failed_at = None;
                }
                let ids: Vec<usize> = self.slots.values().filter(|s| !s.removing).map(|s| s.id).collect();
                match self.begin_rollout(Kind::Restart, ids, String::new(), false) {
                    Ok(seq) => Response::started("rolling restart started", seq),
                    Err(e) => Response::err(e),
                }
            }
            Request::Restart { worker: Some(id), hard } => {
                if hard {
                    return Response::err("--hard restarts whole apps; without it, one worker is replaced gracefully");
                }
                if self.shutting_down || self.stopped {
                    return Response::err("workers are stopped or shutting down; `warden start` starts them");
                }
                if self.is_worker_mode() {
                    return Response::err("worker mode restarts the whole host; use `warden restart <app>`");
                }
                if !self.slots.contains_key(&id) {
                    return Response::err(format!("no worker {id} (this app has {} worker(s))", self.count));
                }
                if let Some(s) = self.slots.get_mut(&id) {
                    s.tracker.reset();
                    s.failed_at = None;
                }
                match self.begin_rollout(Kind::Restart, vec![id], String::new(), false) {
                    Ok(seq) => Response::started(format!("restarting worker {id}"), seq),
                    Err(e) => Response::err(e),
                }
            }
            Request::Scale { count } => self.scale(count),
            Request::Logs { .. } | Request::LogLevel { .. } | Request::Flush | Request::Subscribe { .. } => {
                Response::err("internal: this request is answered by the control socket")
            }
        }
    }

    /// `warden reset`: restart counters to zero, FAILED workers retried now.
    fn reset(&mut self, worker: Option<usize>) -> Response {
        if let Some(id) = worker {
            if !self.slots.contains_key(&id) {
                return Response::err(format!("no worker {id} (this app has {} worker(s))", self.count));
            }
        }
        let ids: Vec<usize> = self.slots.keys().copied().filter(|id| worker.is_none_or(|w| w == *id)).collect();
        let mut retried = Vec::new();
        for id in &ids {
            let Some(s) = self.slots.get_mut(id) else { continue };
            s.tracker.reset();
            s.restarts = 0;
            s.crashes = 0;
            if s.state == State::Failed && !self.stopped && !self.shutting_down {
                s.failed_at = None;
                s.token += 1;
                s.state = State::Restarting;
                retried.push((*id, s.token));
            }
        }
        if worker.is_none() {
            self.standby_reset();
        }
        info!("counters reset on request", workers = ids.len(), failed_retried = retried.len());
        for (id, token) in &retried {
            self.emit_worker(*id, WorkerEvent::Restarting, None, || Some("FAILED; reset on request".into()));
            self.on_restart_due(*id, *token);
        }
        let what = match worker {
            Some(id) => format!("worker {id}"),
            None => format!("{} worker(s)", ids.len()),
        };
        if retried.is_empty() {
            Response::ok(format!("reset {what}"))
        } else {
            Response::ok(format!("reset {what}; restarting {} FAILED worker(s)", retried.len()))
        }
    }

    /// `warden signal SIGUSR2 api[:N]`: to each worker process (in worker
    /// mode, the host process: Workers get no signals).
    fn send_signal(&mut self, name: &str, worker: Option<usize>) -> Response {
        let Some(sig) = crate::signals::parse(name) else {
            return Response::err(format!("unknown signal {name:?}; use a name like SIGUSR2 or USR2, or a number"));
        };
        if self.is_worker_mode() && worker.is_some_and(|w| w != 1) {
            return Response::err("worker mode: signals go to the host process; omit the worker number");
        }
        let mut sent = Vec::new();
        for s in self.slots.values() {
            if worker.is_some_and(|w| w != s.id) && !self.is_worker_mode() {
                continue;
            }
            if let Some(i) = s.current.and_then(|c| self.insts.get(&c)) {
                i.handle.signal(sig);
                sent.push(i.handle.pid.to_string());
            }
        }
        if sent.is_empty() {
            return Response::err("no running worker to signal");
        }
        info!("signal sent on request", signal = name, pids = sent.join(","));
        Response::ok(format!("sent {name} to pid {}", sent.join(", ")))
    }

    /// Effective config and paths for `describe`, `config` and `env`.
    fn info(&self, show_secrets: bool, worker: Option<usize>) -> serde_json::Value {
        let mut v = serde_json::to_value(&self.cfg).unwrap_or(serde_json::Value::Null);
        // The environment that worker starts with (`warden env`): which
        // worker, then each variable with its value and where it comes from.
        let slot = if self.is_worker_mode() { 1 } else { worker.unwrap_or(1).clamp(1, self.count.max(1)) };
        let inst = self.slots.get(&slot).and_then(|s| s.current).unwrap_or(self.next_inst);
        let worker_env: Vec<serde_json::Value> = self
            .worker_env(slot, inst, None)
            .into_iter()
            .map(|(k, val, from)| {
                // Warden's own values are not secret, except the [static]
                // section (its basic_auth); the app's are hidden.
                let secret = if from == "warden" { k == "WARDEN_STATIC" } else { !is_plain_env(&k) };
                let val = if show_secrets || !secret { val } else { format!("(hidden, {} chars)", val.len()) };
                serde_json::json!({"name": k, "value": val, "from": from})
            })
            .collect();
        // What workers get: env_file's variables with `env` on top.
        if let Some(app) = v.pointer_mut("/app").and_then(|a| a.as_object_mut()) {
            let merged: serde_json::Map<String, serde_json::Value> =
                self.cfg.app.environment().map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone()))).collect();
            app.insert("env".into(), serde_json::Value::Object(merged));
        }
        if let Some(env) = v.pointer_mut("/app/env").and_then(|e| e.as_object_mut()) {
            for (k, val) in env.iter_mut() {
                if !show_secrets && !is_plain_env(k) {
                    let n = val.as_str().map(str::len).unwrap_or(0);
                    *val = serde_json::Value::String(format!("(hidden, {n} chars)"));
                }
            }
        }
        serde_json::json!({
            "config": v,
            "config_path": self.cfg_path.as_ref().map(|p| p.display().to_string()),
            "socket": self.cfg.socket_path().display().to_string(),
            "runtime_dir": self.runtime_dir.display().to_string(),
            "log_file": crate::logging::file_path().map(|p| p.display().to_string()),
            "unit": systemd::own_unit(),
            "pid": std::process::id(),
            "shim": self.shim_path.as_ref().map(|p| p.display().to_string()),
            "workers_running": self.count,
            "worker_env_of": if self.is_worker_mode() { "host".to_string() } else { slot.to_string() },
            "worker_env": worker_env,
        })
    }

    fn scale(&mut self, n: usize) -> Response {
        if n == 0 || n > 1024 {
            return Response::err("count must be between 1 and 1024");
        }
        if self.shutting_down || self.stopped {
            return Response::err("workers are stopped or shutting down");
        }
        if self.roll.is_some() {
            return Response::err("a rollout is in progress; try again when it finishes");
        }
        if let Some(p) = self.cfg.app.port {
            if self.cfg.workers.port_strategy == PortStrategy::Offset && p as usize + n - 1 > u16::MAX as usize {
                return Response::err("port + count exceeds 65535");
            }
        }
        let old = self.count;
        self.count = n;
        info!("scaling", from = old, to = n);
        // Before the new workers start: none shares a standby's instance number.
        self.standby_before_scale_up();
        if self.is_worker_mode() {
            if n != old {
                return match self.begin_rollout(Kind::Reload, vec![1], "scale".into(), false) {
                    Ok(seq) => Response::started(format!("scaling to {n} workers (replacing the host process)"), seq),
                    Err(e) => {
                        self.count = old;
                        Response::err(e)
                    }
                };
            }
            return Response::ok(format!("already {n} workers"));
        }
        for id in (old + 1)..=n {
            let slot = self.slots.entry(id).or_insert_with(|| Slot::new(id));
            slot.removing = false;
            slot.token += 1;
            self.spawn_current(id);
        }
        let extra: Vec<usize> = self.slots.keys().copied().filter(|id| *id > n).collect();
        for id in extra {
            self.pending_replace.remove(&id);
            let Some(s) = self.slots.get_mut(&id) else { continue };
            s.removing = true;
            s.token += 1;
            let (current, state) = (s.current, s.state);
            match current {
                Some(inst) => self.stop_instance(inst),
                None => {
                    if state != State::Stopped {
                        self.emit_worker(id, WorkerEvent::Stopped, None, || Some("scaled down".into()));
                    }
                    self.slots.remove(&id);
                }
            }
        }
        Response::ok(format!("scaled from {old} to {n} workers"))
    }

    // --------------------------------------------------------------- status

    fn status(&mut self) -> Status {
        let now = Instant::now();
        let sample = |pid: u32, prev: &mut Option<(Instant, f64)>, started: Instant| {
            let st = metrics::proc_stats(pid)?;
            let pct = match prev {
                Some((t, c)) if now.duration_since(*t) > Duration::from_millis(100) => {
                    (st.cpu_seconds - *c) / now.duration_since(*t).as_secs_f64() * 100.0
                }
                _ => st.cpu_seconds / now.duration_since(started).as_secs_f64().max(0.001) * 100.0,
            };
            *prev = Some((now, st.cpu_seconds));
            Some((st, (pct * 10.0).round() / 10.0))
        };

        // The second the response windows are read at.
        let sec = self.started.elapsed().as_secs();
        let mut workers = Vec::new();
        let mut standbys = Vec::new();
        let mut host = None;
        let mut ready = 0;
        if self.is_worker_mode() {
            let slot = self.slots.get(&1);
            let cur = slot.and_then(|s| s.current);
            if let Some(inst) = cur.and_then(|c| self.insts.get_mut(&c)) {
                let started = inst.started;
                let pid = inst.handle.pid;
                let s = sample(pid, &mut inst.cpu_prev, started);
                host = Some(HostStatus {
                    pid,
                    uptime_secs: started.elapsed().as_secs(),
                    rss_bytes: s.map(|x| x.0.rss_bytes),
                    cpu_percent: s.map(|x| x.1),
                    restarts: slot.map(|s| s.restarts).unwrap_or(0),
                });
                if inst.ready_at.is_some() {
                    ready = inst.threads_listening();
                }
            }
            let slot_state = slot.map(|s| s.state).unwrap_or(State::Stopped);
            let inst = cur.and_then(|c| self.insts.get(&c));
            for id in 1..=self.count {
                let t = inst.and_then(|i| i.threads.get(&id));
                let state = match (slot_state, t) {
                    (State::Running | State::Restarting, Some(t)) if t.crashed => "CRASHED",
                    (State::Running | State::Restarting, Some(t)) if t.listening => "RUNNING",
                    (State::Running | State::Restarting, _) => "STARTING",
                    (other, _) => other.as_str(),
                };
                workers.push(WorkerStatus {
                    id,
                    state: state.into(),
                    pid: inst.map(|i| i.handle.pid),
                    uptime_secs: inst.and_then(|i| i.ready_at).map(|r| r.elapsed().as_secs()),
                    restarts: slot.map(|s| s.restarts).unwrap_or(0),
                    crashes: t.map(|t| t.crashes).unwrap_or(0),
                    rss_bytes: None,
                    cpu_seconds: None,
                    cpu_percent: None,
                    last_exit: t.and_then(|t| t.last_exit.clone()),
                    healthy: inst.and_then(|i| i.healthy),
                    // Each Worker thread has its own event loop and heartbeat.
                    loop_delay: inst.and_then(|i| i.loop_delay.get(&id)).and_then(|l| l.current(now)),
                    listening: Vec::new(),
                    requests: None,
                });
            }
        } else {
            for s in self.slots.values() {
                let inst = s.current.and_then(|c| self.insts.get_mut(&c));
                let (pid, uptime, stats, healthy, loop_delay, requests) = match inst {
                    Some(i) => {
                        let started = i.started;
                        let pid = i.handle.pid;
                        let stats = sample(pid, &mut i.cpu_prev, started);
                        let loop_delay = i.loop_delay.get(&s.id).and_then(|l| l.current(now));
                        let requests = i.requests.as_ref().map(|r| r.window.stats(sec));
                        (Some(pid), Some(started.elapsed().as_secs()), stats, i.healthy, loop_delay, requests)
                    }
                    None => (None, None, None, None, None, None),
                };
                if s.state == State::Running {
                    ready += 1;
                }
                workers.push(WorkerStatus {
                    id: s.id,
                    state: s.state.as_str().into(),
                    pid,
                    uptime_secs: uptime,
                    restarts: s.restarts,
                    crashes: s.crashes,
                    rss_bytes: stats.map(|x| x.0.rss_bytes),
                    cpu_seconds: stats.map(|x| x.0.cpu_seconds),
                    cpu_percent: stats.map(|x| x.1),
                    last_exit: s.last_exit.clone(),
                    healthy,
                    loop_delay,
                    listening: Vec::new(),
                    requests,
                });
            }
            // Hot standbys: their own list (`Status.standbys`).
            self.standby_rows(&mut standbys, &sample);
        }
        // Old processes a rollout replaced, still draining (`Status.draining`).
        let mut draining = Vec::new();
        let worker_mode = self.is_worker_mode();
        let mut retiring: Vec<u64> =
            self.insts.iter().filter(|(_, i)| i.role == Role::Retiring).map(|(id, _)| *id).collect();
        retiring.sort_unstable();
        for id in retiring {
            let Some(i) = self.insts.get_mut(&id) else { continue };
            let (pid, started, slot) = (i.handle.pid, i.started, i.slot);
            let stats = sample(pid, &mut i.cpu_prev, started);
            let (restarts, crashes) = self.slots.get(&slot).map(|s| (s.restarts, s.crashes)).unwrap_or((0, 0));
            draining.push(WorkerStatus {
                id: if worker_mode { 0 } else { slot },
                state: control::DRAINING.into(),
                pid: Some(pid),
                uptime_secs: Some(started.elapsed().as_secs()),
                restarts,
                crashes,
                rss_bytes: stats.map(|x| x.0.rss_bytes),
                cpu_seconds: stats.map(|x| x.0.cpu_seconds),
                cpu_percent: stats.map(|x| x.1),
                last_exit: None,
                healthy: None,
                // An old process's loop delay is not tracked (the map is the new one's).
                loop_delay: None,
                listening: Vec::new(),
                requests: i.requests.as_ref().map(|r| r.window.stats(sec)),
            });
        }
        draining.sort_by_key(|w| w.id);
        let me = std::process::id();
        let started = self.started;
        let sup = sample(me, &mut self.supervisor_cpu_prev, started);
        // What the workers listen on (the standbys do not yet, the draining ones have closed theirs).
        let own = listening::Own { runtime_dir: self.runtime_dir.clone(), app: self.cfg.app.name.clone() };
        let mut live = std::collections::HashSet::new();
        for w in workers.iter_mut() {
            let Some(pid) = w.pid else { continue };
            live.insert(pid);
            // Worker mode: the threads share their host's process; only one that is up holds the port.
            if worker_mode && w.state != "RUNNING" {
                continue;
            }
            let young = w.uptime_secs.is_none_or(|u| u < listening::YOUNG.as_secs());
            w.listening = self.listeners.of(pid, now, young, &own);
        }
        self.listeners.retain(&live);
        let ports = self.port_stats(&workers);
        // Who the app runs as: a running worker's owner (the OS says), else our own user.
        let owner = workers.iter().find_map(|w| w.pid).and_then(crate::platform::proc_owner);
        let user = crate::platform::user_name(owner.unwrap_or_else(crate::sys::euid));
        let rollout = self.rollout_status().or_else(|| self.sweep_status());
        Status {
            app: self.cfg.app.name.clone(),
            namespace: self.cfg.app.namespace.clone().unwrap_or_else(|| "default".into()),
            mode: mode_name(self.cfg.workers.mode).into(),
            config_path: self.cfg_path.as_ref().map(|p| p.display().to_string()),
            launched: launched_by(),
            unit: systemd::own_unit(),
            stopped: self.stopped,
            log_file: crate::logging::file_path().map(|p| p.display().to_string()),
            version: env!("CARGO_PKG_VERSION").into(),
            pid: me,
            uptime_secs: self.started.elapsed().as_secs(),
            workers_configured: self.count,
            workers_ready: ready,
            healthy: if self.cfg.health.enabled && !self.cfg.health.url.is_empty() {
                self.app_health.healthy
            } else {
                None
            },
            supervisor_rss_bytes: sup.map(|x| x.0.rss_bytes),
            host,
            reloading: rollout.is_some(),
            shutting_down: self.shutting_down,
            health_suspended: self.outage,
            log_lines_dropped: {
                let (o, e) = crate::logging::dropped();
                o + e
            },
            rollout,
            last_rollout: self.last_rollout.clone(),
            workers,
            release: self.release_text(),
            standbys,
            draining,
            start_failed: self.start_failed(),
            user: Some(user),
            build: crate::stamp::build(),
            cwd: self.shown_cwd(),
            watching: self.watch.as_ref().is_some_and(|(_, h)| !h.is_finished()),
            requests: self.app_requests(),
            ports,
            hint: crate::config::more_workers_hint(
                &self.cfg,
                self.count,
                cfg!(target_os = "linux"),
                crate::config::cpu_count(),
            ),
        }
    }
}

/// `systemd`, `background` or `terminal` (see `Status.launched`).
fn launched_by() -> String {
    if systemd::own_unit().is_some() {
        "systemd".into()
    } else if std::env::var_os(crate::events::LAUNCH_ENV).is_some_and(|v| v == "background") {
        "background".into()
    } else {
        "terminal".into()
    }
}

/// This binary's path. After an in-place upgrade Linux reports the old
/// inode as "<path> (deleted)"; the path itself now holds the new binary.
pub(crate) fn own_exe() -> PathBuf {
    let p = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("warden"));
    let s = p.to_string_lossy();
    match s.strip_suffix(" (deleted)") {
        Some(orig) => PathBuf::from(orig),
        None => p,
    }
}

/// Env keys whose values are shown by `describe` / `env` without
/// `--show-secrets`: well-known, never secret.
pub fn is_plain_env(key: &str) -> bool {
    matches!(key, "NODE_ENV" | "PORT" | "HOST" | "HOSTNAME" | "TZ" | "LOG_LEVEL" | "NODE_OPTIONS" | "BUN_ENV")
        || key.starts_with("WARDEN_")
}

/// Load the shim before the app: Bun takes `--preload=<shim>` (after `run`
/// when the args start with the `run` subcommand: `bun --preload x run f`
/// prints usage), Node takes `--import=<shim>` before the script.
fn with_preload(command: &str, args: &[String], shim: Option<&Path>) -> Vec<String> {
    let Some(shim) = shim else { return args.to_vec() };
    if crate::config::is_node(command) {
        return [&[format!("--import={}", shim.display())][..], args].concat();
    }
    let flag = [format!("--preload={}", shim.display())];
    match args.first().map(String::as_str) {
        Some("run") => [&args[..1], &flag, &args[1..]].concat(),
        _ => [&flag[..], args].concat(),
    }
}

/// Emit a worker event next to the log line of the same transition. `detail`
/// is only built when someone is subscribed: without subscribers this costs
/// one atomic load.
fn emit(app: &str, worker: usize, event: WorkerEvent, pid: Option<u32>, detail: impl FnOnce() -> Option<String>) {
    emit_to(app, (worker, None), event, pid, detail);
}

/// `emit` for any process: `(worker, standby)` from `Supervisor::event_who`.
fn emit_to(
    app: &str,
    (worker, standby): (usize, Option<usize>),
    event: WorkerEvent,
    pid: Option<u32>,
    detail: impl FnOnce() -> Option<String>,
) {
    if events::active() {
        events::worker(app, worker, standby, event, pid, detail());
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Current => "current",
        Role::Replacement => "replacement",
        Role::Retiring => "retiring",
        Role::Standby => "standby",
    }
}

/// A path Warden writes (a log file, the socket), as the watcher must not
/// see it: lexically clean (done by `Spec::new`), and also by its real path
/// when its directory is reached through a symlink. `log`: the file is a log
/// that may be written per worker (`out-1.log`).
fn skip_entries(path: PathBuf, log: bool) -> Vec<watch::Skip> {
    let make = |p: PathBuf| if log { watch::Skip::Log(p) } else { watch::Skip::Path(p) };
    let mut v = vec![make(path.clone())];
    if let (Some(dir), Some(name)) = (path.parent(), path.file_name()) {
        if let Ok(real) = std::fs::canonicalize(dir) {
            let real = real.join(name);
            if real != watch::clean(&path) {
                v.push(make(real));
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preload_placement() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let shim = Some(Path::new("/r/shim.mjs"));
        assert_eq!(
            with_preload("bun", &s(&["run", "dist/main.js"]), shim),
            s(&["run", "--preload=/r/shim.mjs", "dist/main.js"])
        );
        assert_eq!(with_preload("bun", &s(&["server.ts"]), shim), s(&["--preload=/r/shim.mjs", "server.ts"]));
        assert_eq!(with_preload("bun", &s(&["server.ts"]), None), s(&["server.ts"]));
        assert_eq!(
            with_preload("/usr/bin/node", &s(&["--max-old-space-size=512", "server.js"]), shim),
            s(&["--import=/r/shim.mjs", "--max-old-space-size=512", "server.js"])
        );
    }

    fn sup(toml: &str) -> Supervisor {
        let cfg = Config::parse(toml).unwrap();
        let (tx, _) = mpsc::unbounded_channel();
        let (proc_tx, _) = mpsc::unbounded_channel();
        Supervisor::new(cfg, None, PathBuf::from("/run/w"), (None, None), tx, proc_tx)
    }

    /// The variable's value as a worker sees it (the last one set wins).
    fn get<'a>(env: &'a [(String, String, &'static str)], k: &str) -> Option<(&'a str, &'static str)> {
        env.iter().rev().find(|(n, ..)| n == k).map(|(_, v, from)| (v.as_str(), *from))
    }

    /// W1: what every worker, a standby and a worker-mode host start with
    /// (docs/configuration.md "Environment variables"), and that Warden's win.
    #[test]
    fn worker_environment() {
        let s = sup("[app]\nname = \"api\"\ncommand = \"bun\"\nport = 3000\nenv = { PORT = \"9\", A = \"x\" }\n\
                     [workers]\ncount = 3\nport_strategy = \"offset\"\n[watchdog]\ntimeout = 0\n");
        let env = s.worker_env(2, 7, None);
        assert_eq!(get(&env, "A"), Some(("x", "env")));
        assert_eq!(get(&env, "PORT"), Some(("3001", "warden")), "Warden's PORT wins over env's");
        assert_eq!(get(&env, "NODE_APP_INSTANCE"), Some(("1", "warden")));
        assert_eq!(get(&env, "WARDEN_WORKER_ID"), Some(("2", "warden")));
        assert_eq!(get(&env, "WARDEN_WORKER_COUNT"), Some(("3", "warden")));
        assert_eq!(get(&env, "WARDEN_INSTANCE"), Some(("7", "warden")));
        assert_eq!(get(&env, "WARDEN_APP"), Some(("api", "warden")));
        assert_eq!(get(&env, "WARDEN_IPC_FD"), Some(("3", "warden")));
        assert_eq!(get(&env, "WARDEN_HEARTBEAT_MS"), Some(("1000", "warden")), "also with the watchdog off");
        assert!(get(&env, "WARDEN_STANDBY").is_none() && get(&env, "WARDEN_REUSE_PORT").is_none());
        // The app's own come first, Warden's after: what `spec` applies in order.
        let first_warden = env.iter().position(|e| e.2 == "warden").unwrap();
        assert!(env[first_warden..].iter().all(|e| e.2 == "warden"));
        let spec = s.spec(2, 7, None);
        assert!(spec.env.iter().rev().find(|(k, _)| k == "PORT").is_some_and(|(_, v)| v == "3001"));

        // A standby: worker id 0 and an instance number past the workers'.
        let s = sup("[app]\nname = \"api\"\ncommand = \"bun\"\nport = 3000\n[workers]\ncount = 2\nstandby = 1\n");
        let env = s.worker_env(STANDBY_SLOT, 9, Some((1, 2)));
        assert_eq!(get(&env, "WARDEN_WORKER_ID"), Some(("0", "warden")));
        assert_eq!(get(&env, "WARDEN_STANDBY"), Some(("1", "warden")));
        assert_eq!(get(&env, "NODE_APP_INSTANCE"), Some(("2", "warden")));
        assert_eq!(get(&env, "PORT"), Some(("3000", "warden")));
        assert_eq!(get(&env, "WARDEN_REUSE_PORT"), Some(("1", "warden")));

        // Worker mode: the host gets the Workers' count and entry; each
        // Worker's id and instance number come from the host and the shim.
        let s =
            sup("[app]\nname = \"api\"\ncommand = \"bun\"\nentry = \"main.js\"\nport = 3000\ninstance_var = \"\"\n\
                     [workers]\ncount = 4\nmode = \"worker\"\n");
        let env = s.worker_env(1, 1, None);
        assert_eq!(get(&env, "WARDEN_WORKERS"), Some(("4", "warden")));
        assert_eq!(get(&env, "WARDEN_MODE"), Some(("worker", "warden")));
        assert!(get(&env, "WARDEN_ENTRY").is_some_and(|(v, _)| v.ends_with("main.js")));
        assert!(get(&env, "WARDEN_WORKER_ID").is_none() && get(&env, "NODE_APP_INSTANCE").is_none());
        assert!(get(&env, "WARDEN_INSTANCE_VAR").is_none(), "instance_var = \"\" sets none");
    }

    /// W5: a start fails once every worker crashed (twice) before one was
    /// ready, and says so once. One crash is not enough: it can be a
    /// transient the restart policy gets past.
    #[test]
    fn start_attempt_fails_when_every_worker_crashed() {
        let mut s = sup("[app]\nname = \"api\"\ncommand = \"sh\"\n[workers]\ncount = 2\n");
        for id in [1, 2] {
            let mut slot = Slot::new(id);
            slot.last_exit = Some(format!("exit code {id}"));
            s.slots.insert(id, slot);
        }
        assert_eq!(s.start_failed(), None, "no start yet");
        s.start_attempt = Some(StartAttempt::default());
        s.note_start_crash(1);
        s.note_start_crash(2);
        assert_eq!(s.start_failed(), None, "each crashed once: a transient could explain it");
        assert!(!s.start_attempt.as_ref().unwrap().reported);
        s.note_start_crash(1);
        assert_eq!(s.start_failed(), None, "worker 2 has crashed only once");
        s.note_start_crash(2);
        assert_eq!(s.start_failed().as_deref(), Some("exit code 2"), "the last crash's reason");
        assert!(s.start_attempt.as_ref().unwrap().reported, "logged once");
        // Once a worker was ready, crashes are ordinary ones.
        s.start_attempt = None;
        s.note_start_crash(1);
        assert_eq!(s.start_failed(), None);
    }

    /// `warden env`: the effective environment, secrets hidden.
    #[test]
    fn config_answer_has_the_worker_environment() {
        let s = sup("[app]\nname = \"api\"\ncommand = \"bun\"\nport = 3000\nenv = { DB = \"secret\" }\n\
                     [workers]\ncount = 2\n");
        let info = s.info(false, Some(2));
        assert_eq!(info["worker_env_of"], "2");
        let vars = info["worker_env"].as_array().unwrap();
        let val = |k: &str| vars.iter().rev().find(|v| v["name"] == k).map(|v| v["value"].clone());
        assert_eq!(val("DB"), Some("(hidden, 6 chars)".into()));
        assert_eq!(val("WARDEN_WORKER_ID"), Some("2".into()));
        assert_eq!(val("NODE_APP_INSTANCE"), Some("1".into()), "Warden's values are never hidden");
        assert_eq!(s.info(true, None)["worker_env"][0]["value"], "secret");
        assert_eq!(s.info(false, Some(99))["worker_env_of"], "2", "clamped to the workers there are");
        // A static site's section carries its basic_auth: hidden too.
        let s = sup("[app]\nname = \"site\"\nport = 8080\n[static]\nroot = \"/srv/site\"\nbasic_auth = \"u:pw\"\n");
        let env = s.info(false, None)["worker_env"].to_string();
        assert!(env.contains("WARDEN_STATIC") && !env.contains("pw"), "{env}");
        assert!(s.info(true, None)["worker_env"].to_string().contains("u:pw"));
    }
}

/// `[watch]` in the supervisor: when a change starts a restart, and when it
/// waits, is retried or is dropped (the scanner has its own tests in watch.rs).
#[cfg(test)]
mod watch_tests {
    use super::rig::{Rig, local};
    use super::*;

    /// A worker that listens at once and stays up.
    const APP: &str = "echo '{\"ev\":\"listening\",\"port\":1}' >&3\nexec sleep 60\n";

    fn change(file: &str) -> watch::Change {
        watch::Change { file: file.into(), kind: watch::Kind::Modified, count: 1 }
    }

    fn currents(s: &Supervisor) -> Vec<u64> {
        let mut v: Vec<u64> = s.slots.values().filter_map(|x| x.current).collect();
        v.sort();
        v
    }

    fn all_running(s: &Supervisor) -> bool {
        s.slots.values().all(|x| x.state == State::Running && x.current.is_some())
    }

    /// `count` workers running in a scratch directory, `[watch]` on with `watch` as its keys.
    async fn rig(name: &str, count: usize, watch: &str) -> (Rig, PathBuf) {
        rig_of(name, &format!("count = {count}\n"), APP, watch).await
    }

    /// The same for any `[workers]` keys and worker script.
    async fn rig_of(name: &str, workers: &str, app: &str, watch: &str) -> (Rig, PathBuf) {
        let (mut r, dir) = bare(name, workers, app, watch);
        r.sup.start_all();
        r.until("every worker running", all_running).await;
        (r, dir)
    }

    /// A supervisor whose workers are not started, for what `[watch]` asks for. `watch` is the
    /// rest of the config after `[watch] enabled = true` (more keys, then more sections). Relative
    /// paths in it are relative to the scratch directory, as if Warden had been started there.
    fn bare(name: &str, workers: &str, app: &str, watch: &str) -> (Rig, PathBuf) {
        let dir = std::env::temp_dir().join(format!("warden-watchrig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let sections = format!(
            "working_directory = {:?}\n[workers]\n{workers}overlap = true\n[reload]\nhealth_passes = 0\n\
             [watch]\nenabled = true\n{watch}",
            dir.display().to_string()
        );
        let mut r = Rig::new(&format!("watch-{name}"), &sections, app);
        r.sup.launch_cwd = Some(dir.clone());
        (r, dir)
    }

    /// How many files a watcher with these settings looks at (a first scan).
    fn files_seen(spec: &watch::Spec) -> usize {
        let mut w = watch::Watcher::new(spec).unwrap();
        let scanned = w.scan(&std::sync::atomic::AtomicBool::new(false));
        w.apply(scanned, Instant::now()).baseline.unwrap()
    }

    fn write(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "1").unwrap();
    }

    /// Remove a `bare` scratch directory and the runtime directory of its rig.
    fn tidy(dir: &Path) {
        let _ = std::fs::remove_dir_all(dir);
        let name = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("warden-watchrig-"));
        if let Some(name) = name {
            let _ = std::fs::remove_dir_all(dir.with_file_name(format!("warden-rig-watch-{name}")));
        }
    }

    async fn finish(r: Rig, dir: PathBuf) {
        r.shutdown().await;
        tidy(&dir);
    }

    /// The whole way: a file written in the working directory is noticed by
    /// the watcher, and every worker is replaced by a gated rolling restart.
    #[tokio::test(flavor = "current_thread")]
    async fn a_changed_file_restarts_every_worker() {
        local(async {
            let (mut r, dir) = rig("e2e", 2, "debounce_ms = 100\ninterval_ms = 100\n").await;
            assert!(r.sup.watch.is_some() && r.sup.status().watching, "started with the workers");
            let before = currents(&r.sup);
            // The first scan is the baseline: what is written after it is a change.
            tokio::time::sleep(Duration::from_millis(400)).await;
            std::fs::write(dir.join("app.js"), "v2").unwrap();
            r.until("the restart begins", |s| s.roll.is_some() || s.last_rollout.is_some()).await;
            r.until("the restart ends", |s| s.roll.is_none()).await;
            let o = r.sup.last_rollout.clone().unwrap();
            assert!(o.ok, "{o:?}");
            let after = currents(&r.sup);
            assert!(after.iter().all(|i| !before.contains(i)), "{before:?} -> {after:?}: every worker replaced");
            assert!(all_running(&r.sup));
            finish(r, dir).await;
        })
        .await;
    }

    /// A change during a rollout is kept, and restarts once it is over.
    #[tokio::test(flavor = "current_thread")]
    async fn a_change_during_a_rollout_waits_and_is_not_lost() {
        local(async {
            let (mut r, dir) = rig("wait", 2, "").await;
            let ids: Vec<usize> = r.sup.slots.keys().copied().collect();
            r.sup.begin_rollout(Kind::Reload, ids, String::new(), false).unwrap();
            let token = r.sup.watch_token;
            r.sup.on_watch(token, change("a.js"));
            r.sup.on_watch(token, change("b.js"));
            assert!(r.sup.roll.is_some() && r.sup.watch_held, "waiting for the reload");
            assert_eq!(r.sup.watch_pending.as_ref().map(|c| (c.file.as_str(), c.count)), Some(("a.js", 2)));
            r.until("the reload ends", |s| s.roll.is_none()).await;
            let before = currents(&r.sup);
            // The next tick starts it.
            r.sup.try_watch_restart();
            assert_eq!(r.sup.roll.as_ref().map(|x| x.kind), Some(Kind::Restart));
            assert!(r.sup.watch_pending.is_none() && !r.sup.watch_held);
            r.until("the restart ends", |s| s.roll.is_none()).await;
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true));
            assert!(currents(&r.sup).iter().all(|i| !before.contains(i)));
            finish(r, dir).await;
        })
        .await;
    }

    /// Changes right after a watch restart wait for the throttle's gap.
    #[tokio::test(flavor = "current_thread")]
    async fn restarts_are_spaced_out() {
        local(async {
            let (mut r, dir) = rig("gap", 1, "").await;
            let token = r.sup.watch_token;
            r.sup.on_watch(token, change("a.js"));
            assert!(r.sup.roll.is_some(), "the first starts at once");
            r.until("the restart ends", |s| s.roll.is_none()).await;
            r.sup.on_watch(token, change("b.js"));
            assert!(r.sup.roll.is_none() && r.sup.watch_pending.is_some() && r.sup.watch_held, "within the gap: waits");
            r.sup.try_watch_restart();
            assert!(r.sup.roll.is_none(), "a tick before the gap has passed changes nothing");
            // Time passes: the last one began longer ago than the gap.
            r.sup.watch_throttle = watch::Throttle::default();
            r.sup.watch_throttle.started(Instant::now() - watch::Throttle::GAP - Duration::from_millis(100));
            r.sup.try_watch_restart();
            assert!(r.sup.roll.is_some() && r.sup.watch_pending.is_none());
            r.until("the restart ends", |s| s.roll.is_none()).await;
            finish(r, dir).await;
        })
        .await;
    }

    /// Stopped or shutting down: nothing to restart, and the change is dropped
    /// (the workers that start next read the files as they are). A late event
    /// of an earlier watcher is ignored.
    #[tokio::test(flavor = "current_thread")]
    async fn changes_are_dropped_when_there_is_nothing_to_restart() {
        local(async {
            let (mut r, dir) = rig("drop", 1, "").await;
            let token = r.sup.watch_token;
            r.sup.on_watch(token + 1, change("late.js"));
            assert!(r.sup.roll.is_none() && r.sup.watch_pending.is_none(), "a stale watcher's event");

            r.sup.watch_pending = Some(change("a.js"));
            r.sup.stopped = true;
            r.sup.try_watch_restart();
            assert!(r.sup.watch_pending.is_none() && r.sup.roll.is_none(), "stopped");
            r.sup.on_watch(token, change("b.js"));
            assert!(r.sup.watch_pending.is_none() && r.sup.roll.is_none());
            r.sup.stopped = false;

            r.sup.watch_pending = Some(change("c.js"));
            r.sup.begin_shutdown("test");
            assert!(r.sup.watch.is_none() && r.sup.watch_pending.is_none(), "shutting down: the watcher is gone");
            r.sup.on_watch(token, change("d.js"));
            assert!(r.sup.watch_pending.is_none());
            finish(r, dir).await;
        })
        .await;
    }

    /// The watcher follows `[watch]` and whether the workers run.
    #[tokio::test(flavor = "current_thread")]
    async fn the_watcher_follows_the_config_and_the_workers() {
        local(async {
            let (mut r, dir) = rig("sync", 1, "paths = [\"src\"]\n").await;
            let spec = |s: &Supervisor| s.watch.as_ref().map(|(sp, _)| (sp.base.clone(), sp.paths.clone()));
            assert_eq!(spec(&r.sup), Some((dir.clone(), vec!["src".to_string()])));
            let token = r.sup.watch_token;
            r.sup.sync_watch();
            assert_eq!(r.sup.watch_token, token, "nothing changed: the same watcher keeps its baseline");

            // Stopped workers are not watched; started ones are, from the files as they are.
            assert!(r.sup.status().watching);
            r.sup.stopped = true;
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none() && r.sup.watch_token > token);
            assert!(!r.sup.status().watching, "status says what runs, not what the config asks for");
            r.sup.stopped = false;
            r.sup.sync_watch();
            assert!(r.sup.watch.is_some() && r.sup.status().watching);

            // A reload that changed [watch].
            let token = r.sup.watch_token;
            r.sup.cfg.watch.paths = vec!["lib".into()];
            r.sup.watch_pending = Some(change("old.js"));
            r.sup.sync_watch();
            assert_eq!(spec(&r.sup).map(|s| s.1), Some(vec!["lib".to_string()]));
            assert!(r.sup.watch_token > token && r.sup.watch_pending.is_none(), "an earlier watcher's change is void");

            // Settings that cannot start a watcher: said once, not retried every tick.
            r.sup.cfg.watch.ignore = vec!["!keep".into()];
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none() && r.sup.watch_failed.is_some());
            let token = r.sup.watch_token;
            r.sup.sync_watch();
            assert_eq!(r.sup.watch_token, token, "not tried again until [watch] changes");

            r.sup.cfg.watch.enabled = false;
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none() && r.sup.watch_failed.is_none() && !r.sup.status().watching);
            finish(r, dir).await;
        })
        .await;
    }

    /// A slot that gave up (FAILED: it crashed too often) is tried again by the
    /// watch restart, as by `warden restart`: fixing the file that crashed it
    /// is the point. The worker crashes while `crash` exists in its directory.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_slot_is_tried_again() {
        local(async {
            const CRASHER: &str = "[ -e crash ] && exit 5\n\
                echo '{\"ev\":\"listening\",\"port\":1}' >&3\nexec sleep 60\n";
            let (mut r, dir) = bare(
                "failed",
                "count = 1\n",
                CRASHER,
                "[restart]\nmax_restarts = 1\nrestart_window = 60\nbackoff_initial = 10\nbackoff_max = 20\n",
            );
            write(&dir, "crash");
            r.sup.start_all();
            r.until("the slot gave up", |s| s.slots[&1].state == State::Failed).await;
            assert!(r.sup.slots[&1].current.is_none() && r.sup.status().workers[0].state == "FAILED");
            // The fix: the file that crashed it goes away, and Warden notices the change.
            std::fs::remove_file(dir.join("crash")).unwrap();
            let token = r.sup.watch_token;
            r.sup.on_watch(token, change("crash"));
            assert!(r.sup.roll.is_some() && r.sup.slots[&1].failed_at.is_none());
            r.until("the restart ends", |s| s.roll.is_none()).await;
            let o = r.sup.last_rollout.clone().unwrap();
            assert!(o.ok, "{o:?}");
            assert!(all_running(&r.sup), "the slot serves again: {:?}", r.sup.status().workers);
            finish(r, dir).await;
        })
        .await;
    }

    /// A worker and a hot standby: the restart replaces both (a standby would
    /// otherwise keep the old files in memory and be promoted into a crash's slot).
    #[tokio::test(flavor = "current_thread")]
    async fn standbys_are_replaced_with_the_workers() {
        local(async {
            const POOL: &str = r#"
if [ "$WARDEN_STANDBY" = 1 ]; then
  echo '{"ev":"standby_ready","port":1}' >&3
  read -r line <&3 || exit 0
fi
echo '{"ev":"listening","port":1}' >&3
exec sleep 60
"#;
            let available = |s: &Supervisor| {
                let mut ids: Vec<u64> = (s.insts.iter())
                    .filter(|(_, i)| i.standby.as_ref().is_some_and(|g| g.available))
                    .map(|(id, _)| *id)
                    .collect();
                ids.sort();
                ids
            };
            let (mut r, dir) = rig_of("standby", "count = 1\nstandby = 1\n", POOL, "").await;
            r.until("a standby ready", |s| available(s).len() == 1).await;
            let (worker, standby) = (currents(&r.sup)[0], available(&r.sup)[0]);
            let token = r.sup.watch_token;
            r.sup.on_watch(token, change("a.js"));
            r.until("worker and standby replaced", |s| {
                s.roll.is_none() && available(s).len() == 1 && available(s)[0] != standby
            })
            .await;
            assert_ne!(currents(&r.sup)[0], worker);
            assert!(!r.sup.insts.contains_key(&standby), "the old standby is gone");
            finish(r, dir).await;
        })
        .await;
    }

    /// F1: what Warden writes itself is not a change, however the paths in
    /// `[logging]` are written (`./x`, `a/../x`) and for per-worker files
    /// (`out-1.txt`): else each restart's own log lines would start the next.
    #[test]
    fn the_log_files_are_never_a_change() {
        let (r, dir) = bare(
            "own-logs",
            "count = 2\n",
            APP,
            "ignore = []\n[logging]\nfile = \"./warden.out\"\nout_file = \"logs/../out.txt\"\nper_worker_files = true\n",
        );
        // `..` out of the directory and back in: the path the scanner sees is the clean one.
        let name = dir.file_name().unwrap().to_str().unwrap().to_string();
        let mut r = r;
        r.sup.cfg.logging.err_file = Some(PathBuf::from(format!("../{name}/err.txt")));
        for f in [
            "app.js",
            "warden.out",
            "warden.out.1",
            "out.txt",
            "out-1.txt",
            "out-2.txt",
            "out-s1.txt",
            "out-1.txt.2.gz",
            "err.txt",
            "err-2.txt",
            "err-host.txt.1",
        ] {
            write(&dir, f);
        }
        let spec = r.sup.watch_spec().unwrap().unwrap();
        assert_eq!(files_seen(&spec), 1, "only app.js: {:?}", spec.skip);
        // Not per worker: out-1.txt is the app's own file then.
        r.sup.cfg.logging.per_worker_files = false;
        let spec = r.sup.watch_spec().unwrap().unwrap();
        assert_eq!(files_seen(&spec), 7, "app.js and the six per-worker files, now the app's own");
        tidy(&dir);
    }

    /// F2: a control socket in the watched tree, or above it (`/tmp/app.sock`
    /// with the app in /tmp), must not hide the tree: only what Warden writes
    /// there is skipped.
    #[test]
    fn a_control_socket_next_to_the_app_does_not_hide_it() {
        let (mut r, dir) = bare("socket-in-tree", "count = 1\n", APP, "ignore = []\n");
        let app = r.sup.cfg.app.name.clone();
        r.sup.cfg.control.socket = Some(dir.join("warden.sock"));
        r.sup.runtime_dir = dir.clone();
        for f in [
            "app.js".to_string(),
            "lib/util.js".to_string(),
            "warden.sock".to_string(),
            format!("{app}-shim.mjs"),
            format!("{app}-shim.tmp"),
            format!("{app}-host.mjs"),
            format!("{app}.h1-1.sock"),
            // Neighbours in a shared directory are the app's.
            format!("{app}.helpers.js"),
            format!("{app}-shimmer.js"),
        ] {
            write(&dir, &f);
        }
        let spec = r.sup.watch_spec().unwrap().unwrap();
        assert_eq!(files_seen(&spec), 4, "app.js, lib/util.js and the two neighbours: {:?}", spec.skip);

        // The runtime directory above the working directory (`/tmp/app.sock`, the app in `/tmp/app`).
        let work = dir.join("app");
        write(&work, "main.js");
        r.sup.cfg.app.working_directory = Some(work.clone());
        assert_eq!(files_seen(&r.sup.watch_spec().unwrap().unwrap()), 1, "main.js, not the files next to it");

        // Inside the tree, and only Warden's: skipped whole, whatever is in it.
        let run = dir.join("app/run");
        write(&run, "control.sock");
        write(&run, "anything.else");
        r.sup.runtime_dir = run.clone();
        r.sup.cfg.control.socket = Some(run.join("control.sock"));
        assert_eq!(files_seen(&r.sup.watch_spec().unwrap().unwrap()), 1, "main.js; run/ is Warden's");

        // The same directory reached through a symlink (`/var/run` is one on some systems): the
        // files are found under their real names, so the real name is skipped as well.
        let link = dir.join("link");
        std::os::unix::fs::symlink(&run, &link).unwrap();
        r.sup.runtime_dir = link.clone();
        r.sup.cfg.control.socket = Some(link.join("control.sock"));
        assert_eq!(files_seen(&r.sup.watch_spec().unwrap().unwrap()), 1, "main.js; link/ is run/, Warden's");

        // The working directory reached through a symlink above it, the runtime directory by
        // its real name (on macOS the temporary directory is under /var, which is /private/var):
        // the walk spells the files as the working directory is written, and still skips run/.
        let above = dir.join("above");
        std::os::unix::fs::symlink(&dir, &above).unwrap();
        r.sup.cfg.app.working_directory = Some(above.join("app"));
        r.sup.runtime_dir = std::fs::canonicalize(&run).unwrap();
        r.sup.cfg.control.socket = Some(r.sup.runtime_dir.join("control.sock"));
        assert_eq!(files_seen(&r.sup.watch_spec().unwrap().unwrap()), 1, "main.js; above/app/run/ is run/");
        tidy(&dir);
    }

    /// F3: with no working directory and a directory Warden started in that is
    /// gone, there is no place to look: not "/".
    #[tokio::test(flavor = "current_thread")]
    async fn a_deleted_starting_directory_does_not_make_the_watcher_look_at_the_root() {
        local(async {
            let (mut r, dir) = bare("no-cwd", "count = 1\n", APP, "");
            let fresh = Rig::new("watch-cwd", "", APP);
            assert_eq!(fresh.sup.launch_cwd, std::env::current_dir().ok(), "taken once, at the start");
            drop(fresh);
            let _ = std::fs::remove_dir_all(
                std::env::temp_dir().join(format!("warden-rig-watch-cwd-{}", std::process::id())),
            );
            r.sup.cfg.app.working_directory = None;
            r.sup.launch_cwd = None;
            let e = r.sup.watch_spec().unwrap_err();
            assert!(e.contains("working directory") && e.contains("cannot be read"), "{e}");
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none() && r.sup.watch_blocked && !r.sup.status().watching);
            let token = r.sup.watch_token;
            r.sup.sync_watch();
            assert_eq!(r.sup.watch_token, token, "said once, not every tick");
            // Absolute paths need no starting directory.
            r.sup.cfg.app.working_directory = Some(dir.clone());
            assert!(r.sup.watch_spec().unwrap().is_some());
            r.sup.sync_watch();
            assert!(r.sup.watch.is_some() && !r.sup.watch_blocked, "watching as soon as it can say where");
            // A relative log file needs it.
            r.sup.launch_cwd = None;
            r.sup.cfg.logging.file = Some(PathBuf::from("warden.out"));
            let e = r.sup.watch_spec().unwrap_err();
            assert!(e.contains("the log file warden.out"), "{e}");
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none(), "a watcher that cannot be told where to look goes");
            tidy(&dir);
        })
        .await;
    }

    /// F4: a watcher that died is noticed (not trusted), said once, not
    /// restarted every tick, and started again by `warden reload`.
    #[tokio::test(flavor = "current_thread")]
    async fn a_dead_watcher_is_noticed_and_a_reload_starts_another() {
        local(async {
            let (mut r, dir) = rig("dead", 1, "debounce_ms = 0\ninterval_ms = 100\n").await;
            // Instead of the real watcher: one that panics when it is told of a change.
            let spec = r.sup.watch_spec().unwrap().unwrap();
            r.sup.watch = Some((spec.clone(), watch::spawn(spec, |_| panic!("boom")).unwrap()));
            tokio::time::sleep(Duration::from_millis(300)).await;
            write(&dir, "x.js");
            let t0 = Instant::now();
            while !r.sup.watch.as_ref().is_some_and(|(_, h)| h.is_finished()) {
                assert!(t0.elapsed() < Duration::from_secs(5), "the watcher never died");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(!r.sup.status().watching, "a dead watcher is not watching");
            r.sup.sync_watch();
            assert!(r.sup.watch.is_none() && r.sup.watch_failed.is_some());
            let token = r.sup.watch_token;
            r.sup.sync_watch();
            assert_eq!((r.sup.watch.is_none(), r.sup.watch_token), (true, token), "not started again every tick");
            assert!(!r.sup.status().watching);
            // What the log hint says.
            assert!(r.sup.request_reload(false).ok);
            r.until("the reload ends", |s| s.roll.is_none()).await;
            r.sup.sync_watch();
            assert!(r.sup.watch.is_some() && r.sup.status().watching, "watching again");
            finish(r, dir).await;
        })
        .await;
    }

    /// F6: with workers that cannot run side by side a failing restart leaves
    /// the app down: a watcher that starts says so.
    #[test]
    fn watching_an_app_that_cannot_roll_back_says_so() {
        let (mut r, dir) = bare("no-overlap", "count = 1\n", APP, "");
        assert_eq!(r.sup.watch_start_warning(), None, "workers overlap (overlap = true)");
        r.sup.cfg.workers.overlap = None;
        // `sh` on a port is no Bun or Node: no shim, no SO_REUSEPORT.
        let w = r.sup.watch_start_warning().unwrap();
        assert!(w.contains("without Warden's shim") && w.contains("leaves that worker down"), "{w}");
        r.sup.cfg.workers.overlap = Some(false);
        assert!(r.sup.watch_start_warning().unwrap().contains("overlap = false"));
        r.sup.cfg.workers.overlap = None;
        r.sup.cfg.app.port = None;
        assert_eq!(r.sup.watch_start_warning(), None, "no port: nothing to collide on");
        r.sup.cfg.workers.port_strategy = PortStrategy::Offset;
        assert!(r.sup.watch_start_warning().unwrap().contains("offset"));
        tidy(&dir);
    }

    /// A restart that cannot begin (the release does not resolve: a deploy
    /// may be halfway) is tried again a few times, then given up.
    #[tokio::test(flavor = "current_thread")]
    async fn a_restart_that_cannot_begin_is_retried_a_few_times() {
        local(async {
            let (mut r, dir) = rig("retry", 1, "").await;
            let moved = dir.with_extension("away");
            std::fs::rename(&dir, &moved).unwrap();
            let token = r.sup.watch_token;
            r.sup.on_watch(token, change("a.js"));
            assert!(r.sup.roll.is_none());
            assert!(matches!(r.sup.watch_retry, Some((1, _))), "{:?}", r.sup.watch_retry);
            assert!(r.sup.watch_pending.is_some(), "kept for the next try");

            // Not before the retry time; then again, and the directory is back.
            r.sup.try_watch_restart();
            assert!(matches!(r.sup.watch_retry, Some((1, _))), "too early to count as a try");
            std::fs::rename(&moved, &dir).unwrap();
            r.sup.watch_retry = Some((1, Instant::now()));
            r.sup.try_watch_restart();
            assert!(r.sup.roll.is_some() && r.sup.watch_retry.is_none() && r.sup.watch_pending.is_none());
            r.until("the restart ends", |s| s.roll.is_none()).await;
            r.sup.watch_throttle = watch::Throttle::default();

            // Three failures in a row: dropped (the next change tries afresh).
            std::fs::rename(&dir, &moved).unwrap();
            r.sup.on_watch(token, change("b.js"));
            for _ in 0..2 {
                r.sup.watch_retry = r.sup.watch_retry.map(|(n, _)| (n, Instant::now()));
                r.sup.try_watch_restart();
            }
            assert!(r.sup.watch_retry.is_none() && r.sup.watch_pending.is_none(), "gave up");
            std::fs::rename(&moved, &dir).unwrap();
            finish(r, dir).await;
        })
        .await;
    }
}

/// While the workers a killed supervisor left behind are being stopped
/// (`platform::orphans`, macOS) the supervisor answers, obeys a stop, and
/// starts nothing, not even a standby on a tick: the old workers hold the port.
#[cfg(test)]
mod sweep_tests {
    use super::rig::{Rig, local};
    use super::*;

    /// A worker that listens at once and stays up.
    const APP: &str = "echo '{\"ev\":\"listening\",\"port\":1}' >&3\nexec sleep 60\n";

    fn sweeping(r: &mut Rig) {
        r.sup.sweep =
            Some(Sweeping { started: Instant::now(), progress: Default::default(), longest: Duration::from_secs(33) });
    }

    #[tokio::test(flavor = "current_thread")]
    async fn status_says_what_it_waits_for_and_a_tick_starts_nothing() {
        local(async {
            // A standby would start on any tick (nothing is Starting); the sweep forbids it.
            let mut r = Rig::new("sweep-status", "[workers]\ncount = 2\nstandby = 1\n", APP);
            sweeping(&mut r);
            let st = r.sup.status();
            let ro = st.rollout.expect("the sweep shows as a rollout of its own kind");
            assert_eq!(ro.kind, "sweep");
            assert!(ro.phase.contains("waiting for the previous supervisor's workers") && ro.phase.contains("33 s"));
            assert_eq!((ro.done, ro.total, ro.seq), (0, 0, 0));
            assert!(st.reloading && st.workers.is_empty() && !st.stopped && !st.shutting_down);
            r.sup.on_event(Event::Tick);
            assert!(r.sup.insts.is_empty(), "no standby, no worker, during the sweep");
            // The same tick starts one without it: the guard is what held it back.
            r.sup.sweep = None;
            r.sup.on_event(Event::Tick);
            assert_eq!(r.sup.insts.len(), 1, "a standby starts on a tick when nothing sweeps");
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn what_would_start_or_change_workers_waits_and_the_rest_is_served() {
        local(async {
            let mut r = Rig::new("sweep-requests", "[workers]\ncount = 2\n", APP);
            sweeping(&mut r);
            for req in [
                Request::Reload { safe: false },
                Request::Reload { safe: true },
                Request::Restart { worker: None, hard: false },
                Request::Restart { worker: None, hard: true },
                Request::Restart { worker: Some(1), hard: false },
                Request::Scale { count: 4 },
                Request::Reset { worker: None },
            ] {
                let resp = r.sup.on_request(req);
                assert!(!resp.ok, "refused while sweeping");
                assert!(resp.message.unwrap_or_default().contains("try again then"));
            }
            assert!(r.sup.on_request(Request::Status).status.is_some(), "status is answered");
            // SIGHUP is a reload too.
            assert!(!r.sup.request_reload(false).ok);
            assert!(r.sup.insts.is_empty() && r.sup.slots.is_empty(), "nothing started");
            assert_eq!(r.sup.count, 2, "and nothing scaled");
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_start_or_a_stop_is_remembered_for_when_the_sweep_ends() {
        local(async {
            // `stop` while sweeping: the workers stay stopped afterwards.
            let mut r = Rig::new("sweep-stop", "[workers]\ncount = 2\n", APP);
            sweeping(&mut r);
            assert!(r.sup.on_request(Request::Stop).ok);
            assert!(r.sup.stopped);
            r.sup.sweep_over(None, false);
            assert!(r.sup.sweep.is_none() && r.sup.stopped && r.sup.insts.is_empty(), "no worker starts");
            assert_eq!(r.sup.status().rollout.map(|x| x.kind), None, "and the sweep is not shown any more");
            r.shutdown().await;

            // `start` after it: they start when it ends.
            let mut r = Rig::new("sweep-start", "[workers]\ncount = 2\n", APP);
            sweeping(&mut r);
            assert!(r.sup.on_request(Request::Stop).ok);
            assert!(r.sup.on_request(Request::Start).ok);
            assert!(!r.sup.stopped);
            assert!(r.sup.insts.is_empty(), "not before the old workers are gone");
            r.sup.sweep_over(None, false);
            r.until("both workers running", |s| s.insts.len() == 2).await;
            r.shutdown().await;

            // `warden save` recorded it stopped: it stays so.
            let mut r = Rig::new("sweep-saved", "[workers]\ncount = 2\n", APP);
            sweeping(&mut r);
            r.sup.sweep_over(None, true);
            assert!(r.sup.stopped && r.sup.insts.is_empty());
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_shutdown_during_the_sweep_ends_the_supervisor_without_starting_a_worker() {
        local(async {
            let mut r = Rig::new("sweep-shutdown", "[workers]\ncount = 2\n", APP);
            sweeping(&mut r);
            assert!(r.sup.on_request(Request::Shutdown).ok);
            assert!(r.sup.shutting_down && r.sup.insts.is_empty(), "nothing to wait for: the loop ends");
            // The sweep may still finish in the same turn: it starts nothing.
            r.sup.sweep_over(None, false);
            assert!(r.sup.insts.is_empty() && r.sup.slots.is_empty());
            // `start` and `stop` are not remembered during a shutdown.
            sweeping(&mut r);
            assert!(r.sup.on_request(Request::Start).message.unwrap_or_default().contains("shutting down"));
            r.shutdown().await;
        })
        .await;
    }
}

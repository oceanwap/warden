//! The supervisor: one event loop owning all state (no locks). Child exits,
//! readiness, timers, signals, health results and CLI requests all arrive as
//! events and are handled in order.
//!
//! - `rollout.rs`: gated replacement of workers (reload, safe-reload with a
//!   canary, restart N, recycling), preflight, rollback.
//! - `upkeep.rs`: the 1 s maintenance tick (watchdog, per-worker health,
//!   memory / lifetime recycling, FAILED cooldown).
//! - `standby.rs`: hot standbys (`[workers] standby`), promoted into the
//!   slot of a worker that died.
//! - `release.rs`: release pinning (`[app] pin_release`).

mod release;
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
use crate::worker::{Instance, Role, STANDBY_SLOT, Slot, State, ThreadInfo, describe_exit, standby_label};
use crate::{debug, error, info, metrics, networking, systemd, warn};
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
    supervisor_cpu_prev: Option<(Instant, f64)>,
    ticks: u64,
    /// Fleet-wide health failure: a dependency is probably down; replacements held.
    outage: bool,
    last_tick: Instant,
    /// systemd set WATCHDOG_USEC for us (WatchdogSec= in the unit).
    watchdog_enabled: bool,
    /// Invalidates the pending scheduled restart when the schedule changes.
    schedule_token: u64,
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
    if !sup.cfg.any_worker_path() {
        info!("no health path configured: new workers are gated on listening only (set [health] path)");
    }
    match sup.oom.own() {
        Some(p) => debug!("OOM kills are told apart through the cgroup's counter", file = p.display()),
        None => debug!("no cgroup OOM kill counter found: a SIGKILL's sender can't be told apart from an OOM kill"),
    }
    sup.schedule_next();
    if saved.as_ref().is_some_and(|s| s.stopped) {
        info!("workers stay stopped, as saved by `warden save`", hint = "`warden start <app>` starts them");
        sup.stopped = true;
        sup.announced_ready = true;
        systemd::notify("READY=1\nSTATUS=workers stopped (saved state)");
    } else {
        sup.start_all();
    }

    loop {
        tokio::select! {
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
            supervisor_cpu_prev: None,
            ticks: 0,
            outage: false,
            last_tick: Instant::now(),
            watchdog_enabled: systemd::watchdog_requested(),
            schedule_token: 0,
            exe: own_exe(),
            shutdown_reason: "shutdown".into(),
            rollout_published: None,
            pool: standby::Pool::default(),
            release: None,
            oom: process::exit::OomTracker::new(),
            cfg,
            cfg_path,
        }
    }

    fn is_worker_mode(&self) -> bool {
        self.cfg.workers.mode == Mode::Worker
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
        for id in self.slot_ids() {
            let slot = self.slots.entry(id).or_insert_with(|| Slot::new(id));
            slot.tracker.reset();
            slot.failed_at = None;
            slot.token += 1;
            self.spawn_current(id);
        }
        // Standbys follow once the workers listen (`fill_pool` waits for them).
        self.fill_pool();
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
        let mut env: Vec<(String, String)> = a.environment().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut add = |k: &str, v: String| env.push((k.to_string(), v));
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
        if self.cfg.watchdog.timeout > 0 {
            add("WARDEN_HEARTBEAT_MS", HEARTBEAT_MS.to_string());
        }
        if self.cfg.workers.port_strategy == PortStrategy::Shared {
            add("WARDEN_REUSE_PORT", "1".into());
        }
        if let Some(p) = a.port {
            // Standbys (slot 0) need a shared port (config validation).
            add("PORT", networking::worker_port(p, self.cfg.workers.port_strategy, slot_id.max(1)).to_string());
        }
        let (program, args) = match self.cfg.workers.mode {
            Mode::Process if slot_id == STANDBY_SLOT => {
                // The shim defers its listen until promoted; it then takes the
                // slot's worker id and instance number.
                add("WARDEN_WORKER_ID", "0".into());
                add("WARDEN_STANDBY", "1".into());
                if let (false, Some((_, instance))) = (a.instance_var.is_empty(), standby) {
                    // Past the workers' numbers and unique: code that runs
                    // only on one instance (cron on 0) does not run in a
                    // standby. A scale-up past it replaces the standby.
                    add(&a.instance_var, instance.to_string());
                }
                // In the pinned release, as workers (`release.rs`).
                let args: Vec<String> = a.args.iter().map(|x| self.pinned_arg(x)).collect();
                (self.pinned_arg(&a.command), with_preload(&a.command, &args, self.shim_path.as_deref()))
            }
            Mode::Process => {
                add("WARDEN_WORKER_ID", slot_id.to_string());
                if !a.instance_var.is_empty() {
                    add(&a.instance_var, (slot_id - 1).to_string());
                }
                match &self.cfg.static_files {
                    Some(st) => {
                        let mut st = st.clone();
                        st.root = self.pinned_path(st.root);
                        add("WARDEN_STATIC", serde_json::to_string(&st).unwrap_or_default());
                        (self.exe.display().to_string(), vec!["serve-static".to_string()])
                    }
                    None => {
                        let args: Vec<String> = a.args.iter().map(|x| self.pinned_arg(x)).collect();
                        (self.pinned_arg(&a.command), with_preload(&a.command, &args, self.shim_path.as_deref()))
                    }
                }
            }
            Mode::Worker => {
                add("WARDEN_WORKERS", self.count.to_string());
                if let Some(shim) = &self.shim_path {
                    add("WARDEN_SHIM", shim.display().to_string());
                }
                add("WARDEN_ENTRY", self.entry_path().display().to_string());
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
            Event::WorkerHealth { inst, result } => self.on_worker_health(inst, result),
            Event::Tick => self.on_tick(),
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
                inst.heartbeats.insert(worker, Instant::now());
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
        let Some(inst) = self.insts.get_mut(&inst_id) else { return };
        // A standby is ready once initialized (`standby_ready`), not here.
        if inst.ready_at.is_some() || inst.stopping || inst.role == Role::Standby {
            return;
        }
        let now = Instant::now();
        inst.ready_at = Some(now);
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
        for sock in inst.sockets.values() {
            let _ = std::fs::remove_file(sock);
        }
        // Who ended it: Warden, the OOM killer (just before), someone else, a
        // crash. Its cgroup's OOM kill is its own only if no other process of
        // Warden's there is dying of SIGKILL at this moment to share it.
        let counter = inst.oom_counter.clone();
        let (oom, insts) = (&mut self.oom, &self.insts);
        let verdict = oom.verdict(counter.as_deref(), signal, sent, Instant::now(), || {
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
                if let Some(s) = self.slots.get_mut(&slot_id) {
                    s.current = None;
                    s.state = State::Stopped;
                    s.last_exit = Some(why);
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
        self.stop_all();
    }

    fn on_signal(&mut self, s: Sig) {
        match s {
            Sig::Term | Sig::Int => self.begin_shutdown(s.name()),
            Sig::Hup => {
                let r = self.request_reload(false);
                if !r.ok {
                    warn!("SIGHUP ignored", reason = r.message.unwrap_or_default());
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

    fn request_reload(&mut self, safe: bool) -> Response {
        if self.shutting_down {
            return Response::err("shutting down");
        }
        if self.stopped {
            return Response::err("workers are stopped; use `warden restart`");
        }
        let ids: Vec<usize> = self.slots.values().filter(|s| !s.removing).map(|s| s.id).collect();
        let kind = if safe { Kind::SafeReload } else { Kind::Reload };
        match self.begin_rollout(kind, ids, String::new(), false) {
            Ok(seq) => Response::started(if safe { "safe-reload started" } else { "reload started" }, seq),
            Err(e) => Response::err(e),
        }
    }

    fn on_request(&mut self, req: Request) -> Response {
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
            Request::Config { show_secrets } => Response { info: Some(self.info(show_secrets)), ..Response::ok("") },
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
    fn info(&self, show_secrets: bool) -> serde_json::Value {
        let mut v = serde_json::to_value(&self.cfg).unwrap_or(serde_json::Value::Null);
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
            "shim": self.shim_path.as_ref().map(|p| p.display().to_string()),
            "workers_running": self.count,
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
                });
            }
        } else {
            for s in self.slots.values() {
                let inst = s.current.and_then(|c| self.insts.get_mut(&c));
                let (pid, uptime, stats, healthy) = match inst {
                    Some(i) => {
                        let started = i.started;
                        let pid = i.handle.pid;
                        (Some(pid), Some(started.elapsed().as_secs()), sample(pid, &mut i.cpu_prev, started), i.healthy)
                    }
                    None => (None, None, None, None),
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
            });
        }
        draining.sort_by_key(|w| w.id);
        let me = std::process::id();
        let started = self.started;
        let sup = sample(me, &mut self.supervisor_cpu_prev, started);
        let rollout = self.rollout_status();
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
fn own_exe() -> PathBuf {
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
}

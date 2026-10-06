//! Rollouts: replacing running workers without dropping traffic.
//!
//! Every replacement goes through the same gates, in order, before the worker
//! it replaces is drained (inspired by Kubernetes readiness probes +
//! `minReadySeconds` + `progressDeadlineSeconds`, and nginx's `-t` preflight):
//!
//! 1. listening on the port                      (Starting)
//! 2. `reload.health_passes` consecutive checks
//!    on the new worker's private socket         (Verifying)
//! 3. `reload.verify_command` exits 0            (Verifying)
//! 4. stays up and healthy for `min_ready`, or
//!    `canary_soak` for a safe-reload canary     (Verifying)
//! 5. promote: the old worker is drained         (Draining)
//!
//! The old worker keeps serving until step 5, so a failure anywhere before
//! that is a rollback for free: the new process is stopped and nothing else
//! changes. `safe-reload` adds preflight checks, requires a healthy fleet,
//! soaks the first worker as a canary and pauses between workers; any failure
//! halts the rollout.
//!
//! Workers are replaced in batches (`[reload] surge`, like Kubernetes'
//! `maxSurge`): each worker of a batch is a lane going through the gates on
//! its own, next to the worker it replaces; once every lane has passed, the
//! batch's old workers drain together. A failure in any lane stops the new
//! workers of every lane, so the whole batch rolls back. With `surge = 1`
//! (the default) a batch is one worker. A slot that isn't serving (starting,
//! stopping, down), or that can't overlap, is always a batch of its own.
//!
//! Drains overlap (`[reload] max_draining`): once a batch's old workers have
//! been told to stop (their replacements already listen and passed the
//! gates, so capacity never drops), the next batch starts while they finish
//! draining in the background: closing WebSockets and SSE streams after
//! `long_lived_timeout`, finishing requests in flight. A batch starts only
//! while its old workers fit in max(max_draining, batch size) draining at
//! once, which bounds the extra memory. The rollout ends (and the CLI
//! returns) once every old worker it stopped has exited, by itself or by
//! SIGKILL at `grace_period`; a failure rolls back the batch in progress at
//! once and then waits for those drains too.

use super::*;
use crate::control::{RolloutOutcome, RolloutStatus};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// `warden reload`, SIGHUP, scale in worker mode.
    Reload,
    /// `warden safe-reload`: preflight, canary soak, pauses.
    SafeReload,
    /// `warden restart <id>`.
    Restart,
    /// Recycling one worker: health, memory, lifetime, hang.
    Replace,
    /// Worker mode: a Worker thread died; replace the host.
    Recovery,
}

impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Kind::Reload => "reload",
            Kind::SafeReload => "safe-reload",
            Kind::Restart => "restart",
            Kind::Replace => "replace",
            Kind::Recovery => "recovery",
        }
    }
    fn is_deploy(self) -> bool {
        matches!(self, Kind::Reload | Kind::SafeReload)
    }
}

/// `Tick`: deadlines, drains, and a fallback for the gates' own timers.
/// `Due`: a lane's next gate may be due (its next health check, the end of
/// its soak, or of its wait for its private socket).
pub(super) enum GateEvent {
    Tick { seq: u64 },
    Due { seq: u64, inst: u64 },
    Check { seq: u64, inst: u64, result: Result<(), String> },
    Command { seq: u64, inst: u64, result: Result<(), String> },
    Preflight { seq: u64, result: Result<(), String> },
    PauseDone { seq: u64 },
}

pub(super) struct Roll {
    pub(super) seq: u64,
    pub(super) kind: Kind,
    reason: String,
    queue: VecDeque<usize>,
    total: usize,
    done: usize,
    step: Step,
    started: Instant,
    /// SIGKILL the old worker instead of draining it (it is hung).
    kill_old: bool,
    /// The next replacement is the safe-reload canary.
    canary: bool,
    /// Workers replaced together (`[reload] surge`); 1 = one at a time.
    surge: usize,
    /// Batches started so far.
    batches: usize,
    /// Config (and release pin) in effect before this rollout's preflight
    /// applied a new one; restored if the rollout fails, so restarts don't
    /// roll forward to it.
    prev: Option<Snapshot>,
    /// Old workers this rollout stopped once their replacements took over,
    /// until they exit (see the module doc: drains overlap).
    draining: Vec<Drain>,
    /// The next batch waits for drains to make room (logged once per wait).
    held: bool,
    /// Dropping this stops the gate ticker.
    _ticker: oneshot::Sender<()>,
}

/// An old worker draining in the background.
struct Drain {
    slot: usize,
    old: u64,
    /// When it was told to stop: past `grace_period` it got SIGKILL, and
    /// past that plus `DRAIN_SLACK` the rollout stops waiting for it.
    since: Instant,
}

/// How long past `grace_period` (its SIGKILL) a rollout still waits for an
/// old worker to exit. A SIGKILLed process ends at once unless the kernel
/// holds it (uninterruptible I/O, a hung network filesystem); then the
/// rollout ends without it rather than never (it stays listed as draining
/// until it is reaped).
const DRAIN_SLACK: Duration = Duration::from_secs(10);

struct Snapshot {
    cfg: Config,
    shim_path: Option<PathBuf>,
    host_path: Option<PathBuf>,
    release: Option<release::Pin>,
}

enum Step {
    /// Between batches; persists only while the next batch waits for drains
    /// (`Roll::held`).
    Idle,
    Preflight,
    /// One or more workers being replaced together.
    Batch(Vec<Lane>),
    Pausing,
    /// Nothing left to replace, or the rollout failed (its batch rolled back,
    /// its failure logged): it ends once `Roll::draining` is empty.
    Ending {
        ok: bool,
        message: Option<String>,
    },
}

/// One slot of a batch.
struct Lane {
    slot: usize,
    at: At,
}

enum At {
    /// The new process was started; waiting for it to listen.
    Starting {
        new: u64,
        old: Option<u64>,
        deadline: Instant,
    },
    Verifying(Verify),
    /// Passed every gate; waits for the rest of its batch before taking over.
    Passed {
        new: u64,
        old: Option<u64>,
    },
    /// The old process must exit before the new one starts: workers that
    /// can't overlap, or a slot that wasn't serving (starting, stopping).
    Vacating {
        old: u64,
    },
    /// The new process took over (its old one, if any, drains in the
    /// background: `Roll::draining`).
    Done,
}

impl At {
    /// The new process, until it has taken over.
    fn new_inst(&self) -> Option<u64> {
        match self {
            At::Starting { new, .. } | At::Passed { new, .. } => Some(*new),
            At::Verifying(v) => Some(v.new),
            At::Vacating { .. } | At::Done => None,
        }
    }

    fn involves(&self, inst: u64) -> bool {
        match self {
            At::Starting { new, old, .. } | At::Passed { new, old } => *new == inst || *old == Some(inst),
            At::Verifying(v) => v.new == inst || v.old == Some(inst),
            At::Vacating { old } => *old == inst,
            At::Done => false,
        }
    }
}

struct Verify {
    new: u64,
    old: Option<u64>,
    deadline: Instant,
    passes: u32,
    fails: u32,
    checking: bool,
    /// No health check before this: `health_interval_ms` after the previous
    /// one finished. `None` until the first, which runs once it listens.
    next_check: Option<Instant>,
    /// Found listening (its port open) before the shim reported its
    /// private socket: the first check waits for the report until this,
    /// one interval, then runs anyway.
    socket_wait: Option<Instant>,
    cmd: Cmd,
    soak: Duration,
    soak_until: Option<Instant>,
    canary: bool,
}

impl Verify {
    /// How far along the gates: health checks, verify_command, soak.
    fn stage(&self) -> u64 {
        if self.soak_until.is_some() {
            2
        } else if matches!(self.cmd, Cmd::Running | Cmd::Passed) {
            1
        } else {
            0
        }
    }
}

/// A rollout's phase for change detection (see `rollout_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PhaseKey {
    seq: u64,
    done: usize,
    step: u8,
    /// The instance the step works on (the batch's first).
    inst: u64,
    /// Verifying: health passes, or verify_command running, or soaking;
    /// for a batch, how many of its workers got how far.
    detail: u64,
    /// Old workers draining in the background.
    draining: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Skip,
    Pending,
    Running,
    Passed,
}

/// What `rollout_on_exit` does once it has let go of the batch.
enum OnExit {
    Fail(usize, String),
    Spawn(usize),
}

impl Supervisor {
    /// Start a rollout over `slots`. Preflight failures return `Err` and touch nothing.
    pub(super) fn begin_rollout(
        &mut self,
        kind: Kind,
        slots: Vec<usize>,
        reason: String,
        kill_old: bool,
    ) -> Result<u64, String> {
        if self.shutting_down || self.stopped {
            return Err("workers are stopped or shutting down".into());
        }
        if let Some(r) = &self.roll {
            return Err(format!("a {} is already in progress; try again when it finishes", r.kind.name()));
        }
        if kind == Kind::SafeReload {
            // Don't deploy onto a fleet that is already broken.
            if let Some(s) = self.slots.values().find(|s| s.state != State::Running) {
                return Err(format!(
                    "worker {} is {}; safe-reload needs every worker RUNNING (fix it or run `warden restart {}` first, or use `warden reload`)",
                    self.label(s.id),
                    s.state.as_str(),
                    s.id
                ));
            }
            if let Some(i) = self.insts.values().find(|i| i.role == Role::Current && i.healthy == Some(false)) {
                return Err(format!(
                    "worker {} is failing health checks; fix it before a safe-reload",
                    self.label(i.slot)
                ));
            }
        }

        // Deploys, and restarts of every worker, move to the release
        // `current` points to now (`[app] pin_release`); restoring the
        // snapshot puts the previous pin back.
        let all_slots = slots.len() >= self.slots.values().filter(|s| !s.removing).count();
        let repin = kind.is_deploy() || (kind == Kind::Restart && all_slots);
        let prev = if repin {
            let snap = Snapshot {
                cfg: self.cfg.clone(),
                shim_path: self.shim_path.clone(),
                host_path: self.host_path.clone(),
                release: self.release.clone(),
            };
            let checked = if kind.is_deploy() {
                self.preflight_sync()
            } else {
                self.pin_release().map_err(|e| format!("{e}; nothing was restarted"))
            };
            if let Err(e) = checked {
                self.restore(snap);
                return Err(e);
            }
            Some(snap)
        } else {
            None
        };
        self.roll_seq += 1;
        let seq = self.roll_seq;
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let tx = self.tx.clone();
        let every = Duration::from_millis(self.cfg.reload.health_interval_ms);
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(every) => {
                        if tx.send(Event::Gate(GateEvent::Tick { seq })).is_err() { return; }
                    }
                    _ = &mut cancel_rx => return,
                }
            }
        });

        let total = slots.len();
        let surge = match kind {
            Kind::Reload | Kind::SafeReload | Kind::Restart => self.cfg.reload.surge.batch(total),
            Kind::Replace | Kind::Recovery => 1,
        };
        // The log line of each rollout: what, how many at a time, which release.
        let release = self.release_text();
        if matches!(kind, Kind::Replace | Kind::Recovery) {
            let worker = slots.first().map(|s| self.label(*s)).unwrap_or_default();
            let mut fields: Vec<(&str, &dyn std::fmt::Display)> = vec![("worker", &worker), ("reason", &reason)];
            if let Some(r) = &release {
                fields.push(("release", r));
            }
            crate::logging::event(crate::config::Level::Info, "replacing worker", &fields);
        } else {
            let mut fields: Vec<(&str, &dyn std::fmt::Display)> = vec![("workers", &total), ("seq", &seq)];
            if surge > 1 {
                fields.push(("surge", &surge));
            }
            if let Some(r) = &release {
                fields.push(("release", r));
            }
            crate::logging::event(crate::config::Level::Info, &format!("{} started", kind.name()), &fields);
            if kind.is_deploy() {
                systemd::reloading();
            }
        }
        let preflight = if kind.is_deploy() { self.cfg.reload.preflight.clone() } else { None };
        self.roll = Some(Roll {
            seq,
            kind,
            reason,
            queue: slots.into(),
            total,
            done: 0,
            step: Step::Idle,
            started: Instant::now(),
            kill_old,
            canary: kind == Kind::SafeReload,
            surge,
            batches: 0,
            prev,
            draining: Vec::new(),
            held: false,
            _ticker: cancel_tx,
        });
        if preflight.is_some() {
            if let Some(r) = &mut self.roll {
                r.step = Step::Preflight;
            }
        }
        // Subscribers hear of the rollout before any worker is touched.
        self.publish_rollout();
        match preflight {
            Some(cmd) => {
                info!("running preflight", command = cmd);
                let env = vec![("WARDEN_APP".to_string(), self.cfg.app.name.clone())];
                let fut = run_shell(cmd, self.worker_dir(), env, Duration::from_secs(self.cfg.reload.timeout));
                let tx = self.tx.clone();
                tokio::task::spawn_local(async move {
                    let result =
                        crate::guard::catch_unwind(fut).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
                    let _ = tx.send(Event::Gate(GateEvent::Preflight { seq, result }));
                });
            }
            None => self.advance_rollout(),
        }
        Ok(seq)
    }

    /// Like `nginx -t`: re-read and validate the config and check that what
    /// we are about to run exists, before any worker is touched.
    fn preflight_sync(&mut self) -> Result<(), String> {
        if let Some(path) = self.cfg_path.clone() {
            let new = Config::load(&path).map_err(|e| format!("preflight: config error, nothing was changed: {e}"))?;
            self.apply_config(new)?;
        }
        if let Some(wd) = &self.cfg.app.working_directory {
            if !wd.is_dir() {
                return Err(format!("preflight: working_directory {} does not exist", wd.display()));
            }
        }
        self.pin_release().map_err(|e| format!("preflight: {e}"))?;
        let base = self.worker_dir().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if !base.is_dir() {
            return Err(format!("preflight: working_directory {} does not exist", base.display()));
        }
        match self.cfg.workers.mode {
            Mode::Worker => {
                let e = self.entry_path();
                if !e.is_file() {
                    return Err(format!("preflight: entry {} not found", e.display()));
                }
            }
            Mode::Process if self.cfg.static_files.is_some() => {
                let root = self.cfg.static_files.as_ref().map(|s| s.root.clone()).unwrap_or_default();
                let root = self.pinned_path(root);
                if !root.is_dir() {
                    return Err(format!("preflight: static.root {} is not a directory", root.display()));
                }
            }
            Mode::Process => {
                let env: std::collections::BTreeMap<String, String> =
                    self.cfg.app.environment().map(|(k, v)| (k.clone(), v.clone())).collect();
                let command = self.pinned_arg(&self.cfg.app.command);
                if !command_exists(&command, &env) {
                    return Err(format!("preflight: command `{}` not found", self.cfg.app.command));
                }
                let script = self.cfg.app.args.iter().find(|a| {
                    !a.starts_with('-')
                        && [".js", ".mjs", ".cjs", ".ts", ".mts", ".tsx", ".jsx"].iter().any(|x| a.ends_with(x))
                });
                if let Some(s) = script {
                    let p = base.join(self.pinned_arg(s));
                    if !p.is_file() {
                        return Err(format!("preflight: {} not found", p.display()));
                    }
                }
            }
        }
        let (shim, host) = write_js(&self.cfg, &self.runtime_dir)?;
        self.shim_path = shim;
        self.host_path = host;
        Ok(())
    }

    fn restore(&mut self, snap: Snapshot) {
        self.cfg = snap.cfg;
        self.policy = Policy::from(&self.cfg.restart);
        self.shim_path = snap.shim_path;
        self.host_path = snap.host_path;
        self.release = snap.release;
    }

    /// Apply what can change without restarting Warden; report the rest.
    fn apply_config(&mut self, new: Config) -> Result<(), String> {
        let old = &self.cfg;
        if new.app.name != old.app.name {
            return Err("preflight: app.name cannot change while running".into());
        }
        let diff = |a: &dyn std::fmt::Debug, b: &dyn std::fmt::Debug| format!("{a:?}") != format!("{b:?}");
        let mut applied = Vec::new();
        let mut ignored = Vec::new();
        if diff(&old.app, &new.app) {
            applied.push("app");
        }
        if diff(&old.reload, &new.reload) {
            applied.push("reload");
        }
        if diff(&old.limits, &new.limits) {
            applied.push("limits");
        }
        if diff(&old.watchdog, &new.watchdog) {
            applied.push("watchdog");
        }
        let schedule_changed = old.restart.schedule != new.restart.schedule;
        if diff(&old.restart, &new.restart) {
            applied.push("restart");
        }
        if diff(&old.shutdown, &new.shutdown) {
            applied.push("shutdown");
        }
        if diff(&old.health, &new.health) {
            applied.push("health (per-worker checks)");
        }
        if old.health.url != new.health.url
            || old.health.enabled != new.health.enabled
            || old.health.interval != new.health.interval
        {
            ignored.push("health.url/enabled/interval");
        }
        if diff(&old.workers, &new.workers) {
            ignored.push("workers (use `warden scale` for the count)");
        }
        let mut new_level = None;
        if old.logging.level != new.logging.level {
            new_level = Some(new.logging.level);
            applied.push("logging.level");
        }
        if old.logging.timestamps != new.logging.timestamps || old.logging.worker_output != new.logging.worker_output {
            ignored.push("logging.timestamps/worker_output");
        }
        if diff(&old.metrics, &new.metrics) || diff(&old.control, &new.control) {
            ignored.push("metrics/control");
        }
        let (workers, logging, metrics, control) =
            (old.workers.clone(), old.logging.clone(), old.metrics.clone(), old.control.clone());
        let (url, enabled, interval) = (old.health.url.clone(), old.health.enabled, old.health.interval);
        self.cfg = new;
        self.cfg.workers = workers;
        self.cfg.logging = logging;
        self.cfg.metrics = metrics;
        self.cfg.control = control;
        if let Some(level) = new_level {
            self.cfg.logging.level = level;
            crate::logging::set_level(level);
        }
        self.cfg.health.url = url;
        self.cfg.health.enabled = enabled;
        self.cfg.health.interval = interval;
        self.policy = Policy::from(&self.cfg.restart);
        if schedule_changed {
            self.schedule_next();
        }
        if !applied.is_empty() {
            info!("config reloaded", applied = applied.join(","));
        }
        if !ignored.is_empty() {
            warn!("config changes that need `systemctl restart` were not applied", sections = ignored.join("; "));
        }
        Ok(())
    }

    /// Start the next batch: up to `surge` workers (1 for the canary), each
    /// next to the one it replaces, once the old workers still draining
    /// leave room for this batch's (`[reload] max_draining`). Done when the
    /// queue is empty and every drain has ended.
    pub(super) fn advance_rollout(&mut self) {
        let overlap = self.cfg.overlap();
        let max_draining = self.cfg.reload.max_draining;
        // The drains this rollout waits for (not one it gave up on, below).
        let draining_now = self.roll.as_ref().map_or(0, |r| r.draining.len());
        let worker_mode = self.is_worker_mode();
        let mut ids = Vec::new();
        {
            let Some(roll) = &mut self.roll else { return };
            if !matches!(roll.step, Step::Idle) || self.shutting_down || self.stopped {
                return;
            }
            let size = if roll.canary { 1 } else { roll.surge };
            while ids.len() < size {
                let Some(id) = roll.queue.pop_front() else { break };
                let Some(slot) = self.slots.get(&id).filter(|s| !s.removing) else { continue };
                let serving = slot.current.is_some() && matches!(slot.state, State::Running | State::Restarting);
                // Stop-then-start, or nothing serving: replaced on its own, as
                // there is no old worker to fall back on if another lane fails.
                let alone = !(serving && overlap);
                if alone && !ids.is_empty() {
                    roll.queue.push_front(id);
                    break;
                }
                ids.push(id);
                if alone {
                    break;
                }
            }
            if ids.is_empty() {
                self.conclude(true, None);
                return;
            }
            // Each worker of the batch drains once its replacement took over:
            // start it only while they fit next to the drains still running
            // (a batch larger than max_draining starts once none is left).
            if draining_now + ids.len() > max_draining.max(ids.len()) {
                for id in ids.into_iter().rev() {
                    roll.queue.push_front(id);
                }
                if !roll.held {
                    roll.held = true;
                    let pids: Vec<String> = roll
                        .draining
                        .iter()
                        .filter_map(|d| self.insts.get(&d.old))
                        .map(|i| i.handle.pid.to_string())
                        .collect();
                    let next = roll.queue.front().copied().unwrap_or(0);
                    info!(
                        "waiting for old workers to finish draining before replacing the next ones",
                        next_worker = if worker_mode { "host".to_string() } else { next.to_string() },
                        draining = draining_now,
                        max_draining = max_draining,
                        pids = pids.join(","),
                    );
                }
                return;
            }
            roll.held = false;
            // The batch exists before its first worker starts: a failure
            // while starting the others stops the ones already started.
            roll.step = Step::Batch(Vec::with_capacity(ids.len()));
            roll.batches += 1;
            if ids.len() > 1 {
                info!(
                    "starting new workers next to the old ones",
                    workers = id_list(&ids),
                    batch = roll.batches,
                    surge = roll.surge,
                );
            }
        }
        let deadline = crate::restart::later(Instant::now(), Duration::from_secs(self.cfg.reload.timeout));
        let Some(kind) = self.roll.as_ref().map(|r| r.kind) else { return };
        for slot_id in ids {
            let Some(slot) = self.slots.get_mut(&slot_id) else { continue };
            slot.token += 1; // cancel a pending restart timer; this rollout takes over
            let current = slot.current;
            let serving = current.filter(|_| matches!(slot.state, State::Running | State::Restarting));
            let at = match (serving, current) {
                // Each worker owns its port, or the app can't share it: stop,
                // then start (a gap for this worker only).
                (Some(old), _) if !overlap => {
                    self.stop_instance(old);
                    At::Vacating { old }
                }
                (Some(old), _) => match self.start_replacement(slot_id, kind) {
                    Ok(new) => At::Starting { new, old: Some(old), deadline },
                    Err(e) => {
                        self.fail_at(Some(slot_id), format!("could not start a new worker: {e}"));
                        return;
                    }
                },
                // Starting / stopping: let it finish exiting, then start fresh.
                (None, Some(old)) => {
                    self.stop_instance(old);
                    At::Vacating { old }
                }
                (None, None) => match self.spawn_current(slot_id) {
                    Some(new) => At::Starting { new, old: None, deadline },
                    None => {
                        self.fail_at(Some(slot_id), "could not start a new worker".into());
                        return;
                    }
                },
            };
            if let Some(Roll { step: Step::Batch(lanes), .. }) = &mut self.roll {
                lanes.push(Lane { slot: slot_id, at });
            }
        }
    }

    /// Called from `mark_ready`: the instance is listening.
    pub(super) fn rollout_on_ready(&mut self, inst: u64) {
        let can_check = self.can_check(inst);
        let rl = &self.cfg.reload;
        let (health_passes, verify, canary_soak, min_ready) =
            (rl.health_passes, rl.verify_command.is_some(), rl.canary_soak, rl.min_ready);
        let worker_mode = self.is_worker_mode();
        let label_of = |s: usize| if worker_mode { "host".to_string() } else { s.to_string() };
        let Some(roll) = &mut self.roll else { return };
        let canary = roll.canary && roll.kind == Kind::SafeReload;
        let Step::Batch(lanes) = &mut roll.step else { return };
        let several = lanes.len() > 1;
        let Some(lane) = lanes.iter_mut().find(|l| matches!(l.at, At::Starting { new, .. } if new == inst)) else {
            return;
        };
        let At::Starting { new, old, deadline } = lane.at else { return };
        let soak = Duration::from_secs(if canary { canary_soak } else { min_ready });
        let cmd = if verify { Cmd::Pending } else { Cmd::Skip };
        if canary {
            info!(
                "canary listening next to the old worker",
                worker = label_of(lane.slot),
                soak_s = soak.as_secs(),
                health_passes = if can_check { health_passes } else { 0 },
            );
        }
        // Nothing to wait for: it passed.
        if (health_passes == 0 || !can_check) && cmd == Cmd::Skip && soak.is_zero() {
            lane.at = At::Passed { new, old };
            if several {
                debug!("new worker passed its gates; waiting for the rest of its batch", worker = lane.slot);
            }
            self.promote_if_ready();
            return;
        }
        lane.at = At::Verifying(Verify {
            new,
            old,
            deadline,
            passes: 0,
            fails: 0,
            checking: false,
            next_check: None,
            socket_wait: None,
            cmd,
            soak,
            soak_until: None,
            canary,
        });
        // Its first health check (or verify_command, or soak) starts now.
        self.gate_steps(Some(new));
    }

    /// The shim reported a private health socket of `inst`.
    pub(super) fn rollout_socket_reported(&mut self, inst: u64) {
        self.gate_steps(Some(inst));
    }

    /// With a health path configured, health gates are mandatory: a worker
    /// that can't be checked fails them (never silently skipped).
    fn can_check(&self, _inst: u64) -> bool {
        self.cfg.ready_path().is_some()
    }

    pub(super) fn on_gate(&mut self, ev: GateEvent) {
        let seq_now = self.roll.as_ref().map(|r| r.seq);
        match ev {
            GateEvent::Tick { seq } if Some(seq) == seq_now => self.gate_tick(),
            GateEvent::Due { seq, inst } if Some(seq) == seq_now => self.gate_steps(Some(inst)),
            GateEvent::Check { seq, inst, result } if Some(seq) == seq_now => self.gate_check(inst, result),
            GateEvent::Command { seq, inst, result } if Some(seq) == seq_now => {
                let Some((slot, v)) = self.verifying(inst) else { return };
                match result {
                    Ok(()) => {
                        v.cmd = Cmd::Passed;
                        info!("verify_command passed", worker = slot);
                        self.gate_steps(Some(inst));
                    }
                    Err(e) => self.fail_at(Some(slot), format!("verify_command failed: {e}")),
                }
            }
            GateEvent::Preflight { seq, result } if Some(seq) == seq_now => match result {
                Ok(()) => {
                    info!("preflight passed");
                    if let Some(r) = &mut self.roll {
                        r.step = Step::Idle;
                    }
                    self.advance_rollout();
                }
                Err(e) => self.fail_rollout(format!("preflight command failed: {e}")),
            },
            GateEvent::PauseDone { seq } if Some(seq) == seq_now => {
                if let Some(r) = &mut self.roll {
                    if matches!(r.step, Step::Pausing) {
                        r.step = Step::Idle;
                    }
                }
                self.advance_rollout();
            }
            _ => {} // stale event from a finished rollout
        }
    }

    /// The lane verifying `inst`: its slot and gates.
    fn verifying(&mut self, inst: u64) -> Option<(usize, &mut Verify)> {
        let Some(Roll { step: Step::Batch(lanes), .. }) = &mut self.roll else { return None };
        lanes.iter_mut().find_map(|l| match &mut l.at {
            At::Verifying(v) if v.new == inst => Some((l.slot, v)),
            _ => None,
        })
    }

    /// Deadlines, and room freed by drains. The gates run on their own
    /// timers (`gate_steps`); the tick only runs any that is overdue.
    fn gate_tick(&mut self) {
        // Drains end with their process's exit (`rollout_on_exit`); this
        // catches room freed by an exit seen elsewhere, so a rollout waiting
        // for drains can't wait past them.
        self.after_drain();
        let now = Instant::now();
        let timeout = self.cfg.reload.timeout;
        let required = self.cfg.reload.health_passes;
        let Some(Roll { step: Step::Batch(lanes), .. }) = &self.roll else { return };
        let fail = lanes.iter().find_map(|lane| match &lane.at {
            At::Starting { deadline, .. } if now > *deadline => {
                Some((lane.slot, format!("new worker not listening within {timeout}s")))
            }
            At::Verifying(v) if now > v.deadline => {
                Some((lane.slot, format!("gates not passed within {timeout}s (health {}/{required})", v.passes)))
            }
            _ => None,
        });
        if let Some((slot, msg)) = fail {
            self.fail_at(Some(slot), msg);
            return;
        }
        self.gate_steps(None);
    }

    /// Run the next gate of each verifying lane (`only`: of that worker's)
    /// that is due: a health check `health_interval_ms` after the previous
    /// one finished (the first once it listens), then `verify_command`,
    /// then the soak; a lane past them all passes, and the batch is promoted
    /// as soon as every lane has. Called when a worker listens, when a
    /// check or command ends, by the lane's timers (`GateEvent::Due`), and
    /// by the tick.
    fn gate_steps(&mut self, only: Option<u64>) {
        let now = Instant::now();
        let required = self.cfg.reload.health_passes;
        let checks = required > 0 && self.can_check(0);
        let every = Duration::from_millis(self.cfg.reload.health_interval_ms);
        // Its private socket(s) not reported yet: a check now would fail.
        let expected = if self.cfg.private_sockets() { self.expected_listeners() } else { 0 };
        let insts = &self.insts;
        let unreported = |inst: u64| insts.get(&inst).is_some_and(|i| i.sockets.len() < expected);
        let Some(roll) = &mut self.roll else { return };
        let seq = roll.seq;
        let Step::Batch(lanes) = &mut roll.step else { return };
        let several = lanes.len() > 1;
        // Decide for every lane first, then act (acting needs `self`).
        let mut checks_due = Vec::new();
        let mut commands_due = Vec::new();
        let mut wakes = Vec::new();
        for lane in lanes.iter_mut() {
            let At::Verifying(v) = &mut lane.at else { continue };
            if only.is_some_and(|inst| inst != v.new) || v.checking || v.cmd == Cmd::Running {
                continue;
            }
            // Not yet: the timer `gate_check` set calls again when it is.
            let mut check_due = checks && v.next_check.is_none_or(|t| now >= t);
            if check_due && v.next_check.is_none() && unreported(v.new) {
                // The report, or this timer, calls again.
                let until = *v.socket_wait.get_or_insert_with(|| {
                    wakes.push((v.new, every));
                    crate::restart::later(now, every)
                });
                check_due = now >= until;
            }
            if checks && v.passes < required {
                if check_due {
                    v.checking = true;
                    checks_due.push(v.new);
                }
                continue;
            }
            if v.cmd == Cmd::Pending {
                v.cmd = Cmd::Running;
                commands_due.push((v.new, lane.slot));
                continue;
            }
            if !v.soak.is_zero() {
                let until = match v.soak_until {
                    Some(until) => until,
                    None => {
                        let until = crate::restart::later(now, v.soak);
                        v.soak_until = Some(until);
                        wakes.push((v.new, v.soak));
                        until
                    }
                };
                if now < until {
                    if check_due {
                        v.checking = true;
                        checks_due.push(v.new);
                    }
                    continue;
                }
            }
            let (new, old) = (v.new, v.old);
            lane.at = At::Passed { new, old };
            if several {
                debug!("new worker passed its gates; waiting for the rest of its batch", worker = lane.slot);
            }
        }
        for (inst, after) in wakes {
            self.send_later(after, Event::Gate(GateEvent::Due { seq, inst }));
        }
        for inst in checks_due {
            self.launch_check(seq, inst);
        }
        for (inst, slot) in commands_due {
            self.launch_command(seq, inst, slot);
        }
        self.promote_if_ready();
    }

    fn gate_check(&mut self, inst: u64, result: Result<(), String>) {
        let required = self.cfg.reload.health_passes;
        let threshold = self.cfg.health.failure_threshold;
        let every = Duration::from_millis(self.cfg.reload.health_interval_ms);
        let seq = self.roll.as_ref().map_or(0, |r| r.seq);
        let Some((slot, v)) = self.verifying(inst) else { return };
        v.checking = false;
        let soaking = v.soak_until.is_some();
        // The check run the moment it listened: when checks waited for a
        // tick, a worker had up to one interval more to warm up. Its failure
        // resets the passes but doesn't count toward failing the worker, so
        // a slow starter fails no sooner than it did then.
        let first = v.next_check.is_none();
        v.next_check = Some(crate::restart::later(Instant::now(), every));
        match result {
            Ok(()) => {
                v.fails = 0;
                if !soaking && v.passes < required {
                    v.passes += 1;
                    debug!("gate health check passed", worker = slot, passes = v.passes, required = required);
                    if v.passes == required {
                        info!("new worker passed health checks", worker = slot, passes = required);
                    }
                }
            }
            Err(e) => {
                if !first {
                    v.fails += 1;
                }
                if !soaking {
                    v.passes = 0;
                }
                debug!("gate health check failed", worker = slot, error = e, consecutive = v.fails);
                if soaking && v.fails >= threshold {
                    let what = if v.canary { "canary" } else { "new worker" };
                    self.fail_at(Some(slot), format!("{what} failed {threshold} health checks during soak: {e}"));
                    return;
                }
                if !soaking && v.fails >= threshold * 3 {
                    self.fail_at(Some(slot), format!("new worker keeps failing health checks: {e}"));
                    return;
                }
            }
        }
        // The next check is due one interval after this one finished; the
        // last pass, or the soak's end, moves the lane on now.
        self.send_later(every, Event::Gate(GateEvent::Due { seq, inst }));
        self.gate_steps(Some(inst));
    }

    fn launch_check(&self, seq: u64, inst: u64) {
        let Some(i) = self.insts.get(&inst) else { return };
        let sockets: Vec<PathBuf> = i.sockets.values().cloned().collect();
        let path = self.cfg.ready_path().unwrap_or_else(|| "/".into());
        let timeout = Duration::from_secs(self.cfg.health.timeout);
        let url = crate::health::parse_url(&self.cfg.health.url).ok();
        let expected = if self.cfg.private_sockets() { self.expected_listeners() } else { 0 };
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let check = async move {
                if expected > 0 && sockets.len() >= expected {
                    check_sockets(&sockets, &path, timeout).await
                } else if let Some(t) = url {
                    // No private socket (app without the shim): app-level check.
                    crate::health::check(&t, timeout).await.map(|_| ())
                } else {
                    Err(format!(
                        "only {} of {expected} private health socket(s) reported; the app must listen via Bun.serve (or node:http) with Warden's shim, or set [health] url",
                        sockets.len()
                    ))
                }
            };
            let result =
                crate::guard::catch_unwind(check).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
            let _ = tx.send(Event::Gate(GateEvent::Check { seq, inst, result }));
        });
    }

    fn launch_command(&self, seq: u64, inst: u64, slot: usize) {
        let Some(cmd) = self.cfg.reload.verify_command.clone() else { return };
        let Some(i) = self.insts.get(&inst) else { return };
        let mut env = vec![
            ("WARDEN_APP".to_string(), self.cfg.app.name.clone()),
            ("WARDEN_WORKER_ID".to_string(), self.label(slot)),
            ("WARDEN_WORKER_PID".to_string(), i.handle.pid.to_string()),
            (
                "WARDEN_WORKER_SOCKET".to_string(),
                i.sockets.values().next().map(|p| p.display().to_string()).unwrap_or_default(),
            ),
            (
                "WARDEN_WORKER_SOCKETS".to_string(),
                i.sockets.values().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" "),
            ),
        ];
        if let Some(p) = self.cfg.app.port {
            env.push(("PORT".into(), networking::worker_port(p, self.cfg.workers.port_strategy, slot).to_string()));
        }
        info!("running verify_command", worker = self.label(slot), command = cmd);
        let fut = run_shell(cmd, self.worker_dir(), env, Duration::from_secs(self.cfg.reload.timeout));
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let result = crate::guard::catch_unwind(fut).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
            let _ = tx.send(Event::Gate(GateEvent::Command { seq, inst, result }));
        });
    }

    /// Every new worker of the batch passed every gate: they take over, and
    /// the old ones drain together.
    fn promote_if_ready(&mut self) {
        let Some(Roll { step: Step::Batch(lanes), .. }) = &self.roll else { return };
        if lanes.is_empty() || !lanes.iter().all(|l| matches!(l.at, At::Passed { .. })) {
            return;
        }
        let passed: Vec<(usize, u64, Option<u64>)> = lanes
            .iter()
            .filter_map(|l| match l.at {
                At::Passed { new, old } => Some((l.slot, new, old)),
                _ => None,
            })
            .collect();
        // Worker mode: a Worker of a new host died after its checks.
        for (slot, new, _) in &passed {
            if let Some(t) = self.insts.get(new).and_then(|i| i.threads.iter().find(|(_, t)| t.crashed)) {
                let msg = format!("Worker {} of the new host crashed", t.0);
                self.fail_at(Some(*slot), msg);
                return;
            }
        }
        let Some(roll) = &mut self.roll else { return };
        let (kill_old, canary) = (roll.kill_old, roll.canary);
        roll.canary = false;
        let mut lanes = Vec::with_capacity(passed.len());
        let mut drains = Vec::new();
        for (slot_id, new, old) in passed {
            let old = old.filter(|o| self.insts.contains_key(o));
            if let Some(i) = self.insts.get_mut(&new) {
                i.role = Role::Current;
            }
            let new_pid = self.insts.get(&new).map(|i| i.handle.pid).unwrap_or(0);
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = Some(new);
                s.state = State::Running;
                s.token += 1;
                s.failed_at = None;
                if old.is_some() {
                    s.restarts += 1;
                }
            }
            let label = self.label(slot_id);
            match old {
                Some(old) => {
                    let old_pid = self.insts.get(&old).map(|i| i.handle.pid).unwrap_or(0);
                    // Its replacement listens and passed the gates: the old
                    // one stops taking connections and drains in the
                    // background while the rollout goes on.
                    let draining = self.draining_count() + 1;
                    if canary {
                        info!(
                            "canary passed; draining the worker it replaced",
                            worker = label,
                            new_pid = new_pid,
                            old_pid = old_pid,
                            draining = draining,
                        );
                    } else {
                        info!(
                            "worker replaced; draining old process",
                            worker = label,
                            new_pid = new_pid,
                            old_pid = old_pid,
                            draining = draining,
                        );
                    }
                    if let Some(o) = self.insts.get_mut(&old) {
                        o.role = Role::Retiring;
                    }
                    if kill_old {
                        self.kill_instance(old)
                    } else {
                        self.stop_instance(old)
                    }
                    drains.push(Drain { slot: slot_id, old, since: Instant::now() });
                }
                None => info!("worker passed its gates", worker = label, pid = new_pid),
            }
            lanes.push(Lane { slot: slot_id, at: At::Done });
        }
        if let Some(r) = &mut self.roll {
            r.done += lanes.len();
            r.step = Step::Batch(lanes);
            r.draining.extend(drains);
        }
        self.check_all_ready();
        self.batch_progress();
    }

    /// Old workers draining now: replaced by a rollout, their stop signal sent.
    pub(super) fn draining_count(&self) -> usize {
        self.insts.values().filter(|i| i.role == Role::Retiring).count()
    }

    /// Nothing left to replace (`ok`), or the rollout failed (its batch rolled
    /// back and `message` logged): it ends now, or once the old workers it
    /// stopped have finished draining (the CLI returns then).
    fn conclude(&mut self, ok: bool, message: Option<String>) {
        let Some(roll) = &mut self.roll else { return };
        roll.draining.retain(|d| self.insts.contains_key(&d.old));
        if roll.draining.is_empty() {
            self.end_rollout(ok, message, !ok);
            return;
        }
        let pids: Vec<String> =
            roll.draining.iter().filter_map(|d| self.insts.get(&d.old)).map(|i| i.handle.pid.to_string()).collect();
        let workers: Vec<usize> = roll.draining.iter().map(|d| d.slot).collect();
        // One worker's rollout: its `draining old process` line said it all.
        if ok && roll.total > 1 {
            info!(
                "every worker replaced; waiting for the old ones to finish draining",
                workers = id_list(&workers),
                pids = pids.join(","),
                grace_s = self.cfg.shutdown.grace_period,
            );
        } else {
            debug!(
                "rollout ends once its old workers finish draining",
                workers = id_list(&workers),
                pids = pids.join(","),
                ok = ok,
            );
        }
        if let Some(r) = &mut self.roll {
            r.held = false;
            r.step = Step::Ending { ok, message };
        }
    }

    /// A drain ended, or the ticker found one gone: the batch waiting for
    /// room may start, or a rollout waiting for its last drains ends.
    fn after_drain(&mut self) {
        let limit = self.cfg.grace_period().saturating_add(DRAIN_SLACK);
        let Some(roll) = &mut self.roll else { return };
        roll.draining.retain(|d| self.insts.contains_key(&d.old));
        // SIGKILLed at grace_period and still not gone: held by the kernel.
        let (stuck, waiting): (Vec<Drain>, Vec<Drain>) =
            std::mem::take(&mut roll.draining).into_iter().partition(|d| d.since.elapsed() > limit);
        roll.draining = waiting;
        for d in stuck {
            let pid = self.insts.get(&d.old).map(|i| i.handle.pid).unwrap_or(0);
            warn!(
                "old worker did not exit after its SIGKILL; the rollout stops waiting for it",
                worker = self.label(d.slot),
                pid = pid,
                waited_s = d.since.elapsed().as_secs(),
                hint = format!(
                    "the kernel holds it (uninterruptible I/O, a hung network filesystem): `cat /proc/{pid}/stack` \
                     shows where. Its replacement serves; Warden reaps it when it ends, and `warden status` lists \
                     it as DRAINING until then"
                ),
            );
        }
        let Some(roll) = &mut self.roll else { return };
        match &mut roll.step {
            Step::Idle => self.advance_rollout(),
            Step::Ending { ok, message } if roll.draining.is_empty() => {
                let (ok, message) = (*ok, message.take());
                // A failure was logged when it happened.
                self.end_rollout(ok, message, !ok);
            }
            _ => {}
        }
    }

    /// Every lane of the batch is done: pause (safe-reload), or the next batch.
    fn batch_progress(&mut self) {
        let pause = self.cfg.reload.pause;
        let Some(roll) = &mut self.roll else { return };
        let Step::Batch(lanes) = &roll.step else { return };
        if !lanes.iter().all(|l| matches!(l.at, At::Done)) {
            return;
        }
        roll.step = Step::Idle;
        if roll.kind == Kind::SafeReload && pause > 0 && !roll.queue.is_empty() {
            roll.step = Step::Pausing;
            let seq = roll.seq;
            self.send_later(Duration::from_secs(pause), Event::Gate(GateEvent::PauseDone { seq }));
            return;
        }
        self.advance_rollout();
    }

    /// Called from `on_exit` for every exited instance.
    pub(super) fn rollout_on_exit(&mut self, inst: u64, reason: &str) {
        let deadline = crate::restart::later(Instant::now(), Duration::from_secs(self.cfg.reload.timeout));
        // An old worker drained (or died, or was killed at grace_period).
        if let Some(roll) = &mut self.roll {
            if let Some(pos) = roll.draining.iter().position(|d| d.old == inst) {
                roll.draining.remove(pos);
                self.after_drain();
                return;
            }
        }
        let Some(Roll { step: Step::Batch(lanes), .. }) = &mut self.roll else { return };
        let Some(lane) = lanes.iter_mut().find(|l| l.at.involves(inst)) else { return };
        let slot = lane.slot;
        let act = match &mut lane.at {
            At::Starting { new, .. } if *new == inst => {
                OnExit::Fail(slot, format!("new worker exited before listening: {reason}"))
            }
            At::Verifying(v) if v.new == inst => {
                let what = if v.canary { "canary" } else { "new worker" };
                OnExit::Fail(slot, format!("{what} exited during its checks: {reason}"))
            }
            At::Passed { new, .. } if *new == inst => {
                OnExit::Fail(slot, format!("new worker exited while the rest of its batch was checked: {reason}"))
            }
            // The old worker died first: the new one owns the slot now (see `on_slot_crash`).
            At::Starting { old, .. } | At::Passed { old, .. } => {
                *old = None;
                return;
            }
            At::Verifying(v) => {
                v.old = None;
                return;
            }
            At::Vacating { .. } => OnExit::Spawn(slot),
            At::Done => return,
        };
        match act {
            OnExit::Fail(slot, msg) => self.fail_at(Some(slot), msg),
            OnExit::Spawn(slot) => {
                let at = self.spawn_current(slot).map(|new| At::Starting { new, old: None, deadline });
                match at {
                    Some(at) => {
                        if let Some(Roll { step: Step::Batch(lanes), .. }) = &mut self.roll {
                            if let Some(l) = lanes.iter_mut().find(|l| l.slot == slot) {
                                l.at = at;
                            }
                        }
                    }
                    None => self.fail_at(Some(slot), "could not start a new worker".into()),
                }
            }
        }
    }

    /// The replacement (not yet promoted) this rollout is starting or verifying for `slot_id`.
    pub(super) fn rollout_new_instance(&self, slot_id: usize) -> Option<u64> {
        let Some(Roll { step: Step::Batch(lanes), .. }) = &self.roll else { return None };
        let new = lanes.iter().find(|l| l.slot == slot_id)?.at.new_inst()?;
        self.insts.get(&new).filter(|i| i.role == Role::Replacement).map(|_| new)
    }

    /// A rollout is currently working on this slot.
    pub(super) fn rollout_replacing(&self, slot_id: usize) -> bool {
        match &self.roll {
            Some(Roll { step: Step::Batch(lanes), .. }) => {
                lanes.iter().any(|l| l.slot == slot_id && !matches!(l.at, At::Done))
            }
            _ => false,
        }
    }

    /// Stop the rollout (see `fail_at`), at the batch's first worker.
    pub(super) fn fail_rollout(&mut self, reason: String) {
        let at = match &self.roll {
            Some(Roll { step: Step::Batch(lanes), .. }) => lanes.iter().find(|l| l.at.new_inst().is_some()),
            _ => None,
        }
        .map(|l| l.slot);
        self.fail_at(at, reason);
    }

    /// Stop the rollout because of worker `at`. Every new worker of the batch
    /// that has not taken over is stopped. While the old worker it replaces
    /// still exists this is a free rollback: nothing else changes. If the new
    /// process already owns its slot (the old one died, the slot was empty,
    /// or offset ports), it is stopped and the slot restarted. The config and
    /// release pin in effect before the rollout are restored. Old workers of
    /// earlier batches, already replaced and draining, finish their drain:
    /// the failure is logged now, and the rollout ends once they are gone.
    fn fail_at(&mut self, at: Option<usize>, reason: String) {
        let Some(roll) = &mut self.roll else { return };
        if matches!(roll.step, Step::Ending { .. }) {
            return; // already over: nothing left to roll back
        }
        // (slot, new, old) of every worker of the batch that has not taken over.
        let pending: Vec<(usize, u64, Option<u64>)> = match &roll.step {
            Step::Batch(lanes) => lanes
                .iter()
                .filter_map(|l| match &l.at {
                    At::Starting { new, old, .. } | At::Passed { new, old } => Some((l.slot, *new, *old)),
                    At::Verifying(v) => Some((l.slot, v.new, v.old)),
                    At::Vacating { .. } | At::Done => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        let (kind, done, total, kill_old) = (roll.kind, roll.done, roll.total, roll.kill_old);
        let prev = roll.prev.take();
        let restored = prev.is_some();
        if let Some(p) = prev {
            self.restore(p);
        }
        let (mut rolled_back, mut restarting) = (0usize, 0usize);
        let mut kept = Vec::new();
        for &(slot, new, old) in &pending {
            let old = old.filter(|o| self.insts.contains_key(o));
            if self.insts.contains_key(&new) {
                if old.is_some() {
                    rolled_back += 1;
                } else if let Some(i) = self.insts.get_mut(&new) {
                    // It owns the slot: stop it (draining) and restart the slot afterwards.
                    i.restart_on_exit = true;
                    restarting += 1;
                }
                self.stop_instance(new);
            } else if old.is_some() {
                rolled_back += 1;
            }
            if let Some(o) = old {
                kept.push((slot, o));
            }
        }
        let at = at.or(pending.first().map(|p| p.0));
        let who = at.map(|s| format!("worker {}", self.label(s))).unwrap_or_else(|| "preflight".into());
        let cfg_note = if restored && kind.is_deploy() { " The previous config is back in effect." } else { "" };
        let batch = pending.len();
        let message = match kind {
            Kind::SafeReload | Kind::Reload if done == 0 && rolled_back > 0 && restarting == 0 && batch > 1 => format!(
                "{} failed at {who}: {reason}. Rolled back: the {batch} new workers started together were stopped; every worker still runs the previous version.{cfg_note}",
                kind.name()
            ),
            Kind::SafeReload | Kind::Reload if done == 0 && rolled_back > 0 && restarting == 0 => format!(
                "{} failed at {who}: {reason}. Rolled back: every worker still runs the previous version.{cfg_note}",
                kind.name()
            ),
            Kind::SafeReload | Kind::Reload if done == 0 && restarting > 0 && batch > 1 => format!(
                "{} failed at {who}: {reason}. The new workers started together were stopped; {restarting} of them could not be rolled back (the old process was already gone) and are being restarted, the others still run the previous version.{cfg_note}",
                kind.name()
            ),
            Kind::SafeReload | Kind::Reload if done == 0 && restarting > 0 => format!(
                "{} failed at {who}: {reason}. That worker could not be rolled back (its old process was already gone) and is being restarted; no other worker was touched.{cfg_note}",
                kind.name()
            ),
            Kind::SafeReload | Kind::Reload if done == 0 => {
                format!("{} failed at {who}: {reason}. No worker was replaced.{cfg_note}", kind.name())
            }
            Kind::SafeReload | Kind::Reload => format!(
                "{} halted at {who}: {reason}. {done}/{total} workers were replaced and run the new version; the rest still run the previous one.{cfg_note}",
                kind.name()
            ),
            _ => format!(
                "{} of {who} failed: {reason}{}",
                kind.name(),
                match rolled_back {
                    0 => "",
                    1 => "; the old worker keeps serving",
                    _ => "; the old workers keep serving",
                }
            ),
        };
        // The phase it failed in, if not published yet.
        self.publish_rollout();
        self.log_outcome(false, &message);
        self.conclude(false, Some(message));

        for (slot, o) in kept {
            if kill_old {
                // It was being replaced because it is hung: kill it anyway;
                // crash handling restarts it.
                if let Some(i) = self.insts.get_mut(&o) {
                    i.stopping = false;
                    i.handle.signal(libc::SIGKILL);
                }
            } else if self.insts.get(&o).is_some_and(|i| i.threads.values().any(|t| t.crashed)) {
                // Worker mode: the host we kept is degraded; recover it (with backoff).
                self.on_slot_crash(slot, Duration::ZERO);
            }
        }
    }

    /// A stop, `restart --hard` or shutdown takes over: the rollout ends now
    /// (the old workers still draining are stopped with everything else).
    pub(super) fn abort_rollout(&mut self, reason: &str) {
        // Every worker was replaced already, or its failure was reported:
        // that stays the outcome.
        if let Some(Roll { step: Step::Ending { ok, message }, draining, .. }) = &mut self.roll {
            let (ok, message, left) = (*ok, message.take(), draining.len());
            // Old workers still draining are stopped with everything else
            // (they were signalled already; they leave within their grace
            // period): the outcome must not read as if none were left.
            let message = match (message, left) {
                (Some(m), n) if n > 0 => Some(format!("{m}; stopped while {n} old worker(s) were still draining")),
                (None, n) if n > 0 => Some(format!(
                    "every worker replaced; stopped ({reason}) while {n} old worker(s) were still draining"
                )),
                (m, _) => m,
            };
            self.end_rollout(ok, message, !ok);
        } else if self.roll.is_some() {
            self.end_rollout(false, Some(format!("aborted: {reason}")), false);
        }
    }

    /// The log line of a rollout's outcome: what happened, and for a
    /// failure what Warden did about it and what the operator does next.
    fn log_outcome(&self, ok: bool, message: &str) {
        if ok {
            info!(message)
        } else {
            let hint = if message.starts_with("aborted:") {
                "a stop, `restart --hard` or shutdown took over during the rollout; run it again once the workers \
                 are back, if it is still needed"
            } else {
                "the reason names the gate that failed; the new workers' output is in `warden logs <app>` and the \
                 last rollout in `warden describe <app>`. Fix the release or config, then run it again"
            };
            error!(message, hint = hint)
        }
    }

    /// The rollout is over: record and announce its outcome (`logged`: its
    /// log line was written when it failed, while drains still ran).
    fn end_rollout(&mut self, ok: bool, message: Option<String>, logged: bool) {
        // The phase it failed in, if not published yet (it cannot be after
        // `rollout_done`). Idle: all done, or failed before a step began.
        // Ending: published as it began and as its drains ended; its last
        // drain gone, `rollout_done` says the rest.
        if self.roll.as_ref().is_some_and(|r| !matches!(r.step, Step::Idle | Step::Ending { .. })) {
            self.publish_rollout();
        }
        let Some(roll) = self.roll.take() else { return };
        let secs = roll.started.elapsed().as_secs_f64();
        let batches = match roll.batches {
            _ if roll.surge <= 1 => String::new(),
            1 => format!(" (1 batch of up to {})", roll.surge),
            n => format!(" ({n} batches of up to {})", roll.surge),
        };
        let message = message.unwrap_or_else(|| match roll.kind {
            Kind::Reload | Kind::SafeReload => {
                format!("{} complete: {} worker(s) replaced in {secs:.1}s{batches}", roll.kind.name(), roll.done)
            }
            Kind::Restart => format!("restart complete in {secs:.1}s{batches}"),
            Kind::Replace | Kind::Recovery => format!("worker replaced ({}) in {secs:.1}s", roll.reason),
        });
        if !logged {
            self.log_outcome(ok, &message);
        }
        if roll.kind.is_deploy() || roll.kind == Kind::Restart {
            systemd::notify(if ok {
                "READY=1\nSTATUS=all workers ready"
            } else {
                "READY=1\nSTATUS=last rollout failed"
            });
        }
        let outcome = RolloutOutcome {
            seq: roll.seq,
            kind: roll.kind.name().into(),
            ok,
            message,
            duration_secs: (secs * 10.0).round() / 10.0,
        };
        if crate::events::active() {
            crate::events::emit(crate::events::Event::RolloutDone {
                app: self.cfg.app.name.clone(),
                outcome: outcome.clone(),
            });
        }
        self.last_rollout = Some(outcome);
        self.standby_after_rollout(roll.kind, ok, roll.total);
    }

    /// What `rollout_status().phase` says, without its countdowns (soak time
    /// left): when this changes, a `rollout` event is published.
    pub(super) fn rollout_phase(&self) -> Option<PhaseKey> {
        let r = self.roll.as_ref()?;
        let (step, inst, detail) = match &r.step {
            Step::Idle => (0, 0, u64::from(r.held)),
            Step::Preflight => (1, 0, 0),
            Step::Pausing => (2, 0, 0),
            Step::Batch(lanes) => batch_key(lanes),
            Step::Ending { ok, .. } => (8, 0, u64::from(*ok)),
        };
        Some(PhaseKey { seq: r.seq, done: r.done, step, inst, detail, draining: r.draining.len() })
    }

    pub(super) fn rollout_status(&self) -> Option<RolloutStatus> {
        let r = self.roll.as_ref()?;
        let drains = self.drains_text(r);
        let phase = match &r.step {
            Step::Idle if r.held => format!(
                "worker {} waits for room to drain: {drains} (max_draining = {})",
                r.queue.front().map(|s| self.label(*s)).unwrap_or_default(),
                self.cfg.reload.max_draining
            ),
            // Only seen at the start (the `rollout` event announcing it).
            Step::Idle if r.done == 0 => "starting".to_string(),
            Step::Idle => "next worker".to_string(),
            Step::Preflight => "running preflight".to_string(),
            Step::Pausing => format!("pausing {}s between workers", self.cfg.reload.pause),
            Step::Batch(lanes) => {
                let batch = if lanes.len() == 1 { self.lane_phase(&lanes[0]) } else { self.batch_phase(lanes) };
                if r.draining.is_empty() { batch } else { format!("{batch}; {drains}") }
            }
            Step::Ending { ok: true, .. } => format!("every worker replaced; {drains}"),
            Step::Ending { ok: false, .. } => format!("failed; {drains}"),
        };
        Some(RolloutStatus {
            seq: r.seq,
            kind: r.kind.name().into(),
            phase,
            done: r.done,
            total: r.total,
            elapsed_secs: r.started.elapsed().as_secs(),
        })
    }

    fn pid_of(&self, inst: u64) -> u32 {
        self.insts.get(&inst).map(|x| x.handle.pid).unwrap_or(0)
    }

    /// `2 old workers draining (pid 101, 102)`.
    fn drains_text(&self, r: &Roll) -> String {
        let pids: Vec<String> = r.draining.iter().map(|d| self.pid_of(d.old).to_string()).collect();
        match pids.len() {
            0 => "no old worker left draining".to_string(),
            1 => format!("1 old worker draining (pid {})", pids[0]),
            n => format!("{n} old workers draining (pid {})", pids.join(", ")),
        }
    }

    /// One worker: `worker 2: health checks 1/3`.
    fn lane_phase(&self, l: &Lane) -> String {
        let label = self.label(l.slot);
        match &l.at {
            At::Starting { new, .. } => format!("worker {label}: starting new process (pid {})", self.pid_of(*new)),
            At::Vacating { old } => {
                format!(
                    "worker {label}: draining old process (pid {}) before starting its replacement",
                    self.pid_of(*old)
                )
            }
            At::Passed { .. } => format!("worker {label}: passed its gates"),
            At::Done => format!("worker {label}: done"),
            At::Verifying(v) => {
                let who = if v.canary { format!("worker {label} (canary)") } else { format!("worker {label}") };
                format!("{who}: {}", self.verify_phase(v))
            }
        }
    }

    fn verify_phase(&self, v: &Verify) -> String {
        if let Some(until) = v.soak_until {
            format!("soaking, {}s left", until.saturating_duration_since(Instant::now()).as_secs())
        } else if v.cmd == Cmd::Running {
            "running verify_command".into()
        } else {
            format!("health checks {}/{}", v.passes, self.cfg.reload.health_passes)
        }
    }

    /// Several workers: where the batch stands, from its slowest worker.
    /// `workers 1, 2: health checks 1/3, 1/2 passed`.
    fn batch_phase(&self, lanes: &[Lane]) -> String {
        let n = lanes.len();
        let ids: Vec<usize> = lanes.iter().map(|l| l.slot).collect();
        let who = format!("workers {}", id_list(&ids));
        let starting = lanes.iter().filter(|l| matches!(l.at, At::Starting { .. })).count();
        let passed = lanes.iter().filter(|l| matches!(l.at, At::Passed { .. })).count();
        let slowest = lanes
            .iter()
            .filter_map(|l| if let At::Verifying(v) = &l.at { Some(v) } else { None })
            .min_by_key(|v| (v.stage(), u64::from(v.passes), std::cmp::Reverse(v.soak_until)));
        let passed_note = if passed > 0 { format!(", {passed}/{n} passed") } else { String::new() };
        if starting > 0 {
            format!("{who}: starting {n} new processes ({}/{n} listening)", n - starting)
        } else if let Some(v) = slowest {
            format!("{who}: {}{passed_note}", self.verify_phase(v))
        } else if passed == n {
            format!("{who}: all {n} passed their gates")
        } else {
            format!("{who}: done")
        }
    }
}

/// Phase key of a batch (see `PhaseKey`): which stage its slowest worker is
/// in, and how many workers got how far.
fn batch_key(lanes: &[Lane]) -> (u8, u64, u64) {
    let first = lanes.iter().find_map(|l| l.at.new_inst()).unwrap_or(0);
    let count = |f: fn(&At) -> bool| lanes.iter().filter(|l| f(&l.at)).count() as u64;
    let starting = count(|a| matches!(a, At::Starting { .. }));
    let passed = count(|a| matches!(a, At::Passed { .. }));
    let vacating = count(|a| matches!(a, At::Vacating { .. }));
    let slowest = lanes
        .iter()
        .filter_map(|l| if let At::Verifying(v) = &l.at { Some(v) } else { None })
        .min_by_key(|v| (v.stage(), u64::from(v.passes)));
    if lanes.len() == 1 {
        return match &lanes[0].at {
            At::Starting { new, .. } => (3, *new, 0),
            At::Vacating { old } => (4, *old, 0),
            At::Verifying(v) => {
                let detail = match v.stage() {
                    2 => u64::from(u32::MAX),
                    1 if v.cmd == Cmd::Running => u64::from(u32::MAX - 1),
                    _ => u64::from(v.passes),
                };
                (5, v.new, detail)
            }
            At::Passed { new, .. } => (6, *new, 0),
            At::Done => (7, 0, 0),
        };
    }
    if starting > 0 {
        (3, first, starting)
    } else if let Some(v) = slowest {
        (5, first, (v.stage() << 48) | (u64::from(v.passes) << 24) | passed)
    } else if vacating > 0 {
        (4, first, vacating)
    } else {
        (6, first, passed)
    }
}

/// `1, 2` or `1-4, 7`.
fn id_list(ids: &[usize]) -> String {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < ids.len() {
        let mut j = i;
        while j + 1 < ids.len() && ids[j + 1] == ids[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}-{}", ids[i], ids[j]));
        } else {
            parts.extend(ids[i..=j].iter().map(|x| x.to_string()));
        }
        i = j + 1;
    }
    parts.join(", ")
}

async fn check_sockets(sockets: &[PathBuf], path: &str, timeout: Duration) -> Result<(), String> {
    for s in sockets {
        crate::health::check_unix(s, path, timeout).await?;
    }
    Ok(())
}

pub(super) async fn check_instance_sockets(
    sockets: Vec<PathBuf>,
    path: String,
    timeout: Duration,
) -> Result<(), String> {
    check_sockets(&sockets, &path, timeout).await
}

fn command_exists(cmd: &str, env: &std::collections::BTreeMap<String, String>) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    let path = env.get("PATH").cloned().or_else(|| std::env::var("PATH").ok()).unwrap_or_default();
    path.split(':').filter(|d| !d.is_empty()).any(|d| Path::new(d).join(cmd).is_file())
}

/// Run `sh -c <cmd>` with a timeout; `Err` carries the last output line.
pub(super) async fn run_shell(
    cmd: String,
    cwd: Option<PathBuf>,
    env: Vec<(String, String)>,
    timeout: Duration,
) -> Result<(), String> {
    {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(&cmd).stdin(std::process::Stdio::null()).kill_on_drop(true);
        if let Some(d) = &cwd {
            c.current_dir(d);
        }
        for (k, v) in &env {
            c.env(k, v);
        }
        match tokio::time::timeout(timeout, c.output()).await {
            Err(_) => Err(format!("timed out after {}s", timeout.as_secs())),
            Ok(Err(e)) => Err(format!("cannot run: {e}")),
            Ok(Ok(out)) if out.status.success() => Ok(()),
            Ok(Ok(out)) => {
                let text = [&out.stderr, &out.stdout]
                    .iter()
                    .flat_map(|b| String::from_utf8_lossy(b).lines().map(str::to_string).collect::<Vec<_>>())
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or_default();
                let code = out.status.code().map(|c| format!("exit {c}")).unwrap_or_else(|| "killed".into());
                let text: String = text.chars().take(200).collect();
                Err(if text.is_empty() { code } else { format!("{code}: {text}") })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn shell_runner() {
        assert_eq!(run_shell("true".into(), None, vec![], Duration::from_secs(5)).await, Ok(()));
        assert_eq!(
            run_shell("echo nope >&2; exit 3".into(), None, vec![], Duration::from_secs(5)).await,
            Err("exit 3: nope".into())
        );
        assert_eq!(
            run_shell("test \"$X\" = y".into(), None, vec![("X".into(), "y".into())], Duration::from_secs(5)).await,
            Ok(())
        );
        assert!(
            run_shell("sleep 5".into(), None, vec![], Duration::from_millis(100))
                .await
                .unwrap_err()
                .contains("timed out")
        );
    }

    #[test]
    fn finds_commands() {
        assert!(command_exists("sh", &Default::default()));
        assert!(!command_exists("definitely-not-a-command-xyz", &Default::default()));
        assert!(command_exists("/bin/sh", &Default::default()));
    }

    use super::super::rig::{Rig, local};
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    /// A worker that listens at once (shim protocol on fd 3) and, asked to
    /// stop, drains for FAKE_DRAIN seconds (a WebSocket's long_lived_timeout)
    /// before exiting 0. A worker started with FAKE_FAIL_ID = its id exits 5
    /// before listening (a bad release reaching that worker).
    const DRAINER: &str = r#"
trap 'sleep "${FAKE_DRAIN:-0}"; exit 0' TERM
if [ -n "$FAKE_FAIL_ID" ] && [ "$WARDEN_WORKER_ID" = "$FAKE_FAIL_ID" ]; then exit 5; fi
echo '{"ev":"listening","port":1}' >&3
sleep 60 &
wait $!
"#;

    /// `count` workers that overlap, each draining for `drain` s when stopped.
    async fn drainers(name: &str, count: usize, drain: &str, reload: &str) -> Rig {
        let mut r = Rig::new(
            name,
            &format!("[workers]\ncount = {count}\noverlap = true\n[reload]\nhealth_passes = 0\n{reload}"),
            DRAINER,
        );
        r.sup.cfg.app.env.insert("FAKE_DRAIN".into(), drain.into());
        r.sup.start_all();
        r.until("every worker running", |s| s.slots.values().all(|x| x.state == State::Running)).await;
        r
    }

    fn currents(s: &Supervisor) -> Vec<u64> {
        s.slots.values().filter_map(|x| x.current).collect()
    }

    /// Runs a rolling restart of every worker to its end; returns how long
    /// it took and the most old workers seen draining at once.
    async fn restart_all(r: &mut Rig) -> (Duration, usize) {
        let before = currents(&r.sup);
        let ids: Vec<usize> = r.sup.slots.keys().copied().collect();
        let t0 = Instant::now();
        r.sup.begin_rollout(Kind::Restart, ids, String::new(), false).unwrap();
        let most = Cell::new(r.sup.draining_count());
        r.until("the restart ends", |s| {
            most.set(most.get().max(s.draining_count()));
            s.roll.is_none()
        })
        .await;
        let took = t0.elapsed();
        assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true), "{:?}", r.sup.last_rollout);
        assert_eq!(r.sup.draining_count(), 0, "the rollout ends once every old worker has exited");
        assert!(before.iter().all(|old| !r.sup.insts.contains_key(old)), "every old worker is gone");
        assert!(r.sup.slots.values().all(|x| x.state == State::Running && x.current.is_some()));
        (took, most.get())
    }

    /// The next worker is replaced while the old ones drain, up to
    /// max_draining at once: 4 workers with 2 s drains take about 2 s, not 8.
    #[tokio::test(flavor = "current_thread")]
    async fn drains_overlap_up_to_max_draining() {
        local(async {
            let mut r = drainers("overlap", 4, "2", "").await;
            assert_eq!(r.sup.cfg.reload.max_draining, 4, "the default");
            let (took, most) = restart_all(&mut r).await;
            // The 4 replacements (a shell each) start within the first drain's 2 s.
            assert_eq!(most, 4, "every drain overlapped");
            assert!(took < Duration::from_secs(4), "4 overlapping 2 s drains took {took:?} (one after the other: 8 s)");
            r.shutdown().await;

            let mut r = drainers("overlap2", 4, "1", "max_draining = 2\n").await;
            let phases = RefCell::new(Vec::<String>::new());
            let ids: Vec<usize> = r.sup.slots.keys().copied().collect();
            r.sup.begin_rollout(Kind::Restart, ids, String::new(), false).unwrap();
            let most = Cell::new(0);
            r.until("the restart ends", |s| {
                most.set(most.get().max(s.draining_count()));
                if let Some(p) = s.rollout_status().map(|x| x.phase) {
                    if phases.borrow().last() != Some(&p) {
                        phases.borrow_mut().push(p);
                    }
                }
                s.roll.is_none()
            })
            .await;
            let phases = phases.into_inner();
            assert_eq!(most.get(), 2, "{phases:#?}");
            assert!(phases.iter().any(|p| p.contains("waits for room to drain") && p.contains("max_draining = 2")));
            assert!(phases.iter().any(|p| p.contains("; 1 old worker draining (pid ")), "{phases:#?}");
            assert!(phases.iter().any(|p| p.starts_with("every worker replaced; ")), "{phases:#?}");
            assert!(!phases.iter().any(|p| p.contains("no old worker") || p.contains("(pid )")), "{phases:#?}");
            r.shutdown().await;
        })
        .await;
    }

    /// `max_draining = 1` is the behaviour before drains overlapped: each old
    /// worker exits before the next replacement starts.
    #[tokio::test(flavor = "current_thread")]
    async fn max_draining_1_drains_one_at_a_time() {
        local(async {
            let mut r = drainers("serial", 3, "0.4", "max_draining = 1\n").await;
            let (took, most) = restart_all(&mut r).await;
            assert_eq!(most, 1);
            assert!(took >= Duration::from_millis(1200), "3 drains of 0.4 s one after the other: {took:?}");
            r.shutdown().await;
        })
        .await;
    }

    /// While old workers drain, `status` lists them (`draining`), never as
    /// workers, and the rollout phase says so.
    #[tokio::test(flavor = "current_thread")]
    async fn status_shows_the_draining_old_workers() {
        local(async {
            let mut r = drainers("rows", 2, "1", "").await;
            let old: Vec<u64> = currents(&r.sup);
            let old_pids: Vec<u32> = old.iter().map(|i| r.sup.insts[i].handle.pid).collect();
            r.sup.begin_rollout(Kind::Restart, vec![1, 2], String::new(), false).unwrap();
            r.until("both old workers draining", |s| s.draining_count() == 2).await;
            let st = r.sup.status();
            assert_eq!(st.workers.len(), 2);
            assert!(st.workers.iter().all(|w| w.state == "RUNNING" && !old_pids.contains(&w.pid.unwrap_or(0))));
            let rows: Vec<(usize, &str)> = st.draining.iter().map(|w| (w.id, w.state.as_str())).collect();
            assert_eq!(rows, [(1, control::DRAINING), (2, control::DRAINING)]);
            assert_eq!(st.draining.iter().filter_map(|w| w.pid).collect::<Vec<_>>(), old_pids);
            let phase = st.rollout.map(|x| x.phase).unwrap_or_default();
            assert!(phase.starts_with("every worker replaced; 2 old workers draining (pid "), "{phase}");
            r.until("the restart ends", |s| s.roll.is_none()).await;
            assert!(r.sup.status().draining.is_empty());
            r.shutdown().await;
        })
        .await;
    }

    /// A batch that fails while earlier old workers still drain: the batch
    /// rolls back at once (its old worker keeps serving), the drains finish,
    /// and only then the rollout ends, reporting what was replaced.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failure_rolls_back_its_batch_and_waits_for_earlier_drains() {
        local(async {
            let mut r = drainers("rollback", 3, "1", "").await;
            let old: Vec<u64> = currents(&r.sup);
            // Every new worker 2 fails; the old one was started before.
            r.sup.cfg.app.env.insert("FAKE_FAIL_ID".into(), "2".into());
            r.sup.begin_rollout(Kind::Reload, vec![1, 2, 3], String::new(), false).unwrap();
            let ending = Cell::new(false);
            r.until("the reload ends", |s| {
                if matches!(s.roll, Some(Roll { step: Step::Ending { ok: false, .. }, .. })) {
                    ending.set(true);
                    // Rolled back already: worker 2's old process serves, nothing new runs for it.
                    assert_eq!(s.slots[&2].current, Some(old[1]));
                    assert_eq!(s.draining_count(), 1, "worker 1's old process still drains");
                }
                s.roll.is_none()
            })
            .await;
            assert!(ending.get(), "the failed reload waited for the drain it had started");
            let o = r.sup.last_rollout.clone().unwrap();
            assert!(!o.ok);
            assert!(
                o.message.contains("halted at worker 2") && o.message.contains("1/3 workers were replaced"),
                "{o:?}"
            );
            assert_eq!(r.sup.draining_count(), 0);
            assert!(!r.sup.insts.contains_key(&old[0]), "worker 1's old process drained and exited");
            assert_ne!(r.sup.slots[&1].current, Some(old[0]), "worker 1 runs the new version");
            assert_eq!((r.sup.slots[&2].current, r.sup.slots[&3].current), (Some(old[1]), Some(old[2])));
            r.shutdown().await;
        })
        .await;
    }

    /// An old worker still there long after its grace-period SIGKILL (the
    /// kernel holds it) no longer holds the rollout: it ends, the process
    /// stays tracked (and listed as draining) until it is reaped.
    #[tokio::test(flavor = "current_thread")]
    async fn a_drain_stuck_past_its_sigkill_stops_holding_the_rollout() {
        local(async {
            let mut r = drainers("stuck", 1, "30", "[shutdown]\ngrace_period = 30\n").await;
            let old = currents(&r.sup)[0];
            r.sup.cfg.app.env.remove("FAKE_DRAIN"); // the new worker stops at once
            r.sup.begin_rollout(Kind::Restart, vec![1], String::new(), false).unwrap();
            r.until("replaced, the old one draining", |s| {
                matches!(s.roll, Some(Roll { step: Step::Ending { .. }, .. }))
            })
            .await;
            r.sup.after_drain();
            assert!(r.sup.roll.is_some(), "within grace_period it is waited for");
            // As if SIGKILLed at grace_period and still there DRAIN_SLACK later.
            let long_ago = Instant::now().checked_sub(Duration::from_secs(30) + DRAIN_SLACK + Duration::from_secs(1));
            let Some(long_ago) = long_ago else { return r.shutdown().await }; // a clock too young to go back
            if let Some(roll) = &mut r.sup.roll {
                roll.draining[0].since = long_ago;
            }
            r.sup.after_drain();
            assert!(r.sup.roll.is_none(), "the rollout ended without it");
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true));
            assert_eq!(r.sup.status().draining.len(), 1, "still listed while it is there");
            r.kill(old);
            r.until("the old one reaped", |s| !s.insts.contains_key(&old)).await;
            r.shutdown().await;
        })
        .await;
    }

    /// A stop while a finished rollout waits for its last drains: the
    /// outcome stays a success (every worker was replaced).
    #[tokio::test(flavor = "current_thread")]
    async fn a_stop_during_the_last_drains_keeps_the_outcome() {
        local(async {
            let mut r = drainers("stopend", 2, "2", "").await;
            r.sup.begin_rollout(Kind::Restart, vec![1, 2], String::new(), false).unwrap();
            r.until("every worker replaced", |s| matches!(s.roll, Some(Roll { step: Step::Ending { .. }, .. }))).await;
            r.sup.stopped = true;
            r.sup.stop_all();
            assert!(r.sup.roll.is_none());
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true), "{:?}", r.sup.last_rollout);
            r.shutdown().await;
        })
        .await;
    }

    /// A worker that listens at once, reporting FAKE_SOCK as its private
    /// health socket, and exits when stopped.
    const CHECKED: &str = r#"
trap 'exit 0' TERM
echo "{\"ev\":\"listening\",\"port\":1,\"socket\":\"$FAKE_SOCK\"}" >&3
sleep 60 &
wait $!
"#;

    /// The private health socket of `checked` workers: answers 503 while
    /// `failing` is above zero (counting it down), 200 after; `seen` is
    /// when each readiness check arrived.
    struct Probe {
        seen: Rc<RefCell<Vec<Instant>>>,
        failing: Rc<Cell<u32>>,
    }

    /// One worker gated on `/ready` checks on its private socket.
    async fn checked(name: &str, reload: &str, health: &str) -> (Rig, Probe) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut r = Rig::new(
            name,
            &format!(
                "[workers]\ncount = 1\noverlap = true\n[health]\nready_path = \"/ready\"\n{health}[reload]\n{reload}"
            ),
            CHECKED,
        );
        r.sup.cfg.app.shim = Some(true);
        let sock = std::env::temp_dir().join(format!("wp-probe-{name}-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        r.sup.cfg.app.env.insert("FAKE_SOCK".into(), sock.display().to_string());
        let probe = Probe { seen: Rc::default(), failing: Rc::default() };
        let (seen, failing) = (probe.seen.clone(), probe.failing.clone());
        tokio::task::spawn_local(async move {
            while let Ok((mut s, _)) = listener.accept().await {
                let mut buf = [0u8; 512];
                let n = s.read(&mut buf).await.unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).starts_with("GET /ready ") {
                    seen.borrow_mut().push(Instant::now());
                }
                let code = if failing.get() > 0 { "503 Service Unavailable" } else { "200 OK" };
                failing.set(failing.get().saturating_sub(1));
                let _ = s.write_all(format!("HTTP/1.1 {code}\r\nContent-Length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        r.sup.start_all();
        r.until("the worker running", |s| s.slots.values().all(|x| x.state == State::Running)).await;
        (r, probe)
    }

    /// Restarts the one worker; returns when it took over (`None`: the
    /// rollout failed). After each event it checks that no gate waited for
    /// a tick: a lane verifying has its first check running from the event
    /// that saw it listen on, and never sits on its last pass.
    async fn replace_checked(r: &mut Rig) -> Option<Instant> {
        let old = currents(&r.sup)[0];
        let required = r.sup.cfg.reload.health_passes;
        r.sup.begin_rollout(Kind::Restart, vec![1], String::new(), false).unwrap();
        let promoted = Cell::new(None);
        let waited = RefCell::new(Vec::new());
        r.until("the rollout's end", |s| {
            if let Some(Roll { step: Step::Batch(lanes), .. }) = &s.roll {
                for l in lanes {
                    let At::Verifying(v) = &l.at else { continue };
                    let reported = !s.insts[&v.new].sockets.is_empty();
                    if v.next_check.is_none() && !v.checking && reported {
                        waited.borrow_mut().push("listening, its first check not started");
                    }
                    if v.passes == required && v.cmd == Cmd::Skip && v.soak.is_zero() {
                        waited.borrow_mut().push("passed every check, not promoted");
                    }
                }
            }
            if promoted.get().is_none() && s.slots[&1].current != Some(old) {
                promoted.set(Some(Instant::now()));
            }
            s.roll.is_none()
        })
        .await;
        assert_eq!(waited.borrow().as_slice(), &[] as &[&str], "a gate waited");
        promoted.get()
    }

    fn gaps(seen: &[Instant]) -> Vec<Duration> {
        seen.windows(2).map(|w| w[1] - w[0]).collect()
    }

    /// The gates run on their own timers, not the shared tick: the first
    /// check as soon as it listens, each next one health_interval_ms after
    /// the previous, and the takeover right at the last pass.
    #[tokio::test(flavor = "current_thread")]
    async fn health_gates_run_when_due_and_promote_at_the_last_pass() {
        local(async {
            let every = Duration::from_millis(300);
            let (mut r, p) = checked("gates-due", "health_passes = 3\nhealth_interval_ms = 300\n", "").await;
            replace_checked(&mut r).await.expect("replaced");
            let seen = p.seen.borrow().clone();
            assert_eq!(seen.len(), 3, "health_passes checks, no more");
            for g in gaps(&seen) {
                assert!(g >= every, "checks {g:?} apart: never closer than health_interval_ms");
            }
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true));
            r.shutdown().await;
        })
        .await;
    }

    /// A failure resets the passes and the next check comes an interval
    /// later; min_ready then soaks, still checking, for its full length.
    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_check_resets_the_passes_and_min_ready_soaks_in_full() {
        local(async {
            let every = Duration::from_millis(200);
            let (mut r, p) =
                checked("gates-soak", "health_passes = 2\nhealth_interval_ms = 200\nmin_ready = 1\n", "").await;
            p.failing.set(2);
            let promoted = replace_checked(&mut r).await.expect("replaced");
            let seen = p.seen.borrow().clone();
            // 2 failures, 2 passes (the 4th check), then checks through the 1 s soak.
            assert!(seen.len() >= 5, "{} checks", seen.len());
            for g in gaps(&seen) {
                assert!(g >= every, "checks {g:?} apart");
            }
            let soak = promoted - seen[3];
            assert!(soak >= Duration::from_secs(1), "took over {soak:?} after its last pass (min_ready = 1)");
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true));
            r.shutdown().await;
        })
        .await;
    }

    /// A worker that never passes still fails its gates: after its first
    /// check (run the moment it listened, not counted) and 3 ×
    /// failure_threshold more.
    #[tokio::test(flavor = "current_thread")]
    async fn a_worker_failing_every_check_still_rolls_back() {
        local(async {
            let (mut r, p) =
                checked("gates-fail", "health_passes = 2\nhealth_interval_ms = 100\n", "failure_threshold = 1\n").await;
            let old = currents(&r.sup)[0];
            p.failing.set(u32::MAX);
            assert!(replace_checked(&mut r).await.is_none(), "never took over");
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(false), "{:?}", r.sup.last_rollout);
            assert_eq!(p.seen.borrow().len(), 4, "the first check, then 3 counted failures");
            assert_eq!(currents(&r.sup), vec![old], "the old worker still serves");
            r.shutdown().await;
        })
        .await;
    }
}

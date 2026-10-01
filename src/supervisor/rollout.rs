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

pub(super) enum GateEvent {
    Tick { seq: u64 },
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
    /// Dropping this stops the gate ticker.
    _ticker: oneshot::Sender<()>,
}

struct Snapshot {
    cfg: Config,
    shim_path: Option<PathBuf>,
    host_path: Option<PathBuf>,
    release: Option<release::Pin>,
}

enum Step {
    Idle,
    Preflight,
    /// One or more workers being replaced together.
    Batch(Vec<Lane>),
    Pausing,
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
    /// The old process drains (`then_spawn`: stop first, then start the new one).
    Draining {
        old: u64,
        then_spawn: bool,
    },
    Done,
}

impl At {
    /// The new process, until it has taken over.
    fn new_inst(&self) -> Option<u64> {
        match self {
            At::Starting { new, .. } | At::Passed { new, .. } => Some(*new),
            At::Verifying(v) => Some(v.new),
            At::Draining { .. } | At::Done => None,
        }
    }

    fn involves(&self, inst: u64) -> bool {
        match self {
            At::Starting { new, old, .. } | At::Passed { new, old } => *new == inst || *old == Some(inst),
            At::Verifying(v) => v.new == inst || v.old == Some(inst),
            At::Draining { old, .. } => *old == inst,
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
    Drained(usize),
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
    /// next to the one it replaces. Done when the queue is empty.
    pub(super) fn advance_rollout(&mut self) {
        let overlap = self.cfg.overlap();
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
                self.finish_rollout(true, None);
                return;
            }
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
                    At::Draining { old, then_spawn: true }
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
                    At::Draining { old, then_spawn: true }
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
            cmd,
            soak,
            soak_until: None,
            canary,
        });
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
            GateEvent::Check { seq, inst, result } if Some(seq) == seq_now => self.gate_check(inst, result),
            GateEvent::Command { seq, inst, result } if Some(seq) == seq_now => {
                let Some((slot, v)) = self.verifying(inst) else { return };
                match result {
                    Ok(()) => {
                        v.cmd = Cmd::Passed;
                        info!("verify_command passed", worker = slot);
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

    fn gate_tick(&mut self) {
        let now = Instant::now();
        let timeout = self.cfg.reload.timeout;
        let required = self.cfg.reload.health_passes;
        let checks = required > 0 && self.can_check(0);
        let Some(roll) = &mut self.roll else { return };
        let seq = roll.seq;
        let Step::Batch(lanes) = &mut roll.step else { return };
        let several = lanes.len() > 1;
        // Decide for every lane first, then act (acting needs `self`).
        let mut fail = None;
        let mut checks_due = Vec::new();
        let mut commands_due = Vec::new();
        for lane in lanes.iter_mut() {
            let mut passed = None;
            match &mut lane.at {
                At::Starting { deadline, .. } if now > *deadline => {
                    fail = Some((lane.slot, format!("new worker not listening within {timeout}s")));
                    break;
                }
                At::Verifying(v) => {
                    if now > v.deadline {
                        fail = Some((
                            lane.slot,
                            format!("gates not passed within {timeout}s (health {}/{required})", v.passes),
                        ));
                        break;
                    }
                    if v.checking || v.cmd == Cmd::Running {
                        continue;
                    }
                    if checks && v.passes < required {
                        v.checking = true;
                        checks_due.push(v.new);
                        continue;
                    }
                    if v.cmd == Cmd::Pending {
                        v.cmd = Cmd::Running;
                        commands_due.push((v.new, lane.slot));
                        continue;
                    }
                    if !v.soak.is_zero() {
                        let until = *v.soak_until.get_or_insert(crate::restart::later(now, v.soak));
                        if now < until {
                            if checks {
                                v.checking = true;
                                checks_due.push(v.new);
                            }
                            continue;
                        }
                    }
                    passed = Some((v.new, v.old));
                }
                _ => {}
            }
            if let Some((new, old)) = passed {
                lane.at = At::Passed { new, old };
                if several {
                    debug!("new worker passed its gates; waiting for the rest of its batch", worker = lane.slot);
                }
            }
        }
        if let Some((slot, msg)) = fail {
            self.fail_at(Some(slot), msg);
            return;
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
        let Some((slot, v)) = self.verifying(inst) else { return };
        v.checking = false;
        let soaking = v.soak_until.is_some();
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
                v.fails += 1;
                if !soaking {
                    v.passes = 0;
                }
                debug!("gate health check failed", worker = slot, error = e, consecutive = v.fails);
                if soaking && v.fails >= threshold {
                    let what = if v.canary { "canary" } else { "new worker" };
                    self.fail_at(Some(slot), format!("{what} failed {threshold} health checks during soak: {e}"));
                } else if !soaking && v.fails >= threshold * 3 {
                    self.fail_at(Some(slot), format!("new worker keeps failing health checks: {e}"));
                }
            }
        }
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
        let mut finished = 0;
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
                    if canary {
                        info!(
                            "canary passed; draining the worker it replaced",
                            worker = label,
                            new_pid = new_pid,
                            old_pid = old_pid
                        );
                    } else {
                        info!(
                            "worker replaced; draining old process",
                            worker = label,
                            new_pid = new_pid,
                            old_pid = old_pid
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
                    lanes.push(Lane { slot: slot_id, at: At::Draining { old, then_spawn: false } });
                }
                None => {
                    info!("worker passed its gates", worker = label, pid = new_pid);
                    lanes.push(Lane { slot: slot_id, at: At::Done });
                    finished += 1;
                }
            }
        }
        if let Some(r) = &mut self.roll {
            r.done += finished;
            r.step = Step::Batch(lanes);
        }
        self.check_all_ready();
        self.batch_progress();
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
            At::Draining { then_spawn: true, .. } => OnExit::Spawn(slot),
            At::Draining { then_spawn: false, .. } => OnExit::Drained(slot),
            At::Done => return,
        };
        match act {
            OnExit::Fail(slot, msg) => self.fail_at(Some(slot), msg),
            OnExit::Drained(slot) => {
                if let Some(Roll { step: Step::Batch(lanes), done, .. }) = &mut self.roll {
                    if let Some(l) = lanes.iter_mut().find(|l| l.slot == slot) {
                        l.at = At::Done;
                        *done += 1;
                    }
                }
                self.batch_progress();
            }
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
    /// release pin in effect before the rollout are restored.
    fn fail_at(&mut self, at: Option<usize>, reason: String) {
        let Some(roll) = &mut self.roll else { return };
        // (slot, new, old) of every worker of the batch that has not taken over.
        let pending: Vec<(usize, u64, Option<u64>)> = match &roll.step {
            Step::Batch(lanes) => lanes
                .iter()
                .filter_map(|l| match &l.at {
                    At::Starting { new, old, .. } | At::Passed { new, old } => Some((l.slot, *new, *old)),
                    At::Verifying(v) => Some((l.slot, v.new, v.old)),
                    At::Draining { .. } | At::Done => None,
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
        self.finish_rollout(false, Some(message));

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

    pub(super) fn abort_rollout(&mut self, reason: &str) {
        if self.roll.is_some() {
            self.finish_rollout(false, Some(format!("aborted: {reason}")));
        }
    }

    fn finish_rollout(&mut self, ok: bool, message: Option<String>) {
        // The phase it failed in, if not published yet (it cannot be after
        // `rollout_done`). Idle: all done, or failed before a step began.
        if self.roll.as_ref().is_some_and(|r| !matches!(r.step, Step::Idle)) {
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
        if ok {
            info!(message)
        } else {
            // The message says what failed and what Warden did about it;
            // the hint, what the operator does next.
            let hint = if message.starts_with("aborted:") {
                "a stop, `restart --hard` or shutdown took over during the rollout; run it again once the workers \
                 are back, if it is still needed"
            } else {
                "the reason names the gate that failed; the new workers' output is in `warden logs <app>` and the \
                 last rollout in `warden describe <app>`. Fix the release or config, then run it again"
            };
            error!(message, hint = hint)
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
            Step::Idle => (0, 0, 0),
            Step::Preflight => (1, 0, 0),
            Step::Pausing => (2, 0, 0),
            Step::Batch(lanes) => batch_key(lanes),
        };
        Some(PhaseKey { seq: r.seq, done: r.done, step, inst, detail })
    }

    pub(super) fn rollout_status(&self) -> Option<RolloutStatus> {
        let r = self.roll.as_ref()?;
        let phase = match &r.step {
            // Only seen at the start (the `rollout` event announcing it).
            Step::Idle if r.done == 0 => "starting".to_string(),
            Step::Idle => "next worker".to_string(),
            Step::Preflight => "running preflight".to_string(),
            Step::Pausing => format!("pausing {}s between workers", self.cfg.reload.pause),
            Step::Batch(lanes) if lanes.len() == 1 => self.lane_phase(&lanes[0]),
            Step::Batch(lanes) => self.batch_phase(lanes),
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

    /// One worker: `worker 2: health checks 1/3`.
    fn lane_phase(&self, l: &Lane) -> String {
        let label = self.label(l.slot);
        match &l.at {
            At::Starting { new, .. } => format!("worker {label}: starting new process (pid {})", self.pid_of(*new)),
            At::Draining { old, .. } => format!("worker {label}: draining old process (pid {})", self.pid_of(*old)),
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
        let draining: Vec<String> = lanes
            .iter()
            .filter_map(|l| if let At::Draining { old, .. } = l.at { Some(self.pid_of(old).to_string()) } else { None })
            .collect();
        let passed_note = if passed > 0 { format!(", {passed}/{n} passed") } else { String::new() };
        if starting > 0 {
            format!("{who}: starting {n} new processes ({}/{n} listening)", n - starting)
        } else if let Some(v) = slowest {
            format!("{who}: {}{passed_note}", self.verify_phase(v))
        } else if !draining.is_empty() {
            format!("{who}: draining {} old process(es) (pid {})", draining.len(), draining.join(", "))
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
    let draining = count(|a| matches!(a, At::Draining { .. }));
    let slowest = lanes
        .iter()
        .filter_map(|l| if let At::Verifying(v) = &l.at { Some(v) } else { None })
        .min_by_key(|v| (v.stage(), u64::from(v.passes)));
    if lanes.len() == 1 {
        return match &lanes[0].at {
            At::Starting { new, .. } => (3, *new, 0),
            At::Draining { old, .. } => (4, *old, 0),
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
    } else if draining > 0 {
        (4, first, draining)
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
}

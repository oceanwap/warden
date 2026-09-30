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
    /// Config in effect before this rollout's preflight applied a new one;
    /// restored if the rollout fails, so restarts don't roll forward to it.
    prev: Option<Snapshot>,
    /// Dropping this stops the gate ticker.
    _ticker: oneshot::Sender<()>,
}

struct Snapshot {
    cfg: Config,
    shim_path: Option<PathBuf>,
    host_path: Option<PathBuf>,
}

enum Step {
    Idle,
    Preflight,
    Starting { slot: usize, new: u64, old: Option<u64>, deadline: Instant },
    Verifying(Verify),
    Draining { slot: usize, old: u64, then_spawn: bool },
    Pausing,
}

struct Verify {
    slot: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    Skip,
    Pending,
    Running,
    Passed,
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

        let prev = if kind.is_deploy() {
            let snap = Snapshot {
                cfg: self.cfg.clone(),
                shim_path: self.shim_path.clone(),
                host_path: self.host_path.clone(),
            };
            if let Err(e) = self.preflight_sync() {
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
        match kind {
            Kind::Replace | Kind::Recovery => {
                info!(
                    "replacing worker",
                    worker = slots.first().map(|s| self.label(*s)).unwrap_or_default(),
                    reason = reason
                );
            }
            _ => {
                info!(format!("{} started", kind.name()), workers = total, seq = seq);
                if kind.is_deploy() {
                    systemd::reloading();
                }
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
            prev,
            _ticker: cancel_tx,
        });
        match preflight {
            Some(cmd) => {
                if let Some(r) = &mut self.roll {
                    r.step = Step::Preflight;
                }
                info!("running preflight", command = cmd);
                let env = vec![("WARDEN_APP".to_string(), self.cfg.app.name.clone())];
                let fut = run_shell(
                    cmd,
                    self.cfg.app.working_directory.clone(),
                    env,
                    Duration::from_secs(self.cfg.reload.timeout),
                );
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
        let base =
            self.cfg.app.working_directory.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
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
                if !root.is_dir() {
                    return Err(format!("preflight: static.root {} is not a directory", root.display()));
                }
            }
            Mode::Process => {
                let env: std::collections::BTreeMap<String, String> =
                    self.cfg.app.environment().map(|(k, v)| (k.clone(), v.clone())).collect();
                if !command_exists(&self.cfg.app.command, &env) {
                    return Err(format!("preflight: command `{}` not found", self.cfg.app.command));
                }
                let script = self.cfg.app.args.iter().find(|a| {
                    !a.starts_with('-')
                        && [".js", ".mjs", ".cjs", ".ts", ".mts", ".tsx", ".jsx"].iter().any(|x| a.ends_with(x))
                });
                if let Some(s) = script {
                    let p = base.join(s);
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

    pub(super) fn advance_rollout(&mut self) {
        loop {
            let Some(roll) = &mut self.roll else { return };
            if !matches!(roll.step, Step::Idle) || self.shutting_down || self.stopped {
                return;
            }
            let Some(slot_id) = roll.queue.pop_front() else {
                self.finish_rollout(true, None);
                return;
            };
            let Some(slot) = self.slots.get_mut(&slot_id).filter(|s| !s.removing) else { continue };
            slot.token += 1; // cancel a pending restart timer; this rollout takes over
            let current = slot.current;
            let serving = current.filter(|_| matches!(slot.state, State::Running | State::Restarting));
            let deadline = crate::restart::later(Instant::now(), Duration::from_secs(self.cfg.reload.timeout));
            let overlap = self.cfg.overlap();
            let step = match (serving, current) {
                // Each worker owns its port, or the app can't share it: stop,
                // then start (a gap for this worker only).
                (Some(old), _) if !overlap => {
                    self.stop_instance(old);
                    Step::Draining { slot: slot_id, old, then_spawn: true }
                }
                (Some(old), _) => match self.spawn_instance(slot_id, Role::Replacement) {
                    Ok(new) => Step::Starting { slot: slot_id, new, old: Some(old), deadline },
                    Err(e) => {
                        self.fail_rollout(format!("could not start a new worker: {e}"));
                        return;
                    }
                },
                // Starting / stopping: let it finish exiting, then start fresh.
                (None, Some(old)) => {
                    self.stop_instance(old);
                    Step::Draining { slot: slot_id, old, then_spawn: true }
                }
                (None, None) => match self.spawn_current(slot_id) {
                    Some(new) => Step::Starting { slot: slot_id, new, old: None, deadline },
                    None => {
                        self.fail_rollout("could not start a new worker".into());
                        return;
                    }
                },
            };
            if let Some(r) = &mut self.roll {
                r.step = step;
            }
            return;
        }
    }

    /// Called from `mark_ready`: the instance is listening.
    pub(super) fn rollout_on_ready(&mut self, inst: u64) {
        let can_check = self.can_check(inst);
        let rl = self.cfg.reload.clone();
        let worker_mode = self.is_worker_mode();
        let label_of = |s: usize| if worker_mode { "host".to_string() } else { s.to_string() };
        let Some(roll) = &mut self.roll else { return };
        let Step::Starting { slot, new, old, deadline } = roll.step else { return };
        if new != inst {
            return;
        }
        let canary = roll.canary && roll.kind == Kind::SafeReload;
        let soak = Duration::from_secs(if canary { rl.canary_soak } else { rl.min_ready });
        let cmd = if rl.verify_command.is_some() { Cmd::Pending } else { Cmd::Skip };
        if canary {
            info!(
                "canary listening next to the old worker",
                worker = label_of(slot),
                soak_s = soak.as_secs(),
                health_passes = if can_check { rl.health_passes } else { 0 },
            );
        }
        roll.step = Step::Verifying(Verify {
            slot,
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
        // Nothing to wait for: promote right away.
        if (rl.health_passes == 0 || !can_check) && cmd == Cmd::Skip && soak.is_zero() {
            self.promote();
        }
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
                let Some(Roll { step: Step::Verifying(v), .. }) = &mut self.roll else { return };
                if v.new != inst {
                    return;
                }
                match result {
                    Ok(()) => {
                        v.cmd = Cmd::Passed;
                        info!("verify_command passed", worker = v.slot);
                    }
                    Err(e) => self.fail_rollout(format!("verify_command failed: {e}")),
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

    fn gate_tick(&mut self) {
        let now = Instant::now();
        let timeout = self.cfg.reload.timeout;
        let required = self.cfg.reload.health_passes;
        let Some(roll) = &mut self.roll else { return };
        let seq = roll.seq;
        match &mut roll.step {
            Step::Starting { deadline, .. } if now > *deadline => {
                self.fail_rollout(format!("new worker not listening within {timeout}s"));
            }
            Step::Verifying(v) => {
                if now > v.deadline {
                    let msg = format!("gates not passed within {timeout}s (health {}/{required})", v.passes);
                    self.fail_rollout(msg);
                    return;
                }
                if v.checking || v.cmd == Cmd::Running {
                    return;
                }
                let (new, slot) = (v.new, v.slot);
                let checks = required > 0 && self.can_check(new);
                let Some(Roll { step: Step::Verifying(v), .. }) = &mut self.roll else { return };
                if checks && v.passes < required {
                    v.checking = true;
                    self.launch_check(seq, new);
                    return;
                }
                if v.cmd == Cmd::Pending {
                    v.cmd = Cmd::Running;
                    self.launch_command(seq, new, slot);
                    return;
                }
                if !v.soak.is_zero() {
                    let until = *v.soak_until.get_or_insert(crate::restart::later(now, v.soak));
                    if now < until {
                        if checks {
                            v.checking = true;
                            self.launch_check(seq, new);
                        }
                        return;
                    }
                }
                self.promote();
            }
            _ => {}
        }
    }

    fn gate_check(&mut self, inst: u64, result: Result<(), String>) {
        let required = self.cfg.reload.health_passes;
        let threshold = self.cfg.health.failure_threshold;
        let Some(Roll { step: Step::Verifying(v), .. }) = &mut self.roll else { return };
        if v.new != inst {
            return;
        }
        v.checking = false;
        let soaking = v.soak_until.is_some();
        match result {
            Ok(()) => {
                v.fails = 0;
                if !soaking && v.passes < required {
                    v.passes += 1;
                    debug!("gate health check passed", worker = v.slot, passes = v.passes, required = required);
                    if v.passes == required {
                        info!("new worker passed health checks", worker = v.slot, passes = required);
                    }
                }
            }
            Err(e) => {
                v.fails += 1;
                if !soaking {
                    v.passes = 0;
                }
                debug!("gate health check failed", worker = v.slot, error = e, consecutive = v.fails);
                if soaking && v.fails >= threshold {
                    let what = if v.canary { "canary" } else { "new worker" };
                    self.fail_rollout(format!("{what} failed {threshold} health checks during soak: {e}"));
                } else if !soaking && v.fails >= threshold * 3 {
                    self.fail_rollout(format!("new worker keeps failing health checks: {e}"));
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
        let fut =
            run_shell(cmd, self.cfg.app.working_directory.clone(), env, Duration::from_secs(self.cfg.reload.timeout));
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let result = crate::guard::catch_unwind(fut).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
            let _ = tx.send(Event::Gate(GateEvent::Command { seq, inst, result }));
        });
    }

    /// The new worker passed every gate: it takes over, the old one drains.
    fn promote(&mut self) {
        if let Some(Roll { step: Step::Verifying(v), .. }) = &self.roll {
            if let Some(t) = self.insts.get(&v.new).and_then(|i| i.threads.iter().find(|(_, t)| t.crashed)) {
                let msg = format!("Worker {} of the new host crashed", t.0);
                self.fail_rollout(msg);
                return;
            }
        }
        let Some(roll) = &mut self.roll else { return };
        let Step::Verifying(v) = &roll.step else { return };
        let (slot_id, new, old, canary) = (v.slot, v.new, v.old.filter(|o| self.insts.contains_key(o)), v.canary);
        let kill_old = roll.kill_old;
        roll.canary = false;
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
                if let Some(r) = &mut self.roll {
                    r.step = Step::Draining { slot: slot_id, old, then_spawn: false };
                }
            }
            None => {
                info!("worker passed its gates", worker = label, pid = new_pid);
                self.slot_done();
            }
        }
        self.check_all_ready();
    }

    fn slot_done(&mut self) {
        let pause = self.cfg.reload.pause;
        let Some(roll) = &mut self.roll else { return };
        roll.done += 1;
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
        let Some(roll) = &mut self.roll else { return };
        match &mut roll.step {
            Step::Starting { new, .. } if *new == inst => {
                self.fail_rollout(format!("new worker exited before listening: {reason}"));
            }
            Step::Verifying(v) if v.new == inst => {
                let what = if v.canary { "canary" } else { "new worker" };
                self.fail_rollout(format!("{what} exited during its checks: {reason}"));
            }
            Step::Starting { old, .. } if *old == Some(inst) => *old = None,
            Step::Verifying(v) if v.old == Some(inst) => v.old = None,
            Step::Draining { slot, old, then_spawn } if *old == inst => {
                let (slot, then_spawn) = (*slot, *then_spawn);
                if !then_spawn {
                    self.slot_done();
                    return;
                }
                match self.spawn_current(slot) {
                    Some(new) => {
                        if let Some(r) = &mut self.roll {
                            r.step = Step::Starting { slot, new, old: None, deadline };
                        }
                    }
                    None => self.fail_rollout("could not start a new worker".into()),
                }
            }
            _ => {}
        }
    }

    /// The replacement (not yet promoted) this rollout is starting or verifying for `slot_id`.
    pub(super) fn rollout_new_instance(&self, slot_id: usize) -> Option<u64> {
        let roll = self.roll.as_ref()?;
        let new = match &roll.step {
            Step::Starting { slot, new, .. } if *slot == slot_id => *new,
            Step::Verifying(v) if v.slot == slot_id => v.new,
            _ => return None,
        };
        self.insts.get(&new).filter(|i| i.role == Role::Replacement).map(|_| new)
    }

    /// A rollout is currently working on this slot.
    pub(super) fn rollout_replacing(&self, slot_id: usize) -> bool {
        match self.roll.as_ref().map(|r| &r.step) {
            Some(Step::Starting { slot, .. } | Step::Draining { slot, .. }) => *slot == slot_id,
            Some(Step::Verifying(v)) => v.slot == slot_id,
            _ => false,
        }
    }

    /// Stop the rollout. While the old worker still exists this is a free
    /// rollback: the new process is stopped and nothing else changes. If the
    /// new process already owns the slot (the old one died, the slot was empty,
    /// or offset ports), it is stopped and the slot restarted through crash
    /// handling. The config that was in effect before the rollout is restored.
    pub(super) fn fail_rollout(&mut self, reason: String) {
        let Some(roll) = &mut self.roll else { return };
        let (slot, new, old) = match &roll.step {
            Step::Starting { slot, new, old, .. } => (Some(*slot), Some(*new), *old),
            Step::Verifying(v) => (Some(v.slot), Some(v.new), v.old),
            Step::Draining { slot, .. } => (Some(*slot), None, None),
            _ => (None, None, None),
        };
        let (kind, done, total, kill_old) = (roll.kind, roll.done, roll.total, roll.kill_old);
        let prev = roll.prev.take();
        let restored = prev.is_some();
        if let Some(p) = prev {
            self.restore(p);
        }
        let old = old.filter(|o| self.insts.contains_key(o));
        let new = new.filter(|n| self.insts.contains_key(n));
        let mut rolled_back = false;
        let mut restarting = false;
        if let Some(n) = new {
            if old.is_some() {
                rolled_back = true;
            } else if let Some(i) = self.insts.get_mut(&n) {
                // It owns the slot: stop it (draining) and restart the slot afterwards.
                i.restart_on_exit = true;
                restarting = true;
            }
            self.stop_instance(n);
        } else if old.is_some() {
            rolled_back = true;
        }
        let who = slot.map(|s| format!("worker {}", self.label(s))).unwrap_or_else(|| "preflight".into());
        let cfg_note = if restored { " The previous config is back in effect." } else { "" };
        let message = match kind {
            Kind::SafeReload | Kind::Reload if done == 0 && rolled_back => format!(
                "{} failed at {who}: {reason}. Rolled back: every worker still runs the previous version.{cfg_note}",
                kind.name()
            ),
            Kind::SafeReload | Kind::Reload if done == 0 && restarting => format!(
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
                if rolled_back { "; the old worker keeps serving" } else { "" }
            ),
        };
        self.finish_rollout(false, Some(message));

        let Some(slot) = slot else { return };
        if let Some(o) = old {
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
        let Some(roll) = self.roll.take() else { return };
        let secs = roll.started.elapsed().as_secs_f64();
        let message = message.unwrap_or_else(|| match roll.kind {
            Kind::Reload | Kind::SafeReload => {
                format!("{} complete: {} worker(s) replaced in {secs:.1}s", roll.kind.name(), roll.done)
            }
            Kind::Restart => format!("restart complete in {secs:.1}s"),
            Kind::Replace | Kind::Recovery => format!("worker replaced ({}) in {secs:.1}s", roll.reason),
        });
        if ok {
            info!(message)
        } else {
            error!(message)
        }
        if roll.kind.is_deploy() || roll.kind == Kind::Restart {
            systemd::notify(if ok {
                "READY=1\nSTATUS=all workers ready"
            } else {
                "READY=1\nSTATUS=last rollout failed"
            });
        }
        self.last_rollout = Some(RolloutOutcome {
            seq: roll.seq,
            kind: roll.kind.name().into(),
            ok,
            message,
            duration_secs: (secs * 10.0).round() / 10.0,
        });
    }

    pub(super) fn rollout_status(&self) -> Option<RolloutStatus> {
        let r = self.roll.as_ref()?;
        let pid = |i: &u64| self.insts.get(i).map(|x| x.handle.pid).unwrap_or(0);
        let now = Instant::now();
        let phase = match &r.step {
            Step::Idle => "next worker".to_string(),
            Step::Preflight => "running preflight".to_string(),
            Step::Pausing => format!("pausing {}s between workers", self.cfg.reload.pause),
            Step::Starting { slot, new, .. } => {
                format!("worker {}: starting new process (pid {})", self.label(*slot), pid(new))
            }
            Step::Draining { slot, old, .. } => {
                format!("worker {}: draining old process (pid {})", self.label(*slot), pid(old))
            }
            Step::Verifying(v) => {
                let who = if v.canary {
                    format!("worker {} (canary)", self.label(v.slot))
                } else {
                    format!("worker {}", self.label(v.slot))
                };
                if let Some(until) = v.soak_until {
                    format!("{who}: soaking, {}s left", until.saturating_duration_since(now).as_secs())
                } else if v.cmd == Cmd::Running {
                    format!("{who}: running verify_command")
                } else {
                    format!("{who}: health checks {}/{}", v.passes, self.cfg.reload.health_passes)
                }
            }
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
async fn run_shell(
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

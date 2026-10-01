//! Hot standbys (`[workers] standby = N`, process mode): N extra workers kept
//! started (modules loaded, app initialized) but not listening, outside the
//! slots. When a worker dies, a standby is promoted into its slot: Warden
//! sends `{"cmd":"promote","worker":W}` on fd 3, the shim makes the app's
//! deferred listen for real (a few milliseconds instead of the app's whole
//! startup) and the standby becomes worker W. A new standby then starts in
//! the background.
//!
//! - Gates: a standby can be promoted once it is initialized
//!   (`standby_ready`) and has passed what a rollout's new worker must pass
//!   before taking traffic: `reload.health_passes` checks on its private
//!   socket and `reload.verify_command`. Idle standbys also get the periodic
//!   liveness checks and the watchdog; a failing one is replaced, never
//!   promoted.
//! - Restart policy: a promotion is the slot's restart. The crash is counted
//!   and backed off exactly as without standbys (a crash loop still ends in
//!   FAILED); the standby only replaces the cold start.
//! - Crash loops of standbys: their exits share one restart tracker (same
//!   policy: backoff, `max_restarts` in `restart_window`, then FAILED until
//!   `failed_cooldown` or `warden reset`); crashed workers meanwhile restart
//!   the normal way.
//! - Deploys: a standby runs the code it started with. While a reload,
//!   safe-reload or restart of the workers runs, standbys are neither
//!   promoted nor started (a crash restarts the normal way). When it
//!   succeeds, every standby is replaced by a fresh one (through the gates);
//!   when it fails or rolls back they stay, matching the workers that kept
//!   the previous version. Surge batches change nothing here: the pool stays
//!   paused for the whole rollout, and a batch that rolls back keeps it.
//! - Release pinning (`release.rs`): a standby starts in the pinned release
//!   and records it; it is promoted only while that is still the workers'
//!   release, and replaced when the pin moved outside a deploy.
//! - Recycling (memory, lifetime, health, hang) keeps the code: an available
//!   standby is the replacement worker (it listens next to the old worker,
//!   passes the gates, then the old one drains).

use super::*;
use crate::restart::Tracker;
use crate::worker::STANDBY_SLOT;

/// `worker=` on a standby's log lines: its own `sN`.
fn sb(i: &Instance) -> String {
    standby_label(i.standby_number.unwrap_or(0))
}

/// `worker=` on log lines about the pool as a whole (`--worker standby`
/// shows them with every standby's own lines).
const POOL: &str = "standby";

/// How long a promoted standby may take to listen: it is initialized, so
/// this is a bind (milliseconds); a standby that can't is killed and the
/// slot restarted (bounded by `workers.ready_timeout`).
pub(super) const PROMOTE_TIMEOUT: Duration = Duration::from_secs(5);

/// `status()`'s per-process sampler: (pid, last sample, start) → (stats, CPU %).
pub(super) type Sampler<'a> =
    dyn Fn(u32, &mut Option<(Instant, f64)>, Instant) -> Option<(metrics::ProcStats, f64)> + 'a;

/// The pool's own state; its members are the `Role::Standby` instances.
#[derive(Default)]
pub(super) struct Pool {
    tracker: Tracker,
    /// Invalidates a pending refill (`Event::StandbyDue`).
    token: u64,
    /// A refill is scheduled after a crash (backoff): start nothing before it.
    waiting: bool,
    /// Too many standby crashes: retried after `failed_cooldown`.
    failed_at: Option<Instant>,
    /// Standbys are off until Warden restarts: one listened before promotion.
    disabled: Option<String>,
    restarts: u64,
    crashes: u64,
    last_exit: Option<String>,
}

impl Supervisor {
    fn standby_target(&self) -> usize {
        if self.is_worker_mode() { 0 } else { self.cfg.workers.standby }
    }

    /// A reload, safe-reload or restart of workers runs: standbys would mix
    /// versions, so none is promoted or started until it ends.
    pub(super) fn deploy_in_progress(&self) -> bool {
        self.roll.as_ref().is_some_and(|r| matches!(r.kind, Kind::Reload | Kind::SafeReload | Kind::Restart))
    }

    fn live_standbys(&self) -> Vec<u64> {
        let mut ids: Vec<u64> =
            self.insts.iter().filter(|(_, i)| i.role == Role::Standby && !i.stopping).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        ids
    }

    /// The number of the next standby: the lowest no live standby has
    /// (1..=standby). One being stopped may still have it, as an old worker
    /// shares its number with its replacement during a rollout.
    pub(super) fn free_standby_number(&self) -> usize {
        let taken: std::collections::BTreeSet<usize> = self
            .insts
            .values()
            .filter(|i| i.role == Role::Standby && !i.stopping)
            .filter_map(|i| i.standby_number)
            .collect();
        (1..).find(|n| !taken.contains(n)).unwrap_or(1)
    }

    /// Start standbys up to `[workers] standby`, in the background: not
    /// while a worker is starting (they would compete for the CPU), a
    /// deploy runs, or the pool backs off.
    pub(super) fn fill_pool(&mut self) {
        let target = self.standby_target();
        let p = &self.pool;
        if target == 0
            || self.shutting_down
            || self.stopped
            || p.waiting
            || p.failed_at.is_some()
            || p.disabled.is_some()
        {
            return;
        }
        if self.deploy_in_progress() || self.slots.values().any(|s| s.state == State::Starting) {
            return;
        }
        for _ in self.live_standbys().len()..target {
            if !self.spawn_standby() {
                break;
            }
        }
    }

    /// The release workers start in now (`[app] pin_release`; None: unpinned).
    fn pinned_release(&self) -> Option<PathBuf> {
        self.release.as_ref().map(|p| p.real.clone())
    }

    fn spawn_standby(&mut self) -> bool {
        let label = standby_label(self.free_standby_number());
        match self.spawn_instance(STANDBY_SLOT, Role::Standby) {
            Ok(id) => {
                // `spawn_instance` checked the pin; this is the one it used.
                let release = self.pinned_release();
                if let Some(i) = self.insts.get_mut(&id) {
                    i.release = release;
                }
                true
            }
            Err(e) => {
                error!(
                    "failed to start a standby",
                    worker = label,
                    command = self.cfg.app.command,
                    error = e,
                    hint = "the same command starts the workers: check it (`error` says what failed); standbys \
                            are retried with backoff",
                );
                self.emit_worker(STANDBY_SLOT, WorkerEvent::Crashed, None, || {
                    Some(format!("spawn failed: {e} (standby)"))
                });
                self.pool.crashes += 1;
                self.pool.last_exit = Some(format!("spawn failed: {e}"));
                self.standby_crashed(Duration::ZERO);
                false
            }
        }
    }

    /// The shim's `standby_ready`: the app is initialized, its listen deferred.
    pub(super) fn on_standby_ready(&mut self, inst_id: u64, socket: Option<String>) {
        let port = self.cfg.app.port;
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        if i.stopping {
            return;
        }
        let Some(st) = i.standby.as_mut().filter(|st| st.ready_at.is_none()) else { return };
        st.ready_at = Some(Instant::now());
        if let Some(s) = socket {
            i.sockets.insert(STANDBY_SLOT, PathBuf::from(s));
        }
        let (pid, ms, label) = (i.handle.pid, i.started.elapsed().as_millis(), sb(i));
        // Safety net: a standby in the port's SO_REUSEPORT group takes traffic.
        if let Some(p) = port {
            if networking::count_listeners(pid, p).is_some_and(|n| n > 0) {
                self.disable_pool(format!("standby pid {pid} listens on port {p} before its promotion"));
                return;
            }
        }
        debug!("standby initialized", worker = label, pid = pid, startup_ms = ms);
        self.standby_gates(inst_id);
    }

    /// A standby listened on the app's port before promotion: the app
    /// listens in a way the shim does not defer, so every standby would
    /// take traffic. Stop them and start no more.
    pub(super) fn disable_pool(&mut self, why: String) {
        if self.pool.disabled.is_some() {
            return;
        }
        error!(
            "standbys disabled: a standby took the app's port before being promoted",
            worker = POOL,
            reason = why,
            action = "standbys stopped; crashed workers restart the normal way",
            hint = "the shim defers Bun.serve and node:http/net listen() on app.port; this app listens another way. Set [workers] standby = 0",
        );
        self.pool.disabled = Some(why);
        for id in self.live_standbys() {
            self.stop_instance(id);
        }
    }

    /// Run the next gate of a standby, or declare it available.
    pub(super) fn standby_gates(&mut self, inst_id: u64) {
        let required = self.cfg.reload.health_passes;
        let path = self.cfg.ready_path().filter(|_| required > 0);
        let verify = self.cfg.reload.verify_command.is_some();
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        if i.stopping {
            return;
        }
        let Some(st) = i.standby.as_mut() else { return };
        if st.checking || st.available || st.ready_at.is_none() {
            return;
        }
        if let Some(path) = path.filter(|_| st.passes < required) {
            st.checking = true;
            let sockets: Vec<PathBuf> = i.sockets.values().cloned().collect();
            self.launch_standby_check(inst_id, sockets, path);
            return;
        }
        if verify && !st.verified {
            st.checking = true;
            self.launch_standby_verify(inst_id);
            return;
        }
        st.available = true;
        let (pid, ms) = (i.handle.pid, i.started.elapsed().as_millis());
        info!("standby ready", worker = sb(i), pid = pid, startup_ms = ms);
        self.emit_worker(STANDBY_SLOT, WorkerEvent::Ready, Some(pid), || Some(format!("startup_ms={ms} role=standby")));
    }

    fn launch_standby_check(&self, inst: u64, sockets: Vec<PathBuf>, path: String) {
        let timeout = Duration::from_secs(self.cfg.health.timeout);
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let check = async move {
                if sockets.is_empty() {
                    return Err("the standby reported no private health socket".to_string());
                }
                rollout::check_instance_sockets(sockets, path, timeout).await
            };
            let result =
                crate::guard::catch_unwind(check).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
            let _ = tx.send(Event::StandbyChecked { inst, result });
        });
    }

    fn launch_standby_verify(&self, inst: u64) {
        let (Some(cmd), Some(i)) = (self.cfg.reload.verify_command.clone(), self.insts.get(&inst)) else { return };
        let socket = i.sockets.values().next().map(|p| p.display().to_string()).unwrap_or_default();
        let mut env = vec![
            ("WARDEN_APP".to_string(), self.cfg.app.name.clone()),
            ("WARDEN_WORKER_ID".to_string(), "standby".to_string()),
            ("WARDEN_WORKER_PID".to_string(), i.handle.pid.to_string()),
            ("WARDEN_WORKER_SOCKET".to_string(), socket.clone()),
            ("WARDEN_WORKER_SOCKETS".to_string(), socket),
            ("WARDEN_STANDBY".to_string(), "1".to_string()),
        ];
        if let Some(p) = self.cfg.app.port {
            env.push(("PORT".into(), p.to_string()));
        }
        debug!("running verify_command for a standby", worker = sb(i), pid = i.handle.pid, command = cmd);
        let fut = rollout::run_shell(
            cmd,
            self.cfg.app.working_directory.clone(),
            env,
            Duration::from_secs(self.cfg.reload.timeout),
        );
        let tx = self.tx.clone();
        tokio::task::spawn_local(async move {
            let result = crate::guard::catch_unwind(fut).await.unwrap_or_else(|p| Err(format!("internal error: {p}")));
            let _ = tx.send(Event::StandbyVerified { inst, result });
        });
    }

    pub(super) fn on_standby_checked(&mut self, inst_id: u64, result: Result<(), String>) {
        let required = self.cfg.reload.health_passes;
        let threshold = self.cfg.health.failure_threshold;
        let every = Duration::from_millis(self.cfg.reload.health_interval_ms);
        let app = self.cfg.app.name.clone();
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        let (pid, label) = (i.handle.pid, sb(i));
        let Some(st) = i.standby.as_mut() else { return };
        st.checking = false;
        match result {
            Ok(()) => {
                st.passes += 1;
                st.fails = 0;
                if st.passes < required {
                    self.send_later(every, Event::StandbyGateDue { inst: inst_id });
                } else {
                    self.standby_gates(inst_id);
                }
            }
            Err(e) => {
                st.passes = 0;
                st.fails += 1;
                debug!(
                    "standby gate health check failed",
                    worker = label,
                    pid = pid,
                    error = e,
                    consecutive = st.fails
                );
                if st.fails >= threshold * 3 {
                    warn!(
                        "standby keeps failing its health checks before promotion; replacing it",
                        worker = label,
                        pid = pid,
                        error = e,
                        hint = format!(
                            "the health path must answer once the app is initialized (a standby serves it on its \
                             private socket before listening); its output is in `warden logs {app} --worker {label}`"
                        ),
                    );
                    self.standby_failed(inst_id, format!("failed its health checks: {e}"));
                } else {
                    self.send_later(every, Event::StandbyGateDue { inst: inst_id });
                }
            }
        }
    }

    pub(super) fn on_standby_verified(&mut self, inst_id: u64, result: Result<(), String>) {
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        let (pid, label) = (i.handle.pid, sb(i));
        let Some(st) = i.standby.as_mut() else { return };
        st.checking = false;
        match result {
            Ok(()) => {
                st.verified = true;
                self.standby_gates(inst_id);
            }
            Err(e) => {
                warn!(
                    "standby failed reload.verify_command; replacing it",
                    worker = label,
                    pid = pid,
                    error = e,
                    hint = "the command gets the standby's private socket as WARDEN_WORKER_SOCKET (WARDEN_STANDBY=1); PORT reaches the running workers, not the standby",
                );
                self.standby_failed(inst_id, format!("verify_command failed: {e}"));
            }
        }
    }

    /// Periodic liveness check of an available standby.
    pub(super) fn on_standby_health(&mut self, inst_id: u64, result: Result<(), String>) {
        let threshold = self.cfg.health.failure_threshold;
        let outage = self.outage;
        let app = self.cfg.app.name.clone();
        let Some(i) = self.insts.get_mut(&inst_id) else { return };
        if i.stopping {
            return;
        }
        let (pid, label) = (i.handle.pid, sb(i));
        match result {
            Ok(()) => {
                if i.healthy == Some(false) {
                    info!("standby healthy again", worker = label, pid = pid);
                }
                i.healthy = Some(true);
                i.health_fails = 0;
            }
            Err(e) => {
                i.health_fails += 1;
                let fails = i.health_fails;
                if fails == 1 {
                    warn!(
                        "standby health check failed; it is not promoted while failing",
                        worker = label,
                        pid = pid,
                        error = e,
                        threshold = threshold,
                        hint = "it is replaced after `threshold` failures in a row ([health] failure_threshold)",
                    );
                }
                if fails >= threshold && i.healthy != Some(false) {
                    i.healthy = Some(false);
                    self.emit_worker(STANDBY_SLOT, WorkerEvent::Unhealthy, Some(pid), || {
                        Some(format!("failed {fails} health checks: {e} (standby)"))
                    });
                    if outage {
                        // Like workers (DS2): a shared dependency is down; don't churn.
                        debug!("standby replacement held: fleet-wide health failure", worker = label, pid = pid);
                    } else {
                        error!(
                            "standby unhealthy; replacing it",
                            worker = label,
                            pid = pid,
                            failures = fails,
                            error = e,
                            hint = format!(
                                "it went bad while idle (a lost connection, a timer); its output is in `warden logs \
                                 {app} --worker {label}`"
                            ),
                        );
                        self.standby_failed(inst_id, format!("failed {fails} health checks: {e}"));
                    }
                }
            }
        }
    }

    /// A standby that failed a gate or a check: stopped, and counted like a
    /// crash so a broken app can't make the pool spin.
    fn standby_failed(&mut self, inst_id: u64, reason: String) {
        let Some(i) = self.insts.get(&inst_id).filter(|i| !i.stopping) else { return };
        let uptime = i.started.elapsed();
        self.pool.crashes += 1;
        self.pool.last_exit = Some(reason);
        self.stop_instance(inst_id);
        self.standby_crashed(uptime);
    }

    /// Restart policy for the pool: backoff, or FAILED.
    fn standby_crashed(&mut self, uptime: Duration) {
        if self.shutting_down || self.stopped {
            return;
        }
        let policy = self.policy.clone();
        match self.pool.tracker.on_crash(&policy, Instant::now(), uptime) {
            Decision::RestartAfter(d) => {
                self.pool.token += 1;
                self.pool.waiting = true;
                let token = self.pool.token;
                info!(
                    "standby restarting",
                    worker = POOL,
                    in_ms = d.as_millis(),
                    attempt = self.pool.tracker.restarts_in_window()
                );
                self.emit_worker(STANDBY_SLOT, WorkerEvent::Restarting, None, || {
                    Some(format!("in_ms={} role=standby", d.as_millis()))
                });
                self.send_later(d, Event::StandbyDue { token });
            }
            Decision::GiveUp => {
                self.pool.failed_at = Some(Instant::now());
                let cooldown = self.cfg.restart.failed_cooldown;
                let retry = if !policy.enabled {
                    "restarts are disabled".to_string()
                } else if cooldown > 0 {
                    format!("retrying in {cooldown}s")
                } else {
                    "not retrying".into()
                };
                error!(
                    "standbys failed: too many standby crashes; crashed workers restart the normal way meanwhile",
                    worker = POOL,
                    max_restarts = policy.max_restarts,
                    window_s = policy.window.as_secs(),
                    last_exit = self.pool.last_exit.clone().unwrap_or_default(),
                    next = retry,
                    hint = format!(
                        "see `warden logs {} --worker standby`; fix the cause, then `warden reset {}`",
                        self.cfg.app.name, self.cfg.app.name
                    ),
                );
                self.emit_worker(STANDBY_SLOT, WorkerEvent::Failed, None, || {
                    Some(format!("too many standby restarts; {retry}"))
                });
            }
        }
    }

    pub(super) fn on_standby_due(&mut self, token: u64) {
        if token != self.pool.token {
            return;
        }
        self.pool.waiting = false;
        let before = self.live_standbys().len();
        self.fill_pool();
        self.pool.restarts += self.live_standbys().len().saturating_sub(before) as u64;
    }

    /// From the 1 s tick: the FAILED cooldown, and refills that had to wait
    /// (a worker was starting).
    pub(super) fn standby_tick(&mut self) {
        let cooldown = Duration::from_secs(self.cfg.restart.failed_cooldown);
        if let Some(t) = self.pool.failed_at {
            if !cooldown.is_zero() && self.cfg.restart.enabled && t.elapsed() >= cooldown {
                info!("retrying standbys after cooldown", worker = POOL, cooldown_s = cooldown.as_secs());
                self.pool.failed_at = None;
                self.pool.tracker.reset();
            }
        }
        self.replace_other_releases();
        self.fill_pool();
    }

    /// A standby started in another release than the workers' (the pin moved
    /// outside a deploy: its release directory was deleted, see
    /// `check_release`) is never promoted: replace it. Deploys replace
    /// standbys themselves (`standby_after_rollout`).
    fn replace_other_releases(&mut self) {
        if self.deploy_in_progress() {
            return;
        }
        let release = self.pinned_release();
        let stale: Vec<u64> = self
            .live_standbys()
            .into_iter()
            .filter(|id| self.insts.get(id).is_some_and(|i| i.release != release))
            .collect();
        for id in stale {
            info!(
                "replacing a standby: it runs another release than the workers",
                worker = self.insts.get(&id).map(sb).unwrap_or_default(),
                pid = self.insts.get(&id).map(|i| i.handle.pid).unwrap_or(0),
                release = release.as_ref().map(|r| r.display().to_string()).unwrap_or_default(),
            );
            self.stop_instance(id);
        }
    }

    /// `warden reset` (all workers): standby counters too, FAILED retried now.
    pub(super) fn standby_reset(&mut self) {
        self.pool.tracker.reset();
        self.pool.restarts = 0;
        self.pool.crashes = 0;
        self.pool.failed_at = None;
        self.pool.waiting = false;
        self.pool.token += 1;
        self.fill_pool();
    }

    /// `hint`: from the exit classification (the OOM killer, someone else's
    /// signal), as for workers.
    pub(super) fn on_standby_exit(&mut self, inst: &Instance, why: String, reason: String, hint: Option<&str>) {
        let (pid, label) = (inst.handle.pid, sb(inst));
        if inst.stopping || self.shutting_down || self.stopped {
            match hint {
                Some(h) => warn!("standby stopped", worker = label, pid = pid, reason = why, hint = h),
                None => info!("standby stopped", worker = label, pid = pid, reason = why),
            }
            self.emit_worker(STANDBY_SLOT, WorkerEvent::Stopped, Some(pid), || Some(why.clone()));
        } else {
            let uptime = inst.started.elapsed();
            let logs = format!("its output is in `warden logs {} --worker {label}`", self.cfg.app.name);
            warn!(
                "standby crashed",
                worker = label,
                pid = pid,
                reason = reason,
                uptime_s = uptime.as_secs(),
                hint = match hint {
                    Some(h) => format!("{h}; {logs}"),
                    None => format!("it had taken no traffic; {logs}"),
                },
            );
            // The reason leads the detail: alerts match e.g. `oom-killed`.
            self.emit_worker(STANDBY_SLOT, WorkerEvent::Crashed, Some(pid), || Some(format!("{reason} (standby)")));
            self.pool.crashes += 1;
            self.pool.last_exit = Some(reason);
            self.standby_crashed(uptime);
        }
        self.fill_pool();
    }

    /// Hand an available standby the slot `slot_id` (as its `Current`
    /// worker after a crash, or as a rollout's `Replacement`): it is told to
    /// listen and from then on is that worker. None when no standby can be.
    pub(super) fn promote_standby(&mut self, slot_id: usize, role: Role) -> Option<u64> {
        if self.standby_target() == 0 || self.shutting_down || self.stopped || self.deploy_in_progress() {
            return None;
        }
        // Healthy, through its gates, in the workers' release; the oldest
        // first (warmest JIT).
        let release = self.pinned_release();
        let id = self
            .insts
            .iter()
            .filter(|(_, i)| i.role == Role::Standby && !i.stopping && i.health_fails == 0 && i.healthy != Some(false))
            .filter(|(_, i)| i.standby.as_ref().is_some_and(|s| s.available) && i.release == release)
            .map(|(id, _)| *id)
            .min()?;
        let msg = format!("{{\"cmd\":\"promote\",\"worker\":{slot_id},\"count\":{}}}\n", self.count);
        let label = self.label(slot_id);
        let i = self.insts.get_mut(&id)?;
        let (pid, was) = (i.handle.pid, sb(i));
        if let Err(e) = i.handle.send(msg.as_bytes()) {
            warn!(
                "could not reach a standby to promote it; starting a new worker instead",
                worker = label,
                pid = pid,
                standby = was,
                error = e,
                hint = "the standby is replaced; if this repeats, please report it",
            );
            self.standby_failed(id, format!("promotion message failed: {e}"));
            return None;
        }
        i.role = role;
        i.slot = slot_id;
        i.standby = None;
        i.promoted_at = Some(Instant::now());
        i.ready_at = None;
        // Its heartbeats and socket were reported under the standby id (0).
        i.heartbeats.clear();
        let sockets: Vec<PathBuf> = std::mem::take(&mut i.sockets).into_values().collect();
        for s in sockets {
            i.sockets.insert(slot_id, s);
        }
        i.handle.relabel(&label);
        if role == Role::Current {
            if let Some(s) = self.slots.get_mut(&slot_id) {
                s.current = Some(id);
                s.state = State::Starting;
            }
        }
        self.emit_worker(slot_id, WorkerEvent::Starting, Some(pid), || Some("promoted from standby".into()));
        self.send_later(PROMOTE_TIMEOUT.min(self.cfg.ready_timeout()), Event::ReadyTimeout { inst: id });
        // Its successor starts once it listens (`mark_ready` → `fill_pool`).
        Some(id)
    }

    /// The new worker of a rollout step. Recycling keeps the code, so an
    /// available standby is a correct replacement (it skips the startup);
    /// anything else starts a fresh process.
    pub(super) fn start_replacement(&mut self, slot_id: usize, kind: Kind) -> std::io::Result<u64> {
        if kind == Kind::Replace {
            if let Some(id) = self.promote_standby(slot_id, Role::Replacement) {
                return Ok(id);
            }
        }
        self.spawn_instance(slot_id, Role::Replacement)
    }

    /// A rollout ended. After a successful deploy (reload, safe-reload, or
    /// restart of every worker) the standbys run the previous version:
    /// replace them. After a failure they match the workers that kept it.
    pub(super) fn standby_after_rollout(&mut self, kind: Kind, ok: bool, total: usize) {
        let all = match kind {
            Kind::Reload | Kind::SafeReload => true,
            Kind::Restart => total >= self.slots.len(),
            Kind::Replace | Kind::Recovery => false,
        };
        if ok && all {
            let old = self.live_standbys();
            if !old.is_empty() {
                info!("replacing standbys: they run the previous version", worker = POOL, standbys = old.len());
                for id in old {
                    self.stop_instance(id);
                }
            }
        }
        self.fill_pool();
    }

    /// `status.standbys`: the pool's standbys by number (`id`: `sN` in
    /// `warden status`, `worker=sN` in the log), with a row for each missing
    /// number when the pool can't be refilled.
    pub(super) fn standby_rows(&mut self, rows: &mut Vec<WorkerStatus>, sample: &Sampler<'_>) {
        let target = self.standby_target();
        if target == 0 {
            return;
        }
        let mut ids: Vec<u64> = self.insts.iter().filter(|(_, i)| i.role == Role::Standby).map(|(id, _)| *id).collect();
        ids.sort_unstable();
        let (restarts, crashes) = (self.pool.restarts, self.pool.crashes);
        let last_exit = self.pool.disabled.clone().or_else(|| self.pool.last_exit.clone());
        let row =
            |id: usize, state: &str, pid, uptime, stats: Option<(metrics::ProcStats, f64)>, healthy| WorkerStatus {
                id,
                state: state.into(),
                pid,
                uptime_secs: uptime,
                restarts,
                crashes,
                rss_bytes: stats.map(|x| x.0.rss_bytes),
                cpu_seconds: stats.map(|x| x.0.cpu_seconds),
                cpu_percent: stats.map(|x| x.1),
                last_exit: last_exit.clone(),
                healthy,
            };
        let first = rows.len();
        let mut live = std::collections::BTreeSet::new();
        for id in ids {
            let Some(i) = self.insts.get_mut(&id) else { continue };
            let state = if i.stopping {
                State::Stopping.as_str()
            } else if i.standby.as_ref().is_some_and(|s| s.available) {
                control::STANDBY
            } else {
                control::WARMING
            };
            let n = i.standby_number.unwrap_or(0);
            if !i.stopping {
                live.insert(n);
            }
            let (pid, started) = (i.handle.pid, i.started);
            let stats = sample(pid, &mut i.cpu_prev, started);
            rows.push(row(n, state, Some(pid), Some(started.elapsed().as_secs()), stats, i.healthy));
        }
        let missing = if self.pool.failed_at.is_some() {
            Some(State::Failed)
        } else if self.pool.waiting {
            Some(State::Restarting)
        } else if self.pool.disabled.is_some() || self.stopped {
            Some(State::Stopped)
        } else {
            None
        };
        if let Some(state) = missing {
            for n in (1..=target).filter(|n| !live.contains(n)) {
                rows.push(row(n, state.as_str(), None, None, None, None));
            }
        }
        // By number; a standby being replaced (older) before its successor.
        rows[first..].sort_by_key(|w| w.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fake app speaking the shim's fd-3 protocol. Workers report
    /// `listening`; standbys `standby_ready`, then wait for the promote line
    /// and report `listening`. Knobs: FAKE_STANDBY_EXIT (standbys exit 4),
    /// FAKE_STANDBY_LISTENS (standbys listen at once, as an app the shim
    /// can't hold back would), FAKE_STANDBY_DEAF (standbys ignore SIGTERM:
    /// only the end of fd 3 ends their read, like a Node standby's
    /// thread-pool read), FAKE_WORKER_EXIT (new workers exit 5).
    const FAKE: &str = r#"
if [ "$WARDEN_STANDBY" = 1 ]; then
  [ -n "$FAKE_STANDBY_EXIT" ] && exit 4
  [ -n "$FAKE_STANDBY_DEAF" ] && trap '' TERM
  if [ -z "$FAKE_STANDBY_LISTENS" ]; then
    echo '{"ev":"standby_ready","port":1}' >&3
    read -r line <&3 || exit 0
  fi
else
  [ -n "$FAKE_WORKER_EXIT" ] && exit 5
fi
echo '{"ev":"listening","port":1}' >&3
exec sleep 60
"#;

    struct Rig {
        sup: Supervisor,
        rx: mpsc::UnboundedReceiver<Event>,
        proc_rx: mpsc::UnboundedReceiver<ProcEvent>,
        dir: PathBuf,
    }

    impl Rig {
        /// One worker and one standby running FAKE; `extra` adds config sections.
        fn new(name: &str, extra: &str) -> Rig {
            let dir = std::env::temp_dir().join(format!("warden-pool-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // The control socket is never bound here; a short path keeps the
            // socket-length checks happy where temp_dir is long (macOS).
            let toml = format!(
                "[app]\nname = \"{name}\"\nargs = [\"x.js\"]\nport = 1\n[workers]\ncount = 1\nstandby = 1\n{extra}\n\
                 [control]\nsocket = \"/tmp/wp.sock\"\n"
            );
            let mut cfg = Config::parse(&toml).unwrap();
            cfg.app.command = "sh".into();
            cfg.app.args = vec!["-c".into(), FAKE.into()];
            let (tx, rx) = mpsc::unbounded_channel();
            let (proc_tx, proc_rx) = mpsc::unbounded_channel();
            let sup = Supervisor::new(cfg, None, dir.clone(), (None, None), tx, proc_tx);
            Rig { sup, rx, proc_rx, dir }
        }

        /// The supervisor's event loop, until `f` holds.
        async fn until(&mut self, what: &str, f: impl Fn(&Supervisor) -> bool) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !f(&self.sup) {
                tokio::select! {
                    Some(ev) = self.rx.recv() => self.sup.on_event(ev),
                    Some(pe) = self.proc_rx.recv() => self.sup.on_proc(pe),
                    _ = tokio::time::sleep_until(deadline) => panic!("timed out waiting for {what}"),
                }
            }
        }

        fn kill(&self, inst: u64) {
            self.sup.insts[&inst].handle.signal(libc::SIGKILL);
        }

        async fn shutdown(mut self) {
            self.sup.begin_shutdown("test");
            self.until("every process gone", |s| s.insts.is_empty()).await;
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn available(s: &Supervisor) -> Vec<u64> {
        let ok = |id: &u64| s.insts[id].standby.as_ref().is_some_and(|g| g.available);
        s.live_standbys().into_iter().filter(ok).collect()
    }

    fn running(s: &Supervisor) -> bool {
        s.slots.get(&1).is_some_and(|x| x.state == State::Running)
    }

    fn rows(s: &mut Supervisor) -> Vec<WorkerStatus> {
        s.status().standbys
    }

    async fn local(f: impl std::future::Future<Output = ()>) {
        tokio::task::LocalSet::new().run_until(f).await
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_crashed_worker_gets_the_standby_and_the_pool_refills() {
        local(async {
            let mut r = Rig::new("promote", "");
            r.sup.start_all();
            // Standbys start once the worker is up, not alongside it.
            assert!(r.sup.live_standbys().is_empty());
            r.until("worker and standby ready", |s| running(s) && available(s).len() == 1).await;
            let (worker, standby) = (r.sup.slots[&1].current.unwrap(), available(&r.sup)[0]);
            let states: Vec<String> = rows(&mut r.sup).into_iter().map(|w| w.state).collect();
            assert_eq!(states, [control::STANDBY]);

            r.kill(worker);
            r.until("standby promoted", |s| running(s) && s.slots[&1].current == Some(standby)).await;
            let i = &r.sup.insts[&standby];
            assert_eq!((i.role, i.slot), (Role::Current, 1));
            assert!(i.promoted_at.is_some() && i.standby.is_none() && i.ready_at.is_some());
            let slot = &r.sup.slots[&1];
            assert_eq!((slot.restarts, slot.crashes), (1, 1), "a promotion is the slot's restart");
            assert_eq!(slot.tracker.restarts_in_window(), 1, "and counts towards its restart limit");

            r.until("a fresh standby", |s| available(s).len() == 1).await;
            assert_ne!(available(&r.sup)[0], standby);
            r.shutdown().await;
        })
        .await;
    }

    /// Standbys are numbered s1, s2… (`warden status` rows, `worker=` on
    /// their log lines and output): a new one takes the lowest number no
    /// live standby has, so the numbers stay 1..=standby.
    #[tokio::test(flavor = "current_thread")]
    async fn standbys_are_numbered_and_a_successor_takes_the_free_number() {
        local(async {
            let mut r = Rig::new("numbers", "");
            r.sup.cfg.workers.standby = 2;
            r.sup.start_all();
            r.until("worker and two standbys ready", |s| running(s) && available(s).len() == 2).await;
            let (a, b) = (available(&r.sup)[0], available(&r.sup)[1]);
            let label = |s: &Supervisor, id: u64| s.inst_label(&s.insts[&id]);
            assert_eq!((label(&r.sup, a), label(&r.sup, b)), ("s1".into(), "s2".into()));
            let ids: Vec<usize> = rows(&mut r.sup).iter().map(|w| w.id).collect();
            assert_eq!(ids, [1, 2]);

            // s1 (the oldest) takes the dead worker's slot; its successor is s1.
            r.kill(r.sup.slots[&1].current.unwrap());
            r.until("s1 promoted", |s| running(s) && s.slots[&1].current == Some(a)).await;
            assert_eq!(label(&r.sup, a), "1", "a promoted standby is its slot's worker");
            assert_eq!(r.sup.insts[&a].standby_number, Some(1), "and remembers which standby it was");
            r.until("a fresh standby", |s| available(s).len() == 2).await;
            let new = available(&r.sup).into_iter().find(|id| *id != b).unwrap();
            assert_eq!((label(&r.sup, new), label(&r.sup, b)), ("s1".into(), "s2".into()));
            let ids: Vec<usize> = rows(&mut r.sup).iter().map(|w| w.id).collect();
            assert_eq!(ids, [1, 2]);

            // One being replaced keeps its number until it is gone; its
            // successor gets the same one, as a rollout's new worker does.
            r.sup.stop_instance(b);
            assert_eq!(r.sup.free_standby_number(), 2);
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn crashing_standbys_back_off_then_fail_and_workers_restart_cold() {
        local(async {
            let mut r = Rig::new(
                "crashloop",
                "[restart]\nbackoff_initial = 20\nbackoff_max = 1000\nmax_restarts = 3\nfailed_cooldown = 0\n",
            );
            r.sup.cfg.app.env.insert("FAKE_STANDBY_EXIT".into(), "1".into());
            let t0 = Instant::now();
            r.sup.start_all();
            r.until("standbys FAILED", |s| s.pool.failed_at.is_some()).await;
            // 4 starts: at once, then after 20 and 40 ms; never more.
            assert_eq!((r.sup.pool.crashes, r.sup.pool.restarts), (4, 3));
            assert!(t0.elapsed() >= Duration::from_millis(60), "backoff between restarts: {:?}", t0.elapsed());
            let failed = rows(&mut r.sup);
            assert_eq!(failed.len(), 1);
            assert_eq!((failed[0].id, failed[0].state.as_str()), (1, "FAILED"), "s1, FAILED");
            assert!(failed[0].last_exit.as_deref().unwrap_or("").contains("exit code 4"), "{failed:?}");
            r.sup.fill_pool();
            assert!(r.sup.live_standbys().is_empty(), "nothing starts while FAILED");

            // Without a standby a crash restarts the slot cold.
            let worker = r.sup.slots[&1].current.unwrap();
            r.kill(worker);
            r.until("worker 1 back", |s| running(s) && s.slots[&1].current != Some(worker)).await;
            let now = r.sup.slots[&1].current.unwrap();
            assert!(r.sup.insts[&now].promoted_at.is_none());

            // `warden reset` retries them.
            r.sup.standby_reset();
            assert!(r.sup.pool.failed_at.is_none() && r.sup.pool.crashes == 0);
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn deploys_pause_the_pool_and_replace_it_only_when_they_succeed() {
        local(async {
            let mut r = Rig::new("deploy", "");
            r.sup.start_all();
            r.until("worker and standby ready", |s| running(s) && available(s).len() == 1).await;
            let old = available(&r.sup)[0];

            // A failing deploy (new workers exit): rolled back, and the
            // standby (previous version, like the workers) stays.
            r.sup.cfg.app.env.insert("FAKE_WORKER_EXIT".into(), "1".into());
            r.sup.begin_rollout(Kind::Reload, vec![1], String::new(), false).unwrap();
            assert!(r.sup.deploy_in_progress());
            assert_eq!(r.sup.promote_standby(1, Role::Current), None, "no promotion during a deploy");
            r.until("reload failed", |s| s.roll.is_none()).await;
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(false));
            assert_eq!(available(&r.sup), [old]);

            // A good one: the standby is replaced afterwards.
            r.sup.cfg.app.env.remove("FAKE_WORKER_EXIT");
            r.sup.begin_rollout(Kind::Reload, vec![1], String::new(), false).unwrap();
            r.until("reload done", |s| s.roll.is_none()).await;
            assert_eq!(r.sup.last_rollout.as_ref().map(|o| o.ok), Some(true));
            assert!(r.sup.insts.get(&old).is_none_or(|i| i.stopping), "the old standby is retired");
            r.until("a fresh standby", |s| available(s).len() == 1 && available(s)[0] != old).await;
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recycling_uses_the_standby_as_the_replacement() {
        local(async {
            // (A real standby config has the shim, so workers overlap; `sh` doesn't.)
            let mut r = Rig::new("recycle", "overlap = true\n");
            r.sup.start_all();
            r.until("worker and standby ready", |s| running(s) && available(s).len() == 1).await;
            let (worker, standby) = (r.sup.slots[&1].current.unwrap(), available(&r.sup)[0]);
            r.sup.request_replace(1, "test".into(), false);
            r.until("replaced", |s| s.roll.is_none() && s.slots[&1].current == Some(standby)).await;
            let outcome = r.sup.last_rollout.clone().map(|o| (o.kind, o.ok));
            assert_eq!(outcome, Some(("replace".to_string(), true)));
            assert!(r.sup.insts.get(&worker).is_none_or(|i| i.stopping), "the old worker drains");
            assert_eq!(r.sup.slots[&1].crashes, 0);
            r.shutdown().await;
        })
        .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_standby_that_listens_early_turns_standbys_off() {
        local(async {
            let mut r = Rig::new("early", "");
            r.sup.cfg.app.env.insert("FAKE_STANDBY_LISTENS".into(), "1".into());
            r.sup.start_all();
            r.until("standbys disabled", |s| s.pool.disabled.is_some()).await;
            r.until("its standby gone", |s| s.live_standbys().is_empty() && s.insts.len() == 1).await;
            r.sup.fill_pool();
            assert!(r.sup.live_standbys().is_empty(), "no more standbys");
            let disabled = rows(&mut r.sup);
            assert_eq!(disabled.len(), 1);
            assert_eq!(disabled[0].state, "STOPPED");
            assert!(disabled[0].last_exit.as_deref().unwrap_or("").contains("before its promotion"), "{disabled:?}");
            r.shutdown().await;
        })
        .await;
    }

    /// Stopping a standby ends its pending read of fd 3 (EOF), so a reader
    /// its stop signal can't interrupt never holds it until the grace
    /// period's SIGKILL (30 s here).
    #[tokio::test(flavor = "current_thread")]
    async fn a_stopped_standby_is_not_held_by_its_pending_read() {
        local(async {
            let mut r = Rig::new("deaf", "[shutdown]\ngrace_period = 30\n");
            r.sup.cfg.app.env.insert("FAKE_STANDBY_DEAF".into(), "1".into());
            r.sup.start_all();
            r.until("worker and standby ready", |s| running(s) && available(s).len() == 1).await;
            let standby = available(&r.sup)[0];
            let t0 = Instant::now();
            r.sup.stop_instance(standby);
            r.until("the standby exited", |s| !s.insts.contains_key(&standby)).await;
            assert!(t0.elapsed() < Duration::from_secs(2), "took {:?}", t0.elapsed());
            r.shutdown().await;
        })
        .await;
    }

    /// A promoted standby runs the workers' release: one started in another
    /// (the pin moved outside a deploy) is never promoted, and is replaced
    /// by one in the pinned release.
    #[tokio::test(flavor = "current_thread")]
    async fn only_a_standby_in_the_pinned_release_is_promoted() {
        local(async {
            let mut r = Rig::new("pin", "");
            r.sup.start_all();
            r.until("worker and standby ready", |s| running(s) && available(s).len() == 1).await;
            let old = available(&r.sup)[0];
            assert_eq!(r.sup.insts[&old].release, None, "unpinned (no working_directory)");

            // The workers' release is now another one.
            let release = std::env::temp_dir().join(format!("warden-pool-pin-release-{}", std::process::id()));
            std::fs::create_dir_all(&release).unwrap();
            r.sup.release = Some(release::Pin::resolve(&release).unwrap());
            let real = r.sup.release.as_ref().map(|p| p.real.clone());
            assert_eq!(r.sup.promote_standby(1, Role::Current), None, "not in the workers' release");
            r.sup.standby_tick();
            assert!(r.sup.insts[&old].stopping, "replaced");
            r.until("a standby in the pinned release", |s| available(s).len() == 1 && available(s)[0] != old).await;
            let new = available(&r.sup)[0];
            assert_eq!(r.sup.insts[&new].release, real);

            let worker = r.sup.slots[&1].current.unwrap();
            r.kill(worker);
            r.until("promoted", |s| running(s) && s.slots[&1].current == Some(new)).await;
            r.shutdown().await;
            let _ = std::fs::remove_dir_all(&release);
        })
        .await;
    }
}

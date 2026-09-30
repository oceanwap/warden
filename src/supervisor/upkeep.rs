//! The 1 s maintenance tick: keeping workers alive and fresh over weeks.
//!
//! - Watchdog (systemd `WatchdogSec`, gunicorn `timeout`): the shim sends a
//!   heartbeat from each worker's event loop; silence = hung = killed.
//! - Per-worker health (Kubernetes liveness): checks over each worker's
//!   private socket; a worker failing `failure_threshold` checks in a row is
//!   replaced gracefully.
//! - Recycling (PM2 `max_memory_restart`, gunicorn `max_requests` + jitter):
//!   `max_memory` and `max_lifetime` trigger a graceful replacement.
//! - FAILED cooldown (Kubernetes CrashLoopBackOff): a FAILED worker is retried
//!   after `failed_cooldown`, so a transient outage doesn't need a human.
//!
//! Graceful replacements are queued and run one at a time as `Replace` rollouts.

use super::*;

impl Supervisor {
    pub(super) fn on_tick(&mut self) {
        crate::guard::fault("tick");
        self.ticks += 1;
        self.systemd_watchdog();
        if self.shutting_down || self.stopped {
            return;
        }
        self.watchdog();
        if self.cfg.limits.max_memory > 0 && self.ticks % 5 == 0 {
            self.check_memory();
        }
        self.check_lifetime();
        self.retry_failed();
        if self.cfg.health.enabled && self.ticks % self.cfg.health.interval.max(1) == 0 {
            self.check_workers_health();
        }
        self.process_pending();
    }

    /// CP2: tell systemd we're alive (`WatchdogSec=`), but only while the event
    /// loop keeps up: ticks arriving more than 2 s late mean something blocks
    /// it (a stalled stdout, a bug), and systemd should restart us.
    fn systemd_watchdog(&mut self) {
        let now = Instant::now();
        let gap = now.duration_since(self.last_tick);
        self.last_tick = now;
        if gap < Duration::from_secs(3) {
            if self.watchdog_enabled {
                systemd::notify("WATCHDOG=1");
            }
        } else {
            warn!("event loop was blocked", for_ms = gap.as_millis().saturating_sub(1000));
        }
    }

    /// Queue a graceful (new first, then drain old) replacement of one worker.
    pub(super) fn request_replace(&mut self, slot: usize, reason: String, kill_old: bool) {
        if self.shutting_down || self.stopped || !self.slots.contains_key(&slot) {
            return;
        }
        if self.pending_replace.contains_key(&slot) || self.rollout_replacing(slot) {
            return;
        }
        info!("worker scheduled for replacement", worker = self.label(slot), reason = reason);
        self.pending_replace.insert(slot, (reason, kill_old));
        self.process_pending();
    }

    fn process_pending(&mut self) {
        while self.roll.is_none() {
            let Some((&slot, _)) = self.pending_replace.iter().next() else { return };
            let Some((reason, kill_old)) = self.pending_replace.remove(&slot) else { return };
            if !self.slots.contains_key(&slot) {
                continue;
            }
            match self.begin_rollout(Kind::Replace, vec![slot], reason, kill_old) {
                Ok(_) => return,
                Err(e) => warn!("replacement skipped", worker = self.label(slot), reason = e),
            }
        }
    }

    fn watchdog(&mut self) {
        let timeout = Duration::from_secs(self.cfg.watchdog.timeout);
        if timeout.is_zero() {
            return;
        }
        let now = Instant::now();
        let hung: Vec<(u64, usize, Duration)> = self
            .insts
            .iter()
            .filter(|(_, i)| !i.stopping && !i.hung && i.ready_at.is_some())
            .filter_map(|(id, i)| {
                i.heartbeats
                    .iter()
                    .find(|(_, t)| now.duration_since(**t) > timeout)
                    .map(|(w, t)| (*id, *w, now.duration_since(*t)))
            })
            .collect();
        for (id, worker, silent) in hung {
            let worker_mode = self.is_worker_mode();
            let Some(i) = self.insts.get_mut(&id) else { continue };
            i.hung = true;
            let (slot, role, pid) = (i.slot, i.role, i.handle.pid);
            error!(
                "worker hung: no heartbeat from its event loop",
                worker = worker,
                pid = pid,
                silent_s = silent.as_secs()
            );
            if worker_mode && role == Role::Current {
                // Can't kill one thread: replace the host, then SIGKILL the old one.
                self.on_thread_crash(slot, true);
            } else {
                // Crash handling restarts it; a hung process can't drain anyway.
                i.handle.signal(libc::SIGKILL);
            }
        }
    }

    fn check_memory(&mut self) {
        let limit = self.cfg.limits.max_memory * 1024 * 1024;
        let mut over = Vec::new();
        for i in self.insts.values_mut() {
            if i.role != Role::Current || i.stopping || i.ready_at.is_none() {
                continue;
            }
            let Some(st) = metrics::proc_stats(i.handle.pid) else { continue };
            if st.rss_bytes > limit {
                i.mem_strikes = i.mem_strikes.saturating_add(1);
                // Three samples (15 s) in a row: not just a request spike.
                // Reset so a failed replacement is retried 15 s later.
                if i.mem_strikes >= 3 {
                    i.mem_strikes = 0;
                    over.push((i.slot, st.rss_bytes / 1024 / 1024));
                }
            } else {
                i.mem_strikes = 0;
            }
        }
        for (slot, mb) in over {
            let reason = format!("memory {mb} MB > max_memory {} MB", self.cfg.limits.max_memory);
            self.request_replace(slot, reason, false);
        }
    }

    fn check_lifetime(&mut self) {
        let now = Instant::now();
        // If the replacement fails, try again later rather than never.
        let retry = Duration::from_secs((self.cfg.limits.max_lifetime / 10).max(60));
        let mut due = Vec::new();
        for i in self.insts.values_mut() {
            if i.role == Role::Current && !i.stopping && i.recycle_at.is_some_and(|t| now >= t) {
                i.recycle_at = Some(crate::restart::later(now, retry));
                due.push(i.slot);
            }
        }
        for slot in due {
            self.request_replace(slot, "max_lifetime reached".into(), false);
        }
    }

    fn retry_failed(&mut self) {
        let cooldown = Duration::from_secs(self.cfg.restart.failed_cooldown);
        if cooldown.is_zero() || !self.cfg.restart.enabled {
            return;
        }
        let now = Instant::now();
        let due: Vec<usize> = self
            .slots
            .values()
            .filter(|s| s.state == State::Failed && s.failed_at.is_some_and(|t| now.duration_since(t) >= cooldown))
            .map(|s| s.id)
            .collect();
        for id in due {
            info!("retrying failed worker after cooldown", worker = self.label(id), cooldown_s = cooldown.as_secs());
            let Some(s) = self.slots.get_mut(&id) else { continue };
            s.tracker.reset();
            s.failed_at = None;
            s.token += 1;
            if s.current.is_some() {
                // Worker mode: degraded host still serving. Go through the
                // normal recovery path so a failed attempt backs off again.
                s.state = State::Restarting;
                let token = s.token;
                let _ = self.tx.send(Event::RestartDue { slot: id, token });
            } else {
                s.restarts += 1;
                self.spawn_current(id);
            }
        }
    }

    fn check_workers_health(&mut self) {
        let Some(path) = self.cfg.live_path() else { return };
        let timeout = Duration::from_secs(self.cfg.health.timeout);
        let initial_delay = Duration::from_secs(self.cfg.health.initial_delay);
        for (id, i) in self.insts.iter_mut() {
            if i.role != Role::Current || i.stopping || i.health_inflight || i.sockets.is_empty() {
                continue;
            }
            // W9: give a freshly started worker time to warm up.
            if i.ready_at.is_none_or(|t| t.elapsed() < initial_delay) {
                continue;
            }
            i.health_inflight = true;
            let sockets: Vec<PathBuf> = i.sockets.values().cloned().collect();
            let (inst, path, tx) = (*id, path.clone(), self.tx.clone());
            tokio::task::spawn_local(async move {
                // A failed check task must still answer, or the worker is never checked again.
                let result = crate::guard::catch_unwind(rollout::check_instance_sockets(sockets, path, timeout))
                    .await
                    .unwrap_or_else(|p| Err(format!("internal error in the health check: {p}")));
                let _ = tx.send(Event::WorkerHealth { inst, result });
            });
        }
    }
}

/// `d` ± 10%, so workers started together don't all recycle together.
pub(super) fn jittered(d: Duration, seed: u64) -> Duration {
    let span = d.as_millis() as u64 / 10;
    if span == 0 {
        return d;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos() as u64)
        .unwrap_or(0);
    let x = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407) ^ nanos;
    Duration::from_millis(d.as_millis() as u64 - span + x % (2 * span + 1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_ten_percent() {
        let d = Duration::from_secs(1000);
        for seed in 0..200 {
            let j = jittered(d, seed);
            assert!(j >= Duration::from_secs(900) && j <= Duration::from_secs(1100), "{j:?}");
        }
        assert_eq!(jittered(Duration::from_millis(5), 1), Duration::from_millis(5));
    }
}

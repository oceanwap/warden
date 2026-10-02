//! Host metrics for `host` events and the history: CPU ticks, memory in use
//! and load, read through the OS adapter (`platform::host_snapshot`: /proc on
//! Linux, Mach calls on macOS). Sampled every second while someone subscribes,
//! else once per history sample (10 s). An OS without an adapter: no host
//! metrics.

use crate::events::Event;
use crate::platform::CpuTimes;
/// Busy share between two samples (since boot without a previous one), in
/// percent of all CPUs, rounded to 0.1.
pub(crate) fn cpu_percent(prev: Option<CpuTimes>, now: CpuTimes) -> f64 {
    let (busy, total) = match prev {
        Some(p) => (now.busy.saturating_sub(p.busy), now.total.saturating_sub(p.total)),
        None => (now.busy, now.total),
    };
    if total == 0 {
        return 0.0;
    }
    (busy as f64 / total as f64 * 1000.0).round() / 10.0
}

/// Keeps the previous CPU sample.
#[derive(Default)]
pub(crate) struct Sampler {
    prev: Option<CpuTimes>,
}

impl Sampler {
    /// One `host` event, or `None` when the OS adapter cannot read the host.
    pub fn sample(&mut self) -> Option<Event> {
        let snap = crate::platform::host_snapshot()?;
        let cpu_percent = cpu_percent(self.prev, snap.cpu);
        self.prev = Some(snap.cpu);
        Some(Event::Host {
            cpu_percent,
            mem_used_bytes: snap.mem_used_bytes,
            mem_total_bytes: snap.mem_total_bytes,
            load: snap.load,
            at_ms: crate::events::now_ms(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_share_between_two_samples() {
        let t = CpuTimes { busy: 5656, total: 3_704_855 };
        let later = CpuTimes { busy: t.busy + 50, total: t.total + 200 };
        assert_eq!(cpu_percent(Some(t), later), 25.0);
        assert_eq!(cpu_percent(Some(t), t), 0.0, "no time passed");
        assert!(cpu_percent(None, t) < 1.0, "since boot");
        assert_eq!(cpu_percent(Some(later), t), 0.0, "a counter that went back is not negative");
    }

    #[test]
    fn samples_this_host() {
        let mut s = Sampler::default();
        if !crate::platform::current().capabilities().host_stats {
            assert!(s.sample().is_none());
            return;
        }
        let Some(Event::Host { cpu_percent, mem_used_bytes, mem_total_bytes, .. }) = s.sample() else {
            panic!("the adapter reports host stats but gave none")
        };
        assert!((0.0..=100.0).contains(&cpu_percent));
        assert!(mem_used_bytes <= mem_total_bytes && mem_total_bytes > 0);
        // The second sample is a share of the time in between.
        let Some(Event::Host { cpu_percent, .. }) = s.sample() else { panic!("second sample") };
        assert!((0.0..=100.0).contains(&cpu_percent));
    }
}

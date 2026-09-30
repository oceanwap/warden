//! Host metrics for `host` events: CPU from /proc/stat, memory from
//! /proc/meminfo (MemTotal - MemAvailable), load from /proc/loadavg.
//! Sampled only while someone subscribes.

use crate::events::Event;
use std::io::Read;

/// Jiffies since boot: all of them, and the busy ones (not idle or iowait).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// The aggregate `cpu` line of /proc/stat.
pub(crate) fn parse_cpu(stat: &str) -> Option<CpuTimes> {
    let line = stat.lines().find(|l| l.starts_with("cpu "))?;
    // user nice system idle iowait irq softirq steal (guest time is already in user).
    let mut v = [0u64; 8];
    let mut n = 0;
    for (slot, field) in v.iter_mut().zip(line.split_whitespace().skip(1)) {
        *slot = field.parse().ok()?;
        n += 1;
    }
    if n < 4 {
        return None;
    }
    let total: u64 = v.iter().sum();
    let idle = v[3] + v[4];
    Some(CpuTimes { busy: total.saturating_sub(idle), total })
}

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

/// (used, total) bytes. Kernels before 3.14 have no MemAvailable: then
/// MemFree + Buffers + Cached.
pub(crate) fn parse_meminfo(text: &str) -> Option<(u64, u64)> {
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name) && l[name.len()..].starts_with(':'))?;
        line[name.len() + 1..].split_whitespace().next()?.parse::<u64>().ok().map(|kb| kb * 1024)
    };
    let total = field("MemTotal")?;
    let available = match field("MemAvailable") {
        Some(a) => a,
        None => field("MemFree")? + field("Buffers").unwrap_or(0) + field("Cached").unwrap_or(0),
    };
    Some((total.saturating_sub(available), total))
}

pub(crate) fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut it = text.split_whitespace();
    let mut next = || it.next()?.parse::<f64>().ok();
    Some([next()?, next()?, next()?])
}

/// Keeps the previous CPU sample and one read buffer.
#[derive(Default)]
pub(crate) struct Sampler {
    prev: Option<CpuTimes>,
    buf: String,
}

impl Sampler {
    /// Forget the previous sample (nobody listened for a while).
    pub fn reset(&mut self) {
        self.prev = None;
    }

    fn read(&mut self, path: &str) -> Option<&str> {
        self.buf.clear();
        std::fs::File::open(path).ok()?.read_to_string(&mut self.buf).ok()?;
        Some(&self.buf)
    }

    pub fn sample(&mut self) -> Option<Event> {
        let cpu = parse_cpu(self.read("/proc/stat")?)?;
        let cpu_percent = cpu_percent(self.prev, cpu);
        self.prev = Some(cpu);
        let (mem_used_bytes, mem_total_bytes) = parse_meminfo(self.read("/proc/meminfo")?)?;
        let load = parse_loadavg(self.read("/proc/loadavg")?)?;
        Some(Event::Host { cpu_percent, mem_used_bytes, mem_total_bytes, load, at_ms: crate::events::now_ms() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAT: &str = "cpu  4705 356 584 3699176 23 0 11 0 0 0\n\
                        cpu0 1393280 32966 572056 13343292 6130 0 17875 0 23933 0\n\
                        intr 114930548 113199788 3 0 5 263 0 4 [...]\n";

    #[test]
    fn cpu() {
        let t = parse_cpu(STAT).unwrap();
        assert_eq!(t.total, 4705 + 356 + 584 + 3699176 + 23 + 11);
        assert_eq!(t.busy, 4705 + 356 + 584 + 11);
        let later = CpuTimes { busy: t.busy + 50, total: t.total + 200 };
        assert_eq!(cpu_percent(Some(t), later), 25.0);
        assert_eq!(cpu_percent(Some(t), t), 0.0, "no time passed");
        assert!(cpu_percent(None, t) < 1.0, "since boot");
        assert_eq!(parse_cpu("cpu  1 2\n"), None);
        assert_eq!(parse_cpu("cpu0 1 2 3 4\n"), None, "only the aggregate line counts");
        assert_eq!(parse_cpu("cpu  1 x 3 4\n"), None);
    }

    #[test]
    fn meminfo() {
        let text = "MemTotal:        8000000 kB\nMemFree:          100000 kB\nMemAvailable:    6000000 kB\n\
                    Buffers:           50000 kB\nCached:           900000 kB\n";
        assert_eq!(parse_meminfo(text), Some((2_000_000 * 1024, 8_000_000 * 1024)));
        let old = "MemTotal: 1000 kB\nMemFree: 100 kB\nBuffers: 50 kB\nCached: 250 kB\n";
        assert_eq!(parse_meminfo(old), Some((600 * 1024, 1000 * 1024)));
        assert_eq!(parse_meminfo("MemTotalX: 5 kB\n"), None);
    }

    #[test]
    fn loadavg() {
        assert_eq!(parse_loadavg("0.52 0.58 0.59 1/467 12345\n"), Some([0.52, 0.58, 0.59]));
        assert_eq!(parse_loadavg("0.52\n"), None);
    }

    #[test]
    fn samples_this_host() {
        let mut s = Sampler::default();
        let Some(Event::Host { cpu_percent, mem_used_bytes, mem_total_bytes, .. }) = s.sample() else {
            panic!("no /proc here?")
        };
        assert!((0.0..=100.0).contains(&cpu_percent));
        assert!(mem_used_bytes <= mem_total_bytes && mem_total_bytes > 0);
    }
}

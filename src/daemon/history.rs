//! wardend's resource history (docs/protocol.md, "Resource history"): the
//! last 24 h of each app's CPU, memory, workers and restarts, and of the
//! host's CPU, memory and load, in memory.
//!
//! - **Sampling.** Nothing is polled for it: each app's statuses (every
//!   second, from its watcher) and the host metrics (every second while a
//!   client subscribes, else every 10 s) go into an accumulator, and every
//!   10 s of wall-clock time (`STEP_S`) one sample per series is committed.
//!   A sample covers `[t, t + 10 s)`, `t` a multiple of 10 s.
//! - **Storage.** A fixed ring of `CAP` = 8640 samples (24 h) per app and
//!   one for the host, in 16-byte records (f32 / u32 / u16), plus one ring
//!   of sample times (u32 Unix seconds) they all share. Rings fill as time
//!   passes and never grow past `CAP`: at most `APP_BYTES` (135 KiB) per app
//!   and `BASE_BYTES` (169 KiB) for the host and the times, for at most
//!   `MAX_APPS` apps. A test checks it.
//! - **Life.** The series of an app is kept by name, so it survives the
//!   app's restarts (and wardend restarting its supervisor), until 24 h
//!   pass without a sample of it.
//! - **Disk** (`disk.rs`). A snapshot every minute and at a clean exit,
//!   read back at start, so the history survives wardend's own restarts.
//! - **Queries** downsample to `step_s`: CPU and load are the mean of a
//!   point's samples, memory the max, workers ready the min, workers
//!   configured the max, restarts the sum.

mod disk;

pub(crate) use disk::{Saver, load, path, save_every};

use crate::control::Status;
use std::collections::BTreeMap;
use std::sync::Arc;
use warden_protocol::events::{AppHistory, HostHistory, ResourceHistory, Usage, restarts_since};

/// Seconds per sample.
pub(crate) const STEP_S: u64 = 10;
/// Samples per series: 24 h.
pub(crate) const CAP: usize = 24 * 3600 / STEP_S as usize;
/// Apps with a series at most. Past it, the series of the app gone the
/// longest makes room (one still sampled is never dropped).
pub(crate) const MAX_APPS: usize = 128;
/// Bytes per app at most (its full ring).
pub(crate) const APP_BYTES: usize = CAP * std::mem::size_of::<AppSample>();
/// Bytes for the host's ring and the sample times.
#[cfg(test)]
pub(crate) const BASE_BYTES: usize = CAP * (std::mem::size_of::<HostSample>() + std::mem::size_of::<u32>());
/// A reply holds at most this many numbers: with many apps at a small
/// step, the step is raised (the reply says which one was used).
const MAX_VALUES: usize = 1_000_000;
/// A ring grows by this many samples (1 h) at a time.
const GROW: usize = 360;

// ----------------------------------------------------------------- records

/// One app's 10 s. `cpu` is NaN when nothing reported it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct AppSample {
    cpu: f32,
    mem_kib: u32,
    ready: u16,
    configured: u16,
    restarts: u16,
    /// `PRESENT`: the app was watched; `MEM`: memory was reported.
    flags: u16,
}

const PRESENT: u16 = 1;
const MEM: u16 = 2;

impl AppSample {
    const ABSENT: AppSample = AppSample { cpu: f32::NAN, mem_kib: 0, ready: 0, configured: 0, restarts: 0, flags: 0 };
}

/// The host's 10 s. `cpu` NaN: no sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct HostSample {
    cpu: f32,
    load1: f32,
    mem_used_kib: u32,
    mem_total_kib: u32,
}

impl HostSample {
    const ABSENT: HostSample = HostSample { cpu: f32::NAN, load1: f32::NAN, mem_used_kib: 0, mem_total_kib: 0 };
}

fn kib(bytes: u64) -> u32 {
    u32::try_from(bytes.div_ceil(1024)).unwrap_or(u32::MAX)
}

fn sat16(n: u64) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

/// A ring of at most `CAP` items, grown an hour at a time.
#[derive(Debug)]
struct Ring<T> {
    buf: Vec<T>,
    /// Index of the oldest item once full.
    head: usize,
}

impl<T> Default for Ring<T> {
    fn default() -> Self {
        Ring { buf: Vec::new(), head: 0 }
    }
}

impl<T: Copy> Ring<T> {
    fn push(&mut self, v: T) {
        if self.buf.len() < CAP {
            if self.buf.len() == self.buf.capacity() {
                self.buf.reserve_exact(GROW.min(CAP - self.buf.len()));
            }
            self.buf.push(v);
        } else {
            self.buf[self.head] = v;
            self.head = (self.head + 1) % CAP;
        }
    }

    fn len(&self) -> usize {
        self.buf.len()
    }

    /// `i`: 0 is the oldest.
    fn get(&self, i: usize) -> T {
        self.buf[(self.head + i) % self.buf.len()]
    }

    #[cfg(test)]
    fn heap_bytes(&self) -> usize {
        self.buf.capacity() * std::mem::size_of::<T>()
    }
}

// ------------------------------------------------------------ accumulators

#[derive(Debug, Default)]
struct AppAcc {
    n: u32,
    cpu_sum: f64,
    cpu_n: u32,
    mem_max: Option<u64>,
    ready_min: u16,
    configured_max: u16,
    restarts: u64,
}

#[derive(Debug, Default)]
struct HostAcc {
    n: u32,
    cpu_sum: f64,
    load_sum: f64,
    mem_used_max: u64,
    mem_total: u64,
}

#[derive(Debug, Default)]
struct AppSeries {
    ring: Ring<AppSample>,
    acc: AppAcc,
    /// (supervisor pid, its restart counters) at the last status.
    last_restarts: Option<(u32, u64)>,
    /// wardend's restarts of the supervisor at the last status.
    last_sup_restarts: Option<u32>,
    /// `seq` of the last commit that had a sample of it.
    last_present: u64,
}

/// Every series. Owned by wardend's core (one thread).
#[derive(Debug, Default)]
pub(crate) struct Store {
    /// Start of each committed interval, Unix seconds; every app ring ends
    /// at the newest one (an app's ring may be shorter: it began later).
    times: Ring<u32>,
    host: Ring<HostSample>,
    host_acc: HostAcc,
    apps: BTreeMap<Arc<str>, AppSeries>,
    /// Commits so far.
    seq: u64,
    /// The interval being accumulated (Unix seconds, a multiple of STEP_S).
    open: Option<u64>,
    full_logged: bool,
}

impl Store {
    /// An app's status. `sup_restarts`: wardend's restarts of its supervisor.
    pub fn observe_app(&mut self, app: &Arc<str>, s: &Status, sup_restarts: u32) {
        if !self.apps.contains_key(app) {
            if !self.make_room() {
                return;
            }
            #[cfg(debug_assertions)]
            self.prefill_for_tests(app);
        }
        let seq = self.seq;
        let a = self.apps.entry(app.clone()).or_insert_with(|| AppSeries { last_present: seq, ..Default::default() });
        let u = Usage::of(s);
        let mut restarts = restarts_since(a.last_restarts, s.pid, u.restarts);
        a.last_restarts = Some((s.pid, u.restarts));
        if let Some(before) = a.last_sup_restarts {
            restarts += u64::from(sup_restarts.saturating_sub(before));
        }
        a.last_sup_restarts = Some(sup_restarts);
        let acc = &mut a.acc;
        if let Some(c) = u.cpu_percent.filter(|c| c.is_finite()) {
            acc.cpu_sum += c;
            acc.cpu_n += 1;
        }
        if let Some(m) = u.rss_bytes {
            acc.mem_max = Some(acc.mem_max.map_or(m, |x| x.max(m)));
        }
        let (ready, configured) = (sat16(s.workers_ready as u64), sat16(s.workers_configured as u64));
        acc.ready_min = if acc.n == 0 { ready } else { acc.ready_min.min(ready) };
        acc.configured_max = acc.configured_max.max(configured);
        acc.restarts = acc.restarts.saturating_add(restarts);
        acc.n += 1;
    }

    /// One host sample (`cpu_percent` over the time since the previous one).
    pub fn observe_host(&mut self, cpu_percent: f64, mem_used: u64, mem_total: u64, load1: f64) {
        let h = &mut self.host_acc;
        h.cpu_sum += cpu_percent;
        h.load_sum += load1;
        h.mem_used_max = h.mem_used_max.max(mem_used);
        h.mem_total = mem_total;
        h.n += 1;
    }

    /// Debug builds only (like `WARDEN_FAULT`): `WARDEN_HISTORY_PREFILL=1`
    /// gives every new app (and the host) a full day of made-up samples, to
    /// measure wardend and the GUI with full rings without waiting a day.
    #[cfg(debug_assertions)]
    fn prefill_for_tests(&mut self, app: &Arc<str>) {
        if std::env::var_os("WARDEN_HISTORY_PREFILL").is_none_or(|v| v != "1") {
            return;
        }
        let now = crate::events::now_ms() / 1000;
        let now = now - now % STEP_S;
        // Not over a history loaded from disk: its apps' rings end with its times.
        if self.times.len() < CAP && self.apps.is_empty() {
            self.times = Ring::default();
            self.host = Ring::default();
            for i in 0..CAP {
                let wave = ((i as f32) / 180.0).sin();
                self.times.push(u32::try_from(now - (CAP - i) as u64 * STEP_S).unwrap_or(0));
                self.host.push(HostSample {
                    cpu: 20.0 + 10.0 * wave,
                    load1: 0.5,
                    mem_used_kib: 3_000_000,
                    mem_total_kib: 8_000_000,
                });
            }
            self.seq = self.seq.max(CAP as u64);
        }
        let mut ring = Ring::default();
        for i in 0..self.times.len() {
            let wave = ((i as f32) / 90.0).sin();
            ring.push(AppSample {
                cpu: 3.0 + 2.0 * wave,
                mem_kib: 150_000 + (i as u32 % 1000) * 100,
                ready: 2,
                configured: 2,
                restarts: u16::from(i % 700 == 0),
                flags: PRESENT | MEM,
            });
        }
        let seq = self.seq;
        self.apps.insert(app.clone(), AppSeries { ring, last_present: seq, ..Default::default() });
    }

    /// Room for one more app: drop the series of the app gone the longest
    /// (never one sampled in the last 10 minutes). False: no room.
    fn make_room(&mut self) -> bool {
        if self.apps.len() < MAX_APPS {
            return true;
        }
        let quiet = self.seq.saturating_sub(60);
        let oldest = self
            .apps
            .iter()
            .filter(|(_, a)| a.last_present < quiet)
            .min_by_key(|(_, a)| a.last_present)
            .map(|(k, _)| k.clone());
        match oldest {
            Some(k) => {
                self.apps.remove(&k);
                true
            }
            None => {
                if !self.full_logged {
                    self.full_logged = true;
                    crate::warn!(
                        "resource history is full; new apps get none",
                        apps = MAX_APPS,
                        kib_per_app = APP_BYTES / 1024,
                        hint =
                            "the series of an app gone for 10 minutes makes room; so many apps on one host is unusual",
                    );
                }
                false
            }
        }
    }

    /// Called every second with the time: commits the 10 s that ended.
    pub fn tick(&mut self, now_s: u64) {
        let slot = now_s - now_s % STEP_S;
        match self.open {
            None => self.open = Some(slot),
            Some(open) if slot > open => {
                self.commit(open);
                self.open = Some(slot);
            }
            // The same interval, or the clock went back: keep accumulating.
            Some(_) => {}
        }
    }

    /// Push the accumulated interval starting at `at_s` as one sample.
    fn commit(&mut self, at_s: u64) {
        self.seq += 1;
        let at = u32::try_from(at_s).unwrap_or(u32::MAX);
        self.times.push(at);
        let h = std::mem::take(&mut self.host_acc);
        self.host.push(if h.n == 0 {
            HostSample::ABSENT
        } else {
            HostSample {
                cpu: (h.cpu_sum / f64::from(h.n)) as f32,
                load1: (h.load_sum / f64::from(h.n)) as f32,
                mem_used_kib: kib(h.mem_used_max),
                mem_total_kib: kib(h.mem_total),
            }
        });
        let seq = self.seq;
        for a in self.apps.values_mut() {
            let acc = std::mem::take(&mut a.acc);
            if acc.n == 0 {
                a.ring.push(AppSample::ABSENT);
                continue;
            }
            a.last_present = seq;
            a.ring.push(AppSample {
                cpu: if acc.cpu_n == 0 { f32::NAN } else { (acc.cpu_sum / f64::from(acc.cpu_n)) as f32 },
                mem_kib: acc.mem_max.map(kib).unwrap_or(0),
                ready: acc.ready_min,
                configured: acc.configured_max,
                restarts: sat16(acc.restarts),
                flags: PRESENT | if acc.mem_max.is_some() { MEM } else { 0 },
            });
        }
        // 24 h without a sample: nothing left to show.
        self.apps.retain(|_, a| seq - a.last_present < CAP as u64);
    }

    /// Heap bytes held by the series (the bound the docs state).
    #[cfg(test)]
    pub fn heap_bytes(&self) -> usize {
        let apps: usize = self.apps.values().map(|a| a.ring.heap_bytes()).sum();
        apps + self.times.heap_bytes() + self.host.heap_bytes()
    }

    /// The `history` request.
    pub fn query(&self, app: Option<&str>, since_ms: Option<u64>, step_s: Option<u32>, now_ms: u64) -> ResourceHistory {
        let now_s = now_ms / 1000;
        let oldest = now_s.saturating_sub(CAP as u64 * STEP_S);
        let since = since_ms.map(|m| m / 1000).unwrap_or(0).clamp(oldest, now_s);
        let apps: Vec<(&Arc<str>, &AppSeries)> = match app {
            Some(name) => self.apps.get_key_value(name).into_iter().collect(),
            None => self.apps.iter().collect(),
        };
        // A multiple of STEP_S, at least one sample, at most a day.
        let mut step = u64::from(step_s.unwrap_or(STEP_S as u32)).clamp(STEP_S, 86_400).div_ceil(STEP_S) * STEP_S;
        let values_per_point = 3 + 5 * apps.len();
        let points_for = |step: u64| (now_s - (since - since % step)) / step + 1;
        while points_for(step) as usize * values_per_point > MAX_VALUES && step < 86_400 {
            step = (step * 2).min(86_400);
        }
        let start = since - since % step;
        let points = points_for(step) as usize;
        let bucket = |t: u32| -> Option<usize> {
            let t = u64::from(t);
            (t >= start).then(|| ((t - start) / step) as usize).filter(|b| *b < points)
        };

        let n = self.times.len();
        let mut host = HostAgg::new(points);
        for i in 0..n {
            if let Some(b) = bucket(self.times.get(i)) {
                host.add(b, self.host.get(i));
            }
        }
        let mut out = ResourceHistory {
            start_ms: start * 1000,
            step_s: u32::try_from(step).unwrap_or(u32::MAX),
            points: u32::try_from(points).unwrap_or(u32::MAX),
            host: host.finish(),
            apps: Vec::with_capacity(apps.len()),
        };
        for (name, a) in apps {
            let mut agg = AppAgg::new(points);
            let len = a.ring.len();
            // The app's ring ends with the newest time, like every ring.
            let offset = n.saturating_sub(len);
            for j in 0..len.min(n) {
                if let Some(b) = bucket(self.times.get(offset + j)) {
                    agg.add(b, a.ring.get(j));
                }
            }
            out.apps.push(agg.finish(name));
        }
        out
    }
}

// ------------------------------------------------------------ downsampling

/// Rounded to 0.1 (what the GUI shows; shorter JSON).
fn tenth(x: f64) -> f32 {
    ((x * 10.0).round() / 10.0) as f32
}

struct HostAgg {
    cpu: Vec<(f64, u32)>,
    load: Vec<(f64, u32)>,
    mem: Vec<Option<u32>>,
    total: Option<u32>,
}

impl HostAgg {
    fn new(points: usize) -> HostAgg {
        HostAgg { cpu: vec![(0.0, 0); points], load: vec![(0.0, 0); points], mem: vec![None; points], total: None }
    }

    fn add(&mut self, b: usize, s: HostSample) {
        if s.cpu.is_nan() {
            return;
        }
        self.cpu[b].0 += f64::from(s.cpu);
        self.cpu[b].1 += 1;
        if !s.load1.is_nan() {
            self.load[b].0 += f64::from(s.load1);
            self.load[b].1 += 1;
        }
        self.mem[b] = Some(self.mem[b].map_or(s.mem_used_kib, |m| m.max(s.mem_used_kib)));
        self.total = Some(s.mem_total_kib);
    }

    fn finish(self) -> HostHistory {
        let mean = |v: Vec<(f64, u32)>, digits: f64| -> Vec<Option<f32>> {
            v.into_iter()
                .map(|(sum, n)| (n > 0).then(|| ((sum / f64::from(n) * digits).round() / digits) as f32))
                .collect()
        };
        HostHistory {
            cpu_percent: mean(self.cpu, 10.0),
            mem_used_bytes: self.mem.into_iter().map(|m| m.map(|k| u64::from(k) * 1024)).collect(),
            mem_total_bytes: self.total.map(|k| u64::from(k) * 1024),
            load1: mean(self.load, 100.0),
        }
    }
}

#[derive(Clone, Copy, Default)]
struct AppPoint {
    n: u32,
    cpu_sum: f64,
    cpu_n: u32,
    mem: Option<u32>,
    ready: u16,
    configured: u16,
    restarts: u32,
}

struct AppAgg(Vec<AppPoint>);

impl AppAgg {
    fn new(points: usize) -> AppAgg {
        AppAgg(vec![AppPoint::default(); points])
    }

    fn add(&mut self, b: usize, s: AppSample) {
        if s.flags & PRESENT == 0 {
            return;
        }
        let p = &mut self.0[b];
        if !s.cpu.is_nan() {
            p.cpu_sum += f64::from(s.cpu);
            p.cpu_n += 1;
        }
        if s.flags & MEM != 0 {
            p.mem = Some(p.mem.map_or(s.mem_kib, |m| m.max(s.mem_kib)));
        }
        p.ready = if p.n == 0 { s.ready } else { p.ready.min(s.ready) };
        p.configured = p.configured.max(s.configured);
        p.restarts = p.restarts.saturating_add(u32::from(s.restarts));
        p.n += 1;
    }

    fn finish(self, name: &str) -> AppHistory {
        let pts = self.0;
        let some = |p: &AppPoint| p.n > 0;
        AppHistory {
            app: name.to_string(),
            cpu_percent: pts.iter().map(|p| (p.cpu_n > 0).then(|| tenth(p.cpu_sum / f64::from(p.cpu_n)))).collect(),
            rss_bytes: pts.iter().map(|p| p.mem.map(|k| u64::from(k) * 1024)).collect(),
            workers_ready: pts.iter().map(|p| some(p).then_some(u32::from(p.ready))).collect(),
            workers_configured: pts.iter().map(|p| some(p).then_some(u32::from(p.configured))).collect(),
            restarts: pts.iter().map(|p| some(p).then_some(p.restarts)).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) fn status(ready: usize, cpu: f64, rss: u64, restarts: u64) -> Status {
        let mut s = super::super::policy::tests::status(false, false);
        s.workers_configured = 2;
        s.workers_ready = ready;
        s.supervisor_rss_bytes = Some(1 << 20);
        s.workers = (1..=2)
            .map(|id| crate::control::WorkerStatus {
                id,
                state: "RUNNING".into(),
                pid: Some(100 + id as u32),
                uptime_secs: Some(1),
                restarts: if id == 1 { restarts } else { 0 },
                crashes: 0,
                rss_bytes: Some(rss),
                cpu_seconds: None,
                cpu_percent: Some(cpu),
                last_exit: None,
                healthy: None,
                loop_delay: None,
                listening: Vec::new(),
            })
            .collect();
        s
    }

    const T0: u64 = 1_790_000_000; // a multiple of 10

    #[test]
    fn records_are_compact_and_the_memory_bound_holds() {
        assert_eq!(std::mem::size_of::<AppSample>(), 16);
        assert_eq!(std::mem::size_of::<HostSample>(), 16);
        assert_eq!(CAP, 8640);
        assert_eq!(APP_BYTES, 138_240, "135 KiB per app, as docs/protocol.md says");
        assert_eq!(BASE_BYTES, 172_800);
        let mut st = Store::default();
        let apps: Vec<Arc<str>> = (0..10).map(|i| Arc::from(format!("app{i}"))).collect();
        // A bit more than a day of samples: every ring is full and wrapped.
        let mut now = T0;
        for _ in 0..(CAP + 500) {
            for a in &apps {
                st.observe_app(a, &status(2, 1.0, 50 << 20, 0), 0);
            }
            st.observe_host(10.0, 1 << 30, 8 << 30, 0.5);
            now += STEP_S;
            st.tick(now);
        }
        assert_eq!(st.times.len(), CAP);
        for a in st.apps.values() {
            assert_eq!(a.ring.len(), CAP);
            assert!(a.ring.buf.capacity() <= CAP, "{}", a.ring.buf.capacity());
        }
        let bound = BASE_BYTES + 10 * APP_BYTES;
        assert!(st.heap_bytes() <= bound, "{} > {bound}", st.heap_bytes());
        assert!(st.heap_bytes() >= bound * 9 / 10, "rings are full: {}", st.heap_bytes());
        // A full day at 10 s: 8640 points (plus the open one).
        let h = st.query(Some("app3"), None, None, now * 1000);
        assert!(h.points as usize >= CAP && h.points as usize <= CAP + 1, "{}", h.points);
        assert_eq!(h.apps.len(), 1);
        assert!(h.apps[0].cpu_percent.iter().filter(|c| c.is_some()).count() >= CAP - 1);
    }

    #[test]
    fn samples_aggregate_per_ten_seconds_and_downsample_per_metric() {
        let mut st = Store::default();
        let api: Arc<str> = "api".into();
        st.tick(T0);
        // First 10 s: CPU 1 and 3 per worker (sum 2 and 6: mean 4), one dip in ready.
        st.observe_app(&api, &status(2, 1.0, 10 << 20, 0), 0);
        st.observe_app(&api, &status(1, 3.0, 20 << 20, 0), 0);
        st.observe_host(10.0, 2 << 30, 8 << 30, 1.0);
        st.observe_host(30.0, 3 << 30, 8 << 30, 2.0);
        st.tick(T0 + 10);
        // Next 10 s: two restarts of worker 1, and one of the supervisor by wardend.
        st.observe_app(&api, &status(2, 5.0, 15 << 20, 2), 1);
        st.tick(T0 + 20);
        // 10 s with nothing: a gap.
        st.tick(T0 + 30);
        st.observe_app(&api, &status(2, 1.0, 15 << 20, 2), 1);
        st.tick(T0 + 40);

        let h = st.query(None, Some(T0 * 1000), None, (T0 + 40) * 1000);
        assert_eq!((h.start_ms, h.step_s, h.points), (T0 * 1000, 10, 5));
        let a = &h.apps[0];
        assert_eq!(a.app, "api");
        assert_eq!(a.cpu_percent, [Some(4.0), Some(10.0), None, Some(2.0), None]);
        let mb = |n: u64| Some((2 * n + 1) << 20);
        assert_eq!(a.rss_bytes, [mb(20), mb(15), None, mb(15), None], "max of the interval");
        assert_eq!(a.workers_ready, [Some(1), Some(2), None, Some(2), None], "min");
        assert_eq!(a.workers_configured, [Some(2), Some(2), None, Some(2), None]);
        assert_eq!(a.restarts, [Some(0), Some(3), None, Some(0), None], "the first status is the baseline");
        assert_eq!(h.host.cpu_percent, [Some(20.0), None, None, None, None]);
        assert_eq!(h.host.mem_used_bytes[0], Some(3 << 30));
        assert_eq!(h.host.mem_total_bytes, Some(8 << 30));
        assert_eq!(h.host.load1[0], Some(1.5));

        // 20 s per point: CPU mean, memory max, ready min, restarts sum.
        let h = st.query(Some("api"), Some(T0 * 1000), Some(20), (T0 + 40) * 1000);
        assert_eq!((h.step_s, h.points), (20, 3));
        let a = &h.apps[0];
        assert_eq!(a.cpu_percent, [Some(7.0), Some(2.0), None]);
        assert_eq!(a.rss_bytes, [mb(20), mb(15), None]);
        assert_eq!(a.workers_ready, [Some(1), Some(2), None]);
        assert_eq!(a.restarts, [Some(3), Some(0), None]);
        // Steps are multiples of 10 s; "" asks for the host only; an unknown app has no series.
        assert_eq!(st.query(None, None, Some(15), (T0 + 40) * 1000).step_s, 20);
        assert_eq!(st.query(None, None, Some(1), (T0 + 40) * 1000).step_s, 10);
        assert!(st.query(Some(""), None, None, (T0 + 40) * 1000).apps.is_empty());
        assert!(st.query(Some("nope"), None, None, (T0 + 40) * 1000).apps.is_empty());
    }

    #[test]
    fn history_survives_restarts_and_late_apps_line_up() {
        let mut st = Store::default();
        let (api, web): (Arc<str>, Arc<str>) = ("api".into(), "web".into());
        st.tick(T0);
        st.observe_app(&api, &status(2, 1.0, 1 << 20, 0), 0);
        st.tick(T0 + 10);
        // web appears later; api's supervisor is replaced (a new pid, counters from 0).
        let mut s = status(2, 1.0, 1 << 20, 1);
        s.pid = 4242;
        st.observe_app(&api, &s, 1);
        st.observe_app(&web, &status(2, 7.0, 1 << 20, 0), 0);
        st.tick(T0 + 20);
        let h = st.query(None, Some(T0 * 1000), None, (T0 + 20) * 1000);
        let api_h = h.apps.iter().find(|a| a.app == "api").unwrap();
        let web_h = h.apps.iter().find(|a| a.app == "web").unwrap();
        assert_eq!(api_h.restarts[..2], [Some(0), Some(2)], "the worker's restart and wardend's");
        assert_eq!(web_h.cpu_percent[..2], [None, Some(14.0)], "web's first sample is at its own time");
        // A clock that jumps back does not commit anything.
        st.tick(T0 + 5);
        assert_eq!(st.times.len(), 2);
    }

    #[test]
    fn replies_are_bounded() {
        let mut st = Store::default();
        let mut now = T0;
        for i in 0..MAX_APPS {
            st.observe_app(&Arc::from(format!("a{i}")), &status(1, 1.0, 1, 0), 0);
        }
        for _ in 0..CAP - 10 {
            now += STEP_S;
            st.tick(now);
        }
        let h = st.query(None, None, Some(10), now * 1000);
        assert_eq!(h.apps.len(), MAX_APPS);
        let values = h.points as usize * (3 + 5 * h.apps.len());
        assert!(values <= MAX_VALUES, "{values}");
        assert!(h.step_s > 10, "the step was raised: {}", h.step_s);
        // Every app was gone for a day: forgotten.
        for _ in 0..20 {
            now += STEP_S;
            st.tick(now);
        }
        assert!(st.apps.is_empty(), "{}", st.apps.len());
        // Past MAX_APPS, an app gone for a while makes room; live ones never do.
        let mut st = Store::default();
        for i in 0..MAX_APPS {
            st.observe_app(&Arc::from(format!("a{i}")), &status(1, 1.0, 1, 0), 0);
        }
        st.tick(T0);
        st.tick(T0 + 10);
        st.observe_app(&Arc::from("late"), &status(1, 1.0, 1, 0), 0);
        assert!(!st.apps.contains_key("late"), "all apps are live");
        for i in 0..70 {
            st.observe_app(&Arc::from("a0"), &status(1, 1.0, 1, 0), 0);
            st.tick(T0 + 20 + i * 10);
        }
        st.observe_app(&Arc::from("late"), &status(1, 1.0, 1, 0), 0);
        assert!(st.apps.contains_key("late") && st.apps.contains_key("a0") && st.apps.len() == MAX_APPS);
    }
}

//! The History tab's and the header's data (docs/protocol.md, "Resource
//! history"): what wardend's `history` returned, on its time grid, then
//! live samples (statuses, `host` events) added to the same grid, counted
//! the way wardend counts them. No I/O here: every step is tested.
//!
//! Memory: one app's charts (5 series of at most 360 points) and the host's
//! sparklines (2 × 360): a few tens of KB whatever the range.

use std::collections::VecDeque;
use warden_protocol::control::Status;
use warden_protocol::events::{ResourceHistory, Usage, restarts_since};

/// The History tab's time ranges: 360 points each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Range {
    Hour,
    SixHours,
    Day,
}

impl Range {
    pub const ALL: [Range; 3] = [Range::Hour, Range::SixHours, Range::Day];

    pub fn secs(self) -> u64 {
        match self {
            Range::Hour => 3600,
            Range::SixHours => 6 * 3600,
            Range::Day => 24 * 3600,
        }
    }

    /// Seconds per point (wardend samples every 10 s).
    pub fn step_s(self) -> u64 {
        self.secs() / POINTS as u64
    }

    pub fn label(self) -> &'static str {
        match self {
            Range::Hour => "1 h",
            Range::SixHours => "6 h",
            Range::Day => "24 h",
        }
    }
}

/// Points per chart.
pub const POINTS: usize = 360;

/// How a point combines the samples in it (as wardend downsamples).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Mean,
    Max,
    Min,
    Sum,
}

/// One metric's points, oldest first. `None`: nothing was sampled.
#[derive(Debug, Clone)]
pub struct Series {
    pub values: VecDeque<Option<f32>>,
    agg: Agg,
    /// (sum or extreme, samples) of the newest point, while live samples fill it.
    acc: Option<(f64, u32)>,
}

impl Series {
    fn new(agg: Agg) -> Series {
        Series { values: VecDeque::new(), agg, acc: None }
    }

    fn add(&mut self, v: f32) {
        let Some(last) = self.values.back_mut() else { return };
        let v = f64::from(v);
        let (x, n) = match (self.acc, *last) {
            (Some(a), _) => a,
            // The newest point came with the reply: count it as one sample.
            (None, Some(had)) => (f64::from(had), 1),
            (None, None) => {
                self.acc = Some((v, 1));
                *last = Some(v as f32);
                return;
            }
        };
        let (x, n) = match self.agg {
            Agg::Mean | Agg::Sum => (x + v, n + 1),
            Agg::Max => (x.max(v), n + 1),
            Agg::Min => (x.min(v), n + 1),
        };
        self.acc = Some((x, n));
        *last = Some(if self.agg == Agg::Mean { (x / f64::from(n)) as f32 } else { x as f32 });
    }

    /// Mean and max of the points that have a value.
    pub fn stats(&self) -> Option<(f32, f32)> {
        let (mut sum, mut n, mut max) = (0.0f64, 0u32, f32::MIN);
        for v in self.values.iter().flatten() {
            sum += f64::from(*v);
            n += 1;
            max = max.max(*v);
        }
        (n > 0).then(|| ((sum / f64::from(n)) as f32, max))
    }

    pub fn last(&self) -> Option<f32> {
        self.values.iter().rev().flatten().next().copied()
    }

    pub fn sum(&self) -> f64 {
        self.values.iter().flatten().map(|v| f64::from(*v)).sum()
    }
}

/// Several series on one time grid: point `i` covers `[start_s + i * step_s, + step_s)`.
#[derive(Debug, Clone)]
pub struct Grid {
    pub start_s: u64,
    pub step_s: u64,
    /// Points kept at most (the range).
    pub cap: usize,
    pub series: Vec<Series>,
}

impl Grid {
    pub fn new(step_s: u64, cap: usize, aggs: &[Agg]) -> Grid {
        Grid {
            start_s: 0,
            step_s: step_s.max(1),
            cap: cap.max(1),
            series: aggs.iter().map(|a| Series::new(*a)).collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.series.first().map_or(0, |s| s.values.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Where the newest point ends: the chart's right edge.
    pub fn end_s(&self) -> u64 {
        self.start_s + self.len() as u64 * self.step_s
    }

    /// Replace everything with a reply's points (`values[i]` per series).
    fn load(&mut self, start_s: u64, step_s: u64, values: Vec<Vec<Option<f32>>>) {
        self.start_s = start_s;
        self.step_s = step_s.max(1);
        for (s, v) in self.series.iter_mut().zip(values) {
            s.values = v.into();
            s.acc = None;
        }
        self.trim();
    }

    fn trim(&mut self) {
        while self.len() > self.cap {
            for s in &mut self.series {
                s.values.pop_front();
            }
            self.start_s += self.step_s;
        }
    }

    /// Make the newest point the one holding `t_s`. False: `t_s` is older
    /// than the newest point (a live sample never rewrites the past).
    fn advance(&mut self, t_s: u64) -> bool {
        let b = t_s - t_s % self.step_s;
        if self.is_empty() {
            self.start_s = b;
            for s in &mut self.series {
                s.values.push_back(None);
                s.acc = None;
            }
            return true;
        }
        let last = self.end_s() - self.step_s;
        if b < last {
            return false;
        }
        if (b - last) / self.step_s > self.cap as u64 {
            // Asleep for longer than the range: start over.
            for s in &mut self.series {
                s.values.clear();
            }
            return self.advance(t_s);
        }
        let mut at = last;
        while at < b {
            at += self.step_s;
            for s in &mut self.series {
                s.values.push_back(None);
                s.acc = None;
            }
        }
        self.trim();
        true
    }

    /// One live sample at `t_s`: a value (or none) per series.
    pub fn add(&mut self, t_s: u64, values: &[Option<f32>]) {
        if !self.advance(t_s) {
            return;
        }
        for (s, v) in self.series.iter_mut().zip(values) {
            if let Some(v) = v.filter(|v| v.is_finite()) {
                s.add(v);
            }
        }
    }
}

/// Fetching state.
#[derive(Debug, Clone, PartialEq)]
pub enum Load {
    Loading,
    Ready,
    Failed(String),
}

/// Series of `AppChart.grid`.
pub const CPU: usize = 0;
pub const MEM: usize = 1;
pub const RESTARTS: usize = 2;
pub const READY: usize = 3;
pub const CONFIGURED: usize = 4;

/// One app's charts for one range.
#[derive(Debug, Clone)]
pub struct AppChart {
    pub app: String,
    pub range: Range,
    pub load: Load,
    pub grid: Grid,
    /// (supervisor pid, its restart counters) at the last status.
    last_restarts: Option<(u32, u64)>,
    /// wardend's restarts of the supervisor at the last `apps` event.
    last_sup_restarts: Option<u32>,
}

impl AppChart {
    pub fn loading(app: &str, range: Range) -> AppChart {
        AppChart {
            app: app.to_string(),
            range,
            load: Load::Loading,
            grid: Grid::new(range.step_s(), POINTS, &[Agg::Mean, Agg::Max, Agg::Sum, Agg::Min, Agg::Max]),
            last_restarts: None,
            last_sup_restarts: None,
        }
    }

    /// The `history` reply.
    pub fn loaded(&mut self, h: ResourceHistory) {
        let a = h.apps.into_iter().find(|a| a.app == self.app).unwrap_or_default();
        let n = h.points as usize;
        let f = |v: Vec<Option<u32>>| -> Vec<Option<f32>> { v.into_iter().map(|x| x.map(|x| x as f32)).collect() };
        let pad = |mut v: Vec<Option<f32>>| {
            v.resize(n, None);
            v
        };
        let values = vec![
            pad(a.cpu_percent),
            pad(a.rss_bytes.into_iter().map(|b| b.map(|b| b as f32)).collect()),
            pad(f(a.restarts)),
            pad(f(a.workers_ready)),
            pad(f(a.workers_configured)),
        ];
        self.grid.load(h.start_ms / 1000, u64::from(h.step_s), values);
        self.load = Load::Ready;
    }

    /// A live status of this app at `t_s`.
    pub fn status(&mut self, s: &Status, t_s: u64) {
        let u = Usage::of(s);
        let restarts = restarts_since(self.last_restarts, s.pid, u.restarts);
        self.last_restarts = Some((s.pid, u.restarts));
        if self.load != Load::Ready {
            return;
        }
        self.grid.add(
            t_s,
            &[
                u.cpu_percent.map(|c| c as f32),
                u.rss_bytes.map(|b| b as f32),
                Some(restarts as f32),
                Some(s.workers_ready as f32),
                Some(s.workers_configured as f32),
            ],
        );
    }

    /// wardend's count of restarts of the supervisor (from `apps`).
    pub fn supervisor_restarts(&mut self, n: u32, t_s: u64) {
        let before = self.last_sup_restarts.replace(n);
        if let Some(b) = before.filter(|b| n > *b && self.load == Load::Ready) {
            self.grid.add(t_s, &[None, None, Some((n - b) as f32), None, None]);
        }
    }
}

/// The header's host sparklines: the last hour, a point per 10 s.
#[derive(Debug, Clone)]
pub struct HostSpark {
    pub grid: Grid,
}

pub const HOST_CPU: usize = 0;
pub const HOST_MEM: usize = 1;

impl Default for HostSpark {
    fn default() -> Self {
        HostSpark { grid: Grid::new(Range::Hour.step_s(), POINTS, &[Agg::Mean, Agg::Max]) }
    }
}

impl HostSpark {
    pub fn loaded(&mut self, h: ResourceHistory) {
        let n = h.points as usize;
        let mut cpu = h.host.cpu_percent;
        cpu.resize(n, None);
        let mut mem: Vec<Option<f32>> = h.host.mem_used_bytes.into_iter().map(|b| b.map(|b| b as f32)).collect();
        mem.resize(n, None);
        // Live samples that came first are newer than the reply: kept.
        let live = std::mem::replace(&mut self.grid, HostSpark::default().grid);
        self.grid.load(h.start_ms / 1000, u64::from(h.step_s), vec![cpu, mem]);
        for i in 0..live.len() {
            let t = live.start_s + i as u64 * live.step_s;
            if t >= self.grid.end_s() {
                self.grid.add(t, &[live.series[HOST_CPU].values[i], live.series[HOST_MEM].values[i]]);
            }
        }
    }

    pub fn sample(&mut self, cpu_percent: f64, mem_used: u64, t_s: u64) {
        self.grid.add(t_s, &[Some(cpu_percent as f32), Some(mem_used as f32)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_protocol::events::{AppHistory, HostHistory};

    const T0: u64 = 1_790_000_000; // a multiple of 240

    fn reply(app: &str, step: u32, points: u32) -> ResourceHistory {
        let n = points as usize;
        ResourceHistory {
            start_ms: T0 * 1000,
            step_s: step,
            points,
            host: HostHistory {
                cpu_percent: vec![Some(10.0); n],
                mem_used_bytes: vec![Some(1 << 30); n],
                mem_total_bytes: Some(8 << 30),
                load1: vec![Some(0.5); n],
            },
            apps: vec![AppHistory {
                app: app.into(),
                cpu_percent: (0..n).map(|i| Some(i as f32)).collect(),
                rss_bytes: vec![Some(100 << 20); n],
                workers_ready: vec![Some(2); n],
                workers_configured: vec![Some(2); n],
                restarts: (0..n).map(|i| if i == 3 { Some(2) } else { Some(0) }).collect(),
            }],
        }
    }

    fn status(pid: u32, cpu: f64, restarts: u64) -> Status {
        serde_json::from_value(serde_json::json!({
            "app": "api", "mode": "process", "pid": pid, "uptime_secs": 1, "workers_configured": 2,
            "workers_ready": 2, "healthy": null, "supervisor_rss_bytes": 1048576, "host": null, "reloading": false,
            "shutting_down": false,
            "workers": [{"id": 1, "state": "RUNNING", "pid": 9, "uptime_secs": 1, "restarts": restarts, "crashes": 0,
                         "rss_bytes": 1048576, "cpu_seconds": null, "cpu_percent": cpu, "last_exit": null}]
        }))
        .unwrap()
    }

    #[test]
    fn ranges_have_360_points() {
        assert_eq!(Range::ALL.map(Range::step_s), [10, 60, 240]);
        for r in Range::ALL {
            assert_eq!(r.secs() / r.step_s(), POINTS as u64);
        }
    }

    #[test]
    fn a_reply_then_live_statuses_on_the_same_grid() {
        let mut c = AppChart::loading("api", Range::Hour);
        // A status before the reply sets the restart baseline, adds nothing.
        c.status(&status(42, 1.0, 0), T0);
        assert!(c.grid.is_empty());
        c.loaded(reply("api", 10, 5));
        assert_eq!(c.load, Load::Ready);
        assert_eq!((c.grid.start_s, c.grid.step_s, c.grid.len(), c.grid.end_s()), (T0, 10, 5, T0 + 50));
        assert_eq!(c.grid.series[CPU].values[4], Some(4.0));
        assert_eq!(c.grid.series[RESTARTS].sum(), 2.0);
        // Live: the newest point (T0+40) is merged into, then new points follow.
        c.status(&status(42, 6.0, 0), T0 + 45);
        assert_eq!(c.grid.series[CPU].values[4], Some(5.0), "mean of the reply's 4 and 6");
        c.status(&status(42, 2.0, 1), T0 + 62);
        assert_eq!(c.grid.len(), 7, "T0+50 is empty, T0+60 has the sample");
        assert_eq!(c.grid.series[CPU].values[5], None);
        assert_eq!(c.grid.series[CPU].values[6], Some(2.0));
        assert_eq!(c.grid.series[RESTARTS].values[6], Some(1.0));
        assert_eq!(c.grid.series[MEM].values[6], Some(2.0 * 1048576.0));
        c.status(&status(42, 4.0, 3), T0 + 65);
        assert_eq!(c.grid.series[CPU].values[6], Some(3.0), "mean");
        assert_eq!(c.grid.series[RESTARTS].values[6], Some(3.0), "sum");
        // wardend restarted the supervisor: counted as a restart too.
        c.supervisor_restarts(0, T0 + 66);
        c.supervisor_restarts(1, T0 + 67);
        assert_eq!(c.grid.series[RESTARTS].values[6], Some(4.0));
        // A late sample never rewrites the past.
        c.status(&status(42, 99.0, 3), T0 + 5);
        assert_eq!(c.grid.series[CPU].values[0], Some(0.0));
        let (avg, max) = c.grid.series[CPU].stats().unwrap();
        assert_eq!(max, 5.0);
        assert!((avg - 14.0 / 6.0).abs() < 1e-5, "{avg}");
    }

    #[test]
    fn the_grid_keeps_its_range() {
        let mut g = Grid::new(10, 3, &[Agg::Max]);
        for i in 0..10 {
            g.add(T0 + i * 10, &[Some(i as f32)]);
        }
        assert_eq!(g.len(), 3);
        assert_eq!(g.start_s, T0 + 70);
        assert_eq!(g.series[0].values, [Some(7.0), Some(8.0), Some(9.0)]);
        // A long sleep: start over rather than push a day of empty points.
        g.add(T0 + 86_400, &[Some(1.0)]);
        assert_eq!((g.len(), g.start_s), (1, T0 + 86_400));
        // Max and min.
        let mut g = Grid::new(10, 3, &[Agg::Max, Agg::Min]);
        g.add(T0, &[Some(3.0), Some(3.0)]);
        g.add(T0 + 1, &[Some(5.0), Some(1.0)]);
        g.add(T0 + 2, &[Some(4.0), Some(f32::NAN)]);
        assert_eq!((g.series[0].values[0], g.series[1].values[0]), (Some(5.0), Some(1.0)));
    }

    #[test]
    fn an_app_without_history_and_the_host() {
        let mut c = AppChart::loading("web", Range::Day);
        c.loaded(reply("api", 240, 4));
        assert_eq!(c.grid.len(), 4, "the grid comes anyway, empty");
        assert!(c.grid.series[CPU].values.iter().all(Option::is_none));
        let mut h = HostSpark::default();
        h.sample(50.0, 2 << 30, T0 + 40);
        h.loaded(reply("api", 10, 4));
        assert_eq!(h.grid.len(), 5, "the live sample after the reply is kept");
        assert_eq!(h.grid.series[HOST_CPU].values[4], Some(50.0));
        assert_eq!(h.grid.series[HOST_MEM].last(), Some((2u64 << 30) as f32));
    }
}

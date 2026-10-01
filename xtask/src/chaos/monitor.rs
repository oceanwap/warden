//! What the chaos harness watches while the faults run:
//!
//! - the CLI: `warden list`, `warden status <app>` and `warden list --json`
//!   timed every half second;
//! - processes: zombies that stay, orphaned workers that outlive the drain,
//!   and the open fds and RSS of every supervisor and wardend over time.

use super::Shared;
use super::fleet::{Fleet, GRACE_S};
use super::procfs;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Latency {
    pub t: f64,
    pub cmd: &'static str,
    pub ms: f64,
    pub ok: bool,
}

#[derive(Debug, Clone)]
pub struct ProcSample {
    pub t: f64,
    /// "supervisor" or "wardend".
    pub role: &'static str,
    /// The app (supervisors) or "wardend".
    pub name: String,
    pub pid: u32,
    pub fds: usize,
    pub rss_kb: u64,
    /// Taken while no fault was in progress and the fleet had recovered.
    pub quiet: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Mon {
    pub latency: Vec<Latency>,
    pub procs: Vec<ProcSample>,
    /// Problems found by the process scans (zombies, orphans), deduplicated.
    pub problems: Vec<(f64, String)>,
    pub zombies_seen: usize,
}

/// Time the everyday commands, every 500 ms, until the run stops.
pub fn observer(sh: Arc<Shared>, fleet: Arc<Fleet>) {
    let names: Vec<&'static str> = fleet.apps.iter().map(|a| a.name).collect();
    let mut i = 0usize;
    while !sh.stopped() {
        let app = names.get(i % names.len().max(1)).copied().unwrap_or("api-bun");
        let cmds: [(&'static str, Vec<&str>); 3] =
            [("list", vec!["list"]), ("status <app>", vec!["status", app]), ("list --json", vec!["list", "--json"])];
        for (name, args) in cmds {
            let t = sh.now();
            let t0 = Instant::now();
            let ok = fleet.command(&args).output().is_ok_and(|o| o.status.success());
            let ms = t0.elapsed().as_secs_f64() * 1000.0;
            sh.with_mon(|m| m.latency.push(Latency { t, cmd: name, ms, ok }));
        }
        i += 1;
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Role {
    Supervisor,
    Wardend,
    Other,
}

/// Scan the process table every 2 s until the run stops.
pub fn procmon(sh: Arc<Shared>, fleet: Arc<Fleet>, in_namespace: bool) {
    let home_entry = format!("WARDEN_HOME={}", fleet.home.display());
    let me = std::process::id();
    let bin = fleet.bin.display().to_string();
    // (pid, start) -> ours? (environ is read once per process)
    let mut ours_cache: HashMap<(u32, u64), bool> = HashMap::new();
    // (pid, start) -> (first seen, scans seen) for zombies and orphans.
    let mut zombies: HashMap<(u32, u64), (f64, u32)> = HashMap::new();
    let mut orphans: HashMap<(u32, u64), f64> = HashMap::new();
    let mut reported: std::collections::HashSet<(u32, u64)> = Default::default();
    let mut last_sample = Instant::now() - Duration::from_secs(60);
    while !sh.stopped() {
        let t = sh.now();
        let stats: Vec<procfs::Stat> = procfs::pids().into_iter().filter_map(procfs::stat).collect();
        let by_pid: HashMap<u32, &procfs::Stat> = stats.iter().map(|s| (s.pid, s)).collect();
        let mut role_of: HashMap<u32, (Role, String)> = HashMap::new();
        let mut ours: Vec<&procfs::Stat> = Vec::new();
        for s in &stats {
            if s.pid == me || s.state == 'Z' {
                continue;
            }
            let mine = *ours_cache.entry((s.pid, s.start)).or_insert_with(|| procfs::environ_has(s.pid, &home_entry));
            if !mine {
                continue;
            }
            ours.push(s);
            let argv = procfs::cmdline(s.pid);
            let role = if argv.first().is_some_and(|a| *a == bin) {
                match argv.get(1).map(String::as_str) {
                    Some("start" | "run") if argv.iter().any(|a| a == "-c") => {
                        let cfg = argv.iter().skip_while(|a| *a != "-c").nth(1).cloned().unwrap_or_default();
                        let app = std::path::Path::new(&cfg)
                            .file_stem()
                            .map(|f| f.to_string_lossy().to_string())
                            .unwrap_or_default();
                        (Role::Supervisor, app)
                    }
                    Some("daemon") if argv.len() == 2 => (Role::Wardend, "wardend".to_string()),
                    _ => (Role::Other, String::new()),
                }
            } else {
                (Role::Other, String::new())
            };
            role_of.insert(s.pid, role);
        }
        // Zombies: a child its parent has not reaped. A moment is normal;
        // three scans (4+ s) in a row is a parent that does not reap.
        let mut seen_z = Vec::new();
        for s in stats.iter().filter(|s| s.state == 'Z') {
            let parent_ours = role_of.contains_key(&s.ppid);
            if !(parent_ours || (in_namespace && s.pid != me)) {
                continue;
            }
            let key = (s.pid, s.start);
            seen_z.push(key);
            let e = zombies.entry(key).or_insert((t, 0));
            e.1 += 1;
            if e.1 == 1 {
                sh.with_mon(|m| m.zombies_seen += 1);
            }
            if e.1 >= 3 && reported.insert(key) {
                let parent = by_pid.get(&s.ppid).map(|p| p.comm.clone()).unwrap_or_else(|| "?".into());
                let role = role_of.get(&s.ppid).map(|(r, n)| format!("{r:?} {n}")).unwrap_or_default();
                sh.with_mon(|m| {
                    m.problems.push((
                        e.0,
                        format!(
                            "zombie pid {} ({}) not reaped for {:.0}s by its parent pid {} ({parent}) {role}",
                            s.pid,
                            s.comm,
                            t - e.0,
                            s.ppid
                        ),
                    ))
                });
            }
        }
        zombies.retain(|k, _| seen_z.contains(k));
        // Orphans: a worker whose supervisor is gone drains and exits
        // (PDEATHSIG); one still running after grace_period + 10 s leaked.
        let mut seen_o = Vec::new();
        for s in &ours {
            let role = role_of.get(&s.pid).map(|r| r.0).unwrap_or(Role::Other);
            if role != Role::Other {
                continue;
            }
            let parent_alive = by_pid.get(&s.ppid).is_some_and(|p| p.state != 'Z');
            if s.ppid != 1 && parent_alive {
                continue;
            }
            let key = (s.pid, s.start);
            seen_o.push(key);
            let first = *orphans.entry(key).or_insert(t);
            if t - first > (GRACE_S + 10) as f64 && reported.insert(key) {
                let argv = procfs::cmdline(s.pid).join(" ");
                sh.with_mon(|m| {
                    m.problems.push((
                        first,
                        format!(
                            "orphan pid {} ({}) outlived its supervisor by {:.0}s: {argv}",
                            s.pid,
                            s.comm,
                            t - first
                        ),
                    ))
                });
            }
        }
        orphans.retain(|k, _| seen_o.contains(k));
        // fds and RSS of supervisors and wardend, every 4 s.
        if last_sample.elapsed() >= Duration::from_secs(4) {
            last_sample = Instant::now();
            let quiet = sh.quiet.load(std::sync::atomic::Ordering::Relaxed);
            let mut batch = Vec::new();
            for (pid, (role, name)) in &role_of {
                let role = match role {
                    Role::Supervisor => "supervisor",
                    Role::Wardend => "wardend",
                    Role::Other => continue,
                };
                if let (Some(fds), Some(rss_kb)) = (procfs::fd_count(*pid), procfs::rss_kb(*pid)) {
                    batch.push(ProcSample { t, role, name: name.clone(), pid: *pid, fds, rss_kb, quiet });
                }
            }
            sh.with_mon(|m| m.procs.extend(batch));
        }
        ours_cache.retain(|k, _| by_pid.get(&k.0).is_some_and(|s| s.start == k.1));
        std::thread::sleep(Duration::from_secs(2));
    }
}

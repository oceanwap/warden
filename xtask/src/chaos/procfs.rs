//! What the chaos harness reads about processes, and the signals it sends
//! (through `kill(1)`, so the harness needs no `unsafe`): `/proc` on Linux;
//! `ps` and `lsof` on macOS, which has no `/proc` (one `ps` for the whole
//! process table, so a scan stays cheap).

use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Stat {
    pub pid: u32,
    pub ppid: u32,
    /// R, S, D, T (stopped), Z (zombie)…
    pub state: char,
    /// When it started (Linux: clock ticks since boot; macOS: a hash of
    /// `ps`'s start time): tells a reused pid apart.
    pub start: u64,
    pub comm: String,
}

/// The process `pid` still runs (not a zombie) and is the one that had
/// start time `start` (not a reused pid).
pub fn same(pid: u32, start: u64) -> bool {
    stat(pid).is_some_and(|s| s.start == start && s.state != 'Z')
}

pub fn alive(pid: u32) -> bool {
    stat(pid).is_some_and(|s| s.state != 'Z')
}

/// `kill -s <sig> <pid>`, only while `pid` is still the process started at
/// `start`. True when the signal was sent.
pub fn signal(pid: u32, start: u64, sig: &str) -> bool {
    if !same(pid, start) {
        return false;
    }
    Command::new("kill")
        .args(["-s", sig, &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Every process (one scan).
pub fn all_stats() -> Vec<Stat> {
    imp::all_stats()
}

/// The command lines of `pids` (one `ps` on macOS).
pub fn cmdlines(pids: &[u32]) -> std::collections::HashMap<u32, Vec<String>> {
    imp::cmdlines(pids)
}

pub use imp::{cmdline, cwd, fd_count, is_root, ours, rss_kb, stat, sysctl};

#[cfg(target_os = "linux")]
mod imp {
    use super::Stat;
    use std::path::PathBuf;

    pub fn stat(pid: u32) -> Option<Stat> {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // "pid (comm) state ppid …": comm may hold spaces and parentheses.
        let open = text.find('(')?;
        let close = text.rfind(')')?;
        let comm = text.get(open + 1..close)?.to_string();
        let rest: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
        // rest[0] = state (field 3), rest[1] = ppid (4), rest[19] = starttime (22).
        Some(Stat {
            pid,
            state: rest.first()?.chars().next()?,
            ppid: rest.get(1)?.parse().ok()?,
            start: rest.get(19)?.parse().ok()?,
            comm,
        })
    }

    pub fn all_stats() -> Vec<Stat> {
        pids().into_iter().filter_map(stat).collect()
    }

    pub fn pids() -> Vec<u32> {
        let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
        rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()
    }

    pub fn cmdline(pid: u32) -> Vec<String> {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| {
                b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into()).collect()
            })
            .unwrap_or_default()
    }

    pub fn cmdlines(pids: &[u32]) -> std::collections::HashMap<u32, Vec<String>> {
        pids.iter().map(|p| (*p, cmdline(*p))).collect()
    }

    /// Whether the process's environment holds exactly `entry` (`KEY=value`).
    fn environ_has(pid: u32, entry: &str) -> bool {
        std::fs::read(format!("/proc/{pid}/environ"))
            .map(|b| b.split(|c| *c == 0).any(|kv| kv == entry.as_bytes()))
            .unwrap_or(false)
    }

    /// The live processes whose environment holds `entry`, but `me`.
    pub fn ours(entry: &str, me: u32) -> Vec<u32> {
        pids().into_iter().filter(|p| *p != me && super::alive(*p) && environ_has(*p, entry)).collect()
    }

    pub fn fd_count(pid: u32) -> Option<usize> {
        Some(std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count())
    }

    pub fn rss_kb(pid: u32) -> Option<u64> {
        let st = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        st.lines().find_map(|l| l.strip_prefix("VmRSS:")).and_then(|v| v.split_whitespace().next()?.parse().ok())
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    /// The value of `/proc/sys/<path>` (e.g. `net/ipv4/tcp_migrate_req`).
    pub fn sysctl(path: &str) -> Option<String> {
        std::fs::read_to_string(format!("/proc/sys/{path}")).ok().map(|s| s.trim().to_string())
    }

    pub fn is_root() -> bool {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines().find_map(|l| l.strip_prefix("Uid:")).map(|v| v.split_whitespace().next() == Some("0"))
            })
            .unwrap_or(false)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::Stat;
    use std::path::PathBuf;
    use std::process::Command;

    fn ps(args: &[&str]) -> String {
        Command::new("ps")
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// FNV-1a: a stable number for `ps`'s start time text.
    fn hash(s: &str) -> u64 {
        s.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    /// "pid ppid stat Wed Oct  1 12:00:00 2026 comm…" (lstart is five words).
    fn parse(line: &str) -> Option<Stat> {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 9 {
            return None;
        }
        Some(Stat {
            pid: f[0].parse().ok()?,
            ppid: f[1].parse().ok()?,
            state: f[2].chars().next()?,
            start: hash(&f[3..8].join(" ")),
            comm: f[8..].join(" "),
        })
    }

    /// One `-o` per field: after `=` the rest of an argument, commas
    /// included, is the column's header.
    const FIELDS: [&str; 10] = ["-o", "pid=", "-o", "ppid=", "-o", "stat=", "-o", "lstart=", "-o", "comm="];

    pub fn stat(pid: u32) -> Option<Stat> {
        let pid = pid.to_string();
        ps(&[&FIELDS[..], &["-p", &pid]].concat()).lines().find_map(parse)
    }

    pub fn all_stats() -> Vec<Stat> {
        ps(&[&["-ax"], &FIELDS[..]].concat()).lines().filter_map(parse).collect()
    }

    pub fn cmdline(pid: u32) -> Vec<String> {
        ps(&["-ww", "-o", "command=", "-p", &pid.to_string()]).split_whitespace().map(String::from).collect()
    }

    pub fn cmdlines(pids: &[u32]) -> std::collections::HashMap<u32, Vec<String>> {
        ps(&["-axww", "-o", "pid=", "-o", "command="])
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let pid: u32 = w.next()?.parse().ok()?;
                pids.contains(&pid).then(|| (pid, w.map(String::from).collect()))
            })
            .collect()
    }

    /// `ps -E` appends each process's environment (the user's own processes only).
    pub fn ours(entry: &str, me: u32) -> Vec<u32> {
        ps(&["-E", "-axww", "-o", "pid=", "-o", "stat=", "-o", "command="])
            .lines()
            .filter_map(|l| {
                let mut w = l.split_whitespace();
                let pid: u32 = w.next()?.parse().ok()?;
                let state = w.next()?;
                (pid != me && !state.starts_with('Z') && w.any(|x| x == entry)).then_some(pid)
            })
            .collect()
    }

    fn lsof(pid: u32, extra: &[&str]) -> String {
        Command::new("lsof")
            .args(["-n", "-P", "-a", "-p", &pid.to_string()])
            .args(extra)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    }

    /// Numbered descriptors (`lsof -F f`: "f0", "f1"…; not cwd, txt, mmaps).
    pub fn fd_count(pid: u32) -> Option<usize> {
        let out = lsof(pid, &["-F", "f"]);
        let n = out.lines().filter(|l| l.strip_prefix('f').is_some_and(|d| d.parse::<u32>().is_ok())).count();
        (n > 0).then_some(n)
    }

    pub fn rss_kb(pid: u32) -> Option<u64> {
        ps(&["-o", "rss=", "-p", &pid.to_string()]).trim().parse().ok()
    }

    pub fn cwd(pid: u32) -> Option<PathBuf> {
        lsof(pid, &["-d", "cwd", "-F", "n"]).lines().find_map(|l| l.strip_prefix('n').map(PathBuf::from))
    }

    /// Linux's /proc/sys: nothing here.
    pub fn sysctl(_path: &str) -> Option<String> {
        None
    }

    pub fn is_root() -> bool {
        Command::new("id").arg("-u").output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_own_process() {
        let me = std::process::id();
        let s = stat(me).expect("own stat");
        assert_eq!(s.pid, me);
        assert!(s.start > 0 && s.state != 'Z');
        assert!(same(me, s.start) && !same(me, s.start + 1));
        assert!(fd_count(me).unwrap() >= 3);
        assert!(rss_kb(me).unwrap() > 0);
        assert!(!cmdline(me).is_empty());
        assert_eq!(cmdlines(&[me]).get(&me), Some(&cmdline(me)));
        assert!(all_stats().iter().any(|s| s.pid == me));
        assert_eq!(cwd(me), std::env::current_dir().ok());
    }
}

//! What the chaos harness reads from `/proc` (Linux), and the signals it
//! sends (through `kill(1)`, so the harness needs no `unsafe`).

use std::path::PathBuf;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct Stat {
    pub pid: u32,
    pub ppid: u32,
    /// R, S, D, T (stopped), Z (zombie)…
    pub state: char,
    /// Start time in clock ticks since boot: tells a reused pid apart.
    pub start: u64,
    pub comm: String,
}

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

pub fn pids() -> Vec<u32> {
    let Ok(rd) = std::fs::read_dir("/proc") else { return Vec::new() };
    rd.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).collect()
}

pub fn cmdline(pid: u32) -> Vec<String> {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|c| *c == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into()).collect())
        .unwrap_or_default()
}

/// Whether the process's environment holds exactly `entry` (`KEY=value`).
pub fn environ_has(pid: u32, entry: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .map(|b| b.split(|c| *c == 0).any(|kv| kv == entry.as_bytes()))
        .unwrap_or(false)
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

/// The value of `/proc/sys/<path>` (e.g. `net/ipv4/tcp_migrate_req`).
pub fn sysctl(path: &str) -> Option<String> {
    std::fs::read_to_string(format!("/proc/sys/{path}")).ok().map(|s| s.trim().to_string())
}

pub fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:")).map(|v| v.split_whitespace().next() == Some("0")))
        .unwrap_or(false)
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
        assert!(pids().contains(&me));
    }
}

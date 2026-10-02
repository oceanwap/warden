//! The Linux adapter: everything from `/proc`.

use super::{Capabilities, CpuTimes, Environ, HostSnapshot, Platform, ProcStats};
use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

pub(crate) struct Linux;

impl Platform for Linux {
    fn name(&self) -> &'static str {
        "linux"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            proc_stats: true,
            proc_owner: true,
            listening_ports: true,
            proc_environ: true,
            host_stats: true,
            reuseport_balances: true,
            parent_death_signal: true,
            oom_attribution: true,
        }
    }

    fn proc_stats(&self, pid: u32) -> Option<ProcStats> {
        let statm = std::fs::read_to_string(format!("/proc/{pid}/statm")).ok()?;
        let resident: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let ticks = parse_stat_cpu_ticks(&stat)?;
        let (page, hz) = (crate::sys::page_size(), crate::sys::clock_ticks());
        Some(ProcStats { rss_bytes: resident * page, cpu_seconds: ticks as f64 / hz as f64 })
    }

    fn proc_owner(&self, pid: u32) -> Option<u32> {
        // The owner of /proc/<pid> is the process's effective user.
        std::fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
    }

    fn proc_environ(&self, pid: u32) -> Option<Environ> {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        Some(parse_environ(&raw))
    }

    fn proc_cwd(&self, pid: u32) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    fn proc_name(&self, pid: u32) -> Option<String> {
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        Some(comm.trim().to_string())
    }

    fn listening_ports(&self, pid: u32) -> Option<Vec<u16>> {
        let inodes = socket_inodes(pid)?;
        if inodes.is_empty() {
            return Some(Vec::new());
        }
        let mut ports = Vec::new();
        // /proc/<pid>/net/* shows the process's own network namespace.
        for table in ["tcp", "tcp6"] {
            if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
                ports.extend(parse_listening_ports(&text, &inodes));
            }
        }
        Some(ports)
    }

    fn command_lines(&self) -> Vec<(u32, String)> {
        let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
        dir.filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_string_lossy().parse::<u32>().ok())
            .filter_map(|pid| {
                let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let line: Vec<String> = raw
                    .split(|b| *b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();
                // A kernel thread has no command line; a process that set
                // its title (PM2's daemon does) has it in one string.
                (!line.is_empty()).then(|| (pid, line.join(" ")))
            })
            .collect()
    }

    fn host_snapshot(&self) -> Option<HostSnapshot> {
        let mut buf = String::new();
        let mut read = |path: &str| -> Option<String> {
            buf.clear();
            std::fs::File::open(path).ok()?.read_to_string(&mut buf).ok()?;
            Some(buf.clone())
        };
        let cpu = parse_cpu(&read("/proc/stat")?)?;
        let (mem_used_bytes, mem_total_bytes) = parse_meminfo(&read("/proc/meminfo")?)?;
        let load = parse_loadavg(&read("/proc/loadavg")?)?;
        Some(HostSnapshot { cpu, mem_used_bytes, mem_total_bytes, load })
    }

    fn boot_id(&self) -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok().map(|s| s.trim().to_string())
    }
}

/// utime + stime (clock ticks) from the contents of /proc/<pid>/stat.
/// The command name may contain spaces and parens, so split after the last ')'.
pub(crate) fn parse_stat_cpu_ticks(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    // f[0] is field 3 (state); utime is field 14, stime field 15.
    Some(f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?)
}

/// `NAME=value` entries separated by NULs; entries without `=` are skipped.
pub(crate) fn parse_environ(raw: &[u8]) -> Environ {
    raw.split(|b| *b == 0)
        .filter_map(|kv| {
            let i = kv.iter().position(|b| *b == b'=')?;
            Some((OsString::from_vec(kv[..i].to_vec()), OsString::from_vec(kv[i + 1..].to_vec())))
        })
        .collect()
}

/// The inodes of the sockets a process holds open.
fn socket_inodes(pid: u32) -> Option<HashSet<u64>> {
    let dir = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    let mut set = HashSet::new();
    for entry in dir.flatten() {
        if let Ok(target) = std::fs::read_link(entry.path()) {
            let t = target.to_string_lossy();
            if let Some(inode) = t.strip_prefix("socket:[").and_then(|s| s.strip_suffix(']')) {
                if let Ok(i) = inode.parse() {
                    set.insert(i);
                }
            }
        }
    }
    Some(set)
}

/// The local ports of the rows of a /proc/net/tcp{,6} table in LISTEN (0A)
/// whose inode is in `inodes`: one per socket.
pub(crate) fn parse_listening_ports(text: &str, inodes: &HashSet<u64>) -> Vec<u16> {
    text.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // sl local rem st tx:rx tr:when retrnsmt uid timeout inode
            if f.len() > 9 && f[3] == "0A" && f[9].parse::<u64>().is_ok_and(|i| inodes.contains(&i)) {
                u16::from_str_radix(f[1].rsplit(':').next()?, 16).ok()
            } else {
                None
            }
        })
        .collect()
}

/// The aggregate `cpu` line of /proc/stat: jiffies since boot, all of them
/// and the busy ones (not idle or iowait).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_parsing_handles_spaces_in_comm() {
        let stat = "1234 (my (weird) app) S 1 1234 1234 0 -1 4194304 100 0 0 0 250 50 0 0 20 0 5 0 100 1000 200";
        assert_eq!(parse_stat_cpu_ticks(stat), Some(300));
    }

    #[test]
    fn environment_entries_split_at_the_first_equals() {
        let env = parse_environ(b"A=1\0B=x=y\0NOEQ\0C=\0");
        let os = |s: &str| OsString::from(s);
        assert_eq!(env, [(os("A"), os("1")), (os("B"), os("x=y")), (os("C"), os(""))]);
        assert!(parse_environ(b"").is_empty());
    }

    #[test]
    fn parses_proc_net_tcp() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:9A0D 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1173 2 0000000000000000 100 0 0 10 0
   1: 00000000:0C1C 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 11501 1 0000000000000000 100 0 0 10 0
   2: 00000000:0C1C 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4938 1 0000000000000000 100 0 0 10 0
   3: 0100007F:0C1C 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 5555 1 0000000000000000 100 0 0 10 0";
        let mine: HashSet<u64> = [11501, 5555, 1173].into_iter().collect();
        // Row 1 (inode 11501, port 0x0C1C = 3100) and row 0 (1173, 0x9A0D = 39437) listen and
        // are ours; row 2 is not ours; row 3 is established, not listening.
        assert_eq!(parse_listening_ports(text, &mine), [0x9A0D, 3100]);
        let only: HashSet<u64> = [11501].into_iter().collect();
        assert_eq!(parse_listening_ports(text, &only), [3100]);
        assert!(parse_listening_ports(text, &HashSet::new()).is_empty());
    }

    const STAT: &str = "cpu  4705 356 584 3699176 23 0 11 0 0 0\n\
                        cpu0 1393280 32966 572056 13343292 6130 0 17875 0 23933 0\n\
                        intr 114930548 113199788 3 0 5 263 0 4 [...]\n";

    #[test]
    fn cpu() {
        let t = parse_cpu(STAT).unwrap();
        assert_eq!(t.total, 4705 + 356 + 584 + 3699176 + 23 + 11);
        assert_eq!(t.busy, 4705 + 356 + 584 + 11);
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
    fn the_command_name_and_owner_of_pid_1_are_readable() {
        assert!(Linux.proc_name(1).is_some_and(|n| !n.is_empty()));
        assert_eq!(Linux.proc_owner(1), Some(0));
    }
}

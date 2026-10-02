//! The Linux adapter: everything from `/proc`.

use super::{Capabilities, CpuTimes, Environ, HostSnapshot, Listener, Platform, ProcStats};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::io::{BufRead, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

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
        let tcp = tcp_listeners(pid, &inodes);
        Some(
            tcp.into_iter()
                .filter_map(|l| if let Listener::Tcp { port, .. } = l { Some(port) } else { None })
                .collect(),
        )
    }

    fn listeners(&self, pid: u32) -> Option<Vec<Listener>> {
        self.listeners_of(&[pid])
    }

    fn listeners_of(&self, pids: &[u32]) -> Option<Vec<Listener>> {
        // The socket tables belong to a network namespace and have a row per
        // connection (megabytes on a busy host): each is read once for all the
        // processes in the namespace, not once per process.
        let mut spaces: Vec<(Option<PathBuf>, u32, HashSet<u64>)> = Vec::new();
        for (i, pid) in pids.iter().enumerate() {
            let Some(inodes) = socket_inodes(*pid) else {
                if i == 0 {
                    return None;
                }
                continue;
            };
            if inodes.is_empty() {
                continue;
            }
            let ns = std::fs::read_link(format!("/proc/{pid}/ns/net")).ok();
            match spaces.iter_mut().find(|s| s.0 == ns) {
                Some(s) => s.2.extend(inodes),
                None => spaces.push((ns, *pid, inodes)),
            }
        }
        let mut found = Vec::new();
        for (_, pid, inodes) in &spaces {
            found.extend(tcp_listeners(*pid, inodes));
            for_each_row(&format!("/proc/{pid}/net/unix"), |row| found.extend(parse_unix_row(row, inodes)));
        }
        Some(found)
    }

    fn children(&self, pid: u32) -> Option<Vec<u32>> {
        // The children of every thread: a child belongs to the thread that
        // forked it, and Node and Bun fork from whichever thread asked.
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).ok()?;
        if !has_task_children() {
            return Some(scan_children(pid));
        }
        let mut kids = Vec::new();
        for task in tasks.flatten().take(MAX_TASKS) {
            // A thread that ended since the listing has no file: it has no children.
            if let Ok(text) = std::fs::read_to_string(task.path().join("children")) {
                kids.extend(text.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
            }
        }
        kids.sort_unstable();
        kids.dedup();
        Some(kids)
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

/// The most threads of one process whose children are read (a Node or Bun
/// process has a few dozen).
const MAX_TASKS: usize = 512;

/// `/proc/<pid>/task/<tid>/children` exists (`CONFIG_PROC_CHILDREN`: distro
/// kernels have it, some minimal ones do not); checked once.
fn has_task_children() -> bool {
    static V: OnceLock<bool> = OnceLock::new();
    *V.get_or_init(|| {
        let me = std::process::id();
        std::fs::metadata(format!("/proc/{me}/task/{me}/children")).is_ok()
    })
}

/// Without the `children` files: every process whose parent is `pid`. One
/// pass over `/proc/*/stat` answers all the questions of the next second (a
/// walk of a process tree asks once per process).
fn scan_children(pid: u32) -> Vec<u32> {
    /// When the pass was taken, and the processes by their parent.
    type Pass = Option<(Instant, HashMap<u32, Vec<u32>>)>;
    static PASS: Mutex<Pass> = Mutex::new(None);
    let mut pass = PASS.lock().unwrap_or_else(PoisonError::into_inner);
    if !matches!(&*pass, Some((at, _)) if at.elapsed() < Duration::from_secs(1)) {
        *pass = Some((Instant::now(), children_by_parent()));
    }
    pass.as_ref().and_then(|(_, map)| map.get(&pid).cloned()).unwrap_or_default()
}

/// Every process by its parent, from `/proc/*/stat`.
fn children_by_parent() -> HashMap<u32, Vec<u32>> {
    let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return map };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_string_lossy().parse::<u32>().ok() else { continue };
        if let Some(ppid) = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok().and_then(|s| parse_stat_ppid(&s))
        {
            map.entry(ppid).or_default().push(pid);
        }
    }
    for kids in map.values_mut() {
        kids.sort_unstable();
    }
    map
}

/// The parent's pid (field 4) from the contents of /proc/<pid>/stat.
pub(crate) fn parse_stat_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // f[0] is field 3 (state); the parent is field 4.
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The TCP listeners of a process, from the tables of its own network
/// namespace (`/proc/<pid>/net/*`).
fn tcp_listeners(pid: u32, inodes: &HashSet<u64>) -> Vec<Listener> {
    let mut found = Vec::new();
    for table in ["tcp", "tcp6"] {
        for_each_row(&format!("/proc/{pid}/net/{table}"), |row| found.extend(parse_tcp_row(row, inodes)));
    }
    found
}

/// Each row of a /proc/net table (every line after the header), streamed: a
/// busy host's tables are megabytes. Text that is not UTF-8 is read lossily:
/// one odd socket name must not hide every other socket.
fn for_each_row(path: &str, mut f: impl FnMut(&str)) {
    let Ok(file) = std::fs::File::open(path) else { return };
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut line = Vec::new();
    let mut header = true;
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if std::mem::take(&mut header) {
            continue;
        }
        f(String::from_utf8_lossy(&line).trim_end_matches('\n'));
    }
}

/// The address of a /proc/net/tcp{,6} column: the kernel prints the
/// address as 32-bit words in the machine's byte order, one word for IPv4 and
/// four for IPv6.
fn parse_hex_addr(hex: &str) -> Option<IpAddr> {
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(u32::from_str_radix(hex, 16).ok()?.to_ne_bytes()))),
        32 => {
            let mut bytes = [0u8; 16];
            for (out, word) in bytes.chunks_exact_mut(4).zip(hex.as_bytes().chunks_exact(8)) {
                let w = u32::from_str_radix(std::str::from_utf8(word).ok()?, 16).ok()?;
                out.copy_from_slice(&w.to_ne_bytes());
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

/// One row of /proc/net/tcp{,6}: a listener (state 0A) whose inode is in
/// `inodes`, with the address it is bound to.
fn parse_tcp_row(row: &str, inodes: &HashSet<u64>) -> Option<Listener> {
    // sl local rem st tx:rx tr:when retrnsmt uid timeout inode: no allocation
    // for the many rows that are not listeners.
    let mut f = row.split_whitespace();
    let local = f.nth(1)?;
    if f.nth(1)? != "0A" {
        return None;
    }
    if !f.nth(5)?.parse::<u64>().is_ok_and(|i| inodes.contains(&i)) {
        return None;
    }
    let (addr, port) = local.rsplit_once(':')?;
    Some(Listener::Tcp { port: u16::from_str_radix(port, 16).ok()?, addr: parse_hex_addr(addr)? })
}

/// The rows of a /proc/net/tcp{,6} table that are ours listeners.
#[cfg(test)]
pub(crate) fn parse_tcp_listeners(text: &str, inodes: &HashSet<u64>) -> Vec<Listener> {
    text.lines().skip(1).filter_map(|row| parse_tcp_row(row, inodes)).collect()
}

/// One row of /proc/net/unix: a socket that accepts connections
/// (`__SO_ACCEPTCON` in the flags), has a name, and whose inode is in
/// `inodes`. A path that starts with `@` is an abstract socket (the kernel
/// prints its NUL that way).
fn parse_unix_row(row: &str, inodes: &HashSet<u64>) -> Option<Listener> {
    /// `__SO_ACCEPTCON` (include/linux/net.h).
    const ACCEPTING: u32 = 1 << 16;
    // Num RefCount Protocol Flags Type St Inode [Path]; the inode is padded with spaces.
    let mut rest = row;
    let mut fields = [""; 7];
    for slot in &mut fields {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        (*slot, rest) = rest.split_at(end);
    }
    let [_num, _refs, _proto, flags, _kind, _state, inode] = fields;
    let path = rest.trim_start();
    let accepting = u32::from_str_radix(flags, 16).ok()? & ACCEPTING != 0;
    let mine = inode.parse::<u64>().is_ok_and(|i| inodes.contains(&i));
    (accepting && mine && !path.is_empty()).then(|| Listener::Unix { path: path.to_string() })
}

/// The rows of a /proc/net/unix table that are our listeners.
#[cfg(test)]
pub(crate) fn parse_unix_listeners(text: &str, inodes: &HashSet<u64>) -> Vec<Listener> {
    text.lines().skip(1).filter_map(|row| parse_unix_row(row, inodes)).collect()
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

    #[cfg(target_endian = "little")]
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
        let tcp = |port, addr: &str| Listener::Tcp { port, addr: addr.parse().unwrap() };
        assert_eq!(parse_tcp_listeners(text, &mine), [tcp(0x9A0D, "127.0.0.1"), tcp(3100, "0.0.0.0")]);
        let only: HashSet<u64> = [11501].into_iter().collect();
        assert_eq!(parse_tcp_listeners(text, &only), [tcp(3100, "0.0.0.0")]);
        assert!(parse_tcp_listeners(text, &HashSet::new()).is_empty());
    }

    #[cfg(target_endian = "little")]
    #[test]
    fn parses_proc_net_tcp6_addresses() {
        let text = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000000000000:1F90 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 700 1 0000000000000000 100 0 0 10 0
   1: 00000000000000000000000001000000:1F91 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 701 1 0000000000000000 100 0 0 10 0
   2: 0000000000000000FFFF00000100007F:1F92 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 702 1 0000000000000000 100 0 0 10 0
   3: 000080FE00000000FF0F58E5F60FB4D8:1F93 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 703 1 0000000000000000 100 0 0 10 0";
        let mine: HashSet<u64> = [700, 701, 702, 703].into_iter().collect();
        let got = parse_tcp_listeners(text, &mine);
        let want = |port, addr: &str| Listener::Tcp { port, addr: addr.parse().unwrap() };
        assert_eq!(
            got,
            [
                want(8080, "::"),
                want(8081, "::1"),
                want(8082, "::ffff:127.0.0.1"),
                want(8083, "fe80::e558:fff:d8b4:ff6")
            ]
        );
        assert_eq!(parse_hex_addr("0100007"), None, "a short word");
        assert_eq!(parse_hex_addr("zz00007F"), None);
    }

    #[test]
    fn parses_proc_net_unix() {
        let text = "Num       RefCount Protocol Flags    Type St Inode Path
ffff9d0ac7e46000: 00000002 00000000 00010000 0001 01 21490 /run/dbus/system_bus_socket
ffff9d0ac7e47400: 00000002 00000000 00010000 0001 01  9001 @/tmp/dbus-abstract
ffff9d0ac7e48800: 00000003 00000000 00000000 0001 03 21491 /run/dbus/system_bus_socket
ffff9d0ac7e49c00: 00000002 00000000 00010000 0001 01 21492
ffff9d0ac7e4a000: 00000002 00000000 00010000 0005 01 21493 /tmp/path with spaces/x.sock
ffff9d0ac7e4b400: 00000002 00000000 00010000 0001 01 55555 /not/ours.sock";
        let mine: HashSet<u64> = [21490, 9001, 21491, 21492, 21493].into_iter().collect();
        let unix = |p: &str| Listener::Unix { path: p.into() };
        // 21491 is a connected socket (no ACCEPTCON), 21492 has no name, 55555 is not ours.
        assert_eq!(
            parse_unix_listeners(text, &mine),
            [unix("/run/dbus/system_bus_socket"), unix("@/tmp/dbus-abstract"), unix("/tmp/path with spaces/x.sock")]
        );
        assert!(parse_unix_listeners("Num RefCount Protocol Flags Type St Inode Path\nshort line\n", &mine).is_empty());
    }

    #[test]
    fn a_table_with_a_name_that_is_not_utf8_still_gives_the_other_sockets() {
        let dir = std::env::temp_dir().join(format!("wl-rows-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("unix");
        let mut text = b"Num RefCount Protocol Flags Type St Inode Path\n".to_vec();
        text.extend_from_slice(b"0000000000000000: 00000002 00000000 00010000 0001 01 21490 /run/ok.sock\n");
        text.extend_from_slice(b"0000000000000000: 00000002 00000000 00010000 0001 01  9001 @\xff\xfe-abstract\n");
        text.extend_from_slice(b"0000000000000000: 00000002 00000000 00010000 0001 01 21491 /run/also-ok.sock\n");
        std::fs::write(&file, &text).unwrap();
        let mine: HashSet<u64> = [21490, 9001, 21491].into_iter().collect();
        let mut found = Vec::new();
        for_each_row(file.to_str().unwrap(), |row| found.extend(parse_unix_row(row, &mine)));
        let _ = std::fs::remove_dir_all(&dir);
        let paths: Vec<String> =
            found.into_iter().filter_map(|l| if let Listener::Unix { path } = l { Some(path) } else { None }).collect();
        assert_eq!(paths.len(), 3, "{paths:?}");
        assert!(
            paths[0] == "/run/ok.sock" && paths[1].starts_with('@') && paths[2] == "/run/also-ok.sock",
            "{paths:?}"
        );
    }

    #[test]
    fn processes_in_one_network_namespace_share_one_read_of_the_tables() {
        // Two listeners in this process and a child's: one call lists all of them.
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let found = Linux.listeners_of(&[std::process::id(), child.id()]).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        let port = a.local_addr().unwrap().port();
        assert!(found.iter().any(|l| matches!(l, Listener::Tcp { port: p, .. } if *p == port)), "{found:?}");
        assert_eq!(Linux.listeners_of(&[0x7fff_fff0, std::process::id()]), None, "the first must be readable");
        assert!(Linux.listeners_of(&[std::process::id(), 0x7fff_fff0]).is_some(), "a later one may be gone");
    }

    #[test]
    fn the_parent_is_field_four_of_stat() {
        assert_eq!(parse_stat_ppid("1234 (my (weird) app) S 77 1234 1234 0 -1 4194304"), Some(77));
        assert_eq!(parse_stat_ppid("garbage"), None);
    }

    #[test]
    fn scanning_for_children_agrees_with_the_children_files() {
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let me = std::process::id();
        let scanned = children_by_parent().remove(&me).unwrap_or_default();
        let by_file = Linux.children(me).unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(scanned.contains(&child.id()) && by_file.contains(&child.id()), "{scanned:?} {by_file:?}");
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

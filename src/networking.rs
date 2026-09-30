//! Port handling. Warden never accepts or proxies application traffic; workers
//! bind the port themselves with SO_REUSEPORT and the kernel balances
//! connections. Warden only decides which port each worker uses and checks,
//! from outside, whether a worker is listening.

use crate::config::PortStrategy;

/// The port worker `id` (1-based) should listen on.
pub fn worker_port(base: u16, strategy: PortStrategy, id: usize) -> u16 {
    match strategy {
        PortStrategy::Shared => base,
        PortStrategy::Offset => base + (id as u16 - 1),
    }
}

/// Number of TCP sockets in LISTEN state on `port` owned by `pid`.
/// Linux only (reads /proc); `None` elsewhere or if the process is gone.
pub fn count_listeners(pid: u32, port: u16) -> Option<usize> {
    #[cfg(target_os = "linux")]
    {
        linux::count_listeners(pid, port)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, port);
        None
    }
}

/// Fallback readiness probe for platforms without /proc: can we connect?
/// Imprecise with a shared port (any worker may answer).
pub fn port_accepts(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::HashSet;

    pub fn count_listeners(pid: u32, port: u16) -> Option<usize> {
        let inodes = socket_inodes(pid)?;
        if inodes.is_empty() {
            return Some(0);
        }
        let mut n = 0;
        // /proc/<pid>/net/* shows the process's own network namespace.
        for table in ["tcp", "tcp6"] {
            if let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) {
                n += parse_listeners(&text, port, &inodes);
            }
        }
        Some(n)
    }

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

    /// Count rows of a /proc/net/tcp{,6} table in LISTEN (0A) on `port` whose
    /// inode is in `inodes`.
    pub(super) fn parse_listeners(text: &str, port: u16, inodes: &HashSet<u64>) -> usize {
        let want = format!("{port:04X}");
        text.lines()
            .skip(1)
            .filter(|line| {
                let f: Vec<&str> = line.split_whitespace().collect();
                // sl local rem st tx:rx tr:when retrnsmt uid timeout inode
                f.len() > 9
                    && f[3] == "0A"
                    && f[1].rsplit(':').next() == Some(want.as_str())
                    && f[9].parse::<u64>().is_ok_and(|i| inodes.contains(&i))
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        assert_eq!(worker_port(3000, PortStrategy::Shared, 4), 3000);
        assert_eq!(worker_port(3000, PortStrategy::Offset, 1), 3000);
        assert_eq!(worker_port(3000, PortStrategy::Offset, 4), 3003);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_proc_net_tcp() {
        let text = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:9A0D 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 1173 2 0000000000000000 100 0 0 10 0
   1: 00000000:0C1C 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 11501 1 0000000000000000 100 0 0 10 0
   2: 00000000:0C1C 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4938 1 0000000000000000 100 0 0 10 0
   3: 0100007F:0C1C 0100007F:D2F0 01 00000000:00000000 00:00000000 00000000     0        0 5555 1 0000000000000000 100 0 0 10 0";
        let mine: std::collections::HashSet<u64> = [11501, 5555].into_iter().collect();
        assert_eq!(linux::parse_listeners(text, 3100, &mine), 1);
        assert_eq!(linux::parse_listeners(text, 3101, &mine), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn counts_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        assert_eq!(count_listeners(std::process::id(), port), Some(1));
        drop(l);
        assert_eq!(count_listeners(std::process::id(), port), Some(0));
    }
}

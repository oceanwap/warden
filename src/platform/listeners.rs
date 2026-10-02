//! The sockets a process listens on, and the walk down its children that
//! finds them when a wrapper (`npm run start`, `turbo`, `bun run`, a shell
//! script) is the process Warden started and the server is its grandchild.

use super::Platform;
use std::collections::HashSet;
use std::net::IpAddr;

/// A socket a process accepts connections on.
///
/// The derived order is the order Warden shows: TCP before Unix, by port,
/// then by address; Unix by path.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Listener {
    /// A TCP socket in LISTEN: its port and the address it is bound to
    /// (`0.0.0.0` or `::` for every interface).
    Tcp { port: u16, addr: IpAddr },
    /// A Unix domain socket that accepts connections: its path; a Linux
    /// abstract socket (it has no file) is `@name`.
    Unix { path: String },
}

/// The most processes a walk looks at, and how deep it goes: a server tree
/// is a shell, a package manager and the server, sometimes a worker per CPU.
/// A fork bomb below the app must not turn every status call into a scan.
pub(super) const MAX_PROCESSES: usize = 64;
pub(super) const MAX_DEPTH: usize = 4;

/// What `root` and the processes below it listen on, sorted and without
/// duplicates (processes that share a socket, like a forking server's
/// workers, report it once). `None` when `root` itself cannot be read: it is
/// gone, belongs to another user, or the OS has no adapter. A child that
/// cannot be read is skipped.
pub(super) fn of_tree(p: &dyn Platform, root: u32) -> Option<Vec<Listener>> {
    let mut found = p.listeners_of(&tree(p, root))?;
    found.sort();
    found.dedup();
    Some(found)
}

/// `root` and its descendants, parents before children, within the limits.
fn tree(p: &dyn Platform, root: u32) -> Vec<u32> {
    let mut pids = vec![root];
    let mut seen: HashSet<u32> = HashSet::from([root]);
    let mut level = vec![root];
    'walk: for _ in 0..MAX_DEPTH {
        let mut next = Vec::new();
        for pid in level {
            for child in p.children(pid).unwrap_or_default() {
                if pids.len() >= MAX_PROCESSES {
                    break 'walk;
                }
                if seen.insert(child) {
                    pids.push(child);
                    next.push(child);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        level = next;
    }
    pids
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{Capabilities, Environ, HostSnapshot, ProcStats};
    use std::collections::HashMap;
    use std::net::Ipv4Addr;
    use std::path::PathBuf;
    use std::sync::Mutex;

    /// An OS made of a table: pid → (children, listeners).
    #[derive(Default)]
    struct Fake {
        tree: HashMap<u32, (Vec<u32>, Vec<Listener>)>,
        asked: Mutex<Vec<u32>>,
    }

    impl Platform for Fake {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn capabilities(&self) -> Capabilities {
            super::super::other::none()
        }
        fn proc_stats(&self, _: u32) -> Option<ProcStats> {
            None
        }
        fn proc_owner(&self, _: u32) -> Option<u32> {
            None
        }
        fn proc_environ(&self, _: u32) -> Option<Environ> {
            None
        }
        fn proc_cwd(&self, _: u32) -> Option<PathBuf> {
            None
        }
        fn proc_name(&self, _: u32) -> Option<String> {
            None
        }
        fn listening_ports(&self, _: u32) -> Option<Vec<u16>> {
            None
        }
        fn listeners(&self, pid: u32) -> Option<Vec<Listener>> {
            self.asked.lock().unwrap().push(pid);
            self.tree.get(&pid).map(|t| t.1.clone())
        }
        fn children(&self, pid: u32) -> Option<Vec<u32>> {
            self.tree.get(&pid).map(|t| t.0.clone())
        }
        fn command_lines(&self) -> Vec<(u32, String)> {
            Vec::new()
        }
        fn host_snapshot(&self) -> Option<HostSnapshot> {
            None
        }
        fn boot_id(&self) -> Option<String> {
            None
        }
    }

    fn tcp(port: u16) -> Listener {
        Listener::Tcp { port, addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED) }
    }

    #[test]
    fn a_wrapper_shows_the_ports_of_the_server_below_it() {
        // npm (10) → sh (11) → node (12, listens on 3001 and a socket); a sibling tool (13) on 9229.
        let mut f = Fake::default();
        f.tree.insert(10, (vec![11, 13], vec![]));
        f.tree.insert(11, (vec![12], vec![]));
        f.tree.insert(12, (vec![], vec![Listener::Unix { path: "/tmp/a.sock".into() }, tcp(3001)]));
        f.tree.insert(13, (vec![], vec![tcp(9229)]));
        let got = of_tree(&f, 10).unwrap();
        assert_eq!(got, [tcp(3001), tcp(9229), Listener::Unix { path: "/tmp/a.sock".into() }]);
    }

    #[test]
    fn processes_that_share_a_socket_report_it_once() {
        let mut f = Fake::default();
        f.tree.insert(1, (vec![2, 3], vec![tcp(80)]));
        f.tree.insert(2, (vec![], vec![tcp(80)]));
        f.tree.insert(3, (vec![], vec![tcp(80), tcp(81)]));
        assert_eq!(of_tree(&f, 1).unwrap(), [tcp(80), tcp(81)]);
    }

    #[test]
    fn the_root_that_cannot_be_read_is_none_but_an_unreadable_child_is_skipped() {
        let mut f = Fake::default();
        f.tree.insert(1, (vec![2, 3], vec![tcp(80)]));
        f.tree.insert(3, (vec![], vec![tcp(81)])); // 2 is another user's: no answer
        assert_eq!(of_tree(&f, 1).unwrap(), [tcp(80), tcp(81)]);
        assert_eq!(of_tree(&f, 99), None);
    }

    #[test]
    fn a_wide_tree_is_cut_off_and_a_parent_loop_ends() {
        let mut f = Fake::default();
        let kids: Vec<u32> = (2..500).collect();
        f.tree.insert(1, (kids.clone(), vec![]));
        for k in kids {
            f.tree.insert(k, (vec![], vec![tcp(k as u16)]));
        }
        let got = of_tree(&f, 1).unwrap();
        assert_eq!(got.len(), MAX_PROCESSES - 1, "root plus {} others looked at", MAX_PROCESSES - 1);
        // pid reuse can make a process its own ancestor: each is asked once.
        let mut g = Fake::default();
        g.tree.insert(1, (vec![2], vec![]));
        g.tree.insert(2, (vec![1, 2], vec![tcp(8)]));
        assert_eq!(of_tree(&g, 1).unwrap(), [tcp(8)]);
        assert_eq!(g.asked.lock().unwrap().iter().filter(|p| **p == 1).count(), 1);
    }

    #[test]
    fn the_walk_is_as_deep_as_the_limit() {
        let mut f = Fake::default();
        for pid in 1..=(MAX_DEPTH as u32 + 2) {
            f.tree.insert(pid, (vec![pid + 1], vec![tcp(pid as u16)]));
        }
        let got = of_tree(&f, 1).unwrap();
        // The root and MAX_DEPTH generations below it.
        assert_eq!(got.len(), MAX_DEPTH + 1, "{got:?}");
    }

    #[test]
    fn the_order_is_tcp_by_port_then_unix() {
        let a = Listener::Tcp { port: 80, addr: "::".parse().unwrap() };
        let b = Listener::Tcp { port: 80, addr: "0.0.0.0".parse().unwrap() };
        let mut v = vec![
            Listener::Unix { path: "/b".into() },
            tcp(443),
            a.clone(),
            Listener::Unix { path: "/a".into() },
            b.clone(),
        ];
        v.sort();
        assert_eq!(v, [b, a, tcp(443), Listener::Unix { path: "/a".into() }, Listener::Unix { path: "/b".into() }]);
    }
}

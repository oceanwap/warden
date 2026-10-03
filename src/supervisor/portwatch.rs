//! `[watchdog] port_lost`: a worker that listened on a TCP port and then has
//! none for that long, while it runs, is alive but serving nothing (a dev
//! server whose app crashed and that waits for a file change, a server closed
//! by an error the app caught). It is stopped and restarted like a crash, with
//! the same backoff, so one that keeps losing its port ends FAILED.
//!
//! Everything is read from outside: the kernel's socket tables, never the
//! process. On Linux, one netlink request lists the TCP sockets in LISTEN of
//! the namespace (however many connections the host has), and a worker that
//! still holds one of the sockets it was last seen with is listening; only a
//! worker whose sockets are gone has its process tree walked (`/proc/<pid>/fd`)
//! to find new ones. Elsewhere, and for a worker in another network
//! namespace, its tree's listeners are read the way `warden list` reads them.
//! A worker that never listened is walked every two seconds while it is young,
//! then every 30, and never acted on: a worker without ports is left alone.

use super::*;
use crate::platform;
use crate::sys::TcpListen;
use std::collections::HashSet;

/// How often the workers are looked at, in ticks (seconds).
pub(super) const EVERY_TICKS: u64 = 2;
/// A worker not seen listening yet is walked every tick of the check while it
/// is this young, then every [`WALK_LATER`].
const WALK_YOUNG: Duration = Duration::from_secs(60);
const WALK_LATER: Duration = Duration::from_secs(30);

/// One worker to look at.
#[derive(Debug, Clone)]
pub(super) struct Target {
    pub(super) inst: u64,
    pub(super) pid: u32,
    /// The listening sockets it was last seen with.
    pub(super) known: Vec<u64>,
}

/// What a look found.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Seen {
    /// Listening: on these sockets (empty where the OS gives no inodes) and ports.
    Listening {
        inodes: Vec<u64>,
        ports: Vec<u16>,
    },
    Nothing,
    /// The process could not be read (gone, another user's): no verdict.
    Unknown,
}

/// How the workers are read: the kernel's listening sockets of Warden's
/// namespace (`None`: no such list here), and the per-process lookups.
pub(super) trait Look {
    fn listening(&self) -> Option<Vec<TcpListen>>;
    fn same_namespace(&self, pid: u32) -> bool;
    /// The sockets the process tree holds (inodes).
    fn tree_sockets(&self, pid: u32) -> Option<HashSet<u64>>;
    /// The TCP ports the process tree listens on, read the long way.
    fn tree_ports(&self, pid: u32) -> Option<Vec<u16>>;
}

/// The real OS.
pub(super) struct Os;

impl Look for Os {
    fn listening(&self) -> Option<Vec<TcpListen>> {
        crate::sys::tcp_listeners().ok()
    }

    fn same_namespace(&self, pid: u32) -> bool {
        let mine = platform::net_namespace(std::process::id());
        mine.is_some() && platform::net_namespace(pid) == mine
    }

    fn tree_sockets(&self, pid: u32) -> Option<HashSet<u64>> {
        platform::socket_inodes_of_tree(pid)
    }

    fn tree_ports(&self, pid: u32) -> Option<Vec<u16>> {
        let found = platform::listeners_whole(pid)?;
        Some(
            found
                .into_iter()
                .filter_map(|l| match l {
                    platform::Listener::Tcp { port, .. } => Some(port),
                    platform::Listener::Unix { .. } => None,
                })
                .collect(),
        )
    }
}

/// Look at each target: one list of the listening sockets for all of them.
pub(super) fn look(os: &dyn Look, targets: &[Target]) -> Vec<(u64, Seen)> {
    let table = os.listening();
    targets.iter().map(|t| (t.inst, look_one(os, table.as_deref(), t))).collect()
}

fn look_one(os: &dyn Look, table: Option<&[TcpListen]>, t: &Target) -> Seen {
    match table {
        Some(table) if os.same_namespace(t.pid) => {
            // Still holding one of the sockets it had: listening.
            let still: Vec<&TcpListen> = table.iter().filter(|l| t.known.contains(&l.inode)).collect();
            if !still.is_empty() {
                return listening(still);
            }
            let Some(held) = os.tree_sockets(t.pid) else { return Seen::Unknown };
            let mine: Vec<&TcpListen> = table.iter().filter(|l| held.contains(&l.inode)).collect();
            if mine.is_empty() { Seen::Nothing } else { listening(mine) }
        }
        _ => match os.tree_ports(t.pid) {
            None => Seen::Unknown,
            Some(ports) if ports.is_empty() => Seen::Nothing,
            Some(mut ports) => {
                ports.sort_unstable();
                ports.dedup();
                Seen::Listening { inodes: Vec::new(), ports }
            }
        },
    }
}

fn listening(sockets: Vec<&TcpListen>) -> Seen {
    let mut ports: Vec<u16> = sockets.iter().map(|l| l.port).collect();
    ports.sort_unstable();
    ports.dedup();
    Seen::Listening { inodes: sockets.iter().map(|l| l.inode).collect(), ports }
}

/// What a worker's reading means: whether it has now gone `after` without
/// listening, having listened before. Updates what is known of it.
pub(super) fn verdict(
    w: &mut crate::worker::ListenWatch,
    seen: Seen,
    now: Instant,
    started: Instant,
    after: Duration,
) -> bool {
    match seen {
        Seen::Unknown => false,
        Seen::Listening { inodes, ports } => {
            w.inodes = inodes;
            w.ports = ports;
            w.seen_at = Some(now);
            w.lost_since = None;
            w.next_walk = None;
            false
        }
        Seen::Nothing => {
            w.inodes.clear();
            if w.seen_at.is_none() {
                // Never listened: not watched; walked less often as it ages.
                let young = now.saturating_duration_since(started) < WALK_YOUNG;
                w.next_walk = (!young).then(|| now + WALK_LATER);
                return false;
            }
            let since = *w.lost_since.get_or_insert(now);
            now.saturating_duration_since(since) >= after
        }
    }
}

impl Supervisor {
    /// The workers watched: running ones (not starting, stopping, hung, or
    /// already being restarted for this), and only while Warden runs them.
    fn port_targets(&self, now: Instant) -> Vec<Target> {
        self.insts
            .iter()
            .filter(|(_, i)| {
                i.role == Role::Current
                    && i.ready_at.is_some()
                    && !i.stopping
                    && !i.hung
                    && !i.port_lost
                    && i.listen.next_walk.is_none_or(|t| now >= t)
            })
            .map(|(id, i)| Target { inst: *id, pid: i.handle.pid, known: i.listen.inodes.clone() })
            .collect()
    }

    /// Every [`EVERY_TICKS`]: look at the workers, off the event loop.
    pub(super) fn check_ports(&mut self) {
        // Without restarts (`[restart] enabled = false`) stopping it would only leave the slot down.
        let off = self.cfg.watchdog.port_lost == 0 || !self.cfg.restart.enabled;
        if off || self.port_look_inflight || self.shutting_down || self.stopped {
            return;
        }
        let targets = self.port_targets(Instant::now());
        if targets.is_empty() {
            return;
        }
        self.port_look_inflight = true;
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            // A panic must still answer, or the workers are never looked at again.
            let seen = std::panic::catch_unwind(|| look(&Os, &targets)).unwrap_or_default();
            let _ = tx.send(Event::Ports(seen));
        });
    }

    /// What a look found: restart the workers that stopped listening.
    pub(super) fn on_ports(&mut self, seen: Vec<(u64, Seen)>) {
        self.port_look_inflight = false;
        let after = Duration::from_secs(self.cfg.watchdog.port_lost);
        if after.is_zero() || self.shutting_down || self.stopped {
            return;
        }
        let now = Instant::now();
        let mut lost = Vec::new();
        for (id, s) in seen {
            let Some(i) = self.insts.get_mut(&id) else { continue };
            // It may have changed while it was looked at.
            if i.role != Role::Current || i.stopping || i.hung || i.port_lost {
                continue;
            }
            if verdict(&mut i.listen, s, now, i.started, after) {
                lost.push(id);
            }
        }
        for id in lost {
            self.port_lost(id, after);
        }
    }

    /// Stop a worker that no longer listens, so it is restarted as a crash.
    fn port_lost(&mut self, id: u64, after: Duration) {
        let stop_signal = self.cfg.stop_signal();
        let grace = self.cfg.grace_period();
        let Some(i) = self.insts.get(&id) else { return };
        let (who, event_who) = (self.inst_label(i), self.event_who(i));
        let Some(i) = self.insts.get_mut(&id) else { return };
        i.port_lost = true;
        let pid = i.handle.pid;
        let ports = ports_text(&i.listen.ports);
        error!(
            "worker stopped listening; restarting it",
            worker = who,
            pid = pid,
            was_listening_on = ports.clone(),
            for_s = after.as_secs(),
            hint = "the process runs but holds no listening TCP socket any more, so it serves nothing: its app \
                    crashed under a wrapper that stays up (nodemon, a dev server waiting for a change), or it \
                    closed its server. Its last output is in `warden logs <app> --worker N`. Warden restarts it \
                    with the crash backoff; set [watchdog] port_lost (seconds, 0 = off) for apps that stop \
                    listening on purpose",
        );
        emit_to(&self.cfg.app.name, event_who, WorkerEvent::Unhealthy, Some(pid), || {
            Some(format!("stopped listening on {ports} for {}s; restarting", after.as_secs()))
        });
        i.handle.signal_group(stop_signal);
        self.send_later(grace, Event::KillDue { inst: id });
    }
}

/// `:3000` or `:3000, :9229`; `a port` when none is known.
pub(super) fn ports_text(ports: &[u16]) -> String {
    if ports.is_empty() {
        return "a port".into();
    }
    ports.iter().map(|p| format!(":{p}")).collect::<Vec<_>>().join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::ListenWatch;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// An OS made of tables: the listening sockets, and each pid's sockets
    /// (or `None`: unreadable) and namespace; counts the walks.
    #[derive(Default)]
    struct Fake {
        table: Option<Vec<TcpListen>>,
        held: HashMap<u32, Option<HashSet<u64>>>,
        other_ns: HashSet<u32>,
        ports: HashMap<u32, Option<Vec<u16>>>,
        walks: RefCell<u32>,
    }

    impl Look for Fake {
        fn listening(&self) -> Option<Vec<TcpListen>> {
            self.table.clone()
        }
        fn same_namespace(&self, pid: u32) -> bool {
            !self.other_ns.contains(&pid)
        }
        fn tree_sockets(&self, pid: u32) -> Option<HashSet<u64>> {
            *self.walks.borrow_mut() += 1;
            self.held.get(&pid).cloned().flatten()
        }
        fn tree_ports(&self, pid: u32) -> Option<Vec<u16>> {
            self.ports.get(&pid).cloned().flatten()
        }
    }

    fn sock(inode: u64, port: u16) -> TcpListen {
        TcpListen { inode, addr: "0.0.0.0".parse().unwrap(), port, backlog: 0, max_backlog: 511, drops: 0 }
    }

    fn target(inst: u64, pid: u32, known: &[u64]) -> Target {
        Target { inst, pid, known: known.to_vec() }
    }

    #[test]
    fn a_worker_holding_its_socket_is_listening_without_a_walk() {
        let mut os = Fake { table: Some(vec![sock(10, 3000), sock(11, 3000), sock(12, 9229)]), ..Fake::default() };
        os.held.insert(100, Some(HashSet::from([10, 55])));
        os.held.insert(200, Some(HashSet::from([11])));
        // Worker 1 still has socket 10: no walk. Worker 2 had 99 (closed),
        // and now holds 11 on the same port (SO_REUSEPORT: each its own).
        let seen = look(&os, &[target(1, 100, &[10]), target(2, 200, &[99])]);
        assert_eq!(
            seen,
            vec![
                (1, Seen::Listening { inodes: vec![10], ports: vec![3000] }),
                (2, Seen::Listening { inodes: vec![11], ports: vec![3000] })
            ]
        );
        assert_eq!(*os.walks.borrow(), 1, "only the worker whose socket is gone was walked");
        // Gone, nothing new: nothing. Unreadable: no verdict.
        os.held.insert(200, Some(HashSet::from([55])));
        os.held.insert(300, None);
        let seen = look(&os, &[target(2, 200, &[11, 99]), target(3, 300, &[])]);
        assert_eq!(seen, vec![(2, Seen::Listening { inodes: vec![11], ports: vec![3000] }), (3, Seen::Unknown)]);
        os.table = Some(vec![sock(10, 3000)]);
        assert_eq!(look(&os, &[target(2, 200, &[11])]), vec![(2, Seen::Nothing)]);
    }

    #[test]
    fn without_the_kernel_list_or_in_another_namespace_the_tree_is_read() {
        let mut os = Fake::default();
        os.ports.insert(100, Some(vec![3000, 3000, 80]));
        os.ports.insert(200, Some(vec![]));
        let seen = look(&os, &[target(1, 100, &[]), target(2, 200, &[]), target(3, 300, &[])]);
        assert_eq!(
            seen,
            vec![
                (1, Seen::Listening { inodes: vec![], ports: vec![80, 3000] }),
                (2, Seen::Nothing),
                (3, Seen::Unknown)
            ]
        );
        // A list exists, but the worker is in a namespace of its own: its sockets are not in it.
        os.table = Some(vec![]);
        os.other_ns.insert(100);
        assert_eq!(look(&os, &[target(1, 100, &[7])])[0].1, Seen::Listening { inodes: vec![], ports: vec![80, 3000] });
        assert_eq!(*os.walks.borrow(), 0);
    }

    #[test]
    fn a_worker_is_restarted_only_after_listening_then_going_without_for_the_whole_time() {
        let after = Duration::from_secs(10);
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let up = Seen::Listening { inodes: vec![1], ports: vec![3000] };
        let mut w = ListenWatch::default();
        // Never listened: never restarted, however long.
        for s in [0, 20, 40, 100, 1000] {
            assert!(!verdict(&mut w, Seen::Nothing, at(s), t0, after));
        }
        assert_eq!(w.next_walk, Some(at(1000) + WALK_LATER), "an old worker without ports is walked seldom");
        // Listens: watched from now on.
        assert!(!verdict(&mut w, up.clone(), at(1002), t0, after));
        assert_eq!((w.seen_at, w.next_walk, w.ports.clone()), (Some(at(1002)), None, vec![3000]));
        // Gone for less than `after` (a dev server restarting itself), then back.
        assert!(!verdict(&mut w, Seen::Nothing, at(1004), t0, after));
        assert!(!verdict(&mut w, Seen::Nothing, at(1012), t0, after));
        assert!(!verdict(&mut w, up.clone(), at(1014), t0, after));
        assert_eq!(w.lost_since, None);
        // Gone for good: restarted once `after` has passed since it was first seen gone.
        assert!(!verdict(&mut w, Seen::Nothing, at(1016), t0, after));
        assert!(!verdict(&mut w, Seen::Unknown, at(1020), t0, after), "an unreadable look decides nothing");
        assert!(!verdict(&mut w, Seen::Nothing, at(1024), t0, after));
        assert!(verdict(&mut w, Seen::Nothing, at(1026), t0, after));
        // A young worker without ports is walked at every check.
        let mut young = ListenWatch::default();
        assert!(!verdict(&mut young, Seen::Nothing, at(5), t0, after));
        assert_eq!(young.next_walk, None);
    }

    #[test]
    fn ports_are_named_for_the_log() {
        assert_eq!(ports_text(&[3000]), ":3000");
        assert_eq!(ports_text(&[80, 443]), ":80, :443");
        assert_eq!(ports_text(&[]), "a port");
    }

    /// The real OS, on this process: a listener is found by a walk, then by
    /// its socket alone, and is gone once closed.
    #[test]
    fn this_process_is_seen_listening_and_then_not() {
        if !platform::current().capabilities().listening_ports {
            return;
        }
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id();
        let first = look(&Os, &[target(1, me, &[])]);
        let Seen::Listening { inodes, ports } = &first[0].1 else { panic!("{first:?}") };
        assert!(ports.contains(&port), "{first:?}");
        let again = look(&Os, &[target(1, me, inodes)]);
        assert!(matches!(&again[0].1, Seen::Listening { ports, .. } if ports.contains(&port)), "{again:?}");
        drop(l);
        let after = look(&Os, &[target(1, me, inodes)]);
        assert!(!matches!(&after[0].1, Seen::Listening { ports, .. } if ports.contains(&port)), "{after:?}");
    }
}

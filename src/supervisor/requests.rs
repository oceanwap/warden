//! Request health: the responses the workers sent, by status, and the app's
//! ports as the kernel sees them.
//!
//! Responses come with each worker's heartbeat (once a second), as counts
//! since it started: from Warden's static server, and from the shim under
//! Node (`node:diagnostics_channel`, nothing wrapped). The supervisor turns
//! them into a rate, a minute and a total, per worker and for the app.
//!
//! Ports are read only when someone asks for the status (`warden list`, the
//! GUI, a scrape), at most every two seconds and off the event loop, and
//! only from the kernel: one socket-diagnostics request for the listeners
//! (queued and dropped connections) and one per port that counts its
//! established connections (the kernel matches the port).

use super::*;
use crate::control::{PortStats, Responses};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, PoisonError};

/// How long a reading of the ports is kept.
const PORTS_TTL: Duration = Duration::from_secs(2);

impl Supervisor {
    /// A heartbeat's counts (`counts`, since the worker started) from
    /// reporter `worker` of instance `inst_id`: what is new goes into the
    /// worker's window and the app's.
    pub(super) fn count_requests(&mut self, inst_id: u64, worker: usize, counts: Responses) {
        let sec = self.started.elapsed().as_secs();
        let Some(inst) = self.insts.get_mut(&inst_id) else { return };
        let r = inst.requests.get_or_insert_with(Default::default);
        let before = r.last.insert(worker, counts).unwrap_or_default();
        let new = counts.since(&before);
        r.window.add(sec, &new);
        self.requests.get_or_insert_with(Default::default).add(sec, &new);
    }

    /// `Status.requests`.
    pub(super) fn app_requests(&self) -> Option<control::RequestStats> {
        let sec = self.started.elapsed().as_secs();
        self.requests.as_ref().map(|w| w.stats(sec))
    }

    /// `Status.ports`: the TCP ports the workers listen on (`listening`, from
    /// the status being made), with their queues, drops and connections.
    pub(super) fn port_stats(&self, workers: &[WorkerStatus]) -> Vec<PortStats> {
        let mut ports: Vec<u16> = workers
            .iter()
            .flat_map(|w| &w.listening)
            .filter_map(|l| match l {
                control::Listener::Tcp { port, .. } => Some(*port),
                _ => None,
            })
            .collect();
        ports.sort_unstable();
        ports.dedup();
        // The sockets the port watch has seen the workers hold: on a shared
        // port they tell this app's listeners from another's.
        let inodes: HashSet<u64> = self.insts.values().flat_map(|i| i.listen.inodes.iter().copied()).collect();
        let mut found = self.ports.get(ports, inodes);
        if self.cfg.static_files.is_some() {
            // TCP_DEFER_ACCEPT: the kernel counts each connection's wait for
            // its request as a drop on the listener.
            for p in &mut found {
                p.drops = None;
            }
        }
        found
    }
}

/// The last reading of the ports, kept for [`PORTS_TTL`]; a stale one is
/// served while a blocking thread reads the next.
#[derive(Clone, Default)]
pub(super) struct PortCache {
    inner: Arc<Mutex<PortInner>>,
}

#[derive(Default)]
struct PortInner {
    at: Option<Instant>,
    asked: Vec<u16>,
    found: Vec<PortStats>,
    refreshing: bool,
}

impl PortCache {
    fn get(&self, ports: Vec<u16>, inodes: HashSet<u64>) -> Vec<PortStats> {
        if ports.is_empty() {
            return Vec::new();
        }
        let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if g.at.is_none() && !g.refreshing {
            // The first reading on the spot (one request to the kernel, and
            // one per port), so a single `warden list` has the figures.
            drop(g);
            let found = read_ports(&ports, &inodes);
            let mut g = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            *g = PortInner { at: Some(Instant::now()), asked: ports, found: found.clone(), refreshing: false };
            return found;
        }
        let fresh = g.at.is_some_and(|t| t.elapsed() < PORTS_TTL) && g.asked == ports;
        if !fresh && !g.refreshing {
            g.refreshing = true;
            let me = self.clone();
            tokio::task::spawn_blocking(move || {
                let found = std::panic::catch_unwind(|| read_ports(&ports, &inodes)).unwrap_or_default();
                let mut g = me.inner.lock().unwrap_or_else(PoisonError::into_inner);
                *g = PortInner { at: Some(Instant::now()), asked: ports, found, refreshing: false };
            });
        }
        // The ports asked for that the last reading has (one a worker no
        // longer listens on is left out).
        g.found.clone()
    }
}

/// Read the ports from the kernel (Linux); nothing elsewhere.
fn read_ports(ports: &[u16], inodes: &HashSet<u64>) -> Vec<PortStats> {
    let Ok(listeners) = crate::sys::tcp_listeners() else { return Vec::new() };
    ports.iter().filter_map(|p| port_of(*p, &listeners, inodes, crate::sys::tcp_connections(*p).ok())).collect()
}

/// One port's figures: its listening sockets that are the app's (those of
/// `inodes` when it has any of them, else every one on the port).
fn port_of(
    port: u16,
    listeners: &[crate::sys::TcpListen],
    inodes: &HashSet<u64>,
    connections: Option<u32>,
) -> Option<PortStats> {
    let on_port: Vec<&crate::sys::TcpListen> = listeners.iter().filter(|l| l.port == port).collect();
    let mine: Vec<&crate::sys::TcpListen> = on_port.iter().copied().filter(|l| inodes.contains(&l.inode)).collect();
    let socks = if mine.is_empty() { on_port } else { mine };
    if socks.is_empty() {
        return None;
    }
    Some(PortStats {
        port,
        connections,
        backlog: socks.iter().map(|l| l.backlog).sum(),
        max_backlog: socks.iter().map(|l| l.max_backlog).sum(),
        drops: Some(socks.iter().map(|l| u64::from(l.drops)).sum()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::TcpListen;

    fn sock(inode: u64, port: u16, backlog: u32, drops: u32) -> TcpListen {
        TcpListen { inode, addr: "0.0.0.0".parse().unwrap(), port, backlog, max_backlog: 511, drops }
    }

    #[test]
    fn a_port_sums_the_apps_own_sockets_on_it() {
        // Two workers of this app on 3000 (SO_REUSEPORT), another app's socket on it too, and 9000.
        let all = [sock(1, 3000, 2, 5), sock(2, 3000, 1, 0), sock(3, 3000, 50, 9), sock(4, 9000, 0, 0)];
        let mine = HashSet::from([1, 2]);
        let p = port_of(3000, &all, &mine, Some(42)).unwrap();
        assert_eq!(p, PortStats { port: 3000, connections: Some(42), backlog: 3, max_backlog: 1022, drops: Some(5) });
        // Not knowing which are the app's: every socket on the port.
        let p = port_of(3000, &all, &HashSet::new(), None).unwrap();
        assert_eq!((p.backlog, p.drops, p.connections), (53, Some(14), None));
        assert_eq!(port_of(8080, &all, &mine, Some(0)), None, "not listened on: not shown");
    }

    #[test]
    fn this_process_port_is_read_from_the_kernel() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let _c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        let got = read_ports(&[port], &HashSet::new());
        if crate::sys::tcp_listeners().is_err() {
            assert!(got.is_empty());
            return;
        }
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].port, port);
        assert!(got[0].max_backlog > 0);
        assert!(got[0].connections.is_some_and(|c| c <= 1));
    }
}

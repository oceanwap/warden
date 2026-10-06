//! Port handling. Warden never proxies application traffic; workers bind the
//! port themselves with SO_REUSEPORT and the kernel balances connections.
//! Warden only decides which port each worker uses and checks, from outside,
//! whether a worker is listening. (On macOS, whose kernel does not balance,
//! Warden accepts and hands each connection to a worker: `crate::handoff`.)

use crate::config::PortStrategy;

/// The port worker `id` (1-based) should listen on.
pub fn worker_port(base: u16, strategy: PortStrategy, id: usize) -> u16 {
    match strategy {
        PortStrategy::Shared => base,
        PortStrategy::Offset => base + (id as u16 - 1),
    }
}

/// Number of TCP sockets in LISTEN state on `port` owned by `pid`. `None`
/// when the OS adapter cannot say (no `/proc`, no libproc) or the process is
/// gone: callers fall back to [`port_accepts`].
pub fn count_listeners(pid: u32, port: u16) -> Option<usize> {
    crate::platform::listening_ports(pid).map(|ports| ports.iter().filter(|p| **p == port).count())
}

/// [`count_listeners`], and whether the OS answered cheaply (Linux: the
/// kernel's list of listening sockets, not a table with a row per
/// connection), so that it may be asked again soon.
pub fn count_listeners_cheap(pid: u32, port: u16) -> Option<(usize, bool)> {
    crate::platform::listening_ports_cheap(pid)
        .map(|(ports, cheap)| (ports.iter().filter(|p| **p == port).count(), cheap))
}

/// Fallback readiness probe for platforms whose adapter cannot list listeners: can we connect?
/// Imprecise with a shared port (any worker may answer).
pub fn port_accepts(port: u16) -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    std::net::TcpStream::connect_timeout(&addr, std::time::Duration::from_millis(200)).is_ok()
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

    #[test]
    fn counts_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if !crate::platform::current().capabilities().listening_ports {
            assert_eq!(count_listeners(std::process::id(), port), None);
            return;
        }
        assert_eq!(count_listeners(std::process::id(), port), Some(1));
        drop(l);
        assert_eq!(count_listeners(std::process::id(), port), Some(0));
    }
}

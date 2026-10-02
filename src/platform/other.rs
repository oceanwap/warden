//! The adapter for any other Unix (the BSDs, illumos, ...): it knows
//! nothing, and every question answers `None`. Warden builds and runs there
//! with empty CPU and RSS columns, the connect-probe readiness check and no
//! host events; `warden doctor` says so. A real adapter for a new OS is a
//! new file next to this one and one arm in `platform::current`.

use super::{Capabilities, Environ, HostSnapshot, Listener, Platform, ProcStats};
use std::path::PathBuf;

pub(crate) struct Other;

/// Every capability off.
pub(crate) fn none() -> Capabilities {
    Capabilities {
        proc_stats: false,
        proc_owner: false,
        listening_ports: false,
        proc_environ: false,
        host_stats: false,
        reuseport_balances: false,
        parent_death_signal: false,
        oom_attribution: false,
    }
}

impl Platform for Other {
    fn name(&self) -> &'static str {
        "other"
    }

    fn capabilities(&self) -> Capabilities {
        none()
    }

    fn proc_stats(&self, _pid: u32) -> Option<ProcStats> {
        None
    }

    fn proc_owner(&self, _pid: u32) -> Option<u32> {
        None
    }

    fn proc_environ(&self, _pid: u32) -> Option<Environ> {
        None
    }

    fn proc_cwd(&self, _pid: u32) -> Option<PathBuf> {
        None
    }

    fn proc_name(&self, _pid: u32) -> Option<String> {
        None
    }

    fn listening_ports(&self, _pid: u32) -> Option<Vec<u16>> {
        None
    }

    fn listeners(&self, _pid: u32) -> Option<Vec<Listener>> {
        None
    }

    fn children(&self, _pid: u32) -> Option<Vec<u32>> {
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unknown_os_answers_nothing_and_claims_nothing() {
        let p = Other;
        assert_eq!(p.name(), "other");
        assert_eq!(p.capabilities(), none());
        assert!(p.proc_stats(1).is_none() && p.proc_owner(1).is_none() && p.proc_environ(1).is_none());
        assert!(p.proc_cwd(1).is_none() && p.proc_name(1).is_none() && p.listening_ports(1).is_none());
        assert!(p.listeners(1).is_none() && p.children(1).is_none());
        assert!(p.command_lines().is_empty() && p.host_snapshot().is_none() && p.boot_id().is_none());
    }
}

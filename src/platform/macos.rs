//! The macOS adapter: libproc, `sysctl` and Mach calls, through the safe
//! functions in `sys::darwin`. There is no `/proc` here.
//!
//! What macOS cannot do is `false` in the capabilities: `SO_REUSEPORT` lets
//! processes share a port but gives every connection to one of them, a child
//! does not hear when its parent dies, and the kernel does not say which
//! process it killed for memory.

use super::procargs::parse_procargs;
use super::{Capabilities, CpuTimes, Environ, HostSnapshot, Platform, ProcStats};
use crate::sys::darwin;
use std::path::PathBuf;

pub(crate) struct Macos;

impl Platform for Macos {
    fn name(&self) -> &'static str {
        "macos"
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            proc_stats: true,
            proc_owner: true,
            listening_ports: true,
            proc_environ: true,
            host_stats: true,
            reuseport_balances: false,
            parent_death_signal: false,
            oom_attribution: false,
        }
    }

    fn proc_stats(&self, pid: u32) -> Option<ProcStats> {
        let t = darwin::task_info(pid).ok()?;
        Some(ProcStats { rss_bytes: t.resident_bytes, cpu_seconds: t.cpu_ns as f64 / 1e9 })
    }

    fn proc_owner(&self, pid: u32) -> Option<u32> {
        darwin::owner(pid).ok()
    }

    fn proc_environ(&self, pid: u32) -> Option<Environ> {
        parse_procargs(&darwin::procargs(pid).ok()?).map(|p| p.env)
    }

    fn proc_cwd(&self, pid: u32) -> Option<PathBuf> {
        darwin::cwd(pid).ok()
    }

    fn proc_name(&self, pid: u32) -> Option<String> {
        darwin::name(pid).ok().filter(|n| !n.is_empty())
    }

    fn listening_ports(&self, pid: u32) -> Option<Vec<u16>> {
        darwin::listening_tcp_ports(pid).ok()
    }

    fn command_lines(&self) -> Vec<(u32, String)> {
        let Ok(pids) = darwin::all_pids() else { return Vec::new() };
        pids.into_iter()
            .filter_map(|pid| {
                // Another user's process answers EPERM: it is skipped, as `ps -E` does.
                let p = parse_procargs(&darwin::procargs(pid).ok()?)?;
                let line: Vec<String> = p.args.iter().map(|a| a.to_string_lossy().into_owned()).collect();
                (!line.is_empty()).then(|| (pid, line.join(" ")))
            })
            .collect()
    }

    fn host_snapshot(&self) -> Option<HostSnapshot> {
        let (busy, total) = darwin::host_cpu_ticks().ok()?;
        Some(HostSnapshot {
            cpu: CpuTimes { busy, total },
            mem_used_bytes: darwin::memory_used_bytes().ok()?,
            mem_total_bytes: darwin::memory_total_bytes().ok()?,
            load: crate::sys::loadavg()?,
        })
    }

    fn boot_id(&self) -> Option<String> {
        // The boot time identifies the boot; the microseconds keep two boots
        // in one second apart.
        let (sec, usec) = darwin::boot_time().ok()?;
        Some(format!("{sec}.{usec:06}"))
    }
}

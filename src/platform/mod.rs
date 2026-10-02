//! Platform adapters: what Warden asks the operating system, behind one
//! trait, with one implementation per OS.
//!
//! The rest of Warden never reads `/proc` or calls libproc. It asks the
//! current adapter ([`current`]) for a process's memory and CPU time, its
//! owner, its listening ports, its environment, or the host's load, and the
//! adapter answers from what its OS has:
//!
//! | adapter | where | how |
//! |---|---|---|
//! | `linux::Linux` | Linux | `/proc` |
//! | `macos::Macos` | macOS | libproc, `sysctl` and Mach calls (`sys::darwin`) |
//! | `other::Other` | any other Unix | nothing: every question answers `None` |
//!
//! The adapter is chosen when Warden is built (`cfg(target_os)`), not when it
//! runs: a binary is built for one OS, the calls of the others do not exist
//! to link against, and there is nothing to detect at run time. What an
//! adapter cannot do is `None` (or `false` in [`Capabilities`]): callers
//! degrade (an empty column, the connect-probe readiness check) and
//! `warden doctor` says what is missing.
//!
//! The adapters share one contract, tested by [`contract_tests`] against the
//! real OS the tests run on, so Linux and macOS cannot drift apart: a new
//! question is added to the trait, both adapters answer it, and the same test
//! checks both.

use std::ffi::OsString;
use std::path::PathBuf;

mod counter;
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) mod linux;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
#[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
pub(crate) mod other;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod procargs;

/// A process's resident memory and CPU time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProcStats {
    pub rss_bytes: u64,
    /// User plus system CPU time, in seconds.
    pub cpu_seconds: f64,
}

/// Cumulative CPU time of the whole host, in the OS's own ticks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuTimes {
    pub busy: u64,
    pub total: u64,
}

/// One reading of the host: CPU ticks, memory in use and the load averages.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostSnapshot {
    pub cpu: CpuTimes,
    pub mem_used_bytes: u64,
    pub mem_total_bytes: u64,
    pub load: [f64; 3],
}

/// A process's environment, in the order the kernel kept it.
pub type Environ = Vec<(OsString, OsString)>;

/// What an adapter can do on its OS: for `warden doctor` and the docs, and
/// for code that must know *why* a question answers `None`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    pub proc_stats: bool,
    pub proc_owner: bool,
    pub listening_ports: bool,
    pub proc_environ: bool,
    pub host_stats: bool,
    /// The kernel spreads connections across processes listening on one
    /// port with `SO_REUSEPORT` (Linux does; macOS lets them share the port
    /// but hands every connection to one).
    pub reuseport_balances: bool,
    /// A child gets a signal when its parent dies (`PR_SET_PDEATHSIG`).
    pub parent_death_signal: bool,
    /// A process killed by the kernel for memory is recognised as such
    /// (cgroup memory events).
    pub oom_attribution: bool,
}

/// What Warden asks the OS about processes and the host. `pid`s that do
/// not exist, or belong to another user where the OS refuses, answer `None`.
pub trait Platform: Sync {
    /// `linux`, `macos`, or `other`.
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// Resident memory and CPU time.
    fn proc_stats(&self, pid: u32) -> Option<ProcStats>;

    /// The user id the process runs as.
    fn proc_owner(&self, pid: u32) -> Option<u32>;

    /// The environment the process was started with.
    fn proc_environ(&self, pid: u32) -> Option<Environ>;

    /// The process's working directory.
    fn proc_cwd(&self, pid: u32) -> Option<PathBuf>;

    /// The process's command name (`bun`, `warden`).
    fn proc_name(&self, pid: u32) -> Option<String>;

    /// The TCP ports the process listens on, one entry per listening
    /// socket (a socket on 0.0.0.0 and one on :: are two).
    fn listening_ports(&self, pid: u32) -> Option<Vec<u16>>;

    /// Every process the caller can read: its pid and its command line, the
    /// arguments joined by spaces.
    fn command_lines(&self) -> Vec<(u32, String)>;

    /// The host's CPU ticks, memory and load now.
    fn host_snapshot(&self) -> Option<HostSnapshot>;

    /// What identifies this boot of the machine (a saved app is resurrected
    /// once per boot).
    fn boot_id(&self) -> Option<String>;
}

/// The adapter for the OS Warden was built for.
pub fn current() -> &'static dyn Platform {
    #[cfg(target_os = "linux")]
    {
        &linux::Linux
    }
    #[cfg(target_os = "macos")]
    {
        &macos::Macos
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        &other::Other
    }
}

pub fn proc_stats(pid: u32) -> Option<ProcStats> {
    current().proc_stats(pid)
}

pub fn proc_owner(pid: u32) -> Option<u32> {
    current().proc_owner(pid)
}

pub fn proc_environ(pid: u32) -> Option<Environ> {
    current().proc_environ(pid)
}

pub fn proc_cwd(pid: u32) -> Option<PathBuf> {
    current().proc_cwd(pid)
}

pub fn proc_name(pid: u32) -> Option<String> {
    current().proc_name(pid)
}

pub fn listening_ports(pid: u32) -> Option<Vec<u16>> {
    current().listening_ports(pid)
}

pub fn command_lines() -> Vec<(u32, String)> {
    current().command_lines()
}

pub fn host_snapshot() -> Option<HostSnapshot> {
    current().host_snapshot()
}

pub fn boot_id() -> Option<String> {
    current().boot_id()
}

/// The name of the user with this id (`getpwuid_r`), or the number when
/// there is no such user.
pub fn user_name(uid: u32) -> String {
    crate::sys::user_name(uid).unwrap_or_else(|| uid.to_string())
}

#[cfg(test)]
pub(crate) mod contract_tests {
    //! The same questions asked of whatever adapter this OS has. They run on
    //! Linux in CI and on a Mac with `cargo test`: both must pass them.

    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// `sh -c 'exec sleep 60'` with a marker in its environment, in `dir`.
    struct Sleeper(Child);

    impl Sleeper {
        fn start(dir: &std::path::Path) -> Sleeper {
            let child = Command::new("sh")
                .args(["-c", "exec sleep 60"])
                .env("WARDEN_PLATFORM_TEST", "two words")
                .current_dir(dir)
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let s = Sleeper(child);
            // Wait for the exec: the command name is `sleep`.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) && proc_name(s.0.id()).as_deref() != Some("sleep") {
                std::thread::sleep(Duration::from_millis(10));
            }
            s
        }

        fn pid(&self) -> u32 {
            self.0.id()
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn me() -> u32 {
        std::process::id()
    }

    #[test]
    fn the_adapter_for_this_os_is_the_one_built() {
        let want = if cfg!(target_os = "linux") {
            "linux"
        } else if cfg!(target_os = "macos") {
            "macos"
        } else {
            "other"
        };
        assert_eq!(current().name(), want);
        let c = current().capabilities();
        if want != "other" {
            assert!(c.proc_stats && c.proc_owner && c.listening_ports && c.proc_environ && c.host_stats, "{c:?}");
        }
        assert_eq!(c.reuseport_balances, want == "linux");
        assert_eq!(c.oom_attribution, want == "linux");
    }

    #[test]
    fn memory_and_cpu_time_of_this_process_and_a_busy_child() {
        if !current().capabilities().proc_stats {
            return;
        }
        let me = proc_stats(me()).expect("proc_stats of this process");
        assert!(me.rss_bytes > 1 << 20, "{me:?}");
        // A child that spins for a second has used about a second of CPU.
        let mut spin = Command::new("sh").args(["-c", "while :; do :; done"]).stdin(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        let c = proc_stats(spin.id()).expect("proc_stats of the child");
        let _ = spin.kill();
        let _ = spin.wait();
        assert!((0.3..2.0).contains(&c.cpu_seconds), "{} s of CPU after 1 s of spinning", c.cpu_seconds);
        assert_eq!(proc_stats(0x7fff_fff0), None, "no such process");
    }

    #[test]
    fn owner_name_environment_and_working_directory_of_a_child() {
        let caps = current().capabilities();
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let s = Sleeper::start(&dir);
        if caps.proc_owner {
            assert_eq!(proc_owner(s.pid()), Some(crate::sys::euid()));
        }
        assert_eq!(proc_name(s.pid()).as_deref(), Some("sleep"));
        if caps.proc_environ {
            let env = proc_environ(s.pid()).expect("environment of the child");
            let found = env.iter().find(|(k, _)| k == "WARDEN_PLATFORM_TEST").map(|(_, v)| v.clone());
            assert_eq!(found, Some(OsString::from("two words")), "{} variables", env.len());
            assert_eq!(proc_cwd(s.pid()), Some(dir));
        }
    }

    #[test]
    fn listening_ports_of_this_process_follow_its_sockets() {
        if !current().capabilities().listening_ports {
            return;
        }
        let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = a.local_addr().unwrap().port();
        let ports = listening_ports(me()).expect("listening_ports");
        assert_eq!(ports.iter().filter(|p| **p == port).count(), 1, "{ports:?}");
        // A connection to it is not another listener.
        let c = std::net::TcpStream::connect(a.local_addr().unwrap()).unwrap();
        let (_peer, _) = a.accept().unwrap();
        assert_eq!(listening_ports(me()).unwrap().iter().filter(|p| **p == port).count(), 1);
        drop(c);
        drop(a);
        assert!(!listening_ports(me()).unwrap().contains(&port), "closed listener");
        assert_eq!(listening_ports(0x7fff_fff0), None);
    }

    #[test]
    fn the_command_lines_include_this_test_binary_and_a_child() {
        if !current().capabilities().proc_environ {
            return;
        }
        let s = Sleeper::start(&std::env::temp_dir());
        let all = command_lines();
        assert!(all.iter().any(|(pid, line)| *pid == me() && !line.is_empty()), "{} processes", all.len());
        let child = all.iter().find(|(pid, _)| *pid == s.pid()).map(|(_, l)| l.clone());
        assert!(child.is_some_and(|l| l.contains("sleep") && l.contains("60")), "the child's command line");
    }

    #[test]
    fn host_numbers_are_plausible_and_the_boot_id_is_stable() {
        if current().capabilities().host_stats {
            let h = host_snapshot().expect("host_snapshot");
            assert!(h.cpu.total > 0 && h.cpu.busy <= h.cpu.total, "{h:?}");
            assert!(
                h.mem_total_bytes >= 1 << 28 && h.mem_used_bytes > 0 && h.mem_used_bytes <= h.mem_total_bytes,
                "{h:?}"
            );
            assert!(h.load.iter().all(|l| l.is_finite() && *l >= 0.0), "{h:?}");
        }
        if current().name() != "other" {
            let id = boot_id().expect("boot_id");
            assert!(!id.is_empty());
            assert_eq!(boot_id(), Some(id), "the same all boot");
        }
    }

    #[test]
    fn user_names_fall_back_to_the_number() {
        assert_eq!(user_name(0), "root");
        assert_eq!(user_name(0x7fff_fff0), (0x7fff_fff0u32).to_string());
        assert!(!user_name(crate::sys::euid()).is_empty());
    }
}

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
//! [`orphans`] is the one part that is not an adapter: what a supervisor does
//! on an OS without a parent-death signal, written once against the
//! adapter's [`Platform::proc_identity`] and run on every OS in its tests.
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
mod listeners;
#[cfg(target_os = "macos")]
pub(crate) mod macos;
pub(crate) mod orphans;
#[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
pub(crate) mod other;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod procargs;

pub use listeners::Listener;

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

/// Who a process is: its parent, its process group and when it started. The
/// start time is what tells a process from another one that was given its pid
/// later, so a pid is only ever trusted together with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcIdentity {
    /// When it started, in the OS's own unit (microseconds since the epoch on
    /// macOS, clock ticks since boot on Linux): equal for the same process at
    /// any time, different for any later process with the same pid. Compared,
    /// never interpreted.
    pub start: u64,
    pub ppid: u32,
    pub pgid: u32,
}

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
    /// Workers a killed supervisor left behind are found and stopped when
    /// the app starts again (`orphans`), which is what an OS without a
    /// parent-death signal has instead. Needs `proc_identity`.
    pub orphan_sweep: bool,
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

    /// Who the process is (see [`ProcIdentity`]). `None` when there is no
    /// such process, when it belongs to another user, and for one that has
    /// exited and is waiting to be collected: nothing runs there any more, so
    /// to a caller it is gone.
    fn proc_identity(&self, pid: u32) -> Option<ProcIdentity>;

    /// The process's working directory.
    fn proc_cwd(&self, pid: u32) -> Option<PathBuf>;

    /// The process's command name (`bun`, `warden`).
    fn proc_name(&self, pid: u32) -> Option<String>;

    /// The TCP ports the process listens on, one entry per listening
    /// socket (a socket on 0.0.0.0 and one on :: are two).
    fn listening_ports(&self, pid: u32) -> Option<Vec<u16>>;

    /// [`Platform::listening_ports`], and whether the answer was cheap: the
    /// kernel answered with the listening sockets only, not a table with a
    /// row per connection, so a caller may ask often.
    fn listening_ports_cheap(&self, pid: u32) -> Option<(Vec<u16>, bool)> {
        self.listening_ports(pid).map(|ports| (ports, false))
    }

    /// The sockets this one process accepts connections on: TCP in LISTEN
    /// with the address, and Unix sockets (not UDP, not connected ones).
    fn listeners(&self, pid: u32) -> Option<Vec<Listener>>;

    /// The sockets of several processes at once, as one list. The first is
    /// the one that matters: `None` when it cannot be read; one of the others
    /// that cannot is skipped. An adapter whose OS keeps its socket tables
    /// per host reads them once for all of them.
    fn listeners_of(&self, pids: &[u32]) -> Option<Vec<Listener>> {
        let (first, rest) = pids.split_first()?;
        let mut found = self.listeners(*first)?;
        for pid in rest {
            found.extend(self.listeners(*pid).unwrap_or_default());
        }
        Some(found)
    }

    /// [`Platform::listeners_of`] for several groups of processes, an answer
    /// for each. An adapter whose OS answers for the whole host asks once
    /// for all of them.
    fn listeners_of_each(&self, groups: &[Vec<u32>]) -> Vec<Option<Vec<Listener>>> {
        groups.iter().map(|pids| self.listeners_of(pids)).collect()
    }

    /// The processes this one started (its children, not their children).
    fn children(&self, pid: u32) -> Option<Vec<u32>>;

    /// The inodes of the sockets the process holds open (`socket:[N]`), for
    /// matching them with the kernel's socket tables. `None`: unreadable, or
    /// no way to know on this OS.
    fn socket_inodes(&self, _pid: u32) -> Option<std::collections::HashSet<u64>> {
        None
    }

    /// The network namespace the process is in (sockets are per namespace);
    /// `None` where there are none, or it cannot be read.
    fn net_namespace(&self, _pid: u32) -> Option<PathBuf> {
        None
    }

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

pub fn proc_identity(pid: u32) -> Option<ProcIdentity> {
    current().proc_identity(pid)
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

/// [`listening_ports`], and whether the answer was cheap enough to ask for
/// often (see [`Platform::listening_ports_cheap`]).
pub fn listening_ports_cheap(pid: u32) -> Option<(Vec<u16>, bool)> {
    current().listening_ports_cheap(pid)
}

/// What each process and the processes below it listen on, sorted: the
/// server is often not the process Warden started (`npm run start`, `turbo`,
/// a shell script). At most 64 processes, 4 levels deep. `None` for a
/// process that itself cannot be read. The OS's socket tables are read once
/// for all of them where it has them per host.
pub fn listeners_each(pids: &[u32]) -> Vec<Option<Vec<Listener>>> {
    listeners::of_trees(current(), pids)
}

/// The sockets (inodes) the process and the processes below it hold open.
/// `None` unless every one of them was read: the tree was bigger than a walk
/// looks at (as [`listeners`] limits it), a process in it could not be read,
/// or the OS has no way to say. So an answer without a socket means none.
pub fn socket_inodes_of_tree(pid: u32) -> Option<std::collections::HashSet<u64>> {
    let p = current();
    let mut all = std::collections::HashSet::new();
    for pid in listeners::tree_whole(p, pid)? {
        all.extend(p.socket_inodes(pid)?);
    }
    Some(all)
}

/// What the process and the processes below it listen on, the whole tree or
/// `None` (the same conditions as [`socket_inodes_of_tree`]): an empty
/// answer means nothing listens.
pub fn listeners_whole(pid: u32) -> Option<Vec<Listener>> {
    let p = current();
    let mut found = Vec::new();
    for pid in listeners::tree_whole(p, pid)? {
        found.extend(p.listeners(pid)?);
    }
    found.sort();
    found.dedup();
    Some(found)
}

/// The network namespace of the process, `None` where there are none.
pub fn net_namespace(pid: u32) -> Option<PathBuf> {
    current().net_namespace(pid)
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
pub(crate) mod test_sleep {
    //! A `sleep 60` for tests that read a child's environment. Not the
    //! system's: macOS 27 no longer shows the environment of Apple's own
    //! binaries (`/bin/sleep`; `ps eww` shows none either), only of everyone
    //! else's, which is what Warden supervises. So the sleeper is this test
    //! binary under the name `sleep`: a hard link beside it.

    use std::process::{Command, Stdio};

    const MARKER: &str = "WARDEN_TEST_SLEEP";

    /// `sleep 60`: the command name is `sleep`, the command line has `60`.
    pub(crate) fn command() -> Command {
        let exe = std::env::current_exe().unwrap();
        // Beside the test binary (the same filesystem, and `cargo clean` removes it).
        let dir = exe.with_extension("sleep");
        let link = dir.join("sleep");
        // Once: tests run in parallel, and one must not remove the link another is starting.
        static LINK: std::sync::Once = std::sync::Once::new();
        LINK.call_once(|| {
            std::fs::create_dir_all(&dir).unwrap();
            let _ = std::fs::remove_file(&link);
            std::fs::hard_link(&exe, &link).unwrap();
        });
        // libtest's name for `asleep` below: the module path without the crate.
        let test = format!("{}::asleep", module_path!().split_once("::").unwrap().1);
        let mut c = Command::new(link);
        // `60` is a second filter that matches no test: there for the command line.
        c.args(["--ignored", "--exact", &test, "60"]).env(MARKER, "1").stdout(Stdio::null()).stderr(Stdio::null());
        c
    }

    /// What `command()` runs. Does nothing in a test run that includes ignored tests.
    #[test]
    #[ignore = "the child process of command(), not a test"]
    fn asleep() {
        if std::env::var_os(MARKER).is_some() {
            std::thread::sleep(std::time::Duration::from_secs(60));
        }
    }
}

#[cfg(test)]
pub(crate) mod contract_tests {
    //! The same questions asked of whatever adapter this OS has. They run on
    //! Linux in CI and on a Mac with `cargo test`: both must pass them.

    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    /// `sleep 60` (`test_sleep`) with a marker in its environment, in `dir`.
    struct Sleeper(Child);

    impl Sleeper {
        fn start(dir: &std::path::Path) -> Sleeper {
            let child = test_sleep::command()
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
        // An OS has a parent-death signal or the sweep for the lack of it, not both; `other` has neither.
        assert_eq!(c.parent_death_signal, want == "linux");
        assert_eq!(c.orphan_sweep, want == "macos");
    }

    #[test]
    fn identity_tells_a_process_from_another_with_its_pid() {
        use std::os::unix::process::CommandExt;
        if current().name() == "other" {
            assert_eq!(proc_identity(me()), None);
            return;
        }
        let mine = proc_identity(me()).expect("identity of this process");
        assert_eq!(proc_identity(me()), Some(mine), "the same every time");
        assert_eq!(mine.ppid, std::os::unix::process::parent_id());
        assert_eq!(proc_identity(0x7fff_fff0), None, "no such process");

        // A child in a group of its own: its parent is this process, its group its pid.
        let mut child = Command::new("sleep").arg("60").process_group(0).stdin(Stdio::null()).spawn().unwrap();
        let id = proc_identity(child.id()).expect("identity of the child");
        assert_eq!((id.ppid, id.pgid), (me(), child.id()), "{id:?}");
        assert_ne!(id.start, mine.start, "another process, another start");
        assert_eq!(proc_identity(child.id()), Some(id), "the same every time");

        // Gone once it has exited, whether or not it was collected.
        child.kill().unwrap();
        let t0 = Instant::now();
        while proc_identity(child.id()).is_some() && t0.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(proc_identity(child.id()), None, "exited");
        child.wait().unwrap();
        assert_eq!(proc_identity(child.id()), None, "collected");
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
    fn listeners_are_tcp_with_the_address_and_unix_sockets_that_accept() {
        if !current().capabilities().listening_ports {
            return;
        }
        let tcp = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = tcp.local_addr().unwrap().port();
        let loopback = Listener::Tcp { port, addr: "127.0.0.1".parse().unwrap() };
        // A short path: a Unix socket's path is at most 104 bytes on macOS.
        let path = std::env::temp_dir().join(format!("wl-{}.sock", me()));
        let _ = std::fs::remove_file(&path);
        let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let ends_with_name =
            |l: &Listener| matches!(l, Listener::Unix { path: p } if p.ends_with(&format!("wl-{}.sock", me())));

        let all = current().listeners(me()).expect("listeners");
        assert_eq!(all.iter().filter(|l| **l == loopback).count(), 1, "{all:?}");
        assert_eq!(all.iter().filter(|l| ends_with_name(l)).count(), 1, "{all:?}");

        // A connection to either is not another listener.
        let c = std::net::TcpStream::connect(tcp.local_addr().unwrap()).unwrap();
        let (_peer, _) = tcp.accept().unwrap();
        let u = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let (_upeer, _) = unix.accept().unwrap();
        let all = current().listeners(me()).unwrap();
        assert_eq!(all.iter().filter(|l| **l == loopback).count(), 1, "{all:?}");
        assert_eq!(all.iter().filter(|l| ends_with_name(l)).count(), 1, "{all:?}");
        drop((c, u));

        // IPv6, where the host has it.
        if let Ok(v6) = std::net::TcpListener::bind("[::1]:0") {
            let want = Listener::Tcp { port: v6.local_addr().unwrap().port(), addr: "::1".parse().unwrap() };
            assert!(current().listeners(me()).unwrap().contains(&want));
        }

        drop((tcp, unix));
        let _ = std::fs::remove_file(&path);
        let all = current().listeners(me()).unwrap();
        assert!(!all.contains(&loopback) && !all.iter().any(ends_with_name), "closed: {all:?}");
        assert_eq!(current().listeners(0x7fff_fff0), None);
    }

    #[test]
    fn children_are_the_processes_a_process_started() {
        if !current().capabilities().listening_ports {
            return;
        }
        let s = Sleeper::start(&std::env::temp_dir());
        let kids = current().children(me()).expect("children of this process");
        assert!(kids.contains(&s.pid()), "{kids:?}");
        assert_eq!(current().children(s.pid()), Some(Vec::new()), "a sleeper has none");
        assert!(
            current().children(0x7fff_fff0).unwrap_or_default().is_empty(),
            "no such process: none, or no children"
        );
    }

    #[test]
    fn a_wrapper_shows_what_its_child_listens_on() {
        use std::io::{BufRead, BufReader};
        if !current().capabilities().listening_ports {
            return;
        }
        // sh stays the parent (a command after the python one: no exec); python listens
        // and waits for its stdin to close.
        let py = "import socket,sys; s=socket.socket(); s.bind(('127.0.0.1',0)); s.listen(); print(s.getsockname()[1], flush=True); sys.stdin.read()";
        let Ok(mut sh) = Command::new("sh")
            .arg("-c")
            .arg(format!("python3 -c \"{py}\"; :"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
        else {
            return;
        };
        let mut line = String::new();
        let got = BufReader::new(sh.stdout.take().unwrap()).read_line(&mut line).unwrap_or(0);
        // No python3 here: nothing to look at. Once it printed its port, the tree must be readable.
        let port: Option<u16> = if got == 0 { None } else { line.trim().parse().ok() };
        let tree = port.map(|_| listeners_each(&[sh.id()]).remove(0));
        // python reads its stdin to the end: closing it lets it go.
        drop(sh.stdin.take());
        let _ = sh.kill();
        let _ = sh.wait();
        if let (Some(port), Some(tree)) = (port, tree) {
            let tree = tree.expect("the wrapper's process tree is readable");
            assert!(tree.iter().any(|l| matches!(l, Listener::Tcp { port: p, .. } if *p == port)), "{tree:?}");
        }
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

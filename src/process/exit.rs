//! Why a worker died, for `last_exit` and the log line of its death.
//!
//! The wait status alone says "signal 9". With what Warden knows, the reason
//! says who: the kernel's OOM killer (the cgroup's `oom_kill` counter went
//! up), Warden itself (its waiter delivered that signal), another process,
//! or the program crashing (SIGSEGV, SIGABRT…).

use std::path::{Path, PathBuf};

/// Signals Warden delivered to a process (or its group) while it was alive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Sent(u64);

impl Sent {
    pub fn add(&mut self, sig: i32) {
        if (1..=64).contains(&sig) {
            self.0 |= 1 << (sig - 1);
        }
    }

    pub fn has(self, sig: i32) -> bool {
        (1..=64).contains(&sig) && self.0 & (1 << (sig - 1)) != 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Exited by itself with this code.
    Code(i32),
    /// Exited after Warden's stop signal (`stop`): with a code, or by it.
    AfterStop {
        stop: i32,
        code: Option<i32>,
    },
    /// SIGKILLed by the kernel's OOM killer.
    Oom,
    /// SIGKILLed by Warden (grace period over, hung, not ready in time…).
    KilledByWarden,
    /// Ended by another signal Warden delivered (`warden signal`, a forwarded SIGUSR2).
    SignalFromWarden(i32),
    /// Ended by a signal Warden did not send.
    Killed(i32),
    /// The program crashed (SIGSEGV, SIGABRT…).
    Crashed(i32),
    Unknown,
}

/// What the wait status and Warden's own records say about one death.
/// `oom`: the OOM kill counter rose by one not yet accounted for (only
/// asked for SIGKILL deaths, see `OomCounter::took_one`).
pub fn classify(code: Option<i32>, signal: Option<i32>, sent: Sent, stop: i32, oom: bool) -> Reason {
    match (code, signal) {
        (_, Some(libc::SIGKILL)) if oom => Reason::Oom,
        (_, Some(libc::SIGKILL)) if sent.has(libc::SIGKILL) => Reason::KilledByWarden,
        (_, Some(s)) if s == stop && sent.has(stop) => Reason::AfterStop { stop, code: None },
        (_, Some(s)) if sent.has(s) => Reason::SignalFromWarden(s),
        (_, Some(s)) if crash_signal(s).is_some() => Reason::Crashed(s),
        (_, Some(s)) => Reason::Killed(s),
        (Some(c), None) if sent.has(stop) => Reason::AfterStop { stop, code: Some(c) },
        (Some(c), None) => Reason::Code(c),
        (None, None) => Reason::Unknown,
    }
}

impl Reason {
    /// The short form: `last_exit`, event details, `reason=` in the log.
    pub fn short(self) -> String {
        let name = crate::signals::name;
        match self {
            Reason::Code(c) => format!("exit code {c}"),
            Reason::AfterStop { stop, code: Some(c) } => format!("exit code {c} after Warden's {}", name(stop)),
            Reason::AfterStop { stop, code: None } => format!("stopped by Warden's {}", name(stop)),
            Reason::Oom => "killed by the kernel OOM killer (out of memory)".into(),
            Reason::KilledByWarden => "killed by Warden (SIGKILL)".into(),
            Reason::SignalFromWarden(s) => format!("ended by {} from Warden", name(s)),
            Reason::Killed(s) => format!("killed by another process ({})", name(s)),
            Reason::Crashed(s) => match crash_signal(s) {
                Some((n, what)) => format!("crashed: {n} ({what})"),
                None => format!("crashed: {}", name(s)),
            },
            Reason::Unknown => "unknown exit".into(),
        }
    }

    /// How to fix or investigate it, for the log line of the death.
    /// `oom_detection`: Warden can read the cgroup's OOM kill counter, so a
    /// SIGKILL that did not raise it was not the OOM killer.
    pub fn hint(self, oom_detection: bool) -> Option<&'static str> {
        Some(match self {
            Reason::Oom => OOM_HINT,
            Reason::Killed(libc::SIGKILL) if oom_detection => {
                "something outside Warden sent SIGKILL (kill -9 by a person or script, a container runtime); \
                 Warden restarts it. The kernel's OOM killer was ruled out (the cgroup's oom_kill count did not change)"
            }
            Reason::Killed(libc::SIGKILL) => {
                "something outside Warden sent SIGKILL (kill -9, a script, a container runtime), or the kernel's \
                 OOM killer, which Warden can't tell here (no readable cgroup OOM counter): `journalctl -k | grep -i oom`"
            }
            Reason::Killed(_) => {
                "something outside Warden signalled the worker; under systemd use KillMode=mixed, so only Warden \
                 gets the stop signal and drains its workers"
            }
            Reason::Crashed(libc::SIGABRT) => {
                "the program aborted itself (process.abort(), a failed native assertion, a fatal runtime error): \
                 its last output lines say why (`warden logs <app>`)"
            }
            Reason::Crashed(_) => {
                "a native crash in the runtime or a native module: its last output lines (`warden logs <app>`) and \
                 core dumps (`coredumpctl list`) say where"
            }
            _ => return None,
        })
    }
}

impl Reason {
    /// What to do about a death [`Reason::hint`] has nothing to add to: the
    /// WARN/ERROR line of a crash always says it.
    pub fn plain_hint(self) -> &'static str {
        match self {
            Reason::Code(_) => {
                "the app exited by itself: its last output lines say why (`warden logs <app> --worker N`). Warden \
                 restarts it with backoff; after restart.max_restarts within restart.restart_window it is FAILED"
            }
            Reason::KilledByWarden => {
                "Warden killed it; the line before says why (not ready in time, hung, or still running after \
                 shutdown.grace_period). It starts a new one"
            }
            Reason::AfterStop { .. } | Reason::SignalFromWarden(_) => {
                "it ended on a signal Warden sent (a stop, `warden signal`); Warden starts it again"
            }
            _ => "its last output lines may say why (`warden logs <app> --worker N`); Warden restarts it with backoff",
        }
    }
}

pub const OOM_HINT: &str = "raise memory.max / MemoryMax= or lower `[limits] max_memory` so Warden recycles it \
                            before the kernel kills it";

/// Signals a program gets from its own faults, with what they mean.
fn crash_signal(s: i32) -> Option<(&'static str, &'static str)> {
    Some(match s {
        libc::SIGSEGV => ("SIGSEGV", "segmentation fault"),
        libc::SIGBUS => ("SIGBUS", "bus error"),
        libc::SIGILL => ("SIGILL", "illegal instruction"),
        libc::SIGFPE => ("SIGFPE", "arithmetic error"),
        libc::SIGABRT => ("SIGABRT", "aborted"),
        libc::SIGTRAP => ("SIGTRAP", "trap"),
        libc::SIGSYS => ("SIGSYS", "bad system call"),
        _ => return None,
    })
}

/// The kernel's count of OOM kills in the cgroup Warden and its workers run
/// in (systemd gives each unit one): cgroup v2 `memory.events`, or v1
/// `memory.oom_control`. Workers share it, so each SIGKILL death takes at
/// most one kill the counter shows beyond those already accounted for.
pub struct OomCounter {
    path: Option<PathBuf>,
    seen: u64,
}

impl OomCounter {
    pub fn new() -> OomCounter {
        let path = counter_path();
        let seen = path.as_deref().and_then(read_counter).unwrap_or(0);
        OomCounter { path, seen }
    }

    /// Whether Warden can tell OOM kills apart here.
    pub fn available(&self) -> bool {
        self.path.is_some()
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Asked when a worker died of SIGKILL: did the OOM killer kill a
    /// process here since the last time, that no other death accounted for?
    pub fn took_one(&mut self) -> bool {
        let Some(now) = self.path.as_deref().and_then(read_counter) else { return false };
        if now > self.seen {
            self.seen += 1;
            true
        } else {
            self.seen = now; // a new cgroup started over at 0
            false
        }
    }
}

fn read_counter(p: &Path) -> Option<u64> {
    parse_oom_kill(&std::fs::read_to_string(p).ok()?)
}

/// The `oom_kill N` line of `memory.events` (v2) or `memory.oom_control` (v1).
pub fn parse_oom_kill(text: &str) -> Option<u64> {
    text.lines().find_map(|l| l.strip_prefix("oom_kill ")?.trim().parse().ok())
}

#[cfg(target_os = "linux")]
fn counter_path() -> Option<PathBuf> {
    // Tests point this at a file they write (a real OOM kill needs a cgroup
    // with a memory limit). Debug builds only.
    #[cfg(debug_assertions)]
    if let Some(p) = std::env::var_os("WARDEN_TEST_MEMORY_EVENTS") {
        return Some(PathBuf::from(p));
    }
    let cgroup = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    counter_candidates(&cgroup, &mounts).into_iter().find(|p| p.is_file())
}

#[cfg(not(target_os = "linux"))]
fn counter_path() -> Option<PathBuf> {
    None
}

/// Where the OOM kill counter of our cgroup may be, best first: the cgroup v2
/// `memory.events`, then the v1 memory controller's `memory.oom_control`.
/// `cgroup`: /proc/self/cgroup; `mountinfo`: /proc/self/mountinfo.
#[cfg(any(target_os = "linux", test))]
pub fn counter_candidates(cgroup: &str, mountinfo: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    // "0::/system.slice/warden@api.service" (v2), "4:memory:/docker/abc" (v1).
    let groups: Vec<(&str, &str)> = cgroup
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, ':');
            let (_, ctl, path) = (it.next()?, it.next()?, it.next()?);
            Some((ctl, path))
        })
        .collect();
    // mountinfo: "id parent maj:min root mountpoint opts... - fstype source superopts".
    let mounts: Vec<(&str, &str, &str, &str)> = mountinfo
        .lines()
        .filter_map(|l| {
            let (left, right) = l.split_once(" - ")?;
            let f: Vec<&str> = left.split_whitespace().collect();
            let r: Vec<&str> = right.split_whitespace().collect();
            Some((*f.get(3)?, *f.get(4)?, *r.first()?, r.get(2).copied().unwrap_or("")))
        })
        .collect();
    let file_in = |mount_root: &str, mount_point: &str, path: &str, file: &str| -> Option<PathBuf> {
        // Inside a container the mount's root is our own cgroup (or an ancestor).
        let rel = Path::new(path).strip_prefix(mount_root).ok()?;
        Some(Path::new(mount_point).join(rel).join(file))
    };
    for (ctl, path) in &groups {
        if ctl.is_empty() {
            for (root, point, fstype, _) in &mounts {
                if *fstype == "cgroup2" {
                    out.extend(file_in(root, point, path, "memory.events"));
                }
            }
        }
    }
    for (ctl, path) in &groups {
        if ctl.split(',').any(|c| c == "memory") {
            for (root, point, fstype, opts) in &mounts {
                if *fstype == "cgroup" && opts.split(',').any(|o| o == "memory") {
                    out.extend(file_in(root, point, path, "memory.oom_control"));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERM: i32 = libc::SIGTERM;

    fn sent(sigs: &[i32]) -> Sent {
        let mut s = Sent::default();
        sigs.iter().for_each(|x| s.add(*x));
        s
    }

    #[test]
    fn classification() {
        use Reason::*;
        let none = Sent::default();
        let k = Some(libc::SIGKILL);
        assert_eq!(classify(Some(1), None, none, TERM, false), Code(1));
        assert_eq!(classify(None, k, none, TERM, true), Oom, "the counter rose: OOM, whoever else signalled");
        assert_eq!(classify(None, k, sent(&[libc::SIGKILL]), TERM, true), Oom);
        assert_eq!(classify(None, k, sent(&[TERM, libc::SIGKILL]), TERM, false), KilledByWarden);
        assert_eq!(classify(None, k, sent(&[TERM]), TERM, false), Killed(libc::SIGKILL));
        assert_eq!(classify(None, k, none, TERM, false), Killed(libc::SIGKILL));
        assert_eq!(classify(Some(0), None, sent(&[TERM]), TERM, false), AfterStop { stop: TERM, code: Some(0) });
        assert_eq!(classify(None, Some(TERM), sent(&[TERM]), TERM, false), AfterStop { stop: TERM, code: None });
        // A SIGINT stop signal (PM2-style apps).
        let int = libc::SIGINT;
        assert_eq!(classify(None, Some(int), sent(&[int]), int, false), AfterStop { stop: int, code: None });
        assert_eq!(classify(None, Some(TERM), none, TERM, false), Killed(TERM), "someone else's SIGTERM");
        assert_eq!(classify(None, Some(libc::SIGSEGV), none, TERM, false), Crashed(libc::SIGSEGV));
        assert_eq!(classify(None, Some(libc::SIGABRT), sent(&[TERM]), TERM, false), Crashed(libc::SIGABRT));
        assert_eq!(
            classify(None, Some(libc::SIGUSR2), sent(&[libc::SIGUSR2]), TERM, false),
            SignalFromWarden(libc::SIGUSR2)
        );
        assert_eq!(classify(None, None, none, TERM, false), Unknown);
    }

    #[test]
    fn short_forms_and_hints() {
        assert_eq!(Reason::Code(3).short(), "exit code 3");
        assert_eq!(Reason::Killed(libc::SIGKILL).short(), "killed by another process (SIGKILL)");
        assert_eq!(Reason::KilledByWarden.short(), "killed by Warden (SIGKILL)");
        assert_eq!(Reason::Crashed(libc::SIGSEGV).short(), "crashed: SIGSEGV (segmentation fault)");
        assert_eq!(Reason::Crashed(libc::SIGABRT).short(), "crashed: SIGABRT (aborted)");
        assert_eq!(Reason::AfterStop { stop: TERM, code: Some(0) }.short(), "exit code 0 after Warden's SIGTERM");
        assert_eq!(Reason::AfterStop { stop: TERM, code: None }.short(), "stopped by Warden's SIGTERM");
        assert!(Reason::Oom.short().contains("OOM killer"));
        assert!(Reason::Oom.hint(true).unwrap().contains("memory.max / MemoryMax="));
        assert!(Reason::Oom.hint(true).unwrap().contains("[limits] max_memory"));
        assert!(Reason::Killed(libc::SIGKILL).hint(true).unwrap().contains("ruled out"));
        assert!(Reason::Killed(libc::SIGKILL).hint(false).unwrap().contains("journalctl -k"));
        assert!(Reason::Killed(TERM).hint(true).unwrap().contains("KillMode=mixed"));
        assert!(Reason::Crashed(libc::SIGSEGV).hint(true).unwrap().contains("coredumpctl"));
        assert_eq!(Reason::Code(1).hint(true), None);
        assert_eq!(Reason::KilledByWarden.hint(true), None);
    }

    /// The crash line's hint when `hint` has none follows the cause: Warden's
    /// own SIGKILL is not "the app exited by itself".
    #[test]
    fn plain_hints_follow_the_cause() {
        assert!(Reason::Code(1).plain_hint().contains("exited by itself"));
        assert!(Reason::KilledByWarden.plain_hint().starts_with("Warden killed it"));
        assert!(Reason::SignalFromWarden(libc::SIGUSR2).plain_hint().contains("signal Warden sent"));
        assert!(Reason::AfterStop { stop: TERM, code: Some(1) }.plain_hint().contains("signal Warden sent"));
        assert!(Reason::Unknown.plain_hint().contains("warden logs"));
        assert!(!Reason::KilledByWarden.plain_hint().contains("exited by itself"));
    }

    #[test]
    fn sent_set() {
        let s = sent(&[1, libc::SIGKILL, 64]);
        assert!(s.has(1) && s.has(libc::SIGKILL) && s.has(64));
        assert!(!s.has(TERM) && !s.has(0) && !s.has(65) && !s.has(-1));
        let mut s = Sent::default();
        s.add(0);
        s.add(99);
        assert_eq!(s, Sent::default());
    }

    #[test]
    fn parses_memory_events() {
        let v2 = "low 0\nhigh 0\nmax 12\noom 3\noom_kill 2\noom_group_kill 0\n";
        assert_eq!(parse_oom_kill(v2), Some(2));
        let v1 = "oom_kill_disable 0\nunder_oom 0\noom_kill 6\n";
        assert_eq!(parse_oom_kill(v1), Some(6));
        assert_eq!(parse_oom_kill("oom_kill_disable 0\nunder_oom 0\n"), None, "kernels before 4.13");
        assert_eq!(parse_oom_kill(""), None);
        assert_eq!(parse_oom_kill("oom_kill x\n"), None);
    }

    #[test]
    fn finds_the_counter_file() {
        // Pure cgroup v2, a systemd unit.
        let mi =
            "30 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:4 - cgroup2 cgroup2 rw,nsdelegate\n";
        assert_eq!(
            counter_candidates("0::/system.slice/system-warden.slice/warden@api.service\n", mi),
            vec![PathBuf::from("/sys/fs/cgroup/system.slice/system-warden.slice/warden@api.service/memory.events")]
        );
        // Hybrid: v2 without controllers at /sys/fs/cgroup/unified, memory on v1.
        let mi = "\
35 25 0:30 / /sys/fs/cgroup/unified rw,nosuid,nodev,noexec,relatime shared:10 - cgroup2 cgroup2 rw\n\
40 25 0:35 / /sys/fs/cgroup/memory rw,nosuid,nodev,noexec,relatime shared:16 - cgroup cgroup rw,memory\n\
41 25 0:36 / /sys/fs/cgroup/cpu,cpuacct rw,relatime shared:17 - cgroup cgroup rw,cpu,cpuacct\n";
        let cg = "5:cpu,cpuacct:/user.slice\n4:memory:/user.slice/user-1000.slice\n0::/user.slice/user-1000.slice\n";
        assert_eq!(
            counter_candidates(cg, mi),
            vec![
                PathBuf::from("/sys/fs/cgroup/unified/user.slice/user-1000.slice/memory.events"),
                PathBuf::from("/sys/fs/cgroup/memory/user.slice/user-1000.slice/memory.oom_control"),
            ]
        );
        // A container whose cgroup is the mount's root.
        let mi = "600 500 0:40 /docker/abc /sys/fs/cgroup ro,nosuid - cgroup2 cgroup rw\n";
        assert_eq!(counter_candidates("0::/docker/abc\n", mi), vec![PathBuf::from("/sys/fs/cgroup/memory.events")]);
        assert!(counter_candidates("0::/elsewhere\n", mi).is_empty(), "not under the mount's root");
        assert!(counter_candidates("0::/docker/abcdef\n", mi).is_empty(), "a sibling, not a child");
        assert_eq!(
            counter_candidates("0::/docker/abc/sub\n", mi),
            vec![PathBuf::from("/sys/fs/cgroup/sub/memory.events")]
        );
        assert!(counter_candidates("", "").is_empty());
        assert!(counter_candidates("garbage\n", "junk without separator\n").is_empty());
    }

    #[test]
    fn counter_takes_one_kill_per_death() {
        let dir = std::env::temp_dir().join(format!("warden-oom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("memory.events");
        std::fs::write(&f, "oom_kill 4\n").unwrap();
        let mut c = OomCounter { path: Some(f.clone()), seen: 4 };
        assert!(!c.took_one(), "no change");
        std::fs::write(&f, "oom_kill 6\n").unwrap();
        assert!(c.took_one() && c.took_one(), "two workers killed at once: one each");
        assert!(!c.took_one());
        std::fs::write(&f, "oom_kill 1\n").unwrap();
        assert!(!c.took_one(), "a new cgroup starts over");
        std::fs::write(&f, "oom_kill 2\n").unwrap();
        assert!(c.took_one());
        std::fs::remove_file(&f).unwrap();
        assert!(!c.took_one(), "unreadable: not an OOM kill");
        let none = OomCounter { path: None, seen: 0 };
        assert!(!none.available());
        let _ = std::fs::remove_dir_all(dir);
    }
}

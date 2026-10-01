//! Why a worker died, for `last_exit` and the log line of its death.
//!
//! The wait status alone says "signal 9". With what Warden knows, the reason
//! says who: Warden itself (its waiter delivered that signal), the kernel's
//! OOM killer (the cgroup's `oom_kill` counter went up just before), another
//! process, or the program crashing (SIGSEGV, SIGABRT…).
//!
//! OOM kills are counted per cgroup, not per process. Each worker is tracked
//! in the cgroup it runs in: Warden's own (systemd's unit, a container: every
//! worker shares it) or one of its own (a worker started in a cgroup of its
//! own, e.g. through `systemd-run --scope`; Warden creates none). A kill
//! counted there is charged to at most one SIGKILL death, Warden's own
//! SIGKILLs excepted; when other processes of Warden's in the same cgroup die
//! of SIGKILL at the same moment, more than the kills counted, the reason
//! says the attribution is uncertain instead of guessing (`OomVerdict`).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
    /// SIGKILLed just after its cgroup counted an OOM kill, but other
    /// processes there died of SIGKILL at the same moment, more than the
    /// kills: probably the OOM killer ([`OomVerdict::Probable`]).
    OomProbable,
    /// SIGKILLed next to another process of the same cgroup that took the
    /// OOM kill both could own: the OOM killer or someone else's kill -9
    /// ([`OomVerdict::Possible`]).
    KilledOrOom,
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

/// The exit code a shell reports for a child that died of SIGKILL (128 + 9).
pub const SHELL_SIGKILL: i32 = 128 + libc::SIGKILL;

/// The signal to ask the OOM counter about: the SIGKILL that ended the
/// process, or the one a shell wrapper reports as exit code 137.
pub fn kill_signal(code: Option<i32>, signal: Option<i32>) -> Option<i32> {
    match (code, signal) {
        (Some(SHELL_SIGKILL), None) => Some(libc::SIGKILL),
        _ => signal,
    }
}

/// What the wait status and Warden's own records say about one death.
/// `oom`: what the cgroup's OOM kill count says (`OomTracker::verdict`). A
/// SIGKILL Warden sent is Warden's, even if the counter moved meanwhile (a
/// helper process, another worker).
pub fn classify(code: Option<i32>, signal: Option<i32>, sent: Sent, stop: i32, oom: OomVerdict) -> Reason {
    match (code, signal) {
        (_, Some(libc::SIGKILL)) if sent.has(libc::SIGKILL) => Reason::KilledByWarden,
        (_, Some(libc::SIGKILL)) if oom == OomVerdict::Certain => Reason::Oom,
        (_, Some(libc::SIGKILL)) if oom == OomVerdict::Probable => Reason::OomProbable,
        (_, Some(libc::SIGKILL)) if oom == OomVerdict::Possible => Reason::KilledOrOom,
        (_, Some(s)) if s == stop && sent.has(stop) => Reason::AfterStop { stop, code: None },
        (_, Some(s)) if sent.has(s) => Reason::SignalFromWarden(s),
        (_, Some(s)) if crash_signal(s).is_some() => Reason::Crashed(s),
        (_, Some(s)) => Reason::Killed(s),
        (Some(c), None) if sent.has(stop) => Reason::AfterStop { stop, code: Some(c) },
        // A shell wrapper (`sh -c 'bun app.ts'`) whose child was SIGKILLed
        // exits with 128 + 9 itself: the same death, one process removed.
        (Some(SHELL_SIGKILL), None) if oom == OomVerdict::Certain => Reason::Oom,
        (Some(SHELL_SIGKILL), None) if oom == OomVerdict::Probable => Reason::OomProbable,
        (Some(SHELL_SIGKILL), None) if oom == OomVerdict::Possible => Reason::KilledOrOom,
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
            Reason::Oom => warden_protocol::events::OOM_KILLED.into(),
            Reason::OomProbable => warden_protocol::events::OOM_PROBABLY.into(),
            Reason::KilledOrOom => "killed by another process or the kernel OOM killer (SIGKILL)".into(),
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
    /// SIGKILL that did not raise it just before was probably not the OOM
    /// killer (the counter is read once a second and at each death).
    pub fn hint(self, oom_detection: bool) -> Option<&'static str> {
        Some(match self {
            Reason::Oom => OOM_HINT,
            Reason::OomProbable => {
                "its cgroup counted an OOM kill just before, but other processes of Warden's there died of SIGKILL \
                 at the same moment, more than the kills counted (the cgroup's count is shared), so the kernel may \
                 have killed one of them instead: `journalctl -k | grep -i 'killed process'` names the pid. If it was \
                 this one, raise memory.max / MemoryMax= or lower `[limits] max_memory` so Warden recycles it before \
                 the kernel kills it"
            }
            Reason::KilledOrOom => {
                "it died of SIGKILL at the same moment as another process in its cgroup, which took the cgroup's \
                 OOM kill: one of them was the kernel's OOM victim, the other killed by something else (kill -9, a \
                 container runtime); `journalctl -k | grep -i 'killed process'` names the pid the kernel killed. \
                 Warden restarts it"
            }
            Reason::Killed(libc::SIGKILL) if oom_detection => {
                "something outside Warden sent SIGKILL (kill -9 by a person or script, a container runtime); \
                 Warden restarts it. Probably not the kernel's OOM killer: the cgroup's oom_kill count did not rise \
                 in the 2 s before this death (`journalctl -k | grep -i oom` to be sure)"
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

/// How recent an OOM kill must be to explain a SIGKILL death. The kernel
/// counts the kill as it sends the SIGKILL, and Warden reads the counter
/// every second (the supervisor's tick) and at each death, so a worker's
/// own OOM kill is seen within this; an older one was something else's in
/// the same cgroup (a helper process the app spawned, a verify_command).
pub const OOM_WINDOW: Duration = Duration::from_secs(2);

/// Kills remembered at most (a burst in a big cgroup must not grow this).
const MAX_FRESH: usize = 1024;

/// What a cgroup's OOM kill count says about one SIGKILL death.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OomVerdict {
    /// No OOM kill for it: none counted within `OOM_WINDOW`, no readable
    /// counter, not a SIGKILL, or Warden's own SIGKILL.
    No,
    /// Its cgroup counted an OOM kill within `OOM_WINDOW` before, and as
    /// many kills as Warden's processes there dying of SIGKILL: its own.
    Certain,
    /// A kill was counted, but more of Warden's processes in the cgroup died
    /// of SIGKILL at that moment than kills were counted: probably its own.
    Probable,
    /// No kill left for it, but it died of SIGKILL in a cgroup where, within
    /// `OOM_WINDOW`, another death took the kill both could own.
    Possible,
}

/// The kernel's count of OOM kills in one cgroup: cgroup v2
/// `memory.events.local` (or `memory.events` before Linux 5.2), or v1
/// `memory.oom_control`, read at each death and on the supervisor's tick.
/// Each rise is stamped when it was seen; a SIGKILL death takes at most one
/// rise seen within `OOM_WINDOW` before it.
pub struct OomCounter {
    path: Option<PathBuf>,
    /// The count at the last read.
    seen: u64,
    /// When each kill no death has taken yet was first seen, oldest first.
    fresh: VecDeque<Instant>,
    /// Warden's own SIGKILLs of processes here while a kill was fresh: that
    /// kill may have been theirs, so whoever takes it is not alone.
    warden_kills: VecDeque<Instant>,
    /// A kill was taken while others could own it: deaths of SIGKILL until
    /// then, with no kill left, may have been the kernel's.
    ambiguous_until: Option<Instant>,
}

impl OomCounter {
    /// The counter in `path`, its current count the baseline (`None`: unreadable).
    fn at(path: Option<PathBuf>) -> OomCounter {
        let seen = path.as_deref().and_then(read_counter).unwrap_or(0);
        OomCounter { path, seen, fresh: VecDeque::new(), warden_kills: VecDeque::new(), ambiguous_until: None }
    }

    /// Read the counter: kills since the last read are stamped `now`.
    fn sample(&mut self, now: Instant) {
        if let Some(n) = self.path.as_deref().and_then(read_counter) {
            self.note(n, now);
        }
    }

    /// The counter read `n` at `now`.
    fn note(&mut self, n: u64, now: Instant) {
        if n < self.seen {
            // A new cgroup started over at 0.
            self.fresh.clear();
            self.warden_kills.clear();
            self.ambiguous_until = None;
        } else {
            let new = usize::try_from(n - self.seen).unwrap_or(MAX_FRESH).min(MAX_FRESH);
            self.fresh.extend(std::iter::repeat_n(now, new));
            while self.fresh.len() > MAX_FRESH {
                self.fresh.pop_front();
            }
        }
        self.seen = n;
        self.forget_old(now);
    }

    fn forget_old(&mut self, now: Instant) {
        let old = |t: &Instant| now.saturating_duration_since(*t) > OOM_WINDOW;
        while self.fresh.front().is_some_and(old) {
            self.fresh.pop_front();
        }
        while self.warden_kills.front().is_some_and(old) {
            self.warden_kills.pop_front();
        }
        if self.ambiguous_until.is_some_and(|t| now > t) {
            self.ambiguous_until = None;
        }
    }

    /// A process of this cgroup died of `signal` at `now` (`sent`: the
    /// signals Warden delivered to it). `rivals` counts the other processes
    /// of Warden's in the cgroup dying of SIGKILL at this moment (exiting,
    /// a zombie, or reaped with that death not handled yet); it is asked
    /// only when there is a kill to share.
    pub fn verdict(
        &mut self,
        signal: Option<i32>,
        sent: Sent,
        now: Instant,
        rivals: impl FnOnce() -> usize,
    ) -> OomVerdict {
        if signal != Some(libc::SIGKILL) {
            return OomVerdict::No;
        }
        self.sample(now);
        self.forget_old(now);
        if sent.has(libc::SIGKILL) {
            // Warden's own kill stays Warden's and takes no kill; but a kill
            // waiting now may have been this process's too.
            if !self.fresh.is_empty() && self.warden_kills.len() < MAX_FRESH {
                self.warden_kills.push_back(now);
            }
            return OomVerdict::No;
        }
        if self.fresh.is_empty() {
            return if self.ambiguous_until.is_some() { OomVerdict::Possible } else { OomVerdict::No };
        }
        let kills = self.fresh.len();
        let others = rivals().saturating_add(self.warden_kills.len());
        self.fresh.pop_front();
        if kills > others {
            OomVerdict::Certain
        } else {
            self.ambiguous_until = Some(crate::restart::later(now, OOM_WINDOW));
            OomVerdict::Probable
        }
    }
}

/// The OOM kill counters of the cgroups Warden's processes run in: its own
/// (every worker starts there), and any cgroup of their own a worker was
/// found in once ready (`attach`).
pub struct OomTracker {
    /// Warden's own cgroup's counter file, when readable.
    own: Option<PathBuf>,
    /// /proc/self/cgroup and /proc/self/mountinfo, to find a worker's own
    /// cgroup and its counter (workers share Warden's mount namespace).
    own_cgroup: String,
    mountinfo: String,
    counters: HashMap<PathBuf, OomCounter>,
}

impl OomTracker {
    pub fn new() -> OomTracker {
        let own = counter_path();
        let read = |p: &str| std::fs::read_to_string(p).unwrap_or_default();
        let mut counters = HashMap::new();
        if let Some(p) = &own {
            counters.insert(p.clone(), OomCounter::at(Some(p.clone())));
        }
        OomTracker { own, own_cgroup: read("/proc/self/cgroup"), mountinfo: read("/proc/self/mountinfo"), counters }
    }

    /// Warden's own cgroup's counter file: every worker's until `attach`.
    pub fn own(&self) -> Option<PathBuf> {
        self.own.clone()
    }

    /// The counter file for process `pid`'s cgroup, now tracked: Warden's
    /// own when it runs in Warden's cgroup, else its cgroup's own counter;
    /// `None` when that one can't be read (then its OOM kills can't be told
    /// apart).
    pub fn attach(&mut self, pid: u32) -> Option<PathBuf> {
        let Ok(cgroup) = std::fs::read_to_string(format!("/proc/{pid}/cgroup")) else { return self.own() };
        if cgroup == self.own_cgroup {
            return self.own();
        }
        let path = counter_candidates(&cgroup, &self.mountinfo).into_iter().find(|p| p.is_file())?;
        self.counters.entry(path.clone()).or_insert_with(|| OomCounter::at(Some(path.clone())));
        Some(path)
    }

    /// Read every counter (the supervisor's 1 s tick), and forget those of
    /// cgroups no process in `in_use` runs in any more.
    pub fn sample<'a>(&mut self, now: Instant, in_use: impl Iterator<Item = &'a Path>) {
        let keep: std::collections::HashSet<&Path> = in_use.collect();
        let own = self.own.clone();
        self.counters.retain(|p, _| own.as_deref() == Some(p.as_path()) || keep.contains(p.as_path()));
        for c in self.counters.values_mut() {
            c.sample(now);
        }
    }

    /// A process counted in `counter` (`OomTracker::attach`) died: see
    /// `OomCounter::verdict`.
    pub fn verdict(
        &mut self,
        counter: Option<&Path>,
        signal: Option<i32>,
        sent: Sent,
        now: Instant,
        rivals: impl FnOnce() -> usize,
    ) -> OomVerdict {
        if signal != Some(libc::SIGKILL) {
            return OomVerdict::No;
        }
        let Some(path) = counter else { return OomVerdict::No };
        let c = self.counters.entry(path.to_path_buf()).or_insert_with(|| OomCounter::at(Some(path.to_path_buf())));
        c.verdict(signal, sent, now, rivals)
    }
}

/// Is process `pid` (a child of Warden's, not reaped yet) on its way out by
/// SIGKILL: pending, exiting from it, or a zombie it killed? Its pid can't
/// have been reused while it is unreaped.
pub fn dying_of_sigkill(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        if let Some((state, flags, exit_code)) = parse_stat_exit(&stat) {
            if matches!(state, 'Z' | 'X' | 'x') || flags & PF_EXITING != 0 {
                return exit_code.is_some_and(|c| c & 0x7f == i64::from(libc::SIGKILL));
            }
        }
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
        sigkill_pending(&status)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        false
    }
}

/// `PF_EXITING` in /proc/<pid>/stat's flags: the process is in `do_exit`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const PF_EXITING: u64 = 0x4;

/// (state, flags, exit_code) from /proc/<pid>/stat: fields 3, 9 and 52
/// (`exit_code` since Linux 3.5: the wait status, so a SIGKILL death is 9).
/// The command name may hold spaces and parens: split after the last ')'.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn parse_stat_exit(stat: &str) -> Option<(char, u64, Option<i64>)> {
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let state = f.first()?.chars().next()?;
    let flags = f.get(6)?.parse().ok()?;
    Some((state, flags, f.get(49).and_then(|c| c.parse().ok())))
}

/// SIGKILL pending for the process, from /proc/<pid>/status (`SigPnd` for
/// its thread, `ShdPnd` for the whole process: hex masks, bit n-1 for n).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sigkill_pending(status: &str) -> bool {
    let bit = 1u64 << (libc::SIGKILL - 1);
    status.lines().any(|l| {
        let mask = l.strip_prefix("SigPnd:").or_else(|| l.strip_prefix("ShdPnd:"));
        mask.and_then(|m| u64::from_str_radix(m.trim(), 16).ok()).is_some_and(|m| m & bit != 0)
    })
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

/// Where the OOM kill counter of a cgroup may be, best first: the cgroup v2
/// `memory.events.local` (kills of processes in that cgroup only, Linux
/// 5.2+), `memory.events` (its descendants' too), then the v1 memory
/// controller's `memory.oom_control` (that cgroup only). Local counts keep a
/// kill in a worker's own cgroup below Warden's from being counted in
/// Warden's too. `cgroup`: /proc/<pid>/cgroup; `mountinfo`: /proc/self/mountinfo.
/// (Plain text parsing, so it builds everywhere: `OomTracker::attach` calls it
/// on every platform, where the files simply don't exist.)
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
                    out.extend(file_in(root, point, path, "memory.events.local"));
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
        use OomVerdict as V;
        use Reason::*;
        let none = Sent::default();
        let k = Some(libc::SIGKILL);
        assert_eq!(classify(Some(1), None, none, TERM, V::No), Code(1));
        // A shell wrapper reports its SIGKILLed child as exit code 137: the
        // counter decides, as for a SIGKILL of the process itself.
        assert_eq!(kill_signal(Some(137), None), Some(libc::SIGKILL));
        assert_eq!(kill_signal(Some(1), None), None);
        assert_eq!(kill_signal(None, Some(libc::SIGTERM)), Some(libc::SIGTERM));
        assert_eq!(classify(Some(137), None, none, TERM, V::Certain), Oom);
        assert_eq!(classify(Some(137), None, none, TERM, V::Probable), OomProbable);
        assert_eq!(classify(Some(137), None, none, TERM, V::Possible), KilledOrOom);
        assert_eq!(classify(Some(137), None, none, TERM, V::No), Code(137), "no kill counted: an exit code");
        assert_eq!(classify(Some(137), None, sent(&[TERM]), TERM, V::No), AfterStop { stop: TERM, code: Some(137) });
        assert_eq!(classify(None, k, none, TERM, V::Certain), Oom, "the counter rose: OOM");
        assert_eq!(classify(None, k, sent(&[TERM]), TERM, V::Certain), Oom, "killed while draining");
        assert_eq!(classify(None, k, none, TERM, V::Probable), OomProbable);
        assert_eq!(classify(None, k, none, TERM, V::Possible), KilledOrOom);
        for v in [V::Certain, V::Probable, V::Possible] {
            assert_eq!(
                classify(None, k, sent(&[libc::SIGKILL]), TERM, v),
                KilledByWarden,
                "Warden's own SIGKILL wins over a counter that moved meanwhile"
            );
        }
        assert_eq!(classify(None, k, sent(&[TERM, libc::SIGKILL]), TERM, V::No), KilledByWarden);
        assert_eq!(classify(None, k, sent(&[TERM]), TERM, V::No), Killed(libc::SIGKILL));
        assert_eq!(classify(None, k, none, TERM, V::No), Killed(libc::SIGKILL));
        assert_eq!(classify(Some(0), None, sent(&[TERM]), TERM, V::No), AfterStop { stop: TERM, code: Some(0) });
        assert_eq!(classify(None, Some(TERM), sent(&[TERM]), TERM, V::No), AfterStop { stop: TERM, code: None });
        // A SIGINT stop signal (PM2-style apps).
        let int = libc::SIGINT;
        assert_eq!(classify(None, Some(int), sent(&[int]), int, V::No), AfterStop { stop: int, code: None });
        assert_eq!(classify(None, Some(TERM), none, TERM, V::No), Killed(TERM), "someone else's SIGTERM");
        assert_eq!(classify(None, Some(libc::SIGSEGV), none, TERM, V::No), Crashed(libc::SIGSEGV));
        assert_eq!(classify(None, Some(libc::SIGABRT), sent(&[TERM]), TERM, V::No), Crashed(libc::SIGABRT));
        assert_eq!(
            classify(None, Some(libc::SIGUSR2), sent(&[libc::SIGUSR2]), TERM, V::No),
            SignalFromWarden(libc::SIGUSR2)
        );
        assert_eq!(classify(None, None, none, TERM, V::No), Unknown);
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
        assert!(Reason::Oom.short() == "killed by the kernel OOM killer (out of memory)", "alerts match it");
        // Uncertain: said so, and only the certain reason is OOM_KILLED itself.
        let probable = Reason::OomProbable.short();
        assert_eq!(probable, warden_protocol::events::OOM_PROBABLY);
        assert!(probable.starts_with("probably ") && probable.contains(warden_protocol::events::OOM_KILLED));
        let hint = Reason::OomProbable.hint(true).unwrap();
        assert!(hint.contains("journalctl -k") && hint.contains("MemoryMax="), "{hint}");
        let either = Reason::KilledOrOom.short();
        assert_eq!(either, "killed by another process or the kernel OOM killer (SIGKILL)");
        assert!(!either.contains(warden_protocol::events::OOM_KILLED), "no `oom` alert for a coin toss");
        assert!(Reason::KilledOrOom.hint(true).unwrap().contains("journalctl -k"));
        let kill9 = Reason::Killed(libc::SIGKILL).hint(true).unwrap();
        assert!(kill9.contains("Probably not the kernel's OOM killer") && !kill9.contains("ruled out"), "{kill9}");
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
        // Pure cgroup v2, a systemd unit: the local count first.
        let mi =
            "30 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime shared:4 - cgroup2 cgroup2 rw,nsdelegate\n";
        let unit = "/sys/fs/cgroup/system.slice/system-warden.slice/warden@api.service";
        assert_eq!(
            counter_candidates("0::/system.slice/system-warden.slice/warden@api.service\n", mi),
            vec![PathBuf::from(format!("{unit}/memory.events.local")), PathBuf::from(format!("{unit}/memory.events"))]
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
                PathBuf::from("/sys/fs/cgroup/unified/user.slice/user-1000.slice/memory.events.local"),
                PathBuf::from("/sys/fs/cgroup/unified/user.slice/user-1000.slice/memory.events"),
                PathBuf::from("/sys/fs/cgroup/memory/user.slice/user-1000.slice/memory.oom_control"),
            ]
        );
        // A container whose cgroup is the mount's root.
        let mi = "600 500 0:40 /docker/abc /sys/fs/cgroup ro,nosuid - cgroup2 cgroup rw\n";
        assert_eq!(
            counter_candidates("0::/docker/abc\n", mi),
            vec![PathBuf::from("/sys/fs/cgroup/memory.events.local"), PathBuf::from("/sys/fs/cgroup/memory.events")]
        );
        assert!(counter_candidates("0::/elsewhere\n", mi).is_empty(), "not under the mount's root");
        assert!(counter_candidates("0::/docker/abcdef\n", mi).is_empty(), "a sibling, not a child");
        assert_eq!(
            counter_candidates("0::/docker/abc/sub\n", mi)[0],
            PathBuf::from("/sys/fs/cgroup/sub/memory.events.local")
        );
        assert!(counter_candidates("", "").is_empty());
        assert!(counter_candidates("garbage\n", "junk without separator\n").is_empty());
    }

    const KILL: Option<i32> = Some(libc::SIGKILL);
    const NONE: Sent = Sent(0);

    fn counter(path: Option<PathBuf>, seen: u64) -> OomCounter {
        let mut c = OomCounter::at(None);
        c.path = path;
        c.seen = seen;
        c
    }

    /// No rival dying: the closure `verdict` asks only when a kill is there.
    fn alone() -> usize {
        0
    }

    #[test]
    fn counter_takes_one_kill_per_death() {
        use OomVerdict as V;
        let dir = std::env::temp_dir().join(format!("warden-oom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("memory.events");
        std::fs::write(&f, "oom_kill 4\n").unwrap();
        let mut c = counter(Some(f.clone()), 4);
        let now = Instant::now();
        let never = || -> usize { panic!("asked for rivals without a kill to share") };
        assert_eq!(c.verdict(KILL, NONE, now, never), V::No, "no change");
        std::fs::write(&f, "oom_kill 6\n").unwrap();
        // Two killed at once, both dying now: one kill each.
        assert_eq!(c.verdict(KILL, NONE, now, || 1), V::Certain);
        assert_eq!(c.verdict(KILL, NONE, now, alone), V::Certain);
        assert_eq!(c.verdict(KILL, NONE, now, never), V::No, "no kill left, none shared");
        std::fs::write(&f, "oom_kill 1\n").unwrap();
        assert_eq!(c.verdict(KILL, NONE, now, never), V::No, "a new cgroup starts over");
        std::fs::write(&f, "oom_kill 2\n").unwrap();
        assert_eq!(c.verdict(Some(libc::SIGTERM), NONE, now, never), V::No, "only SIGKILL deaths ask");
        assert_eq!(c.verdict(Some(1), NONE, now, never), V::No);
        assert_eq!(c.verdict(KILL, NONE, now, alone), V::Certain, "and the kill was still there");
        std::fs::remove_file(&f).unwrap();
        assert_eq!(c.verdict(KILL, NONE, now, never), V::No, "unreadable: not an OOM kill");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Only a kill seen within OOM_WINDOW before a death is charged to it:
    /// one from long before (a helper process, a verify_command in the same
    /// cgroup) used to be charged to the next SIGKILL death, however late.
    #[test]
    fn an_old_oom_kill_is_not_charged_to_a_later_death() {
        use OomVerdict as V;
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut c = counter(None, 4);
        c.note(5, t0); // the tick sees a kill
        assert_eq!(c.verdict(KILL, NONE, at(60), alone), V::No, "a minute later: someone else's kill");
        c.note(5, at(61));
        assert_eq!(c.verdict(KILL, NONE, at(61), alone), V::No, "and it stays forgotten");
        // Seen by the tick 1 s before the death, or at the death itself.
        c.note(6, at(100));
        assert_eq!(c.verdict(KILL, NONE, at(101), alone), V::Certain);
        c.note(7, at(110));
        assert_eq!(c.verdict(KILL, NONE, at(110), alone), V::Certain);
        assert_eq!(c.verdict(KILL, NONE, at(110), alone), V::No, "one kill, one death");
        assert_eq!(OOM_WINDOW, Duration::from_secs(2), "the hint says 2 s");
    }

    /// One kill, two SIGKILL deaths at the same moment (the OOM killer took
    /// worker A, someone's kill -9 worker B): whichever is handled first saw
    /// the other dying, so neither is called an OOM kill for sure. The first
    /// is `probably`, the second `possibly`; whatever the order, never
    /// "OOM" for one and "not the OOM killer" for the other.
    #[test]
    fn one_kill_for_two_sigkill_deaths_is_uncertain_for_both() {
        use OomVerdict as V;
        let t0 = Instant::now();
        let mut c = counter(None, 0);
        c.note(1, t0);
        assert_eq!(c.verdict(KILL, NONE, t0, || 1), V::Probable, "the other one is dying too");
        assert_eq!(c.verdict(KILL, NONE, t0 + Duration::from_millis(30), alone), V::Possible);
        // Another SIGKILL death after the window, with no new kill: not the kernel's.
        assert_eq!(c.verdict(KILL, NONE, t0 + Duration::from_secs(3), alone), V::No);
        // As many kills as deaths: certain for each.
        c.note(3, t0 + Duration::from_secs(10));
        assert_eq!(c.verdict(KILL, NONE, t0 + Duration::from_secs(10), || 1), V::Certain);
        assert_eq!(c.verdict(KILL, NONE, t0 + Duration::from_secs(10), alone), V::Certain);
    }

    /// A worker that exits by itself (or by any other signal) when a kill is
    /// counted doesn't take it: the SIGKILL death next to it does.
    #[test]
    fn only_a_sigkill_death_takes_a_kill() {
        use OomVerdict as V;
        let t0 = Instant::now();
        let mut c = counter(None, 0);
        c.note(1, t0);
        assert_eq!(c.verdict(None, NONE, t0, || panic!("not asked")), V::No, "exit code 1");
        assert_eq!(c.verdict(Some(libc::SIGTERM), NONE, t0, || panic!("not asked")), V::No);
        assert_eq!(c.verdict(KILL, NONE, t0, alone), V::Certain);
    }

    /// Warden's own SIGKILL (grace period over, hung, not ready in time) is
    /// Warden's even if the counter moved meanwhile, and takes no kill; but
    /// the kill may have been that process's, so the other SIGKILL death
    /// that takes it is told `probably`. A kill counted only after Warden's
    /// SIGKILL was not that process's.
    #[test]
    fn wardens_own_kill_is_never_an_oom_kill() {
        use OomVerdict as V;
        let (t0, warden) = (Instant::now(), sent(&[libc::SIGTERM, libc::SIGKILL]));
        let mut c = counter(None, 0);
        c.note(1, t0);
        let oom = c.verdict(KILL, warden, t0, || panic!("not asked"));
        assert_eq!(oom, V::No);
        assert_eq!(classify(None, KILL, warden, TERM, oom), Reason::KilledByWarden);
        // The other worker dying of SIGKILL at the same time gets the kill, not for sure.
        let oom = c.verdict(KILL, NONE, t0, alone);
        assert_eq!(classify(None, KILL, NONE, TERM, oom), Reason::OomProbable);
        // Warden's SIGKILL with no kill counted yet: the next one is someone else's.
        let t1 = t0 + Duration::from_secs(10);
        assert_eq!(c.verdict(KILL, warden, t1, || panic!("not asked")), V::No);
        c.note(2, t1 + Duration::from_millis(500));
        assert_eq!(c.verdict(KILL, NONE, t1 + Duration::from_millis(600), alone), V::Certain);
        // A burst is bounded.
        c.note(1 << 40, t1);
        assert_eq!(c.fresh.len(), MAX_FRESH);
    }

    /// A kill counted after a SIGKILL death was handled is not that death's:
    /// the kernel counts a kill once the SIGKILL is sent, before its victim
    /// can have exited.
    #[test]
    fn a_kill_counted_later_belongs_to_a_later_death() {
        use OomVerdict as V;
        let t0 = Instant::now();
        let mut c = counter(None, 0);
        assert_eq!(c.verdict(KILL, NONE, t0, || panic!("not asked")), V::No, "someone's kill -9");
        c.note(1, t0 + Duration::from_millis(200));
        assert_eq!(c.verdict(KILL, NONE, t0 + Duration::from_millis(250), alone), V::Certain);
    }

    /// Workers in cgroups of their own are counted there: a kill in one
    /// cgroup is never charged to a death in another, however close.
    #[test]
    fn each_cgroup_counts_its_own_kills() {
        use OomVerdict as V;
        let dir = std::env::temp_dir().join(format!("warden-oom-groups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (own, a, b) = (dir.join("own"), dir.join("a"), dir.join("b"));
        for f in [&own, &a, &b] {
            std::fs::write(f, "oom_kill 0\n").unwrap();
        }
        let mut t = OomTracker {
            own: Some(own.clone()),
            own_cgroup: String::new(),
            mountinfo: String::new(),
            counters: HashMap::new(),
        };
        t.counters.insert(own.clone(), OomCounter::at(Some(own.clone())));
        t.counters.insert(a.clone(), OomCounter::at(Some(a.clone())));
        t.counters.insert(b.clone(), OomCounter::at(Some(b.clone())));
        let now = Instant::now();
        std::fs::write(&a, "oom_kill 1\n").unwrap();
        t.sample(now, [a.as_path(), b.as_path()].into_iter());
        assert_eq!(
            t.verdict(Some(&b), KILL, NONE, now, || panic!("not asked")),
            V::No,
            "b's death: a's kill is not its"
        );
        assert_eq!(t.verdict(Some(&own), KILL, NONE, now, || panic!("not asked")), V::No, "nor a worker's in Warden's");
        assert_eq!(t.verdict(Some(&a), KILL, NONE, now, alone), V::Certain, "a's own");
        assert_eq!(t.verdict(None, KILL, NONE, now, || panic!("not asked")), V::No, "no readable counter");
        // Cgroups no process runs in any more are forgotten; Warden's stays.
        t.sample(now, [a.as_path()].into_iter());
        assert!(t.counters.contains_key(&own) && t.counters.contains_key(&a) && !t.counters.contains_key(&b));
        assert_eq!(t.own(), Some(own));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Our own process, or one gone: Warden's own counter.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_in_wardens_cgroup_uses_wardens_counter() {
        let mut t = OomTracker::new();
        assert_eq!(t.attach(std::process::id()), t.own());
        assert_eq!(t.attach(u32::MAX / 2), t.own(), "no such process: where it started");
    }

    #[test]
    fn reads_the_dying_state_of_a_process() {
        // /proc/<pid>/stat: a zombie killed by SIGKILL (exit_code 9), the name with spaces and parens.
        let mut f = vec!["0"; 52];
        f[0] = "4242";
        f[1] = "(a (b) c)";
        f[2] = "Z";
        f[8] = "4194564"; // PF_EXITING among the flags
        f[51] = "9";
        let zombie = f.join(" ");
        assert_eq!(parse_stat_exit(&zombie), Some(('Z', 4194564, Some(9))));
        f[2] = "S";
        f[8] = "4194560";
        f[51] = "0";
        assert_eq!(parse_stat_exit(&f.join(" ")), Some(('S', 4194560, Some(0))));
        assert_eq!(parse_stat_exit(&f[..8].join(" ")), None, "no flags field");
        assert_eq!(parse_stat_exit(&f[..40].join(" ")), Some(('S', 4194560, None)), "before Linux 3.5");
        assert_eq!(parse_stat_exit(""), None);
        // /proc/<pid>/status: SIGKILL (bit 8) pending for the thread or the process.
        assert!(sigkill_pending("Name:\tbun\nSigPnd:\t0000000000000100\nShdPnd:\t0000000000000000\n"));
        assert!(sigkill_pending("SigPnd:\t0000000000000000\nShdPnd:\t0000000000000102\n"));
        assert!(!sigkill_pending("SigPnd:\t0000000000004000\nShdPnd:\t0000000000000000\n"), "SIGTERM");
        assert!(!sigkill_pending(""));
    }

    /// A real child, not reaped yet: dying of SIGKILL or not.
    #[cfg(target_os = "linux")]
    #[test]
    fn tells_a_child_dying_of_sigkill() {
        let zombie_of = |sig: i32| {
            let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let pid = child.id();
            assert!(!dying_of_sigkill(pid), "alive and well");
            crate::sys::kill(i32::try_from(pid).unwrap(), sig).unwrap();
            let t0 = Instant::now();
            while !std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default().contains(") Z ") {
                assert!(t0.elapsed() < Duration::from_secs(5), "never a zombie");
                std::thread::sleep(Duration::from_millis(5));
            }
            let dying = dying_of_sigkill(pid);
            let mut child = child;
            let _ = child.wait();
            dying
        };
        assert!(zombie_of(libc::SIGKILL));
        assert!(!zombie_of(libc::SIGTERM));
    }
}

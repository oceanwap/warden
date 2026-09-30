//! sd_notify(3) without libsystemd: one datagram to $NOTIFY_SOCKET.
//! Lets a `Type=notify` unit report "started" only once workers are listening.

pub fn notify(state: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else { return };
    let Ok(sock) = std::os::unix::net::UnixDatagram::unbound() else { return };
    let p = path.to_string_lossy();
    let _ = if let Some(name) = p.strip_prefix('@') {
        send_abstract(&sock, name, state)
    } else {
        sock.send_to(state.as_bytes(), &*p).map(|_| ())
    };
}

#[cfg(target_os = "linux")]
fn send_abstract(sock: &std::os::unix::net::UnixDatagram, name: &str, state: &str) -> std::io::Result<()> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    sock.send_to_addr(state.as_bytes(), &addr).map(|_| ())
}

#[cfg(not(target_os = "linux"))]
fn send_abstract(_: &std::os::unix::net::UnixDatagram, _: &str, _: &str) -> std::io::Result<()> {
    Ok(())
}

/// systemd asks for keep-alive pings by setting WATCHDOG_USEC (and
/// WATCHDOG_PID, which must be us if present).
pub fn watchdog_requested() -> bool {
    let usec = std::env::var("WATCHDOG_USEC").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    let pid_ok = match std::env::var("WATCHDOG_PID").ok().and_then(|v| v.parse::<u32>().ok()) {
        Some(p) => p == std::process::id(),
        None => true,
    };
    usec > 0 && pid_ok && std::env::var_os("NOTIFY_SOCKET").is_some()
}

/// `RELOADING=1` must carry the monotonic timestamp for `Type=notify-reload`.
pub fn reloading() {
    let usec = crate::sys::monotonic_usec();
    notify(&format!("RELOADING=1\nMONOTONIC_USEC={usec}"));
}

/// The systemd unit running us (`warden@api.service`), from our cgroup.
/// Only when systemd started *this* process as the unit's main process:
/// `INVOCATION_ID` alone is inherited by everything a service starts (a CI
/// runner, a shell in some service), and taking that service for our own
/// would make `warden kill` try to stop it.
///
/// Decided once, the first time it is asked (the supervisor asks at start):
/// later our parent may have exited and pid 1 adopted us, which must not
/// make us look started by systemd.
pub fn own_unit() -> Option<String> {
    static UNIT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    UNIT.get_or_init(detect_own_unit).clone()
}

fn detect_own_unit() -> Option<String> {
    std::env::var_os("INVOCATION_ID")?;
    let exec_pid = std::env::var("SYSTEMD_EXEC_PID").ok();
    if !started_by_systemd(exec_pid.as_deref(), std::process::id(), parent_is_systemd) {
        return None;
    }
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    unit_from_cgroup(&cg)
}

/// systemd 248+ sets `SYSTEMD_EXEC_PID` to the pid it executed: only that
/// process is the unit's own. Older systemd doesn't: then our parent must be
/// a systemd manager (pid 1, or a `systemd --user` instance).
fn started_by_systemd(exec_pid: Option<&str>, me: u32, parent_is_systemd: impl Fn() -> bool) -> bool {
    match exec_pid {
        Some(p) => p.trim().parse::<u32>().ok() == Some(me),
        None => parent_is_systemd(),
    }
}

fn parent_is_systemd() -> bool {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else { return false };
    // The fields after the command name, which is in parentheses and may
    // contain spaces: state, ppid, ...
    let Some(after) = stat.rfind(')').map(|i| &stat[i + 1..]) else { return false };
    let Some(ppid) = after.split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok()) else { return false };
    ppid == 1 || std::fs::read_to_string(format!("/proc/{ppid}/comm")).is_ok_and(|c| c.trim() == "systemd")
}

fn unit_from_cgroup(cg: &str) -> Option<String> {
    cg.lines()
        .filter_map(|l| l.splitn(3, ':').nth(2))
        .flat_map(|path| path.split('/'))
        .rfind(|c| c.ends_with(".service"))
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unit_from_cgroup() {
        let cg = "0::/system.slice/system-warden.slice/warden@api.service\n";
        assert_eq!(super::unit_from_cgroup(cg).as_deref(), Some("warden@api.service"));
        assert_eq!(super::unit_from_cgroup("0::/user.slice/user-1000.slice/session-3.scope\n"), None);
    }

    #[test]
    fn only_the_units_main_process_owns_the_unit() {
        use super::started_by_systemd;
        // systemd 248+: SYSTEMD_EXEC_PID names the process it started.
        assert!(started_by_systemd(Some("42"), 42, || false));
        assert!(!started_by_systemd(Some("41"), 42, || true), "a child of the unit's process is not the unit");
        assert!(!started_by_systemd(Some("junk"), 42, || true));
        // Older systemd: the parent decides.
        assert!(started_by_systemd(None, 42, || true));
        assert!(!started_by_systemd(None, 42, || false));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_process_started_by_a_test_runner_is_not_systemds() {
        // Whatever this test runs under, its parent is cargo or a shell.
        assert!(!super::parent_is_systemd() || std::os::unix::process::parent_id() == 1);
    }
}

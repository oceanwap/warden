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
/// Only when systemd started us (`INVOCATION_ID` is set), so a shell inside
/// some user service is not mistaken for a unit.
pub fn own_unit() -> Option<String> {
    std::env::var_os("INVOCATION_ID")?;
    let cg = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    unit_from_cgroup(&cg)
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
}

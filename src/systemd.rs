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

/// `RELOADING=1` must carry the monotonic timestamp for `Type=notify-reload`.
pub fn reloading() {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    let usec = ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000;
    notify(&format!("RELOADING=1\nMONOTONIC_USEC={usec}"));
}

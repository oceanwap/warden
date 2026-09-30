//! Every `unsafe` call in Warden lives here, behind a safe function that
//! states why it is sound, and every function has tests below: error paths,
//! descriptor leaks, descriptor flags, partial writes, and invariants over
//! many inputs.
//!
//! Policy: `unsafe` only for system calls the standard library does not
//! expose (SO_REUSEPORT before bind, sendfile, process groups, pdeathsig),
//! or where it measurably pays (sendfile: no userspace copy). No unsafe
//! memory tricks (`transmute`, unchecked indexing, `from_utf8_unchecked`):
//! Warden is not on the request path, so they would buy nothing.
//!
//! Tools: Miri cannot execute these FFI calls and the sandbox has no nightly
//! toolchain for sanitizers, so the test binary is also run under Valgrind
//! memcheck (`cargo test --bin warden sys::` then valgrind on the binary; see
//! docs/review-process.md).

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

/// Effective user id.
pub fn euid() -> u32 {
    // SAFETY: geteuid takes no arguments and cannot fail.
    unsafe { libc::geteuid() }
}

/// Real user id.
pub fn uid() -> u32 {
    // SAFETY: getuid takes no arguments and cannot fail.
    unsafe { libc::getuid() }
}

pub fn is_root() -> bool {
    euid() == 0
}

/// Is this descriptor a terminal? (false for pipes, files, closed fds)
pub fn isatty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the descriptor number; an invalid one
    // returns 0 with EBADF.
    unsafe { libc::isatty(fd) == 1 }
}

/// Memory page size in bytes (cached).
pub fn page_size() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    // SAFETY: sysconf has no preconditions; -1 (unknown) falls back to 4096.
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    })
}

/// Clock ticks per second for /proc CPU times (cached).
pub fn clock_ticks() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    // SAFETY: sysconf has no preconditions; -1 (unknown) falls back to 100.
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        _ => 100,
    })
}

/// CLOCK_MONOTONIC in microseconds (what systemd's `MONOTONIC_USEC` wants).
pub fn monotonic_usec() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid, exclusively borrowed timespec;
    // CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

/// kill(2), refusing the two targets that would hit far more than one
/// process: pid 0 (our own process group, i.e. Warden itself) and pid -1
/// (every process we may signal). Positive = a process, negative = a group.
pub fn kill(pid: i32, sig: i32) -> io::Result<()> {
    if pid == 0 || pid == -1 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("refusing kill({pid}, {sig})")));
    }
    // SAFETY: plain kill(2); pid/sig are integers, no memory is involved.
    check(unsafe { libc::kill(pid, sig) }).map(|_| ())
}

/// Signal a child's whole process group (when `group`), else just the
/// process; falls back to the process if the group is gone.
pub fn signal_child(pid: u32, sig: i32, group: bool) {
    let Ok(p) = i32::try_from(pid) else { return };
    if p <= 1 {
        return;
    }
    if !(group && kill(-p, sig).is_ok()) {
        let _ = kill(p, sig);
    }
}

/// A pipe whose ends are close-on-exec (only the fd we dup2 into a child
/// survives exec).
pub fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid array of two ints for pipe2 to fill.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: pipe2 just created these descriptors; nothing else owns them,
    // so wrapping each in exactly one OwnedFd is sound.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Broken-down local time for `secs` since the epoch.
pub fn localtime(secs: i64) -> Option<libc::tm> {
    let t: libc::time_t = secs as libc::time_t;
    // SAFETY: all-zero is a valid `tm` (integers and a null tm_zone
    // pointer); localtime_r reads `t` and writes only into `tm`, both owned
    // here, and is thread-safe (unlike localtime).
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() { None } else { Some(tm) }
    }
}

/// write(2) to a descriptor number we were handed (Warden's IPC pipe in a
/// worker). A bad or closed fd is an error, never undefined behaviour.
pub fn write_fd(fd: RawFd, buf: &[u8]) -> io::Result<usize> {
    // SAFETY: `buf` is a valid slice for its length; write(2) only reads it.
    let n = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// A listening TCP socket, non-blocking and close-on-exec, with
/// SO_REUSEADDR and optionally SO_REUSEPORT set before bind (std can't).
pub fn listen_tcp(addr: SocketAddr, reuse_port: bool, backlog: i32) -> io::Result<std::net::TcpListener> {
    let family = if addr.is_ipv4() { libc::AF_INET } else { libc::AF_INET6 };
    // SAFETY: socket(2) with constant arguments.
    let raw = check(unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) })?;
    // SAFETY: socket(2) just returned `raw`; the OwnedFd is its only owner,
    // so every early return below closes it (no leak on error paths).
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let on: libc::c_int = 1;
    let setopt = |opt: libc::c_int| {
        // SAFETY: `on` is a live c_int and the length matches it.
        check(unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                opt,
                &on as *const libc::c_int as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        })
    };
    setopt(libc::SO_REUSEADDR)?;
    if reuse_port {
        setopt(libc::SO_REUSEPORT)?;
    }
    match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
                sin_zero: [0; 8],
            };
            // SAFETY: `sin` is a fully initialised sockaddr_in and the
            // length passed is exactly its size.
            check(unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    &sin as *const libc::sockaddr_in as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            })?;
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr { s6_addr: a.ip().octets() },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: as above, for sockaddr_in6.
            check(unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    &sin6 as *const libc::sockaddr_in6 as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            })?;
        }
    }
    // SAFETY: listen(2) on a descriptor we own.
    check(unsafe { libc::listen(fd.as_raw_fd(), backlog) })?;
    Ok(std::net::TcpListener::from(fd))
}

/// One sendfile(2) call: up to `count` bytes of `input` from `*offset` to
/// `out`; advances `*offset`. The caller loops and waits on EAGAIN.
pub fn sendfile(out: BorrowedFd<'_>, input: BorrowedFd<'_>, offset: &mut i64, count: usize) -> io::Result<usize> {
    let mut off: libc::off_t = *offset as libc::off_t;
    // SAFETY: both descriptors are borrowed, so they stay open (and can't be
    // reused for something else) for the whole call; `off` is a valid,
    // exclusively borrowed off_t. sendfile reads the file and writes the
    // socket; it touches no Rust memory besides `off`.
    let n = unsafe { libc::sendfile(out.as_raw_fd(), input.as_raw_fd(), &mut off, count) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    *offset = off as i64;
    Ok(n as usize)
}

// ------------------------------------------------ between fork and exec
//
// These run in the child after fork(2) and before exec: only
// async-signal-safe calls, no allocation, no locks (see `Command::pre_exec`).

/// Put Warden's IPC pipe at `target` in the child (fd 3), inherited by exec.
pub fn child_dup_ipc(write_fd: RawFd, target: RawFd) -> io::Result<()> {
    // SAFETY: dup2/fcntl on descriptors that exist in the child; both are
    // async-signal-safe.
    unsafe {
        check(libc::dup2(write_fd, target))?;
        if write_fd == target {
            // dup2 onto itself keeps FD_CLOEXEC; clear it so the fd survives exec.
            check(libc::fcntl(target, libc::F_SETFD, 0))?;
        }
    }
    Ok(())
}

/// The child gets `sig` when its parent thread (Warden) dies.
pub fn child_parent_death_signal(sig: i32) -> io::Result<()> {
    // SAFETY: prctl(PR_SET_PDEATHSIG) takes the signal as an integer.
    check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, sig as libc::c_ulong) }).map(|_| ())
}

/// New session: no controlling terminal, own process group.
pub fn child_new_session() -> io::Result<()> {
    // SAFETY: setsid takes no arguments; async-signal-safe.
    check(unsafe { libc::setsid() }).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn open_fds() -> usize {
        std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
    }

    /// Descriptor counting only means something when nothing else in the
    /// process opens files meanwhile: run `probe` (a test in this module)
    /// alone, single-threaded, in a fresh copy of the test binary.
    fn run_isolated(probe: &str) {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([&format!("sys::tests::{probe}"), "--exact", "--test-threads=1", "--nocapture"])
            .env("WARDEN_FD_PROBE", "1")
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && text.contains("1 passed"), "{probe} failed in isolation:\n{text}");
    }

    fn in_probe() -> bool {
        std::env::var_os("WARDEN_FD_PROBE").is_some()
    }

    fn fd_flags(fd: RawFd) -> (bool, bool) {
        // SAFETY (test): fcntl queries on a descriptor we own.
        let fdfl = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        let fl = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        (fdfl & libc::FD_CLOEXEC != 0, fl & libc::O_NONBLOCK != 0)
    }

    #[test]
    fn ids_match_proc() {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let uids: Vec<u32> = status
            .lines()
            .find(|l| l.starts_with("Uid:"))
            .unwrap()
            .split_whitespace()
            .skip(1)
            .map(|x| x.parse().unwrap())
            .collect();
        assert_eq!((uid(), euid()), (uids[0], uids[1]));
        assert_eq!(is_root(), uids[1] == 0);
    }

    #[test]
    fn isatty_is_false_for_non_terminals() {
        let (r, w) = pipe_cloexec().unwrap();
        assert!(!isatty(r.as_raw_fd()) && !isatty(w.as_raw_fd()));
        let f = std::fs::File::open("/dev/null").unwrap();
        assert!(!isatty(f.as_raw_fd()));
        assert!(!isatty(-1) && !isatty(987_654));
    }

    #[test]
    fn sysconf_values_are_sane() {
        assert!(page_size() >= 4096 && page_size().is_power_of_two());
        assert!((1..=10_000).contains(&clock_ticks()));
        assert_eq!(page_size(), page_size(), "cached");
    }

    #[test]
    fn monotonic_clock_moves_with_instant() {
        let (a, t) = (monotonic_usec(), std::time::Instant::now());
        std::thread::sleep(std::time::Duration::from_millis(30));
        let (b, el) = (monotonic_usec(), t.elapsed().as_micros() as u64);
        assert!(b > a);
        assert!((b - a).abs_diff(el) < 5_000, "{} vs {el}", b - a);
    }

    #[test]
    fn kill_refuses_whole_group_and_everyone() {
        assert_eq!(kill(0, 0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(kill(-1, 0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        kill(std::process::id() as i32, 0).unwrap();
        // A pid that can't exist (above pid_max).
        assert_eq!(kill(i32::MAX, 0).unwrap_err().raw_os_error(), Some(libc::ESRCH));
    }

    #[test]
    fn signal_child_reaches_the_group() {
        use std::os::unix::process::CommandExt;
        // sh starts a grandchild sleep in the same group; the group signal
        // must end both.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(std::process::Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let mut line = String::new();
        let mut out = child.stdout.take().unwrap();
        let mut b = [0u8; 1];
        while out.read(&mut b).unwrap() == 1 && b[0] != b'\n' {
            line.push(b[0] as char);
        }
        let grandchild: i32 = line.trim().parse().unwrap();
        signal_child(child.id(), libc::SIGKILL, true);
        child.wait().unwrap();
        // Gone, or a zombie waiting for init to reap it: either way it was killed.
        let dead =
            || std::fs::read_to_string(format!("/proc/{grandchild}/stat")).map(|s| s.contains(") Z ")).unwrap_or(true);
        let t0 = std::time::Instant::now();
        while !dead() && t0.elapsed() < std::time::Duration::from_secs(2) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let gone = dead();
        assert!(gone, "grandchild survived the group signal");
        // Nonsense pids are ignored, not sent to process 0/1.
        signal_child(0, libc::SIGTERM, true);
        signal_child(1, libc::SIGTERM, false);
        signal_child(u32::MAX, libc::SIGTERM, true);
    }

    #[test]
    fn pipes_are_cloexec_and_do_not_leak() {
        run_isolated("pipe_leak_probe");
    }

    #[test]
    fn pipe_leak_probe() {
        if !in_probe() {
            return;
        }
        let before = open_fds();
        for _ in 0..2000 {
            let (r, w) = pipe_cloexec().unwrap();
            assert!(fd_flags(r.as_raw_fd()).0 && fd_flags(w.as_raw_fd()).0);
        }
        assert_eq!(open_fds(), before, "every pipe closed on drop");
        let (r, w) = pipe_cloexec().unwrap();
        let mut w = std::fs::File::from(w);
        let mut r = std::fs::File::from(r);
        w.write_all(b"ping").unwrap();
        drop(w);
        let mut s = String::new();
        r.read_to_string(&mut s).unwrap();
        assert_eq!(s, "ping");
    }

    #[test]
    fn localtime_agrees_with_utc_plus_offset() {
        // For any t: the local fields equal the UTC civil date of t + gmtoff.
        let mut t: i64 = 0;
        for i in 0..10_000i64 {
            t = (t + 7_919 * 3_600 + i * 97) % 4_102_444_800; // up to 2100, spread out
            let tm = localtime(t).unwrap();
            let shifted = t + tm.tm_gmtoff;
            let (y, m, d) = crate::logging::civil_from_days(shifted.div_euclid(86_400));
            let secs = shifted.rem_euclid(86_400);
            assert_eq!(
                (tm.tm_year as i64 + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32),
                (y, m, d),
                "date for t={t}"
            );
            assert_eq!(
                (tm.tm_hour as i64, tm.tm_min as i64, tm.tm_sec as i64),
                (secs / 3600, (secs % 3600) / 60, secs % 60),
                "time for t={t}"
            );
            assert_eq!(tm.tm_wday as i64, (shifted.div_euclid(86_400) + 4).rem_euclid(7), "weekday for t={t}");
        }
    }

    #[test]
    fn write_fd_errors_instead_of_misbehaving() {
        let (r, w) = pipe_cloexec().unwrap();
        assert_eq!(write_fd(w.as_raw_fd(), b"hello\n").unwrap(), 6);
        let mut s = String::new();
        drop(w);
        std::fs::File::from(r).read_to_string(&mut s).unwrap();
        assert_eq!(s, "hello\n");
        assert_eq!(write_fd(-1, b"x").unwrap_err().raw_os_error(), Some(libc::EBADF));
        assert_eq!(write_fd(987_654, b"x").unwrap_err().raw_os_error(), Some(libc::EBADF));
        assert_eq!(write_fd(987_654, b"").unwrap_err().raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn listeners_share_ports_only_with_reuseport() {
        let a = listen_tcp("127.0.0.1:0".parse().unwrap(), true, 128).unwrap();
        let port = a.local_addr().unwrap().port();
        assert!(port > 0);
        assert_eq!(fd_flags(a.as_raw_fd()), (true, true), "CLOEXEC and NONBLOCK");
        let b = listen_tcp(format!("127.0.0.1:{port}").parse().unwrap(), true, 128).unwrap();
        assert_eq!(b.local_addr().unwrap().port(), port, "second SO_REUSEPORT listener joins the group");
        let e = listen_tcp(format!("127.0.0.1:{port}").parse().unwrap(), false, 128).unwrap_err();
        assert_eq!(e.raw_os_error(), Some(libc::EADDRINUSE));
        // Connections reach the group.
        let c = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        drop(c);
        drop((a, b));
    }

    #[test]
    fn ipv6_addresses_round_trip() {
        match listen_tcp("[::1]:0".parse().unwrap(), true, 16) {
            Ok(l) => {
                let got = l.local_addr().unwrap();
                assert!(got.is_ipv6() && got.ip().is_loopback() && got.port() > 0, "{got}");
            }
            // No IPv6 in this environment: the error must be a clean one.
            Err(e) => assert!(matches!(e.raw_os_error(), Some(libc::EAFNOSUPPORT) | Some(libc::EADDRNOTAVAIL)), "{e}"),
        }
    }

    #[test]
    fn failed_binds_do_not_leak_descriptors() {
        run_isolated("bind_leak_probe");
    }

    #[test]
    fn bind_leak_probe() {
        if !in_probe() {
            return;
        }
        let taken = listen_tcp("127.0.0.1:0".parse().unwrap(), false, 16).unwrap();
        let addr = taken.local_addr().unwrap();
        let before = open_fds();
        for _ in 0..500 {
            assert!(listen_tcp(addr, false, 16).is_err());
            // Not an address of this host.
            assert!(listen_tcp("192.0.2.1:0".parse().unwrap(), true, 16).is_err());
        }
        assert_eq!(open_fds(), before);
    }

    fn socket_pair() -> (std::net::TcpStream, std::net::TcpStream) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (s, c)
    }

    fn temp_file(bytes: &[u8]) -> (std::fs::File, std::path::PathBuf) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "warden-sys-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&p, bytes).unwrap();
        (std::fs::File::open(&p).unwrap(), p)
    }

    /// Send all of `[off, off+count)` with blocking sockets.
    fn send_range(sock: &std::net::TcpStream, file: &std::fs::File, mut off: i64, count: usize) -> io::Result<usize> {
        use std::os::fd::AsFd;
        let mut left = count;
        while left > 0 {
            let n = sendfile(sock.as_fd(), file.as_fd(), &mut off, left)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            left -= n;
        }
        Ok(count)
    }

    #[test]
    fn sendfile_sends_exact_bytes_for_many_sizes_and_offsets() {
        let data: Vec<u8> = (0..(5 << 20) as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let (file, path) = temp_file(&data);
        for (off, len) in
            [(0, 0), (0, 1), (1, 4095), (0, 65_536), (12_345, 65_537), (0, data.len()), (4_000_000, 1_000_000)]
        {
            let (tx, mut rx) = socket_pair();
            let reader = std::thread::spawn(move || {
                let mut got = Vec::new();
                rx.read_to_end(&mut got).unwrap();
                got
            });
            let sent = send_range(&tx, &file, off as i64, len).unwrap();
            drop(tx);
            let got = reader.join().unwrap();
            assert_eq!(sent, len);
            assert!(got == data[off..off + len], "bytes differ for offset {off}, length {len}");
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sendfile_on_a_slow_reader_hits_eagain_and_resumes() {
        use std::os::fd::AsFd;
        let data: Vec<u8> = (0..(1 << 20) as u32).map(|i| i as u8).collect();
        let (file, path) = temp_file(&data);
        let (tx, mut rx) = socket_pair();
        tx.set_nonblocking(true).unwrap();
        // Small buffers so the kernel really runs out of room.
        let small: libc::c_int = 32 * 1024;
        tx.set_nodelay(true).unwrap();
        for (fd, opt) in [(tx.as_raw_fd(), libc::SO_SNDBUF), (rx.as_raw_fd(), libc::SO_RCVBUF)] {
            // SAFETY (test): setsockopt with a live c_int and its size.
            unsafe {
                libc::setsockopt(fd, libc::SOL_SOCKET, opt, &small as *const _ as *const libc::c_void, 4);
            }
        }
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match rx.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                    Err(e) => panic!("{e}"),
                }
            }
            got
        });
        let (mut off, mut would_block) = (0i64, 0);
        while (off as usize) < data.len() {
            let left = data.len() - off as usize;
            match sendfile(tx.as_fd(), file.as_fd(), &mut off, left) {
                Ok(0) => panic!("unexpected EOF"),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    would_block += 1;
                    std::thread::yield_now();
                }
                Err(e) => panic!("{e}"),
            }
        }
        drop(tx);
        assert!(reader.join().unwrap() == data, "all bytes, in order, across partial sends");
        assert!(would_block > 0, "the slow reader should have filled the socket buffer");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sendfile_reports_truncation_and_peer_resets() {
        use std::os::fd::AsFd;
        // A file shorter than promised: sendfile returns 0 at EOF.
        let (file, path) = temp_file(&[7u8; 1000]);
        let (tx, _rx) = socket_pair();
        let e = send_range(&tx, &file, 0, 5000).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof);
        // Peer gone: an error (EPIPE/ECONNRESET), never a crash (Rust
        // ignores SIGPIPE, so the process survives).
        let big = vec![1u8; 8 << 20];
        let (file2, path2) = temp_file(&big);
        let (tx, rx) = socket_pair();
        drop(rx);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut off = 0i64;
        let mut err = None;
        for _ in 0..1000 {
            match sendfile(tx.as_fd(), file2.as_fd(), &mut off, big.len()) {
                Ok(_) => continue,
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        let e = err.expect("writing to a closed peer must fail");
        assert!(matches!(e.raw_os_error(), Some(libc::EPIPE) | Some(libc::ECONNRESET)), "unexpected error {e}");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path2);
    }

    #[test]
    fn child_helpers_work_between_fork_and_exec() {
        use std::os::unix::process::CommandExt;
        // setsid: the child leads its own session; the IPC fd lands on fd 3
        // and it is the only extra descriptor the child has.
        let (r, w) = pipe_cloexec().unwrap();
        let wfd = w.as_raw_fd();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo ok >&3; ls /proc/self/fd; cat /proc/self/stat"]).stdout(std::process::Stdio::piped());
        // SAFETY (test): the closure only calls the async-signal-safe helpers.
        unsafe {
            cmd.pre_exec(move || {
                child_dup_ipc(wfd, 3)?;
                child_parent_death_signal(libc::SIGTERM)?;
                child_new_session()
            });
        }
        let out = cmd.output().unwrap();
        drop(w);
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let mut lines = text.lines();
        let mut fds: Vec<u32> = Vec::new();
        for l in lines.by_ref() {
            match l.parse() {
                Ok(n) => fds.push(n),
                Err(_) => {
                    // /proc/self/stat of `cat`: "pid (cat) S ppid pgrp session …"
                    let after = l.rsplit(')').next().unwrap();
                    let f: Vec<&str> = after.split_whitespace().collect();
                    let (pgrp, session): (i64, i64) = (f[2].parse().unwrap(), f[3].parse().unwrap());
                    assert_eq!(pgrp, session, "setsid: own session and group");
                    break;
                }
            }
        }
        // 0, 1, 2, the IPC fd 3, and the fd `ls` opened for /proc/self/fd.
        assert!(fds.iter().all(|f| *f <= 4), "unexpected inherited descriptors: {fds:?}");
        assert!(fds.contains(&3), "{fds:?}");
        let mut msg = String::new();
        std::fs::File::from(r).read_to_string(&mut msg).unwrap();
        assert_eq!(msg, "ok\n");
    }
}

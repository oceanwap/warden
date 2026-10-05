//! Every `unsafe` call in Warden lives here, behind a safe function that
//! states why it is sound, and every function has tests below: error paths,
//! descriptor leaks, descriptor flags, partial writes, and invariants over
//! many inputs.
//!
//! Policy: `unsafe` only for system calls the standard library does not
//! expose (SO_REUSEPORT before bind, process groups, pdeathsig, openat2) or
//! where it measurably pays on a hot path: the static file server
//! (sendfile, openat2 instead of a realpath walk, cache-only preadv2,
//! MSG_MORE, TCP_DEFER_ACCEPT) and worker log capture (vectorised memchr).
//! Each is a plain system or C library call on borrowed descriptors and
//! slices. Tried and dropped for lack of a measured win: bigger worker pipe
//! buffers (F_SETPIPE_SZ: same throughput at 64 KB, 256 KB and 1 MB), and
//! an io_uring transport for the static server (slower than epoll with the
//! response cache; docs/benchmarks.md).
//! No unsafe memory tricks (`transmute`, unchecked indexing,
//! `from_utf8_unchecked`): they would buy nanoseconds against microsecond
//! syscalls, and a mistake there is silent corruption.
//!
//! Tools: Miri cannot execute these FFI calls and the sandbox has no nightly
//! toolchain for sanitizers, so the test binary is also run under Valgrind
//! memcheck: its `sys::` tests, steps and flags in docs/review-process.md
//! ("Valgrind for the `unsafe` code").

#[cfg(target_os = "macos")]
pub mod darwin;

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::sync::OnceLock;

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc) }
}

/// Not Linux: FD_CLOEXEC (and O_NONBLOCK with `also_nonblock`) set after the
/// descriptor exists, where Linux passes O_CLOEXEC / SOCK_NONBLOCK at
/// creation. A fork on another thread in between inherits the descriptor
/// until its exec (the standard library's own pipes and sockets have the
/// same window on macOS).
#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: BorrowedFd<'_>, also_nonblock: bool) -> io::Result<()> {
    // SAFETY: fcntl on a borrowed descriptor, which stays open for the call,
    // with integer arguments only; no memory is passed.
    unsafe {
        check(libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC))?;
        if also_nonblock {
            let fl = check(libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))?;
            check(libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK))?;
        }
    }
    Ok(())
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

/// This process gives way to everything else on the machine: on Linux the
/// idle scheduling policy (it runs only when a CPU has nothing else to do; nice
/// 19 where that is refused) and the idle disk class, on macOS nice 19 and the
/// background quality-of-service class. For work that can wait, like
/// compression; meant for a process (or thread) that does only that.
pub fn lower_priority() {
    // SAFETY: setpriority takes integers; PRIO_PROCESS with id 0 is the
    // calling thread (Linux) or process.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
    #[cfg(target_os = "linux")]
    {
        let param = libc::sched_param { sched_priority: 0 };
        // SAFETY: `param` is a live sched_param, which the call only reads.
        unsafe {
            libc::sched_setscheduler(0, libc::SCHED_IDLE, &param);
        }
        // ioprio_set(IOPRIO_WHO_PROCESS, 0, IOPRIO_CLASS_IDLE << 13): no
        // pointer arguments. Only some I/O schedulers act on it; a failure is
        // no matter.
        // SAFETY: a system call that takes three integers.
        unsafe {
            libc::syscall(libc::SYS_ioprio_set, 1 as libc::c_long, 0 as libc::c_long, (3 << 13) as libc::c_long);
        }
    }
    #[cfg(target_os = "macos")]
    {
        // SAFETY: sets the QoS class of the calling thread; no memory involved.
        unsafe {
            libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_BACKGROUND, 0);
        }
    }
}

/// Close every descriptor above stderr: for a process Warden started that
/// must not hold what its parent had open (the IPC channel to the supervisor
/// is not close-on-exec, and a child that kept it would keep the supervisor
/// from seeing the worker end). Call before anything is opened.
pub fn close_inherited_fds() {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: close_range(2) takes integers; the range is those no one here holds yet.
        if unsafe { libc::syscall(libc::SYS_close_range, 3 as libc::c_uint, libc::c_uint::MAX, 0 as libc::c_uint) } == 0
        {
            return;
        }
    }
    let top = nofile_limit().0.min(65_536) as libc::c_int;
    for fd in 3..top {
        // SAFETY: closing a descriptor number nothing in this process owns.
        unsafe {
            libc::close(fd);
        }
    }
}

/// (soft, hard) limit on open files for this process.
pub fn nofile_limit() -> (u64, u64) {
    let mut r = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `r` is a live, exclusively borrowed rlimit for getrlimit to
    // fill; RLIMIT_NOFILE always exists.
    if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut r) } != 0 {
        return (0, 0);
    }
    #[allow(clippy::unnecessary_cast)] // rlim_t is not u64 on every target
    (r.rlim_cur as u64, r.rlim_max as u64)
}

/// Is this descriptor a terminal? (false for pipes, files, closed fds)
pub fn isatty(fd: RawFd) -> bool {
    // SAFETY: isatty only inspects the descriptor number; an invalid one
    // returns 0 with EBADF.
    unsafe { libc::isatty(fd) == 1 }
}

/// Try to take an exclusive advisory lock on an open file without waiting:
/// `Ok(true)` taken (held until the file is closed), `Ok(false)` another
/// process holds it. Used for the small CLI state files (`ids.rs`).
pub fn try_lock_exclusive(file: &std::fs::File) -> io::Result<bool> {
    // SAFETY: flock only takes the descriptor number of a file that is open
    // for the whole call (borrowed here) and a flag word; it touches no memory.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        return Ok(true);
    }
    let e = io::Error::last_os_error();
    if e.raw_os_error() == Some(libc::EWOULDBLOCK) { Ok(false) } else { Err(e) }
}

/// Width in columns of the terminal on this descriptor; `None` for anything
/// that is not a terminal (a pipe, a file) or reports no width.
pub fn terminal_width(fd: RawFd) -> Option<usize> {
    let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
    // SAFETY: TIOCGWINSZ only writes a `winsize` through the pointer, which
    // is valid and exclusively borrowed for the call; a descriptor that is not
    // a terminal makes it fail with ENOTTY and write nothing.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_col > 0).then_some(usize::from(ws.ws_col))
}

/// Memory page size in bytes (cached).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // for /proc metrics
pub fn page_size() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    // SAFETY: sysconf has no preconditions; -1 (unknown) falls back to 4096.
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_PAGESIZE) } {
        n if n > 0 => n as u64,
        _ => 4096,
    })
}

/// Clock ticks per second for /proc CPU times (cached).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // for /proc metrics
pub fn clock_ticks() -> u64 {
    static V: OnceLock<u64> = OnceLock::new();
    // SAFETY: sysconf has no preconditions; -1 (unknown) falls back to 100.
    *V.get_or_init(|| match unsafe { libc::sysconf(libc::_SC_CLK_TCK) } {
        n if n > 0 => n as u64,
        _ => 100,
    })
}

/// This host's name (gethostname), when it has one.
pub fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid byte array of buf.len() bytes, which
    // gethostname writes a NUL-terminated name into (truncated if longer).
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast::<libc::c_char>(), buf.len()) };
    if rc != 0 {
        return None;
    }
    let name = buf.split(|b| *b == 0).next().unwrap_or(&[]);
    let name = String::from_utf8_lossy(name).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The 1, 5 and 15 minute load averages.
#[cfg_attr(target_os = "linux", allow(dead_code))] // Linux reads /proc/loadavg
pub fn loadavg() -> Option<[f64; 3]> {
    let mut v = [0f64; 3];
    // SAFETY: `v` is a valid array of 3 doubles, which getloadavg fills
    // with up to 3 samples (it returns how many it got, -1 on error).
    let n = unsafe { libc::getloadavg(v.as_mut_ptr(), 3) };
    (n == 3).then_some(v)
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
#[cfg(target_os = "linux")]
pub fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid array of two ints for pipe2 to fill.
    check(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: pipe2 just created these descriptors; nothing else owns them,
    // so wrapping each in exactly one OwnedFd is sound.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// A pipe whose ends are close-on-exec. Not Linux: no pipe2, so pipe(2) and
/// then FD_CLOEXEC on each end (see `set_cloexec` for the window).
#[cfg(not(target_os = "linux"))]
pub fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::AsFd;
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid array of two ints for pipe to fill.
    check(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: pipe just created these descriptors; nothing else owns them,
    // so wrapping each in exactly one OwnedFd is sound (and closes both if
    // setting the flag fails below).
    let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(r.as_fd(), false)?;
    set_cloexec(w.as_fd(), false)?;
    Ok((r, w))
}

/// A connected pair of Unix stream sockets, both close-on-exec: Warden's
/// two-way channel with a worker (the worker's end goes to fd 3 with
/// `child_dup_ipc`, the only copy that survives exec). Both ends block;
/// callers make their own end non-blocking.
#[cfg(target_os = "linux")]
pub fn socketpair_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid array of two ints for socketpair to fill.
    check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })?;
    // SAFETY: socketpair just created these descriptors; nothing else owns
    // them, so wrapping each in exactly one OwnedFd is sound.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// A connected pair of Unix stream sockets, both close-on-exec. Not Linux:
/// no SOCK_CLOEXEC, so socketpair(2) and then FD_CLOEXEC on each end (see
/// `set_cloexec` for the window).
#[cfg(not(target_os = "linux"))]
pub fn socketpair_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    use std::os::fd::AsFd;
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` is a valid array of two ints for socketpair to fill.
    check(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) })?;
    // SAFETY: socketpair just created these descriptors; nothing else owns
    // them, so wrapping each in exactly one OwnedFd is sound (and closes
    // both if setting the flag fails below).
    let (a, b) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    set_cloexec(a.as_fd(), false)?;
    set_cloexec(b.as_fd(), false)?;
    Ok((a, b))
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

/// The login name of a user id (getpwuid_r), or `None` when there is no such
/// user (or the lookup fails: a user database that is down).
pub fn user_name(uid: u32) -> Option<String> {
    // The buffer holds the strings the passwd entry points to; grow it when
    // getpwuid_r says it is too small (ERANGE), within a bound.
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: all-zero is a valid `passwd` (integers and null pointers);
        // getpwuid_r writes only into `pwd` and `buf`, both owned here, up
        // to buf.len() bytes, and is thread-safe (unlike getpwuid). On
        // success (`found` non-null) `pw_name` points to a NUL-terminated
        // string inside `buf`, which outlives the read below.
        let (rc, found, name) = unsafe {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut found: *mut libc::passwd = std::ptr::null_mut();
            let rc = libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr().cast::<libc::c_char>(), buf.len(), &mut found);
            let name = if rc == 0 && !found.is_null() && !pwd.pw_name.is_null() {
                Some(std::ffi::CStr::from_ptr(pwd.pw_name).to_string_lossy().into_owned())
            } else {
                None
            };
            (rc, found, name)
        };
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            let bigger = buf.len() * 4;
            buf.resize(bigger, 0);
            continue;
        }
        let _ = found;
        return name.filter(|n| !n.is_empty());
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
    #[cfg(target_os = "linux")]
    let raw = check(unsafe { libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK, 0) })?;
    // SAFETY: socket(2) with constant arguments (not Linux: no SOCK_CLOEXEC
    // or SOCK_NONBLOCK; both are set right below).
    #[cfg(not(target_os = "linux"))]
    let raw = check(unsafe { libc::socket(family, libc::SOCK_STREAM, 0) })?;
    // SAFETY: socket(2) just returned `raw`; the OwnedFd is its only owner,
    // so every early return below closes it (no leak on error paths).
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    use std::os::fd::AsFd;
    #[cfg(not(target_os = "linux"))]
    set_cloexec(fd.as_fd(), true)?;
    setsockopt_int(fd.as_fd(), libc::SOL_SOCKET, libc::SO_REUSEADDR, 1)?;
    if reuse_port {
        setsockopt_int(fd.as_fd(), libc::SOL_SOCKET, libc::SO_REUSEPORT, 1)?;
    }
    match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                #[cfg(not(target_os = "linux"))]
                sin_len: std::mem::size_of::<libc::sockaddr_in>() as u8,
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
                #[cfg(not(target_os = "linux"))]
                sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
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
#[cfg(target_os = "linux")]
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

/// One sendfile(2) call, as on Linux: up to `count` bytes of `input` from
/// `*offset` to the socket `out`; advances `*offset`; Ok(0) at end of file.
/// macOS's sendfile takes (file, socket, offset, &len) and reports bytes sent
/// in `len`, also when it fails with EAGAIN or EINTR after a partial send:
/// that progress is returned as Ok(n), and the next call says WouldBlock.
#[cfg(not(target_os = "linux"))]
pub fn sendfile(out: BorrowedFd<'_>, input: BorrowedFd<'_>, offset: &mut i64, count: usize) -> io::Result<usize> {
    if count == 0 {
        // A length of 0 means "to the end of the file" to macOS.
        return Ok(0);
    }
    let mut len: libc::off_t = count.min(i64::MAX as usize) as libc::off_t;
    // SAFETY: both descriptors are borrowed, so they stay open for the whole
    // call; `len` is a valid, exclusively borrowed off_t; a null header /
    // trailer list is allowed. sendfile touches no Rust memory besides `len`.
    let rc = unsafe {
        libc::sendfile(input.as_raw_fd(), out.as_raw_fd(), *offset as libc::off_t, &mut len, std::ptr::null_mut(), 0)
    };
    if rc < 0 {
        let e = io::Error::last_os_error();
        if len <= 0 || !matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
            return Err(e);
        }
    }
    *offset += len as i64;
    Ok(len as usize)
}

/// How a total reported by `sendfile_head` splits: the bytes of the head
/// first, then the file's. `None` when `total` is more than was asked for
/// (a kernel or caller bug: the caller fails the connection rather than
/// send a wrong body).
pub fn split_head_body(total: usize, head_left: usize, body_left: u64) -> Option<(usize, u64)> {
    let head = total.min(head_left);
    let body = (total - head) as u64;
    (body <= body_left).then_some((head, body))
}

/// One system call that sends `head` and then up to `count` bytes of
/// `input` from `*offset` to the socket `out`: the whole response head and
/// the start of the body in one go. Returns the bytes sent in all, head
/// first (`split_head_body` splits it); advances `*offset` by the file
/// bytes. A partial send is normal (a full socket buffer): the caller
/// waits for writability and calls again with the rest of the head (empty
/// once it is out). WouldBlock only when nothing at all could be sent;
/// Ok(0) is an end of file before `count` (the file shrank) when there
/// was nothing of the head left to send. `count == 0` sends just the head.
///
/// Linux: `send` with MSG_MORE (the head waits for the body and shares its
/// first packet), then sendfile: two calls, the same as before there was
/// one function for both systems. Not Linux (macOS): one sendfile(2) with
/// an `sf_hdtr` header, a single call; its length argument takes the file
/// bytes wanted and gives back everything sent, header included.
#[cfg(target_os = "linux")]
pub fn sendfile_head(
    out: BorrowedFd<'_>,
    input: BorrowedFd<'_>,
    offset: &mut i64,
    count: usize,
    head: &[u8],
) -> io::Result<usize> {
    let mut sent = 0;
    if !head.is_empty() {
        sent = send(out, head, count > 0)?;
        if sent < head.len() || count == 0 {
            return Ok(sent);
        }
    } else if count == 0 {
        return Ok(0);
    }
    after_head(sent, sendfile(out, input, offset, count))
}

/// What `sendfile_head` returns once `sent` bytes of the head are out and
/// `body` is what its sendfile call said. A body that could not start yet
/// (the socket is full: WouldBlock; a signal: Interrupted) is not an error
/// then: the head is progress and the caller goes on from there. Returning
/// the error instead would make it send the head again, since it only learns
/// what went out from an Ok. With nothing sent (an empty head, or a head
/// that is not out) both errors stay errors: the caller retries with
/// everything still to send.
#[cfg(target_os = "linux")]
fn after_head(sent: usize, body: io::Result<usize>) -> io::Result<usize> {
    match body {
        Ok(n) => Ok(sent + n),
        Err(e) if sent > 0 && matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => Ok(sent),
        Err(e) => Err(e),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn sendfile_head(
    out: BorrowedFd<'_>,
    input: BorrowedFd<'_>,
    offset: &mut i64,
    count: usize,
    head: &[u8],
) -> io::Result<usize> {
    if head.is_empty() {
        return sendfile(out, input, offset, count);
    }
    if count == 0 {
        // A length of 0 means "to the end of the file" to macOS: send the head alone.
        return send(out, head, false);
    }
    let mut iov = libc::iovec { iov_base: head.as_ptr() as *mut libc::c_void, iov_len: head.len() };
    let mut hdtr = libc::sf_hdtr { headers: &mut iov, hdr_cnt: 1, trailers: std::ptr::null_mut(), trl_cnt: 0 };
    let mut len: libc::off_t = count.min(i64::MAX as usize) as libc::off_t;
    // SAFETY: both descriptors are borrowed, so they stay open for the whole
    // call; `iov` points at `head`, which outlives the call, and the kernel
    // only reads it; `hdtr` and `len` are live, exclusively borrowed values
    // on this stack frame, with one header iovec and no trailer. sendfile
    // touches no other Rust memory.
    let rc =
        unsafe { libc::sendfile(input.as_raw_fd(), out.as_raw_fd(), *offset as libc::off_t, &mut len, &mut hdtr, 0) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        // EAGAIN / EINTR after a partial send: `len` says how far it got.
        if len <= 0 || !matches!(e.raw_os_error(), Some(libc::EAGAIN | libc::EINTR)) {
            return Err(e);
        }
    }
    // ASSUMPTION, not checked on a Mac from here (this is written on Linux):
    // with an `sf_hdtr` header, the `len` that sendfile(2) hands back counts
    // every byte sent, the header's included. Everything below rests on it:
    // `split_head_body` takes the head's bytes off the front of `total`, the
    // offset moves by what is left, and the caller drops the head once it is
    // counted. If `len` counted only the file's bytes, the head would be
    // sent again (a corrupt body, broken keep-alive framing). What `len`
    // means on the way in does not matter: a short send is just progress
    // and the caller asks for the rest. The check that settles it is
    // `scripts/mac-check.sh`: it compares the bytes `warden serve` sends for
    // a file with the file itself.
    let total = len.max(0) as usize;
    *offset += total.saturating_sub(head.len()) as i64;
    Ok(total)
}

/// openat2 `resolve` flags, named here so callers compile on every platform
/// (outside Linux, `openat2` is Unsupported and never reads them).
#[cfg(target_os = "linux")]
pub use libc::{RESOLVE_BENEATH, RESOLVE_CACHED, RESOLVE_NO_MAGICLINKS};
#[cfg(not(target_os = "linux"))]
pub const RESOLVE_BENEATH: u64 = 0x08;
#[cfg(not(target_os = "linux"))]
pub const RESOLVE_CACHED: u64 = 0x20;
#[cfg(not(target_os = "linux"))]
pub const RESOLVE_NO_MAGICLINKS: u64 = 0x02;

/// The kernel's `struct open_how` (linux/openat2.h, version 0: 24 bytes).
/// Our own definition because libc's is `#[non_exhaustive]`.
#[cfg(target_os = "linux")]
#[repr(C)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// openat2(2): open `path` relative to the directory `dir` with `resolve`
/// restrictions (e.g. RESOLVE_BENEATH: the kernel refuses, with EXDEV, any
/// resolution that leaves `dir`, including through `..` and symlinks; no
/// window between a check and the open). One syscall instead of a
/// userspace realpath walk. ENOSYS on kernels before 5.6, EPERM where a
/// seccomp filter blocks it: callers fall back.
#[cfg(target_os = "linux")]
pub fn openat2(dir: BorrowedFd<'_>, path: &std::ffi::CStr, flags: i32, resolve: u64) -> io::Result<OwnedFd> {
    let how = OpenHow { flags: (flags | libc::O_CLOEXEC) as u32 as u64, mode: 0, resolve };
    // SAFETY: `path` is a NUL-terminated string that outlives the call;
    // `how` is a live repr(C) struct of the size passed (the kernel's v0
    // layout: three u64s); `dir` is borrowed, so it stays open for the call.
    // The syscall touches no other memory.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            dir.as_raw_fd() as libc::c_long,
            path.as_ptr(),
            &how as *const OpenHow,
            std::mem::size_of::<OpenHow>(),
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat2 returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(rc as RawFd) })
}

/// Not Linux: there is no openat2; always `Unsupported`, and callers use
/// their realpath check instead (`warden serve` starts in that mode).
#[cfg(not(target_os = "linux"))]
pub fn openat2(dir: BorrowedFd<'_>, path: &std::ffi::CStr, flags: i32, resolve: u64) -> io::Result<OwnedFd> {
    let _ = (dir, path, flags, resolve);
    Err(io::Error::new(io::ErrorKind::Unsupported, "openat2 is Linux-only"))
}

/// macOS 15 (Darwin 24) and later: open(2) refuses, with ENOTCAPABLE, a
/// lookup that would leave the directory it starts from (`..` above it, an
/// absolute path or an absolute symlink), in the kernel, so nothing can be
/// swapped in between a check and the open. The `libc` crate lacks it; the
/// value is xnu's `bsd/sys/fcntl.h`: `#define O_RESOLVE_BENEATH 0x00001000`.
/// Older kernels ignore the bit (it is FMARK there, kernel-internal, masked
/// off at open), so it is only used once [`resolve_beneath_works`] said so.
#[cfg(target_os = "macos")]
pub const O_RESOLVE_BENEATH: libc::c_int = 0x1000;

/// openat(2) of `path` relative to `dir`, close-on-exec. No O_CREAT: no
/// mode argument.
#[cfg(target_os = "macos")]
pub fn openat(dir: BorrowedFd<'_>, path: &std::ffi::CStr, flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: `path` is NUL-terminated and outlives the call; `dir` is
    // borrowed, so it stays open for it. Without O_CREAT the variadic mode
    // argument is not read.
    let fd = check(unsafe { libc::openat(dir.as_raw_fd(), path.as_ptr(), flags | libc::O_CLOEXEC) })?;
    // SAFETY: openat returned a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether open(2) honours O_RESOLVE_BENEATH (macOS 15+), asked once: an
/// absolute path is always refused under it (ENOTCAPABLE), so a kernel that
/// opens "/" ignored the flag. Any other answer counts as no.
#[cfg(target_os = "macos")]
pub fn resolve_beneath_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| refuses(c"/", O_RESOLVE_BENEATH, libc::ENOTCAPABLE))
}

/// Whether open(2) honours O_NOFOLLOW_ANY (macOS 11+: ELOOP at any symlink
/// in the path), asked once on `/tmp`, a symlink on every macOS. If it ever
/// were not one, the answer is no, and callers take their slower way.
#[cfg(target_os = "macos")]
pub fn nofollow_any_works() -> bool {
    static WORKS: OnceLock<bool> = OnceLock::new();
    *WORKS.get_or_init(|| refuses(c"/tmp", libc::O_NOFOLLOW_ANY, libc::ELOOP))
}

/// open(2) of `path` with `flag` fails with `errno` (a descriptor it opened
/// is closed at once).
#[cfg(target_os = "macos")]
fn refuses(path: &std::ffi::CStr, flag: libc::c_int, errno: libc::c_int) -> bool {
    // SAFETY: `path` is NUL-terminated and outlives the call; no O_CREAT.
    match check(unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | flag) }) {
        Ok(fd) => {
            // SAFETY: open returned a new descriptor that nothing else owns.
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            false
        }
        Err(e) => e.raw_os_error() == Some(errno),
    }
}

/// The path the kernel has for an open descriptor (fcntl F_GETPATH). Asked
/// of the descriptor, not of a path, it names the file that will be read
/// even if a symlink was swapped since. A file with several hard links may
/// be named by any of them.
#[cfg(target_os = "macos")]
pub fn fd_path(fd: BorrowedFd<'_>) -> io::Result<std::path::PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    // MAXPATHLEN (sys/param.h), which is what F_GETPATH writes at most.
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most MAXPATHLEN
    // (= PATH_MAX, 1024) bytes into `buf`, which is that long and lives for
    // the call; `fd` is borrowed, so it stays open for it.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) })?;
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(std::ffi::OsStr::from_bytes(&buf[..len]).into())
}

/// pidfd_open(2): a descriptor that becomes readable when process `pid`
/// exits, for watching a process that is not our child (`wardend` watching
/// supervisors) without polling and without pid-reuse races. ENOSYS before
/// Linux 5.3 (callers fall back to `kill(pid, 0)`), ESRCH if it is gone.
#[cfg(target_os = "linux")]
pub fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    let Ok(p) = libc::pid_t::try_from(pid) else {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("pid {pid} out of range")));
    };
    if p <= 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("refusing pidfd_open({p})")));
    }
    // SAFETY: plain syscall on integers; no memory is passed.
    let rc = unsafe { libc::syscall(libc::SYS_pidfd_open, p as libc::c_long, 0 as libc::c_long) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: pidfd_open returned a new descriptor (close-on-exec by
    // definition) that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(rc as RawFd) })
}

/// Not Linux: no pidfds; always `Unsupported`, and callers fall back to
/// polling `kill(pid, 0)`.
#[cfg(not(target_os = "linux"))]
pub fn pidfd_open(pid: u32) -> io::Result<OwnedFd> {
    let _ = pid;
    Err(io::Error::new(io::ErrorKind::Unsupported, "pidfd_open is Linux-only"))
}

/// pread that never waits for the disk: preadv2(RWF_NOWAIT) returns
/// EAGAIN (`ErrorKind::WouldBlock`) when the data is not in the page cache,
/// so a server can read cached files inline and hand the rest to a thread.
/// `Unsupported` where the C library lacks preadv2.
pub fn pread_nowait(fd: BorrowedFd<'_>, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    #[cfg(target_env = "gnu")]
    {
        let iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
        // SAFETY: `iov` describes exactly `buf`, which is exclusively
        // borrowed for the call, so the kernel writes only inside it; one
        // iovec; `fd` is borrowed and stays open.
        let n = unsafe { libc::preadv2(fd.as_raw_fd(), &iov, 1, offset as libc::off_t, libc::RWF_NOWAIT) };
        if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
    }
    #[cfg(not(target_env = "gnu"))]
    {
        let _ = (fd, buf, offset);
        Err(io::Error::from(io::ErrorKind::Unsupported))
    }
}

/// send(2) on a socket, never raising SIGPIPE; with `more`, MSG_MORE tells
/// the kernel more data follows (response headers before a sendfile body),
/// so headers and the first body bytes share a packet.
#[cfg(target_os = "linux")]
pub fn send(sock: BorrowedFd<'_>, buf: &[u8], more: bool) -> io::Result<usize> {
    let flags = libc::MSG_NOSIGNAL | if more { libc::MSG_MORE } else { 0 };
    // SAFETY: `buf` is a valid slice for its length and send only reads it;
    // `sock` is borrowed and stays open for the call.
    let n = unsafe { libc::send(sock.as_raw_fd(), buf.as_ptr() as *const libc::c_void, buf.len(), flags) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// send(2) on a socket, never raising SIGPIPE. Not Linux: there is no
/// MSG_NOSIGNAL; the socket must carry SO_NOSIGPIPE (`set_nosigpipe`, once
/// per connection: setting it on every send cost a system call each time;
/// Rust programs also ignore SIGPIPE, so this is belt and braces). There is
/// no MSG_MORE either, so `more` is ignored (headers may go out in their
/// own packet; `sendfile_head` sends them with the file instead).
#[cfg(not(target_os = "linux"))]
pub fn send(sock: BorrowedFd<'_>, buf: &[u8], more: bool) -> io::Result<usize> {
    let _ = more;
    // SAFETY: `buf` is a valid slice for its length and send only reads it;
    // `sock` is borrowed and stays open for the call.
    let n = unsafe { libc::send(sock.as_raw_fd(), buf.as_ptr() as *const libc::c_void, buf.len(), 0) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Take one connection off a listening socket without waiting: the new
/// descriptor is non-blocking and close-on-exec. `WouldBlock` when none is
/// waiting. Linux does it in one call (accept4); elsewhere accept(2) and two
/// fcntl calls. The peer's address is not asked for (nothing here uses it).
#[cfg(target_os = "linux")]
pub fn accept_nonblocking(listener: BorrowedFd<'_>) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    loop {
        // SAFETY: the listener is borrowed and open; null address pointers
        // are allowed and mean "don't return the peer's address".
        let fd = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            )
        };
        if fd >= 0 {
            // SAFETY: accept4 returned a new descriptor that nothing else owns.
            return Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) });
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn accept_nonblocking(listener: BorrowedFd<'_>) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let fd = loop {
        // SAFETY: as above.
        let fd = unsafe { libc::accept(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if fd >= 0 {
            break fd;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // SAFETY: accept returned a new descriptor that nothing else owns; from
    // here it closes on every path out.
    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    // SAFETY: fcntl on the descriptor we own, with integer arguments only.
    let ok = unsafe {
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) >= 0 && {
            let fl = libc::fcntl(fd, libc::F_GETFL);
            fl >= 0 && libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK) >= 0
        }
    };
    if ok { Ok(owned) } else { Err(io::Error::last_os_error()) }
}

/// recv(2) into the spare room of `buf`, growing its length by what arrived.
/// `WouldBlock` when nothing has (the socket must be non-blocking); `Ok(0)`
/// when the peer has closed (or `buf` has no room).
pub fn recv_into(sock: BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<usize> {
    loop {
        let spare = buf.spare_capacity_mut();
        // SAFETY: `spare` is writable memory of the length given; recv
        // writes at most that many bytes into it; `sock` stays open.
        let n = unsafe { libc::recv(sock.as_raw_fd(), spare.as_mut_ptr().cast::<libc::c_void>(), spare.len(), 0) };
        if n >= 0 {
            let n = n as usize;
            // SAFETY: recv initialised the first `n` spare bytes, and n <= spare.len().
            unsafe { buf.set_len(buf.len() + n) };
            return Ok(n);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// A small send buffer on `sock`, so a test can fill it with a modest write.
#[cfg(all(test, target_os = "linux"))]
pub fn set_send_buffer(sock: BorrowedFd<'_>, bytes: i32) -> io::Result<()> {
    setsockopt_int(sock, libc::SOL_SOCKET, libc::SO_SNDBUF, bytes)
}

/// shutdown(2) both directions of the socket `fd`: a read waiting on it
/// returns end-of-file at once, so the task that owns the connection ends and
/// closes it. Takes a bare descriptor because the caller (the static server's
/// idle sweep, `static_server/idle.rs`) is not the owner; it must know the
/// connection is still open, and that is its whole job. The call itself is
/// memory-safe for any number, a stale one only fails (EBADF, ENOTSOCK).
pub fn shutdown_both(fd: std::os::fd::RawFd) -> io::Result<()> {
    // SAFETY: shutdown takes two integers and touches no memory.
    let r = unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Is at least one byte waiting to be read on the socket `fd`? A peek that
/// never blocks and takes nothing off the queue. False when the queue is
/// empty, the peer has closed, or the call fails.
pub fn has_unread(fd: std::os::fd::RawFd) -> bool {
    let mut b = 0u8;
    // SAFETY: `b` is a valid one-byte buffer for the call; MSG_PEEK leaves
    // the data queued and MSG_DONTWAIT makes the call return at once.
    let n =
        unsafe { libc::recv(fd, (&mut b as *mut u8).cast::<libc::c_void>(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n > 0
}

/// Make later `send`s and `sendfile`s on this connection fail with EPIPE
/// instead of raising SIGPIPE. Linux: nothing to do (`send` passes
/// MSG_NOSIGNAL per call). Not Linux: SO_NOSIGPIPE, once, when the
/// connection is accepted. macOS refuses socket options with EINVAL once
/// the connection is already reset: that is not an error here, the next
/// send reports the real one (EPIPE, ECONNRESET).
#[cfg(target_os = "linux")]
pub fn set_nosigpipe(sock: BorrowedFd<'_>) -> io::Result<()> {
    let _ = sock;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_nosigpipe(sock: BorrowedFd<'_>) -> io::Result<()> {
    match setsockopt_int(sock, libc::SOL_SOCKET, libc::SO_NOSIGPIPE, 1) {
        Err(e) if e.raw_os_error() != Some(libc::EINVAL) => Err(e),
        _ => Ok(()),
    }
}

/// TCP_NODELAY on a TCP socket (a listener too). On Linux a connection
/// accepted from a listener that has it set starts with it: one system
/// call at startup instead of one per connection (`NODELAY_INHERITED`).
pub fn set_tcp_nodelay(sock: BorrowedFd<'_>, on: bool) -> io::Result<()> {
    setsockopt_int(sock, libc::IPPROTO_TCP, libc::TCP_NODELAY, on as libc::c_int)
}

/// Do accepted connections inherit TCP_NODELAY from their listener? Linux:
/// yes (tested below). Elsewhere not relied on: callers set it on each
/// accepted socket.
pub const NODELAY_INHERITED: bool = cfg!(target_os = "linux");

/// TCP_CORK on or off (Linux): while it is on, the kernel sends only full-size
/// segments and holds a partial one back; turning it off sends what is held.
/// For a big body that keeps every segment full and the receiver from being
/// woken for each piece (nginx's `tcp_nopush`). Elsewhere `Unsupported`:
/// callers go on without it (macOS has TCP_NOPUSH, not measured here).
#[cfg(target_os = "linux")]
pub fn set_tcp_cork(sock: BorrowedFd<'_>, on: bool) -> io::Result<()> {
    setsockopt_int(sock, libc::IPPROTO_TCP, libc::TCP_CORK, on as libc::c_int)
}

#[cfg(not(target_os = "linux"))]
pub fn set_tcp_cork(sock: BorrowedFd<'_>, on: bool) -> io::Result<()> {
    let _ = (sock, on);
    Err(io::ErrorKind::Unsupported.into())
}

/// A worker's IPC socket (fd 3), set up so that the worker does not wait
/// for Warden, whatever Warden does (stopped, a frozen VM, a bug): room for
/// `IPC_BUFFER` bytes (send buffer on the worker's end, receive buffer on
/// Warden's), as much as the system allows (macOS starts AF_UNIX sockets
/// with 8 KB, about a minute of heartbeats; never made smaller), and with
/// `nonblocking`, O_NONBLOCK on the worker's end: a full socket answers
/// EAGAIN, and the shim keeps what it must not lose until there is room.
pub fn prepare_ipc_socket(ours: BorrowedFd<'_>, theirs: BorrowedFd<'_>, nonblocking: bool) -> io::Result<()> {
    let _ = grow_socket_buffer(theirs, libc::SO_SNDBUF, IPC_BUFFER);
    let _ = grow_socket_buffer(ours, libc::SO_RCVBUF, IPC_BUFFER);
    if !nonblocking {
        return Ok(());
    }
    // SAFETY: fcntl on a borrowed descriptor, which stays open for the call,
    // with integer arguments only.
    unsafe {
        let fl = check(libc::fcntl(theirs.as_raw_fd(), libc::F_GETFL))?;
        check(libc::fcntl(theirs.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK))?;
    }
    Ok(())
}

/// What a worker's IPC socket asks for (`prepare_ipc_socket`): with a
/// heartbeat a second, hours of them while Warden reads nothing.
pub const IPC_BUFFER: libc::c_int = 4 << 20;

/// `opt` (SO_SNDBUF or SO_RCVBUF) of `sock` raised towards `want`: halved
/// while the system refuses it (macOS: above kern.ipc.maxsockbuf; Linux caps
/// it silently at net.core.wmem_max / rmem_max). Returns the size it has.
pub fn grow_socket_buffer(sock: BorrowedFd<'_>, opt: libc::c_int, want: libc::c_int) -> io::Result<libc::c_int> {
    let had = getsockopt_int(sock, libc::SOL_SOCKET, opt)?;
    let mut want = want;
    while want > had {
        if setsockopt_int(sock, libc::SOL_SOCKET, opt, want).is_ok() {
            break;
        }
        want /= 2;
    }
    getsockopt_int(sock, libc::SOL_SOCKET, opt)
}

fn getsockopt_int(fd: BorrowedFd<'_>, level: libc::c_int, name: libc::c_int) -> io::Result<libc::c_int> {
    let mut value: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `value` and `len` are live and sized for a c_int, as passed.
    check(unsafe {
        libc::getsockopt(fd.as_raw_fd(), level, name, &mut value as *mut libc::c_int as *mut libc::c_void, &mut len)
    })?;
    Ok(value)
}

fn setsockopt_int(fd: BorrowedFd<'_>, level: libc::c_int, name: libc::c_int, value: libc::c_int) -> io::Result<()> {
    // SAFETY: `value` is a live c_int and the length passed is its size.
    check(unsafe {
        libc::setsockopt(
            fd.as_raw_fd(),
            level,
            name,
            &value as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    })
    .map(|_| ())
}

/// TCP_DEFER_ACCEPT on a listener: accept(2) wakes the server only once
/// the client has sent data (its request), not on the bare handshake. The
/// kernel still answers the handshake, so TCP health checks work.
#[cfg(target_os = "linux")]
pub fn tcp_defer_accept(listener: BorrowedFd<'_>, secs: i32) -> io::Result<()> {
    setsockopt_int(listener, libc::IPPROTO_TCP, libc::TCP_DEFER_ACCEPT, secs)
}

/// Not Linux: no TCP_DEFER_ACCEPT, so a no-op: accept(2) wakes on the bare
/// handshake, which is just as correct, a little busier.
#[cfg(not(target_os = "linux"))]
pub fn tcp_defer_accept(listener: BorrowedFd<'_>, secs: i32) -> io::Result<()> {
    let _ = (listener, secs);
    Ok(())
}

/// A sealed in-memory file holding exactly `data`: memfd_create(2)
/// (close-on-exec), the bytes written, then sealed against every change
/// (write, grow, shrink, more seals). sendfile(2) sends from it by page
/// reference, without the copy a send(2) of the same bytes makes; it never
/// waits for a disk. `Unsupported` outside Linux.
#[cfg(target_os = "linux")]
pub fn sealed_memfd(name: &std::ffi::CStr, data: &[u8]) -> io::Result<std::fs::File> {
    use std::io::Write;
    // SAFETY: `name` is NUL-terminated and outlives the call; the flags are
    // constants; no other memory is passed.
    let raw = check(unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) })?;
    // SAFETY: memfd_create just returned `raw`; the File is its only owner,
    // so every early return below closes it.
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    file.write_all(data)?;
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;
    // SAFETY: fcntl on a descriptor we own, with integer arguments only.
    check(unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) })?;
    Ok(file)
}

/// Not Linux: no memfd; always `Unsupported` (callers keep the bytes in
/// memory and send(2) them).
#[cfg(not(target_os = "linux"))]
pub fn sealed_memfd(name: &std::ffi::CStr, data: &[u8]) -> io::Result<std::fs::File> {
    let _ = (name, data);
    Err(io::Error::new(io::ErrorKind::Unsupported, "memfd is Linux-only"))
}

/// Index of the first `byte` in `hay`: the C library's memchr, which is
/// vectorised (glibc: SSE2/AVX2/EVEX), far faster than a byte loop on long
/// log chunks.
pub fn memchr(byte: u8, hay: &[u8]) -> Option<usize> {
    if hay.is_empty() {
        return None;
    }
    // SAFETY: memchr reads at most `hay.len()` bytes starting at
    // `hay.as_ptr()`, all inside the borrowed slice; it returns null or a
    // pointer into that same range, so the offset is in bounds.
    unsafe {
        let p = libc::memchr(hay.as_ptr() as *const libc::c_void, byte as libc::c_int, hay.len());
        if p.is_null() { None } else { Some((p as *const u8).offset_from(hay.as_ptr()) as usize) }
    }
}

// ------------------------------------------------------------- splice
//
// `[logging] worker_output = "direct"`: worker output moves from its pipe
// to the log file inside the kernel, never through Warden's memory.

/// One splice(2) call: moves up to `len` bytes from the pipe `pipe` into
/// `out` without copying them through userspace. With `off_out`, the bytes
/// land at `*off_out` (which is advanced by the count, like pwrite: the
/// file position is untouched); without, at `out`'s file position.
///
/// Flags are SPLICE_F_NONBLOCK (an empty pipe is `WouldBlock`, never a
/// wait; it says nothing about `out`, which is written like write(2)) and
/// SPLICE_F_MOVE (a hint to move pages rather than copy). Ok(0) means the
/// pipe is empty and every writer has closed it (EOF). EINVAL means one of
/// the two can't splice (`pipe` is not a pipe, `out` is opened O_APPEND,
/// or its filesystem has no splice support) and ENOSYS/EPERM that the call
/// itself is unavailable (old kernel, seccomp): callers fall back to
/// read/write.
#[cfg(target_os = "linux")]
pub fn splice(pipe: BorrowedFd<'_>, out: BorrowedFd<'_>, off_out: Option<&mut u64>, len: usize) -> io::Result<usize> {
    let flags = libc::SPLICE_F_NONBLOCK | libc::SPLICE_F_MOVE;
    let mut off: libc::loff_t = 0;
    let off_ptr: *mut libc::loff_t = match &off_out {
        Some(o) => {
            off = libc::loff_t::try_from(**o)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, format!("splice offset {o} out of range")))?;
            &mut off
        }
        None => std::ptr::null_mut(),
    };
    // SAFETY: both descriptors are borrowed, so they stay open (and can't be
    // reused for something else) for the whole call. off_in is null (a pipe
    // has no offset); off_out is null or points to `off`, a live,
    // exclusively borrowed loff_t on this stack frame that the kernel reads
    // and updates. splice moves bytes between the two kernel objects and
    // touches no other Rust memory.
    let n = unsafe { libc::splice(pipe.as_raw_fd(), std::ptr::null_mut(), out.as_raw_fd(), off_ptr, len, flags) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if let Some(o) = off_out {
        *o = off as u64;
    }
    Ok(n as usize)
}

// ------------------------------------------------------------- sock_diag
//
// The kernel's socket diagnostics (netlink, NETLINK_SOCK_DIAG: what `ss`
// reads): the TCP sockets in LISTEN of Warden's network namespace, with their
// accept queues, in one request, however many connections the host has (the
// /proc/net/tcp text has a row per connection: megabytes on a busy host).
// Read-only: nothing is asked of the processes that own the sockets.

/// A TCP socket in LISTEN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpListen {
    /// Its inode: `socket:[N]` in the /proc/<pid>/fd of the process holding it.
    pub inode: u64,
    pub addr: std::net::IpAddr,
    pub port: u16,
    /// Connections that are established and wait for the app to accept them.
    pub backlog: u32,
    /// How many may wait (listen(2)'s backlog, capped by net.core.somaxconn).
    pub max_backlog: u32,
    /// Connections dropped since the socket was made: the queue was full, or
    /// memory short (the socket's `sk_drops`, its share of ListenDrops).
    pub drops: u32,
}

#[cfg(target_os = "linux")]
mod diag {
    pub(super) const SOCK_DIAG_BY_FAMILY: u16 = 20;
    pub(super) const NLMSG_ERROR: u16 = 2;
    pub(super) const NLMSG_DONE: u16 = 3;
    pub(super) const TCP_ESTABLISHED: u32 = 1;
    pub(super) const TCP_LISTEN: u32 = 10;
    /// The request's filter attribute, and the comparisons it uses.
    pub(super) const REQ_BYTECODE: u16 = 1;
    pub(super) const BC_S_GE: u8 = 2;
    pub(super) const BC_S_LE: u8 = 3;
    /// The attribute with the socket's memory counters (`INET_DIAG_SKMEMINFO`).
    pub(super) const SKMEMINFO: u16 = 7;
    /// Where `sk_drops` is in it (`SK_MEMINFO_DROPS`), in u32s.
    pub(super) const MEMINFO_DROPS: usize = 8;
    /// struct nlmsghdr, and struct inet_diag_msg before its attributes.
    pub(super) const NLMSG_HDR: usize = 16;
    pub(super) const MSG: usize = 72;
    /// struct unix_diag_msg before its attributes; the name it is bound to
    /// (`UDIAG_SHOW_NAME`, answered as `UNIX_DIAG_NAME`).
    pub(super) const UNIX_MSG: usize = 16;
    pub(super) const UDIAG_SHOW_NAME: u32 = 1;
    pub(super) const UNIX_DIAG_NAME: u16 = 0;
}

/// A Unix socket in LISTEN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnixListen {
    /// Its inode, as for [`TcpListen`].
    pub inode: u64,
    /// The address it is bound to, as the kernel keeps it (`sun_path`, its
    /// length included: a path ends with its NUL, an abstract name starts
    /// with one); `None`: unnamed.
    pub name: Option<Vec<u8>>,
}

/// The TCP sockets in LISTEN of this network namespace, IPv4 and IPv6. Not
/// Linux: `Unsupported`.
#[cfg(target_os = "linux")]
pub fn tcp_listeners() -> io::Result<Vec<TcpListen>> {
    // SAFETY: socket(2) with constant arguments; the descriptor it returns
    // is owned by the OwnedFd from here on, which closes it.
    let fd = check(unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG)
    })?;
    // SAFETY: `fd` was just returned by socket(2) and nothing else owns it.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut found = Vec::new();
    for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
        let req = inet_diag_request(family, 1 << diag::TCP_LISTEN, 1 << (diag::SKMEMINFO - 1));
        diag_dump(std::os::fd::AsFd::as_fd(&sock), &req, |msg| found.extend(parse_tcp_listen(msg)))?;
    }
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
pub fn tcp_listeners() -> io::Result<Vec<TcpListen>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "socket diagnostics are Linux-only"))
}

/// The Unix sockets in LISTEN of this network namespace, with their names:
/// the kernel walks its table and answers only those (`/proc/net/unix` has a
/// row per socket). Not Linux: `Unsupported`.
#[cfg(target_os = "linux")]
pub fn unix_listeners() -> io::Result<Vec<UnixListen>> {
    // SAFETY: as in `tcp_listeners`.
    let fd = check(unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG)
    })?;
    // SAFETY: `fd` was just returned by socket(2) and nothing else owns it.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut found = Vec::new();
    let req = unix_diag_request(1 << diag::TCP_LISTEN, diag::UDIAG_SHOW_NAME);
    diag_dump(std::os::fd::AsFd::as_fd(&sock), &req, |msg| found.extend(parse_unix_listen(msg)))?;
    Ok(found)
}

#[cfg(not(target_os = "linux"))]
pub fn unix_listeners() -> io::Result<Vec<UnixListen>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "socket diagnostics are Linux-only"))
}

/// A dump request: struct nlmsghdr, then struct unix_diag_req for Unix
/// sockets in `states` (a bit per state, TCP's numbers) with what `show`
/// asks for (`UDIAG_SHOW_*`), every inode.
#[cfg(target_os = "linux")]
fn unix_diag_request(states: u32, show: u32) -> [u8; 40] {
    let mut r = [0u8; 40];
    r[0..4].copy_from_slice(&40u32.to_ne_bytes());
    r[4..6].copy_from_slice(&diag::SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    r[6..8].copy_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    r[8..12].copy_from_slice(&1u32.to_ne_bytes());
    r[16] = libc::AF_UNIX as u8;
    r[20..24].copy_from_slice(&states.to_ne_bytes());
    r[28..32].copy_from_slice(&show.to_ne_bytes());
    r
}

/// One struct unix_diag_msg (and its attributes): its inode and its name.
#[cfg(target_os = "linux")]
fn parse_unix_listen(msg: &[u8]) -> Option<UnixListen> {
    if msg.len() < diag::UNIX_MSG || i32::from(msg[0]) != libc::AF_UNIX {
        return None;
    }
    let inode = u64::from(u32::from_ne_bytes(msg[4..8].try_into().ok()?));
    let mut name = None;
    let mut at = diag::UNIX_MSG;
    while at + 4 <= msg.len() {
        let len = usize::from(u16::from_ne_bytes([msg[at], msg[at + 1]]));
        let kind = u16::from_ne_bytes([msg[at + 2], msg[at + 3]]);
        if len < 4 || at + len > msg.len() {
            break;
        }
        if kind == diag::UNIX_DIAG_NAME {
            name = Some(msg[at + 4..at + len].to_vec());
        }
        at += (len + 3) & !3;
    }
    Some(UnixListen { inode, name })
}

/// How many TCP connections are established on local port `port` (IPv4 and
/// IPv6) in this network namespace: the kernel matches the port (a bytecode
/// filter, as `ss sport = :N` sends) and returns only those. Not Linux:
/// `Unsupported`.
#[cfg(target_os = "linux")]
pub fn tcp_connections(port: u16) -> io::Result<u32> {
    // SAFETY: as in `tcp_listeners`.
    let fd = check(unsafe {
        libc::socket(libc::AF_NETLINK, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, libc::NETLINK_SOCK_DIAG)
    })?;
    // SAFETY: `fd` was just returned by socket(2) and nothing else owns it.
    let sock = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut n = 0u32;
    for family in [libc::AF_INET as u8, libc::AF_INET6 as u8] {
        let mut req = inet_diag_request(family, 1 << diag::TCP_ESTABLISHED, 0).to_vec();
        req.extend_from_slice(&sport_filter(port));
        let len = req.len() as u32;
        req[0..4].copy_from_slice(&len.to_ne_bytes());
        diag_dump(std::os::fd::AsFd::as_fd(&sock), &req, |_| n = n.saturating_add(1))?;
    }
    Ok(n)
}

#[cfg(not(target_os = "linux"))]
pub fn tcp_connections(_port: u16) -> io::Result<u32> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "socket diagnostics are Linux-only"))
}

/// The request attribute (INET_DIAG_REQ_BYTECODE) with a filter for local
/// port `port`: source port >= port, then <= port. Each comparison is an
/// inet_diag_bc_op (code, yes, no: where to go on true and on false, in
/// bytes; landing exactly at the end accepts, past it refuses) followed by
/// one whose `no` holds the port.
#[cfg(target_os = "linux")]
fn sport_filter(port: u16) -> [u8; 20] {
    let op = |code: u8, yes: u8, no: u16| {
        let mut o = [0u8; 4];
        o[0] = code;
        o[1] = yes;
        o[2..4].copy_from_slice(&no.to_ne_bytes());
        o
    };
    let mut a = [0u8; 20];
    a[0..2].copy_from_slice(&20u16.to_ne_bytes());
    a[2..4].copy_from_slice(&diag::REQ_BYTECODE.to_ne_bytes());
    a[4..8].copy_from_slice(&op(diag::BC_S_GE, 8, 20));
    a[8..12].copy_from_slice(&op(0, 0, port));
    a[12..16].copy_from_slice(&op(diag::BC_S_LE, 8, 12));
    a[16..20].copy_from_slice(&op(0, 0, port));
    a
}

/// A dump request: struct nlmsghdr, then struct inet_diag_req_v2 for TCP
/// sockets of `family` in `states` (a bit per TCP state) with the attributes
/// in `ext` (a bit per INET_DIAG_* attribute, less one), and a socket id of
/// zeros (every socket).
#[cfg(target_os = "linux")]
fn inet_diag_request(family: u8, states: u32, ext: u8) -> [u8; 72] {
    let mut r = [0u8; 72];
    r[0..4].copy_from_slice(&72u32.to_ne_bytes());
    r[4..6].copy_from_slice(&diag::SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    r[6..8].copy_from_slice(&((libc::NLM_F_REQUEST | libc::NLM_F_DUMP) as u16).to_ne_bytes());
    r[8..12].copy_from_slice(&1u32.to_ne_bytes());
    r[16] = family;
    r[17] = libc::IPPROTO_TCP as u8;
    r[18] = ext;
    r[20..24].copy_from_slice(&states.to_ne_bytes());
    r
}

/// Send `req` to the kernel and hand the payload of each answer to `each`,
/// until the dump is done.
#[cfg(target_os = "linux")]
fn diag_dump(sock: BorrowedFd<'_>, req: &[u8], mut each: impl FnMut(&[u8])) -> io::Result<()> {
    // SAFETY: sockaddr_nl is plain data, for which all zeros is a valid value
    // (the kernel as the destination, no multicast groups).
    let mut to: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    // SAFETY: the descriptor is borrowed, so it stays open for the call; the
    // kernel reads `req.len()` bytes of `req`, and the address, a live
    // sockaddr_nl on this stack frame, for the size given.
    let sent = unsafe {
        libc::sendto(
            sock.as_raw_fd(),
            req.as_ptr().cast(),
            req.len(),
            0,
            (&raw const to).cast(),
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    // A dump arrives in parts of at most 32 KB.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        // SAFETY: the descriptor is borrowed; the kernel writes at most
        // `buf.len()` bytes into `buf`, which this function owns.
        let n = unsafe { libc::recv(sock.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), 0) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "socket diagnostics ended early"));
        }
        if diag_messages(&buf[..n as usize], &mut each)? {
            return Ok(());
        }
    }
}

/// The netlink messages in one part of a dump: each payload of an answer to
/// `each`. True when the dump is done (or ended with an error, returned).
#[cfg(target_os = "linux")]
fn diag_messages(part: &[u8], each: &mut impl FnMut(&[u8])) -> io::Result<bool> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "a malformed socket diagnostics message");
    let mut at = 0;
    while at + diag::NLMSG_HDR <= part.len() {
        let len = u32::from_ne_bytes(part[at..at + 4].try_into().map_err(|_| bad())?) as usize;
        let kind = u16::from_ne_bytes(part[at + 4..at + 6].try_into().map_err(|_| bad())?);
        if len < diag::NLMSG_HDR || at + len > part.len() {
            return Err(bad());
        }
        let payload = &part[at + diag::NLMSG_HDR..at + len];
        match kind {
            diag::NLMSG_DONE => return Ok(true),
            diag::NLMSG_ERROR => {
                let code = payload.get(..4).ok_or_else(bad)?;
                let errno = i32::from_ne_bytes(code.try_into().map_err(|_| bad())?);
                return if errno == 0 { Ok(true) } else { Err(io::Error::from_raw_os_error(-errno)) };
            }
            diag::SOCK_DIAG_BY_FAMILY => each(payload),
            _ => {}
        }
        at += (len + 3) & !3;
    }
    Ok(false)
}

/// One struct inet_diag_msg (and its attributes) of a socket in LISTEN.
#[cfg(target_os = "linux")]
fn parse_tcp_listen(msg: &[u8]) -> Option<TcpListen> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    if msg.len() < diag::MSG {
        return None;
    }
    let u32_at = |i: usize| msg.get(i..i + 4).and_then(|b| b.try_into().ok()).map(u32::from_ne_bytes);
    let addr = match i32::from(msg[0]) {
        libc::AF_INET => IpAddr::V4(Ipv4Addr::new(msg[8], msg[9], msg[10], msg[11])),
        libc::AF_INET6 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(&msg[8..24]).ok()?)),
        _ => return None,
    };
    let mut drops = 0;
    let mut at = diag::MSG;
    while at + 4 <= msg.len() {
        let len = usize::from(u16::from_ne_bytes([msg[at], msg[at + 1]]));
        let kind = u16::from_ne_bytes([msg[at + 2], msg[at + 3]]);
        if len < 4 || at + len > msg.len() {
            break;
        }
        if kind == diag::SKMEMINFO {
            let data = &msg[at + 4..at + len];
            let i = diag::MEMINFO_DROPS * 4;
            if let Some(d) = data.get(i..i + 4) {
                drops = u32::from_ne_bytes(d.try_into().ok()?);
            }
        }
        at += (len + 3) & !3;
    }
    Some(TcpListen {
        inode: u64::from(u32_at(68)?),
        addr,
        port: u16::from_be_bytes([msg[4], msg[5]]),
        backlog: u32_at(56)?,
        max_backlog: u32_at(60)?,
        drops,
    })
}

// ------------------------------------------------ between fork and exec
//
// These run in the child after fork(2) and before exec: only
// async-signal-safe calls, no allocation, no locks (see `Command::pre_exec`).

/// Put the worker's end of Warden's IPC channel (a socket, or any
/// descriptor) at `target` in the child (fd 3), inherited by exec.
pub fn child_dup_ipc(child_fd: RawFd, target: RawFd) -> io::Result<()> {
    // SAFETY: dup2/fcntl on descriptors that exist in the child; both are
    // async-signal-safe.
    unsafe {
        check(libc::dup2(child_fd, target))?;
        if child_fd == target {
            // dup2 onto itself keeps FD_CLOEXEC; clear it so the fd survives exec.
            check(libc::fcntl(target, libc::F_SETFD, 0))?;
        }
    }
    Ok(())
}

/// The child gets `sig` when its parent thread (Warden) dies.
#[cfg(target_os = "linux")]
pub fn child_parent_death_signal(sig: i32) -> io::Result<()> {
    // Tests of the macOS orphan sweep run on Linux with workers that outlive
    // their supervisor (debug builds only). An atomic load: this runs between
    // fork and exec.
    #[cfg(debug_assertions)]
    if WORKERS_OUTLIVE.load(std::sync::atomic::Ordering::Relaxed) {
        return Ok(());
    }
    // SAFETY: prctl(PR_SET_PDEATHSIG) takes the signal as an integer.
    check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, sig as libc::c_ulong) }).map(|_| ())
}

/// Set by [`test_workers_outlive_the_supervisor`].
#[cfg(all(target_os = "linux", debug_assertions))]
static WORKERS_OUTLIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// From now on workers do not get SIGTERM when their supervisor dies, as on
/// macOS: how a Linux test makes the orphans the sweep is for
/// (`WARDEN_TEST_MACOS_ORPHANS`, `platform::orphans`). Debug builds only.
#[cfg(all(target_os = "linux", debug_assertions))]
pub fn test_workers_outlive_the_supervisor() {
    WORKERS_OUTLIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Not Linux: there is no parent-death signal, so this is a no-op. On macOS
/// workers survive a supervisor killed with SIGKILL (a normal stop still
/// stops them); the next start of the app stops them (`platform::orphans`),
/// and until then they keep running.
#[cfg(not(target_os = "linux"))]
pub fn child_parent_death_signal(sig: i32) -> io::Result<()> {
    let _ = sig;
    Ok(())
}

/// New session: no controlling terminal, own process group.
pub fn child_new_session() -> io::Result<()> {
    // SAFETY: setsid takes no arguments; async-signal-safe.
    check(unsafe { libc::setsid() }).map(|_| ())
}

/// Set up `cmd`'s child between fork and exec the way a worker needs: the
/// IPC channel `ipc_fd` (the worker's end of the socketpair) at `target`
/// (fd 3), and, where the OS can, SIGTERM when its parent dies. The safe
/// face of `Command::pre_exec`, whose `unsafe` is only about the closure
/// being async-signal-safe: this one calls nothing but the two helpers
/// above, so it allocates and locks nothing after fork. `ipc_fd` must stay
/// open until the command is spawned.
pub fn pre_exec_worker(cmd: &mut std::process::Command, ipc_fd: RawFd, target: RawFd, handoff: Option<RawFd>) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure only calls `child_dup_ipc` (dup2, fcntl),
    // `child_dup_above` (fcntl) and `child_parent_death_signal` (prctl or
    // nothing; an atomic load in debug builds): all async-signal-safe, no
    // allocation, no locks. It captures three integers.
    unsafe {
        cmd.pre_exec(move || {
            // The handoff channel goes to `target + 1`: first out of the way
            // (it may be `target` itself), then into place after fd 3.
            let handoff = match handoff {
                Some(fd) => Some(child_dup_above(fd, target + 2)?),
                None => None,
            };
            child_dup_ipc(ipc_fd, target)?;
            if let Some(fd) = handoff {
                child_dup_ipc(fd, target + 1)?;
            }
            child_parent_death_signal(libc::SIGTERM)
        });
    }
}

/// Receive one message and the descriptor riding on it, if any (the other
/// end of [`send_with_fd`]). Blocks like the socket does.
#[cfg(test)]
pub fn recv_with_fd(sock: BorrowedFd<'_>, max: usize) -> io::Result<(Vec<u8>, Option<OwnedFd>)> {
    let mut buf = vec![0u8; max];
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr() as *mut libc::c_void, iov_len: buf.len() };
    // SAFETY: an all-zero msghdr is valid; its pointers are set to live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: `msg` points at buffers that outlive the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(n as usize);
    // SAFETY: recvmsg filled `control` and set msg_controllen; CMSG_FIRSTHDR
    // checks the length, and an SCM_RIGHTS header carries one descriptor here.
    let fd = unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (!cmsg.is_null() && (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS)
            .then(|| OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(cmsg) as *const RawFd)))
    };
    Ok((buf, fd))
}

/// In a forked child: a copy of `fd` numbered `min` or above (close-on-exec;
/// `child_dup_ipc` then moves it into place without the flag).
pub fn child_dup_above(fd: RawFd, min: RawFd) -> io::Result<RawFd> {
    // SAFETY: fcntl(F_DUPFD_CLOEXEC) on a descriptor that exists in the
    // child; async-signal-safe.
    check(unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, min) })
}

/// Send `buf` with `fd` attached (SCM_RIGHTS) on a Unix socket: the
/// receiver gets its own copy of the descriptor. Never raises SIGPIPE
/// (MSG_NOSIGNAL on Linux, SO_NOSIGPIPE on the socket elsewhere). The
/// descriptor rides on the first byte, so a short write still delivered it.
pub fn send_with_fd(sock: BorrowedFd<'_>, buf: &[u8], fd: BorrowedFd<'_>) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_NOSIGNAL;
    #[cfg(not(target_os = "linux"))]
    let flags = 0;
    let raw = fd.as_raw_fd();
    // SAFETY: CMSG_SPACE/CMSG_LEN are pure size computations.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut control = vec![0u8; space];
    let mut iov = libc::iovec { iov_base: buf.as_ptr() as *mut libc::c_void, iov_len: buf.len() };
    // SAFETY: an all-zero msghdr is valid; the fields set below point at
    // `iov` and `control`, which outlive the sendmsg call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = space as _;
    // SAFETY: `control` is CMSG_SPACE bytes for one descriptor, so the first
    // header and its data fit; the header is written before its data is.
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(cmsg) as *mut RawFd, raw);
    }
    // SAFETY: `msg` is fully initialized and its buffers are alive.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, flags) };
    if n < 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// Run `cmd`'s child in a new session (`child_new_session`): detached from
/// our terminal and process group, so Ctrl-C here or closing the shell does
/// not reach it. The safe face of `Command::pre_exec`.
pub fn pre_exec_new_session(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: the closure is `child_new_session`: setsid, async-signal-safe,
    // no allocation, no locks.
    unsafe {
        cmd.pre_exec(child_new_session);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// flock is per open file description: a second open of the same file
    /// waits (here: reports `false`) until the first is closed.
    #[test]
    fn try_lock_exclusive_excludes_a_second_open_until_the_first_closes() {
        let path = std::env::temp_dir().join(format!("warden-flock-{}", std::process::id()));
        let open = || std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path).unwrap();
        let (a, b) = (open(), open());
        assert!(try_lock_exclusive(&a).unwrap());
        assert!(!try_lock_exclusive(&b).unwrap(), "held by the first");
        assert!(try_lock_exclusive(&a).unwrap(), "taking it again is fine");
        drop(a);
        assert!(try_lock_exclusive(&b).unwrap(), "free once the first is closed");
        let _ = std::fs::remove_file(&path);
    }

    /// A pipe, a file and a closed descriptor have no width; a pseudo-terminal
    /// reports the size it was given (skipped where there is no pty).
    #[test]
    fn terminal_width_is_none_off_a_terminal_and_the_size_on_one() {
        let null = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(terminal_width(null.as_raw_fd()), None);
        assert_eq!(terminal_width(-1), None);
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: openpty fills the two descriptors we pass pointers to; the
        // other arguments are optional and null.
        let rc = unsafe {
            libc::openpty(&mut master, &mut slave, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
        };
        if rc != 0 {
            return; // no pty here
        }
        let mut ws = libc::winsize { ws_row: 24, ws_col: 77, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCSWINSZ reads a `winsize` from a valid pointer.
        let set = unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &mut ws) };
        assert_eq!(set, 0);
        assert_eq!(terminal_width(slave), Some(77));
        // SAFETY: both descriptors are ours and still open.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }

    fn open_fds() -> usize {
        #[cfg(target_os = "linux")]
        let dir = "/proc/self/fd";
        // No /proc: macOS lists the caller's descriptors in /dev/fd.
        #[cfg(not(target_os = "linux"))]
        let dir = "/dev/fd";
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
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

    #[cfg(target_os = "linux")]
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

    #[cfg(target_os = "linux")]
    #[test]
    fn nofile_limit_matches_proc() {
        let (soft, hard) = nofile_limit();
        let limits = std::fs::read_to_string("/proc/self/limits").unwrap();
        let line = limits.lines().find(|l| l.starts_with("Max open files")).unwrap();
        let f: Vec<&str> = line.split_whitespace().collect();
        let parse = |s: &str| if s == "unlimited" { u64::MAX } else { s.parse().unwrap() };
        assert_eq!((soft, hard), (parse(f[3]), parse(f[4])));
        assert!(soft > 0 && soft <= hard);
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
    fn user_names_come_from_the_user_database() {
        assert_eq!(user_name(0).as_deref(), Some("root"));
        assert_eq!(user_name(0x7fff_fff0), None, "no such user");
    }

    #[test]
    fn hostname_and_load_average() {
        let name = hostname().expect("every host has a name");
        assert!(!name.contains('\0') && !name.trim().is_empty(), "{name:?}");
        let load = loadavg().expect("getloadavg");
        assert!(load.iter().all(|l| l.is_finite() && *l >= 0.0), "{load:?}");
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
        #[cfg(target_os = "linux")]
        let dead =
            || std::fs::read_to_string(format!("/proc/{grandchild}/stat")).map(|s| s.contains(") Z ")).unwrap_or(true);
        // No /proc: gone once launchd has reaped it.
        #[cfg(not(target_os = "linux"))]
        let dead = || kill(grandchild, 0).is_err();
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
    fn socketpairs_are_cloexec_and_do_not_leak() {
        run_isolated("socketpair_leak_probe");
    }

    #[test]
    fn socketpair_leak_probe() {
        if !in_probe() {
            return;
        }
        let before = open_fds();
        for _ in 0..2000 {
            let (a, b) = socketpair_cloexec().unwrap();
            assert_eq!(fd_flags(a.as_raw_fd()), (true, false), "CLOEXEC, blocking");
            assert_eq!(fd_flags(b.as_raw_fd()), (true, false), "CLOEXEC, blocking");
        }
        assert_eq!(open_fds(), before, "every socket closed on drop");
        // Both directions carry bytes; closing one end is EOF at the other.
        let (a, b) = socketpair_cloexec().unwrap();
        let mut a = std::os::unix::net::UnixStream::from(a);
        let mut b = std::os::unix::net::UnixStream::from(b);
        a.write_all(b"ping\n").unwrap();
        b.write_all(b"pong\n").unwrap();
        let (mut x, mut y) = ([0u8; 5], [0u8; 5]);
        b.read_exact(&mut x).unwrap();
        a.read_exact(&mut y).unwrap();
        assert_eq!((&x, &y), (b"ping\n", b"pong\n"));
        drop(a);
        let mut rest = Vec::new();
        assert_eq!(b.read_to_end(&mut rest).unwrap(), 0);
    }

    /// The worker's end of the IPC socket lands on fd 3 and works both ways:
    /// the child reads a line Warden sends and answers on the same fd.
    #[test]
    fn socketpair_reaches_a_child_both_ways() {
        use std::os::unix::process::CommandExt;
        let (ours, theirs) = socketpair_cloexec().unwrap();
        let cfd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "read -r line <&3; echo \"got $line\" >&3"]);
        // SAFETY (test): the closure only calls the async-signal-safe helper.
        unsafe {
            cmd.pre_exec(move || child_dup_ipc(cfd, 3));
        }
        let mut child = cmd.spawn().unwrap();
        drop(theirs); // only the child's copy remains: its exit is EOF here
        let mut s = std::os::unix::net::UnixStream::from(ours);
        s.write_all(b"promote\n").unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(out, "got promote\n");
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

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_memfd_holds_exactly_the_bytes_and_cannot_change() {
        use std::os::unix::fs::FileExt;
        let data: Vec<u8> = (0..100_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let f = sealed_memfd(c"warden-test", &data).unwrap();
        assert_eq!(f.metadata().unwrap().len(), data.len() as u64);
        assert!(fd_flags(f.as_raw_fd()).0, "close-on-exec");
        let mut back = vec![0u8; data.len()];
        f.read_exact_at(&mut back, 0).unwrap();
        assert!(back == data);
        // Sealed: no write anywhere, no resize either way.
        for at in [0, 99_999, 100_000] {
            assert_eq!(f.write_at(b"x", at).unwrap_err().raw_os_error(), Some(libc::EPERM), "write at {at}");
        }
        assert!(f.set_len(10).is_err() && f.set_len(200_000).is_err());
        f.read_exact_at(&mut back, 0).unwrap();
        assert!(back == data, "unchanged");
        // sendfile sends the exact bytes from any offset, and stops at the end.
        use std::os::fd::AsFd;
        for (off, len) in [(0usize, data.len()), (123, 4096), (99_999, 1), (50_000, 50_000)] {
            let (tx, mut rx) = socket_pair();
            let reader = std::thread::spawn(move || {
                let mut got = Vec::new();
                rx.read_to_end(&mut got).unwrap();
                got
            });
            let mut o = off as i64;
            let mut sent = 0;
            while sent < len {
                let n = sendfile(tx.as_fd(), f.as_fd(), &mut o, len - sent).unwrap();
                assert!(n > 0, "EOF inside the memfd at {o}");
                sent += n;
            }
            assert_eq!(sendfile(tx.as_fd(), f.as_fd(), &mut (data.len() as i64), 10).unwrap(), 0, "past the end");
            drop(tx);
            assert!(reader.join().unwrap() == data[off..off + len], "bytes differ at {off}+{len}");
        }
        // Empty is fine too.
        assert_eq!(sealed_memfd(c"warden-empty", &[]).unwrap().metadata().unwrap().len(), 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_memfd_does_not_leak() {
        run_isolated("sealed_memfd_leak_probe");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_memfd_leak_probe() {
        if !in_probe() {
            return;
        }
        let before = open_fds();
        for i in 0..500 {
            let f = sealed_memfd(c"warden-probe", &vec![i as u8; i * 7]).unwrap();
            drop(f);
        }
        assert_eq!(open_fds(), before);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn sealed_memfd_is_unsupported_outside_linux() {
        assert_eq!(sealed_memfd(c"x", b"abc").unwrap_err().kind(), io::ErrorKind::Unsupported);
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
        // The reader starts only once a send has found the buffers full: a reader that keeps up
        // (a fast macOS runner) would otherwise leave nothing to wait for.
        let full = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let started = full.clone();
        let reader = std::thread::spawn(move || {
            while !started.load(std::sync::atomic::Ordering::Acquire) {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
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
                    full.store(true, std::sync::atomic::Ordering::Release);
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

    /// `sendfile_head` with blocking sockets: all of `head`, then `[off, off+count)`.
    fn send_head_range(
        sock: &std::net::TcpStream,
        file: &std::fs::File,
        head: &[u8],
        mut off: i64,
        count: usize,
    ) -> io::Result<i64> {
        use std::os::fd::AsFd;
        let (mut head_left, mut body_left) = (head, count as u64);
        while !head_left.is_empty() || body_left > 0 {
            let n = sendfile_head(sock.as_fd(), file.as_fd(), &mut off, body_left as usize, head_left)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let (h, b) = split_head_body(n, head_left.len(), body_left).expect("never more than asked for");
            head_left = &head_left[h..];
            body_left -= b;
        }
        Ok(off)
    }

    #[test]
    fn split_head_body_counts_the_head_first() {
        assert_eq!(split_head_body(0, 10, 100), Some((0, 0)));
        assert_eq!(split_head_body(4, 10, 100), Some((4, 0)), "a partial head: no body yet");
        assert_eq!(split_head_body(10, 10, 100), Some((10, 0)));
        assert_eq!(split_head_body(11, 10, 100), Some((10, 1)));
        assert_eq!(split_head_body(110, 10, 100), Some((10, 100)), "everything");
        assert_eq!(split_head_body(111, 10, 100), None, "more than was asked for");
        assert_eq!(split_head_body(7, 0, 7), Some((0, 7)), "no head left");
        assert_eq!(split_head_body(8, 0, 7), None);
        assert_eq!(split_head_body(0, 0, 0), Some((0, 0)));
        // Never panics, for any input.
        for total in [0usize, 1, 99, usize::MAX] {
            for head in [0usize, 1, 99] {
                for body in [0u64, 1, 99] {
                    let _ = split_head_body(total, head, body);
                }
            }
        }
    }

    /// Head and file bytes arrive exactly, in order, for empty and large
    /// heads, offsets, and a count of 0 (the head alone).
    #[test]
    fn sendfile_head_sends_the_head_then_exact_file_bytes() {
        let data: Vec<u8> = (0..(3 << 20) as u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let (file, path) = temp_file(&data);
        for (head_len, off, len) in [
            (0usize, 0usize, 0usize),
            (0, 0, 20_000),
            (253, 0, 0),
            (253, 0, 1),
            (253, 17, 48 * 1024),
            (280, 4096, 100_000),
            (70_000, 5, 200_000),
            (300, 0, data.len()),
            (1, data.len() - 10, 10),
        ] {
            let head: Vec<u8> = (0..head_len).map(|i| b'A' + (i % 26) as u8).collect();
            let (tx, mut rx) = socket_pair();
            let reader = std::thread::spawn(move || {
                let mut got = Vec::new();
                rx.read_to_end(&mut got).unwrap();
                got
            });
            let end = send_head_range(&tx, &file, &head, off as i64, len).unwrap();
            assert_eq!(end, (off + len) as i64, "the offset moves by the file bytes sent, not by the head's");
            drop(tx);
            let got = reader.join().unwrap();
            let mut want = head.clone();
            want.extend_from_slice(&data[off..off + len]);
            assert!(got == want, "bytes differ for head {head_len}, offset {off}, length {len}");
        }
        let _ = std::fs::remove_file(path);
    }

    /// A reader slower than the sender: the socket fills in the middle of the
    /// head and in the middle of the body; progress is reported, never lost.
    #[test]
    fn sendfile_head_on_a_slow_reader_hits_eagain_and_resumes() {
        use std::os::fd::AsFd;
        let data: Vec<u8> = (0..(1 << 20) as u32).map(|i| (i % 253) as u8).collect();
        let (file, path) = temp_file(&data);
        let head: Vec<u8> = (0..100_000u32).map(|i| (i % 199) as u8).collect();
        let (tx, mut rx) = socket_pair();
        tx.set_nonblocking(true).unwrap();
        tx.set_nodelay(true).unwrap();
        let small: libc::c_int = 32 * 1024;
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
        let (mut head_left, mut body_left, mut off) = (&head[..], data.len() as u64, 0i64);
        let (mut would_block, mut partial_head) = (0, 0);
        while !head_left.is_empty() || body_left > 0 {
            match sendfile_head(tx.as_fd(), file.as_fd(), &mut off, body_left as usize, head_left) {
                Ok(0) => panic!("unexpected EOF"),
                Ok(n) => {
                    let (h, b) = split_head_body(n, head_left.len(), body_left).unwrap();
                    if h > 0 && h < head_left.len() {
                        partial_head += 1;
                    }
                    head_left = &head_left[h..];
                    body_left -= b;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    would_block += 1;
                    std::thread::yield_now();
                }
                Err(e) => panic!("{e}"),
            }
        }
        drop(tx);
        let got = reader.join().unwrap();
        assert_eq!(got.len(), head.len() + data.len());
        assert!(got[..head.len()] == head[..] && got[head.len()..] == data[..], "head then file, in order");
        assert_eq!(off as usize, data.len(), "the offset followed the file bytes only");
        assert!(would_block > 0, "the slow reader should have filled the socket buffer");
        assert!(partial_head > 0, "a 100 KB head does not fit 32 KB buffers in one call");
        let _ = std::fs::remove_file(path);
    }

    /// A signal (EINTR) or a full socket (EAGAIN) after the head went out is
    /// progress, not an error: the caller (`sendfile_all`) retries on both, and
    /// an error here would send the head a second time, corrupting the body
    /// and the keep-alive framing. Before anything went out they stay errors,
    /// so the retry sends it all.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_body_that_cannot_start_after_the_head_reports_the_head_as_sent() {
        use io::ErrorKind::{BrokenPipe, Interrupted, WouldBlock};
        let err = |k: io::ErrorKind| -> io::Result<usize> { Err(k.into()) };
        for k in [WouldBlock, Interrupted] {
            assert_eq!(after_head(253, err(k)).unwrap(), 253, "{k:?} after the head: the head is out");
            let e = after_head(0, err(k)).unwrap_err();
            assert_eq!(e.kind(), k, "{k:?} with nothing sent stays an error: the caller retries it all");
        }
        assert_eq!(after_head(253, Ok(1000)).unwrap(), 1253, "head and body bytes together");
        assert_eq!(after_head(0, Ok(7)).unwrap(), 7);
        assert_eq!(after_head(253, Ok(0)).unwrap(), 253, "end of file after the head");
        // A real failure is one, whatever went out before it.
        assert_eq!(after_head(253, err(BrokenPipe)).unwrap_err().kind(), BrokenPipe);
        // The raw OS error counts the same as the kind.
        let eintr = Err(io::Error::from_raw_os_error(libc::EINTR));
        assert_eq!(after_head(10, eintr).unwrap(), 10);
    }

    /// A file shorter than promised ends the response early: the caller sees
    /// Ok(0) once the head is out, never a hang or an invented body.
    #[test]
    fn sendfile_head_reports_a_truncated_file() {
        use std::os::fd::AsFd;
        let (file, path) = temp_file(&[7u8; 1000]);
        let (tx, _rx) = socket_pair();
        let mut off = 0i64;
        let mut total = 0;
        let mut rounds = 0;
        // Ask for 5000 bytes of a 1000-byte file, with a 10-byte head.
        let head = [b'h'; 10];
        let mut head_left = &head[..];
        loop {
            rounds += 1;
            assert!(rounds < 10, "must terminate");
            let n = sendfile_head(tx.as_fd(), file.as_fd(), &mut off, 5000, head_left).unwrap();
            if n == 0 {
                break;
            }
            let (h, b) = split_head_body(n, head_left.len(), 5000).unwrap();
            head_left = &head_left[h..];
            total += h + b as usize;
        }
        assert_eq!(total, 10 + 1000, "the head and the file's own bytes, then the end");
        assert_eq!(off, 1000);
        let _ = std::fs::remove_file(path);
    }

    /// Linux: a connection accepted from a listener with TCP_NODELAY set has
    /// it too, so the server needs no setsockopt per connection.
    #[cfg(target_os = "linux")]
    #[test]
    fn accepted_connections_inherit_nodelay_from_the_listener() {
        use std::os::fd::AsFd;
        const { assert!(NODELAY_INHERITED) };
        let accept_one = |nodelay: bool| {
            let l = listen_tcp("127.0.0.1:0".parse().unwrap(), false, 16).unwrap();
            if nodelay {
                set_tcp_nodelay(l.as_fd(), true).unwrap();
            }
            let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
            l.set_nonblocking(false).unwrap();
            let (s, _) = l.accept().unwrap();
            drop(c);
            s.nodelay().unwrap()
        };
        assert!(accept_one(true), "inherited from the listener");
        assert!(!accept_one(false), "off by default: the setting is what makes the difference");
        // Turning it off again works too.
        let (tx, _rx) = socket_pair();
        set_tcp_nodelay(tx.as_fd(), true).unwrap();
        assert!(tx.nodelay().unwrap());
        set_tcp_nodelay(tx.as_fd(), false).unwrap();
        assert!(!tx.nodelay().unwrap());
        // Not a socket: an error, not a crash.
        assert!(set_tcp_nodelay(std::fs::File::open("/dev/null").unwrap().as_fd(), true).is_err());
    }

    /// Linux: a corked socket holds a small write back until the cork is
    /// taken off, then sends it; refuses what is not a socket.
    #[cfg(target_os = "linux")]
    #[test]
    fn tcp_cork_holds_a_partial_segment_until_it_is_released() {
        use std::io::{Read, Write};
        use std::os::fd::AsFd;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut tx = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (mut rx, _) = l.accept().unwrap();
        tx.set_nodelay(true).unwrap();
        rx.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];

        set_tcp_cork(tx.as_fd(), true).unwrap();
        tx.write_all(b"held back").unwrap();
        // With the cork on the bytes are not sent (without it, NODELAY puts
        // them on the wire inside the write call: the loopback delivers them
        // before it returns). Looking at once leaves no room for the kernel's
        // own 200 ms release of a cork to have happened.
        let e = rx.read(&mut buf).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "a corked partial segment must wait");

        set_tcp_cork(tx.as_fd(), false).unwrap();
        let t0 = std::time::Instant::now();
        let n = loop {
            match rx.read(&mut buf) {
                Ok(n) => break n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && t0.elapsed() < std::time::Duration::from_secs(5) => {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Err(e) => panic!("the held bytes never arrived: {e}"),
            }
        };
        assert_eq!(&buf[..n], b"held back");
        // Twice is fine; a file is not a socket.
        set_tcp_cork(tx.as_fd(), false).unwrap();
        assert!(set_tcp_cork(std::fs::File::open("/dev/null").unwrap().as_fd(), true).is_err());
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn tcp_cork_is_unsupported_off_linux() {
        use std::os::fd::AsFd;
        let (tx, _rx) = socket_pair();
        assert_eq!(set_tcp_cork(tx.as_fd(), true).unwrap_err().kind(), io::ErrorKind::Unsupported);
    }

    /// A worker's end of its IPC socket: a write never blocks (EAGAIN when
    /// full, so a stopped Warden can't freeze the worker), and it holds at
    /// least what a plain socket holds (macOS: far more than its 8 KB).
    #[test]
    fn a_workers_ipc_end_never_blocks_and_holds_more_than_a_plain_socket() {
        use std::os::fd::AsFd;
        let line = [b'x'; 120];
        // Bytes written before the socket is full (EAGAIN), Warden reading nothing.
        let fill = |fd: BorrowedFd<'_>| {
            let mut total = 0usize;
            loop {
                match send(fd, &line, false) {
                    Ok(n) => total += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => return total,
                    Err(e) => panic!("{e}"),
                }
                assert!(total < 256 << 20, "never full");
            }
        };
        let (_plain_ours, plain) = socketpair_cloexec().unwrap();
        // SAFETY (test): fcntl with integer arguments on a live descriptor.
        unsafe {
            let fl = libc::fcntl(plain.as_raw_fd(), libc::F_GETFL);
            libc::fcntl(plain.as_raw_fd(), libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
        let plain_room = fill(plain.as_fd());

        let (ours, theirs) = socketpair_cloexec().unwrap();
        prepare_ipc_socket(ours.as_fd(), theirs.as_fd(), true).unwrap();
        // SAFETY (test): fcntl with integer arguments on a live descriptor.
        let fl = unsafe { libc::fcntl(theirs.as_raw_fd(), libc::F_GETFL) };
        assert!(fl & libc::O_NONBLOCK != 0, "the worker's end must not block");
        let room = fill(theirs.as_fd());
        eprintln!("IPC room: {room} bytes (a plain socket: {plain_room})");
        assert!(room >= plain_room, "{room} < {plain_room}");
        #[cfg(target_os = "macos")]
        assert!(room >= 512 << 10, "macOS: {room} bytes, about a minute of heartbeats");
        // Warden reads again: there is room again.
        let mut buf = Vec::with_capacity(64 << 10);
        recv_into(ours.as_fd(), &mut buf).unwrap();
        assert!(send(theirs.as_fd(), &line, false).is_ok());
        // A second call never shrinks what the first set.
        let had = getsockopt_int(theirs.as_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF).unwrap();
        assert!(grow_socket_buffer(theirs.as_fd(), libc::SO_SNDBUF, 4096).unwrap() >= had);
    }

    #[test]
    fn set_nosigpipe_accepts_sockets_and_refuses_other_files() {
        use std::os::fd::AsFd;
        let (tx, _rx) = socket_pair();
        set_nosigpipe(tx.as_fd()).unwrap();
        set_nosigpipe(tx.as_fd()).unwrap(); // twice is fine
        #[cfg(not(target_os = "linux"))]
        assert!(set_nosigpipe(std::fs::File::open("/dev/null").unwrap().as_fd()).is_err());
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
        #[cfg(target_os = "linux")]
        assert!(matches!(e.raw_os_error(), Some(libc::EPIPE) | Some(libc::ECONNRESET)), "unexpected error {e}");
        // macOS's sendfile also says ENOTCONN once the reset has landed.
        #[cfg(not(target_os = "linux"))]
        assert!(
            matches!(e.raw_os_error(), Some(libc::EPIPE) | Some(libc::ECONNRESET) | Some(libc::ENOTCONN)),
            "unexpected error {e}"
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path2);
    }

    /// Everywhere: the IPC fd lands on fd 3, the parent-death call succeeds
    /// (a no-op outside Linux) and setsid makes the child a group leader.
    #[test]
    fn child_helpers_are_async_signal_safe_everywhere() {
        use std::os::unix::process::CommandExt;
        let (r, w) = pipe_cloexec().unwrap();
        let wfd = w.as_raw_fd();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo ok >&3; echo $$; ps -o pgid= -p $$"]).stdout(std::process::Stdio::piped());
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
        let ids: Vec<i64> = text.split_whitespace().map(|x| x.parse().unwrap()).collect();
        assert_eq!(ids.len(), 2, "{text}");
        assert_eq!(ids[0], ids[1], "setsid: the child leads its own process group");
        let mut msg = String::new();
        std::fs::File::from(r).read_to_string(&mut msg).unwrap();
        assert_eq!(msg, "ok\n");
    }

    /// The safe wrappers over `pre_exec` do what the helpers do: the IPC fd
    /// arrives on fd 3 and works both ways, a second descriptor is not
    /// shared by accident, and the child leads its own process group.
    #[test]
    fn the_pre_exec_wrappers_set_up_the_child() {
        let (ours, theirs) = socketpair_cloexec().unwrap();
        let cfd = theirs.as_raw_fd();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "read -r line <&3; echo \"got $line\" >&3; echo $$; ps -o pgid= -p $$ >&3"])
            .stdout(std::process::Stdio::piped());
        pre_exec_worker(&mut cmd, cfd, 3, None);
        pre_exec_new_session(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        drop(theirs); // only the child's copy remains: its exit is EOF here
        let mut s = std::os::unix::net::UnixStream::from(ours);
        s.write_all(b"promote\n").unwrap();
        let mut over_ipc = String::new();
        s.read_to_string(&mut over_ipc).unwrap();
        let mut stdout = String::new();
        child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
        assert!(child.wait().unwrap().success());
        let mut lines = over_ipc.lines();
        assert_eq!(lines.next(), Some("got promote"), "the channel works both ways on fd 3");
        let (pid, pgid): (i64, i64) = (stdout.trim().parse().unwrap(), lines.next().unwrap().trim().parse().unwrap());
        assert_eq!(pid, pgid, "the new session made the child its own group leader");
    }

    #[cfg(target_os = "linux")]
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
        // 0, 1, 2, the IPC fd 3, and the fd `ls` opened for /proc/self/fd,
        // plus any this test process itself inherited without close-on-exec
        // (a CI runner leaks a few into its steps): those reach every child.
        let leaked_to_us = |fd: u32| fd > 2 && !fd_flags(fd as RawFd).0;
        assert!(fds.iter().all(|f| *f <= 4 || leaked_to_us(*f)), "unexpected inherited descriptors: {fds:?}");
        assert!(fds.contains(&3), "{fds:?}");
        let mut msg = String::new();
        std::fs::File::from(r).read_to_string(&mut msg).unwrap();
        assert_eq!(msg, "ok\n");
    }

    // ------------------------------------------------------------ openat2

    /// A scratch tree: root/{a.txt, sub/b.txt, in -> a.txt, abs -> <root>/a.txt,
    /// up -> ../outside.txt, outdir -> <parent>, fifo}, outside.txt next to it.
    fn tree() -> (std::path::PathBuf, std::fs::File) {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "warden-tree-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let root = base.join("root");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "A").unwrap();
        std::fs::write(root.join("sub/b.txt"), "B").unwrap();
        std::fs::write(base.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("in")).unwrap();
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("abs")).unwrap();
        std::os::unix::fs::symlink("../outside.txt", root.join("up")).unwrap();
        std::os::unix::fs::symlink(&base, root.join("outdir")).unwrap();
        let c = std::ffi::CString::new(root.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
        // SAFETY (test): mkfifo with a NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let dir = std::fs::File::open(&root).unwrap();
        (base, dir)
    }

    fn open_rel(dir: &std::fs::File, p: &str, resolve: u64) -> io::Result<OwnedFd> {
        use std::os::fd::AsFd;
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;
        openat2(dir.as_fd(), &std::ffi::CString::new(p).unwrap(), flags, resolve)
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn read_all(fd: OwnedFd) -> String {
        let mut s = String::new();
        std::fs::File::from(fd).read_to_string(&mut s).unwrap();
        s
    }

    const BENEATH: u64 = RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS;

    /// Not Linux: the Linux-only calls say so (callers fall back) or do
    /// nothing, and never touch the descriptors they are given.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn linux_only_calls_are_unsupported_or_no_ops() {
        use std::os::fd::AsFd;
        let (base, dir) = tree();
        assert_eq!(open_rel(&dir, "a.txt", BENEATH).unwrap_err().kind(), io::ErrorKind::Unsupported);
        assert_eq!(pidfd_open(std::process::id()).unwrap_err().kind(), io::ErrorKind::Unsupported);
        let (file, path) = temp_file(b"abc");
        let mut buf = [0u8; 3];
        assert_eq!(pread_nowait(file.as_fd(), &mut buf, 0).unwrap_err().kind(), io::ErrorKind::Unsupported);
        child_parent_death_signal(libc::SIGTERM).unwrap();
        // The descriptors still work.
        assert!(dir.metadata().unwrap().is_dir());
        use std::os::unix::fs::FileExt;
        file.read_exact_at(&mut buf, 0).unwrap();
        assert_eq!(&buf, b"abc");
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_dir_all(base);
    }

    /// macOS: O_RESOLVE_BENEATH is detected exactly where the kernel has
    /// it (Darwin 24, macOS 15, and later), O_NOFOLLOW_ANY on every
    /// supported macOS (11+).
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_open_flags_are_detected_by_kernel_version() {
        let out = std::process::Command::new("uname").arg("-r").output().unwrap();
        let release = String::from_utf8_lossy(&out.stdout).to_string();
        let major: u32 = release.split('.').next().unwrap().trim().parse().unwrap();
        assert_eq!(resolve_beneath_works(), major >= 24, "Darwin {release}");
        assert!(nofollow_any_works(), "Darwin {release}");
    }

    /// macOS: openat with O_RESOLVE_BENEATH stays under the directory (where
    /// the kernel has it), O_NOFOLLOW_ANY refuses every symlink, and
    /// F_GETPATH names the file a descriptor has open.
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_openat_flags_and_fd_path() {
        use std::os::fd::AsFd;
        let (base, dir) = tree();
        let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;
        let at = |p: &str, extra: libc::c_int| openat(dir.as_fd(), &std::ffi::CString::new(p).unwrap(), flags | extra);
        assert_eq!(read_all(at("in", 0).unwrap()), "A");
        assert_eq!(read_all(at("up", 0).unwrap()), "secret", "plain openat follows anything");
        if resolve_beneath_works() {
            assert_eq!(read_all(at("in", O_RESOLVE_BENEATH).unwrap()), "A");
            assert_eq!(read_all(at("sub/b.txt", O_RESOLVE_BENEATH).unwrap()), "B");
            for out in ["up", "abs", "outdir/outside.txt", "..", "sub/../../outside.txt"] {
                let e = at(out, O_RESOLVE_BENEATH).unwrap_err();
                assert_eq!(e.raw_os_error(), Some(libc::ENOTCAPABLE), "{out}: {e}");
            }
        }
        assert_eq!(read_all(at("sub/b.txt", libc::O_NOFOLLOW_ANY).unwrap()), "B");
        for link in ["in", "abs", "up", "outdir/outside.txt"] {
            assert_eq!(at(link, libc::O_NOFOLLOW_ANY).unwrap_err().raw_os_error(), Some(libc::ELOOP), "{link}");
        }
        let root = std::fs::canonicalize(base.join("root")).unwrap();
        assert_eq!(fd_path(dir.as_fd()).unwrap(), root);
        assert_eq!(fd_path(at("in", 0).unwrap().as_fd()).unwrap(), root.join("a.txt"), "the target, not the link");
        assert_eq!(
            fd_path(at("up", 0).unwrap().as_fd()).unwrap(),
            std::fs::canonicalize(base.join("outside.txt")).unwrap()
        );
        assert_eq!(at("missing", 0).unwrap_err().kind(), io::ErrorKind::NotFound);
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_stays_beneath_the_directory() {
        let (base, dir) = tree();
        match open_rel(&dir, "a.txt", BENEATH) {
            Ok(fd) => {
                assert!(fd_flags(fd.as_raw_fd()).0, "CLOEXEC");
                assert_eq!(read_all(fd), "A");
            }
            // Kernel before 5.6 or a seccomp filter: the caller's fallback.
            Err(e) => {
                assert!(matches!(e.raw_os_error(), Some(libc::ENOSYS) | Some(libc::EPERM)), "{e}");
                return;
            }
        }
        assert_eq!(read_all(open_rel(&dir, "sub/b.txt", BENEATH).unwrap()), "B");
        assert_eq!(read_all(open_rel(&dir, "in", BENEATH).unwrap()), "A", "relative symlink inside");
        assert_eq!(read_all(open_rel(&dir, "sub/../a.txt", BENEATH).unwrap()), "A", ".. that stays inside");
        assert!(open_rel(&dir, ".", BENEATH).is_ok(), "the directory itself");
        let exdev = |p: &str| open_rel(&dir, p, BENEATH).unwrap_err().raw_os_error();
        assert_eq!(exdev("../outside.txt"), Some(libc::EXDEV));
        assert_eq!(exdev("up"), Some(libc::EXDEV), "symlink escaping with ..");
        assert_eq!(exdev("abs"), Some(libc::EXDEV), "absolute symlinks are refused (callers re-check)");
        assert_eq!(exdev("outdir/outside.txt"), Some(libc::EXDEV), "directory symlink out");
        assert_eq!(exdev("/etc/passwd"), Some(libc::EXDEV), "absolute path");
        assert_eq!(open_rel(&dir, "missing", BENEATH).unwrap_err().raw_os_error(), Some(libc::ENOENT));
        assert_eq!(open_rel(&dir, "a.txt/x", BENEATH).unwrap_err().raw_os_error(), Some(libc::ENOTDIR));
        // A FIFO opens at once with O_NONBLOCK (no writer needed), so a
        // server can see it is not a regular file instead of hanging.
        let t0 = std::time::Instant::now();
        let fifo = std::fs::File::from(open_rel(&dir, "fifo", BENEATH).unwrap());
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
        assert!(!fifo.metadata().unwrap().is_file());
        // Unknown resolve flags are rejected cleanly, not misread.
        assert_eq!(open_rel(&dir, "a.txt", 1 << 40).unwrap_err().raw_os_error(), Some(libc::EINVAL));
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_cached_lookups_answer_or_say_eagain() {
        let (base, dir) = tree();
        let cached = BENEATH | libc::RESOLVE_CACHED;
        // Warm the dentry cache, then a cached lookup must succeed or, on
        // kernels without RESOLVE_CACHED (< 5.12), fail with EINVAL; on
        // filesystems that always revalidate, EAGAIN.
        let _ = std::fs::metadata(base.join("root/sub/b.txt"));
        match open_rel(&dir, "sub/b.txt", cached) {
            Ok(fd) => assert_eq!(read_all(fd), "B"),
            Err(e) => {
                assert!(
                    matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EAGAIN | libc::ENOSYS | libc::EPERM)),
                    "{e}"
                )
            }
        }
        // Never-seen names can't come from the cache: EAGAIN or ENOENT
        // (a negative dentry) — never a wrong file.
        match open_rel(&dir, "never-created-name", cached) {
            Ok(_) => panic!("a missing file opened"),
            Err(e) => assert!(
                matches!(
                    e.raw_os_error(),
                    Some(libc::EAGAIN | libc::ENOENT | libc::EINVAL | libc::ENOSYS | libc::EPERM)
                ),
                "{e}"
            ),
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_does_not_leak_descriptors() {
        run_isolated("openat2_leak_probe");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn openat2_leak_probe() {
        if !in_probe() {
            return;
        }
        let (base, dir) = tree();
        let before = open_fds();
        for _ in 0..1000 {
            let _ = open_rel(&dir, "a.txt", BENEATH);
            let _ = open_rel(&dir, "up", BENEATH);
            let _ = open_rel(&dir, "missing", BENEATH);
            let _ = open_rel(&dir, "fifo", BENEATH);
        }
        assert_eq!(open_fds(), before, "every descriptor closed, success or error");
        let _ = std::fs::remove_dir_all(base);
    }

    // ------------------------------------------------------- pread_nowait

    #[test]
    fn pread_nowait_reads_exact_ranges() {
        use std::os::fd::AsFd;
        let data: Vec<u8> = (0..200_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
        let (file, path) = temp_file(&data);
        let _ = std::fs::read(&path); // in the page cache
        for (off, len) in [(0usize, 0usize), (0, 1), (1, 4095), (4096, 4096), (12_345, 65_537), (199_999, 1)] {
            let mut buf = vec![0u8; len];
            let mut done = 0;
            while done < len {
                match pread_nowait(file.as_fd(), &mut buf[done..], (off + done) as u64) {
                    Ok(0) => panic!("early EOF"),
                    Ok(n) => done += n,
                    // Data not cached (or no preadv2): the caller's thread fallback.
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Unsupported => return,
                    Err(e) => panic!("{e}"),
                }
            }
            assert!(buf == data[off..off + len], "bytes differ at {off}+{len}");
        }
        // At and past EOF: 0 bytes, not garbage.
        let mut buf = [9u8; 16];
        assert_eq!(pread_nowait(file.as_fd(), &mut buf, 200_000).unwrap(), 0);
        assert_eq!(pread_nowait(file.as_fd(), &mut buf, 1 << 40).unwrap(), 0);
        assert_eq!(buf, [9u8; 16], "buffer untouched");
        // Short read at the tail: exactly the remaining bytes.
        assert_eq!(pread_nowait(file.as_fd(), &mut buf, 199_990).unwrap(), 10);
        assert_eq!(buf[..10], data[199_990..]);
        // Not a file: a clean error.
        let (r, _w) = pipe_cloexec().unwrap();
        assert_eq!(pread_nowait(r.as_fd(), &mut buf, 0).unwrap_err().raw_os_error(), Some(libc::ESPIPE));
        let _ = std::fs::remove_file(path);
    }

    // --------------------------------------------------------------- send

    /// Readable (the process exited) within `ms`?
    #[cfg(target_os = "linux")]
    fn readable_within(fd: &OwnedFd, ms: i32) -> bool {
        let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one live pollfd, count 1.
        let n = unsafe { libc::poll(&mut p, 1, ms) };
        n == 1 && p.revents & libc::POLLIN != 0
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_signals_exit_of_a_process() {
        let mut child = std::process::Command::new("sleep").arg("0.2").spawn().unwrap();
        let fd = pidfd_open(child.id()).unwrap();
        assert!(fd_flags(fd.as_raw_fd()).0, "pidfd must be close-on-exec");
        assert!(!readable_within(&fd, 0), "readable while the process runs");
        assert!(readable_within(&fd, 5000), "not readable after the process exited");
        child.wait().unwrap();
        // Refused and failing targets are errors, never a descriptor.
        assert_eq!(pidfd_open(0).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(pidfd_open(u32::MAX).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        let reaped = child.id();
        // (Ok is legal too: the pid may already belong to another process.)
        if let Err(e) = pidfd_open(reaped) {
            assert_eq!(e.raw_os_error(), Some(libc::ESRCH));
        }
        run_isolated("pidfd_leak_probe");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pidfd_leak_probe() {
        if !in_probe() {
            return;
        }
        let before = open_fds();
        for _ in 0..1000 {
            drop(pidfd_open(std::process::id()).unwrap());
            assert!(pidfd_open(0).is_err());
        }
        assert_eq!(open_fds(), before, "pidfd_open leaks descriptors");
    }

    #[test]
    fn send_with_more_delivers_everything_in_order() {
        use std::os::fd::AsFd;
        let (tx, mut rx) = socket_pair();
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            rx.read_to_end(&mut got).unwrap();
            got
        });
        let mut want = Vec::new();
        for i in 0..200u32 {
            let chunk = format!("header-{i}\r\n").into_bytes();
            let mut off = 0;
            while off < chunk.len() {
                off += send(tx.as_fd(), &chunk[off..], i % 2 == 0).unwrap();
            }
            want.extend_from_slice(&chunk);
        }
        // Data sent with MSG_MORE is still flushed by the last plain send / close.
        assert_eq!(send(tx.as_fd(), b"end", false).unwrap(), 3);
        want.extend_from_slice(b"end");
        drop(tx);
        assert_eq!(reader.join().unwrap(), want);
        assert_eq!(
            send(std::fs::File::open("/dev/null").unwrap().as_fd(), b"x", true).unwrap_err().raw_os_error(),
            Some(libc::ENOTSOCK)
        );
    }

    #[test]
    fn send_to_a_closed_peer_fails_without_sigpipe() {
        run_isolated("sigpipe_probe");
    }

    #[test]
    fn sigpipe_probe() {
        use std::os::fd::AsFd;
        if !in_probe() {
            return;
        }
        // Restore the default SIGPIPE action (which kills the process): only
        // MSG_NOSIGNAL keeps this probe alive.
        // SAFETY (test): signal() with a valid signal and SIG_DFL.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let (tx, rx) = socket_pair();
        // Not Linux: what keeps the probe alive (once per connection, as the server does).
        set_nosigpipe(tx.as_fd()).unwrap();
        drop(rx);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut err = None;
        for _ in 0..100 {
            if let Err(e) = send(tx.as_fd(), &[0u8; 65536], true) {
                err = Some(e);
                break;
            }
        }
        let e = err.expect("a closed peer must fail the send");
        assert!(matches!(e.raw_os_error(), Some(libc::EPIPE) | Some(libc::ECONNRESET)), "{e}");
    }

    // ---------------------------------------------------- socket and pipe options

    /// Everywhere (a no-op outside Linux): a deferred listener still accepts
    /// a client that sends its request.
    #[test]
    fn defer_accept_keeps_accepting() {
        use std::os::fd::AsFd;
        let l = listen_tcp("127.0.0.1:0".parse().unwrap(), false, 16).unwrap();
        tcp_defer_accept(l.as_fd(), 5).unwrap();
        let mut c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        l.set_nonblocking(false).unwrap();
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0u8; 3];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"GET");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn defer_accept_is_set_and_connections_still_arrive() {
        use std::os::fd::AsFd;
        let l = listen_tcp("127.0.0.1:0".parse().unwrap(), false, 16).unwrap();
        tcp_defer_accept(l.as_fd(), 5).unwrap();
        let mut v: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY (test): getsockopt into a live c_int with its size.
        let rc = unsafe {
            libc::getsockopt(
                l.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_DEFER_ACCEPT,
                &mut v as *mut _ as *mut libc::c_void,
                &mut len,
            )
        };
        assert_eq!(rc, 0);
        assert!(v > 0, "TCP_DEFER_ACCEPT set ({v})");
        // A client that sends its request is accepted right away.
        let mut c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        c.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        l.set_nonblocking(false).unwrap();
        let (mut s, _) = l.accept().unwrap();
        let mut b = [0u8; 3];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"GET");
        // Not a socket: a clean error.
        let f = std::fs::File::open("/dev/null").unwrap();
        assert_eq!(tcp_defer_accept(f.as_fd(), 5).unwrap_err().raw_os_error(), Some(libc::ENOTSOCK));
    }

    // ------------------------------------------------------------- memchr

    #[test]
    fn memchr_matches_a_plain_search() {
        assert_eq!(memchr(b'\n', b""), None);
        assert_eq!(memchr(b'\n', b"\n"), Some(0));
        assert_eq!(memchr(b'\n', b"ab"), None);
        assert_eq!(memchr(0, b"a\0b\0"), Some(1));
        assert_eq!(memchr(0xff, &[0xfe, 0xff, 0xff]), Some(1));
        // Every length and alignment up to a few vector widths, the needle
        // at every position (first occurrence wins), and none at all.
        let mut seed = 7u32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (seed >> 16) as u8 % 10 + b'a'
            })
            .collect();
        for start in 0..64 {
            for len in 0..300 {
                let hay = &data[start..start + len];
                assert_eq!(memchr(b'\n', hay), None);
                for needle in [b'a', b'e', b'j'] {
                    assert_eq!(memchr(needle, hay), hay.iter().position(|&b| b == needle), "start {start} len {len}");
                }
                if len > 0 {
                    let mut v = hay.to_vec();
                    for pos in [0, len / 2, len - 1] {
                        v[pos] = b'\n';
                        assert_eq!(memchr(b'\n', &v), v.iter().position(|&b| b == b'\n'));
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------- splice

    #[cfg(target_os = "linux")]
    mod splice_tests {
        use super::*;
        use std::os::fd::AsFd;

        /// A fresh, empty scratch file opened read-write (no O_APPEND).
        fn scratch() -> (std::fs::File, std::path::PathBuf) {
            let (_, p) = temp_file(b"");
            let f = std::fs::OpenOptions::new().read(true).write(true).open(&p).unwrap();
            (f, p)
        }

        /// Splice everything a writer thread puts in a pipe into `out` at
        /// `off`, waiting on EAGAIN; returns the bytes moved.
        fn splice_all(data: Vec<u8>, out: &std::fs::File, off: &mut u64, chunk: usize) -> usize {
            let (r, w) = pipe_cloexec().unwrap();
            let writer = std::thread::spawn(move || std::fs::File::from(w).write_all(&data).unwrap());
            let mut moved = 0;
            loop {
                match splice(r.as_fd(), out.as_fd(), Some(&mut *off), chunk) {
                    Ok(0) => break,
                    Ok(n) => moved += n,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
                    Err(e) => panic!("{e}"),
                }
            }
            writer.join().unwrap();
            moved
        }

        #[test]
        fn splice_moves_exact_bytes_for_many_sizes() {
            let data: Vec<u8> =
                (0..(3 << 20) as u32 + 7).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
            for len in [0usize, 1, 2, 4095, 4096, 4097, 65_535, 65_536, 65_537, 1 << 20, data.len()] {
                for chunk in [1usize, 4096, 1 << 20] {
                    if chunk == 1 && len > 70_000 {
                        continue; // byte-at-a-time over megabytes proves nothing more
                    }
                    let (f, p) = scratch();
                    let mut off = 0u64;
                    let moved = splice_all(data[..len].to_vec(), &f, &mut off, chunk);
                    assert_eq!((moved, off), (len, len as u64), "len {len} chunk {chunk}");
                    let got = std::fs::read(&p).unwrap();
                    assert!(got == data[..len], "bytes differ for len {len}, chunk {chunk}");
                    let _ = std::fs::remove_file(p);
                }
            }
        }

        #[test]
        fn splice_writes_at_the_offset_and_leaves_the_rest() {
            let (mut f, p) = scratch();
            f.write_all(&[b'a'; 10_000]).unwrap();
            // Overwrite in the middle; the file position (10000) is not used or moved.
            let mut off = 1_000u64;
            assert_eq!(splice_all(vec![b'b'; 5_000], &f, &mut off, 1 << 20), 5_000);
            assert_eq!(off, 6_000);
            // Past the end: the gap reads as zeros (callers never leave one).
            let mut off = 12_000u64;
            assert_eq!(splice_all(vec![b'c'; 100], &f, &mut off, 1 << 20), 100);
            assert_eq!(off, 12_100);
            let got = std::fs::read(&p).unwrap();
            assert_eq!(got.len(), 12_100);
            assert!(got[..1_000].iter().all(|&b| b == b'a'));
            assert!(got[1_000..6_000].iter().all(|&b| b == b'b'));
            assert!(got[6_000..10_000].iter().all(|&b| b == b'a'));
            assert!(got[10_000..12_000].iter().all(|&b| b == 0));
            assert!(got[12_000..].iter().all(|&b| b == b'c'));
            // Without an offset: the file position, which advances.
            use std::io::Seek;
            f.seek(io::SeekFrom::Start(2)).unwrap();
            let (r, w) = pipe_cloexec().unwrap();
            std::fs::File::from(w).write_all(b"XYZ").unwrap();
            assert_eq!(splice(r.as_fd(), f.as_fd(), None, 64).unwrap(), 3);
            assert_eq!(f.stream_position().unwrap(), 5);
            assert_eq!(&std::fs::read(&p).unwrap()[..6], b"aaXYZa");
            // An offset beyond what the kernel takes is refused, not wrapped.
            let mut huge = u64::MAX;
            assert_eq!(
                splice(r.as_fd(), f.as_fd(), Some(&mut huge), 1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(huge, u64::MAX);
            let _ = std::fs::remove_file(p);
        }

        #[test]
        fn splice_on_an_empty_pipe_would_block_then_sees_eof() {
            let (f, p) = scratch();
            let (r, w) = pipe_cloexec().unwrap();
            // The read end is blocking: SPLICE_F_NONBLOCK alone must keep us from waiting.
            let mut off = 0u64;
            let t0 = std::time::Instant::now();
            let e = splice(r.as_fd(), f.as_fd(), Some(&mut off), 4096).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
            assert!(t0.elapsed() < std::time::Duration::from_secs(1));
            assert_eq!(off, 0, "offset untouched on error");
            // Data, then EOF once every writer is gone.
            let mut w = std::fs::File::from(w);
            w.write_all(b"last line without newline").unwrap();
            drop(w);
            assert_eq!(splice(r.as_fd(), f.as_fd(), Some(&mut off), 4096).unwrap(), 25);
            assert_eq!(splice(r.as_fd(), f.as_fd(), Some(&mut off), 4096).unwrap(), 0, "EOF");
            assert_eq!(std::fs::read(&p).unwrap(), b"last line without newline");
            let _ = std::fs::remove_file(p);
        }

        #[test]
        fn splice_refuses_what_it_cannot_do() {
            let (f, p) = scratch();
            let (other, p2) = temp_file(b"not a pipe");
            let mut off = 0u64;
            // Input is a regular file, not a pipe: EINVAL (neither end is a pipe).
            let e = splice(other.as_fd(), f.as_fd(), Some(&mut off), 10).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EINVAL), "{e}");
            // Output opened O_APPEND: EINVAL, which is why callers track the offset.
            let appending = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
            let (r, w) = pipe_cloexec().unwrap();
            std::fs::File::from(w).write_all(b"x").unwrap();
            let e = splice(r.as_fd(), appending.as_fd(), None, 10).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EINVAL), "{e}");
            // Output not writable (opened read-only): EBADF.
            let e = splice(r.as_fd(), other.as_fd(), Some(&mut off), 10).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::EBADF), "{e}");
            // An offset for a pipe output: ESPIPE.
            let (_r2, w2) = pipe_cloexec().unwrap();
            let e = splice(r.as_fd(), w2.as_fd(), Some(&mut off), 10).unwrap_err();
            assert_eq!(e.raw_os_error(), Some(libc::ESPIPE), "{e}");
            assert_eq!(off, 0);
            // The byte is still in the pipe after all those failures.
            assert_eq!(splice(r.as_fd(), f.as_fd(), Some(&mut off), 10).unwrap(), 1);
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(p2);
        }

        #[test]
        fn splice_does_not_leak_descriptors() {
            run_isolated("splice_tests::splice_leak_probe");
        }

        #[test]
        fn splice_leak_probe() {
            if !in_probe() {
                return;
            }
            let (f, p) = scratch();
            let (other, p2) = temp_file(b"x");
            let before = open_fds();
            let mut off = 0u64;
            for i in 0..500 {
                let (r, w) = pipe_cloexec().unwrap();
                let mut w = std::fs::File::from(w);
                let _ = splice(r.as_fd(), f.as_fd(), Some(&mut off), 64); // EAGAIN
                w.write_all(b"line\n").unwrap();
                assert_eq!(splice(r.as_fd(), f.as_fd(), Some(&mut off), 64).unwrap(), 5);
                drop(w);
                assert_eq!(splice(r.as_fd(), f.as_fd(), Some(&mut off), 64).unwrap(), 0);
                assert!(splice(other.as_fd(), f.as_fd(), Some(&mut off), 64).is_err());
                assert_eq!(off, 5 * (i + 1));
            }
            assert_eq!(open_fds(), before, "every descriptor closed, success or error");
            let _ = std::fs::remove_file(p);
            let _ = std::fs::remove_file(p2);
        }
    }

    /// The kernel's own view of a listener: found by its port, with the inode
    /// its descriptor shows, and an accept queue that holds the connections
    /// nobody has accepted yet.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_diagnostics_see_a_listener_and_its_queue() {
        use std::net::{TcpListener, TcpStream};
        let inode_of = |fd: RawFd| -> u64 {
            let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
            let link = link.to_string_lossy();
            link.trim_start_matches("socket:[").trim_end_matches(']').parse().unwrap()
        };
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let inode = inode_of(l.as_raw_fd());
        let find = |port: u16| tcp_listeners().unwrap().into_iter().find(|s| s.port == port);
        let s = find(port).expect("listed");
        assert_eq!((s.inode, s.addr, s.backlog), (inode, "127.0.0.1".parse().unwrap(), 0));
        assert!(s.max_backlog >= 1, "{s:?}");
        // Two connections nobody accepts wait in the queue; one accepted leaves it.
        let _a = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let _b = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let t0 = std::time::Instant::now();
        while find(port).unwrap().backlog < 2 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "{:?}", find(port));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _accepted = l.accept().unwrap();
        assert_eq!(find(port).unwrap().backlog, 1);
        drop(l);
        assert!(find(port).is_none(), "closed: no longer listed");
        // IPv6, where the host has it.
        if let Ok(l6) = TcpListener::bind("[::1]:0") {
            let p6 = l6.local_addr().unwrap().port();
            let s6 = find(p6).expect("an IPv6 listener is listed");
            assert_eq!((s6.addr, s6.inode), ("::1".parse().unwrap(), inode_of(l6.as_raw_fd())));
        }
    }

    /// The connections established on a port, counted by the kernel's filter:
    /// those of this port and no other.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_diagnostics_count_the_connections_of_a_port() {
        use std::net::{TcpListener, TcpStream};
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let other = TcpListener::bind("127.0.0.1:0").unwrap();
        assert_eq!(tcp_connections(port).unwrap(), 0);
        let clients: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap()).collect();
        let _elsewhere = TcpStream::connect(other.local_addr().unwrap()).unwrap();
        // Each connection is two sockets on loopback: the client's, whose
        // local port is another, and the server's, whose local port is this
        // one (accepted or still in the queue: established either way).
        let t0 = std::time::Instant::now();
        while tcp_connections(port).unwrap() < 3 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5), "{}", tcp_connections(port).unwrap());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(tcp_connections(port).unwrap(), 3);
        let accepted: Vec<_> = (0..3).map(|_| l.accept().unwrap()).collect();
        assert_eq!(tcp_connections(port).unwrap(), 3, "accepted: still established");
        drop((clients, accepted));
        let t0 = std::time::Instant::now();
        while tcp_connections(port).unwrap() > 0 {
            assert!(t0.elapsed() < std::time::Duration::from_secs(5));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The parts of a dump are read with care: several messages in one part,
    /// the end, an error, and lengths that do not fit are refused.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_diagnostics_messages_are_read_with_care() {
        let msg = |kind: u16, payload: &[u8]| -> Vec<u8> {
            let len = (diag::NLMSG_HDR + payload.len()) as u32;
            let mut m = Vec::new();
            m.extend_from_slice(&len.to_ne_bytes());
            m.extend_from_slice(&kind.to_ne_bytes());
            m.extend_from_slice(&[0; 10]);
            m.extend_from_slice(payload);
            while m.len() % 4 != 0 {
                m.push(0);
            }
            m
        };
        // An inet_diag_msg of 127.0.0.1:3000 in LISTEN, 3 waiting of 128,
        // inode 12345, with its memory counters (7 drops) as an attribute.
        let mut listen = vec![0u8; diag::MSG];
        listen[0] = libc::AF_INET as u8;
        listen[1] = diag::TCP_LISTEN as u8;
        listen[4..6].copy_from_slice(&3000u16.to_be_bytes());
        listen[8..12].copy_from_slice(&[127, 0, 0, 1]);
        listen[56..60].copy_from_slice(&3u32.to_ne_bytes());
        listen[60..64].copy_from_slice(&128u32.to_ne_bytes());
        listen[68..72].copy_from_slice(&12345u32.to_ne_bytes());
        let mut meminfo = vec![0u8; 4 + 9 * 4];
        let meminfo_len = meminfo.len() as u16;
        meminfo[0..2].copy_from_slice(&meminfo_len.to_ne_bytes());
        meminfo[2..4].copy_from_slice(&diag::SKMEMINFO.to_ne_bytes());
        meminfo[4 + 8 * 4..4 + 9 * 4].copy_from_slice(&7u32.to_ne_bytes());
        listen.extend_from_slice(&meminfo);

        let part = [msg(diag::SOCK_DIAG_BY_FAMILY, &listen), msg(diag::SOCK_DIAG_BY_FAMILY, &listen)].concat();
        let mut got = Vec::new();
        assert!(!diag_messages(&part, &mut |m| got.extend(parse_tcp_listen(m))).unwrap(), "more to come");
        let want = TcpListen {
            inode: 12345,
            addr: "127.0.0.1".parse().unwrap(),
            port: 3000,
            backlog: 3,
            max_backlog: 128,
            drops: 7,
        };
        assert_eq!(got, vec![want.clone(), want]);
        let done = [msg(diag::SOCK_DIAG_BY_FAMILY, &listen), msg(diag::NLMSG_DONE, &[0; 4])].concat();
        assert!(diag_messages(&done, &mut |_| {}).unwrap(), "done");
        let err = msg(diag::NLMSG_ERROR, &(-libc::EPERM).to_ne_bytes());
        assert_eq!(diag_messages(&err, &mut |_| {}).unwrap_err().raw_os_error(), Some(libc::EPERM));
        let mut cut = msg(diag::SOCK_DIAG_BY_FAMILY, &listen);
        cut[0..4].copy_from_slice(&1000u32.to_ne_bytes());
        assert_eq!(diag_messages(&cut, &mut |_| {}).unwrap_err().kind(), io::ErrorKind::InvalidData);
        // A message too short to be one, or of another family, is no listener.
        assert_eq!(parse_tcp_listen(&listen[..40]), None);
        let mut unix = listen.clone();
        unix[0] = libc::AF_UNIX as u8;
        assert_eq!(parse_tcp_listen(&unix), None);
        // Without the attribute, no drops; a cut attribute is ignored.
        assert_eq!(parse_tcp_listen(&listen[..diag::MSG]).unwrap().drops, 0);
        assert_eq!(parse_tcp_listen(&listen[..diag::MSG + 10]).unwrap().drops, 0);
    }

    /// The kernel's Unix listeners: a named one (path and abstract) with its
    /// inode and its name as the kernel keeps it; a bound socket that does
    /// not listen, and a connection, are not listed.
    #[cfg(target_os = "linux")]
    #[test]
    fn socket_diagnostics_see_a_unix_listener_and_its_name() {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::{SocketAddr, UnixDatagram, UnixListener, UnixStream};
        let inode_of = |fd: RawFd| -> u64 {
            let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
            let link = link.to_string_lossy();
            link.trim_start_matches("socket:[").trim_end_matches(']').parse().unwrap()
        };
        let dir = std::env::temp_dir().join(format!("wsd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("l.sock");
        let l = UnixListener::bind(&path).unwrap();
        let name = format!("warden-test-{}", std::process::id());
        let abs = UnixListener::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap()).unwrap();
        let dgram = UnixDatagram::bind(dir.join("d.sock")).unwrap();
        let client = UnixStream::connect(&path).unwrap();
        let found = unix_listeners();
        let _ = std::fs::remove_dir_all(&dir);
        // A kernel without unix_diag (a minimal VM's) has no such table: ENOENT.
        let found = match found {
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return,
            found => found.unwrap(),
        };
        let by_inode = |fd: RawFd| found.iter().find(|s| s.inode == inode_of(fd)).cloned();
        let mut want = path.as_os_str().as_encoded_bytes().to_vec();
        want.push(0);
        assert_eq!(by_inode(l.as_raw_fd()).unwrap().name, Some(want), "a path ends with its NUL");
        let mut want = vec![0];
        want.extend_from_slice(name.as_bytes());
        assert_eq!(by_inode(abs.as_raw_fd()).unwrap().name, Some(want), "an abstract name starts with one");
        assert_eq!(by_inode(dgram.as_raw_fd()), None, "bound, not listening");
        assert_eq!(by_inode(client.as_raw_fd()), None, "a connection");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn socket_diagnostics_unix_messages_are_read_with_care() {
        assert_eq!(&unix_diag_request(1 << 10, 1)[16..32], &[1, 0, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0]);
        // A unix_diag_msg of inode 4242 with its name, then another attribute.
        let mut m = vec![0u8; diag::UNIX_MSG];
        m[0] = libc::AF_UNIX as u8;
        m[4..8].copy_from_slice(&4242u32.to_ne_bytes());
        let attr = |kind: u16, data: &[u8]| {
            let mut a = Vec::new();
            a.extend_from_slice(&((4 + data.len()) as u16).to_ne_bytes());
            a.extend_from_slice(&kind.to_ne_bytes());
            a.extend_from_slice(data);
            while a.len() % 4 != 0 {
                a.push(0);
            }
            a
        };
        let bare = m.clone();
        m.extend_from_slice(&attr(diag::UNIX_DIAG_NAME, b"/run/a.sock\0"));
        m.extend_from_slice(&attr(5, &[1, 2, 3, 4]));
        assert_eq!(parse_unix_listen(&m), Some(UnixListen { inode: 4242, name: Some(b"/run/a.sock\0".to_vec()) }));
        assert_eq!(parse_unix_listen(&bare), Some(UnixListen { inode: 4242, name: None }), "unnamed");
        let mut cut = bare.clone();
        cut.extend_from_slice(&attr(diag::UNIX_DIAG_NAME, b"/run/a.sock\0")[..8]);
        assert_eq!(parse_unix_listen(&cut).unwrap().name, None, "a cut attribute is ignored");
        assert_eq!(parse_unix_listen(&bare[..8]), None, "too short");
        let mut inet = bare;
        inet[0] = libc::AF_INET as u8;
        assert_eq!(parse_unix_listen(&inet), None);
    }
}

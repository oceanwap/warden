//! macOS: the libproc, sysctl and Mach calls behind `platform::macos`, as
//! safe functions (the policy in `sys.rs` holds here too). Everything is
//! read-only: they look at processes and the host, they change nothing.
//!
//! Output buffers are plain byte arrays and the fields are read at offsets
//! (`offset_of!` on the `libc` struct where it has one), so there is no
//! `zeroed`, no `transmute` and no reference into memory the kernel wrote.
//! The one structure `libc` lacks, `socket_fdinfo`, has its offsets worked
//! out below from `sys/proc_info.h` and checked at run time against the size
//! the kernel returns: if a future macOS changed the layout the call fails
//! with an error, it does not read the wrong field.
//!
//! Processes of other users answer EPERM (EINVAL for `KERN_PROCARGS2`, as for
//! a process that does not exist): callers get an `io::Error` and treat the
//! process as unreadable.

use std::ffi::{OsString, c_int, c_void};
use std::io;
use std::mem::{offset_of, size_of};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::OnceLock;

/// `PROC_PIDFDSOCKETINFO` (`sys/proc_info.h`): `libc` has the other flavors.
const PROC_PIDFDSOCKETINFO: c_int = 3;
/// `PROC_ALL_PIDS`.
const PROC_ALL_PIDS: u32 = 1;
/// `SOCKINFO_TCP` and `TSI_S_LISTEN`.
const SOCKINFO_TCP: i32 = 2;
const TSI_S_LISTEN: i32 = 1;

// struct socket_fdinfo { struct proc_fileinfo pfi (24 bytes);
//                        struct socket_info psi; }
// struct socket_info   { struct vinfo_stat soi_stat (136); u64 soi_so; u64 soi_pcb;
//                        int soi_type, soi_protocol, soi_family; 8 shorts;
//                        u32 soi_oobmark; 2 × sockbuf_info (24); int soi_kind;
//                        u32 rfu_1; union soi_proto (8-aligned, 528 bytes) }
// so soi_kind is at 24 + 232, soi_proto at 24 + 240, and in the tcp_sockinfo
// at its start: struct in_sockinfo { int insi_fport; int insi_lport; ... } (80
// bytes), then `int tcpsi_state`.
const SOCKET_FDINFO_SIZE: usize = 792;
const SOI_KIND: usize = 24 + 232;
const SOI_PROTO: usize = 24 + 240;
const IN_SOCKINFO_SIZE: usize = 80;
const INSI_LPORT: usize = SOI_PROTO + 4;
const TCPSI_STATE: usize = SOI_PROTO + IN_SOCKINFO_SIZE;

fn ne_u32(buf: &[u8], at: usize) -> u32 {
    buf.get(at..at + 4).and_then(|b| b.try_into().ok()).map(u32::from_ne_bytes).unwrap_or(0)
}

fn ne_u64(buf: &[u8], at: usize) -> u64 {
    buf.get(at..at + 8).and_then(|b| b.try_into().ok()).map(u64::from_ne_bytes).unwrap_or(0)
}

/// `proc_pidinfo` into `buf`: the bytes the kernel wrote. 0 means the call
/// failed (no such process, not ours): the OS error.
fn pidinfo(pid: u32, flavor: c_int, buf: &mut [u8]) -> io::Result<usize> {
    let pid = c_int::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    let len = c_int::try_from(buf.len()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: `buf` is a valid, exclusively borrowed byte slice of `len`
    // bytes, which proc_pidinfo may write up to; the other arguments are
    // integers. It does not keep the pointer.
    let n = unsafe { libc::proc_pidinfo(pid, flavor, 0, buf.as_mut_ptr().cast::<c_void>(), len) };
    if n <= 0 { Err(io::Error::last_os_error()) } else { Ok(n as usize) }
}

/// A flavor whose result must fill the whole struct.
fn pidinfo_exact(pid: u32, flavor: c_int, buf: &mut [u8]) -> io::Result<()> {
    let n = pidinfo(pid, flavor, buf)?;
    if n < buf.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("proc_pidinfo returned {n} of {} bytes", buf.len()),
        ));
    }
    Ok(())
}

/// Mach absolute time units to nanoseconds: 1/1 on Intel, 125/3 on Apple
/// Silicon. `pti_total_user` and `pti_total_system` are in these units.
#[allow(deprecated)] // libc points to the mach2 crate; this is one call
fn timebase() -> (u64, u64) {
    static V: OnceLock<(u64, u64)> = OnceLock::new();
    *V.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: `tb` is a valid, exclusively borrowed mach_timebase_info.
        let rc = unsafe { libc::mach_timebase_info(&mut tb) };
        if rc == 0 && tb.numer > 0 && tb.denom > 0 { (u64::from(tb.numer), u64::from(tb.denom)) } else { (1, 1) }
    })
}

/// A process's memory and CPU time.
#[derive(Debug, Clone, Copy)]
pub struct TaskInfo {
    pub resident_bytes: u64,
    /// User plus system CPU time, in nanoseconds.
    pub cpu_ns: u64,
}

/// `proc_pidinfo(PROC_PIDTASKINFO)`.
pub fn task_info(pid: u32) -> io::Result<TaskInfo> {
    let mut buf = [0u8; size_of::<libc::proc_taskinfo>()];
    pidinfo_exact(pid, libc::PROC_PIDTASKINFO, &mut buf)?;
    let resident_bytes = ne_u64(&buf, offset_of!(libc::proc_taskinfo, pti_resident_size));
    let ticks = ne_u64(&buf, offset_of!(libc::proc_taskinfo, pti_total_user))
        .saturating_add(ne_u64(&buf, offset_of!(libc::proc_taskinfo, pti_total_system)));
    let (numer, denom) = timebase();
    // u128: the product of ticks and 125 can pass u64 after a few years of CPU time.
    let cpu_ns = (u128::from(ticks) * u128::from(numer) / u128::from(denom)).min(u128::from(u64::MAX)) as u64;
    Ok(TaskInfo { resident_bytes, cpu_ns })
}

/// The user id a process runs as (its effective user):
/// `proc_pidinfo(PROC_PIDT_SHORTBSDINFO)`, which, unlike the full
/// `PROC_PIDTBSDINFO`, answers for other users' processes too.
pub fn owner(pid: u32) -> io::Result<u32> {
    let mut buf = [0u8; size_of::<libc::proc_bsdshortinfo>()];
    pidinfo_exact(pid, libc::PROC_PIDT_SHORTBSDINFO, &mut buf)?;
    Ok(ne_u32(&buf, offset_of!(libc::proc_bsdshortinfo, pbsi_uid)))
}

/// The working directory: `proc_pidinfo(PROC_PIDVNODEPATHINFO)`.
pub fn cwd(pid: u32) -> io::Result<PathBuf> {
    let mut buf = vec![0u8; size_of::<libc::proc_vnodepathinfo>()];
    pidinfo_exact(pid, libc::PROC_PIDVNODEPATHINFO, &mut buf)?;
    let at = offset_of!(libc::proc_vnodepathinfo, pvi_cdir) + offset_of!(libc::vnode_info_path, vip_path);
    let path = &buf[at..at + libc::MAXPATHLEN as usize];
    let path = path.split(|b| *b == 0).next().unwrap_or(&[]);
    if path.is_empty() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "the process has no working directory"));
    }
    Ok(PathBuf::from(OsString::from_vec(path.to_vec())))
}

/// The most bytes the kernel keeps of one process's arguments and
/// environment (`kern.argmax`, cached).
fn arg_max() -> usize {
    static V: OnceLock<usize> = OnceLock::new();
    *V.get_or_init(|| match sysctl_by_name::<4>(c"kern.argmax") {
        Ok(raw) => (u32::from_ne_bytes(raw) as usize).clamp(4096, 16 << 20),
        Err(_) => 1 << 20,
    })
}

/// `KERN_PROCARGS2`: argc, the executable path, argv and the environment as
/// the kernel kept them at exec (see `platform::macos::parse_procargs`).
pub fn procargs(pid: u32) -> io::Result<Vec<u8>> {
    let mut mib = [
        libc::CTL_KERN,
        libc::KERN_PROCARGS2,
        c_int::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?,
    ];
    // Zeroed memory the allocator takes lazily: only the pages the kernel
    // writes are ever touched.
    let mut buf = vec![0u8; arg_max()];
    let mut len: libc::size_t = buf.len();
    // SAFETY: `mib` is a valid array of 3 ints; `buf` is a valid byte vector
    // of `len` bytes that the kernel may write up to, and `len` is updated
    // to the bytes written; no new value is passed.
    let rc = unsafe {
        libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast::<c_void>(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(len);
    Ok(buf)
}

/// Every process id the caller may list.
pub fn all_pids() -> io::Result<Vec<u32>> {
    // SAFETY: a null buffer of size 0 only asks how many bytes a list needs.
    let need = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if need <= 0 {
        return Err(io::Error::last_os_error());
    }
    // Processes start while we ask: room for some more.
    let mut buf = vec![0u8; need as usize + 64 * size_of::<c_int>()];
    // SAFETY: `buf` is a valid byte vector of buf.len() bytes, which
    // proc_listpids writes pids (ints) into up to that size.
    let n = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, buf.as_mut_ptr().cast::<c_void>(), buf.len() as c_int) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(buf[..n as usize]
        .chunks_exact(size_of::<c_int>())
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .filter(|p| *p != 0)
        .collect())
}

/// The command name of a process (`proc_name`: up to 32 characters).
pub fn name(pid: u32) -> io::Result<String> {
    let mut buf = [0u8; 64];
    // SAFETY: `buf` is a valid byte array of 64 bytes, which proc_name may
    // write up to (it NUL-terminates inside it).
    let n = unsafe { libc::proc_name(pid as c_int, buf.as_mut_ptr().cast::<c_void>(), buf.len() as u32) };
    if n <= 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(String::from_utf8_lossy(&buf[..(n as usize).min(buf.len())]).into_owned())
}

/// The ports of the TCP sockets this process listens on, one entry per
/// socket (a socket on 0.0.0.0 and one on :: are two).
pub fn listening_tcp_ports(pid: u32) -> io::Result<Vec<u16>> {
    // First ask how large the descriptor list is, then read it with room for
    // descriptors opened in between.
    let ipid = c_int::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::ESRCH))?;
    // SAFETY: a null buffer of size 0 only asks for the size.
    let need = unsafe { libc::proc_pidinfo(ipid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    // libproc answers 0 (not -1) when it fails: a process that is gone or not
    // ours. Every live process has descriptors, so 0 is an error here too.
    if need <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut list = vec![0u8; need as usize + 32 * size_of::<libc::proc_fdinfo>()];
    let n = pidinfo(pid, libc::PROC_PIDLISTFDS, &mut list)?;
    let step = size_of::<libc::proc_fdinfo>();
    let (fd_at, type_at) = (offset_of!(libc::proc_fdinfo, proc_fd), offset_of!(libc::proc_fdinfo, proc_fdtype));
    let mut ports = Vec::new();
    for entry in list[..n].chunks_exact(step) {
        if ne_u32(entry, type_at) != libc::PROX_FDTYPE_SOCKET as u32 {
            continue;
        }
        let fd = ne_u32(entry, fd_at) as c_int;
        // Room to spare: the kernel refuses a buffer smaller than its struct
        // (ENOMEM), which would look like a closed descriptor. A larger struct
        // in a later macOS then shows as a size mismatch below, not as no ports.
        let mut info = [0u8; 2 * SOCKET_FDINFO_SIZE];
        // SAFETY: `info` is a valid byte array of its length, which
        // proc_pidfdinfo may write up to; the rest are integers.
        let got = unsafe {
            libc::proc_pidfdinfo(
                ipid,
                fd,
                PROC_PIDFDSOCKETINFO,
                info.as_mut_ptr().cast::<c_void>(),
                info.len() as c_int,
            )
        };
        if got <= 0 {
            continue; // closed since the list was read
        }
        if got as usize != SOCKET_FDINFO_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("proc_pidfdinfo returned {got} bytes for a socket, expected {SOCKET_FDINFO_SIZE}"),
            ));
        }
        if ne_u32(&info, SOI_KIND) as i32 == SOCKINFO_TCP && ne_u32(&info, TCPSI_STATE) as i32 == TSI_S_LISTEN {
            // The port sits in the low 16 bits, in network byte order.
            ports.push(u16::from_be(ne_u32(&info, INSI_LPORT) as u16));
        }
    }
    Ok(ports)
}

/// The Mach port of this host (one send right, cached: asking again would
/// take another right each time).
fn host_port() -> u32 {
    static V: OnceLock<u32> = OnceLock::new();
    #[allow(deprecated)] // libc points to the mach2 crate; this is one call
    *V.get_or_init(|| {
        // SAFETY: mach_host_self takes no arguments and returns a port name.
        unsafe { libc::mach_host_self() }
    })
}

/// The kernel's 32-bit CPU tick counters, one per state: user, system,
/// idle, nice. They wrap (about every 50 days on ten cores); the caller
/// extends them to 64 bits (`platform::counter`).
pub fn host_cpu_ticks() -> io::Result<[u32; 4]> {
    let mut ticks = [0i32; libc::CPU_STATE_MAX as usize];
    let mut count = libc::HOST_CPU_LOAD_INFO_COUNT;
    // SAFETY: `ticks` is a valid array of CPU_STATE_MAX ints (the size of
    // host_cpu_load_info) and `count` says so: HOST_CPU_LOAD_INFO_COUNT ints.
    let rc = unsafe { libc::host_statistics(host_port(), libc::HOST_CPU_LOAD_INFO, ticks.as_mut_ptr(), &mut count) };
    if rc != 0 {
        return Err(io::Error::other(format!("host_statistics(HOST_CPU_LOAD_INFO) failed: {rc}")));
    }
    let t = |state: c_int| ticks[state as usize] as u32;
    Ok([t(libc::CPU_STATE_USER), t(libc::CPU_STATE_SYSTEM), t(libc::CPU_STATE_IDLE), t(libc::CPU_STATE_NICE)])
}

/// Memory in use the way Activity Monitor counts it: app memory (anonymous
/// pages less the purgeable ones), wired and compressed.
pub fn memory_used_bytes() -> io::Result<u64> {
    const INTS: usize = size_of::<libc::vm_statistics64>() / size_of::<c_int>();
    let mut v = [0i32; INTS];
    let mut count = INTS as u32;
    // SAFETY: `v` is a valid array of INTS ints, which is the size of
    // vm_statistics64, and `count` says so.
    let rc = unsafe { libc::host_statistics64(host_port(), libc::HOST_VM_INFO64, v.as_mut_ptr(), &mut count) };
    if rc != 0 {
        return Err(io::Error::other(format!("host_statistics64(HOST_VM_INFO64) failed: {rc}")));
    }
    let bytes: Vec<u8> = v.iter().flat_map(|i| i.to_ne_bytes()).collect();
    let field = |at: usize| u64::from(ne_u32(&bytes, at));
    let internal = field(offset_of!(libc::vm_statistics64, internal_page_count));
    let purgeable = field(offset_of!(libc::vm_statistics64, purgeable_count));
    let wired = field(offset_of!(libc::vm_statistics64, wire_count));
    let compressed = field(offset_of!(libc::vm_statistics64, compressor_page_count));
    let pages = internal.saturating_sub(purgeable) + wired + compressed;
    Ok(pages * super::page_size())
}

/// `sysctlbyname` for a value of exactly `N` bytes.
fn sysctl_by_name<const N: usize>(name: &std::ffi::CStr) -> io::Result<[u8; N]> {
    let mut out = [0u8; N];
    let mut len: libc::size_t = N;
    // SAFETY: `name` is a valid NUL-terminated string; `out` is a valid byte
    // array of `len` bytes that the kernel may write up to, and `len` is
    // updated; no new value is passed.
    let rc = unsafe {
        libc::sysctlbyname(name.as_ptr(), out.as_mut_ptr().cast::<c_void>(), &mut len, std::ptr::null_mut(), 0)
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if len != N {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("{name:?} is {len} bytes, expected {N}")));
    }
    Ok(out)
}

/// Physical memory (`hw.memsize`).
pub fn memory_total_bytes() -> io::Result<u64> {
    sysctl_by_name::<8>(c"hw.memsize").map(u64::from_ne_bytes)
}

/// What identifies this boot: `kern.bootsessionuuid`, a UUID the kernel
/// makes at boot and keeps (`kern.boottime` moves when the clock is set).
pub fn boot_session_uuid() -> io::Result<String> {
    let mut out = [0u8; 64];
    let mut len: libc::size_t = out.len();
    // SAFETY: the name is a valid NUL-terminated string; `out` is a valid
    // byte array of `len` bytes that the kernel may write up to, and `len`
    // is updated; no new value is passed.
    let rc = unsafe {
        libc::sysctlbyname(
            c"kern.bootsessionuuid".as_ptr(),
            out.as_mut_ptr().cast::<c_void>(),
            &mut len,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    let text = out.get(..len.min(out.len())).unwrap_or(&[]);
    let text = text.split(|b| *b == 0).next().unwrap_or(&[]);
    let id = String::from_utf8_lossy(text).trim().to_string();
    if id.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "kern.bootsessionuuid is empty"));
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn me() -> u32 {
        std::process::id()
    }

    /// `sh -c 'exec sleep 60'` in `dir` with VAR set: killed when dropped.
    struct Sleeper(std::process::Child);

    impl Sleeper {
        fn start(dir: &std::path::Path) -> Sleeper {
            let child = Command::new("sh")
                .args(["-c", "exec sleep 60"])
                .env("WARDEN_DARWIN_TEST", "hello world")
                .current_dir(dir)
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let s = Sleeper(child);
            // Past the exec: the command name is `sleep`.
            let t0 = Instant::now();
            while t0.elapsed() < Duration::from_secs(5) && name(s.0.id()).ok().as_deref() != Some("sleep") {
                std::thread::sleep(Duration::from_millis(10));
            }
            s
        }
    }

    impl Drop for Sleeper {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn socket_fdinfo_offsets_are_the_kernels() {
        // The layout worked out from sys/proc_info.h: 24 + 240 + 528 bytes.
        assert_eq!(SOI_PROTO + 528, SOCKET_FDINFO_SIZE);
        assert_eq!(SOI_KIND, 256);
        assert_eq!(INSI_LPORT, 268);
        assert_eq!(TCPSI_STATE, 344);
    }

    #[test]
    fn task_info_reports_memory_and_cpu_time_in_real_units() {
        let t = task_info(me()).unwrap();
        assert!(t.resident_bytes > 1 << 20, "{t:?}");
        // A child that spins for a second has used about a second of CPU:
        // wrong units (mach ticks as nanoseconds) would be 40 times off.
        let mut spin = Command::new("sh").args(["-c", "while :; do :; done"]).stdin(Stdio::null()).spawn().unwrap();
        std::thread::sleep(Duration::from_millis(1000));
        let c = task_info(spin.id()).unwrap();
        let _ = spin.kill();
        let _ = spin.wait();
        let secs = c.cpu_ns as f64 / 1e9;
        assert!((0.3..2.0).contains(&secs), "{secs} s of CPU after 1 s of spinning");
    }

    #[test]
    fn task_info_of_a_missing_process_is_an_error() {
        assert!(task_info(0x7fff_fff0).is_err());
        assert!(owner(0x7fff_fff0).is_err());
        assert!(cwd(0x7fff_fff0).is_err());
        assert!(procargs(0x7fff_fff0).is_err());
        assert!(listening_tcp_ports(0x7fff_fff0).is_err());
    }

    #[test]
    fn the_owner_of_this_process_and_of_a_child_is_the_effective_user() {
        assert_eq!(owner(me()).unwrap(), crate::sys::euid());
        let s = Sleeper::start(&std::env::temp_dir());
        assert_eq!(owner(s.0.id()).unwrap(), crate::sys::euid());
    }

    #[test]
    fn cwd_and_environment_of_a_child() {
        let dir = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let s = Sleeper::start(&dir);
        assert_eq!(cwd(s.0.id()).unwrap(), dir);
        let raw = procargs(s.0.id()).unwrap();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("WARDEN_DARWIN_TEST=hello world"), "{text:?}");
        assert!(text.contains("sleep"), "{text:?}");
    }

    #[test]
    fn listening_ports_are_this_processes_own_listeners() {
        let a = TcpListener::bind("127.0.0.1:0").unwrap();
        let b = TcpListener::bind("[::1]:0").ok();
        let pa = a.local_addr().unwrap().port();
        let ports = listening_tcp_ports(me()).unwrap();
        assert!(ports.contains(&pa), "{ports:?} should have {pa}");
        if let Some(b) = &b {
            assert!(ports.contains(&b.local_addr().unwrap().port()), "{ports:?}");
        }
        // A connected socket is not a listener.
        let c = std::net::TcpStream::connect(a.local_addr().unwrap()).unwrap();
        let (_peer, _) = a.accept().unwrap();
        let after = listening_tcp_ports(me()).unwrap();
        assert_eq!(after.iter().filter(|p| **p == pa).count(), 1, "{after:?}");
        drop(c);
        drop(a);
        assert!(!listening_tcp_ports(me()).unwrap().contains(&pa));
    }

    #[test]
    fn the_process_list_has_this_process_and_its_name() {
        let pids = all_pids().unwrap();
        assert!(pids.contains(&me()) && pids.contains(&1), "{} pids", pids.len());
        assert!(!name(me()).unwrap().is_empty());
    }

    #[test]
    fn host_numbers_are_plausible() {
        let t = host_cpu_ticks().unwrap();
        assert!(t.iter().map(|x| u64::from(*x)).sum::<u64>() > 0, "{t:?}");
        let (total_mem, used) = (memory_total_bytes().unwrap(), memory_used_bytes().unwrap());
        assert!(total_mem >= 1 << 30, "{total_mem}");
        assert!(used > 0 && used <= total_mem, "{used} of {total_mem}");
        let id = boot_session_uuid().unwrap();
        assert_eq!(id.len(), 36, "{id}");
        assert_eq!(boot_session_uuid().unwrap(), id);
    }
}

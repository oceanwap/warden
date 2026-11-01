//! `[balance]` (Linux): the kernel spreads the connections to an app's own
//! address and public port (`address:443`) over its workers, which each
//! listen on a port of their own (`port_strategy = "offset"`). Nothing sits
//! in the path: a BPF `sk_lookup` program, attached to this network
//! namespace, picks one worker's listening socket for every new connection,
//! by a hash of the visitor's address and port, and the connection lands in
//! that worker's accept queue as if it had been made to the worker's port.
//! The worker sees the visitor's own address, and no byte is copied.
//!
//! Raw bpf(2), no libbpf and no C compiler. Needs Linux 5.9+ and root (or
//! CAP_BPF + CAP_NET_ADMIN); `attach` fails otherwise and the caller says so.
//!
//! The sockmap holds each worker's listener while the worker lives: the
//! kernel drops an entry when the socket closes, so a dead worker leaves the
//! map by itself and the program tries the next slot (`TRIES`). Warden
//! empties a slot when it starts to stop a worker, so a draining worker gets
//! no new connections.

use std::io;
use std::net::IpAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

const BPF_MAP_CREATE: libc::c_long = 0;
const BPF_MAP_UPDATE_ELEM: libc::c_long = 2;
const BPF_MAP_DELETE_ELEM: libc::c_long = 3;
const BPF_PROG_LOAD: libc::c_long = 5;
const BPF_LINK_CREATE: libc::c_long = 28;
const MAP_TYPE_SOCKMAP: u32 = 15;
const PROG_TYPE_SK_LOOKUP: u32 = 30;
const ATTACH_SK_LOOKUP: u32 = 36;
/// `BPF_SK_LOOKUP_F_NO_REUSEPORT`: take exactly this socket. A draining and
/// a new worker may listen on the same port during a rollout; the kernel's
/// own reuseport pick must not choose between them.
const SK_ASSIGN_NO_REUSEPORT: i32 = 2;
/// Slots tried (starting at the hash) before the connection is left to the
/// ordinary lookup: more than this many workers dead at once is an outage
/// that a refused connection reports honestly.
pub(crate) const TRIES: usize = 16;
/// The most workers one app balances over.
pub(crate) const MAX_SLOTS: usize = 256;

/// One instruction of an eBPF program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Insn {
    code: u8,
    dst: u8,
    src: u8,
    off: i16,
    imm: i32,
}

impl Insn {
    const fn new(code: u8, dst: u8, src: u8, off: i16, imm: i32) -> Insn {
        Insn { code, dst, src, off, imm }
    }

    fn bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0] = self.code;
        b[1] = self.dst | (self.src << 4);
        b[2..4].copy_from_slice(&self.off.to_le_bytes());
        b[4..8].copy_from_slice(&self.imm.to_le_bytes());
        b
    }
}

const R1: u8 = 1;
const R2: u8 = 2;
const R3: u8 = 3;
const R4: u8 = 4;
const R5: u8 = 5;
const R6: u8 = 6;
const R7: u8 = 7;
const R8: u8 = 8;
const FP: u8 = 10;

fn mov(dst: u8, src: u8) -> Insn {
    Insn::new(0xbf, dst, src, 0, 0)
}
fn mov_imm(dst: u8, imm: i32) -> Insn {
    Insn::new(0xb7, dst, 0, 0, imm)
}
/// `dst = *(u32 *)(src + off)`
fn ldx32(dst: u8, src: u8, off: i16) -> Insn {
    Insn::new(0x61, dst, src, off, 0)
}
/// `dst = *(u16 *)(src + off)`
fn ldx16(dst: u8, src: u8, off: i16) -> Insn {
    Insn::new(0x69, dst, src, off, 0)
}
fn call(helper: i32) -> Insn {
    Insn::new(0x85, 0, 0, 0, helper)
}
const EXIT: Insn = Insn::new(0x95, 0, 0, 0, 0);

/// Jumps are written with a target label and fixed up in [`assemble`].
enum Item {
    I(Insn),
    /// Two slots: `dst = map fd` (BPF_PSEUDO_MAP_FD).
    LdMap(u8),
    /// A jump (`code`, `dst`, `imm` as in the instruction) to a label.
    Jump(u8, u8, i32, usize),
    Label(usize),
}

const PASS: usize = usize::MAX;

fn assemble(items: &[Item], map_fd: RawFd) -> Vec<Insn> {
    let size = |it: &Item| match it {
        Item::LdMap(_) => 2,
        Item::Label(_) => 0,
        _ => 1,
    };
    let mut at = std::collections::HashMap::new();
    let mut n = 0usize;
    for it in items {
        if let Item::Label(l) = it {
            at.insert(*l, n);
        }
        n += size(it);
    }
    let mut out = Vec::with_capacity(n);
    for it in items {
        match it {
            Item::I(i) => out.push(*i),
            Item::LdMap(dst) => {
                out.push(Insn::new(0x18, *dst, 1, 0, map_fd));
                out.push(Insn::new(0, 0, 0, 0, 0));
            }
            Item::Jump(code, dst, imm, label) => {
                let target = *at.get(label).expect("jump to a label that exists");
                out.push(Insn::new(*code, *dst, 0, (target as i64 - out.len() as i64 - 1) as i16, *imm));
            }
            Item::Label(_) => {}
        }
    }
    out
}

const JNE_IMM: u8 = 0x55;
const JEQ_IMM: u8 = 0x15;
const JA: u8 = 0x05;

/// The `sk_lookup` program: connections to `addr:port` go to one of `slots`
/// sockets in the sockmap `map_fd`; everything else is left alone
/// (`SK_PASS` without a selection).
///
/// The context (`struct bpf_sk_lookup`): family at 8, remote_ip4 at 16,
/// remote_ip6 at 20, remote_port at 36 (network order), local_ip4 at 40,
/// local_ip6 at 44, local_port at 60 (host order).
pub(crate) fn program(addr: IpAddr, port: u16, slots: usize, map_fd: RawFd) -> Vec<Insn> {
    assert!((1..=MAX_SLOTS).contains(&slots), "1 to {MAX_SLOTS} slots");
    let mut p = vec![Item::I(mov(R6, R1)), Item::I(ldx32(R2, R6, 60)), Item::Jump(JNE_IMM, R2, i32::from(port), PASS)];
    match addr {
        IpAddr::V4(a) => {
            p.extend([
                Item::I(ldx32(R2, R6, 8)),
                Item::Jump(JNE_IMM, R2, libc::AF_INET, PASS),
                Item::I(ldx32(R2, R6, 40)),
                Item::Jump(JNE_IMM, R2, i32::from_ne_bytes(a.octets()), PASS),
                Item::I(ldx32(R7, R6, 16)),
            ]);
        }
        IpAddr::V6(a) => {
            p.extend([Item::I(ldx32(R2, R6, 8)), Item::Jump(JNE_IMM, R2, libc::AF_INET6, PASS)]);
            for (i, word) in a.octets().chunks_exact(4).enumerate() {
                let w = i32::from_ne_bytes([word[0], word[1], word[2], word[3]]);
                p.extend([Item::I(ldx32(R2, R6, 44 + 4 * i as i16)), Item::Jump(JNE_IMM, R2, w, PASS)]);
            }
            p.push(Item::I(ldx32(R7, R6, 32)));
        }
    }
    // hash = ((ip * 2654435761) ^ (port * 40503)) >> 16, 32-bit
    p.extend([
        Item::I(ldx16(R4, R6, 36)),
        Item::I(Insn::new(0x24, R7, 0, 0, 2654435761u32 as i32)),
        Item::I(Insn::new(0x24, R4, 0, 0, 40503)),
        Item::I(Insn::new(0xac, R7, R4, 0, 0)),
        Item::I(Insn::new(0x74, R7, 0, 0, 16)),
    ]);
    for k in 0..slots.min(TRIES) {
        let (next, assign) = (2 * k, 2 * k + 1);
        p.extend([
            Item::I(mov(R3, R7)),
            Item::I(Insn::new(0x04, R3, 0, 0, k as i32)),
            Item::I(Insn::new(0x94, R3, 0, 0, slots as i32)),
            Item::I(Insn::new(0x63, FP, R3, -4, 0)),
            Item::LdMap(R1),
            Item::I(mov(R2, FP)),
            Item::I(Insn::new(0x07, R2, 0, 0, -4)),
            Item::I(call(1)), // bpf_map_lookup_elem
            Item::Jump(JEQ_IMM, 0, 0, next),
            Item::I(mov(R8, 0)),
            Item::I(mov(R1, R6)),
            Item::I(mov(R2, R8)),
            Item::I(mov_imm(R3, SK_ASSIGN_NO_REUSEPORT)),
            Item::I(call(124)), // bpf_sk_assign
            Item::I(mov(R1, R8)),
            Item::I(call(86)), // bpf_sk_release
            Item::Jump(JA, 0, 0, PASS),
            Item::Label(next),
        ]);
        let _ = assign;
    }
    p.extend([Item::Label(PASS), Item::I(mov_imm(0, 1)), Item::I(EXIT)]);
    // R5 is only here to keep the register list complete for readers.
    let _ = R5;
    assemble(&p, map_fd)
}

/// A `bpf_attr` as the bytes the kernel reads.
struct Attr([u8; 128]);

impl Attr {
    fn new() -> Attr {
        Attr([0; 128])
    }
    fn u32(mut self, off: usize, v: u32) -> Attr {
        self.0[off..off + 4].copy_from_slice(&v.to_ne_bytes());
        self
    }
    fn u64(mut self, off: usize, v: u64) -> Attr {
        self.0[off..off + 8].copy_from_slice(&v.to_ne_bytes());
        self
    }
    fn object(mut self, cmd: libc::c_long) -> io::Result<OwnedFd> {
        crate::sys::bpf(cmd, &mut self.0)
    }
    fn run(mut self, cmd: libc::c_long) -> io::Result<()> {
        crate::sys::bpf_cmd(cmd, &mut self.0)
    }
}

/// The attached program: dropping it detaches the program.
pub(crate) struct Steer {
    _link: OwnedFd,
    _prog: OwnedFd,
    map: OwnedFd,
    slots: usize,
}

impl Steer {
    /// Attach the program for `addr:port` with room for `slots` workers.
    pub(crate) fn attach(addr: IpAddr, port: u16, slots: usize) -> io::Result<Steer> {
        let map =
            Attr::new().u32(0, MAP_TYPE_SOCKMAP).u32(4, 4).u32(8, 4).u32(12, slots as u32).object(BPF_MAP_CREATE)?;
        let code: Vec<u8> = program(addr, port, slots, map.as_raw_fd()).iter().flat_map(|i| i.bytes()).collect();
        let license = b"GPL\0";
        let mut log = vec![0u8; 1 << 16];
        let prog = Attr::new()
            .u32(0, PROG_TYPE_SK_LOOKUP)
            .u32(4, (code.len() / 8) as u32)
            .u64(8, code.as_ptr() as u64)
            .u64(16, license.as_ptr() as u64)
            .u32(24, 1)
            .u32(28, log.len() as u32)
            .u64(32, log.as_mut_ptr() as u64)
            .u32(68, ATTACH_SK_LOOKUP)
            .object(BPF_PROG_LOAD)
            .map_err(|e| {
                let end = log.iter().position(|b| *b == 0).unwrap_or(0);
                io::Error::new(
                    e.kind(),
                    format!("loading the sk_lookup program: {e}: {}", String::from_utf8_lossy(&log[..end])),
                )
            })?;
        let netns = std::fs::File::open("/proc/self/ns/net")?;
        let link = Attr::new()
            .u32(0, prog.as_raw_fd() as u32)
            .u32(4, netns.as_raw_fd() as u32)
            .u32(8, ATTACH_SK_LOOKUP)
            .object(BPF_LINK_CREATE)
            .map_err(|e| {
                io::Error::new(e.kind(), format!("attaching the sk_lookup program to this network namespace: {e}"))
            })?;
        Ok(Steer { _link: link, _prog: prog, map, slots })
    }

    /// Slot `slot` (0-based) takes connections on `listener`. The map keeps
    /// the socket, not the descriptor: the caller may close its copy.
    pub(crate) fn set(&self, slot: usize, listener: RawFd) -> io::Result<()> {
        if slot >= self.slots {
            return Err(io::Error::other(format!("slot {slot} of {}", self.slots)));
        }
        let (key, val) = (slot as u32, listener as u32);
        Attr::new()
            .u32(0, self.map.as_raw_fd() as u32)
            .u64(8, &key as *const u32 as u64)
            .u64(16, &val as *const u32 as u64)
            .run(BPF_MAP_UPDATE_ELEM)
    }

    /// Slot `slot` takes no new connections. Not an error when it was empty.
    pub(crate) fn clear(&self, slot: usize) {
        let key = slot as u32;
        let _ =
            Attr::new().u32(0, self.map.as_raw_fd() as u32).u64(8, &key as *const u32 as u64).run(BPF_MAP_DELETE_ELEM);
    }
}

/// Inodes of the TCP sockets in LISTEN state on `port`, from the tables of
/// the network namespace of `pid`.
fn listener_inodes(pid: u32, port: u16) -> Vec<u64> {
    let mut found = Vec::new();
    for table in ["tcp", "tcp6"] {
        let Ok(text) = std::fs::read_to_string(format!("/proc/{pid}/net/{table}")) else { continue };
        for row in text.lines().skip(1) {
            let mut f = row.split_whitespace();
            let (Some(local), Some("0A")) = (f.nth(1), f.nth(1)) else { continue };
            let p = local.rsplit_once(':').and_then(|(_, p)| u16::from_str_radix(p, 16).ok());
            if p == Some(port) {
                if let Some(inode) = f.nth(5).and_then(|i| i.parse().ok()) {
                    found.push(inode);
                }
            }
        }
    }
    found
}

/// A copy of the listening socket on `port` that one of `pids` (a worker
/// and the processes below it) holds.
pub(crate) fn find_listener(pids: &[u32], port: u16) -> io::Result<OwnedFd> {
    for pid in pids {
        let inodes = listener_inodes(*pid, port);
        if inodes.is_empty() {
            continue;
        }
        let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/fd")) else { continue };
        for entry in dir.flatten() {
            let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse::<RawFd>().ok()) else { continue };
            let Ok(target) = std::fs::read_link(entry.path()) else { continue };
            let inode =
                target.to_str().and_then(|t| t.strip_prefix("socket:[")?.strip_suffix(']')?.parse::<u64>().ok());
            if inode.is_some_and(|i| inodes.contains(&i)) {
                return crate::sys::pidfd_getfd(*pid, fd);
            }
        }
    }
    Err(io::Error::other(format!("no process of the worker listens on port {port}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};

    #[test]
    fn the_program_is_closed_and_jumps_stay_inside() {
        for addr in ["127.0.0.2", "2001:db8::10"] {
            for slots in [1, 4, 16, 40] {
                let p = program(addr.parse().unwrap(), 443, slots, 7);
                let last = p.last().unwrap();
                assert_eq!(*last, EXIT);
                for (i, insn) in p.iter().enumerate() {
                    if matches!(insn.code, JNE_IMM | JEQ_IMM | JA) {
                        let to = i as i64 + 1 + insn.off as i64;
                        assert!((0..p.len() as i64).contains(&to), "{addr} {slots}: jump at {i} leaves the program");
                    }
                }
            }
        }
    }

    /// Needs root and a kernel with sk_lookup; says so and passes where it
    /// has neither.
    #[test]
    fn connections_to_the_public_port_land_on_the_workers_ports() {
        let addr = IpAddr::V4(Ipv4Addr::new(127, 77, 0, 1));
        let listeners: Vec<TcpListener> = (0..4).map(|_| TcpListener::bind("127.0.0.1:0").unwrap()).collect();
        let public = 39443;
        let steer = match Steer::attach(addr, public, 4) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skipped: {e}");
                return;
            }
        };
        for (i, l) in listeners.iter().enumerate() {
            steer.set(i, l.as_raw_fd()).unwrap();
        }
        let mut hits = [0usize; 4];
        let count = |hits: &mut [usize; 4], n: usize| {
            for _ in 0..n {
                let c = TcpStream::connect(SocketAddr::new(addr, public)).unwrap();
                let who = listeners
                    .iter()
                    .position(|l| {
                        l.set_nonblocking(true).unwrap();
                        l.accept().is_ok()
                    })
                    .or_else(|| {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        listeners.iter().position(|l| l.accept().is_ok())
                    })
                    .expect("a worker got the connection");
                hits[who] += 1;
                drop(c);
            }
        };
        count(&mut hits, 200);
        assert!(hits.iter().all(|h| *h > 20), "spread over every worker: {hits:?}");
        // A cleared slot gets nothing, and its share goes to the next one.
        steer.clear(1);
        let mut after = [0usize; 4];
        count(&mut after, 100);
        assert_eq!(after[1], 0, "{after:?}");
        assert_eq!(after.iter().sum::<usize>(), 100);
        // Other ports and addresses are not touched.
        assert!(TcpStream::connect(SocketAddr::new(addr, public + 1)).is_err());
    }
}

//! io_uring for `warden serve` (`[static] io = "uring"`): a ring that owns
//! everything the kernel may touch while an operation is in flight.
//!
//! The one unsafe operation is queueing a submission entry: the kernel will
//! later read or write the buffer the entry points to and use its
//! descriptor, so both must stay valid, unmoved and untouched until the
//! entry's completion arrives (which may be long after the caller stopped
//! caring, e.g. a connection dropped while a receive was pending). `Ring`
//! guarantees that by construction: an operation's buffer and the owner of
//! its descriptor are moved into the ring's own table when the entry is
//! queued, and handed back only with its completion. Nothing outside this
//! file can reach them in between. If the ring is dropped with operations
//! still in flight, it cancels them and waits for their completions for a
//! moment; whatever the kernel still holds after that is leaked, never
//! freed.
//!
//! Operations used: RECV, SEND (with MSG_MORE), ACCEPT, POLL_ADD and
//! ASYNC_CANCEL, all single-shot; probed at setup (kernel 5.6+), with a
//! round trip through the ring so a seccomp filter that allows setup but
//! not use is caught there, not on the first request.

use io_uring::{IoUring, opcode, squeue, types};
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Bytes a send reads. Heap memory that does not move while the ring
/// holds it: a Vec is never touched (so never reallocated) until it comes
/// back, and an `Arc<[u8]>` is immutable.
pub enum SendBuf {
    Vec(Vec<u8>),
    Shared(Arc<[u8]>),
}

impl SendBuf {
    fn bytes(&self) -> &[u8] {
        match self {
            SendBuf::Vec(v) => v,
            SendBuf::Shared(a) => a,
        }
    }
}

/// A socket an operation works on. Std types only: their descriptor is
/// fixed for their lifetime and closed only when the last owner drops it,
/// so it can't be closed or reused while the ring holds an owner.
#[derive(Clone)]
pub enum Sock {
    Stream(Arc<TcpStream>),
    Listener(Arc<TcpListener>),
}

impl Sock {
    fn raw(&self) -> RawFd {
        match self {
            Sock::Stream(s) => s.as_raw_fd(),
            Sock::Listener(l) => l.as_raw_fd(),
        }
    }
}

/// An operation in flight: slot index (low 32 bits) and the slot's
/// generation (high 32 bits), so a stale id never matches a reused slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(u64);

/// What an operation keeps alive until its completion.
enum Held {
    Recv { _sock: Sock, buf: Vec<u8> },
    Send { _sock: Sock, buf: SendBuf, start: usize, end: usize },
    Accept { _sock: Sock },
    Poll { _sock: Sock },
}

/// A finished operation; buffers come back with it.
pub enum Done {
    /// Bytes received into `buf[..n]` (0: the peer closed).
    Recv(io::Result<usize>, Vec<u8>),
    /// Bytes sent from the start of the range (may be short); the buffer.
    Send(io::Result<usize>, SendBuf),
    /// A new connection: close-on-exec, non-blocking.
    Accept(io::Result<TcpStream>),
    /// The poll mask that fired.
    Poll(io::Result<u32>),
}

struct Slot {
    generation: u32,
    held: Option<Held>,
}

/// user_data of entries whose completions carry nothing to hand back
/// (cancel requests, the setup round trip).
const INTERNAL: u64 = u64::MAX;

pub struct Ring {
    ring: IoUring,
    slots: Vec<Slot>,
    free: Vec<u32>,
    in_flight: usize,
    /// Entries that did not fit in the submission queue, in order; they go
    /// in as it drains (queueing never fails). Bounded by the operations in
    /// flight: at most one entry and one cancel each.
    backlog: std::collections::VecDeque<squeue::Entry>,
}

fn errno(res: i32) -> io::Error {
    io::Error::from_raw_os_error(-res)
}

impl Ring {
    /// A ring with `entries` submission slots (completions: 4×, and never
    /// dropped: the kernel keeps overflow until it is read). Errors when
    /// io_uring is missing, blocked (seccomp, `kernel.io_uring_disabled`),
    /// or lacks an operation this file uses.
    pub fn new(entries: u32) -> io::Result<Ring> {
        let ring = IoUring::builder().setup_cqsize(entries.saturating_mul(4)).setup_clamp().build(entries)?;
        if !ring.params().is_feature_nodrop() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this kernel's io_uring may drop completions (no IORING_FEAT_NODROP, kernel 5.5+)",
            ));
        }
        let mut probe = io_uring::Probe::new();
        ring.submitter().register_probe(&mut probe)?;
        for (code, name) in [
            (opcode::Recv::CODE, "RECV"),
            (opcode::Send::CODE, "SEND"),
            (opcode::Accept::CODE, "ACCEPT"),
            (opcode::PollAdd::CODE, "POLL_ADD"),
            (opcode::AsyncCancel::CODE, "ASYNC_CANCEL"),
            (opcode::Nop::CODE, "NOP"),
        ] {
            if !probe.is_supported(code) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("this kernel's io_uring has no {name} operation (kernel 5.6+ needed)"),
                ));
            }
        }
        let mut r = Ring { ring, slots: Vec::new(), free: Vec::new(), in_flight: 0, backlog: Default::default() };
        // A round trip: setup can be allowed where submitting is not.
        r.push(opcode::Nop::new().build().user_data(INTERNAL));
        r.ring.submit_and_wait(1)?;
        let got = r.ring.completion().next().map(|c| c.result());
        match got {
            Some(0) => Ok(r),
            Some(res) => Err(errno(res)),
            None => Err(io::Error::other("io_uring returned no completion for a NOP")),
        }
    }

    /// The ring's descriptor: readable when completions are waiting.
    pub fn raw_fd(&self) -> RawFd {
        self.ring.as_raw_fd()
    }

    #[cfg(test)]
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Entries queued and not yet submitted.
    pub fn queued(&mut self) -> usize {
        self.ring.submission().len() + self.backlog.len()
    }

    /// Put `entry` on the submission queue (`false`: it is full).
    ///
    /// Only for entries whose memory and descriptor are owned by a slot in
    /// `self.slots` (or that reference none): see `start`.
    fn sq_push(&mut self, entry: &squeue::Entry) -> bool {
        // SAFETY: every entry built in this file references either no
        // memory (NOP, ACCEPT with null address, POLL_ADD, ASYNC_CANCEL) or
        // a buffer owned by the `Held` that `start` stored in `self.slots`
        // before building the entry; that `Held` also owns the descriptor.
        // It stays there, unread and unmodified, until `complete` takes it
        // out after the kernel has posted this entry's completion (or
        // forever, if `Drop` has to leak it), so the kernel never sees freed
        // or moved memory or a reused descriptor.
        unsafe { self.ring.submission().push(entry) }.is_ok()
    }

    /// Queue `entry`: on the submission queue, or after the backlog (order
    /// is kept: a cancel always follows what it cancels).
    fn push(&mut self, entry: squeue::Entry) {
        if !self.backlog.is_empty() || !self.sq_push(&entry) {
            self.backlog.push_back(entry);
        }
    }

    /// Move backlog entries to the submission queue while there is room.
    fn flush_backlog(&mut self) {
        while let Some(e) = self.backlog.front() {
            let e = e.clone();
            if !self.sq_push(&e) {
                break;
            }
            self.backlog.pop_front();
        }
    }

    /// Store `held` in a free slot and queue the entry `build` makes from
    /// it (pointers into the slot's own buffer).
    fn start(&mut self, held: Held, build: impl FnOnce(&mut Held) -> squeue::Entry) -> io::Result<OpId> {
        let index = match self.free.pop() {
            Some(i) => i,
            None => {
                let i =
                    u32::try_from(self.slots.len()).map_err(|_| io::Error::other("too many io_uring operations"))?;
                self.slots.push(Slot { generation: 0, held: None });
                i
            }
        };
        let slot = &mut self.slots[index as usize];
        let id = OpId((slot.generation as u64) << 32 | index as u64);
        let held = slot.held.insert(held);
        let entry = build(held).user_data(id.0);
        self.push(entry);
        self.in_flight += 1;
        Ok(id)
    }

    /// Receive into all of `buf` (which must not be empty).
    pub fn recv(&mut self, sock: Sock, mut buf: Vec<u8>) -> io::Result<OpId> {
        if buf.is_empty() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty receive buffer"));
        }
        // At most 4 GiB per receive (the entry's length is 32-bit).
        buf.truncate(u32::MAX as usize);
        let fd = types::Fd(sock.raw());
        self.start(Held::Recv { _sock: sock, buf }, |h| match h {
            Held::Recv { buf, .. } => opcode::Recv::new(fd, buf.as_mut_ptr(), buf.len() as u32).build(),
            _ => opcode::Nop::new().build(),
        })
    }

    /// Send `buf[range]` (`more`: MSG_MORE, more data follows). Never
    /// raises SIGPIPE. MSG_WAITALL: since 5.18 the kernel itself waits for
    /// room and sends the rest, so the whole range takes one entry; older
    /// kernels may still return a short count (send the rest again).
    pub fn send(&mut self, sock: Sock, buf: SendBuf, range: std::ops::Range<usize>, more: bool) -> io::Result<OpId> {
        let len = buf.bytes().len();
        if range.start > range.end || range.end > len {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "send range outside the buffer"));
        }
        let flags = libc::MSG_NOSIGNAL | libc::MSG_WAITALL | if more { libc::MSG_MORE } else { 0 };
        let fd = types::Fd(sock.raw());
        self.start(Held::Send { _sock: sock, buf, start: range.start, end: range.end }, |h| match h {
            Held::Send { buf, start, end, .. } => {
                let n = (*end - *start).min(u32::MAX as usize) as u32;
                opcode::Send::new(fd, buf.bytes()[*start..].as_ptr(), n).flags(flags).build()
            }
            _ => opcode::Nop::new().build(),
        })
    }

    /// Accept one connection (close-on-exec, non-blocking).
    pub fn accept(&mut self, listener: Arc<TcpListener>) -> io::Result<OpId> {
        let fd = types::Fd(listener.as_raw_fd());
        self.start(Held::Accept { _sock: Sock::Listener(listener) }, |_| {
            opcode::Accept::new(fd, std::ptr::null_mut(), std::ptr::null_mut())
                .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
                .build()
        })
    }

    /// Completes once `sock` is writable (or has an error / hung up).
    pub fn poll_writable(&mut self, sock: Sock) -> io::Result<OpId> {
        let fd = types::Fd(sock.raw());
        self.start(Held::Poll { _sock: sock }, |_| opcode::PollAdd::new(fd, libc::POLLOUT as u32).build())
    }

    /// Completes once `sock` is readable (for a listener: a connection is
    /// waiting), or has an error.
    pub fn poll_readable(&mut self, sock: Sock) -> io::Result<OpId> {
        let fd = types::Fd(sock.raw());
        self.start(Held::Poll { _sock: sock }, |_| opcode::PollAdd::new(fd, libc::POLLIN as u32).build())
    }

    /// Ask the kernel to cancel `id` if it is still pending. Its completion
    /// still arrives (with ECANCELED, or its result if it finished first).
    pub fn cancel(&mut self, id: OpId) {
        let live = self
            .slots
            .get((id.0 & 0xffff_ffff) as usize)
            .is_some_and(|s| s.generation as u64 == id.0 >> 32 && s.held.is_some());
        if live {
            self.push(opcode::AsyncCancel::new(id.0).build().user_data(INTERNAL));
        }
    }

    /// Hand queued entries to the kernel (one io_uring_enter per
    /// submission queue's worth); a no-op without a syscall when nothing is
    /// queued. EBUSY means completions must be read first.
    pub fn submit(&mut self) -> io::Result<usize> {
        let mut total = 0;
        loop {
            self.flush_backlog();
            let overflow = self.ring.submission().cq_overflow();
            if self.ring.submission().is_empty() && !overflow {
                return Ok(total);
            }
            match self.ring.submit() {
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
                Ok(n) => {
                    total += n;
                    if n == 0 || self.backlog.is_empty() {
                        return Ok(total);
                    }
                }
            }
        }
    }

    /// Submit, then wait up to `timeout` for at least one completion.
    pub fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        self.flush_backlog();
        let ts = types::Timespec::from(timeout);
        let args = types::SubmitArgs::new().timespec(&ts);
        match self.ring.submitter().submit_with_args(1, &args) {
            Ok(_) => Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ETIME | libc::EINTR | libc::EBUSY)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Every completion available now, without a syscall; returns how many.
    pub fn complete(&mut self, mut f: impl FnMut(OpId, Done)) -> usize {
        let mut n = 0;
        let Ring { ring, slots, free, in_flight, .. } = self;
        for cqe in ring.completion() {
            let ud = cqe.user_data();
            if ud == INTERNAL {
                continue;
            }
            let index = (ud & 0xffff_ffff) as usize;
            let Some(slot) = slots.get_mut(index) else { continue };
            if slot.generation as u64 != ud >> 32 {
                continue;
            }
            let Some(held) = slot.held.take() else { continue };
            slot.generation = slot.generation.wrapping_add(1);
            free.push(index as u32);
            *in_flight -= 1;
            n += 1;
            let res = cqe.result();
            let done = match held {
                Held::Recv { buf, .. } => Done::Recv(if res < 0 { Err(errno(res)) } else { Ok(res as usize) }, buf),
                Held::Send { buf, .. } => Done::Send(if res < 0 { Err(errno(res)) } else { Ok(res as usize) }, buf),
                Held::Accept { .. } if res < 0 => Done::Accept(Err(errno(res))),
                // SAFETY: a successful accept's result is a new descriptor
                // the kernel installed for us; nothing else owns it.
                Held::Accept { .. } => Done::Accept(Ok(TcpStream::from(unsafe { OwnedFd::from_raw_fd(res) }))),
                Held::Poll { .. } => Done::Poll(if res < 0 { Err(errno(res)) } else { Ok(res as u32) }),
            };
            f(OpId(ud), done);
        }
        n
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if self.in_flight == 0 {
            return;
        }
        // Cancel what is still pending and collect the completions for a
        // moment, so buffers and sockets are released normally.
        let live: Vec<OpId> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.held.is_some())
            .map(|(i, s)| OpId((s.generation as u64) << 32 | i as u64))
            .collect();
        for id in live {
            self.cancel(id);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while self.in_flight > 0 && Instant::now() < deadline {
            if self.wait(Duration::from_millis(50)).is_err() {
                break;
            }
            self.complete(|_, _| {});
        }
        // Anything left may still be written by the kernel: leak it.
        for slot in &mut self.slots {
            if let Some(held) = slot.held.take() {
                std::mem::forget(held);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// A ring, or None (with a note) where io_uring is unavailable: the
    /// server falls back to epoll there, so there is nothing to test.
    fn ring() -> Option<Ring> {
        match Ring::new(8) {
            Ok(r) => Some(r),
            Err(e) => {
                eprintln!("skipping: io_uring is unavailable here ({e})");
                None
            }
        }
    }

    fn pair() -> (Arc<TcpStream>, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        s.set_nonblocking(true).unwrap();
        (Arc::new(s), c)
    }

    /// Run the ring until `n` completions arrived (5 s at most).
    fn collect(r: &mut Ring, n: usize) -> Vec<(OpId, Done)> {
        let mut out = Vec::new();
        let t0 = Instant::now();
        while out.len() < n {
            assert!(t0.elapsed() < Duration::from_secs(5), "only {} of {n} completions", out.len());
            r.wait(Duration::from_millis(20)).unwrap();
            r.complete(|id, d| out.push((id, d)));
        }
        out
    }

    #[test]
    fn receives_wait_for_data_and_return_the_buffer() {
        let Some(mut r) = ring() else { return };
        let (s, mut c) = pair();
        let id = r.recv(Sock::Stream(s.clone()), vec![0u8; 64]).unwrap();
        r.submit().unwrap();
        // Nothing sent yet: the receive must wait, not fail with EAGAIN
        // (the socket is non-blocking; io_uring polls it).
        r.wait(Duration::from_millis(50)).unwrap();
        assert_eq!(r.complete(|_, _| panic!("completed without data")), 0);
        assert_eq!(r.in_flight(), 1);
        c.write_all(b"hello").unwrap();
        let mut got = collect(&mut r, 1);
        let (gid, Done::Recv(Ok(5), buf)) = got.remove(0) else { panic!("not a 5-byte receive") };
        assert_eq!((gid, &buf[..5], buf.len()), (id, &b"hello"[..], 64));
        // The peer closes: 0 bytes.
        drop(c);
        r.recv(Sock::Stream(s), buf).unwrap();
        let mut got = collect(&mut r, 1);
        assert!(matches!(got.remove(0).1, Done::Recv(Ok(0), _)));
        assert_eq!(r.in_flight(), 0);
        assert!(r.recv(Sock::Stream(pair().0), Vec::new()).is_err(), "empty buffer refused");
    }

    #[test]
    fn sends_deliver_exact_bytes_to_a_slow_reader() {
        let Some(mut r) = ring() else { return };
        let (s, mut c) = pair();
        // Small buffers: the kernel has to wait for the reader many times
        // within one send (older kernels return short counts instead).
        let small: libc::c_int = 16 * 1024;
        for (fd, opt) in [(s.as_raw_fd(), libc::SO_SNDBUF), (c.as_raw_fd(), libc::SO_RCVBUF)] {
            // SAFETY (test): setsockopt with a live c_int and its size.
            unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, opt, &small as *const _ as *const libc::c_void, 4) };
        }
        let data: Arc<[u8]> = (0..3_000_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            got
        });
        // A Vec head with MSG_MORE, then a shared body from an offset, in order.
        let head = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        let mut want = head.clone();
        want.extend_from_slice(&data[7..]);
        let mut queue = vec![
            (SendBuf::Vec(head.clone()), 0..head.len(), true),
            (SendBuf::Shared(data.clone()), 7..data.len(), false),
        ];
        queue.reverse();
        while let Some((buf, range, more)) = queue.pop() {
            r.send(Sock::Stream(s.clone()), buf, range.clone(), more).unwrap();
            let mut got = collect(&mut r, 1);
            let Done::Send(Ok(n), buf) = got.remove(0).1 else { panic!("send failed") };
            assert!(n > 0 && n <= range.len());
            if n < range.len() {
                queue.push((buf, range.start + n..range.end, more));
            }
        }
        drop(s);
        assert!(reader.join().unwrap() == want, "every byte, in order");
        // A range outside the buffer is refused before anything is queued.
        let (s, _c) = pair();
        assert!(r.send(Sock::Stream(s), SendBuf::Vec(vec![1, 2]), 1..3, false).is_err());
        assert_eq!(r.in_flight(), 0);
    }

    #[test]
    fn sends_to_a_closed_peer_fail_without_sigpipe() {
        let Some(mut r) = ring() else { return };
        let (s, c) = pair();
        drop(c);
        std::thread::sleep(Duration::from_millis(20));
        let mut err = None;
        for _ in 0..50 {
            r.send(Sock::Stream(s.clone()), SendBuf::Vec(vec![0u8; 65536]), 0..65536, false).unwrap();
            if let Done::Send(Err(e), ..) = collect(&mut r, 1).remove(0).1 {
                err = Some(e);
                break;
            }
        }
        let e = err.expect("a closed peer must fail the send");
        assert!(matches!(e.raw_os_error(), Some(libc::EPIPE | libc::ECONNRESET)), "{e}");
    }

    #[test]
    fn accepts_connections_as_nonblocking_cloexec_sockets() {
        let Some(mut r) = ring() else { return };
        let l = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        l.set_nonblocking(true).unwrap();
        r.accept(l.clone()).unwrap();
        r.submit().unwrap();
        r.wait(Duration::from_millis(30)).unwrap();
        assert_eq!(r.complete(|_, _| panic!("accepted nothing")), 0);
        let mut c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let Done::Accept(Ok(mut s)) = collect(&mut r, 1).remove(0).1 else { panic!("accept failed") };
        // SAFETY (test): fcntl queries on a descriptor we own.
        let (fdfl, fl) =
            unsafe { (libc::fcntl(s.as_raw_fd(), libc::F_GETFD), libc::fcntl(s.as_raw_fd(), libc::F_GETFL)) };
        assert!(fdfl & libc::FD_CLOEXEC != 0 && fl & libc::O_NONBLOCK != 0);
        c.write_all(b"ping").unwrap();
        s.set_nonblocking(false).unwrap();
        let mut b = [0u8; 4];
        s.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"ping");
    }

    #[test]
    fn polls_for_writability() {
        let Some(mut r) = ring() else { return };
        let (s, _c) = pair();
        r.poll_writable(Sock::Stream(s)).unwrap();
        let Done::Poll(Ok(mask)) = collect(&mut r, 1).remove(0).1 else { panic!("poll failed") };
        assert!(mask & libc::POLLOUT as u32 != 0, "{mask:#x}");
    }

    #[test]
    fn a_listener_polls_readable_when_a_connection_waits() {
        let Some(mut r) = ring() else { return };
        let l = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        r.poll_readable(Sock::Listener(l.clone())).unwrap();
        r.submit().unwrap();
        r.wait(Duration::from_millis(30)).unwrap();
        assert_eq!(r.complete(|_, _| panic!("readable with no connection")), 0);
        let _c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let Done::Poll(Ok(mask)) = collect(&mut r, 1).remove(0).1 else { panic!("poll failed") };
        assert!(mask & libc::POLLIN as u32 != 0, "{mask:#x}");
    }

    #[test]
    fn cancelled_receives_give_back_the_buffer_and_release_the_socket() {
        let Some(mut r) = ring() else { return };
        let (s, mut c) = pair();
        let id = r.recv(Sock::Stream(s.clone()), vec![7u8; 32]).unwrap();
        r.submit().unwrap();
        // While the receive is pending, the ring holds the socket: dropping
        // our handle does not close it.
        drop(s);
        c.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let mut b = [0u8; 1];
        assert!(c.read(&mut b).is_err(), "closed while the kernel still had a receive on it");
        r.cancel(id);
        let Done::Recv(Err(e), buf) = collect(&mut r, 1).remove(0).1 else { panic!("not cancelled") };
        assert_eq!(e.raw_os_error(), Some(libc::ECANCELED));
        assert_eq!(buf, vec![7u8; 32], "the buffer comes back untouched");
        // Now it is closed: the peer sees the end.
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        assert_eq!(c.read(&mut b).unwrap(), 0);
        // Cancelling a finished operation is a no-op.
        r.cancel(id);
        assert_eq!((r.queued(), r.in_flight()), (0, 0));
    }

    #[test]
    fn dropping_the_ring_cancels_what_is_pending() {
        let Some(mut r) = ring() else { return };
        let (s, mut c) = pair();
        let l = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
        r.recv(Sock::Stream(s.clone()), vec![0u8; 16]).unwrap();
        r.accept(l.clone()).unwrap();
        r.submit().unwrap();
        drop((s, l));
        let t0 = Instant::now();
        drop(r);
        assert!(t0.elapsed() < Duration::from_millis(900), "cancelled, not timed out");
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let mut b = [0u8; 1];
        assert_eq!(c.read(&mut b).unwrap(), 0, "the socket was released and closed");
    }

    #[test]
    fn many_operations_reuse_slots_and_survive_a_full_queue() {
        let Some(mut r) = ring() else { return };
        // More receives than submission entries (8): pushing submits as needed.
        let mut pairs: Vec<_> = (0..40).map(|_| pair()).collect();
        let mut ids = Vec::new();
        for (s, _) in &pairs {
            ids.push(r.recv(Sock::Stream(s.clone()), vec![0u8; 8]).unwrap());
        }
        assert_eq!(r.queued(), 40, "8 in the submission queue, the rest in the backlog");
        r.submit().unwrap();
        assert_eq!(r.queued(), 0);
        for (i, (_, c)) in pairs.iter_mut().enumerate() {
            c.write_all(&[i as u8]).unwrap();
        }
        let got = collect(&mut r, 40);
        for (id, d) in got {
            let i = ids.iter().position(|x| *x == id).unwrap();
            let Done::Recv(Ok(1), buf) = d else { panic!("bad receive") };
            assert_eq!(buf[0], i as u8);
        }
        assert_eq!(r.in_flight(), 0);
        let slots = r.slots.len();
        for (s, c) in &mut pairs {
            r.recv(Sock::Stream(s.clone()), vec![0u8; 8]).unwrap();
            c.write_all(b"x").unwrap();
            collect(&mut r, 1);
        }
        assert_eq!(r.slots.len(), slots, "freed slots are reused");
    }

    #[test]
    fn ring_operations_do_not_leak_descriptors() {
        if std::env::var_os("WARDEN_FD_PROBE").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["sys::uring::tests::ring_operations_do_not_leak_descriptors", "--exact", "--test-threads=1"])
                .env("WARDEN_FD_PROBE", "1")
                .output()
                .unwrap();
            let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success() && text.contains("1 passed"), "failed in isolation:\n{text}");
            return;
        }
        let fds = || std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0);
        let Some(probe) = ring() else { return };
        drop(probe);
        let before = fds();
        for _ in 0..50 {
            let mut r = ring().unwrap();
            let l = Arc::new(TcpListener::bind("127.0.0.1:0").unwrap());
            let addr = l.local_addr().unwrap();
            r.accept(l.clone()).unwrap();
            let c = TcpStream::connect(addr).unwrap();
            let Done::Accept(Ok(s)) = collect(&mut r, 1).remove(0).1 else { panic!("accept failed") };
            let s = Arc::new(s);
            r.recv(Sock::Stream(s.clone()), vec![0u8; 8]).unwrap();
            r.accept(l).unwrap(); // pending at drop
            r.submit().unwrap();
            drop((s, c, r));
        }
        assert_eq!(fds(), before, "every socket and ring closed");
    }
}

//! `[static] io = "uring"`: the static server's TCP connections driven
//! through io_uring instead of epoll (experimental, off by default).
//!
//! Only the transport changes. The connection loop, request parsing,
//! responses and the cache are the same code as with epoll: a connection
//! gets a `Reader` (tokio's `AsyncBufRead`, receiving with io_uring RECV)
//! and a `Writer` (io_uring SEND, MSG_MORE for heads; sendfile(2) on the
//! non-blocking socket for large bodies, waiting with io_uring POLL_ADD).
//! Accepts are io_uring ACCEPTs. The Unix health socket stays on tokio.
//!
//! What it saves: the per-request recv/send syscalls and epoll re-arming.
//! Connection tasks only queue entries; one driver task submits everything
//! queued since its last turn with a single io_uring_enter and hands out
//! completions. It runs on tokio's reactor: the ring's descriptor is
//! readable when completions are waiting.
//!
//! Buffers and sockets an operation uses belong to the ring until its
//! completion (`sys::uring`), so a connection dropped mid-operation (a
//! timeout, the drain) is safe: its pending receive is cancelled and the
//! buffer and socket are released when the kernel says so.

use super::OutBuf;
use crate::sys::uring::{Done, OpId, Ring, SendBuf, Sock};
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use tokio::io::unix::AsyncFd;

/// Submission entries per worker (more are submitted in batches).
const ENTRIES: u32 = 256;
/// Receive buffer per connection (what tokio's BufReader uses).
const RECV_BUF: usize = 8 * 1024;

struct RingFd(RawFd);

impl AsRawFd for RingFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[derive(Default)]
struct Waiter {
    waker: Option<Waker>,
    done: Option<Done>,
    /// Its future was dropped: discard the completion when it comes.
    orphaned: bool,
}

struct State {
    ring: Ring,
    waiters: HashMap<OpId, Waiter>,
}

/// One per worker: the ring, and who waits for what.
pub struct Driver {
    /// Registered with tokio's reactor; dropped (deregistered) before the
    /// ring closes its descriptor, as fields drop in order.
    afd: AsyncFd<RingFd>,
    state: Mutex<State>,
    /// Entries were queued: the driver task should submit them.
    queued: tokio::sync::Notify,
}

impl Driver {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        // Never held across an await or a callback; a panic can't leave the
        // state half-changed, so a poisoned lock is still consistent.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Start an operation; the returned future resolves with its result.
    fn start(self: &Arc<Self>, cancel_on_drop: bool, f: impl FnOnce(&mut Ring) -> io::Result<OpId>) -> io::Result<Op> {
        let id = {
            let mut st = self.lock();
            let id = f(&mut st.ring)?;
            st.waiters.insert(id, Waiter::default());
            id
        };
        self.queued.notify_one();
        Ok(Op { d: self.clone(), id: Some(id), cancel_on_drop })
    }

    /// Submit what is queued, hand out completions; again while either
    /// finds work. Wakers are woken after the lock is released. Ok(true):
    /// the kernel refused entries for now (try again shortly).
    fn turn(&self) -> io::Result<bool> {
        let mut wake = Vec::new();
        let busy = {
            let mut st = self.lock();
            let State { ring, waiters } = &mut *st;
            loop {
                let busy = match ring.submit() {
                    Ok(_) => false,
                    // EBUSY: completions must be read first (they are,
                    // below); EAGAIN: the kernel is short of memory.
                    Err(e) if matches!(e.raw_os_error(), Some(libc::EBUSY | libc::EAGAIN)) => true,
                    Err(e) => return Err(e),
                };
                let n = ring.complete(|id, done| {
                    if let Some(w) = waiters.get_mut(&id) {
                        if w.orphaned {
                            waiters.remove(&id);
                        } else {
                            w.done = Some(done);
                            if let Some(k) = w.waker.take() {
                                wake.push(k);
                            }
                        }
                    }
                });
                if n == 0 && (busy || ring.queued() == 0) {
                    break busy;
                }
            }
        };
        for k in wake {
            k.wake();
        }
        Ok(busy)
    }
}

/// The driver task: submit and complete whenever entries were queued or
/// the ring has completions.
async fn drive(d: Arc<Driver>) {
    loop {
        match d.turn() {
            Ok(false) => {}
            Ok(true) => tokio::time::sleep(std::time::Duration::from_millis(1)).await,
            Err(e) => {
                // Nothing can make progress without the ring; a restarted
                // worker gets a fresh one (or epoll, if io_uring is gone).
                crate::error!(
                    "io_uring stopped working; this static worker exits so Warden restarts it",
                    error = e,
                    hint = "if it keeps happening, set [static] io = \"epoll\" (the default)"
                );
                std::process::exit(1);
            }
        }
        tokio::select! {
            biased;
            r = d.afd.readable() => match r {
                // Clear first, then read everything (next turn): a
                // completion arriving in between marks it ready again.
                Ok(mut g) => g.clear_ready(),
                // The runtime is shutting down.
                Err(_) => return,
            },
            _ = d.queued.notified() => {}
        }
    }
}

/// An operation in flight. Dropping it before it completes abandons the
/// result (the ring still owns its buffers until the kernel is done), and
/// for receives and accepts also asks the kernel to cancel it, which is
/// what releases the socket.
struct Op {
    d: Arc<Driver>,
    id: Option<OpId>,
    cancel_on_drop: bool,
}

impl Future for Op {
    type Output = Done;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Done> {
        let Some(id) = self.id else {
            return Poll::Ready(Done::Poll(Err(io::Error::other("io_uring operation polled after it completed"))));
        };
        let mut st = self.d.lock();
        let Some(w) = st.waiters.get_mut(&id) else {
            // Can't happen (only this future removes its waiter).
            drop(st);
            self.id = None;
            return Poll::Ready(Done::Poll(Err(io::Error::other("io_uring operation lost"))));
        };
        if let Some(done) = w.done.take() {
            st.waiters.remove(&id);
            drop(st);
            self.id = None;
            return Poll::Ready(done);
        }
        if !w.waker.as_ref().is_some_and(|k| k.will_wake(cx.waker())) {
            w.waker = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        let mut cancelled = false;
        {
            let mut st = self.d.lock();
            match st.waiters.get_mut(&id) {
                Some(w) if w.done.is_some() => {
                    st.waiters.remove(&id);
                }
                Some(w) => {
                    w.orphaned = true;
                    w.waker = None;
                    if self.cancel_on_drop {
                        st.ring.cancel(id);
                        cancelled = true;
                    }
                }
                None => {}
            }
        }
        if cancelled {
            self.d.queued.notify_one();
        }
    }
}

fn unexpected() -> io::Error {
    io::Error::other("io_uring returned a different kind of completion")
}

/// Accepts connections with io_uring.
pub struct Listener {
    d: Arc<Driver>,
    sock: Arc<TcpListener>,
    /// The pending accept (or readiness poll), kept across `accept()` calls
    /// (the server's select! drops the future when another branch wins).
    op: Option<Op>,
    /// The next operation is a readiness poll, then the accept: an accept
    /// completed with EAGAIN.
    poll_first: bool,
}

/// What the listener does after a completion.
#[derive(Debug, PartialEq)]
enum Next {
    /// Hand it to the server: a connection, or an error it handles.
    Done,
    /// Queue another accept: EINTR, or the poll said a connection waits.
    Accept,
    /// Wait for a connection with a poll first, then accept: the accept
    /// completed with EAGAIN (some kernels do that for a non-blocking
    /// listener instead of waiting); accepting again at once would spin.
    PollThenAccept,
}

fn next(done: &Done) -> Next {
    match done {
        Done::Accept(Err(e)) if e.kind() == io::ErrorKind::WouldBlock => Next::PollThenAccept,
        Done::Accept(Err(e)) if e.kind() == io::ErrorKind::Interrupted => Next::Accept,
        Done::Poll(Ok(_)) => Next::Accept,
        _ => Next::Done,
    }
}

impl Listener {
    /// Set up the ring and its driver task (needs the tokio runtime). On
    /// failure the listener comes back as it was (non-blocking), so the
    /// caller can use epoll.
    pub fn new(l: TcpListener) -> Result<Listener, (io::Error, TcpListener)> {
        let ring = match Ring::new(ENTRIES) {
            Ok(r) => r,
            Err(e) => return Err((e, l)),
        };
        let afd = match AsyncFd::with_interest(RingFd(ring.raw_fd()), tokio::io::Interest::READABLE) {
            Ok(a) => a,
            Err(e) => return Err((e, l)),
        };
        // A blocking listener, so an ACCEPT waits in the kernel for a
        // connection. Since "io_uring: check file O_NONBLOCK state for
        // accept" (5.10), some kernels complete it at once with EAGAIN on a
        // non-blocking one, which would spin a CPU per idle worker. Changed
        // last: every failure above leaves it non-blocking for epoll, and
        // nothing else uses this socket from here on.
        if let Err(e) = l.set_nonblocking(false) {
            return Err((e, l));
        }
        let d = Arc::new(Driver {
            afd,
            state: Mutex::new(State { ring, waiters: HashMap::new() }),
            queued: tokio::sync::Notify::new(),
        });
        tokio::spawn(drive(d.clone()));
        Ok(Listener { d, sock: Arc::new(l), op: None, poll_first: false })
    }

    pub async fn accept(&mut self) -> io::Result<Stream> {
        std::future::poll_fn(|cx| self.poll_accept(cx)).await
    }

    fn poll_accept(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Stream>> {
        loop {
            if self.op.is_none() {
                let sock = self.sock.clone();
                let started = if self.poll_first {
                    self.d.start(true, |r| r.poll_readable(Sock::Listener(sock)))
                } else {
                    self.d.start(true, |r| r.accept(sock))
                };
                match started {
                    Ok(op) => self.op = Some(op),
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            let Some(op) = self.op.as_mut() else { return Poll::Pending };
            // A new operation is still pending here: completions only come
            // from the driver task, so this loop never spins.
            let done = match Pin::new(op).poll(cx) {
                Poll::Ready(d) => d,
                Poll::Pending => return Poll::Pending,
            };
            self.op = None;
            match next(&done) {
                Next::Accept => self.poll_first = false,
                Next::PollThenAccept => self.poll_first = true,
                Next::Done => {
                    return Poll::Ready(match done {
                        Done::Accept(Ok(s)) => {
                            let _ = s.set_nodelay(true);
                            Ok(Stream { d: self.d.clone(), sock: Arc::new(s) })
                        }
                        Done::Accept(Err(e)) | Done::Poll(Err(e)) => Err(e),
                        _ => Err(unexpected()),
                    });
                }
            }
        }
    }
}

/// An accepted connection.
pub struct Stream {
    d: Arc<Driver>,
    sock: Arc<TcpStream>,
}

impl Stream {
    pub fn split(self) -> (Reader, Writer) {
        let r = Reader {
            d: self.d.clone(),
            sock: self.sock.clone(),
            buf: vec![0u8; RECV_BUF],
            pos: 0,
            end: 0,
            eof: false,
            op: None,
        };
        (r, Writer { d: self.d, sock: self.sock })
    }
}

/// The receiving half: a buffered reader over io_uring receives.
pub struct Reader {
    d: Arc<Driver>,
    sock: Arc<TcpStream>,
    /// Owned by the pending receive while there is one (then empty here).
    buf: Vec<u8>,
    pos: usize,
    end: usize,
    eof: bool,
    op: Option<Op>,
}

impl tokio::io::AsyncBufRead for Reader {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let me = self.get_mut();
        while me.pos >= me.end && !me.eof {
            if me.op.is_none() {
                let buf = std::mem::take(&mut me.buf);
                let sock = Sock::Stream(me.sock.clone());
                match me.d.start(true, |r| r.recv(sock, buf)) {
                    Ok(op) => me.op = Some(op),
                    Err(e) => return Poll::Ready(Err(e)),
                }
            }
            let Some(op) = me.op.as_mut() else { continue };
            let done = match Pin::new(op).poll(cx) {
                Poll::Ready(d) => d,
                Poll::Pending => return Poll::Pending,
            };
            me.op = None;
            let Done::Recv(res, buf) = done else { return Poll::Ready(Err(unexpected())) };
            me.buf = buf;
            match res {
                Ok(0) => me.eof = true,
                Ok(n) => (me.pos, me.end) = (0, n),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
        Poll::Ready(Ok(&me.buf[me.pos..me.end]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let me = self.get_mut();
        me.pos = (me.pos + amt).min(me.end);
    }
}

impl tokio::io::AsyncRead for Reader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        use tokio::io::AsyncBufRead;
        let n = match self.as_mut().poll_fill_buf(cx) {
            Poll::Ready(Ok(data)) => {
                let n = data.len().min(out.remaining());
                out.put_slice(&data[..n]);
                n
            }
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        };
        self.consume(n);
        Poll::Ready(Ok(()))
    }
}

/// The sending half.
pub struct Writer {
    d: Arc<Driver>,
    sock: Arc<TcpStream>,
}

impl Writer {
    /// All of `b`, in order (resubmitting after short sends); with `more`,
    /// MSG_MORE (a sendfile body follows).
    pub async fn send_all(&mut self, b: OutBuf, more: bool) -> io::Result<()> {
        let (mut buf, mut range) = match b {
            OutBuf::Vec(v) => {
                let n = v.len();
                (SendBuf::Vec(v), 0..n)
            }
            OutBuf::Shared(a, r) => (SendBuf::Shared(a), r),
        };
        while range.start < range.end {
            let sock = Sock::Stream(self.sock.clone());
            let r = range.clone();
            let done = self.d.start(false, |ring| ring.send(sock, buf, r, more))?.await;
            let Done::Send(res, b) = done else { return Err(unexpected()) };
            buf = b;
            match res {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(n) => range.start += n,
                Err(e) if matches!(e.kind(), io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.sock.shutdown(std::net::Shutdown::Write)
    }

    /// `count` bytes of `file` from `offset` with sendfile(2) on the
    /// non-blocking socket, waiting for room with an io_uring poll.
    pub async fn send_file(&mut self, file: &std::fs::File, offset: u64, count: u64) -> io::Result<u64> {
        let mut off = offset as i64;
        let mut left = count;
        while left > 0 {
            let chunk = left.min(1 << 30) as usize;
            match crate::sys::sendfile(self.sock.as_fd(), file.as_fd(), &mut off, chunk) {
                // The file shrank under us: the promised Content-Length can't
                // be met, so the connection must close.
                Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file truncated while sending")),
                Ok(n) => left -= (n as u64).min(left),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let sock = Sock::Stream(self.sock.clone());
                    if let Done::Poll(Err(e)) = self.d.start(true, |r| r.poll_writable(sock))?.await {
                        return Err(e);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::time::Duration;
    use tokio::io::AsyncReadExt;

    /// O_NONBLOCK on `fd`, from /proc (no unsafe outside src/sys*).
    fn nonblocking(fd: RawFd) -> bool {
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{fd}")).unwrap();
        let flags = info.lines().find_map(|l| l.strip_prefix("flags:")).unwrap().trim();
        i64::from_str_radix(flags, 8).unwrap() & libc::O_NONBLOCK as i64 != 0
    }

    #[test]
    fn an_eagain_accept_waits_for_a_connection_before_accepting_again() {
        let failed = |n| Done::Accept(Err(io::Error::from_raw_os_error(n)));
        assert_eq!(next(&failed(libc::EAGAIN)), Next::PollThenAccept, "never re-armed at once: that would spin");
        assert_eq!(next(&failed(libc::EINTR)), Next::Accept);
        assert_eq!(next(&Done::Poll(Ok(libc::POLLIN as u32))), Next::Accept);
        // The server's to handle (and back off from).
        assert_eq!(next(&failed(libc::EMFILE)), Next::Done);
        assert_eq!(next(&failed(libc::ECONNABORTED)), Next::Done);
        assert_eq!(next(&Done::Poll(Err(io::Error::from_raw_os_error(libc::ECANCELED)))), Next::Done);
    }

    #[test]
    fn the_ring_gets_a_blocking_listener_and_serves_through_it() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            // As the server makes it: non-blocking, for epoll.
            let l = crate::sys::listen_tcp("127.0.0.1:0".parse().unwrap(), false, 16).unwrap();
            let addr = l.local_addr().unwrap();
            assert!(nonblocking(l.as_raw_fd()));
            let mut ul = match Listener::new(l) {
                Ok(u) => u,
                Err((e, back)) => {
                    assert!(nonblocking(back.as_raw_fd()), "given back as it was, for epoll");
                    eprintln!("skipping: io_uring is unavailable here ({e})");
                    return;
                }
            };
            assert!(!nonblocking(ul.sock.as_raw_fd()), "an ACCEPT on it waits in the kernel");
            // Idle: the pending accept neither completes nor fails.
            assert!(tokio::time::timeout(Duration::from_millis(100), ul.accept()).await.is_err());
            // Straight to the accept, and (as after an EAGAIN) through a poll first.
            for poll_first in [false, true] {
                ul.poll_first = poll_first;
                let client = std::thread::spawn(move || {
                    let mut c = TcpStream::connect(addr).unwrap();
                    c.write_all(b"ping").unwrap();
                    let mut b = [0u8; 4];
                    c.read_exact(&mut b).unwrap();
                    b
                });
                let s = tokio::time::timeout(Duration::from_secs(5), ul.accept()).await.unwrap().unwrap();
                let (mut r, mut w) = s.split();
                let mut b = [0u8; 4];
                r.read_exact(&mut b).await.unwrap();
                assert_eq!(&b, b"ping");
                w.send_all(OutBuf::Vec(b"pong".to_vec()), false).await.unwrap();
                assert_eq!(&client.join().unwrap(), b"pong", "poll_first={poll_first}");
            }
        });
    }
}

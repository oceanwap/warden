//! Connection handoff (macOS). The kernel spreads connections over workers
//! sharing a port (`SO_REUSEPORT`) on Linux, not on macOS: there one worker
//! gets them all. So on macOS, with more than one worker, Warden owns the
//! port, accepts, and passes each connection to a ready worker over the
//! worker's Node IPC channel (fd 4, `NODE_CHANNEL_FD`): the `NODE_HANDLE`
//! message Node's own cluster module sends, with the socket attached
//! (SCM_RIGHTS). The shim gives it to the app's `node:http` server
//! (`server.emit("connection")`), under Node and Bun alike. From then on the
//! client and the worker talk directly: Warden never reads or writes a byte
//! of the traffic, and the app sees the client's own address.
//!
//! One connection in flight per worker, as Node does: the next waits for the
//! worker's `NODE_HANDLE_ACK`. A worker gets the next connection when it has
//! the fewest in flight (round robin among equals). A worker leaves the
//! rotation before it drains (`remove`); connections queued for it go to the
//! others. With no worker in the rotation Warden stops accepting, so new
//! connections wait in the kernel's backlog until one is back.
//!
//! A worker that cannot take a connection (at its file-descriptor limit)
//! answers `NODE_HANDLE_NACK`, and the connection goes round again, to the
//! least busy worker. Like Node (`MAX_HANDLE_RETRANSMISSIONS`), it is sent
//! again at most 3 times: refused a 4th time, it is closed, so a fleet out of
//! descriptors sheds connections instead of passing them around forever.
//!
//! A Bun.serve app takes part when its Bun has `server.adopt(fd)`: its shim
//! reports `adopt: true` with `listening`, and its connections go out as
//! `"type":"fd"` (the bare descriptor, which Bun hands to the shim as a
//! number without reading from it, so a TLS ClientHello is still there for
//! Bun.serve) instead of `"type":"net.Socket"`. Everything else is the same.
//!
//! The dispatcher runs on its own thread (with its own small runtime), so the
//! supervisor's loop (logs, IPC, timers) never delays a connection.

use std::collections::VecDeque;
use std::io::{self, Read as _};
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

/// What a worker's shim receives with each connection (`process.on("message")`).
pub const MESSAGE: &str = "warden:connection";

/// How long a worker may take to acknowledge a connection before it is
/// taken out of the rotation (the connection is closed: it may have it).
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How many times a refused (NACKed) connection is sent again before it is
/// closed: Node's `MAX_HANDLE_RETRANSMISSIONS`.
const MAX_RETRANSMISSIONS: u8 = 3;

/// At most one warning about closed refused connections this often.
const REFUSED_WARN_EVERY: Duration = Duration::from_secs(60);

/// The dispatcher of one app. Dropping it stops the thread and closes the port.
pub struct Handoff {
    ctl: mpsc::UnboundedSender<Ctl>,
    pub addr: SocketAddr,
}

/// How a worker takes its connections: the `type` of the `NODE_HANDLE` message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `"net.Socket"`: Node's own; the shim emits it on a node:http server.
    Socket,
    /// `"fd"`: the bare descriptor, for Bun.serve's `server.adopt(fd)`.
    Fd,
}

enum Ctl {
    Add(u64, OwnedFd, Kind),
    Remove(u64),
}

/// An accepted connection on its way to a worker.
struct Conn {
    fd: OwnedFd,
    /// Times a worker refused it (`NODE_HANDLE_NACK`).
    refused: u8,
}

/// From a worker's task back to the dispatcher.
enum Back {
    /// Not delivered: give it to another worker.
    Again(Conn),
    /// The worker refused it (NACK): again, up to `MAX_RETRANSMISSIONS` times.
    Refused(Conn),
    /// The worker's channel failed: out of the rotation. `true`: it did not
    /// answer (a hung worker); otherwise it is gone, and its exit says so.
    Gone(u64, bool),
}

impl Handoff {
    /// Listen on `addr` and start the dispatcher thread.
    pub fn start(app: &str, addr: SocketAddr) -> io::Result<Handoff> {
        let listener = crate::sys::listen_tcp(addr, false, 1024)?;
        let addr = listener.local_addr()?;
        let (tx, rx) = mpsc::unbounded_channel();
        let app = app.to_string();
        std::thread::Builder::new().name("handoff".into()).spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(rt) => rt,
                Err(e) => {
                    crate::error!(
                        "connection handoff could not start",
                        app = app,
                        error = e,
                        hint = "Warden could not create the dispatcher's runtime (out of memory or threads?)",
                    );
                    return;
                }
            };
            rt.block_on(run(app, OwnedFd::from(listener), rx));
        })?;
        Ok(Handoff { ctl: tx, addr })
    }

    /// `inst` is ready: connections may go to it over `channel` (its fd 4),
    /// as `kind` says.
    pub fn add(&self, inst: u64, channel: OwnedFd, kind: Kind) {
        let _ = self.ctl.send(Ctl::Add(inst, channel, kind));
    }

    /// `inst` stops getting connections (it drains, or is gone).
    pub fn remove(&self, inst: u64) {
        let _ = self.ctl.send(Ctl::Remove(inst));
    }
}

struct Worker {
    inst: u64,
    tx: mpsc::UnboundedSender<Conn>,
    /// Connections sent or queued, not yet acknowledged.
    pending: Arc<AtomicUsize>,
    /// Out of the rotation: whatever is still queued goes back.
    closed: Arc<AtomicBool>,
}

async fn run(app: String, listener: OwnedFd, mut ctl: mpsc::UnboundedReceiver<Ctl>) {
    let listener = match AsyncFd::with_interest(listener, Interest::READABLE) {
        Ok(l) => l,
        Err(e) => {
            crate::error!(
                "connection handoff could not start",
                app = app,
                error = e,
                hint = "the port's socket could not be watched; the app gets no connections",
            );
            return;
        }
    };
    let (back_tx, mut back_rx) = mpsc::unbounded_channel::<Back>();
    let mut workers: Vec<Worker> = Vec::new();
    let mut waiting: VecDeque<Conn> = VecDeque::new();
    let mut next = 0usize;
    let mut errors = 0u32;
    let mut refused = RefusedLog::default();
    loop {
        tokio::select! {
            c = ctl.recv() => match c {
                None => break,
                Some(Ctl::Add(inst, channel, kind)) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let pending = Arc::new(AtomicUsize::new(0));
                    let closed = Arc::new(AtomicBool::new(false));
                    tokio::spawn(deliver(inst, channel, kind, rx, pending.clone(), closed.clone(), back_tx.clone()));
                    workers.push(Worker { inst, tx, pending, closed });
                    while let Some(conn) = waiting.pop_front() {
                        dispatch(&workers, &mut next, conn, &mut waiting);
                    }
                }
                Some(Ctl::Remove(inst)) => drop_worker(&mut workers, inst),
            },
            b = back_rx.recv() => match b {
                Some(Back::Again(conn)) => dispatch(&workers, &mut next, conn, &mut waiting),
                Some(Back::Refused(conn)) => match again_after_refusal(conn) {
                    Some(conn) => dispatch(&workers, &mut next, conn, &mut waiting),
                    None => {
                        if let Some(closed) = refused.closed(Instant::now()) {
                            crate::warn!(
                                "connections refused by every worker they were sent to were closed",
                                app = app,
                                closed = closed,
                                tries = MAX_RETRANSMISSIONS + 1,
                                hint = "workers answer NACK when they cannot take a connection, usually at their \
                                        file-descriptor limit: raise it (`ulimit -n`, LimitNOFILE) or add workers; \
                                        this is said at most once a minute",
                            );
                        }
                    }
                },
                Some(Back::Gone(inst, hung)) => {
                    if hung && workers.iter().any(|w| w.inst == inst) {
                        crate::warn!(
                            "a worker stopped taking connections; it is out of the rotation",
                            app = app,
                            hint = "it did not acknowledge a connection within 5 s (a blocked event loop?); it is \
                                    back in the rotation when it is restarted",
                        );
                    }
                    drop_worker(&mut workers, inst);
                }
                None => {}
            },
            // Only while a worker can take them: otherwise they wait in the backlog.
            g = listener.readable(), if !workers.is_empty() => {
                let Ok(mut guard) = g else { break };
                loop {
                    match crate::sys::accept_nonblocking(listener.get_ref().as_fd()) {
                        Ok(conn) => {
                            errors = 0;
                            dispatch(&workers, &mut next, Conn { fd: conn, refused: 0 }, &mut waiting);
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            guard.clear_ready();
                            break;
                        }
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            // EMFILE and the like: say so once, back off a little.
                            errors += 1;
                            if errors == 1 {
                                crate::warn!(
                                    "accepting a connection failed",
                                    app = app,
                                    error = e,
                                    hint = "Warden is out of file descriptors (raise LimitNOFILE / ulimit -n) or memory; \
                                            it retries",
                                );
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// A refused connection: back for another try, or `None` once it has been
/// refused `MAX_RETRANSMISSIONS + 1` times (dropped, which closes it).
fn again_after_refusal(mut conn: Conn) -> Option<Conn> {
    conn.refused = conn.refused.saturating_add(1);
    (conn.refused <= MAX_RETRANSMISSIONS).then_some(conn)
}

/// Counts connections closed after refusals, and says when to warn: at the
/// first, then at most once per `REFUSED_WARN_EVERY` (with the count since).
#[derive(Default)]
struct RefusedLog {
    last: Option<Instant>,
    unreported: u64,
}

impl RefusedLog {
    fn closed(&mut self, now: Instant) -> Option<u64> {
        self.unreported += 1;
        if self.last.is_some_and(|t| now.duration_since(t) < REFUSED_WARN_EVERY) {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.unreported))
    }
}

fn drop_worker(workers: &mut Vec<Worker>, inst: u64) {
    if let Some(i) = workers.iter().position(|w| w.inst == inst) {
        let w = workers.remove(i);
        w.closed.store(true, Ordering::Release);
        // Dropping `tx` ends its task once the queue is empty; what is
        // queued comes back as `Again`.
    }
}

/// The worker with the fewest connections in flight, starting the search at
/// `next` so equals take turns. None in the rotation: it waits.
fn dispatch(workers: &[Worker], next: &mut usize, conn: Conn, waiting: &mut VecDeque<Conn>) {
    let Some(i) = pick(workers.iter().map(|w| w.pending.load(Ordering::Acquire)), *next) else {
        waiting.push_back(conn);
        return;
    };
    *next = i + 1;
    let w = &workers[i];
    w.pending.fetch_add(1, Ordering::AcqRel);
    if let Err(mpsc::error::SendError(conn)) = w.tx.send(conn) {
        w.pending.fetch_sub(1, Ordering::AcqRel);
        waiting.push_back(conn);
    }
}

/// Index of the smallest load, the first one found from `start` on (wrapping).
fn pick(loads: impl Iterator<Item = usize>, start: usize) -> Option<usize> {
    let loads: Vec<usize> = loads.collect();
    let n = loads.len();
    (0..n).map(|k| (start + k) % n).min_by_key(|&i| loads[i])
}

/// One worker's deliveries: send a connection, wait for its acknowledgement,
/// close Warden's copy, next.
async fn deliver(
    inst: u64,
    channel: OwnedFd,
    kind: Kind,
    mut rx: mpsc::UnboundedReceiver<Conn>,
    pending: Arc<AtomicUsize>,
    closed: Arc<AtomicBool>,
    back: mpsc::UnboundedSender<Back>,
) {
    let channel = std::os::unix::net::UnixStream::from(channel);
    let channel = match channel.set_nonblocking(true).and_then(|()| AsyncFd::new(channel)) {
        Ok(c) => c,
        Err(_) => {
            let _ = back.send(Back::Gone(inst, false));
            return give_back(&mut rx, &back, &pending).await;
        }
    };
    let line = handle_line(kind);
    let mut buf = Vec::with_capacity(256);
    while let Some(conn) = rx.recv().await {
        if closed.load(Ordering::Acquire) {
            pending.fetch_sub(1, Ordering::AcqRel);
            let _ = back.send(Back::Again(conn));
            continue;
        }
        if send(&channel, line.as_bytes(), &conn.fd).await.is_err() {
            // Not delivered (the worker is gone): someone else takes it.
            pending.fetch_sub(1, Ordering::AcqRel);
            let _ = back.send(Back::Again(conn));
            let _ = back.send(Back::Gone(inst, false));
            return give_back(&mut rx, &back, &pending).await;
        }
        let acked = tokio::time::timeout(ACK_TIMEOUT, ack(&channel, &mut buf)).await;
        pending.fetch_sub(1, Ordering::AcqRel);
        match acked {
            Ok(Ok(true)) => drop(conn),
            // NACK: the worker could not take it; it is still ours to give.
            Ok(Ok(false)) => {
                let _ = back.send(Back::Refused(conn));
            }
            // No answer: the worker may hold it, so it is closed, not resent.
            Ok(Err(_)) | Err(_) => {
                drop(conn);
                let _ = back.send(Back::Gone(inst, acked.is_err()));
                return give_back(&mut rx, &back, &pending).await;
            }
        }
    }
}

/// Everything still queued for a worker that left goes to the others.
async fn give_back(rx: &mut mpsc::UnboundedReceiver<Conn>, back: &mpsc::UnboundedSender<Back>, pending: &AtomicUsize) {
    rx.close();
    while let Some(conn) = rx.recv().await {
        pending.fetch_sub(1, Ordering::AcqRel);
        let _ = back.send(Back::Again(conn));
    }
}

/// The `NODE_HANDLE` line a connection goes out with.
fn handle_line(kind: Kind) -> String {
    let ty = match kind {
        Kind::Socket => "net.Socket",
        Kind::Fd => "fd",
    };
    format!(r#"{{"cmd":"NODE_HANDLE","type":"{ty}","msg":"{MESSAGE}","key":null}}"#) + "\n"
}

/// Write `line` with `conn` attached to the first byte.
async fn send(channel: &AsyncFd<UnixStream>, line: &[u8], conn: &OwnedFd) -> io::Result<()> {
    let mut sent = 0;
    while sent < line.len() {
        let mut guard = channel.writable().await?;
        let r = guard.try_io(|c| {
            if sent == 0 {
                crate::sys::send_with_fd(c.get_ref().as_fd(), line, conn.as_fd())
            } else {
                crate::sys::send(c.get_ref().as_fd(), &line[sent..], false)
            }
        });
        match r {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(n)) => sent += n,
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => {}
        }
    }
    Ok(())
}

/// Read lines until the worker's `NODE_HANDLE_ACK` (true) or `NODE_HANDLE_NACK`
/// (false); anything else on the channel is skipped.
async fn ack(channel: &AsyncFd<UnixStream>, buf: &mut Vec<u8>) -> io::Result<bool> {
    loop {
        while let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=end).collect();
            if let Some(v) = acknowledgement(&line) {
                return Ok(v);
            }
        }
        let mut guard = channel.readable().await?;
        let mut chunk = [0u8; 512];
        match guard.try_io(|c| (&mut c.get_ref()).read(&mut chunk)) {
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Ok(n)) => buf.extend_from_slice(&chunk[..n]),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => return Err(e),
            Err(_would_block) => {}
        }
    }
}

/// `Some(true)` for an ACK line, `Some(false)` for a NACK, `None` otherwise.
fn acknowledgement(line: &[u8]) -> Option<bool> {
    let v: serde_json::Value = serde_json::from_slice(line).ok()?;
    match v.get("cmd")?.as_str()? {
        "NODE_HANDLE_ACK" => Some(true),
        "NODE_HANDLE_NACK" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_busy_worker_gets_it_and_equals_take_turns() {
        assert_eq!(pick([0, 0, 0].into_iter(), 0), Some(0));
        assert_eq!(pick([0, 0, 0].into_iter(), 1), Some(1));
        assert_eq!(pick([0, 0, 0].into_iter(), 5), Some(2), "wraps");
        assert_eq!(pick([1, 0, 1].into_iter(), 0), Some(1), "fewest in flight");
        assert_eq!(pick([1, 1, 0].into_iter(), 2), Some(2));
        assert_eq!(pick(std::iter::empty(), 0), None, "nobody: it waits");
    }

    #[test]
    fn acknowledgements_are_read_and_other_lines_skipped() {
        assert_eq!(acknowledgement(br#"{"cmd":"NODE_HANDLE_ACK"}"#), Some(true));
        assert_eq!(acknowledgement(b"{\"cmd\":\"NODE_HANDLE_NACK\"}\n"), Some(false));
        assert_eq!(acknowledgement(br#"{"cmd":"NODE_CLUSTER"}"#), None);
        assert_eq!(acknowledgement(b"not json"), None);
    }

    #[test]
    fn a_worker_that_adopts_descriptors_gets_type_fd() {
        let parse = |k| serde_json::from_str::<serde_json::Value>(handle_line(k).trim_end()).unwrap();
        let socket = parse(Kind::Socket);
        assert_eq!(socket["cmd"], "NODE_HANDLE");
        assert_eq!(socket["type"], "net.Socket");
        assert_eq!(socket["msg"], MESSAGE);
        let fd = parse(Kind::Fd);
        assert_eq!(fd["type"], "fd");
        assert_eq!(fd["msg"], MESSAGE, "the same message either way");
        assert!(handle_line(Kind::Fd).ends_with('\n'));
    }

    #[test]
    fn a_refused_connection_is_sent_again_three_times_like_node_then_closed() {
        let (fd, _peer) = crate::sys::socketpair_cloexec().unwrap();
        let mut conn = Conn { fd, refused: 0 };
        for tries in 1..=MAX_RETRANSMISSIONS {
            conn = again_after_refusal(conn).unwrap_or_else(|| panic!("closed after {tries} refusals"));
            assert_eq!(conn.refused, tries);
        }
        assert!(again_after_refusal(conn).is_none(), "the 4th refusal closes it");
    }

    #[test]
    fn closed_refused_connections_are_reported_at_most_once_a_minute() {
        let mut log = RefusedLog::default();
        let t0 = Instant::now();
        assert_eq!(log.closed(t0), Some(1), "the first one is said at once");
        assert_eq!(log.closed(t0 + Duration::from_secs(1)), None);
        assert_eq!(log.closed(t0 + Duration::from_secs(59)), None);
        assert_eq!(log.closed(t0 + REFUSED_WARN_EVERY), Some(3), "then with the count since");
        assert_eq!(log.closed(t0 + REFUSED_WARN_EVERY + Duration::from_secs(1)), None);
    }

    /// A worker that refuses every connection (NACK) gets it 4 times, as
    /// Node's cluster would send it, then Warden closes it: the client sees
    /// the end of the connection instead of Warden resending it forever.
    #[test]
    fn a_connection_refused_four_times_is_closed_not_resent_forever() {
        use std::io::{Read, Write};
        let (ours, theirs) = crate::sys::socketpair_cloexec().unwrap();
        let h = Handoff::start("t", "127.0.0.1:0".parse().unwrap()).unwrap();
        h.add(1, ours, Kind::Socket);
        let mut client = std::net::TcpStream::connect(h.addr).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let theirs = std::os::unix::net::UnixStream::from(theirs);
        theirs.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        let mut sends = 0;
        while let Ok((line, fd)) = crate::sys::recv_with_fd(theirs.as_fd(), 4096) {
            if line.is_empty() {
                break;
            }
            assert!(String::from_utf8_lossy(&line).contains("NODE_HANDLE"));
            drop(fd.expect("a descriptor came with it"));
            sends += 1;
            assert!(sends <= 10, "resent forever");
            (&theirs).write_all(b"{\"cmd\":\"NODE_HANDLE_NACK\"}\n").unwrap();
        }
        assert_eq!(sends, 1 + MAX_RETRANSMISSIONS as usize, "sent once, then again 3 times");
        let mut got = [0u8; 1];
        assert_eq!(client.read(&mut got).unwrap(), 0, "closed by Warden");
        // The worker is still in the rotation: the next connection reaches it.
        let _c2 = std::net::TcpStream::connect(h.addr).unwrap();
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (line, fd) = crate::sys::recv_with_fd(theirs.as_fd(), 4096).unwrap();
        assert!(String::from_utf8_lossy(&line).contains("NODE_HANDLE") && fd.is_some());
    }

    /// A worker added with `Kind::Fd` gets its connections as `"type":"fd"`,
    /// with the descriptor attached all the same.
    #[test]
    fn an_adopting_worker_gets_its_connections_as_fd() {
        use std::io::Write;
        let (ours, theirs) = crate::sys::socketpair_cloexec().unwrap();
        let h = Handoff::start("t", "127.0.0.1:0".parse().unwrap()).unwrap();
        h.add(1, ours, Kind::Fd);
        let _client = std::net::TcpStream::connect(h.addr).unwrap();
        let theirs = std::os::unix::net::UnixStream::from(theirs);
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (line, fd) = crate::sys::recv_with_fd(theirs.as_fd(), 4096).unwrap();
        let line = String::from_utf8(line).unwrap();
        assert!(line.contains(r#""type":"fd""#) && line.contains(MESSAGE), "{line}");
        assert!(fd.is_some(), "a descriptor came with it");
        (&theirs).write_all(b"{\"cmd\":\"NODE_HANDLE_ACK\"}\n").unwrap();
    }

    /// A connection accepted here arrives at the other end of the channel
    /// as a working socket, and the next one waits for the acknowledgement.
    #[test]
    fn a_connection_is_passed_with_its_message_and_acknowledged() {
        use std::io::{Read, Write};
        let (ours, theirs) = crate::sys::socketpair_cloexec().unwrap();
        let h = Handoff::start("t", "127.0.0.1:0".parse().unwrap()).unwrap();
        h.add(1, ours, Kind::Socket);
        let mut client = std::net::TcpStream::connect(h.addr).unwrap();
        // The worker's side: read the message and the descriptor.
        let theirs = std::os::unix::net::UnixStream::from(theirs);
        theirs.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let (line, fd) = crate::sys::recv_with_fd(theirs.as_fd(), 4096).unwrap();
        let line = String::from_utf8(line).unwrap();
        assert!(line.contains("NODE_HANDLE") && line.contains(MESSAGE) && line.ends_with('\n'), "{line}");
        let fd = fd.expect("a descriptor came with it");
        (&theirs).write_all(b"{\"cmd\":\"NODE_HANDLE_ACK\"}\n").unwrap();
        // It is the client's connection.
        let mut conn = std::net::TcpStream::from(fd);
        conn.set_nonblocking(false).unwrap();
        client.write_all(b"ping").unwrap();
        let mut got = [0u8; 4];
        conn.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"ping");
        conn.write_all(b"pong").unwrap();
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"pong", "client and worker talk directly");
        // A second connection is delivered too.
        let _c2 = std::net::TcpStream::connect(h.addr).unwrap();
        let (line2, fd2) = crate::sys::recv_with_fd(theirs.as_fd(), 4096).unwrap();
        assert!(String::from_utf8(line2).unwrap().contains("NODE_HANDLE") && fd2.is_some());
    }
}

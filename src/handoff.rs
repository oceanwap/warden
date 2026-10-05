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
//! The dispatcher runs on its own thread (with its own small runtime), so the
//! supervisor's loop (logs, IPC, timers) never delays a connection.

use std::collections::VecDeque;
use std::io::{self, Read as _};
use std::net::SocketAddr;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::Interest;
use tokio::io::unix::AsyncFd;
use tokio::sync::mpsc;

/// What a worker's shim receives with each connection (`process.on("message")`).
pub const MESSAGE: &str = "warden:connection";

/// How long a worker may take to acknowledge a connection before it is
/// taken out of the rotation (the connection is closed: it may have it).
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// The dispatcher of one app. Dropping it stops the thread and closes the port.
pub struct Handoff {
    ctl: mpsc::UnboundedSender<Ctl>,
    pub addr: SocketAddr,
}

enum Ctl {
    Add(u64, OwnedFd),
    Remove(u64),
}

/// From a worker's task back to the dispatcher.
enum Back {
    /// Not delivered: give it to another worker.
    Again(OwnedFd),
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
                    crate::error!("connection handoff could not start", app = app, error = e);
                    return;
                }
            };
            rt.block_on(run(app, OwnedFd::from(listener), rx));
        })?;
        Ok(Handoff { ctl: tx, addr })
    }

    /// `inst` is ready: connections may go to it over `channel` (its fd 4).
    pub fn add(&self, inst: u64, channel: OwnedFd) {
        let _ = self.ctl.send(Ctl::Add(inst, channel));
    }

    /// `inst` stops getting connections (it drains, or is gone).
    pub fn remove(&self, inst: u64) {
        let _ = self.ctl.send(Ctl::Remove(inst));
    }
}

struct Worker {
    inst: u64,
    tx: mpsc::UnboundedSender<OwnedFd>,
    /// Connections sent or queued, not yet acknowledged.
    pending: Arc<AtomicUsize>,
    /// Out of the rotation: whatever is still queued goes back.
    closed: Arc<AtomicBool>,
}

async fn run(app: String, listener: OwnedFd, mut ctl: mpsc::UnboundedReceiver<Ctl>) {
    let listener = match AsyncFd::with_interest(listener, Interest::READABLE) {
        Ok(l) => l,
        Err(e) => {
            crate::error!("connection handoff could not start", app = app, error = e);
            return;
        }
    };
    let (back_tx, mut back_rx) = mpsc::unbounded_channel::<Back>();
    let mut workers: Vec<Worker> = Vec::new();
    let mut waiting: VecDeque<OwnedFd> = VecDeque::new();
    let mut next = 0usize;
    let mut errors = 0u32;
    loop {
        tokio::select! {
            c = ctl.recv() => match c {
                None => break,
                Some(Ctl::Add(inst, channel)) => {
                    let (tx, rx) = mpsc::unbounded_channel();
                    let pending = Arc::new(AtomicUsize::new(0));
                    let closed = Arc::new(AtomicBool::new(false));
                    tokio::spawn(deliver(inst, channel, rx, pending.clone(), closed.clone(), back_tx.clone()));
                    workers.push(Worker { inst, tx, pending, closed });
                    while let Some(conn) = waiting.pop_front() {
                        dispatch(&workers, &mut next, conn, &mut waiting);
                    }
                }
                Some(Ctl::Remove(inst)) => drop_worker(&mut workers, inst),
            },
            b = back_rx.recv() => match b {
                Some(Back::Again(conn)) => dispatch(&workers, &mut next, conn, &mut waiting),
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
                            dispatch(&workers, &mut next, conn, &mut waiting);
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
                                crate::warn!("accepting a connection failed", app = app, error = e);
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
fn dispatch(workers: &[Worker], next: &mut usize, conn: OwnedFd, waiting: &mut VecDeque<OwnedFd>) {
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
    mut rx: mpsc::UnboundedReceiver<OwnedFd>,
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
    let line = format!(r#"{{"cmd":"NODE_HANDLE","type":"net.Socket","msg":"{MESSAGE}","key":null}}"#) + "\n";
    let mut buf = Vec::with_capacity(256);
    while let Some(conn) = rx.recv().await {
        if closed.load(Ordering::Acquire) {
            pending.fetch_sub(1, Ordering::AcqRel);
            let _ = back.send(Back::Again(conn));
            continue;
        }
        if send(&channel, line.as_bytes(), &conn).await.is_err() {
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
                let _ = back.send(Back::Again(conn));
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
async fn give_back(
    rx: &mut mpsc::UnboundedReceiver<OwnedFd>,
    back: &mpsc::UnboundedSender<Back>,
    pending: &AtomicUsize,
) {
    rx.close();
    while let Some(conn) = rx.recv().await {
        pending.fetch_sub(1, Ordering::AcqRel);
        let _ = back.send(Back::Again(conn));
    }
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

    /// A connection accepted here arrives at the other end of the channel
    /// as a working socket, and the next one waits for the acknowledgement.
    #[test]
    fn a_connection_is_passed_with_its_message_and_acknowledged() {
        use std::io::{Read, Write};
        let (ours, theirs) = crate::sys::socketpair_cloexec().unwrap();
        let h = Handoff::start("t", "127.0.0.1:0".parse().unwrap()).unwrap();
        h.add(1, ours);
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

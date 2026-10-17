//! Writing a response: the output half of a connection (`Conn`), the sends it
//! can make, and what is left over when a socket cannot take a response whole.

use super::cache::MemFile;
use super::open::read_body;
use super::tls::TlsIo;
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bytes to send: a buffer built for this response, or a range of a cached
/// one (no copy).
pub(super) enum OutBuf {
    Vec(Vec<u8>),
    Shared(Arc<[u8]>, std::ops::Range<usize>),
}

impl OutBuf {
    pub(super) fn as_slice(&self) -> &[u8] {
        match self {
            OutBuf::Vec(v) => v,
            OutBuf::Shared(b, r) => &b[r.clone()],
        }
    }

    /// Without the first `n` bytes.
    pub(super) fn advance(self, n: usize) -> OutBuf {
        match self {
            OutBuf::Vec(mut v) => {
                v.drain(..n);
                OutBuf::Vec(v)
            }
            OutBuf::Shared(b, r) => OutBuf::Shared(b, r.start + n..r.end),
        }
    }
}

impl From<String> for OutBuf {
    fn from(s: String) -> Self {
        OutBuf::Vec(s.into_bytes())
    }
}

impl From<Vec<u8>> for OutBuf {
    fn from(v: Vec<u8>) -> Self {
        OutBuf::Vec(v)
    }
}

/// Where the bytes of a file response come from: a file opened for this
/// request, or the memfd of a cached response (kept alive by sharing it, so a
/// response that is finished later still has it).
pub(super) enum Source {
    File(std::fs::File),
    Memfd(Arc<MemFile>),
}

impl Source {
    fn file(&self) -> &std::fs::File {
        match self {
            Source::File(f) => f,
            Source::Memfd(m) => &m.file,
        }
    }

    /// An owned file to read from (the health socket's way of sending a body).
    fn into_file(self) -> std::io::Result<std::fs::File> {
        match self {
            Source::File(f) => Ok(f),
            Source::Memfd(m) => m.file.try_clone(),
        }
    }
}

pub(super) enum Rest {
    /// Bytes in memory: `out` from `at` on.
    Bytes { out: OutBuf, at: usize },
    /// What is left of a response head, then `count` bytes of `src` from `off`.
    File { head: Vec<u8>, src: Source, off: u64, count: u64 },
}

/// The writing half of a client connection. TCP bodies go out with
/// sendfile(2); the Unix health socket uses a plain copy.
pub(super) enum Conn<'a> {
    Tcp(tokio::net::tcp::WriteHalf<'a>),
    Unix(tokio::net::unix::WriteHalf<'a>),
    /// HTTPS: the write half of a finished TLS handshake (`tls.rs`). Reads
    /// and writes go through per-connection encryption, so bodies are copied
    /// from the file instead of sendfile'd (ciphertext cannot be shared: the
    /// keys differ on every connection). The accept loop's raw quick path
    /// never applies here: a handshake has to run first.
    Tls(tokio::io::WriteHalf<TlsIo<tokio::net::TcpStream>>),
    /// In-memory sink: the HTTP/2 adaptor answering through the normal H1
    /// handler, whose H1 bytes are then translated to H2 frames. Every
    /// decision (auth, paths, conditionals, ranges, compression, the cache)
    /// is shared with the plain path this way, instead of mirrored.
    Mem(Vec<u8>),
    /// The accept loop answering a new connection's first request itself
    /// (`first_request`): never waits.
    Raw(RawSink<'a>),
}

/// A socket written to without waiting. A write the socket cannot take whole
/// is accepted all the same: what is left is kept, for the connection's task
/// to finish (`Rest`). Every response is a single write, made last, so a
/// second one before the first is finished is a bug and fails the request.
pub(super) struct RawSink<'a> {
    fd: BorrowedFd<'a>,
    rest: Option<Rest>,
    /// Anything has been handed to the socket.
    wrote: bool,
}

/// The most the accept loop hands a socket in one sendfile. The worker is one
/// thread, and a file's rest goes on as a task, which sends it in pieces and
/// lets the other connections run between them (`SEND_CHUNK`).
pub(super) const INLINE_SEND_MAX: u64 = 256 * 1024;

impl RawSink<'_> {
    fn check_idle(&self) -> std::io::Result<()> {
        match self.rest {
            None => Ok(()),
            Some(_) => Err(std::io::Error::other("a second write before the first was finished")),
        }
    }

    fn bytes(&mut self, out: OutBuf) -> std::io::Result<()> {
        use std::io::ErrorKind::{Interrupted, WouldBlock};
        self.check_idle()?;
        let at = match crate::sys::send(self.fd, out.as_slice(), false) {
            Ok(n) => n,
            Err(e) if matches!(e.kind(), WouldBlock | Interrupted) => 0,
            Err(e) => return Err(e),
        };
        self.wrote |= at > 0;
        if at < out.as_slice().len() {
            self.rest = Some(Rest::Bytes { out, at });
        }
        Ok(())
    }

    /// `head`, then `count` bytes of `src` from `offset`. The source moves
    /// into `rest` when it is needed there, so nothing can fail once bytes
    /// are on the wire.
    fn file(&mut self, head: &[u8], src: Source, offset: u64, count: u64) -> std::io::Result<()> {
        use std::io::ErrorKind::{Interrupted, UnexpectedEof, WouldBlock};
        self.check_idle()?;
        // A body the task would cork goes out by the task, all of it: a part
        // sent here would be uncorked, and the packets of its end half empty.
        if count >= CORK_MIN {
            self.rest = Some(Rest::File { head: head.to_vec(), src, off: offset, count });
            return Ok(());
        }
        let now = count.min(INLINE_SEND_MAX);
        let mut off = offset as i64;
        let n = match crate::sys::sendfile_head(self.fd, src.file().as_fd(), &mut off, now as usize, head) {
            // Nothing could be sent: the socket is full.
            Err(e) if matches!(e.kind(), WouldBlock | Interrupted) => 0,
            Err(e) => return Err(e),
            // The file shrank under us: the promised Content-Length can't be met.
            Ok(0) if head.is_empty() && now > 0 => {
                return Err(std::io::Error::new(UnexpectedEof, "file truncated while sending"));
            }
            Ok(n) => n,
        };
        let (h, b) = crate::sys::split_head_body(n, head.len(), now)
            .ok_or_else(|| std::io::Error::other("sendfile reported more bytes than were asked for"))?;
        self.wrote |= n > 0;
        if h < head.len() || b < count {
            self.rest = Some(Rest::File { head: head[h..].to_vec(), src, off: offset + b, count: count - b });
        }
        Ok(())
    }
}

impl<'a> Conn<'a> {
    /// The accept loop answering a request itself (`first_request`): writes
    /// never wait.
    pub(super) fn raw(fd: BorrowedFd<'a>) -> Conn<'a> {
        Conn::Raw(RawSink { fd, rest: None, wrote: false })
    }

    /// Of a `raw` connection: what the socket did not take of the response,
    /// and whether anything went out at all.
    pub(super) fn unsent(&mut self) -> (Option<Rest>, bool) {
        match self {
            Conn::Raw(r) => (r.rest.take(), r.wrote),
            _ => (None, false),
        }
    }

    pub(super) async fn write_all(&mut self, b: impl Into<OutBuf>) -> std::io::Result<()> {
        let b = b.into();
        match self {
            Conn::Tcp(w) => w.write_all(b.as_slice()).await,
            Conn::Unix(w) => w.write_all(b.as_slice()).await,
            Conn::Tls(w) => w.write_all(b.as_slice()).await,
            Conn::Mem(v) => {
                v.extend_from_slice(b.as_slice());
                Ok(())
            }
            Conn::Raw(r) => r.bytes(b),
        }
    }

    pub(super) async fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Tcp(w) => w.flush().await,
            Conn::Unix(w) => w.flush().await,
            Conn::Tls(w) => w.flush().await,
            Conn::Mem(_) | Conn::Raw(_) => Ok(()),
        }
    }

    /// The response `head` (possibly empty), then `count` bytes of `src` from
    /// `offset`: on TCP by page reference (sendfile; the head goes in the
    /// same system call on macOS, and in the same packet on Linux); on the
    /// Unix health socket and on TLS a plain copy (TLS encrypts
    /// per connection, so the kernel cannot move the pages as they are).
    pub(super) async fn send_file(
        &mut self,
        head: &[u8],
        src: Source,
        offset: u64,
        count: u64,
    ) -> std::io::Result<u64> {
        match self {
            Conn::Tcp(w) => sendfile_all(w.as_ref(), head, src.file(), offset, count).await,
            Conn::Unix(w) => copy_file(w, head, src, offset, count).await,
            Conn::Tls(w) => copy_file(w, head, src, offset, count).await,
            Conn::Mem(v) => {
                // No socket to fill: the whole range goes into memory (under
                // TLS there is no sendfile anyway: every byte is encrypted in
                // userspace before it goes out).
                v.extend_from_slice(head);
                let from = v.len();
                v.resize(from + count as usize, 0);
                let (_, buf) = read_body(src.into_file()?, std::mem::take(v), from, offset).await?;
                *v = buf;
                Ok(count)
            }
            Conn::Raw(r) => r.file(head, src, offset, count).map(|()| count),
        }
    }
}

/// The head, then `count` bytes of `src` from `offset`, copied through
/// userspace: for transports that transform the bytes (TLS encrypts per
/// connection), where sendfile's page reference cannot apply.
async fn copy_file(
    w: &mut (impl AsyncWrite + Unpin),
    head: &[u8],
    src: Source,
    offset: u64,
    count: u64,
) -> std::io::Result<u64> {
    use tokio::io::AsyncWriteExt;
    w.write_all(head).await?;
    let mut f = tokio::fs::File::from_std(src.into_file()?);
    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
    }
    tokio::io::copy(&mut f.take(count), w).await
}

/// The most one sendfile(2) call is asked to move. The worker is one thread:
/// while a call runs nobody else is served, and on a loopback or fast local
/// link the kernel takes a whole multi-megabyte file in one call (10 MB:
/// 5 ms, during which every other request on the worker waited). After each
/// chunk the connection yields, so the others get their turn: a 1 KB request
/// next to a 10 MB download took 5.4 ms and takes 0.34 ms (docs/benchmarks.md).
/// Files up to this size still go in one call, as before; a smaller chunk
/// (256 KB) would shorten the wait further but made files of 0.5 to 1 MB
/// slower (an extra reader wake-up per chunk). Big files cost the same CPU.
const SEND_CHUNK: u64 = 1 << 20;

/// Bodies of this size and more go out with TCP_CORK: every segment full, the
/// reader woken less often. Measured on loopback: a 2 MB file 1.06 ms
/// uncorked, 0.70 corked; 10 MB 5.5 ms, 3.5. Below it corking costs more than
/// it saves (a 256 KB file: 41 us of CPU uncorked, 97 corked), and between
/// 1 and 1.25 MB the two are within the noise of this machine.
const CORK_MIN: u64 = 5 << 18;

/// TCP_CORK on for the length of a big body; off again, which sends what is
/// held, however the transfer ends.
struct Corked<'a>(BorrowedFd<'a>);

impl Drop for Corked<'_> {
    fn drop(&mut self) {
        let _ = crate::sys::set_tcp_cork(self.0, false);
    }
}

/// Zero-copy body: the kernel moves file pages to the socket, no userspace
/// buffer, after the response `head` (empty when it is already part of
/// what the file holds). In chunks of `SEND_CHUNK`, yielding between them;
/// loops on partial sends and waits for writability on EAGAIN. Where the
/// system can, the head and the start of the body are one system call and
/// one packet (`sys::sendfile_head`).
async fn sendfile_all(
    sock: &tokio::net::TcpStream,
    head: &[u8],
    file: &std::fs::File,
    offset: u64,
    count: u64,
) -> std::io::Result<u64> {
    let mut head_left = head;
    let mut off = offset as i64;
    let mut left = count;
    let _cork =
        (count >= CORK_MIN && crate::sys::set_tcp_cork(sock.as_fd(), true).is_ok()).then(|| Corked(sock.as_fd()));
    while !head_left.is_empty() || left > 0 {
        sock.writable().await?;
        let chunk = left.min(SEND_CHUNK) as usize;
        let asked = head_left.len() + chunk;
        let res = sock.try_io(tokio::io::Interest::WRITABLE, || {
            crate::sys::sendfile_head(sock.as_fd(), file.as_fd(), &mut off, chunk, head_left)
        });
        match res {
            // The file shrank under us: the promised Content-Length can't be
            // met, so the connection must close.
            Ok(0) => {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "file truncated while sending"));
            }
            Ok(n) => match crate::sys::split_head_body(n, head_left.len(), left) {
                Some((h, b)) => {
                    head_left = &head_left[h..];
                    left -= b;
                    // A whole chunk went in and there is more: let the other
                    // connections run before the next one. (A short send means
                    // the socket is full; waiting for it does that already.)
                    if left > 0 && n >= asked {
                        tokio::task::yield_now().await;
                    }
                }
                None => return Err(std::io::Error::other("sendfile reported more bytes than were asked for")),
            },
            // Nothing of this call went out (`sendfile_head` reports a head that
            // did as an Ok, also when the body then met EAGAIN or EINTR), so
            // trying again cannot send the head twice.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(count)
}

/// `Conn::Tls` copies head + file range through encryption: the client
/// decrypts exactly those bytes (no sendfile under TLS: every byte is
/// encrypted in userspace before it goes out). Runs on every platform,
/// unlike the Linux-only connection tests.
#[cfg(test)]
mod tests {
    use super::super::tls;
    use super::*;

    #[test]
    fn tls_copies_head_and_file_range() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            use tokio::io::AsyncReadExt;
            let dir = tls::test_dir("conn");
            let body: Vec<u8> = (0..50_000).map(|i| (i % 251) as u8).collect();
            std::fs::write(dir.join("f.bin"), &body).unwrap();
            let (cert, key) = tls::test_cert_files(&dir);
            let cert_pem = std::fs::read(&cert).unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(tls::load(&cert, &key).expect("the test chain loads"));
            // A real loopback socket: `Conn::Tls` holds a socket stream.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let task_dir = dir.clone();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.expect("a connection");
                let tls = tls::accept(&acceptor.clone(), socket).await.expect("the handshake runs");
                let (_, w) = tokio::io::split(tls);
                let mut conn = Conn::Tls(w);
                let file = std::fs::File::open(task_dir.join("f.bin")).unwrap();
                let n = conn.send_file(b"HEAD:", Source::File(file), 100, 1000).await.unwrap();
                assert_eq!(n, 1000);
            });
            let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut client = tls::test_client(b"http/1.1", &cert_pem)
                .connect("localhost".try_into().unwrap(), socket)
                .await
                .expect("the client handshakes");
            let mut got = vec![0u8; 5 + 1000];
            client.read_exact(&mut got).await.unwrap();
            assert_eq!(&got[..5], b"HEAD:");
            assert_eq!(&got[5..], &body[100..1100]);
            server.await.unwrap();
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}

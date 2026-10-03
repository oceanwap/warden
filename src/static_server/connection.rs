//! A client connection: requests in a loop while keep-alive holds, and the
//! quick path that answers a new connection's first request from the accept
//! loop.

use super::Site;
use super::conn::{Conn, Rest};
use super::handler::{Scratch, handle};
use super::head::{Head, HeadBuf, Request, find_head, parse_head, read_head};
use super::idle;
use super::response::respond_error;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::Instant;

/// Where a connection's task takes over: what `first_request` leaves it.
pub(super) struct Resume {
    /// The bytes read so far: a head not complete yet, a request the quick
    /// path leaves to the normal one, or what follows a request already
    /// answered.
    head: HeadBuf,
    /// The rest of a response that did not fit the socket.
    pending: Option<Pending>,
    /// A request was answered already: the wait for the next is the one
    /// between requests (the longer limit), not the first.
    answered: bool,
}

impl Resume {
    /// From the start: nothing answered, `head` as it stands.
    pub(super) fn new(head: HeadBuf) -> Resume {
        Resume { head, pending: None, answered: false }
    }
}

/// The part of a response `first_request` could not send yet.
struct Pending {
    rest: Rest,
    /// Keep the connection for more requests afterwards.
    keep: bool,
}

/// What became of a new connection in the accept loop.
pub(super) enum First {
    /// Finished: the response went out and the connection is closed (or the
    /// client had gone away).
    Done,
    /// Carry on as a task.
    Go { fd: OwnedFd, resume: Resume },
}

/// One client connection: requests in a loop while keep-alive holds.
///
/// A drain does not close idle keep-alive connections: a client may be
/// sending its next request at that very moment, and would see it fail
/// (found by `cargo xtask chaos`). A request that arrives while the worker
/// drains is answered with `Connection: close`, like the shim does for
/// Bun and Node apps; a connection still idle when the drain ends closes
/// as the worker exits.
pub(super) async fn connection<R>(mut r: R, mut w: Conn<'_>, site: Arc<Site>, watch: &idle::Guard, resume: Resume)
where
    R: tokio::io::AsyncRead + Unpin,
{
    let Resume { mut head, pending, answered } = resume;
    if let Some(p) = pending {
        // `first_request` sent part of a response and the socket was full:
        // the rest goes first. It counts as a request in flight, so a drain
        // waits for it as it does for any other.
        let busy = site.busy();
        let sent = match p.rest {
            Rest::Bytes { out, at } => w.write_all(out.advance(at)).await,
            Rest::File { head, src, off, count } => w.send_file(&head, src, off, count).await.map(|_| ()),
        };
        drop(busy);
        if sent.is_err() || !p.keep {
            head.give_back(&site);
            return;
        }
    }
    serve_requests(&mut r, &mut w, &site, watch, &mut head, answered).await;
    head.give_back(&site);
}

/// `answered`: a request has been answered on this connection already, so the
/// wait for the next is the one between requests, not the first.
async fn serve_requests<R>(
    r: &mut R,
    w: &mut Conn<'_>,
    site: &Site,
    watch: &idle::Guard,
    head: &mut HeadBuf,
    answered: bool,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut first = !answered;
    let mut scratch = Scratch::default();
    loop {
        // The wait for a head is limited (idle.rs); serving a request is not.
        watch.waiting(if first { site.head_timeout } else { site.idle_timeout });
        first = false;
        let read = read_head(r, head).await;
        watch.busy();
        let len = match read {
            Ok(Some(n)) => n,
            Ok(None) => return,
            Err(code) => {
                let _ = respond_error(w, code, false, false).await;
                return;
            }
        };
        let req = match parse_head(&head.buf[head.pos..head.pos + len]) {
            Ok(req) => req,
            Err(code) => {
                let _ = respond_error(w, code, false, false).await;
                return;
            }
        };
        let busy = site.busy();
        let t0 = site.cfg.access_log.then(Instant::now);
        let keep = req.keep_alive && !site.draining.load(Ordering::SeqCst);
        scratch.cache = "";
        let result = handle(&req, site, w, keep, &mut scratch).await;
        drop(busy);
        if let Some(t0) = t0 {
            log_access(&req, &result, t0, scratch.cache);
        }
        // Closing: the caller's drop of the connection is close(2), which
        // sends the FIN after the response bytes (no shutdown(2) first).
        if result.is_err() || !keep || site.draining.load(Ordering::SeqCst) {
            return;
        }
        head.consume(len);
    }
}

/// The first request of a new connection, answered without leaving the accept
/// loop when that costs nothing: the request is already in the socket (Linux
/// accepts a connection only once its data has arrived, TCP_DEFER_ACCEPT), so
/// the normal handler runs on the spot, once, with the raw socket as its
/// output (`Conn::Raw`). A cached hit, a file opened and sent, a 404: the
/// whole exchange is `recv`, the file's open, stat and read or sendfile, the
/// send and, unless the client wants the connection kept, `close`. No task,
/// no epoll registration (and so no deregistration), no timer, nothing waited
/// for. A client that sends `Connection: close` (or HTTP/1.0: `ab`, `curl`,
/// health checks, a browser's first request on each connection) costs about
/// half what it did.
///
/// What cannot be answered without waiting is handed on as it stands, as
/// `Go`: a head still arriving, a lookup or a read the disk has to serve (the
/// handler would wait for a thread), a response the socket has no room for
/// (the rest goes with it, `pending`).
pub(super) fn first_request(site: &Site, fd: OwnedFd, try_inline: bool) -> First {
    let mut head = HeadBuf::take(site);
    let hand_on = |fd, head| First::Go { fd, resume: Resume::new(head) };
    if !try_inline {
        return hand_on(fd, head);
    }
    match crate::sys::recv_into(fd.as_fd(), &mut head.buf) {
        Ok(0) => {
            // The client closed before it sent anything.
            head.give_back(site);
            return First::Done;
        }
        Ok(_) => {}
        // Nothing there yet (Linux hands over a connection with its data, but
        // not always: the defer timeout, a peer that connected and stalled).
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return hand_on(fd, head),
        Err(_) => {
            head.give_back(site);
            return First::Done;
        }
    }
    let (skip, len) = match find_head(&head.buf) {
        Head::Done { skip, len } => (skip, len),
        Head::More { skip, scan } => {
            // Part of a head: the task carries on from here.
            head.pos = skip;
            head.scan = scan;
            return hand_on(fd, head);
        }
        Head::TooLong => return hand_on(fd, head),
    };
    head.pos = skip;
    let Ok(req) = parse_head(&head.buf[skip..skip + len]) else {
        // The task answers it (400).
        return hand_on(fd, head);
    };
    let t0 = site.cfg.access_log.then(Instant::now);
    let keep = req.keep_alive && !site.draining.load(Ordering::SeqCst);
    let (result, rest, cache) = match answer_at_once(site, &req, fd.as_fd(), keep) {
        Quick::Answered { result, rest, cache } => (result, rest, cache),
        Quick::Wait => return hand_on(fd, head),
        Quick::Lost => {
            head.give_back(site);
            return First::Done;
        }
    };
    if let Some(t0) = t0 {
        log_access(&req, &result, t0, cache);
    }
    if result.is_err() {
        // The client is gone, or a file could not be read: no answer to give.
        head.give_back(site);
        return First::Done;
    }
    site.inline.fetch_add(1, Ordering::Relaxed);
    head.consume(len);
    match rest {
        Some(rest) => First::Go { fd, resume: Resume { head, pending: Some(Pending { rest, keep }), answered: true } },
        None if keep => First::Go { fd, resume: Resume { head, pending: None, answered: true } },
        None => {
            head.give_back(site);
            First::Done
        }
    }
}

/// What running the handler once on the spot came to.
enum Quick {
    /// It answered (`result`; an error: the client is gone or a file could not
    /// be read). `rest` is what the socket did not take of the response;
    /// `cache` the access log's note on the cache.
    Answered { result: std::io::Result<(u16, u64)>, rest: Option<Rest>, cache: &'static str },
    /// It had to wait (for a thread, for the disk) and nothing has gone out,
    /// a response being one write made last: the task starts over from the
    /// same head.
    Wait,
    /// Nothing to be done for this connection: the handler panicked, or
    /// waited after part of a response went out.
    Lost,
}

/// Poll the handler once, with a waker that does nothing: what it can do
/// without waiting it does, and the first thing it would wait for ends the
/// poll.
fn answer_at_once(site: &Site, req: &Request<'_>, fd: BorrowedFd<'_>, keep: bool) -> Quick {
    let mut sink = Conn::raw(fd);
    let mut scratch = Scratch::default();
    let polled = {
        let mut fut = std::pin::pin!(handle(req, site, &mut sink, keep, &mut scratch));
        let cx = &mut std::task::Context::from_waker(std::task::Waker::noop());
        // A task that panics costs its connection and nothing else (tokio
        // contains it); here the accept loop is the task, so do the same.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fut.as_mut().poll(cx)))
    };
    let polled = match polled {
        Ok(polled) => polled,
        Err(payload) => {
            eprintln!(
                "warden serve-static: a request handler panicked ({}): {} {}; the connection was closed",
                crate::guard::panic_message(&*payload),
                req.method,
                req.path
            );
            return Quick::Lost;
        }
    };
    let (rest, wrote) = sink.unsent();
    match polled {
        Poll::Ready(result) => Quick::Answered { result, rest, cache: scratch.cache },
        Poll::Pending if !wrote => Quick::Wait,
        Poll::Pending => Quick::Lost,
    }
}

/// The access log line of a request (`access_log = true`).
fn log_access(req: &Request<'_>, result: &std::io::Result<(u16, u64)>, t0: Instant, cached: &str) {
    let (status, bytes) = result.as_ref().map(|x| *x).unwrap_or((0, 0));
    println!("{} {} {status} {bytes}B {:.1}ms{cached}", req.method, req.path, t0.elapsed().as_secs_f64() * 1000.0);
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::super::cache::{Cache, Entry};
    use super::super::cached::cached_bytes;
    use super::super::conn::INLINE_SEND_MAX;
    use super::super::head::parse_head;
    use super::super::open::{OPEN_CACHED, OPEN_LEGACY};
    use super::super::{Fixed, HEAD_TIMEOUT, IDLE_TIMEOUT};
    use super::*;
    use crate::config::Static;
    use std::os::fd::AsRawFd;
    use std::path::Path;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize};
    use std::time::Duration;

    // ---- first_request: answering from the accept loop

    fn test_site(access_log: bool) -> Site {
        test_site_at(Path::new("/"), access_log)
    }

    fn test_site_at(root: &Path, access_log: bool) -> Site {
        let cfg: Static = serde_json::from_value(serde_json::json!({"root": root, "access_log": access_log})).unwrap();
        Site {
            root: root.to_path_buf(),
            dir: Arc::new(OwnedFd::from(std::fs::File::open(root).unwrap())),
            open_mode: AtomicU8::new(OPEN_CACHED),
            cached_hits: AtomicU64::new(0),
            cached_misses: AtomicU64::new(0),
            cache: Cache::new(8 << 20, 1 << 16, 1000, 1024),
            fixed: Fixed::new(&cfg),
            skip_siblings: crate::static_server::names::ExtSet::new(&cfg.precompressed_skip),
            cfg,
            auth: None,
            draining: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: idle::Idle::new(),
            head_timeout: HEAD_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            bufs: Mutex::new(Vec::new()),
            inline: AtomicU64::new(0),
        }
    }

    /// A cache entry for `/t.js`: a response of `body` bytes, as the server builds them.
    fn put_entry(site: &Site, body: usize) -> Arc<Entry> {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: {body}\r\nETag: W/\"1-2\"\r\n\
             Last-Modified: Thu, 01 Jan 1970 00:00:02 GMT\r\nConnection: keep-alive\r\n\r\n"
        );
        let conn_at = head.find("keep-alive").unwrap();
        let nm = "HTTP/1.1 304 Not Modified\r\nETag: W/\"1-2\"\r\nConnection: keep-alive\r\n\r\n";
        let mut resp = head.clone().into_bytes();
        resp.extend((0..body).map(|i| b'a' + (i % 26) as u8));
        let e = Arc::new(Entry {
            resp: resp.into(),
            file: None,
            head_len: head.len(),
            conn_at,
            not_modified: nm.as_bytes().to_vec().into(),
            nm_conn_at: nm.find("keep-alive").unwrap(),
            etag: "W/\"1-2\"".into(),
            mtime: 2,
            body_len: body as u64,
            deps: vec![],
        });
        site.cache.as_ref().unwrap().insert("t.js\u{0}0".into(), e.clone(), Instant::now());
        e
    }

    /// A connected pair: the client end, and the accepted end as the accept loop gets it.
    fn accepted() -> (std::net::TcpStream, OwnedFd) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let c = std::net::TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        s.set_nonblocking(true).unwrap();
        (c, OwnedFd::from(s))
    }

    fn wait_readable(c: &std::net::TcpStream) {
        // The request has crossed loopback by the time a peek sees it.
        c.set_nonblocking(false).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    }

    #[test]
    fn a_cached_first_request_is_answered_without_leaving_the_accept_loop() {
        use std::io::{Read, Write};
        let site = test_site(false);
        let e = put_entry(&site, 1000);

        // Connection: close. The whole exchange, and the connection is closed.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert_eq!(got, want.as_slice());
        assert!(String::from_utf8_lossy(&got).contains("Connection: close\r\n"));
        assert_eq!(site.cache.as_ref().unwrap().hits.load(Ordering::Relaxed), 1);
        assert_eq!(site.inline.load(Ordering::Relaxed), 1, "counted");

        // Keep-alive: answered, and the connection goes on as a task. What
        // came after the request is kept for it.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nHost: x\r\n\r\nGET /next HTTP/1.1\r\n").unwrap();
        wait_readable(&c);
        let First::Go { fd: _fd, resume: Resume { head, pending, answered } } = first_request(&site, fd, true) else {
            panic!("kept alive")
        };
        assert!(pending.is_none());
        assert!(answered, "one request is done: the next wait is the one between requests");
        assert_eq!(&head.buf[head.pos..], b"GET /next HTTP/1.1\r\n");
        let mut got = vec![0u8; e.resp.len()];
        c.read_exact(&mut got).unwrap();
        assert_eq!(&got[..], &e.resp[..], "the keep-alive response, byte for byte");

        // HEAD and a 304 come from the same entry.
        let (mut c, fd) = accepted();
        c.write_all(b"HEAD /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"HTTP/1.1 200 OK\r\n") && got.ends_with(b"\r\n\r\n"), "head only");
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nIf-None-Match: W/\"1-2\"\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        assert!(matches!(first_request(&site, fd, true), First::Done));
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(got.starts_with(b"HTTP/1.1 304 Not Modified\r\n"));
    }

    /// What the accept loop does not answer is handed on whole, nothing sent.
    #[test]
    fn what_cannot_be_answered_at_once_is_handed_on_whole() {
        use std::io::{Read, Write};
        let site = test_site(false);
        put_entry(&site, 1000);
        let leaves = |raw: &[u8]| {
            let (mut c, fd) = accepted();
            c.write_all(raw).unwrap();
            wait_readable(&c);
            let First::Go { fd: _fd, resume: Resume { head, pending, answered } } = first_request(&site, fd, true)
            else {
                panic!("{raw:?}")
            };
            assert!(pending.is_none() && !answered, "{raw:?}");
            assert_eq!(&head.buf[head.pos..], raw, "every byte is still there");
            c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
            assert!(c.read(&mut [0u8; 1]).is_err(), "nothing was sent: {raw:?}");
        };
        leaves(b"GET /t.js HTTP/1.1\r\nHost: x"); // the head is still arriving
        leaves(b"GET /t.js HTTP/1.1\r\nHost: x\r\n"); // so is this one
        leaves(b"GET\r\n\r\n"); // a request line the task answers with 400

        // Nothing sent yet: carry on.
        let (_c, fd) = accepted();
        let First::Go { resume: Resume { head, .. }, .. } = first_request(&site, fd, true) else {
            panic!("no data yet")
        };
        assert!(head.buf.is_empty());
        // Closed without a word: finished.
        let (c, fd) = accepted();
        drop(c);
        std::thread::sleep(Duration::from_millis(30));
        assert!(matches!(first_request(&site, fd, true), First::Done));
        // Switched off: the request is not even read.
        let (mut c, fd) = accepted();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let First::Go { resume: Resume { head, .. }, .. } = first_request(&site, fd, false) else { panic!("off") };
        assert!(head.buf.is_empty());
    }

    /// Whatever the request, the accept loop's answer is the normal one: files
    /// that are not cached, misses, ranges, 404s, other methods, a password.
    #[test]
    fn requests_that_are_not_cache_hits_are_answered_at_once_too() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("warden-first-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("f.bin"), &body).unwrap();
        std::fs::write(dir.join("small.txt"), b"hello").unwrap();
        let mut site = test_site_at(&dir, false);
        site.cache = None; // nothing is cached: every file is opened and sent
        // A lookup the dentry cache cannot answer waits for a thread, and the
        // request goes on as a task (nothing sent); the lookup warms the
        // cache, so a later try is answered at once (a 404 looks up two names).
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let _in_runtime = rt.enter();
        let exchange = |site: &Site, raw: &str| -> Vec<u8> {
            for attempt in 0..3 {
                let (mut c, fd) = accepted();
                c.write_all(raw.as_bytes()).unwrap();
                wait_readable(&c);
                match first_request(site, fd, true) {
                    First::Done => {
                        let mut got = Vec::new();
                        c.read_to_end(&mut got).unwrap();
                        return got;
                    }
                    First::Go { resume: Resume { pending: None, answered: false, .. }, .. } if attempt < 2 => {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    First::Go { .. } => panic!("not answered at once: {raw:?}"),
                }
            }
            unreachable!()
        };
        let split = |got: &[u8]| -> (String, Vec<u8>) {
            let at = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
            (String::from_utf8_lossy(&got[..at]).into_owned(), got[at..].to_vec())
        };

        let (h, b) = split(&exchange(&site, "GET /f.bin HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n") && h.contains("Content-Length: 30000\r\n"), "{h}");
        assert!(h.contains("Connection: close\r\n") && b == body);
        let (h, b) = split(&exchange(&site, "GET /small.txt HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n") && b == b"hello", "{h}");
        let (h, b) = split(&exchange(&site, "HEAD /f.bin HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.contains("Content-Length: 30000\r\n") && b.is_empty());
        let (h, b) = split(&exchange(&site, "GET /f.bin HTTP/1.1\r\nRange: bytes=10-19\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 206 ") && b == body[10..20], "{h}");
        let (h, _) = split(&exchange(&site, "GET /nope HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 404 "), "{h}");
        let (h, _) = split(&exchange(&site, "GET /../etc/passwd HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 403 "), "{h}");
        let (h, _) = split(&exchange(&site, "POST /f.bin HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 405 "), "{h}");
        let (h, b) = split(&exchange(&site, "GET /f.bin HTTP/1.1\r\nIf-None-Match: *\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 304 ") && b.is_empty(), "{h}");
        site.auth = Some("Basic eDp5".into());
        let (h, _) = split(&exchange(&site, "GET /f.bin HTTP/1.1\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 401 "), "{h}");
        let (h, b) =
            split(&exchange(&site, "GET /f.bin HTTP/1.1\r\nAuthorization: Basic eDp5\r\nConnection: close\r\n\r\n"));
        assert!(h.starts_with("HTTP/1.1 200 ") && b == body, "{h}");
        assert!(site.inline.load(Ordering::Relaxed) >= 10, "answered by the accept loop");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file bigger than the accept loop sends in one go goes on as a task
    /// after its first part, and the client gets the file once. Two sizes: 3 MB
    /// is handed on whole (the task corks big bodies, `CORK_MIN`); 600 KB is
    /// sent by the accept loop as far as it goes (at most `INLINE_SEND_MAX`,
    /// less when the socket is full), and the task carries on from there.
    #[tokio::test(flavor = "current_thread")]
    async fn a_big_file_goes_on_as_a_task_after_its_first_part() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("warden-first-big-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut site = test_site_at(&dir, false);
        site.cache = None;
        let site = Arc::new(site);
        for (len, corked) in [(3_000_000usize, true), (600_000, false)] {
            let body: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            std::fs::write(dir.join("f.bin"), &body).unwrap();
            for small_send_buffer in [false, true] {
                let (mut c, fd) = accepted();
                if small_send_buffer {
                    crate::sys::set_send_buffer(fd.as_fd(), 4096).unwrap();
                }
                c.write_all(b"GET /f.bin HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
                wait_readable(&c);
                let First::Go { fd, resume: Resume { head, pending, answered } } = first_request(&site, fd, true)
                else {
                    panic!("{len} bytes are not all sent at once")
                };
                let Some(Pending { keep: false, rest: Rest::File { off, count, .. } }) = &pending else {
                    panic!("the rest of the file goes on")
                };
                assert!(answered);
                assert_eq!(off + count, len as u64, "what was sent and what is left make the file");
                assert!(*off <= INLINE_SEND_MAX, "the accept loop sends at most {INLINE_SEND_MAX} bytes: {off}");
                if corked {
                    assert_eq!((*off, *count), (0, len as u64), "a body the task corks is handed on whole");
                }
                let reader = std::thread::spawn(move || {
                    let mut got = Vec::new();
                    c.read_to_end(&mut got).unwrap();
                    got
                });
                let stream = tokio::net::TcpStream::from_std(std::net::TcpStream::from(fd)).unwrap();
                let guard = site.idle.register(stream.as_raw_fd());
                let s2 = site.clone();
                tokio::spawn(async move {
                    let mut stream = stream;
                    let guard = guard;
                    let (r, w) = stream.split();
                    connection(r, Conn::Tcp(w), s2, &guard, Resume { head, pending, answered }).await;
                })
                .await
                .unwrap();
                let got = reader.join().unwrap();
                let at = got.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                assert!(
                    got[at..] == body[..],
                    "{len} bytes, send buffer shrunk: {small_send_buffer}: {} body bytes",
                    got.len() - at
                );
                assert!(String::from_utf8_lossy(&got[..at]).contains(&format!("Content-Length: {len}\r\n")));
            }
        }
        assert_eq!(site.active.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request the handler cannot finish without waiting (here: its lookup
    /// needs a thread, and the only one is busy) is handed on whole, nothing
    /// sent, for the task to start over.
    #[test]
    fn a_request_that_has_to_wait_is_handed_on_whole() {
        use std::io::{Read, Write};
        let dir = std::env::temp_dir().join(format!("warden-first-wait-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        let site = test_site_at(&dir, false);
        // Opens without openat2 are a call on a thread.
        site.open_mode.store(OPEN_LEGACY, Ordering::Relaxed);
        let rt = tokio::runtime::Builder::new_current_thread().max_blocking_threads(1).enable_all().build().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        rt.spawn_blocking(move || {
            started_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        started_rx.recv().unwrap();
        let _in_runtime = rt.enter();

        let request = b"GET /a.txt HTTP/1.1\r\nConnection: close\r\n\r\n";
        let (mut c, fd) = accepted();
        c.write_all(request).unwrap();
        wait_readable(&c);
        let First::Go { fd: _fd, resume: Resume { head, pending, answered } } = first_request(&site, fd, true) else {
            panic!("the lookup cannot finish at once")
        };
        assert!(pending.is_none() && !answered);
        assert_eq!(&head.buf[head.pos..], request, "every byte is still there");
        c.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
        assert!(c.read(&mut [0u8; 1]).is_err(), "nothing was sent");
        assert_eq!(site.inline.load(Ordering::Relaxed), 0);
        release_tx.send(()).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A response that does not fit the socket's room: what was sent is
    /// counted, and the rest is handed on to be written by the task.
    #[test]
    fn a_response_too_big_for_the_socket_goes_on_as_a_task() {
        use std::io::{Read, Write};
        let site = test_site(false);
        let e = put_entry(&site, 2_000_000);
        let (mut c, fd) = accepted();
        crate::sys::set_send_buffer(fd.as_fd(), 4096).unwrap();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let got = first_request(&site, fd, true);
        let First::Go { fd: _fd, resume: Resume { pending: Some(p), answered, .. } } = got else {
            panic!(
                "partial: {}",
                match got {
                    First::Done => "Done".to_string(),
                    First::Go { resume: Resume { pending, head, .. }, .. } =>
                        format!("Go pending={} head={}", pending.is_some(), head.buf.len()),
                }
            )
        };
        assert!(!p.keep && answered);
        let Rest::Bytes { out, at } = p.rest else { panic!("bytes in memory") };
        let total = out.as_slice().len();
        assert!(at < total, "the socket took {at} of {total}");
        // What was sent plus what is pending is the whole response, in order.
        let mut sent = vec![0u8; at];
        c.read_exact(&mut sent).unwrap();
        let rest = out.advance(at);
        sent.extend_from_slice(rest.as_slice());
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert_eq!(sent, want.as_slice());
    }

    /// What `first_request` could not send is finished by the connection's
    /// task, which counts it as a request in flight (a drain waits for it).
    #[tokio::test(flavor = "current_thread")]
    async fn the_task_finishes_a_partial_response_and_counts_it_as_active() {
        use std::io::{Read, Write};
        let site = Arc::new(test_site(false));
        let e = put_entry(&site, 2_000_000);
        let (mut c, fd) = accepted();
        crate::sys::set_send_buffer(fd.as_fd(), 4096).unwrap();
        c.write_all(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap();
        wait_readable(&c);
        let First::Go { fd, resume: Resume { head, pending, answered } } = first_request(&site, fd, true) else {
            panic!("partial")
        };
        assert!(pending.is_some() && answered);

        let stream = tokio::net::TcpStream::from_std(std::net::TcpStream::from(fd)).unwrap();
        let guard = site.idle.register(stream.as_raw_fd());
        let s2 = site.clone();
        let task = tokio::spawn(async move {
            let mut stream = stream;
            let guard = guard;
            let (r, w) = stream.split();
            connection(r, Conn::Tcp(w), s2, &guard, Resume { head, pending, answered }).await;
        });
        // The client has read nothing, so the socket is full: the write is
        // in progress, and counted.
        let t0 = Instant::now();
        while site.active.load(Ordering::SeqCst) == 0 {
            assert!(t0.elapsed() < Duration::from_secs(5), "the task never began the write");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(site.active.load(Ordering::SeqCst), 1, "still writing");
        let reader = std::thread::spawn(move || {
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            got
        });
        task.await.unwrap();
        assert_eq!(site.active.load(Ordering::SeqCst), 0, "done");
        let got = reader.join().unwrap();
        let want =
            cached_bytes(&parse_head(b"GET /t.js HTTP/1.1\r\nConnection: close\r\n\r\n").unwrap(), &e, false, false).0;
        assert!(
            got == want.as_slice(),
            "the whole response, once, then the close ({} of {} bytes)",
            got.len(),
            want.as_slice().len()
        );
    }
}

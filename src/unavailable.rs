//! The 503 page (`[unavailable]`, `docs/unavailable.md`): a listener in the
//! app's SO_REUSEPORT group that exists only while no worker serves. It runs
//! on a thread of its own, so a flood during an outage never touches the
//! supervisor's event loop, and it is dropped (the thread ends, the socket
//! closes) as soon as a worker serves again.

#![deny(unsafe_code)]

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Semaphore, oneshot};

/// Largest page file read.
pub const MAX_PAGE: u64 = 64 * 1024;
/// Connections answered at once; more are closed unanswered.
const MAX_OPEN: usize = 64;
const IO_TIMEOUT: Duration = Duration::from_millis(500);

const BUILTIN: &str = "<!doctype html><meta charset=utf-8><meta name=viewport content=\"width=device-width\">\
<title>Temporarily unavailable</title><body style=\"font:16px system-ui;margin:20vh auto;max-width:30em;\
padding:0 1em\"><h1>Temporarily unavailable</h1><p>This service is not running right now. Please try again in a moment.</p>";

/// The page on the port. Dropping it stops answering and closes the socket.
pub struct Responder {
    stop: Option<oneshot::Sender<()>>,
}

impl Responder {
    /// Joins the SO_REUSEPORT group of `addr`. `page` is the body (None: the built-in page).
    pub fn start(addr: SocketAddr, page: Option<Vec<u8>>, retry_after: u64) -> io::Result<Responder> {
        let listener = crate::sys::listen_tcp(addr, true, 128)?;
        let body = page.unwrap_or_else(|| BUILTIN.as_bytes().to_vec());
        let head = format!(
            "HTTP/1.1 503 Service Unavailable\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
             Retry-After: {retry_after}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let reply = Arc::new((head.into_bytes(), body));
        let (stop, stopped) = oneshot::channel();
        std::thread::Builder::new().name("unavailable".into()).spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
            rt.block_on(serve(listener, reply, stopped));
        })?;
        Ok(Responder { stop: Some(stop) })
    }
}

impl Drop for Responder {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(()); // the thread may have ended already
        }
        // The thread closes the socket and ends within about a second; the
        // supervisor's loop does not wait for it.
    }
}

async fn serve(listener: std::net::TcpListener, reply: Arc<(Vec<u8>, Vec<u8>)>, mut stopped: oneshot::Receiver<()>) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else { return };
    let open = Arc::new(Semaphore::new(MAX_OPEN));
    loop {
        tokio::select! {
            _ = &mut stopped => break,
            r = listener.accept() => match r {
                Ok((s, _)) => {
                    if let Ok(permit) = open.clone().try_acquire_owned() {
                        let reply = reply.clone();
                        tokio::spawn(async move {
                            answer(s, &reply).await;
                            drop(permit);
                        });
                    }
                }
                // Out of descriptors or memory: wait rather than spin.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            },
        }
    }
    // Connections that arrived while down and wait in the backlog get their
    // answer too, rather than a reset.
    while let Ok(Ok((s, _))) = tokio::time::timeout(Duration::from_millis(1), listener.accept()).await {
        if let Ok(permit) = open.clone().try_acquire_owned() {
            let reply = reply.clone();
            tokio::spawn(async move {
                answer(s, &reply).await;
                drop(permit);
            });
        }
    }
    drop(listener);
    // Let the answers in flight finish (each is bounded by IO_TIMEOUT).
    let _ = tokio::time::timeout(Duration::from_secs(1), open.acquire_many(MAX_OPEN as u32)).await;
}

/// Reads the request's first bytes (to know HEAD, and so that closing does
/// not reset the connection under the answer), writes the page, closes.
async fn answer(mut s: tokio::net::TcpStream, reply: &(Vec<u8>, Vec<u8>)) {
    let mut buf = [0u8; 1024];
    let n = match tokio::time::timeout(IO_TIMEOUT, s.read(&mut buf)).await {
        Ok(Ok(n)) => n,
        _ => 0,
    };
    // A TLS hello: this port speaks TLS, and a plain-text answer would only
    // be noise. Closing is what a refused connection looked like.
    if n == 0 || buf[0] == 0x16 {
        return;
    }
    let head_only = buf[..n].starts_with(b"HEAD ");
    let write = async {
        s.write_all(&reply.0).await?;
        if !head_only {
            s.write_all(&reply.1).await?;
        }
        s.shutdown().await
    };
    let _ = tokio::time::timeout(IO_TIMEOUT, write).await; // a slow client loses its answer, nothing else
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn ask(addr: SocketAddr, req: &[u8]) -> Vec<u8> {
        let mut c = std::net::TcpStream::connect(addr).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        c.write_all(req).unwrap();
        let mut out = Vec::new();
        let _ = c.read_to_end(&mut out);
        out
    }

    fn free_port() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
    }

    #[test]
    fn answers_503_with_the_page_and_stops_on_drop() {
        let addr = free_port();
        let r = Responder::start(addr, Some(b"<p>back soon</p>".to_vec()), 7).unwrap();
        let get = String::from_utf8(ask(addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n")).unwrap();
        assert!(get.starts_with("HTTP/1.1 503 Service Unavailable\r\n"), "{get}");
        assert!(get.contains("Retry-After: 7\r\n") && get.contains("Content-Length: 16\r\n"), "{get}");
        assert!(get.ends_with("<p>back soon</p>"), "{get}");
        let head = String::from_utf8(ask(addr, b"HEAD / HTTP/1.1\r\n\r\n")).unwrap();
        assert!(head.starts_with("HTTP/1.1 503") && head.ends_with("\r\n\r\n"), "{head}");
        drop(r);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::net::TcpStream::connect(addr).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the port is closed again");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_tls_hello_gets_no_plain_text_answer() {
        let addr = free_port();
        let _r = Responder::start(addr, None, 5).unwrap();
        assert!(ask(addr, &[0x16, 3, 1, 0, 5, 1, 0, 0, 1, 0]).is_empty());
    }

    #[test]
    fn shares_the_port_with_a_reuseport_server() {
        let addr = free_port();
        let _server = crate::sys::listen_tcp(addr, true, 16).unwrap();
        assert!(Responder::start(addr, None, 5).is_ok(), "joins the group of a server that is there");
    }
}

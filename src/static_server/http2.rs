//! HTTP/2 for `[static]` sites: the adaptor for connections that negotiate
//! `h2` in ALPN (`tls.rs`). A different transport from the plain-HTTP/1.1
//! path, the same answers: each stream runs the normal H1 request handler
//! into an in-memory sink, and the H1 bytes it produces are translated to
//! H2 HEADERS + DATA. Auth, paths, conditionals, ranges, compression, SPA
//! fallback and the response cache all come along unchanged instead of
//! mirrored — the cache holds plaintext bodies shared across connections
//! (ciphertext cannot be shared: every connection has its own keys), and
//! only the framing differs per stream.
//!
//! Two costs this pays against the plain path, both inherent to encryption
//! rather than to this layer: bodies go through userspace (no sendfile,
//! which cannot feed a TLS connection), and a cached body is copied once per
//! stream on its way into DATA frames.

use super::conn::Conn;
use super::connection::log_access;
use super::handler::{Scratch, handle};
use super::head::parse_head;
use super::tls::TlsIo;
use super::Site;
use bytes::Bytes;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

/// One connection negotiated to HTTP/2: streams until the client goes away.
/// New streams arriving while the worker drains are refused (the client
/// retries on a new connection, which reaches the new workers); streams in
/// flight finish and the drain waits for them through the request count.
pub(super) async fn serve<IO>(tls: TlsIo<IO>, site: Arc<Site>)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_io(tls, site).await
}

/// The same driver over any transport: a TLS stream, or a kernel-owned
/// socket past the kTLS handover (plaintext to us, encrypted on the wire).
pub(super) async fn serve_io<IO>(io: IO, site: Arc<Site>)
where
    IO: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let mut conn = match h2::server::handshake(io).await {
        Ok(c) => c,
        Err(_) => return,
    };
    while let Some(request) = conn.accept().await {
        let (req, respond) = match request {
            Ok(r) => r,
            // A broken stream, not a broken connection: the next accept says
            // when the connection itself is over.
            Err(_) => continue,
        };
        let site = site.clone();
        tokio::spawn(async move { stream(site, req, respond).await });
    }
}

/// One stream: the H2 request through the H1 handler, translated to frames.
async fn stream(site: Arc<Site>, req: http::Request<h2::RecvStream>, mut respond: h2::server::SendResponse<Bytes>) {
    let _busy = site.busy();
    if site.draining.load(Ordering::SeqCst) {
        respond.send_reset(h2::Reason::REFUSED_STREAM);
        return;
    }
    // Request bodies are never read (static files): GET and HEAD have none,
    // and anything else is a 405 from the handler below.
    let head = match synthesize(&req) {
        Ok(h) => h,
        Err(code) => {
            send_empty(&mut respond, code).await;
            site.responded(code);
            return;
        }
    };
    let h1 = match parse_head(&head) {
        Ok(r) => r,
        Err(code) => {
            send_empty(&mut respond, code).await;
            site.responded(code);
            return;
        }
    };
    let t0 = site.cfg.access_log.then(Instant::now);
    let mut sink = Conn::Mem(Vec::new());
    let mut scratch = Scratch::default();
    let result = handle(&h1, &site, &mut sink, true, &mut scratch).await;
    if let Some(t0) = t0 {
        log_access(&h1, &result, t0, scratch.cache);
    }
    let (status, _) = match result {
        Ok(x) => x,
        // The client is gone, or a file could not be read: no answer to give.
        Err(_) => return,
    };
    let Conn::Mem(raw) = sink else { unreachable!("the sink above is memory") };
    let (response, body) = match translate(status, &raw) {
        Ok(x) => x,
        Err(code) => {
            send_empty(&mut respond, code).await;
            site.responded(code);
            return;
        }
    };
    if send(&mut respond, response, body).await {
        site.responded(status);
    }
}

/// The H2 request as H1 bytes for the shared parser: the method, the path,
/// `:authority` as host, and the headers (`cookie` joined, as H2 sends it
/// split). H1 and H2 agree on what is garbage: non-UTF-8 values fail here
/// as they would on the plain path. Over the head limit is 431, as there.
fn synthesize(req: &http::Request<h2::RecvStream>) -> Result<Vec<u8>, u16> {
    /// Request heads are capped at 16 KB, like the plain path's.
    const HEAD_CAP: usize = 16 * 1024;
    let path = req.uri().path_and_query().map(|pq| pq.as_str()).unwrap_or("/");
    let mut head = Vec::with_capacity(512);
    head.extend_from_slice(req.method().as_str().as_bytes());
    head.push(b' ');
    head.extend_from_slice(path.as_bytes());
    head.extend_from_slice(b" HTTP/1.1\r\n");
    if let Some(auth) = req.uri().authority() {
        head.extend_from_slice(b"host: ");
        head.extend_from_slice(auth.as_str().as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    let mut cookies: Vec<&[u8]> = Vec::new();
    for (k, v) in req.headers() {
        if k == "cookie" {
            cookies.push(v.as_bytes());
            continue;
        }
        head.extend_from_slice(k.as_str().as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(v.to_str().map_err(|_| 400u16)?.as_bytes());
        head.extend_from_slice(b"\r\n");
    }
    if !cookies.is_empty() {
        head.extend_from_slice(b"cookie: ");
        for (i, c) in cookies.iter().enumerate() {
            if i > 0 {
                head.extend_from_slice(b"; ");
            }
            head.extend_from_slice(std::str::from_utf8(c).map_err(|_| 400u16)?.as_bytes());
        }
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    if head.len() > HEAD_CAP {
        return Err(431);
    }
    Ok(head)
}

/// The H1 bytes of one answered request as an H2 response and its body.
/// Connection-specific headers (`connection`, `keep-alive`,
/// `transfer-encoding`) stay on the H1 path: H2 forbids them, and the
/// stream's own lifecycle replaces them. `content-length` is kept: it is
/// exact by construction.
fn translate(status: u16, raw: &[u8]) -> Result<(http::Response<()>, Bytes), u16> {
    let end = memchr::memmem::find(raw, b"\r\n\r\n").ok_or(500u16)? + 4;
    let mut response = http::Response::builder().status(status);
    for line in raw[..end].split(|&b| b == b'\n').skip(1) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        let at = line.iter().position(|&b| b == b':').ok_or(500u16)?;
        let (name, value) = (&line[..at], &line[at + 1..]);
        let lower = name.to_ascii_lowercase();
        if matches!(lower.as_slice(), b"connection" | b"keep-alive" | b"transfer-encoding" | b"upgrade") {
            continue;
        }
        let value = value.strip_prefix(b" ").unwrap_or(value);
        let name = http::header::HeaderName::from_bytes(&lower).map_err(|_| 500u16)?;
        let value = http::header::HeaderValue::from_bytes(value).map_err(|_| 500u16)?;
        response = response.header(name, value);
    }
    // A cached body is copied once here; it stays shared in the cache.
    Ok((response.body(()).map_err(|_| 500u16)?, Bytes::copy_from_slice(&raw[end..])))
}

/// HEADERS, then the body in frames within the peer's flow-control window.
/// False when the client went away first (nothing to count then).
async fn send(
    respond: &mut h2::server::SendResponse<Bytes>,
    response: http::Response<()>,
    body: Bytes,
) -> bool {
    /// One DATA frame at most: larger bodies go out in turns.
    const FRAME: usize = 16 * 1024;
    let mut tx = match respond.send_response(response, body.is_empty()) {
        Ok(tx) => tx,
        Err(_) => return false,
    };
    let mut rest = body;
    while !rest.is_empty() {
        let n = rest.len().min(FRAME);
        tx.reserve_capacity(n);
        let got = std::future::poll_fn(|cx| tx.poll_capacity(cx)).await;
        let avail = match got {
            Some(Ok(a)) => a,
            _ => return false,
        };
        if avail == 0 {
            return false;
        }
        let take = avail.min(n);
        let last = take == rest.len();
        if tx.send_data(rest.split_to(take), last).is_err() {
            return false;
        }
    }
    true
}

/// A status with no body (a bad request, or translation failing where the
/// plain path's bytes cannot become frames).
async fn send_empty(respond: &mut h2::server::SendResponse<Bytes>, status: u16) {
    let response = http::Response::builder().status(status).body(()).unwrap_or_else(|_| http::Response::new(()));
    let _ = respond.send_response(response, true);
}

#[cfg(test)]
mod tests {
    use super::super::cache::Cache;
    use super::super::names::ExtSet;
    use super::super::tls;
    use super::super::{Fixed, HEAD_TIMEOUT, IDLE_TIMEOUT};
    use super::*;
    use crate::config::Static;
    use std::os::fd::OwnedFd;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize};

    fn site_at(root: &std::path::Path) -> Arc<Site> {
        let cfg: Static = serde_json::from_value(serde_json::json!({"root": root})).unwrap();
        Arc::new(Site {
            root: root.to_path_buf(),
            dir: Arc::new(OwnedFd::from(std::fs::File::open(root).unwrap())),
            // Like production: openat2 on Linux, the legacy open elsewhere.
            open_mode: AtomicU8::new(if cfg!(target_os = "linux") {
                super::super::open::OPEN_CACHED
            } else {
                super::super::open::OPEN_LEGACY
            }),
            cached_hits: AtomicU64::new(0),
            cached_misses: AtomicU64::new(0),
            cache: Cache::new(8 << 20, 1 << 16, 1000, 1024),
            fixed: Fixed::new(&cfg),
            skip_siblings: ExtSet::new(&cfg.precompressed_skip),
            compressor: None,
            cfg,
            auth: None,
            draining: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            idle: super::super::idle::Idle::new(),
            head_timeout: HEAD_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            bufs: Mutex::new(Vec::new()),
            inline: AtomicU64::new(0),
            responses: Some(Default::default()),
        })
    }

    /// One request over an H2 connection: its status, headers and body bytes.
    async fn roundtrip(
        send: &mut h2::client::SendRequest<Bytes>,
        method: &str,
        uri: &str,
        headers: &[(&str, &str)],
    ) -> (http::StatusCode, http::HeaderMap, Vec<u8>) {
        let mut req = http::Request::builder().method(method).uri(uri);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let (response, _) = send.send_request(req.body(()).unwrap(), true).expect("a stream");
        let response = response.await.expect("a response");
        let (status, headers) = (response.status(), response.headers().clone());
        let mut body = response.into_body();
        let mut got = Vec::new();
        while let Some(chunk) = body.data().await {
            got.extend_from_slice(&chunk.expect("a DATA frame"));
        }
        (status, headers, got)
    }

    /// HTTP/2 end to end over TLS: files, a 404, a 304 and a HEAD come back
    /// with the same status and bytes the plain path answers.
    #[tokio::test]
    async fn h2_serves_what_the_plain_path_serves() {
        let dir = tls::test_dir("h2");
        let body: Vec<u8> = (0..30_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(dir.join("f.bin"), &body).unwrap();
        let site = site_at(&dir);
        let (cert, key) = tls::test_cert_files(&dir);
        let cert_pem = std::fs::read(&cert).unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(tls::load(&cert, &key).expect("the test chain loads"));
        let (client_io, server_io) = tokio::io::duplex(1 << 20);
        let worker = site.clone();
        let acceptor = acceptor.clone();
        let server = tokio::spawn(async move {
            let tls = tls::accept(&acceptor, server_io).await.expect("the handshake runs");
            serve(tls, worker).await;
        });
        let io = tls::test_client(b"h2", &cert_pem)
            .connect("localhost".try_into().unwrap(), client_io)
            .await
            .expect("the client handshakes");
        let (mut send, conn) = h2::client::handshake(io).await.expect("the H2 preface runs");
        let drive = tokio::spawn(async move { conn.await.expect("the connection drives") });

        let (status, headers, got) = roundtrip(&mut send, "GET", "https://localhost/f.bin", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-length"], body.len().to_string());
        assert_eq!(headers["content-type"], "application/octet-stream");
        assert_eq!(got, body);
        let etag = headers["etag"].to_str().unwrap().to_string();

        // The validator the plain path made: a 304 with no body.
        let (status, _, got) =
            roundtrip(&mut send, "GET", "https://localhost/f.bin", &[("if-none-match", &etag)]).await;
        assert_eq!((status, got.len()), (http::StatusCode::NOT_MODIFIED, 0));

        // HEAD: the head, no body.
        let (status, headers, got) = roundtrip(&mut send, "HEAD", "https://localhost/f.bin", &[]).await;
        assert_eq!(status, 200);
        assert_eq!(headers["content-length"], body.len().to_string());
        assert!(got.is_empty());

        // Missing: the plain path's 404 page.
        let (status, _, _) = roundtrip(&mut send, "GET", "https://localhost/nope", &[]).await;
        assert_eq!(status, 404);

        drop(send);
        // Tear down as a closed socket would: the server task ends, and the
        // client sees the EOF. (Neither side closes first on its own: the
        // server serves until the client goes away, like a keep-alive
        // connection on the plain path.)
        server.abort();
        tokio::time::timeout(std::time::Duration::from_secs(5), drive)
            .await
            .expect("the client ends")
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Port 80: answers the CA's HTTP-01 challenge from `challenges/` and sends
//! everything else to https. Not on any app's request path: nothing listened
//! on port 80 before. Every supervisor with `[tls]` runs one on SO_REUSEPORT,
//! so any of them can answer for an order made by another.

use crate::warn;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};

const MAX_HEAD: usize = 8192;
const READ_TIMEOUT: Duration = Duration::from_secs(5);

fn bind(port: u16) -> std::io::Result<TcpListener> {
    // Dual-stack [::] first (the CA tries IPv6 when the name has an AAAA record), else IPv4 only.
    let attempt = |v6: bool| {
        let (s, addr) = if v6 {
            (TcpSocket::new_v6()?, std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)))
        } else {
            (TcpSocket::new_v4()?, std::net::SocketAddr::from((std::net::Ipv4Addr::UNSPECIFIED, port)))
        };
        s.set_reuseport(true)?;
        s.set_reuseaddr(true)?;
        s.bind(addr)?;
        s.listen(128)
    };
    attempt(true).or_else(|_| attempt(false))
}

pub async fn serve(port: u16, dir: PathBuf, redirect: bool) {
    let mut warned = false;
    loop {
        match bind(port) {
            Ok(l) => loop {
                let Ok((conn, _)) = l.accept().await else {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                };
                let dir = dir.clone();
                tokio::task::spawn_local(async move {
                    let _ = tokio::time::timeout(READ_TIMEOUT * 2, handle(conn, &dir, redirect)).await;
                });
            },
            Err(e) => {
                if !warned {
                    warn!(
                        "port for the certificate challenge is not available; certificates cannot be ordered meanwhile",
                        port = port,
                        error = e,
                        hint = "another server (nginx?) holds the port, or Warden lacks the right to bind it (root, or net.ipv4.ip_unprivileged_port_start=80)",
                    );
                    warned = true;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    }
}

async fn handle(mut conn: TcpStream, dir: &Path, redirect: bool) {
    let mut buf = Vec::with_capacity(512);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < MAX_HEAD {
        match tokio::time::timeout(READ_TIMEOUT, conn.read(&mut chunk)).await {
            Ok(Ok(n)) if n > 0 => buf.extend_from_slice(&chunk[..n]),
            _ => break,
        }
    }
    let _ = conn.write_all(&respond(&buf, dir, redirect)).await;
    let _ = conn.shutdown().await;
}

fn reply(status: &str, extra: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
        .into_bytes()
}

/// The answer to one request head (no body is ever read).
fn respond(head: &[u8], dir: &Path, redirect: bool) -> Vec<u8> {
    let text = String::from_utf8_lossy(head);
    let mut lines = text.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, path) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    if !(method == "GET" || method == "HEAD") || !path.starts_with('/') {
        return reply("400 Bad Request", "", "");
    }
    if let Some(token) = path.strip_prefix("/.well-known/acme-challenge/") {
        let ok = !token.is_empty()
            && token.len() <= 128
            && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        return match ok.then(|| std::fs::read_to_string(dir.join(token)).ok()).flatten() {
            Some(auth) => reply("200 OK", "Content-Type: text/plain\r\n", &auth),
            None => reply("404 Not Found", "", ""),
        };
    }
    if !redirect {
        return reply("404 Not Found", "", "");
    }
    let host =
        lines.find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("host")).map(|(_, v)| v.trim()));
    // Only a plain hostname goes into the Location header (no CR/LF or other tricks).
    let host =
        host.map(|h| h.rsplit_once(':').filter(|(_, p)| p.bytes().all(|b| b.is_ascii_digit())).map_or(h, |(n, _)| n));
    match host.filter(|h| {
        !h.is_empty() && h.len() <= 253 && h.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    }) {
        Some(h) => reply("308 Permanent Redirect", &format!("Location: https://{h}{path}\r\n"), ""),
        None => reply("400 Bad Request", "", ""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(b: Vec<u8>) -> String {
        String::from_utf8(b).unwrap()
    }

    #[test]
    fn serves_tokens_and_redirects_the_rest() {
        let d = std::env::temp_dir().join(format!("warden-responder-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("tok-1_A"), "tok-1_A.thumb").unwrap();
        let get = |p: &str, redirect| {
            text(respond(format!("GET {p} HTTP/1.1\r\nHost: Api.example.com:80\r\n\r\n").as_bytes(), &d, redirect))
        };
        let ok = get("/.well-known/acme-challenge/tok-1_A", true);
        assert!(ok.starts_with("HTTP/1.1 200 OK") && ok.ends_with("tok-1_A.thumb"), "{ok}");
        assert!(get("/.well-known/acme-challenge/nope", true).starts_with("HTTP/1.1 404"));
        // Not a file name: nothing outside the directory is read.
        assert!(get("/.well-known/acme-challenge/../x", true).starts_with("HTTP/1.1 404"));
        assert!(get("/a?b=1", true).contains("Location: https://Api.example.com/a?b=1\r\n"));
        assert!(get("/a", false).starts_with("HTTP/1.1 404"));
        let bad = text(respond(b"GET / HTTP/1.1\r\nX: y\r\n\r\n", &d, true));
        assert!(bad.starts_with("HTTP/1.1 400"), "no Host header: {bad}");
        assert!(text(respond(b"POST / HTTP/1.1\r\nHost: a.test\r\n\r\n", &d, true)).starts_with("HTTP/1.1 400"));
        assert!(
            text(respond(b"GET / HTTP/1.1\r\nHost: a.test\r\n\r\n", &d, true))
                .contains("Location: https://a.test/\r\n")
        );
        let _ = std::fs::remove_dir_all(&d);
    }
}

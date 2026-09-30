//! Health checks: an independent `GET` from Warden, never proxying traffic.
//!
//! - Per worker: over the worker's private Unix socket (opened by the shim), so
//!   the answer comes from exactly that worker.
//! - App level: through the shared port, where the kernel picks the worker.
//!
//! Plain HTTP/1.1; no HTTP client dependency needed.

use std::path::Path;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpStream, UnixStream};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub fn parse_url(url: &str) -> Result<Target, String> {
    let rest = url.strip_prefix("http://").ok_or("only http:// URLs are supported")?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !h.ends_with(']') || h.starts_with('[') => {
            (h, p.parse::<u16>().map_err(|_| format!("bad port {p:?}"))?)
        }
        _ => (authority, 80),
    };
    if host.is_empty() {
        return Err("missing host".into());
    }
    Ok(Target { host: host.trim_matches(['[', ']']).to_string(), port, path: path.to_string() })
}

/// App-level check through TCP. `Ok(status)` for 2xx/3xx, `Err(reason)` otherwise.
pub async fn check(t: &Target, timeout: Duration) -> Result<u16, String> {
    let host = format!("{}:{}", t.host, t.port);
    with_timeout(timeout, async {
        let s = TcpStream::connect((t.host.as_str(), t.port)).await.map_err(|e| e.to_string())?;
        request(s, &host, &t.path).await
    })
    .await
}

/// Per-worker check over the worker's private Unix socket.
pub async fn check_unix(socket: &Path, path: &str, timeout: Duration) -> Result<u16, String> {
    with_timeout(timeout, async {
        let s = UnixStream::connect(socket).await.map_err(|e| format!("{}: {e}", socket.display()))?;
        request(s, "localhost", path).await
    })
    .await
}

async fn with_timeout(
    timeout: Duration,
    f: impl std::future::Future<Output = Result<u16, String>>,
) -> Result<u16, String> {
    match tokio::time::timeout(timeout, f).await {
        Ok(r) => r,
        Err(_) => Err(format!("timed out after {}s", timeout.as_secs_f32())),
    }
}

async fn request<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, host: &str, path: &str) -> Result<u16, String> {
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: warden-health\r\nConnection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut buf = [0u8; 256];
    let mut n = 0;
    while n < 12 {
        let r = s.read(&mut buf[n..]).await.map_err(|e| e.to_string())?;
        if r == 0 {
            break;
        }
        n += r;
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let code = head
        .strip_prefix("HTTP/1.")
        .and_then(|r| r.get(2..5))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or_else(|| format!("not an HTTP response: {:?}", head.lines().next().unwrap_or("")))?;
    if (200..400).contains(&code) { Ok(code) } else { Err(format!("HTTP {code}")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        assert_eq!(
            parse_url("http://127.0.0.1:3000/health").unwrap(),
            Target { host: "127.0.0.1".into(), port: 3000, path: "/health".into() }
        );
        assert_eq!(parse_url("http://localhost").unwrap().port, 80);
        assert_eq!(parse_url("http://localhost").unwrap().path, "/");
        assert_eq!(parse_url("http://[::1]:8080/h?x=1").unwrap().host, "::1");
        assert!(parse_url("https://x/").is_err());
        assert!(parse_url("http://:80/").is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checks_status() {
        use tokio::net::TcpListener;
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            for code in ["200 OK", "503 Service Unavailable"] {
                let (mut s, _) = l.accept().await.unwrap();
                let mut b = [0u8; 512];
                let _ = s.read(&mut b).await;
                let _ = s.write_all(format!("HTTP/1.1 {code}\r\ncontent-length: 0\r\n\r\n").as_bytes()).await;
            }
        });
        let t = Target { host: "127.0.0.1".into(), port, path: "/".into() };
        assert_eq!(check(&t, Duration::from_secs(1)).await, Ok(200));
        assert_eq!(check(&t, Duration::from_secs(1)).await, Err("HTTP 503".into()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn checks_unix_socket() {
        let dir = std::env::temp_dir().join(format!("warden-h-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("h.sock");
        let _ = std::fs::remove_file(&sock);
        let l = tokio::net::UnixListener::bind(&sock).unwrap();
        tokio::spawn(async move {
            let (mut s, _) = l.accept().await.unwrap();
            let mut b = [0u8; 512];
            let n = s.read(&mut b).await.unwrap();
            assert!(String::from_utf8_lossy(&b[..n]).starts_with("GET /health HTTP/1.1"));
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\n\r\n").await;
        });
        assert_eq!(check_unix(&sock, "/health", Duration::from_secs(1)).await, Ok(204));
        assert!(check_unix(&dir.join("missing.sock"), "/", Duration::from_secs(1)).await.is_err());
    }
}

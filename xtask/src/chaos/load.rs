//! Steady client load for the chaos soak: plain std threads, no retries.
//!
//! - keep-alive: one connection reused for every request; a failed request
//!   is counted and the next one goes on a new connection (no retry);
//! - new-connection: a fresh TCP connection per request (`Connection: close`);
//! - WebSocket and SSE: a long-lived connection held until the server ends
//!   it, then reconnected at once (as EventSource and well-behaved WebSocket
//!   clients do). Every ending is recorded with how it ended.
//!
//! Every outcome carries the time, the app, and when known the worker that
//! answered (`<pid>` or `<pid>:<thread>` from the body), so the report can
//! tell a request lost on a killed worker from one lost for no reason.

use super::Shared;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A request still unanswered after this long is a failure (timeout). It is
/// above `[watchdog] timeout` + a tick, so a request stuck on a hung worker
/// ends when the watchdog kills that worker, not here.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Client {
    KeepAlive,
    NewConn,
    Ws,
    Sse,
}

impl Client {
    pub fn name(self) -> &'static str {
        match self {
            Client::KeepAlive => "keep-alive",
            Client::NewConn => "new-connection",
            Client::Ws => "websocket",
            Client::Sse => "sse",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ErrClass {
    /// connect(2) refused: nothing listens on the port.
    Refused,
    /// ECONNRESET / EPIPE.
    Reset,
    /// The connection closed before a complete response.
    Eof,
    Timeout,
    /// An HTTP status other than 200 (101 for a WebSocket).
    Status,
    Other,
}

impl ErrClass {
    pub fn name(self) -> &'static str {
        match self {
            ErrClass::Refused => "refused",
            ErrClass::Reset => "reset",
            ErrClass::Eof => "eof",
            ErrClass::Timeout => "timeout",
            ErrClass::Status => "http-status",
            ErrClass::Other => "other",
        }
    }
}

type Fail = (ErrClass, String);

fn classify(e: &std::io::Error, what: &str) -> Fail {
    let class = match e.kind() {
        ErrorKind::ConnectionRefused => ErrClass::Refused,
        ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => ErrClass::Reset,
        ErrorKind::UnexpectedEof => ErrClass::Eof,
        ErrorKind::WouldBlock | ErrorKind::TimedOut => ErrClass::Timeout,
        _ => ErrClass::Other,
    };
    (class, format!("{what}: {e}"))
}

/// macOS refuses options on a socket its peer already reset (EINVAL): a
/// connection the worker dropped, not a client error.
fn sockopt(e: &std::io::Error, what: &str) -> Fail {
    let (class, detail) = classify(e, what);
    (if e.kind() == ErrorKind::InvalidInput { ErrClass::Reset } else { class }, detail)
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub t: f64,
    pub app: String,
    pub client: Client,
    pub class: ErrClass,
    /// The worker that answered the previous request on this connection.
    pub who: Option<String>,
    /// The first request on a new connection (it may have waited in an
    /// accept queue), as opposed to one on a connection a worker had accepted.
    pub fresh: bool,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ended {
    /// Still open when the run ended.
    Open,
    /// A close frame with this code, answered, then the server closed.
    WsClose(u16),
    /// The chunked body's last chunk; `complete`: after a whole event.
    SseEnd { complete: bool },
    /// Cut without a close frame or a last chunk (a browser: 1006, an error).
    Broken(String),
}

#[derive(Debug, Clone)]
pub struct Session {
    pub app: String,
    pub path: String,
    pub who: String,
    pub start: f64,
    pub end: f64,
    pub ended: Ended,
}

/// `<pid>` or `<pid>:<thread>` at the start of a body.
pub fn parse_who(body: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(body);
    let w: String = s.trim().chars().take_while(|c| c.is_ascii_digit() || *c == ':').collect();
    (!w.is_empty() && w.starts_with(|c: char| c.is_ascii_digit())).then_some(w)
}

pub fn who_pid(who: &str) -> Option<u32> {
    who.split(':').next()?.parse().ok()
}

fn connect(port: u16) -> Result<TcpStream, Fail> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).map_err(|e| classify(&e, "connect"))?;
    // Short reads so a stuck read still notices the deadline and `stop`.
    s.set_read_timeout(Some(Duration::from_millis(500))).map_err(|e| sockopt(&e, "set_read_timeout"))?;
    s.set_write_timeout(Some(Duration::from_secs(5))).map_err(|e| sockopt(&e, "set_write_timeout"))?;
    let _ = s.set_nodelay(true);
    Ok(s)
}

/// One more read into `buf`: Ok(true) when bytes came, Ok(false) on a read
/// timeout (nothing yet), Err on EOF, a reset or another error.
fn read_more(s: &mut TcpStream, buf: &mut Vec<u8>) -> Result<bool, Fail> {
    let mut tmp = [0u8; 16384];
    match s.read(&mut tmp) {
        Ok(0) => Err((ErrClass::Eof, "the connection closed before a complete response".into())),
        Ok(n) => {
            buf.extend_from_slice(&tmp[..n]);
            Ok(true)
        }
        Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => Ok(false),
        Err(e) if e.kind() == ErrorKind::Interrupted => Ok(false),
        Err(e) => Err(classify(&e, "read")),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Read until `parse` takes what it needs from the front of `buf`.
fn read_until<T>(
    s: &mut TcpStream,
    buf: &mut Vec<u8>,
    deadline: Instant,
    mut parse: impl FnMut(&mut Vec<u8>) -> Option<T>,
) -> Result<T, Fail> {
    loop {
        if let Some(t) = parse(buf) {
            return Ok(t);
        }
        if Instant::now() > deadline {
            return Err((ErrClass::Timeout, format!("no answer within {}s", REQUEST_TIMEOUT.as_secs())));
        }
        read_more(s, buf)?;
    }
}

fn http_head(b: &mut Vec<u8>) -> Option<String> {
    let i = find(b, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&b[..i]).to_string();
    b.drain(..i + 4);
    Some(head)
}

/// One chunk of a chunked body off the front of `buf` (empty: the last one).
fn http_chunk(b: &mut Vec<u8>) -> Option<Result<Vec<u8>, String>> {
    let i = find(b, b"\r\n")?;
    let line = String::from_utf8_lossy(&b[..i]).to_string();
    let Ok(size) = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16) else {
        return Some(Err(format!("bad chunk size line {line:?}")));
    };
    if b.len() < i + 2 + size + 2 {
        return None;
    }
    let data = b[i + 2..i + 2 + size].to_vec();
    b.drain(..i + 2 + size + 2);
    Some(Ok(data))
}

struct Response {
    status: u16,
    close: bool,
    body: Vec<u8>,
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
    })
}

fn read_response(s: &mut TcpStream, buf: &mut Vec<u8>) -> Result<Response, Fail> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let head = read_until(s, buf, deadline, http_head)?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| (ErrClass::Other, format!("not an HTTP response: {:?}", head.lines().next())))?;
    let close = header(&head, "connection").is_some_and(|v| v.eq_ignore_ascii_case("close"));
    let chunked = header(&head, "transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
    let mut body = Vec::new();
    if chunked {
        loop {
            match read_until(s, buf, deadline, http_chunk)? {
                Ok(d) if d.is_empty() => break,
                Ok(d) => body.extend_from_slice(&d),
                Err(e) => return Err((ErrClass::Other, e)),
            }
        }
    } else if let Some(n) = header(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        while buf.len() < n {
            if Instant::now() > deadline {
                return Err((ErrClass::Timeout, "body not complete in time".into()));
            }
            read_more(s, buf)?;
        }
        body = buf.drain(..n).collect();
    } else if close {
        // Body until EOF.
        loop {
            if Instant::now() > deadline {
                return Err((ErrClass::Timeout, "body not complete in time".into()));
            }
            match read_more(s, buf) {
                Ok(_) => {}
                Err((ErrClass::Eof, _)) => break,
                Err(e) => return Err(e),
            }
        }
        body = std::mem::take(buf);
    }
    Ok(Response { status, close, body })
}

fn pause_unless_stopped(sh: &Shared, d: Duration) {
    if !sh.stopped() {
        std::thread::sleep(d);
    }
}

/// One keep-alive connection, reused until the server closes it
/// (`Connection: close` during a drain) or a request fails.
pub fn keepalive(sh: Arc<Shared>, app: String, port: u16, path: String, pause: Duration) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: warden-chaos\r\n\r\n");
    let mut conn: Option<(TcpStream, Vec<u8>)> = None;
    let mut who: Option<String> = None;
    while !sh.stopped() {
        let fresh = conn.is_none();
        if fresh {
            match connect(port) {
                Ok(s) => conn = Some((s, Vec::new())),
                Err((class, detail)) => {
                    sh.fail(&app, Client::KeepAlive, class, None, true, detail);
                    pause_unless_stopped(&sh, pause.max(Duration::from_millis(50)));
                    continue;
                }
            }
        }
        let Some((s, buf)) = conn.as_mut() else { continue };
        let r = s.write_all(req.as_bytes()).map_err(|e| classify(&e, "write")).and_then(|_| read_response(s, buf));
        match r {
            Ok(resp) => {
                if resp.status == 200 {
                    sh.ok(&app, Client::KeepAlive);
                    if let Some(w) = parse_who(&resp.body) {
                        who = Some(w);
                    }
                } else {
                    sh.fail(
                        &app,
                        Client::KeepAlive,
                        ErrClass::Status,
                        who.clone(),
                        fresh,
                        format!("HTTP {}", resp.status),
                    );
                }
                if resp.close {
                    conn = None;
                    who = None;
                }
            }
            Err((class, detail)) => {
                sh.fail(&app, Client::KeepAlive, class, who.clone(), fresh, detail);
                conn = None;
                who = None;
            }
        }
        pause_unless_stopped(&sh, pause);
    }
}

/// A new connection per request.
pub fn newconn(sh: Arc<Shared>, app: String, port: u16, path: String, pause: Duration) {
    let req =
        format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: warden-chaos\r\nConnection: close\r\n\r\n");
    while !sh.stopped() {
        let r = connect(port).and_then(|mut s| {
            let mut buf = Vec::new();
            s.write_all(req.as_bytes()).map_err(|e| classify(&e, "write"))?;
            read_response(&mut s, &mut buf)
        });
        match r {
            Ok(resp) if resp.status == 200 => sh.ok(&app, Client::NewConn),
            Ok(resp) => {
                let who = parse_who(&resp.body);
                sh.fail(&app, Client::NewConn, ErrClass::Status, who, true, format!("HTTP {}", resp.status));
            }
            Err((class, detail)) => sh.fail(&app, Client::NewConn, class, None, true, detail),
        }
        pause_unless_stopped(&sh, pause);
    }
}

// ------------------------------------------------------------ long-lived

/// One server frame (unmasked) off the front of `buf`: (opcode, payload).
fn ws_frame(b: &mut Vec<u8>) -> Option<(u8, Vec<u8>)> {
    if b.len() < 2 {
        return None;
    }
    let (len, off) = match b[1] & 0x7f {
        126 if b.len() >= 4 => (u16::from_be_bytes([b[2], b[3]]) as usize, 4),
        127 if b.len() >= 10 => {
            let mut n = [0u8; 8];
            n.copy_from_slice(&b[2..10]);
            (u64::from_be_bytes(n) as usize, 10)
        }
        126 | 127 => return None,
        n => (n as usize, 2),
    };
    if b.len() < off + len {
        return None;
    }
    let frame = (b[0] & 0x0f, b[off..off + len].to_vec());
    b.drain(..off + len);
    Some(frame)
}

/// A client frame (masked, payload under 126 bytes).
fn ws_client_frame(op: u8, payload: &[u8]) -> Vec<u8> {
    let mask = [0x37, 0xfa, 0x21, 0x3d];
    let len = payload.len().min(125);
    let mut f = vec![0x80 | op, 0x80 | len as u8, mask[0], mask[1], mask[2], mask[3]];
    f.extend(payload[..len].iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

/// After answering a close frame: the server should close the connection.
fn wait_eof(s: &mut TcpStream, within: Duration) -> Result<(), String> {
    let t0 = Instant::now();
    let mut sink = Vec::new();
    while t0.elapsed() < within {
        match read_more(s, &mut sink) {
            Err((ErrClass::Eof, _)) => return Ok(()),
            Err((_, e)) => return Err(e),
            Ok(_) => sink.clear(),
        }
    }
    Err("the server did not close the connection".into())
}

/// A WebSocket on /ws until the server closes it or the run stops. It sends
/// a message every 5 s and answers pings, as a browser would, so no idle
/// timeout ends it.
fn ws_session(sh: &Shared, port: u16) -> Result<(String, Ended), Fail> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut s = connect(port)?;
    s.write_all(
        b"GET /ws HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
          Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    )
    .map_err(|e| classify(&e, "write"))?;
    let mut buf = Vec::new();
    let head = read_until(&mut s, &mut buf, deadline, http_head)?;
    if !head.starts_with("HTTP/1.1 101") {
        return Err((ErrClass::Status, format!("no upgrade: {:?}", head.lines().next())));
    }
    let who = match read_until(&mut s, &mut buf, deadline, ws_frame)? {
        (1, p) => String::from_utf8_lossy(&p).to_string(),
        (op, _) => return Err((ErrClass::Other, format!("expected the server's hello, got opcode {op}"))),
    };
    let mut last_send = Instant::now();
    let ended = loop {
        if sh.stopped() {
            break Ended::Open;
        }
        if last_send.elapsed() > Duration::from_secs(5) {
            if let Err(e) = s.write_all(&ws_client_frame(1, b"chaos")) {
                break Ended::Broken(format!("write: {e}"));
            }
            last_send = Instant::now();
        }
        if let Some((op, p)) = ws_frame(&mut buf) {
            match op {
                8 => {
                    let code = if p.len() >= 2 { u16::from_be_bytes([p[0], p[1]]) } else { 1005 };
                    let _ = s.write_all(&ws_client_frame(8, &p[..p.len().min(2)]));
                    break match wait_eof(&mut s, Duration::from_secs(3)) {
                        Ok(()) => Ended::WsClose(code),
                        Err(e) => Ended::Broken(format!("close {code}, then {e}")),
                    };
                }
                9 => {
                    let _ = s.write_all(&ws_client_frame(10, &p));
                }
                _ => {}
            }
            continue;
        }
        if let Err((_, e)) = read_more(&mut s, &mut buf) {
            break Ended::Broken(format!("{e} without a close frame (a browser reports 1006)"));
        }
    };
    Ok((who, ended))
}

/// An SSE stream (an EventSource's request) until the server ends it or the
/// run stops.
fn sse_session(sh: &Shared, port: u16, path: &str) -> Result<(String, Ended), Fail> {
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut s = connect(port)?;
    s.write_all(
        format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\nCache-Control: no-cache\r\n\r\n"
        )
        .as_bytes(),
    )
    .map_err(|e| classify(&e, "write"))?;
    let mut buf = Vec::new();
    let head = read_until(&mut s, &mut buf, deadline, http_head)?;
    let lower = head.to_ascii_lowercase();
    if !head.starts_with("HTTP/1.1 200")
        || !lower.contains("content-type: text/event-stream")
        || !lower.contains("transfer-encoding: chunked")
    {
        return Err((ErrClass::Status, format!("not a chunked event stream: {:?}", head.lines().next())));
    }
    // The first event names the worker: "id: 0\ndata: <who> 0\n\n".
    let mut text = String::new();
    let who = loop {
        match read_until(&mut s, &mut buf, deadline, http_chunk)? {
            Ok(d) if d.is_empty() => return Err((ErrClass::Eof, "the stream ended before its first event".into())),
            Ok(d) => {
                text.push_str(&String::from_utf8_lossy(&d));
                if let Some(w) = text.lines().find_map(|l| l.strip_prefix("data: ")) {
                    break w.split(' ').next().unwrap_or("").to_string();
                }
            }
            Err(e) => return Err((ErrClass::Other, e)),
        }
    };
    // Only the tail matters from here: whether the stream ends after a whole event.
    let mut tail = text.chars().rev().take(2).collect::<String>().chars().rev().collect::<String>();
    let ended = loop {
        if sh.stopped() {
            break Ended::Open;
        }
        match http_chunk(&mut buf) {
            Some(Ok(d)) if d.is_empty() => break Ended::SseEnd { complete: tail.ends_with("\n\n") },
            Some(Ok(d)) => {
                tail.push_str(&String::from_utf8_lossy(&d));
                let keep = tail.len().saturating_sub(2);
                tail = tail.get(keep..).unwrap_or("").to_string();
                continue;
            }
            Some(Err(e)) => break Ended::Broken(e),
            None => {}
        }
        if let Err((_, e)) = read_more(&mut s, &mut buf) {
            break Ended::Broken(format!("{e} before the stream's last chunk"));
        }
    };
    Ok((who, ended))
}

/// A long-lived client on `path` (`/ws` or an SSE path) that reconnects at
/// once whenever its connection ends.
pub fn longlived(sh: Arc<Shared>, app: String, port: u16, path: String) {
    let client = if path == "/ws" { Client::Ws } else { Client::Sse };
    while !sh.stopped() {
        let start = sh.now();
        let r = if client == Client::Ws { ws_session(&sh, port) } else { sse_session(&sh, port, &path) };
        match r {
            Ok((who, ended)) => {
                let open = ended == Ended::Open;
                sh.session(Session { app: app.clone(), path: path.clone(), who, start, end: sh.now(), ended });
                if open {
                    return;
                }
            }
            Err((class, detail)) => {
                sh.fail(&app, client, class, None, true, detail);
                pause_unless_stopped(&sh, Duration::from_millis(100));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_comes_from_the_body() {
        assert_eq!(parse_who(b"1234:0").as_deref(), Some("1234:0"));
        assert_eq!(parse_who(b"987\n").as_deref(), Some("987"));
        assert_eq!(parse_who(b"<!doctype html>"), None);
        assert_eq!(who_pid("1234:5"), Some(1234));
    }

    #[test]
    fn chunks_and_frames_parse() {
        let mut b = b"5\r\nhello\r\n0\r\n\r\n".to_vec();
        assert_eq!(http_chunk(&mut b), Some(Ok(b"hello".to_vec())));
        assert_eq!(http_chunk(&mut b), Some(Ok(Vec::new())));
        let mut f = vec![0x88, 2, 0x03, 0xe9];
        assert_eq!(ws_frame(&mut f), Some((8, vec![0x03, 0xe9])));
        assert!(f.is_empty());
        let c = ws_client_frame(1, b"ab");
        assert_eq!(c[1], 0x82);
    }
}

//! What the server says: the heads of file responses, and the small generated
//! responses (errors, redirects, listings).

use super::conn::Conn;
use super::head::Request;
use super::text::{parse_http_date, put, put_dec};
use crate::config::Static;

pub(super) fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        301 => "Moved Permanently",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

/// The parts of a file response's head that don't depend on the request.
pub(super) struct HeadParts<'a> {
    pub(super) etag: &'a str,
    pub(super) last_modified: &'a str,
    pub(super) cache_control: &'a str,
    pub(super) vary: &'a str,
    pub(super) extra: &'a str,
}

/// What a file response's head says about the response: its status, the
/// type and length of the body, the Connection value, the encoding of a
/// precompressed variant and, for a 206, (first byte, file length).
#[derive(Clone, Copy)]
pub(super) struct FileHead<'a> {
    pub(super) code: u16,
    pub(super) mime: &'a str,
    pub(super) count: u64,
    pub(super) conn: &'a str,
    pub(super) encoding: Option<&'a str>,
    pub(super) range: Option<(u64, u64)>,
}

/// The head of a file response, appended to `out`; returns where its
/// Connection value starts in `out` (a cached head is stored with
/// `keep-alive` there and patched for `close`). `range`: (first byte, file
/// length) for a 206.
pub(super) fn file_head_into(out: &mut Vec<u8>, p: &HeadParts, h: &FileHead) -> usize {
    let FileHead { code, mime, count, conn, encoding, range } = *h;
    put(out, "HTTP/1.1 ");
    put_dec(out, u64::from(code));
    out.push(b' ');
    put(out, reason(code));
    put(out, "\r\nContent-Type: ");
    put(out, mime);
    put(out, "\r\nContent-Length: ");
    put_dec(out, count);
    put(out, "\r\nETag: ");
    put(out, p.etag);
    put(out, "\r\nLast-Modified: ");
    put(out, p.last_modified);
    put(out, "\r\nCache-Control: ");
    put(out, p.cache_control);
    put(out, "\r\nAccept-Ranges: bytes\r\n");
    put(out, p.vary);
    put(out, p.extra);
    put(out, "Connection: ");
    let conn_at = out.len();
    put(out, conn);
    put(out, "\r\n");
    if let Some(e) = encoding {
        put(out, "Content-Encoding: ");
        put(out, e);
        put(out, "\r\n");
    }
    if let Some((start, len)) = range {
        put(out, "Content-Range: bytes ");
        put_dec(out, start);
        out.push(b'-');
        put_dec(out, start + count - 1);
        out.push(b'/');
        put_dec(out, len);
        put(out, "\r\n");
    }
    put(out, "\r\n");
    conn_at
}

/// More than a response head needs (about 250 bytes and the configured
/// headers): room for one and the file after it in a single allocation.
const HEAD_ROOM: usize = 384;

/// A buffer for a response head (and `body` bytes after it), allocated once.
pub(super) fn head_buffer(p: &HeadParts, body: usize) -> Vec<u8> {
    Vec::with_capacity(HEAD_ROOM + p.extra.len() + body)
}

/// The 304 head, appended to `out`; returns where its Connection value starts.
pub(super) fn not_modified_head_into(out: &mut Vec<u8>, p: &HeadParts, conn: &str) -> usize {
    put(out, "HTTP/1.1 304 Not Modified\r\nETag: ");
    put(out, p.etag);
    put(out, "\r\nLast-Modified: ");
    put(out, p.last_modified);
    put(out, "\r\nCache-Control: ");
    put(out, p.cache_control);
    put(out, "\r\n");
    put(out, p.vary);
    put(out, p.extra);
    put(out, "Connection: ");
    let conn_at = out.len();
    put(out, conn);
    put(out, "\r\n\r\n");
    conn_at
}

/// Cache-Control of an HTML page (also an index page and the SPA fallback).
/// By default `no-cache`: the browser asks again on every navigation and
/// gets a 304 (one round trip per page load, whatever the file size).
/// `html_max_age` (seconds) lets it reuse the page without asking for that
/// long; unset or 0 keeps `no-cache`. `private` with Basic auth, so a shared
/// cache in front never stores a page that needed a password (`public`
/// would allow it for requests with an Authorization header).
pub(super) fn html_cache_control(cfg: &Static) -> String {
    match cfg.html_max_age {
        None | Some(0) => "no-cache".to_string(),
        Some(n) if cfg.basic_auth.is_some() => format!("private, max-age={n}"),
        Some(n) => format!("public, max-age={n}"),
    }
}

/// If-None-Match (wins when present), else If-Modified-Since.
pub(super) fn is_not_modified(req: &Request<'_>, etag: &str, mtime: u64) -> bool {
    match req.header("if-none-match") {
        Some(tags) => tags.split(',').any(|t| t.trim() == etag || t.trim() == "*"),
        None => req.header("if-modified-since").and_then(parse_http_date).is_some_and(|since| mtime <= since),
    }
}

/// A small response the server makes itself: header lines and a text body.
#[derive(Default)]
pub(super) struct Page<'a> {
    /// Header lines, each ending in CRLF, put before the Content-Length.
    pub(super) headers: &'a str,
    /// The Content-Type of the body (left out when there is none).
    pub(super) mime: &'a str,
    pub(super) body: &'a str,
}

/// Status line, `page`, Content-Length and Connection, and the body unless
/// the request was a HEAD: all in one write (one packet; two writes were two
/// packets and two system calls).
pub(super) async fn respond(
    w: &mut Conn<'_>,
    code: u16,
    keep: bool,
    head_only: bool,
    page: Page<'_>,
) -> std::io::Result<(u16, u64)> {
    let Page { headers, mime, body } = page;
    let mut out = Vec::with_capacity(160 + headers.len() + body.len());
    put(&mut out, "HTTP/1.1 ");
    put_dec(&mut out, u64::from(code));
    out.push(b' ');
    put(&mut out, reason(code));
    put(&mut out, "\r\n");
    put(&mut out, headers);
    if !body.is_empty() {
        put(&mut out, "Content-Type: ");
        put(&mut out, mime);
        put(&mut out, "\r\n");
    }
    put(&mut out, "Content-Length: ");
    put_dec(&mut out, body.len() as u64);
    put(&mut out, if keep { "\r\nConnection: keep-alive\r\n\r\n" } else { "\r\nConnection: close\r\n\r\n" });
    if !head_only {
        put(&mut out, body);
    }
    w.write_all(out).await?;
    w.flush().await?;
    Ok((code, if head_only { 0 } else { body.len() as u64 }))
}

/// A plain-text error: the status and its reason.
pub(super) async fn respond_error(
    w: &mut Conn<'_>,
    code: u16,
    keep: bool,
    head_only: bool,
) -> std::io::Result<(u16, u64)> {
    let body = format!("{code} {}\n", reason(code));
    let headers = if code == 405 { "Allow: GET, HEAD\r\n" } else { "" };
    respond(w, code, keep, head_only, Page { headers, mime: "text/plain; charset=utf-8", body: &body }).await
}

#[cfg(test)]
mod tests {
    use super::super::cache::KEEP_ALIVE;
    use super::super::head::owned_request as req;
    use super::super::names::mime;
    use super::*;

    /// A head as a string, and where its Connection value starts.
    fn file_head(
        p: &HeadParts,
        code: u16,
        mime: &str,
        count: u64,
        conn: &str,
        encoding: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> (String, usize) {
        let mut head = head_buffer(p, 0);
        let conn_at = file_head_into(&mut head, p, &FileHead { code, mime, count, conn, encoding, range });
        (String::from_utf8(head).unwrap(), conn_at)
    }

    fn not_modified_head(p: &HeadParts, conn: &str) -> (String, usize) {
        let mut head = head_buffer(p, 0);
        let conn_at = not_modified_head_into(&mut head, p, conn);
        (String::from_utf8(head).unwrap(), conn_at)
    }

    /// The heads, byte for byte as the server wrote them before they were
    /// split around the Connection value (the cache stores `keep-alive`
    /// and patches `close` in at `conn_at`).
    #[test]
    fn heads_are_byte_identical_and_patchable() {
        let (etag, lm) = ("W/\"5dc-6a1b2c3d-br\"", "Wed, 30 Sep 2026 12:00:01 GMT");
        for (vary, extra) in [("", ""), ("Vary: Accept-Encoding\r\n", "X-Frame-Options: DENY\r\nX-A: b\r\n")] {
            for cache in ["no-cache", "public, max-age=3600"] {
                let p = HeadParts { etag, last_modified: lm, cache_control: cache, vary, extra };
                for conn in ["keep-alive", "close"] {
                    let old_304 = format!(
                        "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nLast-Modified: {lm}\r\nCache-Control: {cache}\r\n\
                         {vary}{extra}Connection: {conn}\r\n\r\n"
                    );
                    assert_eq!(not_modified_head(&p, conn).0, old_304);
                    for (code, encoding, start, count, len) in [
                        (200u16, None, 0u64, 1500u64, 1500u64),
                        (200, Some("br"), 0, 300, 300),
                        (404, None, 0, 14, 14),
                        (206, None, 100, 10_000, 15_000),
                    ] {
                        let mut old = format!(
                            "HTTP/1.1 {code} {}\r\nContent-Type: {}\r\nContent-Length: {count}\r\nETag: {etag}\r\n\
                             Last-Modified: {lm}\r\nCache-Control: {cache}\r\nAccept-Ranges: bytes\r\n{vary}{extra}Connection: {conn}\r\n",
                            reason(code),
                            mime("css"),
                        );
                        if let Some(e) = encoding {
                            old += &format!("Content-Encoding: {e}\r\n");
                        }
                        if code == 206 {
                            old += &format!("Content-Range: bytes {start}-{}/{len}\r\n", start + count - 1);
                        }
                        old += "\r\n";
                        let range = (code == 206).then_some((start, len));
                        assert_eq!(file_head(&p, code, mime("css"), count, conn, encoding, range).0, old);
                    }
                }
                // Patching `close` into the keep-alive form gives the close form.
                let (keep, at) = file_head(&p, 200, "text/css", 3, "keep-alive", Some("gzip"), None);
                let patched = format!("{}close{}", &keep[..at], &keep[at + KEEP_ALIVE.len()..]);
                assert_eq!(patched, file_head(&p, 200, "text/css", 3, "close", Some("gzip"), None).0);
                let (keep, at) = not_modified_head(&p, "keep-alive");
                let patched = format!("{}close{}", &keep[..at], &keep[at + KEEP_ALIVE.len()..]);
                assert_eq!(patched, not_modified_head(&p, "close").0);
            }
        }
    }

    #[test]
    fn html_pages_revalidate_unless_html_max_age_is_set() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        assert_eq!(cfg.html_max_age, None);
        assert_eq!(html_cache_control(&cfg), "no-cache", "the default: ask again on every page load");
        cfg.html_max_age = Some(0);
        assert_eq!(html_cache_control(&cfg), "no-cache", "0 is the same as unset");
        cfg.html_max_age = Some(60);
        assert_eq!(html_cache_control(&cfg), "public, max-age=60");
        cfg.basic_auth = Some("user:pass".into());
        assert_eq!(html_cache_control(&cfg), "private, max-age=60", "a shared cache must not keep a password page");
        // Not a knob for the other files: their policy is cache_max_age.
        cfg.html_max_age = None;
        assert_eq!(html_cache_control(&cfg), "no-cache");
    }

    #[test]
    fn conditional_requests() {
        let etag = "W/\"3-5\"";
        assert!(is_not_modified(&req(&[("If-None-Match", "\"x\", W/\"3-5\"")]).req(), etag, 5));
        assert!(is_not_modified(&req(&[("If-None-Match", "*")]).req(), etag, 5));
        // If-None-Match wins over If-Modified-Since.
        assert!(!is_not_modified(
            &req(&[("If-None-Match", "\"x\""), ("If-Modified-Since", "Wed, 30 Sep 2026 12:00:01 GMT")]).req(),
            etag,
            5
        ));
        assert!(is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:05 GMT")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[("If-Modified-Since", "garbage")]).req(), etag, 5));
        assert!(!is_not_modified(&req(&[]).req(), etag, 5));
    }
}

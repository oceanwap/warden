//! What the server says: the heads of file responses, and the small generated
//! responses (errors, redirects, listings).

use super::conn::Conn;
use super::head::Request;
use super::text::{now, parse_http_date, put, put_dec};
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
        412 => "Precondition Failed",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        _ => "Error",
    }
}

/// The parts of a file response's head that don't depend on the request.
pub(super) struct HeadParts<'a> {
    /// The `Date` value: when this response is made.
    pub(super) date: &'a str,
    pub(super) etag: &'a str,
    /// Empty: no Last-Modified (the file's second is not over).
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

/// Where, in a head, the two values are that differ from one response to the
/// next of a cached one: the date (always `DATE_LEN` bytes, written over with
/// the time of each response) and the Connection value (`keep-alive`, patched
/// for `close`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Spots {
    pub(super) date: usize,
    pub(super) conn: usize,
}

/// The head of a file response, appended to `out`; returns where its Date
/// and Connection values start in `out` (a cached head is stored with the
/// time of its making and `keep-alive`, and patched for each response).
/// `range`: (first byte, file length) for a 206.
pub(super) fn file_head_into(out: &mut Vec<u8>, p: &HeadParts, h: &FileHead) -> Spots {
    let FileHead { code, mime, count, conn, encoding, range } = *h;
    put(out, "HTTP/1.1 ");
    put_dec(out, u64::from(code));
    out.push(b' ');
    put(out, reason(code));
    put(out, "\r\nDate: ");
    let date_at = out.len();
    put(out, p.date);
    put(out, "\r\nContent-Type: ");
    put(out, mime);
    put(out, "\r\nContent-Length: ");
    put_dec(out, count);
    put(out, "\r\nETag: ");
    put(out, p.etag);
    last_modified_into(out, p);
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
    Spots { date: date_at, conn: conn_at }
}

/// The Last-Modified line (without its CRLF, which comes before the next),
/// when there is one.
fn last_modified_into(out: &mut Vec<u8>, p: &HeadParts) {
    if !p.last_modified.is_empty() {
        put(out, "\r\nLast-Modified: ");
        put(out, p.last_modified);
    }
}

/// More than a response head needs (about 290 bytes and the configured
/// headers): room for one and the file after it in a single allocation.
const HEAD_ROOM: usize = 384;

/// A buffer for a response head (and `body` bytes after it), allocated once.
pub(super) fn head_buffer(p: &HeadParts, body: usize) -> Vec<u8> {
    Vec::with_capacity(HEAD_ROOM + p.extra.len() + body)
}

/// The 304 head, appended to `out`; returns where its Date and Connection
/// values start. It says what a 200 would have: the date, the validators, the
/// Cache-Control and the Vary (RFC 9110, 15.4.5), so a cache that takes it
/// to refresh what it holds has the freshness the server means.
pub(super) fn not_modified_head_into(out: &mut Vec<u8>, p: &HeadParts, conn: &str) -> Spots {
    put(out, "HTTP/1.1 304 Not Modified\r\nDate: ");
    let date_at = out.len();
    put(out, p.date);
    put(out, "\r\nETag: ");
    put(out, p.etag);
    last_modified_into(out, p);
    put(out, "\r\nCache-Control: ");
    put(out, p.cache_control);
    put(out, "\r\n");
    put(out, p.vary);
    put(out, p.extra);
    put(out, "Connection: ");
    let conn_at = out.len();
    put(out, conn);
    put(out, "\r\n\r\n");
    Spots { date: date_at, conn: conn_at }
}

/// Cache-Control of a file that is neither a page nor fingerprinted. By
/// default (`cache_max_age = 0`) `no-cache`: the browser asks on every use
/// and gets a 304 while the file is unchanged, so a new deploy is what it
/// shows at once (MDN's advice for a file whose name does not change with
/// its content). `cache_max_age` (seconds) lets it reuse the file without
/// asking for that long, an old one too after a deploy; `private` with Basic
/// auth, so a shared cache in front never keeps a file that needed a password.
pub(super) fn plain_cache_control(cfg: &Static) -> String {
    match cfg.cache_max_age {
        0 => "no-cache".to_string(),
        n if cfg.basic_auth.is_some() => format!("private, max-age={n}"),
        n => format!("public, max-age={n}"),
    }
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

/// If-Match, else If-Unmodified-Since (RFC 9110, 13.2.2, steps 1 and 2):
/// does the client want this version? If-Match compares strongly, and every
/// ETag here is weak, so only `*` meets it; If-Unmodified-Since is met when
/// the file has not changed since (a date that does not read is ignored). A
/// request that is not met gets a 412, never the file.
pub(super) fn preconditions_hold(req: &Request<'_>, mtime: u64) -> bool {
    if let Some(tags) = req.header("if-match") {
        return tags.split(',').any(|t| t.trim() == "*");
    }
    req.header("if-unmodified-since").and_then(parse_http_date).is_none_or(|since| mtime <= since)
}

/// If-None-Match (wins when present), else If-Modified-Since. Tags are
/// compared weakly, as RFC 9110 says for If-None-Match: `W/"x"` and `"x"`
/// are the same version. A date later than `now` is no date (RFC 2616 said
/// so; a client that sends its own clock's time, ahead of the server's,
/// would otherwise be told that a file changed since is the one it has).
pub(super) fn is_not_modified(req: &Request<'_>, etag: &str, mtime: u64, now: u64) -> bool {
    match req.header("if-none-match") {
        Some(tags) => {
            let ours = etag.strip_prefix("W/").unwrap_or(etag);
            tags.split(',').map(str::trim).any(|t| t == "*" || t.strip_prefix("W/").unwrap_or(t) == ours)
        }
        None => req
            .header("if-modified-since")
            .and_then(parse_http_date)
            .is_some_and(|since| since <= now && mtime <= since),
    }
}

/// May the `Range` of the request be answered with a 206? Not when it has an
/// `If-Range` the file does not meet: the client holds part of some version,
/// and gets the whole of this one, never the rest of another (a download
/// resumed after a deploy, a video seeked in a file that was replaced).
/// The comparison is strong (RFC 9110, 13.1.5): every ETag here is weak, so
/// none can meet it; a date meets it when it is the file's modification time
/// exactly, and that time is over (the file cannot change twice within the
/// second it names).
pub(super) fn if_range_allows(req: &Request<'_>, mtime: u64, now: u64) -> bool {
    let Some(v) = req.header("if-range") else { return true };
    let v = v.trim();
    if v.starts_with('"') || v.starts_with("W/") {
        return false;
    }
    parse_http_date(v).is_some_and(|t| t == mtime && mtime < now)
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

/// Status line, Date, `page`, Content-Length and Connection, and the body unless
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
    let mut out = Vec::with_capacity(200 + headers.len() + body.len());
    put(&mut out, "HTTP/1.1 ");
    put_dec(&mut out, u64::from(code));
    out.push(b' ');
    put(&mut out, reason(code));
    put(&mut out, "\r\nDate: ");
    put(&mut out, now().date());
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
    use super::super::text::DATE_LEN;
    use super::*;

    const DATE: &str = "Wed, 30 Sep 2026 12:00:09 GMT";

    /// A head as a string, and where its Date and Connection values start.
    fn file_head(
        p: &HeadParts,
        code: u16,
        mime: &str,
        count: u64,
        conn: &str,
        encoding: Option<&str>,
        range: Option<(u64, u64)>,
    ) -> (String, Spots) {
        let mut head = head_buffer(p, 0);
        let at = file_head_into(&mut head, p, &FileHead { code, mime, count, conn, encoding, range });
        (String::from_utf8(head).unwrap(), at)
    }

    fn not_modified_head(p: &HeadParts, conn: &str) -> (String, Spots) {
        let mut head = head_buffer(p, 0);
        let at = not_modified_head_into(&mut head, p, conn);
        (String::from_utf8(head).unwrap(), at)
    }

    /// The heads, byte for byte as the server wrote them before they were
    /// split around the Date and the Connection value (the cache stores the
    /// time of its making and `keep-alive`, and patches both in at the
    /// `Spots` for each response).
    #[test]
    fn heads_are_byte_identical_and_patchable() {
        let (etag, lm) = ("W/\"5dc-6a1b2c3d-br\"", "Wed, 30 Sep 2026 12:00:01 GMT");
        for (vary, extra) in [("", ""), ("Vary: Accept-Encoding\r\n", "X-Frame-Options: DENY\r\nX-A: b\r\n")] {
            for cache in ["no-cache", "public, max-age=3600"] {
                let p = HeadParts { date: DATE, etag, last_modified: lm, cache_control: cache, vary, extra };
                for conn in ["keep-alive", "close"] {
                    let old_304 = format!(
                        "HTTP/1.1 304 Not Modified\r\nDate: {DATE}\r\nETag: {etag}\r\nLast-Modified: {lm}\r\n\
                         Cache-Control: {cache}\r\n{vary}{extra}Connection: {conn}\r\n\r\n"
                    );
                    assert_eq!(not_modified_head(&p, conn).0, old_304);
                    for (code, encoding, start, count, len) in [
                        (200u16, None, 0u64, 1500u64, 1500u64),
                        (200, Some("br"), 0, 300, 300),
                        (404, None, 0, 14, 14),
                        (206, None, 100, 10_000, 15_000),
                    ] {
                        let mut old = format!(
                            "HTTP/1.1 {code} {}\r\nDate: {DATE}\r\nContent-Type: {}\r\nContent-Length: {count}\r\n\
                             ETag: {etag}\r\nLast-Modified: {lm}\r\nCache-Control: {cache}\r\nAccept-Ranges: bytes\r\n\
                             {vary}{extra}Connection: {conn}\r\n",
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
                // Writing another date over the date and `close` over `keep-alive`
                // gives the head made for that time and connection.
                let other = "Thu, 01 Oct 2026 08:30:00 GMT";
                assert_eq!(other.len(), DATE_LEN);
                let q = HeadParts { date: other, ..p };
                let patch = |head: &str, at: Spots| {
                    format!(
                        "{}{other}{}close{}",
                        &head[..at.date],
                        &head[at.date + DATE_LEN..at.conn],
                        &head[at.conn + KEEP_ALIVE.len()..]
                    )
                };
                let (keep, at) = file_head(&p, 200, "text/css", 3, "keep-alive", Some("gzip"), None);
                assert_eq!(&keep[at.date..at.date + DATE_LEN], DATE);
                assert_eq!(patch(&keep, at), file_head(&q, 200, "text/css", 3, "close", Some("gzip"), None).0);
                let (keep, at) = not_modified_head(&p, "keep-alive");
                assert_eq!(&keep[at.date..at.date + DATE_LEN], DATE);
                assert_eq!(patch(&keep, at), not_modified_head(&q, "close").0);
            }
        }
    }

    /// A file whose second is not over gets no Last-Modified (in the 200 and
    /// the 304 alike); the rest of the head is the same.
    #[test]
    fn heads_without_last_modified() {
        let with = HeadParts {
            date: DATE,
            etag: "W/\"3-5\"",
            last_modified: "Wed, 30 Sep 2026 12:00:01 GMT",
            cache_control: "no-cache",
            vary: "",
            extra: "",
        };
        let without = HeadParts { last_modified: "", ..with };
        let line = "\r\nLast-Modified: Wed, 30 Sep 2026 12:00:01 GMT";
        let (a, _) = file_head(&with, 200, "text/css", 3, "keep-alive", None, None);
        let (b, at) = file_head(&without, 200, "text/css", 3, "keep-alive", None, None);
        assert!(a.contains(line) && !b.contains("Last-Modified"));
        assert_eq!(a.replace(line, ""), b);
        assert_eq!(&b[at.date..at.date + DATE_LEN], DATE);
        assert_eq!(&b[at.conn..at.conn + KEEP_ALIVE.len()], "keep-alive");
        let (a, _) = not_modified_head(&with, "close");
        let (b, _) = not_modified_head(&without, "close");
        assert!(a.contains(line) && !b.contains("Last-Modified"));
        assert_eq!(a.replace(line, ""), b);
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
    fn other_files_revalidate_unless_cache_max_age_is_set() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        assert_eq!(cfg.cache_max_age, 0);
        assert_eq!(plain_cache_control(&cfg), "no-cache", "the default: never an old file after a deploy");
        cfg.cache_max_age = 3600;
        assert_eq!(plain_cache_control(&cfg), "public, max-age=3600");
        cfg.basic_auth = Some("user:pass".into());
        assert_eq!(plain_cache_control(&cfg), "private, max-age=3600", "a shared cache must not keep a password file");
        cfg.cache_max_age = 0;
        assert_eq!(plain_cache_control(&cfg), "no-cache");
    }

    #[test]
    fn conditional_requests() {
        let etag = "W/\"3-5\"";
        let now = 1_790_769_601;
        let nm = |h: &[(&str, &str)]| is_not_modified(&req(h).req(), etag, 5, now);
        assert!(nm(&[("If-None-Match", "\"x\", W/\"3-5\"")]));
        assert!(nm(&[("If-None-Match", "*")]));
        // If-None-Match wins over If-Modified-Since.
        assert!(!nm(&[("If-None-Match", "\"x\""), ("If-Modified-Since", "Wed, 30 Sep 2026 12:00:01 GMT")]));
        assert!(nm(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:05 GMT")]));
        assert!(!nm(&[("If-Modified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]));
        assert!(!nm(&[("If-Modified-Since", "garbage")]));
        assert!(!nm(&[]));
        // A date after the server's now is no date: the file is sent.
        assert!(nm(&[("If-Modified-Since", "Wed, 30 Sep 2026 12:00:01 GMT")]), "now itself is fine");
        assert!(!nm(&[("If-Modified-Since", "Wed, 30 Sep 2026 12:00:02 GMT")]), "a second ahead");
        assert!(!nm(&[("If-Modified-Since", "Fri, 01 Jan 2100 00:00:00 GMT")]));
    }

    /// If-Match and If-Unmodified-Since: a request for a version this file
    /// is not gets a 412. Every ETag here is weak, and If-Match compares
    /// strongly, so a tag never meets it; `*` does (the file exists).
    #[test]
    fn preconditions_ask_for_this_version() {
        let holds = |h: &[(&str, &str)]| preconditions_hold(&req(h).req(), 5);
        assert!(holds(&[]));
        assert!(holds(&[("If-Match", "*")]));
        assert!(holds(&[("If-Match", "\"a\", *")]));
        assert!(!holds(&[("If-Match", "W/\"3-5\"")]), "weak: never a strong match");
        assert!(!holds(&[("If-Match", "\"3-5\"")]));
        assert!(!holds(&[("If-Match", "")]));
        assert!(holds(&[("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:05 GMT")]));
        assert!(holds(&[("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:09 GMT")]));
        assert!(!holds(&[("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]), "changed since");
        assert!(holds(&[("If-Unmodified-Since", "garbage")]), "a date that does not read is ignored");
        // If-Match wins over If-Unmodified-Since.
        assert!(holds(&[("If-Match", "*"), ("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:04 GMT")]));
        assert!(!holds(&[("If-Match", "\"x\""), ("If-Unmodified-Since", "Thu, 01 Jan 1970 00:00:09 GMT")]));
        assert_eq!(reason(412), "Precondition Failed");
    }

    /// If-None-Match compares weakly: the tag with or without `W/` is the
    /// same version (a proxy or a client may drop the prefix), another one is not.
    #[test]
    fn if_none_match_compares_weakly() {
        let etag = "W/\"3-5\"";
        for (sent, want) in [
            ("W/\"3-5\"", true),
            ("\"3-5\"", true),
            ("\"a\", \"3-5\" , W/\"b\"", true),
            ("\"3-6\"", false),
            ("W/\"3-6\"", false),
            ("3-5", false),
            ("W/3-5", false),
            ("", false),
        ] {
            assert_eq!(is_not_modified(&req(&[("If-None-Match", sent)]).req(), etag, 5, 9), want, "{sent:?}");
        }
        // The same for a tag that is not weak.
        assert!(is_not_modified(&req(&[("If-None-Match", "W/\"x\"")]).req(), "\"x\"", 5, 9));
    }

    /// A range is answered only for the version the client holds part of.
    #[test]
    fn if_range_asks_for_the_same_version() {
        let allowed = |v: &[(&str, &str)], mtime, now| if_range_allows(&req(v).req(), mtime, now);
        let lm = "Wed, 30 Sep 2026 12:00:01 GMT";
        let t = 1_790_769_601;
        // No If-Range: the range stands.
        assert!(allowed(&[], t, t + 100));
        // A date that is the file's, long past: it stands.
        assert!(allowed(&[("If-Range", lm)], t, t + 100));
        assert!(allowed(&[("If-Range", lm)], t, t + 1), "the second is over");
        // Another date, an older or a newer one: the whole file.
        assert!(!allowed(&[("If-Range", "Wed, 30 Sep 2026 12:00:00 GMT")], t, t + 100));
        assert!(!allowed(&[("If-Range", "Wed, 30 Sep 2026 12:00:02 GMT")], t, t + 100));
        // The file's second is not over: it may change again within it.
        assert!(!allowed(&[("If-Range", lm)], t, t));
        assert!(!allowed(&[("If-Range", lm)], t, t - 5), "a clock behind the file");
        // A tag is compared strongly, and every tag here is weak.
        assert!(!allowed(&[("If-Range", "W/\"3-5\"")], 5, t));
        assert!(!allowed(&[("If-Range", "\"3-5\"")], 5, t));
        // Nonsense is not a match.
        assert!(!allowed(&[("If-Range", "garbage")], t, t + 100));
        assert!(!allowed(&[("If-Range", "")], t, t + 100));
    }
}

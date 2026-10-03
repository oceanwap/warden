//! The request: finding and parsing a head, and the values of the headers
//! the server reads (`Range`, `Accept-Encoding`).

use super::Site;
use tokio::io::AsyncReadExt;

pub(super) const MAX_HEAD: usize = 16 * 1024;

/// One request head, borrowed from the connection's read buffer: nothing is
/// copied or allocated per request. The headers the server looks at on every
/// request are picked out in one pass over the head; any other name is
/// searched for in `rest` when asked for. A name that appears twice gives its
/// first value.
pub(super) struct Request<'a> {
    pub(super) method: &'a str,
    pub(super) path: &'a str,
    pub(super) keep_alive: bool,
    pub(super) accept_encoding: Option<&'a str>,
    pub(super) range: Option<&'a str>,
    pub(super) if_none_match: Option<&'a str>,
    pub(super) if_modified_since: Option<&'a str>,
    /// The rare preconditions: looked for on every file response, so picked
    /// out with the others (a search of `rest` for each cost more).
    pub(super) if_match: Option<&'a str>,
    pub(super) if_unmodified_since: Option<&'a str>,
    pub(super) if_range: Option<&'a str>,
    pub(super) authorization: Option<&'a str>,
    pub(super) accept: Option<&'a str>,
    /// The header lines, as received.
    pub(super) rest: &'a str,
}

impl<'a> Request<'a> {
    pub(super) fn header(&self, name: &str) -> Option<&'a str> {
        let hot = match name {
            "accept-encoding" => Some(self.accept_encoding),
            "range" => Some(self.range),
            "if-none-match" => Some(self.if_none_match),
            "if-modified-since" => Some(self.if_modified_since),
            "if-match" => Some(self.if_match),
            "if-unmodified-since" => Some(self.if_unmodified_since),
            "if-range" => Some(self.if_range),
            "authorization" => Some(self.authorization),
            "accept" => Some(self.accept),
            _ => None,
        };
        if let Some(v) = hot {
            return v;
        }
        self.rest.split('\n').find_map(|l| {
            let (k, v) = l.strip_suffix('\r').unwrap_or(l).split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

/// A connection's bytes read but not yet used: the request head being
/// received, and anything the client sent after it. Buffers are reused from
/// one connection to the next (`Site::bufs`).
pub(super) struct HeadBuf {
    pub(super) buf: Vec<u8>,
    /// Where the unused bytes start.
    pub(super) pos: usize,
    /// How far into the head being received (from `pos`) `find_head` has
    /// looked: the start of its first unfinished line. A head arriving a few
    /// bytes at a time is then scanned once, not once per read.
    pub(super) scan: usize,
}

const HEAD_BUF: usize = 8 * 1024;
const HEAD_BUFS_KEPT: usize = 256;

impl HeadBuf {
    pub(super) fn take(site: &Site) -> HeadBuf {
        let buf = site.bufs.lock().unwrap_or_else(|e| e.into_inner()).pop();
        HeadBuf { buf: buf.unwrap_or_else(|| Vec::with_capacity(HEAD_BUF)), pos: 0, scan: 0 }
    }

    pub(super) fn give_back(mut self, site: &Site) {
        // A buffer that grew past the head limit is not worth keeping.
        if self.buf.capacity() <= 2 * MAX_HEAD {
            self.buf.clear();
            let mut pool = site.bufs.lock().unwrap_or_else(|e| e.into_inner());
            if pool.len() < HEAD_BUFS_KEPT {
                pool.push(self.buf);
            }
        }
    }

    /// The head just found has been served: its `n` bytes are used up.
    pub(super) fn consume(&mut self, n: usize) {
        self.pos += n;
        self.scan = 0;
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
    }

    /// Room to read more: the bytes already used are dropped from the front.
    fn make_room(&mut self) {
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
            self.scan = 0;
        } else if self.pos > 0 && self.buf.capacity() - self.buf.len() < 1024 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.capacity() - self.buf.len() < 1024 {
            self.buf.reserve(HEAD_BUF);
        }
    }
}

/// Where the head in `buf` ends.
#[derive(Debug, PartialEq)]
pub(super) enum Head {
    /// `len` bytes from `skip` on are a whole head (up to and including its
    /// blank line); `skip` is the blank lines before it (a client may send a
    /// stray one between requests).
    Done { skip: usize, len: usize },
    /// Not all here yet. `skip` is the blank lines before the head; `scan`
    /// where to pick up the search (from the head's start, after `skip`) when
    /// more has arrived: the first line not yet ended.
    More { skip: usize, scan: usize },
    /// Over the 16 KB limit before its blank line.
    TooLong,
}

/// Find the end of the head at the start of `buf`: the first blank line after
/// at least one other line, `\r\n` or a bare `\n` ending each line. A head
/// may be at most `MAX_HEAD` bytes with its blank line.
pub(super) fn find_head(buf: &[u8]) -> Head {
    find_head_from(buf, 0)
}

/// `find_head`, resuming after an earlier `More`: `buf` starts at the head
/// (the blank lines before it dropped) and its lines before `scan` are known
/// not to end it.
fn find_head_from(buf: &[u8], scan: usize) -> Head {
    let mut start = 0;
    if scan == 0 {
        loop {
            match &buf[start..] {
                [b'\n', ..] => start += 1,
                [b'\r', b'\n', ..] => start += 2,
                // Perhaps the first half of a blank line.
                [b'\r'] => return Head::More { skip: start, scan: 0 },
                _ => break,
            }
        }
    }
    let mut line = start + scan;
    loop {
        let Some(nl) = memchr::memchr(b'\n', &buf[line..]) else {
            return if buf.len() - start > MAX_HEAD {
                Head::TooLong
            } else {
                Head::More { skip: start, scan: line - start }
            };
        };
        let end = line + nl + 1;
        if end - start > MAX_HEAD {
            return Head::TooLong;
        }
        let l = &buf[line..end];
        if line > start && (l == b"\n" || l == b"\r\n") {
            return Head::Done { skip: start, len: end - start };
        }
        line = end;
    }
}

/// Read until one whole request head is in the buffer; its length (from
/// `h.pos`). `Ok(None)`: the client closed between requests. Errors are the
/// status to answer with: 431 over the head limit, 400 for a read error or a
/// close in the middle of a head. A line is never read past the limit, so a
/// client that never sends a newline gets 431, not unbounded buffering.
pub(super) async fn read_head<R: tokio::io::AsyncRead + Unpin>(
    r: &mut R,
    h: &mut HeadBuf,
) -> Result<Option<usize>, u16> {
    loop {
        match find_head_from(&h.buf[h.pos..], h.scan) {
            Head::Done { skip, len } => {
                h.pos += skip;
                h.scan = 0;
                return Ok(Some(len));
            }
            Head::TooLong => return Err(431),
            Head::More { skip, scan } => {
                h.pos += skip;
                h.scan = scan;
            }
        }
        h.make_room();
        let n = r.read_buf(&mut h.buf).await.map_err(|_| 400u16)?;
        if n == 0 {
            return if h.pos == h.buf.len() { Ok(None) } else { Err(400) };
        }
    }
}

/// Parse a head found by `find_head`. Errors: 400 for bytes that are not
/// UTF-8 or a request line without a target.
pub(super) fn parse_head(bytes: &[u8]) -> Result<Request<'_>, u16> {
    let text = std::str::from_utf8(bytes).map_err(|_| 400u16)?;
    let b = text.as_bytes();
    let first_end = memchr::memchr(b'\n', b).unwrap_or(b.len());
    let rest = text.get(first_end + 1..).unwrap_or("");
    let first = text[..first_end].strip_suffix('\r').unwrap_or(&text[..first_end]);
    let mut parts = first.split(' ');
    let (method, target, version) =
        (parts.next().unwrap_or(""), parts.next().ok_or(400u16)?, parts.next().unwrap_or(""));
    let mut req = Request {
        method,
        path: target,
        keep_alive: false,
        accept_encoding: None,
        range: None,
        if_none_match: None,
        if_modified_since: None,
        if_match: None,
        if_unmodified_since: None,
        if_range: None,
        authorization: None,
        accept: None,
        rest,
    };
    let mut connection = None;
    let mut at = 0;
    let r = rest.as_bytes();
    while at < r.len() {
        let end = memchr::memchr(b'\n', &r[at..]).map_or(r.len(), |i| at + i);
        let line = &rest[at..end];
        at = end + 1;
        let line = line.strip_suffix('\r').unwrap_or(line);
        let Some(colon) = memchr::memchr(b':', line.as_bytes()) else { continue };
        let (k, v) = (line[..colon].trim(), line[colon + 1..].trim());
        let slot = match k.len() {
            5 if k.eq_ignore_ascii_case("range") => &mut req.range,
            6 if k.eq_ignore_ascii_case("accept") => &mut req.accept,
            8 if k.eq_ignore_ascii_case("if-match") => &mut req.if_match,
            8 if k.eq_ignore_ascii_case("if-range") => &mut req.if_range,
            10 if k.eq_ignore_ascii_case("connection") => &mut connection,
            13 if k.eq_ignore_ascii_case("if-none-match") => &mut req.if_none_match,
            13 if k.eq_ignore_ascii_case("authorization") => &mut req.authorization,
            15 if k.eq_ignore_ascii_case("accept-encoding") => &mut req.accept_encoding,
            17 if k.eq_ignore_ascii_case("if-modified-since") => &mut req.if_modified_since,
            19 if k.eq_ignore_ascii_case("if-unmodified-since") => &mut req.if_unmodified_since,
            _ => continue,
        };
        slot.get_or_insert(v);
    }
    req.keep_alive = match version {
        "HTTP/1.1" => !connection.is_some_and(|c| c.eq_ignore_ascii_case("close")),
        _ => connection.is_some_and(|c| c.eq_ignore_ascii_case("keep-alive")),
    };
    Ok(req)
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` against `len`. None: ignore (not a
/// single bytes range); Some(Err): unsatisfiable.
pub(super) fn parse_range(h: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = h.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None; // multiple ranges: send the whole file
    }
    let (a, b) = spec.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    let r = match (a.is_empty(), b.is_empty()) {
        (true, false) => {
            let n: u64 = b.parse().ok()?;
            if n == 0 || len == 0 {
                return Some(Err(()));
            }
            (len.saturating_sub(n), len - 1)
        }
        (false, _) => {
            let start: u64 = a.parse().ok()?;
            let end: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse().ok()? };
            if start >= len || end < start {
                return Some(Err(()));
            }
            (start, end.min(len - 1))
        }
        _ => return None,
    };
    Some(Ok(r))
}

/// Precompressed encodings (in preference order) and their file suffixes.
pub(super) const ENCODINGS: [(&str, &str); 2] = [("br", "br"), ("gzip", "gz")];

/// Whether an `Accept-Encoding` header takes `enc` (RFC 9110, 12.5.3): it names
/// it, in any letter case, and not with a quality of 0, which means "not this
/// one".
pub(super) fn accepts(accept_encoding: &str, enc: &str) -> bool {
    accept_encoding.split(',').any(|item| {
        let mut parts = item.split(';');
        parts.next().is_some_and(|name| name.trim().eq_ignore_ascii_case(enc)) && !parts.any(refuses)
    })
}

/// A parameter of the form `q=0`, `Q = 0.00`.
fn refuses(param: &str) -> bool {
    param
        .split_once('=')
        .is_some_and(|(k, v)| k.trim().eq_ignore_ascii_case("q") && v.trim().parse::<f32>().is_ok_and(|q| q == 0.0))
}

/// A request head with the given headers, for the tests of several modules.
#[cfg(test)]
pub(super) struct Owned(String);

#[cfg(test)]
impl Owned {
    pub(super) fn req(&self) -> Request<'_> {
        parse_head(self.0.as_bytes()).unwrap()
    }
}

#[cfg(test)]
pub(super) fn owned_request(lines: &[(&str, &str)]) -> Owned {
    let mut head = "GET /a.css HTTP/1.1\r\n".to_string();
    for (k, v) in lines {
        head += &format!("{k}: {v}\r\n");
    }
    head += "\r\n";
    Owned(head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn accept_encoding_names_are_matched_the_way_the_rfc_says() {
        assert!(accepts("gzip, deflate, br", "br") && accepts("gzip, deflate, br", "gzip"));
        assert!(accepts("GZip", "gzip") && accepts(" br ; q=0.8", "br"));
        assert!(accepts("br;q=0.001", "br") && accepts("gzip;level=1", "gzip"));
        // A quality of 0 refuses the encoding; the others in the list stay.
        for refused in ["br;q=0", "br; q=0.0", "BR;Q=0.000", "gzip, br;q=0"] {
            assert!(!accepts(refused, "br"), "{refused}");
        }
        assert!(accepts("br;q=0, gzip", "gzip") && !accepts("br;q=0, gzip;q=0", "gzip"));
        // Not named, or only part of a name.
        assert!(!accepts("", "br") && !accepts("identity", "gzip") && !accepts("x-gzip, brotli", "gzip"));
        assert!(!accepts("*", "gzip"), "a wildcard is answered with the file as it is");
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-9", 100), Some(Ok((0, 9))));
        assert_eq!(parse_range("bytes=90-", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=-10", 100), Some(Ok((90, 99))));
        assert_eq!(parse_range("bytes=50-500", 100), Some(Ok((50, 99))));
        assert_eq!(parse_range("bytes=100-", 100), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,5-6", 100), None);
        assert_eq!(parse_range("items=0-1", 100), None);
    }

    /// The head parser, on bytes arriving as the test says.
    async fn heads(chunks: &[&[u8]]) -> (Result<Option<usize>, u16>, HeadBuf) {
        let (mut tx, mut rx) = tokio::io::duplex(1 << 20);
        for c in chunks {
            tx.write_all(c).await.unwrap();
        }
        drop(tx);
        let mut h = HeadBuf { buf: Vec::with_capacity(HEAD_BUF), pos: 0, scan: 0 };
        let got = read_head(&mut rx, &mut h).await;
        (got, h)
    }

    /// A head line that never ends is refused at the limit, not buffered.
    #[tokio::test]
    async fn endless_head_lines_are_cut_off() {
        let mut endless = tokio::io::repeat(b'a');
        let mut h = HeadBuf { buf: Vec::new(), pos: 0, scan: 0 };
        assert_eq!(read_head(&mut endless, &mut h).await.err(), Some(431));
        assert!(h.buf.len() <= MAX_HEAD + 2 * HEAD_BUF, "bounded: {}", h.buf.len());
    }

    #[tokio::test]
    async fn heads_are_found_however_they_arrive() {
        let (got, h) = heads(&[b"\r\nGET /x HTTP/1.0\r\nConnection: keep-alive\r\nA:b\r\n\r\nrest"]).await;
        let len = got.unwrap().unwrap();
        let req = parse_head(&h.buf[h.pos..h.pos + len]).unwrap();
        assert_eq!((req.method, req.path, req.keep_alive), ("GET", "/x", true));
        assert_eq!(req.header("a"), Some("b"));
        assert_eq!(&h.buf[h.pos + len..], b"rest", "what follows stays for the next request");

        // Split anywhere, a bare \n for line ends, a lone CR before the LF arrives.
        let whole: &[u8] = b"GET /y HTTP/1.1\nHost: x\n\n";
        for cut in 1..whole.len() {
            let (got, h) = heads(&[&whole[..cut], &whole[cut..]]).await;
            let len = got.unwrap().unwrap();
            assert_eq!(&h.buf[h.pos..h.pos + len], whole, "cut at {cut}");
        }
        let (got, h) = heads(&[b"GET /z HTTP/1.1\r\n\r", b"\n"]).await;
        let len = got.unwrap().unwrap();
        assert_eq!(&h.buf[h.pos..h.pos + len], b"GET /z HTTP/1.1\r\n\r\n");

        let (got, _) = heads(&[b"GET / HTTP/1.1\r\nHost: x"]).await;
        assert_eq!(got.err(), Some(400), "cut short");
        let (got, _) = heads(&[b""]).await;
        assert!(got.unwrap().is_none(), "clean end");
        let (got, _) = heads(&[b"\r\n\r\n"]).await;
        assert!(got.unwrap().is_none(), "only blank lines, then the end: clean");
    }

    #[test]
    fn the_head_limit_counts_the_blank_line() {
        let mut head = b"GET / HTTP/1.1\r\nX: ".to_vec();
        let filler = MAX_HEAD - head.len() - 4;
        head.extend(std::iter::repeat_n(b'a', filler));
        head.extend_from_slice(b"\r\n\r\n");
        assert_eq!(head.len(), MAX_HEAD);
        assert_eq!(find_head(&head), Head::Done { skip: 0, len: MAX_HEAD }, "exactly the limit");
        let mut over = head.clone();
        over.insert(20, b'a');
        assert_eq!(find_head(&over), Head::TooLong, "one byte more");
        assert_eq!(find_head(&head[..MAX_HEAD - 1]), Head::More { skip: 0, scan: MAX_HEAD - 2 });
        assert_eq!(find_head(b"\r\n\nGET / HTTP/1.1\r\n"), Head::More { skip: 3, scan: 16 });
    }

    /// Resuming from where the last search stopped finds what a search of
    /// the whole buffer finds, however the bytes are cut up.
    #[test]
    fn a_head_found_in_pieces_is_the_head_found_whole() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let (mut done, mut more, mut too_long) = (0, 0, 0);
        for round in 0..20_000 {
            // Few distinct bytes, so that blank lines, bare CRs and lines
            // that end are all common.
            let len = 1 + next() as usize % 60;
            let buf: Vec<u8> = (0..len).map(|_| b"ab\r\n\n"[next() as usize % 5]).collect();
            let (mut pos, mut scan) = (0, 0);
            let mut end = 0;
            while end < buf.len() {
                end = (end + 1 + next() as usize % 7).min(buf.len());
                let piece = find_head_from(&buf[pos..end], scan);
                let whole = find_head(&buf[..end]);
                let want = match piece {
                    Head::Done { skip, len } => Head::Done { skip: pos + skip, len },
                    Head::TooLong => Head::TooLong,
                    Head::More { skip, scan: s } => {
                        let w = Head::More { skip: pos + skip, scan: s };
                        pos += skip;
                        scan = s;
                        w
                    }
                };
                assert_eq!(want, whole, "round {round}, {:?} up to {end}", String::from_utf8_lossy(&buf));
                match whole {
                    Head::Done { .. } => {
                        done += 1;
                        break;
                    }
                    Head::More { .. } => more += 1,
                    Head::TooLong => {
                        too_long += 1;
                        break;
                    }
                }
            }
        }
        assert!(done > 2000 && more > 2000, "the cases are varied: {done} {more} {too_long}");

        // Past the limit, in pieces: the same answer, and each piece looks
        // at the new bytes only (the search resumes at the unfinished line).
        let mut buf = Vec::new();
        let (mut pos, mut scan) = (0, 0);
        let mut got = None;
        for _ in 0..200 {
            buf.extend(std::iter::repeat_n(*b"a\n", 100).flatten());
            match find_head_from(&buf[pos..], scan) {
                Head::More { skip, scan: s } => {
                    assert_eq!(skip, 0);
                    assert_eq!(s, buf.len(), "every line so far has ended: resume after them");
                    (pos, scan) = (pos + skip, s);
                }
                other => {
                    got = Some(other);
                    break;
                }
            }
        }
        assert_eq!(got, Some(Head::TooLong));
        assert_eq!(find_head(&buf), Head::TooLong);
    }

    /// The first value of a name wins, names are case-insensitive, values are
    /// trimmed, and lines without a colon are ignored.
    #[test]
    fn headers_are_picked_out_as_before() {
        let head = b"GET /p?q=1 HTTP/1.1\r\nrange:  bytes=0-1 \r\nRANGE: bytes=5-6\r\nnonsense\r\n\
                     Accept-Encoding:gzip\r\nX-Other: 1\r\nx-other: 2\r\nIf-None-Match: \"a\"\r\n\
                     Authorization: Basic eA==\r\nAccept: text/html\r\nIf-Modified-Since: d\r\n\
                     if-match: *\r\nIF-RANGE: \"r\"\r\nIf-Unmodified-Since: u\r\nIf-Match: x\r\n\r\n";
        let r = parse_head(head).unwrap();
        assert_eq!((r.method, r.path), ("GET", "/p?q=1"));
        assert_eq!(r.header("range"), Some("bytes=0-1"));
        assert_eq!(r.header("accept-encoding"), Some("gzip"));
        assert_eq!(r.header("if-none-match"), Some("\"a\""));
        assert_eq!(r.header("authorization"), Some("Basic eA=="));
        assert_eq!(r.header("accept"), Some("text/html"));
        assert_eq!(r.header("if-modified-since"), Some("d"));
        assert_eq!(r.header("if-match"), Some("*"));
        assert_eq!(r.header("if-range"), Some("\"r\""));
        assert_eq!(r.header("if-unmodified-since"), Some("u"));
        assert_eq!(r.header("x-other"), Some("1"));
        assert_eq!(r.header("nonsense"), None);
        assert_eq!(r.header("host"), None);
        assert!(r.keep_alive, "HTTP/1.1 stays open by default");
        let close = parse_head(b"GET / HTTP/1.1\r\nConnection: Close\r\n\r\n").unwrap();
        assert!(!close.keep_alive);
        let old = parse_head(b"GET / HTTP/1.0\r\n\r\n").unwrap();
        assert!(!old.keep_alive, "HTTP/1.0 closes unless asked");
        let no_version = parse_head(b"GET /\r\n\r\n").unwrap();
        assert_eq!((no_version.path, no_version.keep_alive), ("/", false));
        assert_eq!(parse_head(b"GET\r\n\r\n").err(), Some(400), "no target");
        assert_eq!(parse_head(b"GET /\xff HTTP/1.1\r\n\r\n").err(), Some(400), "not UTF-8");
    }
}

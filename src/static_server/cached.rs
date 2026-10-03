//! The optional response cache as the handler uses it: the key of a request,
//! the lookup, the entry built on a miss, and answering from one. (`cache.rs`
//! is the store itself.)

use super::cache::{Cache, Dated, Dep, Entry, KEEP_ALIVE, Lookup, Stamp};
use super::conn::{Conn, OutBuf, Source};
use super::head::{ENCODINGS, Request, accepts};
use super::open::{Opened, open, read_body};
use super::response::{FileHead, HeadParts, file_head_into, head_buffer, is_not_modified, not_modified_head_into};
use super::text::{Now, now, parse_http_date};
use super::{Exchange, Site};
use crate::config::Static;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Instant, SystemTime};

/// A cache miss being served: the response is cached if it turns out to
/// be a plain 200 from a small file. `deps` collects every lookup that
/// chose it, so a later check can tell whether the same request would
/// still get the same response.
pub(super) struct Fill {
    pub(super) key: String,
    pub(super) deps: Vec<Dep>,
}

/// What the cache says about a request.
pub(super) enum Cached {
    /// A response to send as it is.
    Hit(Arc<Entry>),
    /// Not there: serve normally, and keep the response if it qualifies.
    Miss(Fill),
    /// Not consulted: no cache, or a request that bypasses it (a range).
    Skip,
}

/// Look the request up. The cache answers plain GET / HEAD (with
/// If-None-Match or If-Modified-Since or neither) for a path that passed the
/// checks before; ranges, and the rare If-Match and If-Unmodified-Since,
/// take the normal path. `key` is the connection's buffer for the key.
pub(super) async fn consult(x: Exchange<'_>, rel: &str, slash: bool, key: &mut String) -> Cached {
    let Some(cache) = &x.site.cache else { return Cached::Skip };
    if ["range", "if-match", "if-unmodified-since"].iter().any(|h| x.req.header(h).is_some()) {
        return Cached::Skip;
    }
    cache_key_into(key, rel, &x.site.cfg, x.req, slash);
    let now = Instant::now();
    let hit = match cache.lookup(key, now) {
        Lookup::Fresh(e) => Some(e),
        Lookup::Stale(e) if still_matches(x.site, &e.deps).await => {
            cache.confirm(key, &e, now);
            Some(e)
        }
        Lookup::Stale(e) => {
            cache.invalidate(key, &e);
            None
        }
        Lookup::Miss => None,
    };
    match hit {
        Some(e) => {
            cache.hits.fetch_add(1, Ordering::Relaxed);
            Cached::Hit(e)
        }
        None => {
            cache.misses.fetch_add(1, Ordering::Relaxed);
            Cached::Miss(Fill { key: key.clone(), deps: Vec::new() })
        }
    }
}

/// Cache key: the checked relative path, which precompressed encodings the
/// client accepts (they pick the variant), and whether the URL ended in a
/// slash (`/docs` is a redirect, `/docs/` the index page). Built into `out`
/// (cleared first), a buffer the connection keeps, so a cache hit allocates
/// nothing for it.
fn cache_key_into(out: &mut String, rel: &str, cfg: &Static, req: &Request, slash: bool) {
    let mut mask = 0u8;
    if cfg.precompressed {
        let accept = req.header("accept-encoding").unwrap_or("");
        for (bit, (enc, _)) in ENCODINGS.iter().enumerate() {
            if accepts(accept, enc) {
                mask |= 1 << bit;
            }
        }
    }
    out.clear();
    out.push_str(rel);
    // NUL can't be in `rel` (relative() refuses it), so keys can't collide.
    out.push('\0');
    out.push((b'0' + mask) as char);
    if slash {
        out.push('/');
    }
}

#[cfg(test)]
fn cache_key(rel: &str, cfg: &Static, req: &Request, slash: bool) -> String {
    let mut out = String::new();
    cache_key_into(&mut out, rel, cfg, req, slash);
    out
}

/// Would the same lookups find the same things now? Each path goes through
/// the normal open path, so a file that was swapped for a symlink leaving
/// the root no longer matches.
async fn still_matches(site: &Site, deps: &[Dep]) -> bool {
    for d in deps {
        let now = open(site, &d.path).await.ok();
        if !d.seen.matches(now.as_ref().map(|o| &o.meta)) {
            return false;
        }
    }
    true
}

/// Does this response go into the cache? A plain 200 of a small file that
/// has not changed lately (a change within the same second as the last
/// could go unnoticed).
pub(super) fn fits(cache: &Cache, status: u16, meta: &std::fs::Metadata) -> bool {
    status == 200 && meta.len() <= cache.max_file && meta.is_file() && !Stamp::of(meta).racy(SystemTime::now())
}

/// Build the whole response of a file once (the head, and the body read
/// after it), keep it unless the file changed while it was read, and return
/// it to answer from.
pub(super) async fn store(
    cache: &Cache,
    fill: Fill,
    parts: &HeadParts<'_>,
    mime: &str,
    encoding: Option<&str>,
    body: Opened,
    mtime: u64,
) -> std::io::Result<Arc<Entry>> {
    let len = body.meta.len();
    let stamp = Stamp::of(&body.meta);
    let mut buf = head_buffer(parts, len as usize);
    let head = FileHead { code: 200, mime, count: len, conn: "keep-alive", encoding, range: None };
    let spots = file_head_into(&mut buf, parts, &head);
    let head_len = buf.len();
    let mut not_modified = head_buffer(parts, 0);
    let nm_spots = not_modified_head_into(&mut not_modified, parts, "keep-alive");
    buf.resize(head_len + len as usize, 0);
    let (file, buf) = read_body(body.file, buf, head_len, 0).await?;
    let unchanged = file.metadata().is_ok_and(|m| Stamp::of(&m) == stamp);
    // A big body goes into a memfd (sent by page reference); the head stays
    // in memory.
    let mem = cache.memfd(&buf[head_len..], len).map(Arc::new);
    let resp: Arc<[u8]> = if mem.is_some() { buf[..head_len].into() } else { buf.into() };
    // The second the heads were dated with (a date that does not read back
    // just means the first response dates them anew).
    let secs = parse_http_date(parts.date).unwrap_or(u64::MAX);
    let entry = Arc::new(Entry {
        resp: Dated::new(resp, spots.date, secs),
        file: mem,
        head_len,
        conn_at: spots.conn,
        not_modified: Dated::new(not_modified.into(), nm_spots.date, secs),
        nm_conn_at: nm_spots.conn,
        etag: parts.etag.into(),
        mtime,
        body_len: len,
        deps: fill.deps,
    });
    if unchanged {
        cache.insert(fill.key, entry.clone(), Instant::now());
    }
    Ok(entry)
}

/// Answer from a cached entry: 304, HEAD or the full response, the same
/// bytes the normal path sends, with the second of this response as their
/// `Date` (never the one the entry was made in).
pub(super) async fn respond_cached(w: &mut Conn<'_>, x: Exchange<'_>, e: &Arc<Entry>) -> std::io::Result<(u16, u64)> {
    let Exchange { req, keep, .. } = x;
    let now = now();
    let not_modified = is_not_modified(req, &e.etag, e.mtime, now.secs);
    if let (Some(mem), false, false) = (&e.file, not_modified, x.head_only()) {
        // The body is in a memfd: the head, then the body by page reference.
        let resp = e.resp.as_of(&now);
        let head: std::borrow::Cow<[u8]> =
            if keep { (&resp[..e.head_len]).into() } else { closing(&resp[..e.head_len], e.conn_at).into() };
        w.send_file(&head, Source::Memfd(mem.clone()), 0, e.body_len).await?;
        w.flush().await?;
        return Ok((200, e.body_len));
    }
    let (out, status, bytes) = cached_bytes(req, e, keep, not_modified, &now);
    w.write_all(out).await?;
    w.flush().await?;
    Ok((status, bytes))
}

/// `bytes` with `close` in place of the `keep-alive` at `conn_at`.
fn closing(bytes: &[u8], conn_at: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes.len());
    v.extend_from_slice(&bytes[..conn_at]);
    v.extend_from_slice(b"close");
    v.extend_from_slice(&bytes[conn_at + KEEP_ALIVE.len()..]);
    v
}

/// The bytes that answer `req` from the cached `e`, and the status and body
/// length they carry: a 304, a HEAD's head, or the whole response, dated
/// `now`, with the Connection value patched for `close` when the connection
/// is not kept. (Not for a GET of an entry whose body is in a memfd: that is
/// sent by sendfile, `respond_cached`.)
pub(super) fn cached_bytes(
    req: &Request<'_>,
    e: &Entry,
    keep: bool,
    not_modified: bool,
    now: &Now,
) -> (OutBuf, u16, u64) {
    let (dated, conn_at, end, status, bytes) = if not_modified {
        (&e.not_modified, e.nm_conn_at, e.not_modified.len(), 304, 0)
    } else if req.method == "HEAD" {
        (&e.resp, e.conn_at, e.head_len, 200, 0)
    } else {
        (&e.resp, e.conn_at, e.resp.len(), 200, e.body_len)
    };
    let buf = dated.as_of(now);
    let out = if keep { OutBuf::Shared(buf, 0..end) } else { OutBuf::Vec(closing(&buf[..end], conn_at)) };
    (out, status, bytes)
}

#[cfg(test)]
mod tests {
    use super::super::head::owned_request as req;
    use super::*;

    #[test]
    fn cache_keys_separate_what_changes_the_response() {
        let mut cfg: Static = serde_json::from_str(r#"{"root": "/srv"}"#).unwrap();
        let plain = cache_key("a.css", &cfg, &req(&[]).req(), false);
        let both = cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "gzip, deflate, br")]).req(), false);
        let gz = cache_key("a.css", &cfg, &req(&[("accept-encoding", "gzip;q=1")]).req(), false);
        let slash = cache_key("a.css", &cfg, &req(&[]).req(), true);
        let all = [&plain, &both, &gz, &slash];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Encodings the server never picks don't split the cache.
        assert_eq!(cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "deflate, zstd")]).req(), false), plain);
        // Without precompressed files, Accept-Encoding doesn't matter.
        cfg.precompressed = false;
        assert_eq!(
            cache_key("a.css", &cfg, &req(&[("Accept-Encoding", "br")]).req(), false),
            cache_key("a.css", &cfg, &req(&[]).req(), false)
        );
        assert_ne!(cache_key("a", &cfg, &req(&[]).req(), false), cache_key("a\u{1}", &cfg, &req(&[]).req(), false));
    }
}

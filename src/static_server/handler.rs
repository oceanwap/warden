//! Answering one request: the checks, the cache, the lookup, and the file.

use super::cache::{Dep, Seen, Stamp};
use super::cached::{self, Cached, Fill, respond_cached};
use super::conn::{Conn, Source};
use super::head::{ENCODINGS, Request, accepts, parse_range};
use super::names::{extension_of, fingerprinted, mime};
use super::open::{Opened, inside, open, open_error_status, open_stored, read_body};
use super::path::{join_rel, path_of, relative_cow};
use super::response::{
    FileHead, HeadParts, Page, file_head_into, head_buffer, is_not_modified, not_modified_head_into, respond,
    respond_error,
};
use super::store::{Entry, Version};
use super::text::{etag, http_date_short};
use super::{Exchange, Site};
use std::path::Path;
use std::time::UNIX_EPOCH;

/// Files up to this size are copied into the response and sent with the
/// headers in one write; larger ones go headers (MSG_MORE) + sendfile.
/// Measured (bench/static.ts, 48 KB file): 55k req/s copied vs 62–68k with
/// sendfile; a 1.5 KB page is the same either way. 16 KB is past the point
/// where the copy costs more than the extra syscall.
const SMALL_FILE: u64 = 16 * 1024;

/// What answering a request leaves for its caller.
#[derive(Default)]
pub(super) struct Scratch {
    /// The cache key of the request, built in place: the connection keeps
    /// the buffer from one request to the next, so a hit allocates nothing
    /// for it.
    pub(super) key: String,
    /// What the access log says about the cache: " cache=hit", " cache=miss"
    /// or nothing.
    pub(super) cache: &'static str,
}

/// Answer `req` on `w`: the status and the number of body bytes.
pub(super) async fn handle(
    req: &Request<'_>,
    site: &Site,
    w: &mut Conn<'_>,
    keep: bool,
    scratch: &mut Scratch,
) -> std::io::Result<(u16, u64)> {
    // Debug builds only: `WARDEN_FAULT=static:3` panics here on the third request (tests).
    crate::guard::fault("static");
    let x = Exchange { site, req, keep };
    if req.method != "GET" && req.method != "HEAD" {
        return respond_error(w, 405, keep, false).await;
    }
    if let Some(expected) = &site.auth {
        if req.header("authorization") != Some(expected.as_str()) {
            let page = Page {
                headers: "WWW-Authenticate: Basic realm=\"warden\"\r\n",
                mime: "text/plain; charset=utf-8",
                body: "401 Unauthorized\n",
            };
            return respond(w, 401, keep, x.head_only(), page).await;
        }
    }
    let rel = match relative_cow(req.path, site.cfg.dotfiles) {
        Ok(p) => p,
        Err(code) => return not_found_or(w, x, code).await,
    };
    let url_path = path_of(req.path);
    let mut fill = match cached::consult(x, &rel, url_path.ends_with('/'), &mut scratch.key).await {
        Cached::Hit(entry) => {
            scratch.cache = " cache=hit";
            return respond_cached(w, x, &entry).await;
        }
        Cached::Miss(fill) => {
            scratch.cache = " cache=miss";
            Some(fill)
        }
        Cached::Skip => None,
    };
    let found = match open(site, &rel).await {
        Ok(o) => o,
        Err(e) => return not_found_or(w, x, open_error_status(&e)).await,
    };
    if found.meta.is_dir() {
        drop(found);
        return serve_dir(w, x, &rel, url_path, fill).await;
    }
    if !found.meta.is_file() {
        return not_found_or(w, x, 404).await;
    }
    if let Some(f) = &mut fill {
        f.deps.push(Dep { path: rel.as_ref().into(), seen: Seen::File(Stamp::of(&found.meta)) });
    }
    send_file(w, x, &rel, found, 200, fill).await
}

/// A directory: the redirect to its URL with a slash, its index file, or a
/// listing (if enabled).
async fn serve_dir(
    w: &mut Conn<'_>,
    x: Exchange<'_>,
    rel: &str,
    url_path: &str,
    mut fill: Option<Fill>,
) -> std::io::Result<(u16, u64)> {
    let site = x.site;
    if !url_path.ends_with('/') {
        let headers = format!("Location: {url_path}/\r\n");
        return respond(w, 301, x.keep, false, Page { headers: &headers, ..Page::default() }).await;
    }
    let index = join_rel(rel, &site.cfg.index);
    match open(site, &index).await {
        Ok(o) if o.meta.is_file() => {
            if let Some(f) = &mut fill {
                f.deps.push(Dep { path: rel.into(), seen: Seen::Dir });
                f.deps.push(Dep { path: index.as_str().into(), seen: Seen::File(Stamp::of(&o.meta)) });
            }
            send_file(w, x, &index, o, 200, fill).await
        }
        _ => {
            let dir = if rel.is_empty() { site.root.clone() } else { site.root.join(rel) };
            if site.cfg.listing && inside(&site.root, &dir) {
                listing(w, &dir, url_path, x).await
            } else {
                not_found_or(w, x, 404).await
            }
        }
    }
}

/// SPA fallback to index.html, then 404.html, then a plain 404.
async fn not_found_or(w: &mut Conn<'_>, x: Exchange<'_>, code: u16) -> std::io::Result<(u16, u64)> {
    let Exchange { site, req, keep } = x;
    if code == 404 {
        let wants_page = req.header("accept").is_none_or(|a| a.contains("text/html") || a.contains("*/*"));
        let last = path_of(req.path).rsplit('/').next().unwrap_or("");
        if site.cfg.spa && wants_page && !last.contains('.') {
            if let Ok(o) = open(site, &site.cfg.index).await {
                if o.meta.is_file() {
                    return send_file(w, x, &site.cfg.index, o, 200, None).await;
                }
            }
        }
        if let Ok(o) = open(site, "404.html").await {
            if o.meta.is_file() {
                return send_file(w, x, "404.html", o, 404, None).await;
            }
        }
    }
    respond_error(w, code, keep, x.head_only()).await
}

async fn listing(w: &mut Conn<'_>, dir: &Path, url_path: &str, x: Exchange<'_>) -> std::io::Result<(u16, u64)> {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
    let mut names: Vec<(bool, String)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().to_string()))
                .filter(|(_, n)| !n.starts_with('.'))
                .collect()
        })
        .unwrap_or_default();
    names.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut body = format!("<!doctype html><meta charset=utf-8><title>{0}</title><h1>{0}</h1><ul>", esc(url_path));
    if url_path != "/" {
        body += "<li><a href=\"../\">../</a>";
    }
    for (is_dir, n) in names {
        let shown = if is_dir { format!("{n}/") } else { n };
        body += &format!("<li><a href=\"{0}\">{0}</a>", esc(&shown));
    }
    body += "</ul>\n";
    let page = Page { headers: "Cache-Control: no-cache\r\n", mime: "text/html; charset=utf-8", body: &body };
    respond(w, 200, x.keep, x.head_only(), page).await
}

/// How long a file with a compressed sibling of its own is left out of the
/// background compression, before that is looked into again.
const LEAVE_FOR_SIBLINGS: u64 = 3600;

/// What a file is sent as, besides itself.
enum Compressed {
    /// A compressed copy: the file and its encoding.
    Copy(Opened, &'static str),
    /// None yet, and a copy is being made: the file goes out as it is this
    /// time, which no cache may keep for later requests.
    Coming,
    /// None, and none is coming.
    No,
}

/// The compressed copy of the file `found` (at `rel`) that the client
/// accepts, `.br` before `.gz`: the one the background compression made of
/// this very version of the file (`store.rs`), else a sibling file next to it
/// (`file.js.br`, made by the site's build). Without either, the file is
/// queued for compression. The lookups a cache entry depends on are noted in
/// `fill`.
async fn compressed(x: Exchange<'_>, rel: &str, found: &Opened, mut fill: Option<&mut Fill>) -> Compressed {
    let site = x.site;
    let accept = x.req.header("accept-encoding").unwrap_or("");
    // A client that takes neither encoding (most requests for a script or a
    // stylesheet come from one that does; an image tag's may not) costs nothing here.
    if !ENCODINGS.iter().any(|(enc, _)| accepts(accept, enc)) {
        return Compressed::No;
    }
    let version = Version::of(&found.meta);
    let compressor = site.compressor.as_ref().filter(|c| found.meta.is_file() && c.wants(version.size()));
    // Not for a version that is known to have no copy, and not coming.
    let compressor_expects = compressor.filter(|c| c.expects(&version));
    if let Some(c) = compressor_expects {
        for (enc, suffix) in ENCODINGS {
            if !accepts(accept, enc) {
                continue;
            }
            let entry = Entry::new(rel, &version, suffix);
            if let Ok(o) = open_stored(site, c.store(), &entry).await {
                if o.meta.is_file() {
                    return Compressed::Copy(o, enc);
                }
            }
        }
    }
    for (enc, suffix) in ENCODINGS {
        if !accepts(accept, enc) {
            continue;
        }
        let sibling = format!("{rel}.{suffix}");
        let got = open(site, &sibling).await;
        if let Some(f) = fill.as_mut() {
            let seen = match &got {
                Ok(o) if o.meta.is_file() => Seen::File(Stamp::of(&o.meta)),
                _ => Seen::NotFile,
            };
            f.deps.push(Dep { path: sibling.into(), seen });
        }
        if let Ok(o) = got {
            if o.meta.is_file() {
                // Made by the site: nothing to make here.
                if let Some(c) = compressor_expects {
                    c.leave(version, LEAVE_FOR_SIBLINGS);
                }
                return Compressed::Copy(o, enc);
            }
        }
    }
    let Some(c) = compressor else { return Compressed::No };
    if !x.head_only() {
        c.submit(rel, version);
    }
    Compressed::Coming
}

/// `rel` is the file's path under the root (its name picks the MIME type
/// and cache policy); `found` is that file, already open. With `fill` (a
/// cache miss), a small 200 response is built once, cached, and sent from
/// the cache entry.
async fn send_file(
    w: &mut Conn<'_>,
    x: Exchange<'_>,
    rel: &str,
    found: Opened,
    status: u16,
    mut fill: Option<Fill>,
) -> std::io::Result<(u16, u64)> {
    let Exchange { site, req, keep } = x;
    let mut ext_buf = [0u8; 12];
    let ext = extension_of(rel, &mut ext_buf);
    let variant = if site.cfg.precompressed
        && status == 200
        && !site.skip_siblings.contains(ext)
        && req.header("range").is_none()
    {
        compressed(x, rel, &found, fill.as_mut()).await
    } else {
        Compressed::No
    };
    let (body, encoding) = match variant {
        Compressed::Copy(copy, enc) => (copy, Some(enc)),
        Compressed::Coming => {
            fill = None;
            (found, None)
        }
        Compressed::No => (found, None),
    };
    let len = body.meta.len();
    let mtime = body.meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
    let etag = etag(len, mtime, encoding);
    let last_modified = http_date_short(mtime);
    let cache_control: &str = if status != 200 {
        "no-cache"
    } else if matches!(ext, "html" | "htm") {
        &site.fixed.cc_html
    } else if fingerprinted(rel) {
        "public, max-age=31536000, immutable"
    } else {
        &site.fixed.cc_plain
    };
    let vary = if site.cfg.precompressed { "Vary: Accept-Encoding\r\n" } else { "" };
    let parts = HeadParts {
        etag: etag.as_str(),
        last_modified: last_modified.as_str(),
        cache_control,
        vary,
        extra: &site.fixed.extra,
    };

    // A small file on a cache miss: build the whole response once, keep it
    // (unless the file changed while it was read, or so recently that a
    // change could go unnoticed), and answer from it.
    if let (Some(fill), Some(cache)) = (fill, &site.cache) {
        if cached::fits(cache, status, &body.meta) {
            let entry = cached::store(cache, fill, &parts, mime(ext), encoding, body, mtime).await?;
            return respond_cached(w, x, &entry).await;
        }
    }

    let conn = if keep { "keep-alive" } else { "close" };
    if status == 200 && is_not_modified(req, parts.etag, mtime) {
        let mut head = head_buffer(&parts, 0);
        not_modified_head_into(&mut head, &parts, conn);
        w.write_all(head).await?;
        w.flush().await?;
        return Ok((304, 0));
    }
    // A single byte range of a file served as it is.
    let mut range = None;
    if status == 200 && encoding.is_none() {
        match req.header("range").and_then(|r| parse_range(r, len)) {
            Some(Ok(r)) => range = Some(r),
            Some(Err(())) => {
                let headers = format!("Content-Range: bytes */{len}\r\n");
                return respond(w, 416, keep, false, Page { headers: &headers, ..Page::default() }).await;
            }
            None => {}
        }
    }
    let (code, start, count) = match range {
        Some((a, b)) => (206, a, b - a + 1),
        None => (status, 0, len),
    };
    // The head, and for a small file its body after it: one allocation, one write.
    let head_only = x.head_only();
    let small = !head_only && count <= SMALL_FILE;
    let mut buf = head_buffer(&parts, if small { count as usize } else { 0 });
    let range = (code == 206).then_some((start, len));
    file_head_into(&mut buf, &parts, &FileHead { code, mime: mime(ext), count, conn, encoding, range });
    if head_only {
        w.write_all(buf).await?;
        w.flush().await?;
        return Ok((code, 0));
    }
    if small {
        let body_start = buf.len();
        buf.resize(body_start + count as usize, 0);
        let (_, buf) = read_body(body.file, buf, body_start, start).await?;
        w.write_all(buf).await?;
        w.flush().await?;
        return Ok((code, count));
    }
    let sent = w.send_file(&buf, Source::File(body.file), start, count).await?;
    w.flush().await?;
    Ok((code, sent))
}

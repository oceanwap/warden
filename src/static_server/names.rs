//! What a file's name says about it: its extension and MIME type, and whether
//! the name carries a content hash (a fingerprint).

/// The last segment of `rel`, a path under the root (no trailing slash, no
/// `.` or `..` segment: `relative` has seen to that).
fn file_name(rel: &str) -> &str {
    rel.rsplit_once('/').map_or(rel, |(_, name)| name)
}

/// The extension of the file `rel`, in lowercase, as `mime` wants it: what
/// follows the last dot of its name (none for `.dotfile`). An extension
/// longer than `buf`, which no known type has, comes back empty.
pub(super) fn extension_of<'a>(rel: &str, buf: &'a mut [u8; 12]) -> &'a str {
    let name = file_name(rel);
    match name.rfind('.') {
        Some(i) if i > 0 && name.len() - i - 1 <= buf.len() => {
            let ext = &name.as_bytes()[i + 1..];
            for (to, from) in buf.iter_mut().zip(ext) {
                *to = from.to_ascii_lowercase();
            }
            std::str::from_utf8(&buf[..ext.len()]).unwrap_or("")
        }
        _ => "",
    }
}

/// `app.3f9a2c1b.js`, `index-DkS8xW2q.css`: a content hash in the name.
pub(super) fn fingerprinted(rel: &str) -> bool {
    file_name(rel).split(['.', '-', '_']).any(|part| {
        let b = part.as_bytes();
        b.len() >= 8
            && b.iter().all(u8::is_ascii_alphanumeric)
            && b.iter().any(u8::is_ascii_digit)
            && b.iter().any(u8::is_ascii_alphabetic)
    })
}

pub(super) fn mime(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "txt" | "md" => "text/plain; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        "pdf" => "application/pdf",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "zip" => "application/zip",
        "gz" => "application/gzip",
        "yaml" | "yml" => "application/yaml",
        _ => "application/octet-stream",
    }
}

/// A set of file extensions (lowercase, at most 12 bytes: what `extension_of`
/// gives) that a request tests its file against, `precompressed_skip`. Each
/// is packed into 16 bytes and the set is sorted, so a lookup costs a copy and
/// a few comparisons, whatever the list holds, and allocates nothing.
pub(super) struct ExtSet(Vec<u128>);

fn pack(ext: &str) -> Option<u128> {
    let bytes = ext.as_bytes();
    if bytes.is_empty() || bytes.len() > 12 {
        return None;
    }
    let mut packed = [0u8; 16];
    packed[..bytes.len()].copy_from_slice(bytes);
    Some(u128::from_le_bytes(packed))
}

impl ExtSet {
    /// Entries that are not extensions are left out.
    pub(super) fn new<S: AsRef<str>>(list: &[S]) -> ExtSet {
        let mut packed: Vec<u128> = list.iter().filter_map(|e| pack(e.as_ref())).collect();
        packed.sort_unstable();
        packed.dedup();
        ExtSet(packed)
    }

    pub(super) fn contains(&self, ext: &str) -> bool {
        pack(ext).is_some_and(|p| self.0.binary_search(&p).is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn types_and_fingerprints() {
        assert!(fingerprinted("app.3f9a2c1b.js"));
        assert!(fingerprinted("index-DkS8xW2q.css"));
        assert!(!fingerprinted("favicon.ico"));
        assert!(!fingerprinted("background.png"));
        assert_eq!(mime("woff2"), "font/woff2");
    }

    #[test]
    fn a_set_of_extensions_finds_exactly_its_members() {
        let set = ExtSet::new(&["png", "pdf", "woff2", "tar-gz", "png", "", "waytoolongextension"]);
        for ext in ["png", "pdf", "woff2", "tar-gz"] {
            assert!(set.contains(ext), "{ext}");
        }
        // Not members: other names, prefixes and extensions of members, nothing, too long.
        for ext in ["js", "pn", "pngg", "pd", "woff", "", "waytoolongextension", "PNG"] {
            assert!(!set.contains(ext), "{ext:?}");
        }
        assert!(!ExtSet::new::<&str>(&[]).contains("png"));
        // Whatever the default list holds is found, and only that.
        let default = ExtSet::new(crate::config::PRECOMPRESSED_SKIP);
        assert!(crate::config::PRECOMPRESSED_SKIP.iter().all(|e| default.contains(e)));
        assert!(!default.contains("css") && !default.contains("svg"));
    }

    #[test]
    fn extensions_and_fingerprints_agree_with_what_they_replaced() {
        // Extensions and fingerprints, over names with every kind of dot.
        let old_ext = |rel: &str| {
            Path::new(rel).extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
        };
        let old_fp = |rel: &str| {
            let name = Path::new(rel).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            name.split(['.', '-', '_']).any(|part| {
                part.len() >= 8
                    && part.chars().all(|c| c.is_ascii_alphanumeric())
                    && part.chars().any(|c| c.is_ascii_digit())
                    && part.chars().any(|c| c.is_ascii_alphabetic())
            })
        };
        for rel in [
            "a.js",
            "A.JS",
            "dir/b.Woff2",
            "x/.hidden",
            ".hidden",
            "x/.hidden.css",
            "noext",
            "trailing.",
            "a.b.c.d",
            "dir.v2/file",
            "dir.v2/file.webmanifest",
            "dir/file.averyveryverylongextension",
            "ü.png",
            "dir/ünï.PÑG",
            "app.3f9a2c1b.js",
            "index-DkS8xW2q.css",
            "favicon.ico",
            "abcdefgh.js",
            "12345678.js",
            "a_1b2c3d4e5.map",
            "x/y/vendor-0a1b2c3d4e.min.js",
        ] {
            let mut buf = [0u8; 12];
            let got = extension_of(rel, &mut buf);
            // Extensions too long for any known type are not looked at.
            if old_ext(rel).len() <= 12 {
                assert_eq!(got, old_ext(rel), "extension of {rel:?}");
            } else {
                assert_eq!(mime(got), mime(&old_ext(rel)));
            }
            assert_eq!(fingerprinted(rel), old_fp(rel), "fingerprint of {rel:?}");
        }
    }
}

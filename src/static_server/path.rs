//! From the target of a request to a path under the root.

use std::borrow::Cow;
use std::path::{Component, Path};

/// URL path → a path relative to the root ("" for the root itself), or
/// why not. Rejects `..`, NUL, and (unless enabled) dotfiles.
pub(super) fn relative(url_path: &str, dotfiles: bool) -> Result<String, u16> {
    let path = url_path.split(['?', '#']).next().unwrap_or("/");
    let decoded = percent_decode(path).ok_or(400u16)?;
    if decoded.contains('\0') || !decoded.starts_with('/') {
        return Err(400);
    }
    let mut out = String::with_capacity(decoded.len());
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => return Err(403),
            s if s.starts_with('.') && !dotfiles && s != ".well-known" => return Err(404),
            s => {
                if Path::new(s).components().any(|c| !matches!(c, Component::Normal(_))) {
                    return Err(400);
                }
                if !out.is_empty() {
                    out.push('/');
                }
                out.push_str(s);
            }
        }
    }
    Ok(out)
}

/// `relative`, without allocating for the paths nearly every request has:
/// no percent-escapes, no NUL, no empty, `.`, `..` or dot-leading segment
/// (a trailing slash is fine). Those come back as a slice of `url_path`;
/// anything else takes the full `relative` and its answer, so the two always
/// agree (a test compares them on thousands of paths).
pub(super) fn relative_cow(url_path: &str, dotfiles: bool) -> Result<Cow<'_, str>, u16> {
    if let Some(rest) = path_of(url_path).strip_prefix('/') {
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        // One pass: every segment non-empty, none starting with a dot, no `%`, no NUL.
        let mut segment_start = true;
        let mut plain = true;
        for &c in rest.as_bytes() {
            match c {
                b'/' if segment_start => plain = false,
                b'/' => segment_start = true,
                b'%' | 0 => plain = false,
                b'.' if segment_start => plain = false,
                _ => segment_start = false,
            }
            if !plain {
                break;
            }
        }
        // A trailing empty segment (`a//`) is not plain either.
        if plain && (!segment_start || rest.is_empty()) {
            return Ok(Cow::Borrowed(rest));
        }
    }
    relative(url_path, dotfiles).map(Cow::Owned)
}

/// A request target without its query or fragment.
pub(super) fn path_of(target: &str) -> &str {
    match target.bytes().position(|c| c == b'?' || c == b'#') {
        Some(i) => &target[..i],
        None => target,
    }
}

pub(super) fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_root() {
        assert_eq!(relative("/a/b.js?v=1", false), Ok("a/b.js".into()));
        assert_eq!(relative("/a%20b.txt", false), Ok("a b.txt".into()));
        assert_eq!(relative("/", false), Ok("".into()));
        assert_eq!(relative("//a//./b/", false), Ok("a/b".into()));
        assert_eq!(relative("/../etc/passwd", false), Err(403));
        assert_eq!(relative("/a/%2e%2e/%2e%2e/etc/passwd", false), Err(403));
        assert_eq!(relative("/.env", false), Err(404));
        assert!(relative("/.env", true).is_ok());
        assert!(relative("/.well-known/security.txt", false).is_ok());
        assert_eq!(relative("/a%00b", false), Err(400));
        assert_eq!(relative("/%zz", false), Err(400));
        assert_eq!(relative("noslash", false), Err(400));
        assert_eq!(join_rel("", "index.html"), "index.html");
        assert_eq!(join_rel("docs", "index.html"), "docs/index.html");
    }

    /// The allocation-free path normalizer gives the same answer as the full
    /// one, on every combination of the pieces that make paths interesting.
    #[test]
    fn relative_cow_agrees_with_relative() {
        let pieces = [
            "/",
            "/",
            "a",
            "b.js",
            ".",
            "..",
            ".x",
            ".well-known",
            "%2e",
            "%2E%2e",
            "%2f",
            "%00",
            "%",
            "%zz",
            "?",
            "#",
            "x y",
            "é",
        ];
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut checked = 0;
        for _ in 0..20_000 {
            let mut p = String::new();
            for _ in 0..(next() % 7) {
                p += pieces[(next() % pieces.len() as u64) as usize];
            }
            for dotfiles in [false, true] {
                assert_eq!(
                    relative_cow(&p, dotfiles).map(|c| c.into_owned()),
                    relative(&p, dotfiles),
                    "{p:?} dotfiles={dotfiles}"
                );
                checked += 1;
            }
        }
        for p in ["/", "/a", "/a/", "/a/b.js", "/a/b.js?x=1#y", "", "a", "/%41", "//a", "/a//b", "/a/./b", "/.git/x"] {
            for dotfiles in [false, true] {
                assert_eq!(relative_cow(p, dotfiles).map(|c| c.into_owned()), relative(p, dotfiles), "{p:?}");
            }
        }
        assert!(checked > 0);
        // The ordinary paths borrow.
        assert!(matches!(relative_cow("/assets/app.3f9a.js?v=2", false), Ok(Cow::Borrowed("assets/app.3f9a.js"))));
        assert!(matches!(relative_cow("/", false), Ok(Cow::Borrowed(""))));
        assert!(matches!(relative_cow("/docs/", false), Ok(Cow::Borrowed("docs"))));
    }
}

//! Which warden executable this process started from.
//!
//! A supervisor lives for weeks and keeps running the code it started with:
//! `cargo build`, `cargo install` and package upgrades replace the file, not
//! the running process. So the numbers a newer warden knows (cpu and memory
//! on macOS, ports, the user) are `-` in an older supervisor's status and a
//! fix is not in it. The supervisor reports the file it started from and
//! whether that file has been replaced since; `warden list` and the window
//! say so and what to run.

use std::path::Path;
use std::sync::OnceLock;
use std::time::UNIX_EPOCH;
use warden_protocol::control::Build;

/// `size:modified-seconds` of the file at `path`, None if it is not there.
pub fn stamp(path: &Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    let secs = m.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}:{secs}", m.len()))
}

/// This process's executable as it was when first asked (call it early: at
/// start), or None when the OS does not say where it is.
fn started_from() -> &'static Option<(String, String)> {
    static OWN: OnceLock<Option<(String, String)>> = OnceLock::new();
    OWN.get_or_init(|| {
        let path = std::env::current_exe().ok()?;
        // Linux says "<path> (deleted)" for a file replaced under a running process.
        let text = path.display().to_string();
        let text = text.strip_suffix(" (deleted)").unwrap_or(&text).to_string();
        let stamp = stamp(Path::new(&text))?;
        Some((text, stamp))
    })
}

/// Remember the executable now (the supervisor calls this when it starts).
pub fn remember() {
    let _ = started_from();
}

/// What a supervisor reports: where it started from, and whether that file
/// has changed since (one `stat`).
pub fn build() -> Option<Build> {
    let (path, was) = started_from().as_ref()?;
    // A file that is gone cannot be compared: not "replaced".
    let replaced = stamp(Path::new(path)).is_some_and(|now| now != *was);
    Some(Build { path: path.clone(), stamp: was.clone(), replaced })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_changes_with_the_file() {
        let dir = std::env::temp_dir().join(format!("warden-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("warden");
        assert_eq!(stamp(&f), None);
        std::fs::write(&f, b"one").unwrap();
        let a = stamp(&f).unwrap();
        assert!(a.starts_with("3:"), "{a}");
        std::fs::write(&f, b"three").unwrap();
        assert_ne!(stamp(&f).unwrap(), a);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn this_binary_is_not_replaced_while_it_runs() {
        remember();
        let b = build().expect("a test binary has a path");
        assert!(!b.replaced && b.stamp.contains(':') && std::path::Path::new(&b.path).exists(), "{b:?}");
    }
}

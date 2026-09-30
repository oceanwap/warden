//! Where the sockets live (docs/protocol.md): the runtime directory holds
//! every app's directory (`<app>/control.sock`) and wardend's socket.
//!
//! This crate makes no system calls, so the caller passes its user ids
//! (`geteuid`, `getuid`).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// wardend's socket, inside the runtime directory.
pub const WARDEND_SOCKET: &str = "wardend.sock";

/// The runtime directory of root on this OS (a system-wide wardend).
pub const ROOT_RUNTIME_DIR: &str = if cfg!(target_os = "linux") { "/run/warden" } else { "/var/run/warden" };

/// The runtime directory: `$WARDEN_RUNTIME_DIR`, else `/run/warden` for root
/// (`/var/run/warden` on macOS), else `$XDG_RUNTIME_DIR/warden`, else
/// `/tmp/warden-<uid>`. `euid`, `uid`: this process's effective and real
/// user ids.
pub fn runtime_dir(euid: u32, uid: u32) -> PathBuf {
    runtime_dir_with(|k| std::env::var_os(k), euid, uid)
}

/// `runtime_dir` with the environment given as a lookup function (tests).
pub fn runtime_dir_with(env: impl Fn(&str) -> Option<OsString>, euid: u32, uid: u32) -> PathBuf {
    if let Some(d) = env("WARDEN_RUNTIME_DIR") {
        return PathBuf::from(d);
    }
    if euid == 0 {
        // macOS has no /run (and a read-only /): /var/run is its place for this.
        return PathBuf::from(ROOT_RUNTIME_DIR);
    }
    if let Some(d) = env("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("warden");
    }
    #[cfg(target_os = "linux")]
    let tmp = std::env::temp_dir();
    // Not Linux: macOS's per-user $TMPDIR (/var/folders/…/T/) leaves too
    // little of the ~104-byte socket path for the workers' health sockets.
    #[cfg(not(target_os = "linux"))]
    let tmp = PathBuf::from("/tmp");
    tmp.join(format!("warden-{uid}"))
}

/// wardend's socket in the runtime directory `runtime_dir`.
pub fn wardend_socket(runtime_dir: &Path) -> PathBuf {
    runtime_dir.join(WARDEND_SOCKET)
}

/// wardend's socket when it runs as root (`sudo warden startup`): the default
/// for a remote host.
pub fn root_wardend_socket() -> PathBuf {
    wardend_socket(Path::new(ROOT_RUNTIME_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |k| vars.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v))
    }

    #[test]
    fn runtime_dir_follows_the_documented_order() {
        let all = [("WARDEN_RUNTIME_DIR", "/w"), ("XDG_RUNTIME_DIR", "/run/user/1000")];
        assert_eq!(runtime_dir_with(env(&all), 1000, 1000), PathBuf::from("/w"));
        assert_eq!(runtime_dir_with(env(&all), 0, 0), PathBuf::from("/w"));
        let xdg = [("XDG_RUNTIME_DIR", "/run/user/1000")];
        assert_eq!(runtime_dir_with(env(&xdg), 0, 1000), PathBuf::from(ROOT_RUNTIME_DIR));
        assert_eq!(runtime_dir_with(env(&xdg), 1000, 1000), PathBuf::from("/run/user/1000/warden"));
        let none = runtime_dir_with(env(&[]), 1000, 1000);
        assert!(none.ends_with("warden-1000"), "{}", none.display());
        assert_eq!(wardend_socket(Path::new("/w")), PathBuf::from("/w/wardend.sock"));
        assert!(root_wardend_socket().ends_with("warden/wardend.sock"));
    }
}

//! New releases: the window asks the machine's warden (`warden upgrade --check --json`) when it
//! connects and every few hours, says so in a banner and once per release in a desktop
//! notification, and its Update button runs `warden upgrade --yes`: the release installed, every
//! supervisor and wardend restarted onto it. Then the window starts again on the new version.

use crate::commands::{self, Host};
use std::time::Duration;

/// How often the window asks again.
pub const EVERY: Duration = Duration::from_secs(6 * 3600);

/// What `warden upgrade --check --json` says.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Check {
    pub current: String,
    pub latest: String,
    pub update: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Release {
    /// Not asked yet (on this host).
    #[default]
    Unknown,
    Checking,
    UpToDate(String),
    Available(Check),
    /// `warden upgrade` runs; seconds so far.
    Updating {
        to: String,
        secs: u64,
    },
    /// The last check or update failed: what was said.
    Failed(String),
}

impl Release {
    /// A newer release this window can install.
    pub fn available(&self) -> Option<&Check> {
        match self {
            Release::Available(c) => Some(c),
            _ => None,
        }
    }
}

/// Ask the host's warden whether a newer release exists.
pub async fn check(host: &Host) -> Result<Check, String> {
    let args = ["upgrade".to_string(), "--check".into(), "--json".into()];
    let out = commands::run_warden(host, &args, None, &[], Duration::from_secs(60)).await?;
    if !out.ok {
        return Err(out.text());
    }
    parse(&out.stdout).ok_or_else(|| format!("`{}` said: {}", out.command, out.text()))
}

fn parse(stdout: &str) -> Option<Check> {
    stdout.lines().rev().find_map(|l| serde_json::from_str(l.trim()).ok())
}

/// A desktop notification (Notification Center on macOS, notify-send elsewhere); best effort.
pub fn notify(title: &str, body: &str) {
    let mut cmd = if cfg!(target_os = "macos") {
        let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let mut c = std::process::Command::new("/usr/bin/osascript");
        c.arg("-e").arg(format!("display notification \"{}\" with title \"{}\"", q(body), q(title)));
        c
    } else {
        let mut c = std::process::Command::new("notify-send");
        c.args(["--app-name=Warden", title, body]);
        c
    };
    let spawned = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
    }
}

/// What this window's program is on disk: its file's device, inode, size and modification
/// time. An update replaces the file, so another stamp than at the start means this window
/// runs the old version. `None`: the file cannot be read (it is being replaced).
pub type Stamp = (u64, u64, u64, i64);

pub fn exe_stamp() -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(std::env::current_exe().ok()?).ok()?;
    Some((m.dev(), m.ino(), m.size(), m.mtime()))
}

/// What a window does about an update made behind it (`warden upgrade` or the installer in a
/// terminal): nothing while its program is the one it started from, else it starts again,
/// unless the user is in the middle of something (`idle` is false): then it only says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    Nothing,
    Relaunch,
    Tell,
}

pub fn follow(start: Option<Stamp>, now: Option<Stamp>, idle: bool) -> Follow {
    match (start, now) {
        (Some(a), Some(b)) if a != b && idle => Follow::Relaunch,
        (Some(a), Some(b)) if a != b => Follow::Tell,
        _ => Follow::Nothing,
    }
}

/// Start this window again, on the program now on disk (the upgrade replaced it): the app
/// (`open -n`) when this is the one inside Warden.app, else this program. The caller exits.
pub fn relaunch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    // A second later, once this window has gone (one Warden window at a time).
    let (script, target) = match app_of(&exe) {
        Some(app) => ("sleep 1; exec open -n \"$0\"", app.to_path_buf()),
        None => ("sleep 1; exec \"$0\"", exe.clone()),
    };
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .arg(&target)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("cannot start {} again: {e}", target.display()))
}

/// `…/Warden.app` when `exe` is `…/Warden.app/Contents/MacOS/<program>`.
fn app_of(exe: &std::path::Path) -> Option<&std::path::Path> {
    let app = exe.parent()?.parent()?.parent()?;
    app.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")).then_some(app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn a_window_follows_an_update_made_behind_it_when_nothing_is_open() {
        let (a, b) = (Some((1, 10, 500, 1000)), Some((1, 11, 520, 2000)));
        assert_eq!(follow(a, a, true), Follow::Nothing, "the program it started from");
        assert_eq!(follow(a, b, true), Follow::Relaunch);
        assert_eq!(follow(a, b, false), Follow::Tell, "a form is open: it is not closed under the user");
        assert_eq!(follow(a, None, true), Follow::Nothing, "being replaced right now: not yet");
        assert_eq!(follow(None, b, true), Follow::Nothing);
        assert!(exe_stamp().is_some() && exe_stamp() == exe_stamp());
    }

    #[test]
    fn the_check_is_read_from_the_last_json_line() {
        let c = parse("{\"current\":\"0.1.1\",\"latest\":\"0.2.0\",\"update\":true}\n").unwrap();
        assert_eq!(c, Check { current: "0.1.1".into(), latest: "0.2.0".into(), update: true });
        assert!(
            parse("note\n{\"current\":\"0.2.0\",\"latest\":\"0.2.0\",\"update\":false}").is_some_and(|c| !c.update)
        );
        assert_eq!(parse("warden: unknown command \"upgrade\""), None);
    }

    #[test]
    fn the_app_is_relaunched_when_this_is_its_program() {
        assert_eq!(
            app_of(Path::new("/Applications/Warden.app/Contents/MacOS/warden-gui")),
            Some(Path::new("/Applications/Warden.app"))
        );
        assert_eq!(app_of(Path::new("/home/me/.local/bin/warden-gui")), None);
    }
}

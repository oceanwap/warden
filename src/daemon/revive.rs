//! Supervisors bring wardend back when it dies. wardend restarts supervisors that die, and
//! each supervisor watches wardend in return, so neither is a single point of failure and no
//! service manager is needed for it (with one, launchd or systemd restarts it and this stays
//! out of the way).
//!
//! What counts as dying is told by the socket. A clean exit (SIGTERM, SIGINT, the `shutdown`
//! request `warden kill` sends) removes `<runtime dir>/wardend.sock` just before wardend
//! exits. A crash, a panic, `kill -9` or the OOM killer leaves it behind with nobody
//! listening, and that is what a supervisor looks for: a socket that refuses connections. No
//! socket means wardend never ran or was stopped on purpose, and it stays stopped until the
//! next `warden start` or `warden resurrect`.
//!
//! Best effort and never on the apps' path: a supervisor that cannot start wardend logs it and
//! carries on, and an app never waits for any of this.

use super::{client, log_path, socket_path};
use crate::fleet;
use std::path::Path;
use std::time::Duration;

/// How often a supervisor looks (`WARDEN_REVIVE_EVERY_MS` replaces it in tests).
const EVERY: Duration = Duration::from_secs(5);
/// The longest wait between two attempts when wardend keeps failing to start.
const LONGEST_BACKOFF: Duration = Duration::from_secs(300);
/// How long a new wardend gets to answer.
const COMES_UP_WITHIN: Duration = Duration::from_secs(10);

/// What the socket says about wardend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// It answers, or is too busy to say, or is not ours to ask.
    Alive,
    /// No socket: never started, or stopped on purpose (a clean exit removes it).
    Stopped,
    /// A socket nobody listens on: wardend was killed or crashed.
    Died,
}

pub(crate) async fn probe(path: &Path) -> Probe {
    use std::os::unix::fs::FileTypeExt;
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() => {}
        _ => return Probe::Stopped,
    }
    match tokio::time::timeout(Duration::from_secs(1), tokio::net::UnixStream::connect(path)).await {
        Ok(Ok(_)) => Probe::Alive,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::ConnectionRefused => Probe::Died,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => Probe::Stopped,
        // Permission denied (another user's wardend), a full backlog, a timeout: not ours to judge.
        _ => Probe::Alive,
    }
}

fn every() -> Duration {
    std::env::var("WARDEN_REVIVE_EVERY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(EVERY, Duration::from_millis)
}

/// Should this supervisor watch wardend at all? Not with `WARDEN_NO_DAEMON=1`, and not when
/// launchd or systemd runs wardend: they restart it, and a second one started by hand would
/// only fight theirs for the socket.
fn watching() -> bool {
    !client::disabled() && !crate::startup::wardend_managed()
}

/// Every few seconds: if wardend died, start it again. Runs for the supervisor's whole life.
pub(crate) async fn watch() {
    if !watching() {
        return;
    }
    let every = every();
    let mut wait = every;
    let mut failures = 0u32;
    loop {
        // Offset by pid, so the supervisors of a host do not all look at the same moment.
        tokio::time::sleep(wait + Duration::from_millis(u64::from(std::process::id() % 500))).await;
        if probe(&socket_path()).await != Probe::Died {
            failures = 0;
            wait = every;
            continue;
        }
        match revive().await {
            Revived::Started | Revived::SomeoneElse => {
                failures = 0;
                wait = every;
            }
            Revived::Failed => {
                // Not in a tight loop: it may fail every time (a full disk, a bad binary).
                failures += 1;
                wait = (every * 2u32.saturating_pow(failures.min(8))).min(LONGEST_BACKOFF);
            }
        }
    }
}

enum Revived {
    Started,
    /// Another supervisor got there first.
    SomeoneElse,
    Failed,
}

async fn revive() -> Revived {
    let path = socket_path();
    // The supervisors of a host see it die in the same second: let one of them go first, then
    // look again. (wardend's own lock stops a second one if two still start together.)
    tokio::time::sleep(Duration::from_millis(u64::from(std::process::id() % 700))).await;
    if probe(&path).await != Probe::Died {
        return Revived::SomeoneElse;
    }
    crate::warn!(
        "wardend died without a clean exit; starting it again",
        socket = path.display(),
        log = log_path().display(),
        hint = "`warden kill` stops it for good, WARDEN_NO_DAEMON=1 keeps it from starting; its log says why it died",
    );
    let mut child = match fleet::spawn_daemon(false) {
        Ok(c) => c,
        Err(e) => {
            crate::error!(
                "could not start wardend again; the apps are not affected",
                error = e,
                hint = "`warden resurrect` tries again, and shows the error",
            );
            return Revived::Failed;
        }
    };
    let pid = child.id();
    let started = tokio::time::Instant::now();
    let mut answered = None;
    while started.elapsed() < COMES_UP_WITHIN {
        if let Ok(Some(status)) = child.try_wait() {
            // Lost the race to another one, or failed: the answer tells which.
            if client::hello_pid(&path).await.is_some() {
                return Revived::SomeoneElse;
            }
            crate::error!(
                "wardend exited right after it was started again; the apps are not affected",
                exit = status,
                log = log_path().display(),
                hint = "the last lines of its log say why",
            );
            return Revived::Failed;
        }
        if let Some(p) = client::hello_pid(&path).await {
            answered = Some(p);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let Some(running) = answered else {
        crate::error!(
            "wardend was started again but does not answer",
            pid = pid,
            log = log_path().display(),
            hint = "see its log; the apps are not affected",
        );
        // Keep the child (it may just be slow): reaped when it exits.
        reap_later(child);
        return Revived::Failed;
    };
    crate::info!("wardend is back", pid = running, hint = "it started again after dying without a clean exit");
    // The supervisor is its parent: wait for it on a thread, so that if it dies again it does
    // not stay a zombie until this supervisor exits.
    reap_later(child);
    Revived::Started
}

fn reap_later(mut child: std::process::Child) {
    let _ = std::thread::Builder::new().name("wardend-reaper".into()).spawn(move || {
        let _ = child.wait();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn dir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("warden-revive-{}-{}", std::process::id(), crate::events::now_ms()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn no_socket_means_stopped_on_purpose_or_never_started() {
        let d = dir();
        assert_eq!(probe(&d.join("wardend.sock")).await, Probe::Stopped);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn a_socket_nobody_listens_on_means_wardend_died() {
        let d = dir();
        let sock = d.join("wardend.sock");
        let l = UnixListener::bind(&sock).unwrap();
        assert_eq!(probe(&sock).await, Probe::Alive, "listening");
        // A killed process leaves its socket file behind; closing the listener without
        // unlinking is the same thing.
        drop(l);
        assert!(sock.exists());
        assert_eq!(probe(&sock).await, Probe::Died);
        // A clean exit removes it.
        std::fs::remove_file(&sock).unwrap();
        assert_eq!(probe(&sock).await, Probe::Stopped);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn something_that_is_not_a_socket_is_left_alone() {
        let d = dir();
        let f = d.join("wardend.sock");
        std::fs::write(&f, "x").unwrap();
        assert_eq!(probe(&f).await, Probe::Stopped);
        std::fs::remove_dir_all(&d).unwrap();
    }
}

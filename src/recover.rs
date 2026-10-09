//! Workers that outlive every Warden process of their app, taken back by the
//! next supervisor instead of being stopped and replaced.
//!
//! Under a keeper (`crate::keeper`) workers outlive their supervisor, and the
//! keeper hands them to the supervisor it restarts. When the keeper dies too
//! (both killed at once, an OOM kill of the whole group's parent, wardend and
//! all of them going down together), nothing holds the workers' channels any
//! more. So on Linux, for a worker under the shim:
//!
//! - the worker holds a copy of Warden's ends of its channels itself
//!   (`sys::SPARE_FD`): its output pipes never lose their reader (no EPIPE
//!   for a `console.log`, which kills a Node app), and fd 3 stays open;
//! - the shim sees the supervisor and the keeper gone (`WARDEN_RECOVER` names
//!   them) and starts `warden recover-worker` with those descriptors: it
//!   starts wardend again if wardend died too, and waits for the app's next
//!   supervisor (wardend restarts it), whose `<app>.recover.sock` it hands
//!   the descriptors to;
//! - that supervisor, at its start, looks at the record the dead one left
//!   (`platform::orphans`, which keeps what the supervisor knew of each
//!   worker) and waits a few seconds for those workers to hand themselves
//!   back. Each one that does is supervised again exactly as after a crash
//!   of the supervisor alone: its logs, health checks, restarts and
//!   `warden stop`. The rest are stopped and replaced, as before.
//!
//! wardend, when it starts, restarts the apps whose workers are running with
//! no supervisor and no keeper (`daemon`), so this needs nothing but one
//! Warden process to come back.

use crate::keeper::{FdKind, Meta, client::KeptWorker};
use crate::platform::orphans;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The worker's variable: a JSON [`Spec`].
pub const ENV: &str = "WARDEN_RECOVER";

/// How long a starting supervisor waits for orphaned workers to hand
/// themselves back. The shim looks every second.
const WAIT: Duration = Duration::from_secs(5);

/// How long `warden recover-worker` waits for the app's next supervisor.
const HELPER_WAIT: Duration = Duration::from_secs(60);

/// The descriptors a worker holds at `sys::SPARE_FD` on, in order, and that
/// `recover-worker` has at fd 3 on.
const KINDS: [FdKind; crate::sys::SPARES] = [FdKind::Ipc, FdKind::Handoff, FdKind::Out, FdKind::Err];

/// What the shim needs to hand its worker back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    /// The `warden` binary.
    pub exe: PathBuf,
    /// Where the next supervisor of the app takes workers back.
    pub socket: PathBuf,
    /// The processes of Warden's that hold the worker now (its supervisor,
    /// its keeper): while one of them runs, there is nothing to do.
    pub pids: Vec<u32>,
}

/// The app's hand-back socket: `<runtime dir>/<app>.recover.sock`. `None` when
/// the path is too long for a Unix socket (108 bytes with the final NUL on
/// Linux).
pub fn socket_path(runtime_dir: &Path, app: &str) -> Option<PathBuf> {
    let p = runtime_dir.join(format!("{app}.recover.sock"));
    (p.as_os_str().len() < 108).then_some(p)
}

/// What `recover-worker` sends, with the descriptors attached.
#[derive(Debug, Serialize, Deserialize)]
struct Hello {
    pid: u32,
    kinds: Vec<FdKind>,
}

/// The supervisor's answer: the processes that hold the worker from now on.
#[derive(Debug, Serialize, Deserialize)]
struct Answer {
    #[serde(default)]
    pids: Vec<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    refused: Option<String>,
}

/// A worker of a dead supervisor and keeper that may hand itself back.
struct Expected {
    start: u64,
    meta: Meta,
}

/// The workers the records of `app` name that are running with neither their
/// supervisor nor their keeper, and can hand themselves back. Not `taken`
/// (the keeper handed them over already), nor those of a record of our own
/// keeper (it restarted us, and handed over what it had).
fn expected(dir: &Path, app: &str, taken: &[u32]) -> BTreeMap<u32, Expected> {
    let me = std::process::id();
    let boot = crate::platform::boot_id();
    let keeper = crate::keeper::client::keeper_pid();
    let cx = orphans::Ctx { procs: &orphans::Os, boot: boot.as_deref(), me, keeper };
    let mut out = BTreeMap::new();
    for found in orphans::read_all(dir, Some(app)) {
        let Ok(rec) = found.record else { continue };
        if rec.keeper.as_ref().is_some_and(|k| Some(k.pid) == keeper) {
            continue;
        }
        let plan = orphans::plan(&rec, &cx);
        for o in plan.orphans {
            if taken.contains(&o.member.pid) {
                continue;
            }
            // Only a serving worker comes back: one that was draining has
            // stopped looking, and the rest would be stopped anyway.
            let serving = |m: &&Meta| m.recoverable && m.role == "current" && m.ready && !m.stopping;
            if let Some(meta) = rec.meta.get(&o.member.pid).filter(serving) {
                out.insert(o.member.pid, Expected { start: o.member.start, meta: meta.clone() });
            }
        }
    }
    out
}

/// At a supervisor's start: take back the workers a dead supervisor and
/// keeper of the app left running, as far as they hand themselves back
/// within a few seconds. They come as the keeper would hand them over.
pub async fn take_back(app: &str, runtime_dir: &Path, taken: &[u32]) -> Vec<KeptWorker> {
    if !cfg!(target_os = "linux") || !crate::keeper::client::active() {
        return Vec::new();
    }
    let dir = orphans::dir(&crate::fleet::state_dir());
    let mut want = expected(&dir, app, taken);
    if want.is_empty() {
        return Vec::new();
    }
    let Some(path) = socket_path(runtime_dir, app) else { return Vec::new() };
    let listener = match bind(&path) {
        Ok(l) => l,
        Err(e) => {
            crate::warn!(
                "cannot take back the workers left running by the previous Warden of this app; they are replaced",
                socket = path.display(),
                error = e,
                hint = "is the runtime directory writable?",
            );
            return Vec::new();
        }
    };
    crate::info!(
        "workers of this app are running with no Warden process; waiting for them to hand themselves back",
        workers = want.len(),
        hint = "the previous supervisor and its keeper died together; workers that do not answer within a few \
                seconds are stopped and replaced",
    );
    let mut got: Vec<KeptWorker> = Vec::new();
    let answer =
        Answer { pids: vec![std::process::id(), crate::keeper::client::keeper_pid().unwrap_or(0)], refused: None };
    let until = tokio::time::Instant::now() + WAIT;
    let mut look = tokio::time::interval(Duration::from_millis(100));
    while !want.is_empty() {
        let accepted = tokio::select! {
            a = listener.accept() => a,
            _ = tokio::time::sleep_until(until) => break,
            _ = look.tick() => {
                // Gone meanwhile (a worker that was told to stop): not waited for.
                want.retain(|pid, e| crate::platform::proc_identity(*pid).is_some_and(|id| id.start == e.start));
                continue;
            }
        };
        let Ok((stream, _)) = accepted else { continue };
        match receive(&stream, &want).await {
            Ok((pid, fds)) => {
                let Some(e) = want.remove(&pid) else { continue };
                let _ = reply(&stream, &answer).await;
                got.push(KeptWorker {
                    pid,
                    start: Some(e.start),
                    age: age(e.start),
                    meta: e.meta,
                    fds,
                    logs: Vec::new(),
                    dropped: 0,
                    exited: None,
                });
            }
            Err(why) => {
                crate::debug!("refused a worker's hand-back", reason = why);
                let _ = reply(&stream, &Answer { pids: Vec::new(), refused: Some(why) }).await;
            }
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(&path);
    if !want.is_empty() {
        crate::warn!(
            "some workers left running by the previous Warden did not hand themselves back; they are stopped and \
             replaced",
            workers = want.keys().map(u32::to_string).collect::<Vec<_>>().join(", "),
            hint = "a worker that is not under the shim, or busy for seconds on end, cannot answer",
        );
    }
    got
}

/// How long ago a process started, from its start time (Linux: clock ticks
/// after boot); zero when unknown.
fn age(start: u64) -> Duration {
    let up = std::fs::read_to_string("/proc/uptime").ok();
    let up: Option<f64> = up.as_deref().and_then(|u| u.split_whitespace().next()?.parse().ok());
    let ticks = crate::sys::clock_ticks().max(1) as f64;
    up.map_or(Duration::ZERO, |up| Duration::from_secs_f64((up - start as f64 / ticks).max(0.0)))
}

fn bind(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;
    // A socket file of a dead supervisor: nobody listens on it.
    let _ = std::fs::remove_file(path);
    let l = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

/// Read one hand-back, and check it: the sender is a child of the worker it
/// names, of this user, and the worker is one expected; each descriptor is
/// the kind of file it says.
async fn receive(
    stream: &tokio::net::UnixStream,
    want: &BTreeMap<u32, Expected>,
) -> Result<(u32, Vec<(FdKind, OwnedFd)>), String> {
    let cred = stream.peer_cred().map_err(|e| format!("peer credentials: {e}"))?;
    if cred.uid() != crate::sys::euid() {
        return Err(format!("sent by uid {}", cred.uid()));
    }
    let sender = cred.pid().and_then(|p| u32::try_from(p).ok()).ok_or("no sender pid")?;
    let mut buf = [0u8; 1024];
    let (n, fds) = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if stream.readable().await.is_err() {
                return Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
            }
            match crate::sys::recv_fds(stream.as_fd(), &mut buf) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                r => return r,
            }
        }
    })
    .await
    .map_err(|_| "nothing sent".to_string())?
    .map_err(|e| e.to_string())?;
    let hello: Hello = serde_json::from_slice(&buf[..n]).map_err(|e| format!("not a hand-back: {e}"))?;
    let e = want.get(&hello.pid).ok_or_else(|| format!("pid {} is no worker left running", hello.pid))?;
    let parent = crate::platform::proc_identity(sender).map(|id| id.ppid);
    if parent != Some(hello.pid) {
        return Err(format!("sender {sender} is not a child of worker {}", hello.pid));
    }
    if crate::platform::proc_identity(hello.pid).map(|id| id.start) != Some(e.start) {
        return Err(format!("pid {} is another process now", hello.pid));
    }
    if hello.kinds.len() != fds.len() {
        return Err("descriptors missing".into());
    }
    let mut out = Vec::new();
    for (kind, fd) in hello.kinds.into_iter().zip(fds) {
        let want_type = if matches!(kind, FdKind::Ipc | FdKind::Handoff) { libc::S_IFSOCK } else { libc::S_IFIFO };
        if crate::sys::fd_type(fd.as_fd()).ok() != Some(want_type) {
            return Err(format!("{kind:?} is not the kind of file it should be"));
        }
        out.push((kind, fd));
    }
    if !out.iter().any(|(k, _)| *k == FdKind::Ipc) {
        return Err("no IPC channel".into());
    }
    Ok((hello.pid, out))
}

async fn reply(stream: &tokio::net::UnixStream, answer: &Answer) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(answer).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut off = 0;
    while off < line.len() {
        stream.writable().await?;
        match stream.try_write(&line[off..]) {
            Ok(n) => off += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ helper

/// `warden recover-worker <socket> <fds>`: started by the shim of a worker
/// whose supervisor and keeper are both gone, with Warden's ends of the
/// worker's channels at fd 3 on (the order of [`KINDS`]); `<fds>` lists the
/// worker's numbers of those it holds (`5,7,8`), the only ones taken.
/// Starts wardend again if it died, then hands the descriptors to the app's
/// next supervisor. Prints that supervisor's answer (a JSON line) and exits
/// 0 once taken back; 1 otherwise (the shim tries again later).
pub fn helper_main(args: &[String]) -> i32 {
    let Some(socket) = args.first().map(PathBuf::from) else {
        eprintln!("warden recover-worker: internal command, started by the shim");
        return 2;
    };
    let worker = std::os::unix::process::parent_id();
    let held: Vec<usize> = args
        .get(1)
        .map(|l| l.split(',').filter_map(|n| n.parse::<i32>().ok()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|fd| usize::try_from(fd - crate::sys::SPARE_FD).ok())
        .filter(|i| *i < KINDS.len())
        .collect();
    let mut fds: Vec<(FdKind, OwnedFd)> = Vec::new();
    for i in held {
        if let Some(fd) = crate::sys::take_inherited_fd(3 + i as i32) {
            fds.push((KINDS[i], fd));
        }
    }
    if fds.is_empty() {
        return 2;
    }
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return 1 };
    rt.block_on(async move {
        // The app's next supervisor comes from wardend: bring it back first.
        crate::daemon::revive::revive_if_died().await;
        let started = Instant::now();
        while started.elapsed() < HELPER_WAIT {
            if let Ok(stream) = tokio::net::UnixStream::connect(&socket).await {
                if let Some(answer) = hand_back(stream, worker, &fds).await {
                    if answer.refused.is_none() {
                        if let Ok(line) = serde_json::to_string(&answer) {
                            println!("{line}");
                        }
                        return 0;
                    }
                    return 1;
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        1
    })
}

async fn hand_back(stream: tokio::net::UnixStream, worker: u32, fds: &[(FdKind, OwnedFd)]) -> Option<Answer> {
    let hello = Hello { pid: worker, kinds: fds.iter().map(|(k, _)| *k).collect() };
    let msg = serde_json::to_vec(&hello).ok()?;
    let borrowed: Vec<_> = fds.iter().map(|(_, f)| f.as_fd()).collect();
    stream.writable().await.ok()?;
    crate::sys::send_fds(stream.as_fd(), &msg, &borrowed).ok()?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut chunk = [0u8; 512];
        loop {
            stream.readable().await.ok()?;
            match stream.try_read(&mut chunk) {
                Ok(0) => return Some(()),
                Ok(n) => {
                    buf.extend_from_slice(&chunk[..n]);
                    if buf.contains(&b'\n') {
                        return Some(());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => return None,
            }
        }
    })
    .await
    .ok()??;
    serde_json::from_slice(buf.split(|b| *b == b'\n').next()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_socket_is_named_after_the_app() {
        let p = socket_path(Path::new("/run/warden/api"), "api").unwrap();
        assert_eq!(p, Path::new("/run/warden/api/api.recover.sock"));
        assert!(socket_path(Path::new(&"x".repeat(120)), "api").is_none(), "too long for sun_path");
    }

    #[test]
    fn the_spec_round_trips() {
        let s = Spec { exe: "/usr/bin/warden".into(), socket: "/run/a.sock".into(), pids: vec![10, 9] };
        let back: Spec = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!((back.exe, back.socket, back.pids), (s.exe, s.socket, s.pids));
    }
}

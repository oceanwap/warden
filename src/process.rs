//! Spawning and owning one OS process: stdout/stderr capture, the fd-3 IPC
//! pipe used by the shim, signal delivery, and exit reporting.
//!
//! Each process is owned by a task that holds its `Child`. Signals go through
//! that task, so a signal is only ever sent while the child is still unreaped
//! and its pid cannot have been reused.

use serde::Deserialize;
use std::os::fd::{FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::mpsc;

pub const IPC_FD: i32 = 3;
const MAX_LINE: usize = 16 * 1024;

pub struct Spec {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    /// Prefix for this process's output lines (`worker=<label>`).
    pub label: String,
    /// Let the worker write straight to Warden's stdout/stderr (no prefix,
    /// no copying through Warden; `warden logs` won't show its output).
    pub inherit_output: bool,
}

/// Message written by the shim / worker-mode host on fd 3, one JSON object per line.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct IpcMsg {
    pub ev: String,
    #[serde(default)]
    pub worker: Option<usize>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub code: Option<i32>,
    #[serde(default)]
    pub expected: Option<bool>,
    #[serde(default)]
    pub message: Option<String>,
    /// Private per-worker health socket opened by the shim.
    #[serde(default)]
    pub socket: Option<String>,
}

#[derive(Debug)]
pub enum ProcEvent {
    /// `note` explains exits Warden didn't observe normally (lost track of it).
    Exited {
        inst: u64,
        code: Option<i32>,
        signal: Option<i32>,
        note: Option<String>,
    },
    Ipc {
        inst: u64,
        msg: IpcMsg,
    },
}

/// Handle to a running process. Dropping it does not kill the process.
pub struct Handle {
    pub pid: u32,
    ctl: mpsc::UnboundedSender<i32>,
}

impl Handle {
    /// SIGTERM and SIGKILL go to the whole process group (like supervisord's
    /// stopasgroup/killasgroup), so `bun run <script>` wrappers or helpers the
    /// app spawned are not orphaned. Other signals go to the process only.
    pub fn signal(&self, sig: i32) {
        let _ = self.ctl.send(sig);
    }
}

pub fn spawn(spec: Spec, inst: u64, events: mpsc::UnboundedSender<ProcEvent>) -> std::io::Result<Handle> {
    let (ipc_read, ipc_write) = pipe()?;
    let write_fd = std::os::fd::AsRawFd::as_raw_fd(&ipc_write);

    let mut cmd = Command::new(&spec.program);
    let out = || if spec.inherit_output { Stdio::inherit() } else { Stdio::piped() };
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(out())
        .stderr(out())
        // Own process group: a terminal Ctrl-C reaches Warden only, and Warden
        // orchestrates the drain. SIGKILL goes to the group to catch grandchildren.
        .process_group(0)
        .kill_on_drop(false);
    if let Some(d) = &spec.cwd {
        cmd.current_dir(d);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.env("WARDEN_IPC_FD", IPC_FD.to_string());
    // SAFETY: only async-signal-safe libc calls between fork and exec.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(write_fd, IPC_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if write_fd == IPC_FD {
                // dup2 onto itself keeps FD_CLOEXEC; clear it so the fd survives exec.
                libc::fcntl(IPC_FD, libc::F_SETFD, 0);
            }
            #[cfg(target_os = "linux")]
            {
                // If Warden dies without cleaning up (SIGKILL), take the workers with it.
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    drop(ipc_write);
    let pid = child.id().unwrap_or(0);
    let label = spec.label.clone();
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<i32>();

    // Readers of the worker's output and IPC pipe. If one of them fails, the
    // worker could block on a full pipe or lose its readiness/heartbeat
    // channel, so it is killed and restarted rather than left half-supervised.
    let reader_failed = |what: &'static str, ctl: mpsc::UnboundedSender<i32>, label: String| {
        move |msg: String| {
            crate::error!(
                "worker's output reader failed; killing the worker so it restarts cleanly",
                worker = label,
                reader = what,
                panic = msg,
                hint = "this is a Warden bug: please report it with the log lines above",
            );
            let _ = ctl.send(libc::SIGKILL);
        }
    };
    if let Some(out) = child.stdout.take() {
        let on_fail = reader_failed("stdout", ctl_tx.clone(), label.clone());
        let fut = pump_output(out, label.clone(), "stdout");
        tokio::task::spawn_local(async move {
            if let Err(m) = crate::guard::catch_unwind(fut).await {
                on_fail(m);
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let on_fail = reader_failed("stderr", ctl_tx.clone(), label.clone());
        let fut = pump_output(err, label.clone(), "stderr");
        tokio::task::spawn_local(async move {
            if let Err(m) = crate::guard::catch_unwind(fut).await {
                on_fail(m);
            }
        });
    }
    match tokio::net::unix::pipe::Receiver::from_owned_fd(ipc_read) {
        Ok(rx) => {
            let on_fail = reader_failed("ipc", ctl_tx.clone(), label.clone());
            let fut = pump_ipc(rx, inst, events.clone(), label.clone());
            tokio::task::spawn_local(async move {
                if let Err(m) = crate::guard::catch_unwind(fut).await {
                    on_fail(m);
                }
            });
        }
        Err(e) => crate::warn!(
            "cannot read the worker's IPC pipe; readiness falls back to /proc and the watchdog is off for it",
            worker = label,
            error = e,
        ),
    }

    tokio::task::spawn_local(async move {
        let waited = crate::guard::catch_unwind(async move {
            crate::guard::fault("waiter");
            loop {
                tokio::select! {
                    st = child.wait() => break st,
                    Some(sig) = ctl_rx.recv() => {
                        // child.id() is None once reaped; never signal a reused pid.
                        if let Some(p) = child.id() {
                            let p = p as i32;
                            // SAFETY: plain kill(2).
                            // The group contains the process itself: signal once.
                            unsafe {
                                let group = (sig == libc::SIGKILL || sig == libc::SIGTERM) && libc::kill(-p, sig) == 0;
                                if !group {
                                    libc::kill(p, sig);
                                }
                            }
                        }
                    }
                }
            }
        })
        .await;
        let (code, signal, note) = match waited {
            Ok(Ok(st)) => {
                use std::os::unix::process::ExitStatusExt;
                (st.code(), st.signal(), None)
            }
            Ok(Err(e)) => {
                crate::error!(
                    "waiting for the worker failed; treating it as exited",
                    worker = label,
                    pid = pid,
                    error = e
                );
                (None, None, Some(format!("wait failed: {e}")))
            }
            Err(msg) => {
                // The task that owned the worker died. Its Child handle is now
                // an orphan (tokio reaps it later), so the pid is still ours:
                // kill the group now, then report the exit so crash handling
                // starts a fresh worker instead of the slot looking alive forever.
                if pid > 0 {
                    // SAFETY: plain kill(2) on our still-unreaped child.
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                }
                crate::error!(
                    "Warden lost track of a worker (its supervising task failed); killed it so it restarts",
                    worker = label,
                    pid = pid,
                    panic = msg,
                    hint = "this is a Warden bug: please report it with the log lines above",
                );
                (None, Some(libc::SIGKILL), Some(format!("killed by Warden after an internal error: {msg}")))
            }
        };
        // Anything the worker left behind in its process group (helpers that
        // ignored SIGTERM) goes with it. The group id can't be reused while any
        // member is alive, so this can only hit the worker's own group.
        if pid > 0 {
            // SAFETY: plain kill(2) on our child's process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        let _ = events.send(ProcEvent::Exited { inst, code, signal, note });
    });

    Ok(Handle { pid, ctl: ctl_tx })
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0i32; 2];
    #[cfg(target_os = "linux")]
    // SAFETY: fds is a valid 2-element array.
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let rc = unsafe {
        let rc = libc::pipe(fds.as_mut_ptr());
        if rc == 0 {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        rc
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: pipe just returned these fds and nothing else owns them.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Forward a child stream line by line to the log, capping line length.
/// Per-worker budget for output lines. Above it, lines are dropped (and
/// counted) instead of letting one chatty worker eat Warden's CPU (CP6).
const OUTPUT_LINES_PER_SEC: u32 = 10_000;
/// Per-worker budget for fd-3 IPC messages (heartbeats are 1/s per Worker).
const IPC_MSGS_PER_SEC: u32 = 1_000;
/// Longest IPC line kept; Warden's shim sends a few hundred bytes at most.
const MAX_IPC_LINE: usize = 64 * 1024;

/// Fixed one-second window counter.
struct RateLimit {
    window: std::time::Instant,
    used: u32,
    limit: u32,
    dropped: u64,
    last_report: Option<std::time::Instant>,
    last_bad_report: Option<std::time::Instant>,
}

impl RateLimit {
    fn new(limit: u32) -> Self {
        RateLimit {
            window: std::time::Instant::now(),
            used: 0,
            limit,
            dropped: 0,
            last_report: None,
            last_bad_report: None,
        }
    }
    fn allow(&mut self) -> bool {
        if self.window.elapsed() >= std::time::Duration::from_secs(1) {
            self.window = std::time::Instant::now();
            self.used = 0;
        }
        self.used += 1;
        if self.used > self.limit {
            self.dropped += 1;
            false
        } else {
            true
        }
    }
    /// Same cadence for a second kind of report (invalid input).
    fn report_bad(&mut self) -> bool {
        let due = self.last_bad_report.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
        if due {
            self.last_bad_report = Some(std::time::Instant::now());
        }
        due
    }
    /// Drops since the last report: the first right away, then at most one
    /// report per 10 s.
    fn report(&mut self) -> Option<u64> {
        let due = self.last_report.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10));
        if self.dropped > 0 && due {
            self.last_report = Some(std::time::Instant::now());
            Some(std::mem::take(&mut self.dropped))
        } else {
            None
        }
    }
}

/// Forward a child stream line by line to the log, capping line length and rate.
async fn pump_output<R: tokio::io::AsyncRead + Unpin>(mut r: R, label: String, stream: &'static str) {
    crate::guard::fault(stream);
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::with_capacity(256);
    let mut rate = RateLimit::new(OUTPUT_LINES_PER_SEC);
    loop {
        let n = match r.read(&mut chunk).await {
            Ok(0) => break,
            Err(e) => {
                crate::warn!("stopped reading worker output", worker = label, stream = stream, error = e);
                break;
            }
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' {
                emit(&label, stream, &mut line, &mut rate);
            } else {
                line.push(b);
                if line.len() >= MAX_LINE {
                    emit(&label, stream, &mut line, &mut rate);
                }
            }
        }
        if let Some(n) = rate.report() {
            crate::warn!(
                "worker writes too much output; lines dropped",
                worker = label,
                stream = stream,
                dropped = n,
                limit_per_s = OUTPUT_LINES_PER_SEC,
                hint = "lower the app's log level, or set [logging] worker_output = \"inherit\" to bypass Warden",
            );
        }
    }
    if !line.is_empty() {
        emit(&label, stream, &mut line, &mut rate);
    }
}

fn emit(label: &str, stream: &str, line: &mut Vec<u8>, rate: &mut RateLimit) {
    if rate.allow() {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        crate::logging::worker_output(label, stream, &String::from_utf8_lossy(line));
    }
    line.clear();
}

async fn pump_ipc(
    mut rx: tokio::net::unix::pipe::Receiver,
    inst: u64,
    events: mpsc::UnboundedSender<ProcEvent>,
    label: String,
) {
    crate::guard::fault("ipc");
    let mut chunk = [0u8; 4096];
    let mut buf: Vec<u8> = Vec::new();
    let mut rate = RateLimit::new(IPC_MSGS_PER_SEC);
    // Bad input is counted and summarised with the rate report, never logged
    // per line: a broken or hostile worker must not flood the log (A5).
    let (mut malformed, mut oversized) = (0u64, 0u64);
    let mut last_error = String::new();
    let hint = "something in the app writes to fd 3 (WARDEN_IPC_FD); only Warden's shim should";
    loop {
        let n = match rx.read(&mut chunk).await {
            Ok(0) => return,
            Err(e) => {
                crate::warn!(
                    "stopped reading the worker's IPC pipe; readiness and heartbeats from it are lost",
                    worker = label,
                    error = e,
                    hint = "the watchdog will replace the worker if heartbeats are required",
                );
                return;
            }
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        let mut start = 0;
        while let Some(i) = buf[start..].iter().position(|&b| b == b'\n') {
            let line = &buf[start..start + i];
            start += i + 1;
            if !rate.allow() {
                continue;
            }
            match serde_json::from_slice::<IpcMsg>(line) {
                Ok(msg) => {
                    // The supervisor is gone only while Warden exits.
                    let _ = events.send(ProcEvent::Ipc { inst, msg });
                }
                Err(e) => {
                    if malformed == 0 {
                        crate::debug!("ignoring malformed IPC line", worker = label, error = e);
                    }
                    malformed += 1;
                    last_error = e.to_string();
                }
            }
        }
        buf.drain(..start);
        if buf.len() > MAX_IPC_LINE {
            oversized += 1;
            buf.clear();
        }
        if let Some(n) = rate.report() {
            crate::warn!(
                "worker floods Warden's IPC pipe; messages dropped",
                worker = label,
                dropped = n,
                limit_per_s = IPC_MSGS_PER_SEC,
                hint = hint,
            );
        }
        if malformed + oversized > 0 && rate.report_bad() {
            crate::warn!(
                "worker sent invalid IPC messages; ignored",
                worker = label,
                malformed = malformed,
                oversized = oversized,
                max_bytes = MAX_IPC_LINE,
                last_error = last_error,
                hint = hint,
            );
            (malformed, oversized) = (0, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limit_window() {
        let mut r = RateLimit::new(3);
        assert!(r.allow() && r.allow() && r.allow());
        assert!(!r.allow() && !r.allow());
        assert_eq!(r.dropped, 2);
    }

    #[test]
    fn ipc_msg_parses() {
        let m: IpcMsg = serde_json::from_str(r#"{"ev":"listening","port":3000,"worker":2}"#).unwrap();
        assert_eq!(m.ev, "listening");
        assert_eq!(m.port, Some(3000));
        assert_eq!(m.worker, Some(2));
        let m: IpcMsg = serde_json::from_str(r#"{"ev":"exit","worker":1,"code":1,"expected":false}"#).unwrap();
        assert_eq!(m.expected, Some(false));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn spawns_reports_ipc_and_exit() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let spec = Spec {
                    program: "sh".into(),
                    args: vec!["-c".into(), "echo '{\"ev\":\"listening\",\"port\":1}' >&3; exit 7".into()],
                    cwd: None,
                    env: vec![],
                    label: "t".into(),
                    inherit_output: false,
                };
                spawn(spec, 42, tx).unwrap();
                // Exit and IPC arrive on independent tasks; accept either order.
                let (mut got_ipc, mut got_exit) = (false, false);
                while !(got_ipc && got_exit) {
                    match rx.recv().await.unwrap() {
                        ProcEvent::Ipc { inst, msg } => {
                            assert_eq!(inst, 42);
                            assert_eq!(msg.ev, "listening");
                            got_ipc = true;
                        }
                        ProcEvent::Exited { inst, code, .. } => {
                            assert_eq!((inst, code), (42, Some(7)));
                            got_exit = true;
                        }
                    }
                }
            })
            .await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn signals_are_delivered() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let spec = Spec {
                    program: "sleep".into(),
                    args: vec!["30".into()],
                    cwd: None,
                    env: vec![],
                    label: "t".into(),
                    inherit_output: false,
                };
                let h = spawn(spec, 1, tx).unwrap();
                h.signal(libc::SIGTERM);
                match rx.recv().await.unwrap() {
                    ProcEvent::Exited { signal, .. } => assert_eq!(signal, Some(libc::SIGTERM)),
                    e => panic!("unexpected {e:?}"),
                }
            })
            .await;
    }
}

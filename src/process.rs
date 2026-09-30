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
    Exited { inst: u64, code: Option<i32>, signal: Option<i32> },
    Ipc { inst: u64, msg: IpcMsg },
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
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
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

    if let Some(out) = child.stdout.take() {
        tokio::task::spawn_local(pump_output(out, spec.label.clone(), "stdout"));
    }
    if let Some(err) = child.stderr.take() {
        tokio::task::spawn_local(pump_output(err, spec.label.clone(), "stderr"));
    }
    if let Ok(rx) = tokio::net::unix::pipe::Receiver::from_owned_fd(ipc_read) {
        tokio::task::spawn_local(pump_ipc(rx, inst, events.clone(), spec.label.clone()));
    }

    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<i32>();
    tokio::task::spawn_local(async move {
        let status = loop {
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
        };
        // Anything the worker left behind in its process group (helpers that
        // ignored SIGTERM) goes with it. The group id can't be reused while any
        // member is alive, so this can only hit the worker's own group.
        if pid > 0 {
            // SAFETY: plain kill(2) on our child's process group.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
        let (code, signal) = match status {
            Ok(st) => {
                use std::os::unix::process::ExitStatusExt;
                (st.code(), st.signal())
            }
            Err(_) => (None, None),
        };
        let _ = events.send(ProcEvent::Exited { inst, code, signal });
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
async fn pump_output<R: tokio::io::AsyncRead + Unpin>(mut r: R, label: String, stream: &'static str) {
    let mut chunk = [0u8; 8192];
    let mut line: Vec<u8> = Vec::with_capacity(256);
    loop {
        let n = match r.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if b == b'\n' {
                emit(&label, stream, &mut line);
            } else {
                line.push(b);
                if line.len() >= MAX_LINE {
                    emit(&label, stream, &mut line);
                }
            }
        }
    }
    if !line.is_empty() {
        emit(&label, stream, &mut line);
    }
}

fn emit(label: &str, stream: &str, line: &mut Vec<u8>) {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    crate::logging::worker_output(label, stream, &String::from_utf8_lossy(line));
    line.clear();
}

async fn pump_ipc(
    mut rx: tokio::net::unix::pipe::Receiver,
    inst: u64,
    events: mpsc::UnboundedSender<ProcEvent>,
    label: String,
) {
    let mut chunk = [0u8; 4096];
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let n = match rx.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        buf.extend_from_slice(&chunk[..n]);
        while let Some(i) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            match serde_json::from_slice::<IpcMsg>(&line) {
                Ok(msg) => {
                    let _ = events.send(ProcEvent::Ipc { inst, msg });
                }
                Err(e) => crate::debug!("ignoring malformed IPC line", worker = label, error = e),
            }
        }
        if buf.len() > 64 * 1024 {
            buf.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! Control socket: the CLI talks to a running supervisor over a Unix socket
//! (mode 0600), one JSON request line and one JSON response line.
//! `logs` is answered here directly from the log ring buffer.

use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    /// Stop all workers; the supervisor stays up.
    Stop,
    /// Stop all workers and exit the supervisor.
    Shutdown,
    /// Restart one worker (graceful), or all workers (stop, then start).
    Restart {
        worker: Option<usize>,
    },
    /// Rolling restart: each new worker must pass the gates before the old one
    /// is drained. `safe`: preflight, canary soak with rollback, pauses.
    Reload {
        #[serde(default)]
        safe: bool,
    },
    Scale {
        count: usize,
    },
    Logs {
        lines: usize,
        follow: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// Rollout started by this request; poll `status.last_rollout` for its outcome.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl Response {
    pub fn ok(msg: impl Into<String>) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, message: Some(msg.into()), status: None, seq: None }
    }
    pub fn started(msg: impl Into<String>, seq: u64) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: Some(seq) }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub app: String,
    pub mode: String,
    pub pid: u32,
    pub uptime_secs: u64,
    pub workers_configured: usize,
    pub workers_ready: usize,
    pub healthy: Option<bool>,
    pub supervisor_rss_bytes: Option<u64>,
    /// Worker mode: the Bun process hosting the Workers.
    pub host: Option<HostStatus>,
    pub reloading: bool,
    pub shutting_down: bool,
    /// Rollout in progress (reload, safe-reload, restart N, recycling).
    #[serde(default)]
    pub rollout: Option<RolloutStatus>,
    #[serde(default)]
    pub last_rollout: Option<RolloutOutcome>,
    pub workers: Vec<WorkerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutStatus {
    pub seq: u64,
    pub kind: String,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RolloutOutcome {
    pub seq: u64,
    pub kind: String,
    pub ok: bool,
    pub message: String,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub restarts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerStatus {
    pub id: usize,
    pub state: String,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
    pub restarts: u64,
    pub crashes: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_seconds: Option<f64>,
    pub cpu_percent: Option<f64>,
    pub last_exit: Option<String>,
    /// Per-worker health verdict (private-socket checks).
    #[serde(default)]
    pub healthy: Option<bool>,
}

pub type ControlMsg = (Request, oneshot::Sender<Response>);

/// The runtime directory holds the control socket, the shim every worker
/// preloads and the per-worker health sockets. If another user could write
/// to it they could replace the shim (code execution as the service user), so
/// refuse unless we own it, it is not a symlink, and only we can write to it.
pub fn ensure_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if !dir.exists() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("chmod {}: {e}", dir.display()))?;
    }
    let meta = std::fs::symlink_metadata(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // SAFETY: geteuid has no preconditions.
    let euid = unsafe { libc::geteuid() };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!("{} must be a real directory, not a symlink", dir.display()));
    }
    if meta.uid() != euid {
        return Err(format!(
            "{} is owned by uid {}, not by us (uid {euid}); refusing to use it",
            dir.display(),
            meta.uid()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group/others (mode {:o}); refusing to use it",
            dir.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(())
}

/// Bind the socket, refusing if another supervisor already answers on it.
pub async fn bind(path: &Path) -> Result<UnixListener, String> {
    if let Some(dir) = path.parent() {
        ensure_private_dir(dir)?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            return Err(format!("another warden is already running on {}", path.display()));
        }
        let _ = std::fs::remove_file(path);
    }
    let l = UnixListener::bind(path).map_err(|e| format!("binding {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(l)
}

pub async fn serve(listener: UnixListener, tx: mpsc::UnboundedSender<ControlMsg>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else { continue };
        let tx = tx.clone();
        tokio::task::spawn_local(async move {
            let _ = handle(stream, tx).await;
        });
    }
}

async fn handle(stream: UnixStream, tx: mpsc::UnboundedSender<ControlMsg>) -> std::io::Result<()> {
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(r.take(64 * 1024));
    reader.read_line(&mut line).await?;
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => return write_json(&mut w, &Response::err(format!("bad request: {e}"))).await,
    };
    if let Request::Logs { lines, follow } = req {
        // Subscribe first so nothing is lost between the snapshot and the stream.
        let mut rx = crate::logging::subscribe();
        for l in crate::logging::recent(lines) {
            w.write_all(l.as_bytes()).await?;
            w.write_all(b"\n").await?;
        }
        if !follow {
            return Ok(());
        }
        loop {
            match rx.recv().await {
                Ok(l) => {
                    w.write_all(l.as_bytes()).await?;
                    w.write_all(b"\n").await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    w.write_all(format!("... {n} lines skipped\n").as_bytes()).await?;
                }
                Err(_) => return Ok(()),
            }
        }
    }
    let (rtx, rrx) = oneshot::channel();
    let resp = if tx.send((req, rtx)).is_err() {
        Response::err("supervisor is shutting down")
    } else {
        rrx.await.unwrap_or_else(|_| Response::err("supervisor is shutting down"))
    };
    write_json(&mut w, &resp).await
}

async fn write_json(w: &mut tokio::net::unix::OwnedWriteHalf, v: &Response) -> std::io::Result<()> {
    let mut s = serde_json::to_string(v).unwrap_or_else(|_| "{\"ok\":false}".into());
    s.push('\n');
    w.write_all(s.as_bytes()).await
}

/// Client side: send one request. For `logs`, lines are written to `out` as they arrive.
pub async fn call(
    path: &Path,
    req: &Request,
    out: &mut (dyn std::io::Write + Send),
) -> Result<Option<Response>, String> {
    let stream = UnixStream::connect(path)
        .await
        .map_err(|e| format!("cannot reach warden at {} ({e}). Is it running?", path.display()))?;
    let (r, mut w) = stream.into_split();
    let mut s = serde_json::to_string(req).map_err(|e| e.to_string())?;
    s.push('\n');
    w.write_all(s.as_bytes()).await.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    if matches!(req, Request::Logs { .. }) {
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => return Ok(None),
                Ok(_) => {
                    let _ = out.write_all(line.as_bytes());
                    let _ = out.flush();
                }
            }
        }
    }
    reader.read_line(&mut line).await.map_err(|e| e.to_string())?;
    serde_json::from_str(line.trim()).map(Some).map_err(|e| format!("bad response: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_dir_must_be_private() {
        let base = std::env::temp_dir().join(format!("warden-dir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ok = base.join("ok");
        ensure_private_dir(&ok).unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(ensure_private_dir(&ok).unwrap_err().contains("writable by group/others"));
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&ok).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&ok, &link).unwrap();
        assert!(ensure_private_dir(&link).unwrap_err().contains("symlink"));
    }

    #[test]
    fn request_wire_format() {
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"restart","worker":2}"#).unwrap(),
            Request::Restart { worker: Some(2) }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"scale","count":3}"#).unwrap(),
            Request::Scale { count: 3 }
        );
    }
}

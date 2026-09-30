//! Control socket: the CLI talks to a running supervisor over a Unix socket
//! (mode 0600), one JSON request line and one JSON response line.
//! `logs` is answered here directly from the log ring buffer, and
//! `subscribe` streams events from the in-process bus (`events`).

use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    /// Stop all workers; the supervisor stays up.
    Stop,
    /// Stop all workers and exit the supervisor.
    Shutdown,
    /// Replace workers one at a time through the health gates (all, or one);
    /// `hard`: stop every worker, then start them again (PM2's `restart`).
    Restart {
        worker: Option<usize>,
        #[serde(default)]
        hard: bool,
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
    /// Start the workers of a stopped app.
    Start,
    /// Clear restart counters and FAILED state (one worker or all), and
    /// start FAILED workers now.
    Reset {
        #[serde(default)]
        worker: Option<usize>,
    },
    /// Send a signal to the workers (or one worker): `SIGUSR2`, `USR2`, `12`.
    Signal {
        signal: String,
        #[serde(default)]
        worker: Option<usize>,
    },
    /// The effective config and paths, with env values hidden unless
    /// `show_secrets`.
    Config {
        #[serde(default)]
        show_secrets: bool,
    },
    /// Empty the in-memory log buffers and truncate the log file.
    Flush,
    Logs {
        lines: usize,
        follow: bool,
        /// Only lines about this worker (its output and Warden's events for it).
        #[serde(default)]
        worker: Option<String>,
        /// Only Warden's own events, no worker output.
        #[serde(default)]
        events: bool,
        /// Only worker output on this stream: "stdout" or "stderr".
        #[serde(default)]
        stream: Option<String>,
    },
    /// Show or change the log level at runtime (not saved to the config).
    #[serde(rename = "log-level")]
    LogLevel {
        #[serde(default)]
        level: Option<crate::config::Level>,
    },
    /// Stream live events (docs/protocol.md): a status snapshot, worker and
    /// rollout events as they happen, a status every `interval_ms`.
    Subscribe {
        #[serde(default)]
        interval_ms: Option<u64>,
        /// Also every log line, as `log` events.
        #[serde(default)]
        logs: bool,
    },
}

/// Most control connections served at once; more are refused with a message.
pub const MAX_CONNECTIONS: usize = 64;
/// A client must send its request line within this time.
pub const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

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
    /// `config`: the effective config and paths.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub info: Option<serde_json::Value>,
}

impl Response {
    pub fn ok(msg: impl Into<String>) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: None, info: None }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Response { ok: false, message: Some(msg.into()), status: None, seq: None, info: None }
    }
    pub fn started(msg: impl Into<String>, seq: u64) -> Self {
        Response { ok: true, message: Some(msg.into()), status: None, seq: Some(seq), info: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub app: String,
    #[serde(default)]
    pub namespace: String,
    pub mode: String,
    /// The config file this supervisor was started with.
    #[serde(default)]
    pub config_path: Option<String>,
    /// systemd unit running this supervisor (`warden@api.service`), if any.
    #[serde(default)]
    pub unit: Option<String>,
    /// How this supervisor was started: `systemd`, `background` (`warden
    /// start` or `wardend`: `wardend` restarts it if it dies) or `terminal`.
    #[serde(default)]
    pub launched: String,
    /// `warden stop`: workers stopped on request, supervisor idle.
    #[serde(default)]
    pub stopped: bool,
    #[serde(default)]
    pub log_file: Option<String>,
    #[serde(default)]
    pub version: String,
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
    /// DS2: replacements held because most workers fail health at once.
    #[serde(default)]
    pub health_suspended: bool,
    /// Log lines dropped because stdout could not keep up (CP5).
    #[serde(default)]
    pub log_lines_dropped: u64,
    /// Rollout in progress (reload, safe-reload, restart N, recycling).
    #[serde(default)]
    pub rollout: Option<RolloutStatus>,
    #[serde(default)]
    pub last_rollout: Option<RolloutOutcome>,
    pub workers: Vec<WorkerStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutStatus {
    pub seq: u64,
    pub kind: String,
    pub phase: String,
    pub done: usize,
    pub total: usize,
    pub elapsed_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RolloutOutcome {
    pub seq: u64,
    pub kind: String,
    pub ok: bool,
    pub message: String,
    pub duration_secs: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HostStatus {
    pub pid: u32,
    pub uptime_secs: u64,
    pub rss_bytes: Option<u64>,
    pub cpu_percent: Option<f64>,
    pub restarts: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    let euid = crate::sys::euid();
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
    let slots = std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    let mut last_refusal_log: Option<std::time::Instant> = None;
    let mut last_accept_log: Option<std::time::Instant> = None;
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            Err(e) => {
                // Usually EMFILE/ENFILE: out of file descriptors. Back off
                // instead of spinning, and say so at most every 10 s.
                if last_accept_log.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10)) {
                    crate::error!(
                        "control socket cannot accept connections; retrying",
                        error = e,
                        hint = "Warden may be out of file descriptors: check LimitNOFILE and `ls /proc/<warden pid>/fd | wc -l`",
                    );
                    last_accept_log = Some(std::time::Instant::now());
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            if last_refusal_log.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(10)) {
                crate::warn!(
                    "too many control connections; refusing new ones",
                    limit = MAX_CONNECTIONS,
                    hint =
                        "something is opening the control socket in a loop or leaving `warden logs -f` sessions open",
                );
                last_refusal_log = Some(std::time::Instant::now());
            }
            crate::guard::spawn_request("control refusal", async move {
                let (_, mut w) = stream.into_split();
                let msg = format!("too many control connections (limit {MAX_CONNECTIONS}); try again");
                let _ = tokio::time::timeout(REQUEST_TIMEOUT, write_json(&mut w, &Response::err(msg))).await;
            });
            continue;
        };
        let tx = tx.clone();
        crate::guard::spawn_request("control request", async move {
            let _permit = permit;
            if let Err(e) = handle(stream, tx).await {
                crate::debug!("control connection ended with an error", error = e);
            }
        });
    }
}

async fn handle(stream: UnixStream, tx: mpsc::UnboundedSender<ControlMsg>) -> std::io::Result<()> {
    crate::guard::fault("control");
    let (r, mut w) = stream.into_split();
    let mut line = String::new();
    let mut reader = BufReader::new(r.take(64 * 1024));
    match tokio::time::timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await {
        Ok(res) => {
            res?;
        }
        Err(_) => {
            crate::debug!("control client sent no request in time; closing", timeout_s = REQUEST_TIMEOUT.as_secs());
            let msg = format!("no request received within {} s", REQUEST_TIMEOUT.as_secs());
            return reply(&mut w, &Response::err(msg)).await;
        }
    }
    let req: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("bad request: {e} (is the `warden` CLI the same version as the running Warden?)");
            return reply(&mut w, &Response::err(msg)).await;
        }
    };
    match req {
        Request::Logs { lines, follow, worker, events, stream } => {
            let keep = |l: &str| log_filter(l, worker.as_deref(), events) && stream_filter(l, stream.as_deref());
            // Subscribe first so nothing is lost between the snapshot and the stream.
            let mut rx = crate::logging::subscribe();
            let recent = crate::logging::recent_matching(lines, &keep);
            for l in recent {
                w.write_all(l.as_bytes()).await?;
                w.write_all(b"\n").await?;
            }
            if !follow {
                return Ok(());
            }
            loop {
                match rx.recv().await {
                    Ok(l) if keep(&l) => {
                        w.write_all(l.as_bytes()).await?;
                        w.write_all(b"\n").await?;
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        let note = format!("... {n} lines skipped (this client reads more slowly than Warden logs)\n");
                        w.write_all(note.as_bytes()).await?;
                    }
                    Err(_) => return Ok(()),
                }
            }
        }
        Request::Subscribe { interval_ms, logs } => {
            // Anything the client sent after its request line is ignored; the
            // raw read half only tells us when it hangs up.
            let r = reader.into_inner().into_inner();
            subscribe(r, w, tx, interval_ms, logs).await
        }
        Request::Flush => {
            crate::logging::clear();
            crate::info!("logs flushed on request");
            reply(&mut w, &Response::ok("log buffer emptied and log file truncated")).await
        }
        Request::LogLevel { level } => {
            let old = crate::logging::level();
            let resp = match level {
                None => Response::ok(format!("log level: {}", level_name(old))),
                Some(new) => {
                    // Log at whichever of the two levels lets the line through.
                    if new > old {
                        crate::info!("log level changed", from = level_name(old), to = level_name(new));
                    }
                    crate::logging::set_level(new);
                    if new <= old {
                        crate::info!("log level changed", from = level_name(old), to = level_name(new));
                    }
                    Response::ok(format!(
                        "log level: {} (was {}; until Warden restarts or a reload changes logging.level)",
                        level_name(new),
                        level_name(old)
                    ))
                }
            };
            reply(&mut w, &resp).await
        }
        req => {
            let (rtx, rrx) = oneshot::channel();
            let resp = if tx.send((req, rtx)).is_err() {
                Response::err("Warden is shutting down")
            } else {
                rrx.await.unwrap_or_else(|_| Response::err("Warden is shutting down"))
            };
            reply(&mut w, &resp).await
        }
    }
}

/// `subscribe` (docs/protocol.md): `hello`, a `status` snapshot, then bus
/// events, a `status` every interval and (with `logs`) every log line, until
/// the client hangs up, stops reading for `REQUEST_TIMEOUT`, or the
/// supervisor says `bye`. Every write is bounded, and the supervisor only
/// ever hands this task events through the bounded bus: a stuck client costs
/// it nothing but lost events (`lagged`).
async fn subscribe(
    mut r: OwnedReadHalf,
    mut w: OwnedWriteHalf,
    tx: mpsc::UnboundedSender<ControlMsg>,
    interval_ms: Option<u64>,
    logs: bool,
) -> std::io::Result<()> {
    // Subscribe first so nothing is lost between the snapshot and the stream.
    let mut bus = crate::events::subscribe();
    let mut lines = logs.then(crate::logging::subscribe);
    let Some(status) = fetch_status(&tx).await else {
        return reply(&mut w, &Response::err("Warden is shutting down")).await;
    };
    let app = status.app.clone();
    let hello = Event::Hello {
        protocol: crate::events::PROTOCOL,
        app: Some(app.clone()),
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    let mut out = Subscriber { w, app: app.clone() };
    if !out.send(&hello).await? || !out.send(&Event::Status { app: app.clone(), status: Box::new(status) }).await? {
        return Ok(());
    }
    let mut tick = tokio::time::interval(crate::events::interval(interval_ms));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await; // the first tick is immediate: the snapshot above
    let mut scratch = [0u8; 512];
    loop {
        let ev = tokio::select! {
            ev = bus.recv() => match ev {
                Ok(ev) => {
                    if matches!(&ev, Event::Bye { app: Some(a), .. } if *a == app) {
                        // Log lines already queued (the supervisor's last words) go first.
                        while let Some(Ok(line)) = lines.as_mut().map(|l| l.try_recv()) {
                            if !out.send(&Event::Log { app: app.clone(), line: line.to_string() }).await? {
                                return Ok(());
                            }
                        }
                        out.send(&ev).await?;
                        return Ok(());
                    }
                    ev
                }
                Err(RecvError::Lagged(n)) => Event::Lagged { app: Some(app.clone()), dropped: n },
                // The bus sender is a static that is never dropped.
                Err(RecvError::Closed) => return Ok(()),
            },
            line = next_line(&mut lines) => match line {
                Ok(line) => Event::Log { app: app.clone(), line: line.to_string() },
                Err(RecvError::Lagged(n)) => Event::Lagged { app: Some(app.clone()), dropped: n },
                Err(RecvError::Closed) => {
                    lines = None;
                    continue;
                }
            },
            _ = tick.tick() => match fetch_status(&tx).await {
                Some(s) => Event::Status { app: app.clone(), status: Box::new(s) },
                // Exiting: `bye` (or EOF) follows.
                None => continue,
            },
            n = r.read(&mut scratch) => match n {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => continue,
            },
        };
        if !out.send(&ev).await? {
            return Ok(());
        }
    }
}

/// The writing end of one subscription.
struct Subscriber {
    w: OwnedWriteHalf,
    app: String,
}

impl Subscriber {
    /// Write one event line. `Ok(false)`: the client did not read it within
    /// `REQUEST_TIMEOUT` and is dropped (it can reconnect).
    async fn send(&mut self, ev: &Event) -> std::io::Result<bool> {
        let mut s = match serde_json::to_string(ev) {
            Ok(s) => s,
            // Not expected for these types; skip the event rather than the client.
            Err(e) => {
                crate::debug!("event could not be encoded; skipped", error = e);
                return Ok(true);
            }
        };
        s.push('\n');
        match tokio::time::timeout(REQUEST_TIMEOUT, self.w.write_all(s.as_bytes())).await {
            Ok(r) => r.map(|_| true),
            Err(_) => {
                crate::debug!(
                    "event subscriber stopped reading; disconnected it",
                    app = self.app,
                    timeout_s = REQUEST_TIMEOUT.as_secs(),
                    hint = "the client (`warden events`, wardend, a GUI) is stuck or too slow; it can reconnect",
                );
                Ok(false)
            }
        }
    }
}

async fn next_line(
    lines: &mut Option<tokio::sync::broadcast::Receiver<crate::logging::Line>>,
) -> Result<crate::logging::Line, RecvError> {
    match lines {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// A status from the supervisor's event loop, as `warden status` gets it.
async fn fetch_status(tx: &mpsc::UnboundedSender<ControlMsg>) -> Option<Status> {
    let (rtx, rrx) = oneshot::channel();
    tx.send((Request::Status, rtx)).ok()?;
    tokio::time::timeout(REQUEST_TIMEOUT, rrx).await.ok()?.ok()?.status
}

/// Longest the supervisor waits for subscribers to take its `bye` before exiting.
pub const BYE_FLUSH: std::time::Duration = std::time::Duration::from_millis(200);

/// The supervisor exits on purpose: tell every subscriber `bye`, then give
/// their tasks up to `max` to write it (at least `min`, so replies already
/// sent to other control clients go out too). A client that does not take it
/// in time sees EOF without `bye`, as for a crash.
pub async fn say_bye(app: &str, reason: &str, min: std::time::Duration, max: std::time::Duration) {
    let t0 = tokio::time::Instant::now();
    crate::events::emit(Event::Bye { app: Some(app.to_string()), reason: reason.to_string() });
    loop {
        let waited = t0.elapsed();
        if waited >= max || (waited >= min && !crate::events::active()) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
}

fn level_name(l: crate::config::Level) -> &'static str {
    use crate::config::Level::*;
    match l {
        Debug => "debug",
        Info => "info",
        Warn => "warn",
        Error => "error",
    }
}

/// `warden logs --worker N` keeps worker N's output and Warden's events that
/// name it (`worker=N`); `--events` drops worker output.
pub fn log_filter(line: &str, worker: Option<&str>, events_only: bool) -> bool {
    let is_output = line.contains(" OUT   worker=");
    if events_only && is_output {
        return false;
    }
    match worker {
        None => true,
        // Output lines: only the prefix counts, not what the app printed.
        Some(w) if is_output => line.contains(&format!(" OUT   worker={w} ")),
        Some(w) => {
            let needle = format!(" worker={w}");
            line.match_indices(&needle)
                .any(|(i, _)| matches!(line.as_bytes().get(i + needle.len()), None | Some(b' ') | Some(b'\n')))
        }
    }
}

/// `--out` / `--err`: only worker output lines on that stream.
pub fn stream_filter(line: &str, stream: Option<&str>) -> bool {
    match stream {
        None => true,
        Some(s) => line.contains(" OUT   worker=") && line.contains(&format!(" {s}: ")),
    }
}

/// Write a response, giving up if the client does not read it in time.
async fn reply(w: &mut tokio::net::unix::OwnedWriteHalf, v: &Response) -> std::io::Result<()> {
    match tokio::time::timeout(REQUEST_TIMEOUT, write_json(w, v)).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "client did not read the response")),
    }
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
                    // The reader went away (`| head`, Ctrl-C): stop following.
                    if out.write_all(line.as_bytes()).and_then(|_| out.flush()).is_err() {
                        return Ok(None);
                    }
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
    fn log_filters() {
        let out = "2026-09-30T12:00:00.000Z OUT   worker=1 stdout: hello worker=12";
        let ev = "2026-09-30T12:00:00.000Z INFO  worker ready worker=12 pid=5";
        let other = "2026-09-30T12:00:00.000Z INFO  reload finished";
        assert!(log_filter(out, None, false) && log_filter(ev, None, false));
        assert!(!log_filter(out, None, true) && log_filter(ev, None, true) && log_filter(other, None, true));
        assert!(log_filter(out, Some("1"), false));
        assert!(!log_filter(ev, Some("1"), false), "worker=12 is not worker=1");
        assert!(log_filter(ev, Some("12"), false));
        assert!(!log_filter(out, Some("12"), false), "what the app printed doesn't count");
        assert!(!log_filter(other, Some("1"), false));
        assert!(!log_filter(out, Some("1"), true));
    }

    fn status_of(app: &str) -> Status {
        Status {
            app: app.into(),
            namespace: "default".into(),
            mode: "process".into(),
            config_path: None,
            unit: None,
            launched: "terminal".into(),
            stopped: false,
            log_file: None,
            version: "test".into(),
            pid: 1,
            uptime_secs: 0,
            workers_configured: 1,
            workers_ready: 1,
            healthy: None,
            supervisor_rss_bytes: None,
            host: None,
            reloading: false,
            shutting_down: false,
            health_suspended: false,
            log_lines_dropped: 0,
            rollout: None,
            last_rollout: None,
            workers: vec![],
        }
    }

    /// A stand-in for the supervisor's event loop: answers `status`.
    fn fake_supervisor(app: &'static str) -> mpsc::UnboundedSender<ControlMsg> {
        let (tx, mut rx) = mpsc::unbounded_channel::<ControlMsg>();
        tokio::spawn(async move {
            while let Some((req, reply)) = rx.recv().await {
                let resp = match req {
                    Request::Status => Response { status: Some(status_of(app)), ..Response::ok("") },
                    _ => Response::err("not in this test"),
                };
                let _ = reply.send(resp);
            }
        });
        tx
    }

    type Lines = tokio::io::Lines<BufReader<OwnedReadHalf>>;

    /// Connect and subscribe; the server side runs `handle` as the socket does.
    async fn subscribed(
        app: &'static str,
        req: &str,
    ) -> (Lines, OwnedWriteHalf, tokio::task::JoinHandle<std::io::Result<()>>) {
        let (client, server) = UnixStream::pair().unwrap();
        let server = tokio::spawn(handle(server, fake_supervisor(app)));
        let (r, mut w) = client.into_split();
        w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        (BufReader::new(r).lines(), w, server)
    }

    /// The next event, or None at EOF.
    async fn next(lines: &mut Lines) -> Option<Event> {
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("no event within 5 s")
            .unwrap()?;
        Some(serde_json::from_str(&line).unwrap_or_else(|e| panic!("bad event {line}: {e}")))
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_streams_hello_status_events_and_bye() {
        const APP: &str = "unit-sub";
        let (mut lines, _w, server) = subscribed(APP, r#"{"cmd":"subscribe","interval_ms":250}"#).await;
        match next(&mut lines).await {
            Some(Event::Hello { protocol, app, pid, .. }) => {
                assert_eq!((protocol, app.as_deref(), pid), (crate::events::PROTOCOL, Some(APP), std::process::id()));
            }
            other => panic!("expected hello, got {other:?}"),
        }
        assert!(matches!(next(&mut lines).await, Some(Event::Status { app, .. }) if app == APP));

        // Bus events are forwarded (other tests may emit on the same bus).
        crate::events::worker(APP, 3, crate::events::WorkerEvent::Ready, Some(7), Some("startup_ms=1".into()));
        let mut periodic = 0;
        loop {
            match next(&mut lines).await {
                Some(Event::Worker { app, worker: 3, event, pid, .. }) if app == APP => {
                    assert_eq!((event, pid), (crate::events::WorkerEvent::Ready, Some(7)));
                    break;
                }
                Some(Event::Status { .. }) => periodic += 1,
                Some(_) => {}
                None => panic!("EOF before the worker event"),
            }
        }
        // A status every interval_ms.
        let t0 = std::time::Instant::now();
        while periodic < 2 {
            if let Some(Event::Status { app, .. }) = next(&mut lines).await {
                assert_eq!(app, APP);
                periodic += 1;
            }
        }
        assert!(t0.elapsed() < std::time::Duration::from_secs(2), "{:?}", t0.elapsed());

        // Another app's bye (not possible in a supervisor, but on a shared test bus) is just forwarded.
        crate::events::emit(Event::Bye { app: Some("someone-else".into()), reason: "x".into() });
        crate::events::emit(Event::Bye { app: Some(APP.into()), reason: "shutdown request".into() });
        loop {
            match next(&mut lines).await {
                Some(Event::Bye { app: Some(app), reason }) if app == APP => {
                    assert_eq!(reason, "shutdown request");
                    break;
                }
                Some(_) => {}
                None => panic!("EOF before bye"),
            }
        }
        assert!(next(&mut lines).await.is_none(), "EOF right after bye");
        server.await.unwrap().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscriber_hanging_up_is_noticed_at_once() {
        let (mut lines, w, server) = subscribed("unit-hangup", r#"{"cmd":"subscribe","interval_ms":60000}"#).await;
        assert!(matches!(next(&mut lines).await, Some(Event::Hello { .. })));
        assert!(matches!(next(&mut lines).await, Some(Event::Status { .. })));
        drop((lines, w));
        // No event is due for a minute: only the read side can notice.
        let done = tokio::time::timeout(std::time::Duration::from_secs(2), server).await;
        assert!(done.is_ok(), "the subscription outlived its client");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn subscribe_with_logs_sends_log_lines() {
        let (mut lines, _w, _server) = subscribed("unit-logs", r#"{"cmd":"subscribe","logs":true}"#).await;
        assert!(matches!(next(&mut lines).await, Some(Event::Hello { .. })));
        assert!(matches!(next(&mut lines).await, Some(Event::Status { .. })));
        crate::info!("unit-logs marker line", n = 1);
        loop {
            match next(&mut lines).await {
                Some(Event::Log { app, line }) if line.contains("unit-logs marker line") => {
                    assert_eq!(app, "unit-logs");
                    break;
                }
                Some(_) => {}
                None => panic!("EOF before the log line"),
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bye_waits_for_subscribers_but_never_long() {
        // A subscriber that never takes its `bye`: exit is delayed by `max`, no more.
        let _stuck = crate::events::subscribe();
        let t0 = std::time::Instant::now();
        say_bye("unit-bye", "test", std::time::Duration::from_millis(50), BYE_FLUSH).await;
        let took = t0.elapsed();
        assert!(took >= BYE_FLUSH && took < BYE_FLUSH * 3, "{took:?}");
    }

    #[test]
    fn request_wire_format() {
        assert_eq!(serde_json::to_string(&Request::Status).unwrap(), r#"{"cmd":"status"}"#);
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"restart","worker":2}"#).unwrap(),
            Request::Restart { worker: Some(2), hard: false }
        );
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"scale","count":3}"#).unwrap(),
            Request::Scale { count: 3 }
        );
        // Older clients send logs without the filter fields.
        assert_eq!(
            serde_json::from_str::<Request>(r#"{"cmd":"logs","lines":5,"follow":false}"#).unwrap(),
            Request::Logs { lines: 5, follow: false, worker: None, events: false, stream: None }
        );
        assert_eq!(
            serde_json::to_string(&Request::LogLevel { level: Some(crate::config::Level::Debug) }).unwrap(),
            r#"{"cmd":"log-level","level":"debug"}"#
        );
    }
}

//! wardend's socket (docs/protocol.md, "Daemon socket"): newline-delimited
//! JSON over a Unix socket, local or at the end of an SSH tunnel.
//!
//! - `feed`: one long-lived `subscribe` connection, as a stream of batches.
//!   It reconnects with backoff when wardend restarts, and says so. The
//!   socket is read continuously and events are handed to the UI at most
//!   once per `FLUSH_EVERY`: statuses and host metrics are coalesced, and
//!   log lines past `BATCH_LOG_CAP` are dropped and counted. A slow window
//!   loses lines; wardend never waits for it (and never drops it).
//! - `daemon_request`, `app_request`, `app_logs`: one short connection each.
//!
//! Everything here runs on the async executor, never on the UI thread, and
//! every error says what failed and how to fix it.

use crate::ssh;
use iced::futures::channel::mpsc;
use iced::futures::{SinkExt, Stream, StreamExt};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::time::Instant;
use warden_protocol::control::{Request, Response};
use warden_protocol::events::{DaemonReply, DaemonRequest, Event, ResourceHistory};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// wardend answers `subscribe` with `hello` at once.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// wardend waits up to 30 s for a supervisor to answer a forwarded request.
pub const APP_TIMEOUT: Duration = Duration::from_secs(35);
/// wardend answers `history` from memory, at once.
pub const HISTORY_TIMEOUT: Duration = Duration::from_secs(10);
/// At most one batch per frame-ish: a flood costs the UI 20 updates a second.
pub const FLUSH_EVERY: Duration = Duration::from_millis(50);
/// Longest line accepted from wardend (an `apps` event with many apps).
pub const MAX_LINE: usize = 8 << 20;
/// Log lines kept per batch (the logs pane holds as many).
pub const BATCH_LOG_CAP: usize = crate::logs::LOG_CAP;
/// Other events kept per batch; more are counted as skipped.
pub const BATCH_EVENT_CAP: usize = 5000;

/// Where wardend is.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Endpoint {
    /// A socket on this machine.
    Socket(PathBuf),
    /// A remote host, through `ssh -L`.
    Ssh(ssh::Target),
}

impl Endpoint {
    pub fn describe(&self) -> String {
        match self {
            Endpoint::Socket(p) => p.display().to_string(),
            Endpoint::Ssh(t) => format!("{}:{} (ssh)", t.dest, t.remote_socket),
        }
    }
}

/// wardend's socket on this machine, as `warden` finds it.
pub fn local_socket() -> PathBuf {
    let euid = rustix::process::geteuid().as_raw();
    let uid = rustix::process::getuid().as_raw();
    warden_protocol::paths::wardend_socket(&warden_protocol::paths::runtime_dir(euid, uid))
}

/// The `subscribe` request's options.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FeedOptions {
    pub interval_ms: u64,
    pub logs: bool,
    /// Only these apps' events (`apps` and `host` always come); empty: all.
    pub apps: Vec<String>,
}

impl FeedOptions {
    /// Every app, a status per second, no log lines.
    pub fn all() -> FeedOptions {
        FeedOptions { interval_ms: 1000, logs: false, apps: Vec::new() }
    }

    /// One app's log lines (statuses thinned out: the main feed has them).
    pub fn logs_of(app: &str) -> FeedOptions {
        FeedOptions { interval_ms: 60_000, logs: true, apps: vec![app.to_string()] }
    }
}

/// What the feed tells the UI.
#[derive(Debug, Clone)]
pub enum FeedMsg {
    Connecting {
        attempt: u32,
        what: String,
    },
    /// `hello` arrived; short requests go to `socket` (the tunnel's local end
    /// for a remote host).
    Connected {
        socket: PathBuf,
    },
    Batch(Batch),
    /// The connection failed or ended; the next attempt is in `retry_in`.
    /// `not_running`: nothing listens (wardend is not started).
    Disconnected {
        error: String,
        not_running: bool,
        retry_in: Duration,
        attempt: u32,
    },
}

/// Events since the last batch, oldest first.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    pub events: Vec<Event>,
    /// `log` events: (app, line).
    pub logs: VecDeque<(String, String)>,
    /// Log lines dropped because more came than a batch keeps.
    pub logs_dropped: u64,
    /// Other events dropped (a batch keeps `BATCH_EVENT_CAP`).
    pub events_dropped: u64,
    /// Lines this GUI does not understand (a newer wardend's event types).
    pub unknown: u64,
}

impl Batch {
    pub fn push(&mut self, ev: Event) {
        match ev {
            Event::Log { app, line } => {
                if self.logs.len() >= BATCH_LOG_CAP {
                    self.logs.pop_front();
                    self.logs_dropped += 1;
                }
                self.logs.push_back((app, line));
            }
            // Only the newest status of each app, and the newest host metrics, matter.
            Event::Status { ref app, .. } => {
                let slot = self.events.iter_mut().rev().find(|e| matches!(e, Event::Status { app: a, .. } if a == app));
                match slot {
                    Some(s) => *s = ev,
                    None => self.push_event(ev),
                }
            }
            Event::Host { .. } => match self.events.iter_mut().rev().find(|e| matches!(e, Event::Host { .. })) {
                Some(s) => *s = ev,
                None => self.push_event(ev),
            },
            ev => self.push_event(ev),
        }
    }

    fn push_event(&mut self, ev: Event) {
        if self.events.len() >= BATCH_EVENT_CAP {
            self.events_dropped += 1;
        } else {
            self.events.push(ev);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.logs.is_empty()
            && self.logs_dropped == 0
            && self.events_dropped == 0
            && self.unknown == 0
    }
}

/// Reconnect delays: quick at first (wardend restarting), then every 5 s.
#[derive(Debug, Clone, Default)]
pub struct Backoff {
    pub attempt: u32,
}

impl Backoff {
    const STEPS_MS: [u64; 6] = [250, 500, 1000, 2000, 4000, 5000];

    pub fn next_delay(&mut self) -> Duration {
        let i = (self.attempt as usize).min(Self::STEPS_MS.len() - 1);
        self.attempt = self.attempt.saturating_add(1);
        Duration::from_millis(Self::STEPS_MS[i])
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// Why connecting failed, and whether that means wardend is not running.
pub fn connect_error(path: &Path, e: &std::io::Error) -> (String, bool) {
    use std::io::ErrorKind::*;
    if path.as_os_str().len() > 100 {
        return (
            format!(
                "cannot connect to {}: the path is {} bytes long, and Unix sockets allow about 100. Set \
                 WARDEN_RUNTIME_DIR to a shorter directory (for wardend and the apps too)",
                path.display(),
                path.as_os_str().len()
            ),
            false,
        );
    }
    match e.kind() {
        NotFound => (format!("wardend is not running: there is no socket at {}", path.display()), true),
        ConnectionRefused => (
            format!(
                "wardend is not running: nothing answers on {} (left over from an earlier wardend)",
                path.display()
            ),
            true,
        ),
        PermissionDenied => (
            format!(
                "cannot open {}: permission denied. wardend runs as another user (root?): run the GUI as that user, \
                 or connect with SSH as that user",
                path.display()
            ),
            false,
        ),
        _ => (format!("cannot connect to wardend on {}: {e}", path.display()), false),
    }
}

async fn connect(path: &Path) -> Result<UnixStream, (String, bool)> {
    match tokio::time::timeout(CONNECT_TIMEOUT, UnixStream::connect(path)).await {
        Ok(Ok(s)) => Ok(s),
        Ok(Err(e)) => Err(connect_error(path, &e)),
        Err(_) => Err((
            format!(
                "wardend did not accept the connection on {} within {} s: it may be stuck (`warden daemon status`)",
                path.display(),
                CONNECT_TIMEOUT.as_secs()
            ),
            false,
        )),
    }
}

async fn send_line(w: &mut tokio::net::unix::OwnedWriteHalf, req: &DaemonRequest) -> Result<(), String> {
    let mut line = serde_json::to_vec(req).map_err(|e| format!("cannot encode the request: {e}"))?;
    line.push(b'\n');
    match tokio::time::timeout(CONNECT_TIMEOUT, w.write_all(&line)).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(format!("sending the request to wardend failed: {e}")),
        Err(_) => Err("wardend did not read the request within 5 s: it may be stuck (`warden daemon status`)".into()),
    }
}

/// Read one line into `buf` (appending: a read cancelled by `select!` keeps
/// what it had). `Ok(false)`: EOF.
async fn read_line(r: &mut BufReader<tokio::net::unix::OwnedReadHalf>, buf: &mut Vec<u8>) -> Result<bool, String> {
    loop {
        let room = MAX_LINE.saturating_sub(buf.len()) as u64;
        if room == 0 {
            return Err(format!("wardend sent a line longer than {} MB; is it a wardend?", MAX_LINE >> 20));
        }
        let n = r.take(room).read_until(b'\n', buf).await.map_err(|e| format!("reading from wardend failed: {e}"))?;
        if buf.ends_with(b"\n") {
            return Ok(true);
        }
        if n == 0 {
            return Ok(false);
        }
    }
}

/// One request, one reply.
pub async fn daemon_request(socket: &Path, req: &DaemonRequest, limit: Duration) -> Result<DaemonReply, String> {
    let exchange = async {
        let s = connect(socket).await.map_err(|(e, not_running)| {
            if not_running { format!("{e}; start it with `warden daemon --background` (or Start wardend)") } else { e }
        })?;
        let (r, mut w) = s.into_split();
        send_line(&mut w, req).await?;
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        if !read_line(&mut reader, &mut buf).await? {
            return Err("wardend closed the connection without answering (it may be exiting); try again".into());
        }
        serde_json::from_slice::<DaemonReply>(&buf).map_err(|e| {
            format!("wardend's answer is not what this GUI expects ({e}); are wardend and the GUI the same version?")
        })
    };
    tokio::time::timeout(limit, exchange).await.map_err(|_| {
        format!("wardend did not answer within {} s; it may be stuck (`warden daemon status`)", limit.as_secs())
    })?
}

/// Send `request` to `app`'s supervisor through wardend. Ok: the
/// supervisor's answer (which may itself say `ok: false`).
pub async fn app_request(socket: &Path, app: &str, request: Request) -> Result<Response, String> {
    let req = DaemonRequest::App { app: app.to_string(), request };
    let reply = daemon_request(socket, &req, APP_TIMEOUT).await?;
    match reply.response {
        Some(r) => Ok(r),
        None => Err(reply.message.unwrap_or_else(|| "wardend refused the request without saying why".into())),
    }
}

/// Start an app's supervisor (`start` on wardend: also clears `gave_up`).
pub async fn start_app(socket: &Path, app: &str) -> Result<String, String> {
    let reply = daemon_request(socket, &DaemonRequest::Start { app: app.to_string() }, APP_TIMEOUT).await?;
    let msg = reply.message.unwrap_or_default();
    if reply.ok { Ok(msg) } else { Err(msg) }
}

/// wardend's resource history from `since_ms`, `step_s` per point. `app`:
/// that app and the host (`""`: the host only).
pub async fn history(socket: &Path, app: &str, since_ms: u64, step_s: u32) -> Result<ResourceHistory, String> {
    let req = DaemonRequest::History { app: Some(app.to_string()), since_ms: Some(since_ms), step_s: Some(step_s) };
    let reply = daemon_request(socket, &req, HISTORY_TIMEOUT).await?;
    match (reply.ok, reply.history, reply.message) {
        (true, Some(h), _) => Ok(h),
        (_, _, Some(m)) if m.contains("unknown variant") => {
            Err("this wardend keeps no history: it is older than this GUI. Update warden on that host, then restart \
             wardend (`warden daemon stop`, `warden daemon --background`)"
                .into())
        }
        (_, _, Some(m)) => Err(m),
        _ => Err("wardend answered without the history".into()),
    }
}

/// The last `lines` log lines of `app` (`logs` without `follow`).
pub async fn app_logs(socket: &Path, app: &str, lines: usize) -> Result<Vec<String>, String> {
    let req = DaemonRequest::App {
        app: app.to_string(),
        request: Request::Logs { lines, follow: false, worker: None, events: false, stream: None },
    };
    let fetch = async {
        let s = connect(socket).await.map_err(|(e, _)| e)?;
        let (r, mut w) = s.into_split();
        send_line(&mut w, &req).await?;
        let mut reader = BufReader::new(r);
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while read_line(&mut reader, &mut buf).await? {
            let line = String::from_utf8_lossy(&buf).trim_end_matches(['\n', '\r']).to_string();
            buf.clear();
            // wardend refuses with a reply line instead (unknown app, supervisor down).
            if out.is_empty()
                && line.starts_with('{')
                && let Ok(r) = serde_json::from_str::<DaemonReply>(&line)
                && !r.ok
            {
                return Err(r.message.unwrap_or_else(|| "wardend refused to read the logs".into()));
            }
            out.push(line);
            if out.len() > lines * 2 + 16 {
                break;
            }
        }
        Ok(out)
    };
    tokio::time::timeout(Duration::from_secs(10), fetch)
        .await
        .map_err(|_| format!("reading {app}'s recent logs took more than 10 s; the supervisor may be busy"))?
}

/// The live feed from wardend at `endpoint`, reconnecting forever. Dropping
/// the stream closes the connection (and the SSH tunnel).
///
/// The socket is read by a task of its own: iced polls a subscription's
/// stream only while the window takes its messages, and a reader that waits
/// for the window would make wardend drop it. Must run inside a tokio
/// runtime (iced's executor, or a test's).
pub fn feed(endpoint: Endpoint, opts: FeedOptions) -> impl Stream<Item = FeedMsg> + Send + 'static {
    iced::stream::channel(8, async move |mut out: mpsc::Sender<FeedMsg>| {
        let (tx, mut rx) = mpsc::channel(8);
        let _reader = AbortOnDrop(tokio::spawn(run_feed(endpoint, opts, tx)).abort_handle());
        while let Some(m) = rx.next().await {
            if out.send(m).await.is_err() {
                return;
            }
        }
    })
}

/// Stops the reader task (and with it the connection and the tunnel) when
/// the stream is dropped.
struct AbortOnDrop(tokio::task::AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

enum End {
    /// The UI went away.
    Closed,
    Failed {
        error: String,
        not_running: bool,
    },
}

async fn run_feed(endpoint: Endpoint, opts: FeedOptions, mut out: mpsc::Sender<FeedMsg>) {
    let mut backoff = Backoff::default();
    let mut tunnel: Option<ssh::Tunnel> = None;
    loop {
        let attempt = backoff.attempt + 1;
        let socket = match &endpoint {
            Endpoint::Socket(p) => p.clone(),
            Endpoint::Ssh(t) => {
                if !tunnel.as_mut().is_some_and(|t| t.is_alive()) {
                    tunnel = None;
                    let what = format!("opening an SSH tunnel to {}", t.dest);
                    if out.send(FeedMsg::Connecting { attempt, what }).await.is_err() {
                        return;
                    }
                    match ssh::Tunnel::open(t).await {
                        Ok(open) => tunnel = Some(open),
                        Err(error) => {
                            if !retry(&mut out, &mut backoff, error, false).await {
                                return;
                            }
                            continue;
                        }
                    }
                }
                match &tunnel {
                    Some(t) => t.local.clone(),
                    None => continue,
                }
            }
        };
        let what = format!("connecting to wardend on {}", endpoint.describe());
        if out.send(FeedMsg::Connecting { attempt, what }).await.is_err() {
            return;
        }
        let (error, not_running) = match session(&socket, &opts, &mut out, tunnel.as_mut(), &mut backoff).await {
            End::Closed => return,
            End::Failed { error, not_running } => (error, not_running),
        };
        if !retry(&mut out, &mut backoff, error, not_running).await {
            return;
        }
    }
}

/// Say why the connection is gone, then wait. False: the UI went away.
async fn retry(out: &mut mpsc::Sender<FeedMsg>, backoff: &mut Backoff, error: String, not_running: bool) -> bool {
    let retry_in = backoff.next_delay();
    let msg = FeedMsg::Disconnected { error, not_running, retry_in, attempt: backoff.attempt };
    if out.send(msg).await.is_err() {
        return false;
    }
    tokio::time::sleep(retry_in).await;
    true
}

async fn tunnel_exit(t: Option<&mut ssh::Tunnel>) -> String {
    match t {
        Some(t) => t.exited().await,
        None => std::future::pending().await,
    }
}

/// One `subscribe` connection, until it ends.
async fn session(
    socket: &Path,
    opts: &FeedOptions,
    out: &mut mpsc::Sender<FeedMsg>,
    mut tunnel: Option<&mut ssh::Tunnel>,
    backoff: &mut Backoff,
) -> End {
    let remote = tunnel.as_ref().map(|t| t.dest.clone());
    let stream = match connect(socket).await {
        Ok(s) => s,
        Err((error, not_running)) => return End::Failed { error, not_running },
    };
    let (r, mut w) = stream.into_split();
    let req =
        DaemonRequest::Subscribe { interval_ms: Some(opts.interval_ms), logs: opts.logs, apps: opts.apps.clone() };
    if let Err(error) = send_line(&mut w, &req).await {
        return End::Failed { error, not_running: false };
    }
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    let mut batch = Batch::default();
    let mut connected = false;
    let mut bye: Option<String> = None;
    let hello_by = Instant::now() + HELLO_TIMEOUT;
    let mut flush_at: Option<Instant> = None;
    loop {
        tokio::select! {
            got = read_line(&mut reader, &mut buf) => {
                let more = match got {
                    Ok(more) => more,
                    Err(error) => return End::Failed { error, not_running: false },
                };
                if !more {
                    if !batch.is_empty() && out.send(FeedMsg::Batch(std::mem::take(&mut batch))).await.is_err() {
                        return End::Closed;
                    }
                    let tail = match tunnel.as_mut() {
                        Some(t) => {
                            // Let ssh's reader catch its message about the failed channel.
                            tokio::time::sleep(Duration::from_millis(150)).await;
                            t.stderr_text().lines().last().unwrap_or("").to_string()
                        }
                        None => String::new(),
                    };
                    return End::Failed { error: ended(connected, bye.as_deref(), remote.as_deref(), &tail), not_running: !connected && remote.is_some() };
                }
                let line = std::mem::take(&mut buf);
                if !connected {
                    match serde_json::from_slice::<Event>(&line) {
                        Ok(ev) => {
                            connected = true;
                            backoff.reset();
                            if out.send(FeedMsg::Connected { socket: socket.to_path_buf() }).await.is_err() {
                                return End::Closed;
                            }
                            batch.push(ev);
                        }
                        Err(e) => {
                            let error = match serde_json::from_slice::<DaemonReply>(&line) {
                                Ok(r) if !r.ok => format!(
                                    "wardend refused the subscription: {}",
                                    r.message.unwrap_or_else(|| "no reason given".into())
                                ),
                                _ => format!(
                                    "wardend's first line is not what this GUI expects ({e}); are wardend and the \
                                     GUI the same version? (`warden --version`)"
                                ),
                            };
                            return End::Failed { error, not_running: false };
                        }
                    }
                } else {
                    match serde_json::from_slice::<Event>(&line) {
                        Ok(ev) => {
                            if let Event::Bye { app: None, reason } = &ev {
                                bye = Some(reason.clone());
                            }
                            batch.push(ev);
                        }
                        // A newer wardend's event type: skipped, as `warden events` does.
                        Err(_) => batch.unknown += 1,
                    }
                }
                if flush_at.is_none() {
                    flush_at = Some(Instant::now() + FLUSH_EVERY);
                }
            }
            _ = tokio::time::sleep_until(flush_at.unwrap_or(hello_by)), if flush_at.is_some() => {
                match out.try_send(FeedMsg::Batch(std::mem::take(&mut batch))) {
                    Ok(()) => flush_at = None,
                    Err(e) if e.is_full() => {
                        // The window is behind: keep collecting (bounded) and try again.
                        if let FeedMsg::Batch(b) = e.into_inner() {
                            batch = b;
                        }
                        flush_at = Some(Instant::now() + FLUSH_EVERY);
                    }
                    Err(_) => return End::Closed,
                }
            }
            _ = tokio::time::sleep_until(hello_by), if !connected => {
                return End::Failed {
                    error: format!(
                        "wardend accepted the connection on {} but sent nothing within {} s; it may be stuck \
                         (`warden daemon status`)",
                        socket.display(),
                        HELLO_TIMEOUT.as_secs()
                    ),
                    not_running: false,
                };
            }
            why = tunnel_exit(tunnel.as_deref_mut()) => {
                return End::Failed { error: why, not_running: false };
            }
        }
    }
}

/// Why a stream ended at EOF.
fn ended(connected: bool, bye: Option<&str>, remote: Option<&str>, ssh_said: &str) -> String {
    match (connected, bye, remote) {
        (true, Some(reason), _) => format!("wardend exited ({reason}); every app keeps running"),
        (true, None, None) => "wardend closed the connection without saying bye: it died or was killed. Apps keep \
                               running; `warden daemon --background` (or Start wardend) starts it again"
            .into(),
        (true, None, Some(dest)) => format!(
            "the connection to wardend on {dest} ended without a bye: wardend died there, or the SSH connection \
             dropped{}",
            if ssh_said.is_empty() { String::new() } else { format!(" (ssh: {ssh_said})") }
        ),
        (false, _, None) => "wardend closed the connection before answering; it may be exiting (`warden daemon \
                             status`)"
            .into(),
        (false, _, Some(dest)) => format!(
            "no answer from wardend through the SSH tunnel to {dest}{}: is it running there? Start it on that host \
             (`warden daemon --background`, or Start wardend), or fix the remote socket path",
            if ssh_said.is_empty() { String::new() } else { format!(" (ssh: {ssh_said})") }
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use warden_protocol::events::WorkerEvent;

    fn status(app: &str, ready: usize) -> Event {
        let mut s = crate::model::tests::status(app, 2);
        s.workers_ready = ready;
        Event::Status { app: app.into(), status: Box::new(s) }
    }

    #[test]
    fn batches_coalesce_statuses_and_host_and_bound_logs() {
        let mut b = Batch::default();
        assert!(b.is_empty());
        b.push(status("api", 0));
        b.push(Event::Worker {
            app: "api".into(),
            worker: 1,
            standby: None,
            event: WorkerEvent::Ready,
            pid: None,
            detail: None,
            at_ms: 0,
        });
        b.push(status("web", 1));
        b.push(status("api", 2));
        for i in 0..3 {
            b.push(Event::Host {
                cpu_percent: i as f64,
                mem_used_bytes: 0,
                mem_total_bytes: 0,
                load: [0.0; 3],
                at_ms: 0,
            });
        }
        assert_eq!(b.events.len(), 4, "{:?}", b.events);
        assert!(matches!(&b.events[0], Event::Status { app, status } if app == "api" && status.workers_ready == 2));
        assert!(matches!(b.events[3], Event::Host { cpu_percent, .. } if cpu_percent == 2.0));
        for i in 0..(BATCH_LOG_CAP + 5) {
            b.push(Event::Log { app: "api".into(), line: format!("l{i}") });
        }
        assert_eq!((b.logs.len(), b.logs_dropped), (BATCH_LOG_CAP, 5));
        assert_eq!(b.logs.front().map(|(_, l)| l.as_str()), Some("l5"));
        for _ in 0..BATCH_EVENT_CAP {
            b.push(Event::Lagged { app: None, dropped: 1 });
        }
        assert_eq!(b.events.len(), BATCH_EVENT_CAP);
        assert_eq!(b.events_dropped, 4);
    }

    #[test]
    fn backoff_grows_then_caps_and_resets() {
        let mut b = Backoff::default();
        let d: Vec<u64> = (0..8).map(|_| b.next_delay().as_millis() as u64).collect();
        assert_eq!(d, [250, 500, 1000, 2000, 4000, 5000, 5000, 5000]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_millis(250));
    }

    #[test]
    fn connect_errors_say_whether_wardend_runs() {
        let p = Path::new("/run/warden/wardend.sock");
        let (msg, not_running) = connect_error(p, &std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(not_running && msg.contains("not running") && msg.contains("/run/warden/wardend.sock"), "{msg}");
        assert!(connect_error(p, &std::io::Error::from(std::io::ErrorKind::ConnectionRefused)).1);
        let (msg, not_running) = connect_error(p, &std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        assert!(!not_running && msg.contains("another user"), "{msg}");
        let long = PathBuf::from(format!("/tmp/{}/wardend.sock", "x".repeat(100)));
        let (msg, _) = connect_error(&long, &std::io::Error::from(std::io::ErrorKind::InvalidInput));
        assert!(msg.contains("WARDEN_RUNTIME_DIR"), "{msg}");
    }

    #[test]
    fn end_of_stream_reasons() {
        assert!(ended(true, Some("SIGTERM"), None, "").contains("exited (SIGTERM)"));
        assert!(ended(true, None, None, "").contains("died or was killed"));
        let r = ended(false, None, Some("u@h"), "channel 2: open failed");
        assert!(r.contains("u@h") && r.contains("open failed") && r.contains("remote socket"), "{r}");
    }

    #[test]
    fn options() {
        assert_eq!(FeedOptions::all(), FeedOptions { interval_ms: 1000, logs: false, apps: vec![] });
        assert_eq!(FeedOptions::logs_of("api").apps, ["api"]);
        assert!(local_socket().ends_with("wardend.sock"));
    }
}

//! Watching one supervisor (docs/protocol.md, "Second-level supervision"):
//!
//! - its event stream (`subscribe`), or `status` every interval when it
//!   refuses `subscribe` (an older version, e.g. during an upgrade);
//! - its process, through a pidfd (`kill(pid, 0)` every second where
//!   pidfd_open is unavailable), so a death is seen at once even when its
//!   socket is gone;
//! - silence: no line for 3 intervals and no answer to `status` within 5 s
//!   is reported as unresponsive, never acted on.
//!
//! Lines are forwarded to the bus as the supervisor wrote them (they carry
//! `app`); only `status` is parsed in full, to keep the last one.
//! wardend and `warden events` (when wardend is not running) both use it.

use super::{Bus, Frame, Kind};
use crate::control::{Request, Response, Status};
use crate::events::Event;
use std::future::Future;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, watch};

/// Longest event line accepted from a supervisor (a status with many
/// workers, a 16 KB log line); longer means a broken peer.
const MAX_LINE: usize = 1024 * 1024;
/// How long any request to a supervisor may take.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);
/// A supervisor wardend started should answer on its socket by then.
const STARTUP_LIMIT: Duration = Duration::from_secs(15);
/// Silence (no line, no status) this many intervals long, plus a failed
/// `status`, makes a supervisor unresponsive.
const SILENT_INTERVALS: u32 = 3;

/// What to watch.
pub(crate) struct Spec {
    pub app: Arc<str>,
    pub socket: PathBuf,
    /// Tags every message, so the receiver can ignore a replaced watcher.
    pub epoch: u64,
    /// Known when wardend started the process itself.
    pub pid: Option<u32>,
    pub interval: Duration,
}

/// What a watcher reports (`app`, `epoch` identify it).
#[derive(Debug)]
pub(crate) enum WatchMsg {
    /// Reached: it answers, with this first status.
    Attached {
        app: Arc<str>,
        epoch: u64,
        pid: u32,
        streaming: bool,
        status: Box<Status>,
    },
    Status {
        app: Arc<str>,
        epoch: u64,
        status: Box<Status>,
    },
    /// The process exited. `on_purpose`: it said `bye` (or, polled, it
    /// removed its socket / was shutting down).
    Gone {
        app: Arc<str>,
        epoch: u64,
        pid: u32,
        on_purpose: bool,
        detail: String,
    },
    /// Never reached, and no pid to wait on: nothing is running there.
    Lost {
        app: Arc<str>,
        epoch: u64,
        detail: String,
    },
    Unresponsive {
        app: Arc<str>,
        epoch: u64,
        pid: Option<u32>,
        detail: String,
    },
    Responsive {
        app: Arc<str>,
        epoch: u64,
    },
    /// A worker or rollout event alerts are made of (`crashed`, `failed`,
    /// `unhealthy`, `hung`, `rollout_done`); it went to the bus as well.
    Event {
        app: Arc<str>,
        epoch: u64,
        event: Box<Event>,
    },
    /// wardend's own timer: restart after the backoff.
    RestartDue {
        app: Arc<str>,
        epoch: u64,
    },
    /// The watcher task panicked (a bug); it is gone.
    Ended {
        app: Arc<str>,
        epoch: u64,
        panic: String,
    },
}

impl WatchMsg {
    pub fn key(&self) -> (&Arc<str>, u64) {
        match self {
            WatchMsg::Attached { app, epoch, .. }
            | WatchMsg::Status { app, epoch, .. }
            | WatchMsg::Gone { app, epoch, .. }
            | WatchMsg::Lost { app, epoch, .. }
            | WatchMsg::Unresponsive { app, epoch, .. }
            | WatchMsg::Responsive { app, epoch }
            | WatchMsg::Event { app, epoch, .. }
            | WatchMsg::RestartDue { app, epoch }
            | WatchMsg::Ended { app, epoch, .. } => (app, *epoch),
        }
    }
}

// ------------------------------------------------------------- the process

/// Resolves when a process exits.
pub(crate) struct PidWatch {
    pub pid: u32,
    how: How,
}

enum How {
    Fd(AsyncFd<OwnedFd>),
    Poll(tokio::time::Interval),
    Gone,
}

fn poll_every_second() -> How {
    let mut t = tokio::time::interval(Duration::from_secs(1));
    t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    How::Poll(t)
}

impl PidWatch {
    pub fn open(pid: u32) -> PidWatch {
        #[cfg(debug_assertions)]
        if std::env::var_os("WARDEN_NO_PIDFD").is_some_and(|v| v == "1") {
            return PidWatch { pid, how: poll_every_second() };
        }
        let how = match crate::sys::pidfd_open(pid) {
            Ok(fd) => match AsyncFd::with_interest(fd, tokio::io::Interest::READABLE) {
                Ok(a) => How::Fd(a),
                Err(e) => {
                    crate::debug!("cannot poll a pidfd; checking the process every second", pid = pid, error = e);
                    poll_every_second()
                }
            },
            Err(e) if e.raw_os_error() == Some(libc::ESRCH) => How::Gone,
            // ENOSYS (Linux < 5.3), EPERM (seccomp), EMFILE: poll instead.
            Err(e) => {
                crate::debug!("pidfd_open is unavailable; checking the process every second", pid = pid, error = e);
                poll_every_second()
            }
        };
        PidWatch { pid, how }
    }

    /// Returns once the process has exited (at once if it already has).
    pub async fn exited(&mut self) {
        loop {
            match &mut self.how {
                How::Gone => return,
                How::Fd(a) => match a.readable().await {
                    // Readable = exited; it stays readable, so later calls return at once.
                    Ok(_) => return,
                    Err(_) => self.how = poll_every_second(),
                },
                How::Poll(t) => {
                    t.tick().await;
                    if !alive(self.pid) {
                        self.how = How::Gone;
                    }
                }
            }
        }
    }
}

/// kill(pid, 0): EPERM still means it exists. A zombie child counts as
/// alive until it is reaped (wardend reaps its children every 2 s).
fn alive(pid: u32) -> bool {
    let Ok(p) = i32::try_from(pid) else { return false };
    match crate::sys::kill(p, 0) {
        Ok(()) => true,
        Err(e) => e.raw_os_error() != Some(libc::ESRCH),
    }
}

// ------------------------------------------------------------ the socket

/// An open subscription. The write half stays open: closing it would tell
/// the supervisor this subscriber left.
pub(crate) struct Conn {
    reader: BufReader<OwnedReadHalf>,
    _writer: OwnedWriteHalf,
}

pub(crate) enum Opened {
    /// Streaming; the first status (and its line, as sent).
    Stream { conn: Conn, status: Box<Status>, line: Vec<u8> },
    /// It answered `subscribe` with an error: an older supervisor.
    Refused(String),
}

/// One line into `buf` (with its `\n`). `Ok(false)`: end of stream.
async fn read_line(reader: &mut BufReader<OwnedReadHalf>, buf: &mut Vec<u8>) -> Result<bool, String> {
    buf.clear();
    let n = (&mut *reader).take(MAX_LINE as u64).read_until(b'\n', buf).await.map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(false);
    }
    if buf.last() != Some(&b'\n') {
        if buf.len() >= MAX_LINE {
            return Err(format!("an event line longer than {} KB", MAX_LINE / 1024));
        }
        return Ok(false); // cut off by EOF
    }
    Ok(true)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum LineKind {
    Hello,
    Status,
    Bye,
    Log,
    Lagged,
    /// `worker` and `rollout_done`: forwarded, and looked at for alerts.
    Worker,
    RolloutDone,
    Other,
    Junk,
}

/// The `type` of an event line, without allocating.
fn classify(line: &[u8]) -> LineKind {
    #[derive(serde::Deserialize)]
    struct Envelope<'a> {
        #[serde(rename = "type", borrow)]
        kind: std::borrow::Cow<'a, str>,
    }
    match serde_json::from_slice::<Envelope>(line) {
        Ok(e) => match &*e.kind {
            "hello" => LineKind::Hello,
            "status" => LineKind::Status,
            "bye" => LineKind::Bye,
            "log" => LineKind::Log,
            "lagged" => LineKind::Lagged,
            "worker" => LineKind::Worker,
            "rollout_done" => LineKind::RolloutDone,
            _ => LineKind::Other,
        },
        Err(_) => LineKind::Junk,
    }
}

/// A worker or rollout event that alerts are made of, parsed.
fn alertable(line: &[u8]) -> Option<Box<Event>> {
    use crate::events::WorkerEvent as W;
    match serde_json::from_slice::<Event>(line).ok()? {
        ev @ Event::Worker { event: W::Crashed | W::Failed | W::Unhealthy | W::Hung, .. } => Some(Box::new(ev)),
        ev @ Event::RolloutDone { .. } => Some(Box::new(ev)),
        _ => None,
    }
}

fn parse_status(line: &[u8]) -> Option<Box<Status>> {
    match serde_json::from_slice::<Event>(line) {
        Ok(Event::Status { status, .. }) => Some(status),
        _ => None,
    }
}

/// Open a subscription: `hello`, then the status snapshot.
pub(crate) async fn subscribe(socket: PathBuf, interval: Duration, logs: bool) -> Result<Opened, String> {
    let deadline = tokio::time::Instant::now() + ANSWER_TIMEOUT;
    let late = || format!("no answer within {} s", ANSWER_TIMEOUT.as_secs());
    let stream = tokio::time::timeout_at(deadline, UnixStream::connect(&socket))
        .await
        .map_err(|_| late())?
        .map_err(|e| format!("cannot connect to {} ({e})", socket.display()))?;
    let (r, mut w) = stream.into_split();
    let req = Request::Subscribe { interval_ms: Some(interval.as_millis() as u64), logs };
    let mut text = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    text.push(b'\n');
    tokio::time::timeout_at(deadline, w.write_all(&text)).await.map_err(|_| late())?.map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(r);
    let mut buf = Vec::new();
    if !tokio::time::timeout_at(deadline, read_line(&mut reader, &mut buf)).await.map_err(|_| late())?? {
        return Err("it closed the connection without answering".into());
    }
    if classify(&buf) != LineKind::Hello {
        if let Ok(resp) = serde_json::from_slice::<Response>(&buf) {
            if !resp.ok {
                return Ok(Opened::Refused(resp.message.unwrap_or_default()));
            }
        }
        let shown = String::from_utf8_lossy(&buf[..buf.len().min(200)]).trim().to_string();
        return Err(format!("unexpected answer to subscribe: {shown}"));
    }
    // The snapshot follows `hello`; anything before it is covered by it.
    loop {
        if !tokio::time::timeout_at(deadline, read_line(&mut reader, &mut buf)).await.map_err(|_| late())?? {
            return Err("the stream ended before the status snapshot".into());
        }
        if classify(&buf) == LineKind::Status {
            if let Some(status) = parse_status(&buf) {
                return Ok(Opened::Stream { conn: Conn { reader, _writer: w }, status, line: buf });
            }
        }
    }
}

/// One `status` request (the polling fallback, and the silence probe).
pub(crate) async fn poll_status(socket: PathBuf) -> Result<Box<Status>, String> {
    let mut sink = std::io::sink();
    match tokio::time::timeout(ANSWER_TIMEOUT, crate::control::call(&socket, &Request::Status, &mut sink)).await {
        Ok(Ok(Some(r))) => r.status.map(Box::new).ok_or_else(|| r.message.unwrap_or_else(|| "no status".into())),
        Ok(Ok(None)) => Err("no answer".into()),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!("no answer to `status` within {} s", ANSWER_TIMEOUT.as_secs())),
    }
}

// --------------------------------------------------------------- watcher

enum Probe {
    Status(Result<Box<Status>, String>),
    Subscribed(Result<Opened, String>),
}

type ProbeFuture = Pin<Box<dyn Future<Output = Probe>>>;

enum Step {
    Exited,
    Line(Result<bool, String>),
    Tick,
    Probe(Probe),
    Logs(bool),
}

struct Watcher {
    spec: Spec,
    tx: mpsc::Sender<WatchMsg>,
    bus: Bus,
    logs_bus: Bus,
    logs: watch::Receiver<bool>,
    buf: Vec<u8>,
    /// Subscription (true) or `status` polling (false).
    streaming: bool,
    bye: Option<String>,
    /// The last status said it was shutting down.
    shutting_down: bool,
    last_heard: Instant,
    unresponsive: bool,
    /// Whether the open subscription carries log lines.
    subscribed_logs: bool,
    /// The receiver is gone: stop.
    closed: bool,
}

/// Watch one supervisor until its process exits (`Gone`) or, when no pid
/// is known, until it cannot be reached (`Lost`).
pub(crate) async fn run(spec: Spec, tx: mpsc::Sender<WatchMsg>, bus: Bus, logs_bus: Bus, logs: watch::Receiver<bool>) {
    let mut w = Watcher {
        spec,
        tx,
        bus,
        logs_bus,
        logs,
        buf: Vec::new(),
        streaming: true,
        bye: None,
        shutting_down: false,
        last_heard: Instant::now(),
        unresponsive: false,
        subscribed_logs: false,
        closed: false,
    };
    w.main().await;
}

impl Watcher {
    async fn send(&mut self, m: WatchMsg) {
        if self.tx.send(m).await.is_err() {
            self.closed = true;
        }
    }

    fn app(&self) -> Arc<str> {
        self.spec.app.clone()
    }

    fn silent(&self) -> bool {
        self.last_heard.elapsed() >= self.spec.interval * SILENT_INTERVALS
    }

    /// Forward the line in `buf` as it is.
    fn forward(&self, kind: Kind) {
        let bus = if kind == Kind::Log { &self.logs_bus } else { &self.bus };
        if bus.receiver_count() == 0 {
            return;
        }
        if let Ok(text) = std::str::from_utf8(&self.buf) {
            let _ = bus.send(Frame { app: Some(self.spec.app.clone()), kind, line: Arc::from(text) });
        }
    }

    /// A status we polled (no line to forward): serialize one.
    fn forward_status(&self, status: Box<Status>) -> Box<Status> {
        if self.bus.receiver_count() == 0 {
            return status;
        }
        let ev = Event::Status { app: self.spec.app.to_string(), status };
        super::emit(&self.bus, Some(&self.spec.app), Kind::Status, &ev);
        match ev {
            Event::Status { status, .. } => status,
            _ => unreachable!("built as a status above"),
        }
    }

    async fn heard(&mut self) {
        self.last_heard = Instant::now();
        if self.unresponsive {
            self.unresponsive = false;
            let m = WatchMsg::Responsive { app: self.app(), epoch: self.spec.epoch };
            self.send(m).await;
        }
    }

    async fn status(&mut self, status: Box<Status>) {
        self.shutting_down = status.shutting_down;
        let m = WatchMsg::Status { app: self.app(), epoch: self.spec.epoch, status };
        self.send(m).await;
    }

    /// The line in `buf`, from the stream.
    async fn on_line(&mut self) {
        self.heard().await;
        match classify(&self.buf) {
            LineKind::Hello | LineKind::Junk => {}
            LineKind::Bye => {
                let reason = match serde_json::from_slice::<Event>(&self.buf) {
                    Ok(Event::Bye { reason, .. }) => reason,
                    _ => String::new(),
                };
                self.bye = Some(reason);
            }
            LineKind::Status => {
                if let Some(status) = parse_status(&self.buf) {
                    self.forward(Kind::Status);
                    self.status(status).await;
                }
            }
            LineKind::Log => self.forward(Kind::Log),
            LineKind::Lagged => match serde_json::from_slice::<Event>(&self.buf) {
                // Say whose stream lagged.
                Ok(Event::Lagged { app: None, dropped }) => {
                    let ev = Event::Lagged { app: Some(self.spec.app.to_string()), dropped };
                    super::emit(&self.bus, Some(&self.spec.app), Kind::Other, &ev);
                }
                _ => self.forward(Kind::Other),
            },
            LineKind::Other => self.forward(Kind::Other),
            LineKind::Worker | LineKind::RolloutDone => {
                self.forward(Kind::Other);
                if let Some(event) = alertable(&self.buf) {
                    let m = WatchMsg::Event { app: self.app(), epoch: self.spec.epoch, event };
                    self.send(m).await;
                }
            }
        }
    }

    /// Reach the supervisor: subscribe, or poll if it refuses.
    async fn reach(&mut self) -> Result<(Option<Conn>, Box<Status>), String> {
        let logs = *self.logs.borrow_and_update();
        match subscribe(self.spec.socket.clone(), self.spec.interval, logs).await? {
            Opened::Stream { conn, status, line } => {
                self.streaming = true;
                self.subscribed_logs = logs;
                self.buf = line;
                self.forward(Kind::Status);
                Ok((Some(conn), status))
            }
            Opened::Refused(why) => {
                crate::debug!(
                    "supervisor refused subscribe (an older version?); polling its status instead",
                    app = self.spec.app,
                    answer = why,
                );
                self.streaming = false;
                let status = poll_status(self.spec.socket.clone()).await?;
                Ok((None, self.forward_status(status)))
            }
        }
    }

    async fn main(&mut self) {
        let mut pid_watch = self.spec.pid.map(PidWatch::open);
        let t0 = Instant::now();
        let mut reported = false;
        let (mut conn, status) = loop {
            let err = match self.reach().await {
                Ok(v) => break v,
                Err(e) => e,
            };
            let Some(pw) = pid_watch.as_mut() else {
                let m = WatchMsg::Lost { app: self.app(), epoch: self.spec.epoch, detail: err };
                return self.send(m).await;
            };
            // Started by wardend: wait for its socket while it lives.
            tokio::select! {
                _ = pw.exited() => {
                    let pid = pw.pid;
                    let detail = format!("while starting, before it answered on its control socket ({err})");
                    let m = WatchMsg::Gone { app: self.app(), epoch: self.spec.epoch, pid, on_purpose: false, detail };
                    return self.send(m).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
            if !reported && t0.elapsed() >= STARTUP_LIMIT {
                reported = true;
                self.unresponsive = true;
                let detail =
                    format!("no answer on its control socket {} s after it started ({err})", STARTUP_LIMIT.as_secs());
                let m = WatchMsg::Unresponsive { app: self.app(), epoch: self.spec.epoch, pid: self.spec.pid, detail };
                self.send(m).await;
            }
            if self.closed {
                return;
            }
        };
        let pid = status.pid;
        let mut pw = match pid_watch {
            Some(p) if p.pid == pid => p,
            _ => PidWatch::open(pid),
        };
        self.shutting_down = status.shutting_down;
        self.heard().await;
        let streaming = self.streaming;
        self.send(WatchMsg::Attached { app: self.app(), epoch: self.spec.epoch, pid, streaming, status }).await;

        let mut tick = tokio::time::interval(self.spec.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // the first tick is immediate
        let mut probe: Option<ProbeFuture> = None;
        // The log flag a subscribe in flight asked for.
        let mut probe_logs = false;
        let mut logs_open = true;
        while !self.closed {
            let step = {
                let buf = &mut self.buf;
                let logs = &mut self.logs;
                tokio::select! {
                    _ = pw.exited() => Step::Exited,
                    r = async {
                        match conn.as_mut() {
                            Some(c) => read_line(&mut c.reader, buf).await,
                            None => std::future::pending().await,
                        }
                    } => Step::Line(r),
                    _ = tick.tick() => Step::Tick,
                    p = async {
                        match probe.as_mut() {
                            Some(f) => f.await,
                            None => std::future::pending().await,
                        }
                    } => Step::Probe(p),
                    r = logs.changed(), if logs_open => Step::Logs(r.is_ok()),
                }
            };
            match step {
                Step::Exited => return self.exited(pid, conn.as_mut()).await,
                Step::Line(Ok(true)) => self.on_line().await,
                Step::Line(r) => {
                    // End of stream (or a broken one). If it died, the pid
                    // watch fires in a moment; if not, the next tick
                    // subscribes again.
                    if let Err(e) = r {
                        crate::debug!("dropping a broken event stream", app = self.spec.app, error = e);
                    }
                    conn = None;
                }
                Step::Tick => {
                    if probe.is_some() {
                        continue;
                    }
                    let want_logs = *self.logs.borrow();
                    if self.streaming && conn.is_some() && want_logs != self.subscribed_logs {
                        // Log lines wanted (or no longer): subscribe again.
                        conn = None;
                    }
                    if !self.streaming {
                        probe = Some(Box::pin(status_probe(self.spec.socket.clone())));
                    } else if conn.is_none() && self.bye.is_none() {
                        probe_logs = want_logs;
                        probe =
                            Some(Box::pin(subscribe_probe(self.spec.socket.clone(), self.spec.interval, want_logs)));
                    } else if conn.is_some() && self.silent() {
                        probe = Some(Box::pin(status_probe(self.spec.socket.clone())));
                    }
                }
                Step::Probe(p) => {
                    probe = None;
                    match p {
                        Probe::Status(Ok(status)) => {
                            self.heard().await;
                            let status = self.forward_status(status);
                            self.status(status).await;
                            // Silent stream, answering socket: the stream is broken.
                            if self.streaming && conn.is_some() {
                                conn = None;
                            }
                        }
                        Probe::Subscribed(Ok(Opened::Stream { conn: c, status, line })) => {
                            conn = Some(c);
                            self.subscribed_logs = probe_logs;
                            self.heard().await;
                            self.buf = line;
                            self.forward(Kind::Status);
                            self.status(status).await;
                        }
                        Probe::Subscribed(Ok(Opened::Refused(_))) => self.streaming = false,
                        Probe::Status(Err(e)) | Probe::Subscribed(Err(e)) => {
                            if self.silent() && !self.unresponsive {
                                self.unresponsive = true;
                                let m = WatchMsg::Unresponsive {
                                    app: self.app(),
                                    epoch: self.spec.epoch,
                                    pid: Some(pid),
                                    detail: e,
                                };
                                self.send(m).await;
                            }
                        }
                    }
                }
                Step::Logs(false) => logs_open = false,
                Step::Logs(true) => {
                    // Mark it seen; the next tick subscribes again with or
                    // without log lines (older supervisors have none to give).
                    self.logs.borrow_and_update();
                }
            }
        }
    }

    /// The process is gone: was it on purpose?
    async fn exited(&mut self, pid: u32, conn: Option<&mut Conn>) {
        // Lines still buffered may hold the `bye` (and the last worker events).
        if let Some(c) = conn {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
            while let Ok(Ok(true)) = tokio::time::timeout_at(deadline, read_line(&mut c.reader, &mut self.buf)).await {
                self.on_line().await;
            }
        }
        let socket_left = self.spec.socket.exists();
        let (on_purpose, detail) = match (&self.bye, self.streaming) {
            (Some(reason), _) => (true, format!("on request ({})", if reason.is_empty() { "bye" } else { reason })),
            // No `bye` seen: a supervisor that exits on purpose still removes
            // its socket as its very last step; a crashed one leaves it
            // behind. The `bye` can be missed when wardend was stalled
            // (SIGSTOP, overload) past the supervisor's short flush window:
            // restarting an app the operator just stopped would be worse.
            (None, _) if !socket_left => (true, "on request (it removed its control socket)".to_string()),
            (None, true) => (false, "without `bye`: a crash, an OOM kill or kill -9".to_string()),
            (None, false) if self.shutting_down => (true, "on request (it was shutting down)".to_string()),
            (None, false) => {
                (false, "without removing its control socket: a crash, an OOM kill or kill -9".to_string())
            }
        };
        let m = WatchMsg::Gone { app: self.app(), epoch: self.spec.epoch, pid, on_purpose, detail };
        self.send(m).await;
    }
}

async fn status_probe(socket: PathBuf) -> Probe {
    Probe::Status(poll_status(socket).await)
}

async fn subscribe_probe(socket: PathBuf, interval: Duration, logs: bool) -> Probe {
    Probe::Subscribed(subscribe(socket, interval, logs).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncBufReadExt;
    use tokio::net::UnixListener;

    fn status(pid: u32, shutting_down: bool) -> Status {
        let mut s = super::super::policy::tests::status(false, shutting_down);
        s.pid = pid;
        s
    }

    fn line(ev: &Event) -> String {
        serde_json::to_string(ev).unwrap() + "\n"
    }

    struct Setup {
        dir: PathBuf,
        socket: PathBuf,
        child: std::process::Child,
        rx: mpsc::Receiver<WatchMsg>,
        frames: tokio::sync::broadcast::Receiver<Frame>,
        task: tokio::task::JoinHandle<()>,
        _logs: watch::Sender<bool>,
    }

    /// A fake supervisor process (`sleep`) whose pid the fake socket reports.
    fn setup(name: &str, serve: impl FnOnce(UnixListener, u32) -> Pin<Box<dyn Future<Output = ()>>>) -> Setup {
        let dir = std::env::temp_dir().join(format!("wardend-watch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("control.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        tokio::task::spawn_local(serve(listener, pid));
        let (tx, rx) = mpsc::channel(64);
        let (bus, frames) = tokio::sync::broadcast::channel(64);
        let (logs_bus, _) = tokio::sync::broadcast::channel(64);
        let (logs_tx, logs) = watch::channel(false);
        let spec = Spec {
            app: "api".into(),
            socket: socket.clone(),
            epoch: 7,
            pid: None,
            interval: Duration::from_millis(250),
        };
        let task = tokio::task::spawn_local(run(spec, tx, bus, logs_bus, logs));
        Setup { dir, socket, child, rx, frames, task, _logs: logs_tx }
    }

    impl Setup {
        async fn next(&mut self) -> WatchMsg {
            tokio::time::timeout(Duration::from_secs(10), self.rx.recv()).await.expect("a message in time").unwrap()
        }

        /// Kill the fake supervisor's process and wait for `Gone`.
        async fn kill_and_wait_gone(&mut self) -> (bool, String) {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
            loop {
                match self.next().await {
                    WatchMsg::Gone { on_purpose, detail, epoch, .. } => {
                        assert_eq!(epoch, 7);
                        return (on_purpose, detail);
                    }
                    _ => continue,
                }
            }
        }
    }

    impl Drop for Setup {
        fn drop(&mut self) {
            self.task.abort();
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Speaks the protocol: hello, status, a worker event, optionally bye;
    /// then closes the stream (EOF) and stops answering.
    fn streaming(bye: bool) -> impl FnOnce(UnixListener, u32) -> Pin<Box<dyn Future<Output = ()>>> {
        move |l, pid| {
            Box::pin(async move {
                let (s, _) = l.accept().await.unwrap();
                let (r, mut w) = s.into_split();
                let mut req = String::new();
                BufReader::new(r).read_line(&mut req).await.unwrap();
                assert!(req.contains(r#""cmd":"subscribe""#), "{req}");
                let mut out = line(&Event::Hello { protocol: 1, app: Some("api".into()), pid, version: "0".into() });
                out += &line(&Event::Status { app: "api".into(), status: Box::new(status(pid, false)) });
                out += r#"{"type":"worker","app":"api","worker":2,"event":"crashed","pid":9,"detail":"exit code 1","at_ms":5}"#;
                out += "\n";
                if bye {
                    out += &line(&Event::Bye { app: Some("api".into()), reason: "shutdown".into() });
                }
                w.write_all(out.as_bytes()).await.unwrap();
                // Returning drops the stream and the listener: EOF, then
                // nothing answers any more (the process lives on).
            })
        }
    }

    async fn attached(s: &mut Setup) -> (u32, bool) {
        match s.next().await {
            WatchMsg::Attached { pid, streaming, status, epoch, .. } => {
                assert_eq!((epoch, status.pid), (7, pid));
                (pid, streaming)
            }
            other => panic!("expected Attached, got {other:?}"),
        }
    }

    fn local<F: Future>(f: F) -> F::Output {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        tokio::task::LocalSet::new().block_on(&rt, f)
    }

    #[test]
    fn stream_events_are_forwarded_and_bye_means_exited() {
        local(async {
            let mut s = setup("bye", streaming(true));
            let (pid, streaming) = attached(&mut s).await;
            assert_eq!((pid, streaming), (s.child.id(), true));
            // The snapshot status and the worker event reach the bus as sent.
            let mut kinds = Vec::new();
            while kinds.len() < 2 {
                let f = tokio::time::timeout(Duration::from_secs(5), s.frames.recv()).await.unwrap().unwrap();
                assert_eq!(f.app.as_deref(), Some("api"));
                kinds.push((f.kind, f.line.to_string()));
            }
            assert_eq!(kinds[0].0, Kind::Status);
            assert!(kinds[1].1.starts_with(r#"{"type":"worker","app":"api","worker":2"#), "{kinds:?}");
            assert!(kinds[1].1.ends_with('\n'));
            // The crash also reaches wardend's core, for alerts.
            match s.next().await {
                WatchMsg::Event { event, epoch: 7, .. } => {
                    assert!(matches!(*event, Event::Worker { worker: 2, .. }), "{event:?}")
                }
                other => panic!("expected the crash, got {other:?}"),
            }
            let (on_purpose, detail) = s.kill_and_wait_gone().await;
            assert!(on_purpose, "bye seen: {detail}");
            assert!(detail.contains("shutdown"), "{detail}");
        });
    }

    #[test]
    fn eof_without_bye_means_died() {
        local(async {
            let mut s = setup("died", streaming(false));
            attached(&mut s).await;
            let (on_purpose, detail) = s.kill_and_wait_gone().await;
            assert!(!on_purpose, "{detail}");
            assert!(detail.contains("without `bye`"), "{detail}");
        });
    }

    #[test]
    fn missed_bye_but_socket_removed_means_exited() {
        // Review R2: wardend stalled past the supervisor's bye flush, then
        // the supervisor removed its socket on the way out: on purpose.
        local(async {
            let mut s = setup("missed-bye", streaming(false));
            attached(&mut s).await;
            std::fs::remove_file(&s.socket).unwrap();
            let (on_purpose, detail) = s.kill_and_wait_gone().await;
            assert!(on_purpose, "{detail}");
            assert!(detail.contains("removed its control socket"), "{detail}");
        });
    }

    /// An older supervisor: `subscribe` is an error, `status` works.
    fn old_supervisor(l: UnixListener, pid: u32) -> Pin<Box<dyn Future<Output = ()>>> {
        Box::pin(async move {
            loop {
                let (s, _) = l.accept().await.unwrap();
                let (r, mut w) = s.into_split();
                let mut req = String::new();
                BufReader::new(r).read_line(&mut req).await.unwrap();
                let resp = if req.contains("subscribe") {
                    Response::err("subscribe is not supported by this Warden version")
                } else {
                    assert!(req.contains(r#""cmd":"status""#), "{req}");
                    Response { status: Some(status(pid, false)), ..Response::ok("") }
                };
                let text = serde_json::to_string(&resp).unwrap() + "\n";
                w.write_all(text.as_bytes()).await.unwrap();
            }
        })
    }

    #[test]
    fn refused_subscribe_falls_back_to_polling() {
        local(async {
            let mut s = setup("poll", old_supervisor);
            let (pid, streaming) = attached(&mut s).await;
            assert_eq!((pid, streaming), (s.child.id(), false));
            // Polled every interval (250 ms here).
            for _ in 0..2 {
                match s.next().await {
                    WatchMsg::Status { status, .. } => assert_eq!(status.pid, pid),
                    other => panic!("expected a polled status, got {other:?}"),
                }
            }
            // It died and left its socket behind: died, not exited.
            let (on_purpose, detail) = s.kill_and_wait_gone().await;
            assert!(!on_purpose, "{detail}");
        });
    }

    #[test]
    fn polled_supervisor_that_removed_its_socket_exited_on_purpose() {
        local(async {
            let mut s = setup("poll-exit", old_supervisor);
            attached(&mut s).await;
            std::fs::remove_file(&s.socket).unwrap();
            let (on_purpose, detail) = s.kill_and_wait_gone().await;
            assert!(on_purpose, "{detail}");
        });
    }

    #[test]
    fn nothing_listening_is_lost() {
        local(async {
            let (tx, mut rx) = mpsc::channel(4);
            let (bus, _) = tokio::sync::broadcast::channel(4);
            let (logs_bus, _) = tokio::sync::broadcast::channel(4);
            let (_keep, logs) = watch::channel(false);
            let spec = Spec {
                app: "ghost".into(),
                socket: std::env::temp_dir().join("wardend-watch-nothing-here.sock"),
                epoch: 1,
                pid: None,
                interval: Duration::from_millis(250),
            };
            run(spec, tx, bus, logs_bus, logs).await;
            assert!(matches!(rx.recv().await, Some(WatchMsg::Lost { .. })));
        });
    }

    #[test]
    fn pid_watch_sees_an_exit() {
        local(async {
            let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
            let mut w = PidWatch::open(child.id());
            assert!(tokio::time::timeout(Duration::from_millis(200), w.exited()).await.is_err(), "still running");
            child.kill().unwrap();
            child.wait().unwrap();
            tokio::time::timeout(Duration::from_secs(3), w.exited()).await.expect("exit seen");
            // A pid that does not exist.
            let mut gone = PidWatch::open(u32::MAX / 2);
            tokio::time::timeout(Duration::from_secs(3), gone.exited()).await.expect("no such process");
            assert!(!alive(u32::MAX / 2));
        });
    }

    #[test]
    fn only_alertable_events_go_to_the_core() {
        let line = |e: &str| format!(r#"{{"type":"worker","app":"a","worker":1,"event":"{e}","at_ms":1}}"#);
        for e in ["crashed", "failed", "unhealthy", "hung"] {
            assert!(alertable(line(e).as_bytes()).is_some(), "{e}");
        }
        for e in ["ready", "starting", "stopping", "restarting"] {
            assert!(alertable(line(e).as_bytes()).is_none(), "{e}");
        }
        let done = r#"{"type":"rollout_done","app":"a","outcome":{"seq":1,"kind":"reload","ok":false,"message":"x","duration_secs":1.0}}"#;
        assert!(alertable(done.as_bytes()).is_some());
        assert!(alertable(br#"{"type":"worker""#).is_none(), "junk");
    }

    #[test]
    fn classifies_lines() {
        assert_eq!(classify(br#"{"type":"status","app":"a"}"#), LineKind::Status);
        assert_eq!(classify(br#"{"app":"a","type":"bye","reason":"x"}"#), LineKind::Bye);
        assert_eq!(classify(br#"{"type":"rollout_done"}"#), LineKind::RolloutDone);
        assert_eq!(classify(br#"{"type":"rollout"}"#), LineKind::Other);
        assert_eq!(classify(br#"{"type":"worker","app":"a"}"#), LineKind::Worker);
        assert_eq!(classify(br#"{"type":"log"}"#), LineKind::Log, "escapes are fine");
        assert_eq!(classify(b"not json"), LineKind::Junk);
        assert_eq!(classify(br#"{"ok":false}"#), LineKind::Junk);
    }
}

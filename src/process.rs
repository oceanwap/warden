//! Spawning and owning one OS process: stdout/stderr capture, the fd-3 IPC
//! socket used by the shim, signal delivery, and exit reporting.
//!
//! Each process is owned by a task that holds its `Child`. Signals go through
//! that task, so a signal is only ever sent while the child is still unreaped
//! and its pid cannot have been reused.

pub mod exit;

use serde::Deserialize;
use std::cell::RefCell;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Stdio;
use std::rc::Rc;
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
    /// Where the worker's stdout and stderr go (`[logging] worker_output`).
    pub output: Output,
    /// Output lines kept per second per stream (0 = no limit; capture only).
    pub max_lines_per_sec: u32,
}

/// `[logging] worker_output`.
#[derive(Debug, Clone)]
pub enum Output {
    /// Pipes read by Warden: lines prefixed, rate-limited, kept for
    /// `warden logs`, copied to stdout/journald and the log files.
    Capture,
    /// Straight to Warden's stdout/stderr (no prefix, no copy through
    /// Warden; `warden logs` won't show it).
    Inherit,
    /// Pipes spliced byte for byte into files, rotated at line boundaries.
    Direct(crate::logging::DirectFiles),
}

impl Output {
    pub fn from_config(l: &crate::config::Logging) -> Output {
        use crate::config::WorkerOutput;
        match (l.worker_output, &l.out_file) {
            (WorkerOutput::Inherit, _) => Output::Inherit,
            (WorkerOutput::Direct, Some(out)) => Output::Direct(crate::logging::DirectFiles {
                out: out.clone(),
                err: l.err_file.clone(),
                per_worker: l.per_worker_files,
                policy: crate::logging::RotatePolicy::from(&l.rotate),
            }),
            // Direct without out_file is refused by config validation.
            (WorkerOutput::Direct, None) | (WorkerOutput::Capture, _) => Output::Capture,
        }
    }
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
    /// `long_lived_closed`: WebSockets closed with 1001, SSE streams ended.
    #[serde(default)]
    pub ws: Option<u64>,
    #[serde(default)]
    pub sse: Option<u64>,
}

#[derive(Debug)]
pub enum ProcEvent {
    /// `note` explains exits Warden didn't observe normally (lost track of it).
    /// `sent`: the signals Warden delivered while the process was alive
    /// (who killed it: see `exit::classify`).
    Exited {
        inst: u64,
        code: Option<i32>,
        signal: Option<i32>,
        note: Option<String>,
        sent: exit::Sent,
    },
    Ipc {
        inst: u64,
        msg: IpcMsg,
    },
}

/// The `worker=` label on a process's captured output. Shared with its
/// output readers, so a promoted standby's lines carry the slot it took.
#[derive(Clone)]
pub struct Label(Rc<RefCell<Rc<str>>>);

impl Label {
    fn new(s: &str) -> Label {
        Label(Rc::new(RefCell::new(s.into())))
    }
    fn get(&self) -> Rc<str> {
        self.0.borrow().clone()
    }
}

/// Handle to a running process. Dropping it does not kill the process.
pub struct Handle {
    pub pid: u32,
    ctl: mpsc::UnboundedSender<(i32, bool)>,
    /// Warden's end of the fd-3 socket (a non-blocking duplicate of the one
    /// the IPC reader owns), for messages to the worker (read by the shim
    /// only in a standby).
    ipc: Option<std::os::unix::net::UnixStream>,
    label: Label,
}

impl Handle {
    /// Send one line to the worker on fd 3 without waiting: the message is
    /// tiny and the socket buffer empty, so anything short of a whole write
    /// (the worker stopped reading, the channel is gone) is an error.
    pub fn send(&self, line: &[u8]) -> std::io::Result<()> {
        use std::os::fd::AsFd as _;
        let Some(ipc) = self.ipc.as_ref() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "no IPC channel to this worker"));
        };
        // A worker gone is EPIPE here, never a SIGPIPE.
        match crate::sys::send(ipc.as_fd(), line, false)? {
            n if n == line.len() => Ok(()),
            n => Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                format!("only {n} of {} bytes written", line.len()),
            )),
        }
    }

    /// Nothing more for the worker: shut down Warden's sending side of fd 3,
    /// so a read the worker has pending there returns (EOF) instead of
    /// waiting forever (a Node standby's read on a thread-pool thread held
    /// up its exit). Its messages to Warden still arrive.
    pub fn close_input(&self) {
        if let Some(ipc) = self.ipc.as_ref() {
            // NotConnected: the worker is gone already; nothing to end.
            let _ = ipc.shutdown(std::net::Shutdown::Write);
        }
    }

    /// From now on, label this process's captured output `label`.
    pub fn relabel(&self, label: &str) {
        *self.label.0.borrow_mut() = label.into();
    }

    /// SIGTERM and SIGKILL go to the whole process group (like supervisord's
    /// stopasgroup/killasgroup), so `bun run <script>` wrappers or helpers the
    /// app spawned are not orphaned. Other signals go to the process only.
    pub fn signal(&self, sig: i32) {
        let _ = self.ctl.send((sig, sig == libc::SIGKILL || sig == libc::SIGTERM));
    }

    /// The configured stop signal (SIGTERM or e.g. SIGINT): to the group.
    pub fn signal_group(&self, sig: i32) {
        let _ = self.ctl.send((sig, true));
    }
}

pub fn spawn(spec: Spec, inst: u64, events: mpsc::UnboundedSender<ProcEvent>) -> std::io::Result<Handle> {
    // fd 3: a socket, so Warden can also send to the worker (`Handle::send`).
    let (ipc_ours, ipc_child) = crate::sys::socketpair_cloexec()?;
    let child_fd = std::os::fd::AsRawFd::as_raw_fd(&ipc_child);

    let mut cmd = Command::new(&spec.program);
    // Direct mode: our own pipes (read end, file, stream), spliced into
    // the files on the output thread.
    let mut direct: Vec<(OwnedFd, PathBuf, &'static str)> = Vec::new();
    let (stdout, stderr) = match &spec.output {
        Output::Capture => (Stdio::piped(), Stdio::piped()),
        Output::Inherit => (Stdio::inherit(), Stdio::inherit()),
        Output::Direct(files) => {
            let (out_path, err_path) = files.paths(&spec.label);
            let (out_r, out_w) = pipe()?;
            direct.push((out_r, out_path, "stdout"));
            let stderr = match err_path {
                Some(err_path) => {
                    let (err_r, err_w) = pipe()?;
                    direct.push((err_r, err_path, "stderr"));
                    Stdio::from(err_w)
                }
                // No separate err_file: stderr shares stdout's pipe, like
                // `>out.log 2>&1`, so both keep the order they were written in.
                None => Stdio::from(out_w.try_clone()?),
            };
            (Stdio::from(out_w), stderr)
        }
    };
    cmd.args(&spec.args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
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
    // How the supervisor was launched is not the workers' business (a
    // worker running `warden` itself would misreport).
    cmd.env_remove(crate::events::LAUNCH_ENV);
    // SAFETY: the closure only calls the async-signal-safe helpers in
    // `sys` (dup2, fcntl, prctl): no allocation or locking after fork.
    #[allow(unsafe_code)]
    unsafe {
        cmd.pre_exec(move || {
            crate::sys::child_dup_ipc(child_fd, IPC_FD)?;
            // If Warden dies without cleaning up (SIGKILL), take the workers with it.
            crate::sys::child_parent_death_signal(libc::SIGTERM)
        });
    }
    let mut child = cmd.spawn()?;
    // Our copies of the pipes' write ends (in `cmd`) and of the worker's end
    // of the IPC socket: only the worker's remain, so its exit is EOF.
    drop(cmd);
    drop(ipc_child);
    let pid = child.id().unwrap_or(0);
    let label = spec.label.clone();
    let shared_label = Label::new(&label);
    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<(i32, bool)>();

    // Readers of the worker's output and IPC pipe. If one of them fails, the
    // worker could block on a full pipe or lose its readiness/heartbeat
    // channel, so it is killed and restarted rather than left half-supervised.
    let reader_failed = |what: &'static str, ctl: mpsc::UnboundedSender<(i32, bool)>, label: String| {
        move |msg: String| {
            crate::error!(
                "worker's output reader failed; killing the worker so it restarts cleanly",
                worker = label,
                reader = what,
                panic = msg,
                hint = "this is a Warden bug: please report it with the log lines above",
            );
            let _ = ctl.send((libc::SIGKILL, true));
        }
    };
    if let Some(out) = child.stdout.take().and_then(|o| output_receiver(o.into_owned_fd(), &label, "stdout")) {
        let on_fail = reader_failed("stdout", ctl_tx.clone(), label.clone());
        let fut = pump_output(out, shared_label.clone(), "stdout", spec.max_lines_per_sec);
        tokio::task::spawn_local(async move {
            if let Err(m) = crate::guard::catch_unwind(fut).await {
                on_fail(m);
            }
        });
    }
    if let Some(err) = child.stderr.take().and_then(|e| output_receiver(e.into_owned_fd(), &label, "stderr")) {
        let on_fail = reader_failed("stderr", ctl_tx.clone(), label.clone());
        let fut = pump_output(err, shared_label.clone(), "stderr", spec.max_lines_per_sec);
        tokio::task::spawn_local(async move {
            if let Err(m) = crate::guard::catch_unwind(fut).await {
                on_fail(m);
            }
        });
    }
    if let Output::Direct(files) = &spec.output {
        for (fd, path, stream) in direct {
            let on_fail = reader_failed(stream, ctl_tx.clone(), label.clone());
            start_direct(fd, path, files.policy.clone(), label.clone(), stream, on_fail);
        }
    }
    let mut ipc = None;
    match ipc_stream(ipc_ours) {
        Ok((rx, tx)) => {
            ipc = Some(tx);
            let on_fail = reader_failed("ipc", ctl_tx.clone(), label.clone());
            let fut = pump_ipc(rx, inst, events.clone(), label.clone());
            tokio::task::spawn_local(async move {
                if let Err(m) = crate::guard::catch_unwind(fut).await {
                    on_fail(m);
                }
            });
        }
        Err(e) => crate::warn!(
            "cannot use the worker's IPC socket; readiness falls back to /proc, the watchdog is off for it and it cannot be promoted from standby",
            worker = label,
            error = e,
            hint = "this is a Warden bug: please report it",
        ),
    }

    // Signals delivered, shared with the waiter's caller (it may panic).
    let sent = std::rc::Rc::new(std::cell::Cell::new(exit::Sent::default()));
    let sent_by_waiter = sent.clone();
    tokio::task::spawn_local(async move {
        let waited = crate::guard::catch_unwind(async move {
            crate::guard::fault("waiter");
            loop {
                tokio::select! {
                    st = child.wait() => break st,
                    Some((sig, to_group)) = ctl_rx.recv() => {
                        // child.id() is None once reaped; never signal a reused pid.
                        // The group contains the process itself: signal once.
                        if let Some(p) = child.id() {
                            crate::sys::signal_child(p, sig, to_group);
                            let mut s = sent_by_waiter.get();
                            s.add(sig);
                            sent_by_waiter.set(s);
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
                // Our still-unreaped child: its pid can't have been reused.
                crate::sys::signal_child(pid, libc::SIGKILL, true);
                crate::sys::signal_child(pid, libc::SIGKILL, false);
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
        if let Ok(p) = i32::try_from(pid) {
            if p > 1 {
                let _ = crate::sys::kill(-p, libc::SIGKILL);
            }
        }
        let _ = events.send(ProcEvent::Exited { inst, code, signal, note, sent: sent.get() });
    });

    Ok(Handle { pid, ctl: ctl_tx, ipc, label: shared_label })
}

/// Warden's end of the IPC socket, non-blocking: a tokio stream for the
/// reader, and a duplicate for `Handle::send`, which writes directly (no
/// reactor state involved: a send right after spawn must work too).
fn ipc_stream(fd: OwnedFd) -> std::io::Result<(tokio::net::UnixStream, std::os::unix::net::UnixStream)> {
    let s = std::os::unix::net::UnixStream::from(fd);
    s.set_nonblocking(true)?;
    let writer = s.try_clone()?;
    Ok((tokio::net::UnixStream::from_std(s)?, writer))
}

/// A worker's stdout/stderr pipe as a non-blocking receiver. If that fails
/// (it can't for a pipe we created), the pipe is closed: the worker gets
/// EPIPE on output instead of blocking forever on a pipe nobody reads.
fn output_receiver(
    fd: std::io::Result<OwnedFd>,
    label: &str,
    stream: &str,
) -> Option<tokio::net::unix::pipe::Receiver> {
    match fd.and_then(tokio::net::unix::pipe::Receiver::from_owned_fd) {
        Ok(rx) => Some(rx),
        Err(e) => {
            crate::error!(
                "cannot read the worker's output; it is discarded",
                worker = label,
                stream = stream,
                error = e,
                hint = "this is a Warden bug: please report it",
            );
            None
        }
    }
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    crate::sys::pipe_cloexec()
}

/// Forward a child stream line by line to the log, capping line length.
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
    /// Under budget there is no clock read at all; the window is only
    /// checked once the budget is used up (then a new window starts if a
    /// second has passed since the last one began).
    fn allow(&mut self) -> bool {
        self.used = self.used.saturating_add(1);
        if self.used <= self.limit {
            return true;
        }
        if self.window.elapsed() >= std::time::Duration::from_secs(1) {
            self.window = std::time::Instant::now();
            self.used = 1;
            return true;
        }
        self.dropped += 1;
        false
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

thread_local! {
    /// One read buffer for every output reader on this thread: readers
    /// borrow it only between `readable()` and the end of the synchronous
    /// read-and-split (never across an await), so 16 workers × stdout and
    /// stderr share 64 KB instead of holding 2 MB.
    static READ_BUF: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0u8; READ_CHUNK]);
}

/// Forward a child stream line by line to the log, capping line length and rate.
async fn pump_output(rx: tokio::net::unix::pipe::Receiver, label: Label, stream: &'static str, limit: u32) {
    crate::guard::fault(stream);
    let mut line: Vec<u8> = Vec::new();
    let mut rate = RateLimit::new(if limit == 0 { u32::MAX } else { limit });
    loop {
        if let Err(e) = rx.readable().await {
            crate::warn!("stopped reading worker output", worker = label.get(), stream = stream, error = e);
            break;
        }
        // Everything this read produced goes to the log as one batch.
        let mut batch = crate::logging::OutputBatch::new(&label.get(), stream);
        let read = READ_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            let n = rx.try_read(&mut buf)?;
            split_lines(&buf[..n], &mut line, &mut |l: &mut Vec<u8>| keep(&mut batch, l, &mut rate));
            Ok::<usize, std::io::Error>(n)
        });
        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                crate::warn!("stopped reading worker output", worker = label.get(), stream = stream, error = e);
                break;
            }
        }
        if limit == 0 {
            crate::logging::room_for(batch.len(), batch.bytes(), KEEP_ALL_MAX_WAIT).await;
        }
        crate::logging::worker_output_batch(batch);
        if line.capacity() > 4 * MAX_LINE {
            line.shrink_to(MAX_LINE); // after an unusually long partial line
        }
        if let Some(n) = rate.report() {
            crate::warn!(
                "worker writes too much output; lines dropped",
                worker = label.get(),
                stream = stream,
                dropped = n,
                limit_per_s = limit,
                hint = "lower the app's log level, raise [logging] max_lines_per_sec (0 = no limit), or set [logging] \
                        worker_output = \"inherit\" (straight to Warden's stdout, not in `warden logs`) or \"direct\" \
                        (into out_file, no budget)",
            );
        }
    }
    if !line.is_empty() {
        let mut batch = crate::logging::OutputBatch::new(&label.get(), stream);
        keep(&mut batch, &mut line, &mut rate);
        crate::logging::worker_output_batch(batch);
    }
}

/// `worker_output = "direct"`: pump one of the worker's pipes into its file
/// on the output thread. If the pump panics, the worker is killed so it
/// restarts (its writes would otherwise block on a pipe nobody reads).
fn start_direct(
    pipe: OwnedFd,
    path: PathBuf,
    policy: crate::logging::RotatePolicy,
    label: String,
    stream: &'static str,
    on_fail: impl FnOnce(String) + Send + 'static,
) {
    let active = crate::logging::DirectActive::begin();
    crate::logging::on_output_thread(Box::new(move || {
        tokio::task::spawn_local(async move {
            let _active = active;
            if let Err(m) = crate::guard::catch_unwind(pump_direct(pipe, path, policy, label, stream)).await {
                on_fail(m);
            }
        });
    }));
}

/// Move the pipe's bytes into the file as they arrive (splice: no parsing,
/// no copy through Warden) until EOF, i.e. until the worker and everything
/// that inherited its stdout are gone, so its last lines are drained.
async fn pump_direct(
    pipe: OwnedFd,
    path: PathBuf,
    policy: crate::logging::RotatePolicy,
    label: String,
    stream: &'static str,
) {
    use crate::logging::Step;
    crate::guard::fault(stream);
    let pipe = match tokio::net::unix::pipe::Receiver::from_owned_fd(pipe)
        .and_then(|rx| rx.into_nonblocking_fd())
        .and_then(|fd| tokio::io::unix::AsyncFd::new(std::fs::File::from(fd)))
    {
        Ok(p) => p,
        Err(e) => {
            crate::error!(
                "cannot read the worker's output; it is discarded",
                worker = label,
                stream = stream,
                error = e,
                hint = "this is a Warden bug: please report it",
            );
            return;
        }
    };
    let file = crate::logging::DirectWriter::open(path, policy, &label, stream);
    // `warden logs -f`: the unfinished line so far (only while someone follows).
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let mut ready = match pipe.readable().await {
            Ok(r) => r,
            Err(e) => {
                crate::warn!("stopped reading worker output", worker = label, stream = stream, error = e);
                break;
            }
        };
        let following = crate::logging::following();
        if !following {
            partial.clear();
        }
        let mut follow = |b: &[u8]| follow_direct(b, &mut partial, &label, stream);
        let echo: crate::logging::Echo = if following { Some(&mut follow) } else { None };
        match ready.try_io(|p| file.step(p.get_ref(), echo)) {
            // Empty for now: readiness cleared, wait for more.
            Err(_would_block) => {}
            Ok(Ok(Step::Moved(_))) => {}
            Ok(Ok(Step::Eof)) => break,
            Ok(Ok(Step::Wait(d))) => {
                drop(ready);
                tokio::time::sleep(d).await;
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(Err(e)) => {
                crate::warn!(
                    "stopped reading worker output",
                    worker = label,
                    stream = stream,
                    error = e,
                    hint = "the worker gets EPIPE on further output; restart it to reconnect",
                );
                break;
            }
        }
    }
    if !partial.is_empty() {
        follow_direct(b"\n", &mut partial, &label, stream); // the last line had no newline
    }
}

/// `warden logs -f` in direct mode: the bytes just written, cut into lines
/// as captured output is (they are shown, not kept or written again).
fn follow_direct(bytes: &[u8], partial: &mut Vec<u8>, label: &str, stream: &'static str) {
    let mut batch = crate::logging::OutputBatch::new(label, stream);
    split_lines(bytes, partial, &mut |l: &mut Vec<u8>| {
        if l.last() == Some(&b'\r') {
            l.pop();
        }
        batch.push(&String::from_utf8_lossy(l));
        l.clear();
    });
    crate::logging::follow_only(batch);
}

/// Keep-all mode: longest a read waits for room in the log queue.
const KEEP_ALL_MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Bytes read from a worker's stdout/stderr per call.
const READ_CHUNK: usize = 64 * 1024;

/// Cut `data` into lines: each complete line (without its `\n`) is handed
/// to `emit` in `line`; a line longer than MAX_LINE is emitted in MAX_LINE
/// pieces; an unfinished tail stays in `line` for the next chunk. Newlines
/// are found with the C library's vectorised memchr and copied in bulk
/// (the old byte-at-a-time loop was the capture path's main CPU cost).
fn split_lines(mut data: &[u8], line: &mut Vec<u8>, emit: &mut impl FnMut(&mut Vec<u8>)) {
    loop {
        let (seg, done) = match crate::sys::memchr(b'\n', data) {
            Some(i) => (&data[..i], Some(i + 1)),
            None => (data, None),
        };
        let mut seg = seg;
        while line.len() + seg.len() >= MAX_LINE {
            let take = MAX_LINE - line.len();
            line.extend_from_slice(&seg[..take]);
            emit(line);
            seg = &seg[take..];
        }
        line.extend_from_slice(seg);
        match done {
            Some(next) => {
                emit(line);
                data = &data[next..];
            }
            None => return,
        }
    }
}

/// A complete line: into the batch if the rate budget allows; then cleared.
fn keep(batch: &mut crate::logging::OutputBatch, line: &mut Vec<u8>, rate: &mut RateLimit) {
    if rate.allow() {
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        batch.push(&String::from_utf8_lossy(line));
    }
    line.clear();
}

async fn pump_ipc(mut rx: tokio::net::UnixStream, inst: u64, events: mpsc::UnboundedSender<ProcEvent>, label: String) {
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
        while let Some(i) = crate::sys::memchr(b'\n', &buf[start..]) {
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
        // A second later a new window starts with the next line.
        r.window -= std::time::Duration::from_millis(1100);
        assert!(r.allow() && r.allow() && r.allow());
        assert!(!r.allow());
        assert_eq!(r.dropped, 3);
        // A slow logger: lines spread over many seconds are never dropped,
        // even though the budget is only checked once it runs out.
        let mut slow = RateLimit::new(3);
        for _ in 0..20 {
            assert!(slow.allow());
            slow.window -= std::time::Duration::from_secs(2); // time passes
        }
        assert_eq!(slow.dropped, 0);
    }

    /// The previous splitter, byte by byte: the reference for `split_lines`.
    fn split_reference(data: &[u8], line: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
        for &b in data {
            if b == b'\n' {
                out.push(std::mem::take(line));
            } else {
                line.push(b);
                if line.len() >= MAX_LINE {
                    out.push(std::mem::take(line));
                }
            }
        }
    }

    #[test]
    fn split_lines_matches_the_byte_loop_for_any_chunking() {
        let mut seed = 12_345u64;
        let mut rnd = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for round in 0..200 {
            // Lines of every kind: empty, short, CRLF, exactly MAX_LINE, longer.
            let mut data = Vec::new();
            for _ in 0..(1 + rnd(40)) {
                let len = match rnd(6) {
                    0 => 0,
                    1 => rnd(10),
                    2 => rnd(300),
                    3 => MAX_LINE as u64 - 1 + rnd(3),
                    4 => MAX_LINE as u64 * (1 + rnd(3)) + rnd(50),
                    _ => rnd(2000),
                };
                data.extend((0..len).map(|i| b'a' + (i % 26) as u8));
                if rnd(4) == 0 {
                    data.push(b'\r');
                }
                data.push(b'\n');
            }
            if round % 3 == 0 {
                data.extend_from_slice(b"unterminated tail");
            }
            let (mut want, mut ref_line) = (Vec::new(), Vec::new());
            split_reference(&data, &mut ref_line, &mut want);
            // Same data cut into random chunks.
            let (mut got, mut line) = (Vec::new(), Vec::new());
            let mut rest = &data[..];
            while !rest.is_empty() {
                let n = (1 + rnd(70_000) as usize).min(rest.len());
                split_lines(&rest[..n], &mut line, &mut |l: &mut Vec<u8>| got.push(std::mem::take(l)));
                rest = &rest[n..];
            }
            assert_eq!(got.len(), want.len(), "round {round}: line count");
            assert!(got == want, "round {round}: lines differ");
            assert_eq!(line, ref_line, "round {round}: unfinished tail");
        }
    }

    #[test]
    fn ipc_msg_parses() {
        let m: IpcMsg = serde_json::from_str(r#"{"ev":"listening","port":3000,"worker":2}"#).unwrap();
        assert_eq!(m.ev, "listening");
        assert_eq!(m.port, Some(3000));
        assert_eq!(m.worker, Some(2));
        let m: IpcMsg = serde_json::from_str(r#"{"ev":"exit","worker":1,"code":1,"expected":false}"#).unwrap();
        assert_eq!(m.expected, Some(false));
        let m: IpcMsg = serde_json::from_str(r#"{"ev":"long_lived_closed","ws":3,"sse":2,"worker":1}"#).unwrap();
        assert_eq!((m.ws, m.sse), (Some(3), Some(2)));
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
                    output: Output::Capture,
                    max_lines_per_sec: 0,
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

    /// fd 3 is two-way: the worker reads what Warden sends (a standby's
    /// `promote`) and answers on the same descriptor.
    #[tokio::test(flavor = "current_thread")]
    async fn sends_lines_to_the_worker_on_fd3() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let spec = Spec {
                    program: "sh".into(),
                    args: vec!["-c".into(), r#"read -r l <&3; echo "{\"ev\":\"$l\"}" >&3"#.into()],
                    cwd: None,
                    env: vec![],
                    label: "t".into(),
                    output: Output::Capture,
                    max_lines_per_sec: 0,
                };
                let h = spawn(spec, 9, tx).unwrap();
                h.relabel("2");
                assert_eq!(&*h.label.get(), "2");
                h.send(b"promoted\n").unwrap();
                let (mut got_ipc, mut got_exit) = (false, false);
                while !(got_ipc && got_exit) {
                    match rx.recv().await.unwrap() {
                        ProcEvent::Ipc { msg, .. } => {
                            assert_eq!(msg.ev, "promoted");
                            got_ipc = true;
                        }
                        ProcEvent::Exited { code, .. } => {
                            assert_eq!(code, Some(0));
                            got_exit = true;
                        }
                    }
                }
                // The worker is gone: sending fails cleanly instead of blocking.
                assert!(h.send(b"again\n").is_err());
            })
            .await;
    }

    /// `close_input`: a worker blocked reading fd 3 (and deaf to SIGTERM)
    /// gets EOF at once, and can still report to Warden afterwards.
    #[tokio::test(flavor = "current_thread")]
    async fn closing_input_ends_a_pending_read_on_fd3() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (tx, mut rx) = mpsc::unbounded_channel();
                let spec = Spec {
                    program: "sh".into(),
                    args: vec![
                        "-c".into(),
                        r#"trap '' TERM; echo '{"ev":"reading"}' >&3; read -r l <&3; echo "{\"ev\":\"eof $?\"}" >&3"#
                            .into(),
                    ],
                    cwd: None,
                    env: vec![],
                    label: "t".into(),
                    output: Output::Capture,
                    max_lines_per_sec: 0,
                };
                let h = spawn(spec, 3, tx).unwrap();
                async fn next(rx: &mut mpsc::UnboundedReceiver<ProcEvent>) -> ProcEvent {
                    tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
                        .await
                        .expect("the worker is still blocked in its read")
                        .unwrap()
                }
                match next(&mut rx).await {
                    ProcEvent::Ipc { msg, .. } => assert_eq!(msg.ev, "reading"),
                    e => panic!("unexpected {e:?}"),
                }
                h.signal(libc::SIGTERM); // ignored: only the EOF ends the read
                h.close_input();
                let (mut got_eof, mut got_exit) = (false, false);
                while !(got_eof && got_exit) {
                    match next(&mut rx).await {
                        ProcEvent::Ipc { msg, .. } => {
                            assert_eq!(msg.ev, "eof 1", "read(1) saw the end of input");
                            got_eof = true;
                        }
                        ProcEvent::Exited { code, .. } => {
                            assert_eq!(code, Some(0));
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
                    output: Output::Capture,
                    max_lines_per_sec: 0,
                };
                let h = spawn(spec, 1, tx).unwrap();
                h.signal(libc::SIGTERM);
                match rx.recv().await.unwrap() {
                    ProcEvent::Exited { signal, sent, .. } => {
                        assert_eq!(signal, Some(libc::SIGTERM));
                        // Recorded, so the exit reads "stopped by Warden's SIGTERM".
                        assert!(sent.has(libc::SIGTERM) && !sent.has(libc::SIGKILL), "{sent:?}");
                    }
                    e => panic!("unexpected {e:?}"),
                }
            })
            .await;
    }
}

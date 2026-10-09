//! The keeper: a small process that holds an app's workers through a crash of
//! the app's supervisor, and hands them back to the supervisor it restarts.
//!
//! ```text
//! wardend, systemd or a terminal
//!    └─ warden run  (keeper: the process they started, same pid)
//!         ├─ warden run  (the supervisor, restarted by the keeper if it dies)
//!         ├─ worker 1 ─┐ their output pipes, fd 3 and fd 4: the supervisor
//!         └─ worker 2 ─┘ reads them, the keeper holds a copy of each
//! ```
//!
//! `warden run` (how every supervisor starts) becomes the keeper unless
//! `[restart] keep_workers_on_crash = false` or `WARDEN_KEEPER=0`: it starts
//! the real supervisor as its child and stays the process its launcher
//! watches, so wardend, systemd (`KillMode=mixed` kills a unit's leftovers
//! when its main process dies, and the keeper is that main process) and a
//! terminal see no difference.
//!
//! While the supervisor runs, the keeper only listens: for each worker it
//! starts, the supervisor sends the keeper a copy of the worker's pipes and
//! sockets (SCM_RIGHTS) and what it knows of it ([`Meta`]). The supervisor
//! reads them itself, as before; nothing on the request path changes.
//!
//! When the supervisor dies without saying goodbye (a crash, a panic, kill -9):
//! - the workers keep running: no parent-death signal ties them to the
//!   supervisor any more (`sys::workers_outlive_the_supervisor`), and on
//!   Linux the keeper is their subreaper, so it gets their exit codes;
//! - the keeper reads their output into a buffer (1 MB per stream, oldest
//!   dropped first), so no worker blocks writing a log line;
//! - it keeps the macOS listening socket open: visitors wait in the kernel's
//!   queue instead of being refused;
//! - it starts the supervisor again (right away, then with backoff), and
//!   hands everything over: the pipes and sockets, the buffered output, the
//!   exits it saw. The supervisor takes the workers that were serving back
//!   into their slots and stops the rest.
//!
//! When the keeper itself dies, the supervisor stops its workers (SIGTERM
//! from the parent-death signal on Linux, end of the keeper's socket on
//! macOS): the behaviour of a supervisor without a keeper.
//!
//! The channel is a Unix stream socket: frames of a 4-byte big-endian length
//! and a JSON [`Msg`], with the descriptors a message carries attached to it.

use crate::config::Config;
use crate::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The supervisor's end of the channel, set by the keeper.
pub const FD_ENV: &str = "WARDEN_KEEPER_FD";
/// The keeper's pid, and when it started (unix ms): what `status` reports
/// as the app's process and its uptime.
pub const PID_ENV: &str = "WARDEN_KEEPER_PID";
pub const STARTED_ENV: &str = "WARDEN_KEEPER_STARTED";
/// The systemd unit the keeper runs as ("" for none): the supervisor is not
/// the unit's main process, so it can't tell by itself (`systemd::own_unit`).
pub const UNIT_ENV: &str = "WARDEN_KEEPER_UNIT";
/// `0`: no keeper (tests that need a supervisor's workers to die with it).
pub const OFF_ENV: &str = "WARDEN_KEEPER";
/// Every variable above: not passed on to workers.
pub const ENVS: [&str; 5] = [FD_ENV, PID_ENV, STARTED_ENV, UNIT_ENV, OFF_ENV];

/// Buffered output per worker stream while no supervisor reads it.
const RING: usize = 1 << 20;
/// Supervisor crashes within `CRASH_WINDOW` after which the keeper gives up
/// (stops the workers and exits like the supervisor did).
const MAX_CRASHES: usize = 5;
const CRASH_WINDOW: Duration = Duration::from_secs(60);
/// The protocol's version, in `hello`.
const VERSION: u32 = 1;
/// Longest frame accepted (a frame is a few hundred bytes).
const MAX_FRAME: usize = 1 << 20;

/// Which of a worker's descriptors a copy is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FdKind {
    /// The read end of its stdout pipe (stdout and stderr, when they share one).
    Out,
    /// The read end of its stderr pipe.
    Err,
    /// Warden's end of fd 3.
    Ipc,
    /// Warden's end of fd 4 (connection handoff).
    Handoff,
}

/// What the supervisor knows of a worker, kept by the keeper so the next
/// supervisor can put the worker back where it was.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Meta {
    pub slot: usize,
    /// `current`, `replacement`, `retiring` or `standby`.
    pub role: String,
    pub ready: bool,
    pub stopping: bool,
    /// The worker's `inst` (`WARDEN_INSTANCE`).
    pub inst: u64,
    pub label: String,
    pub handoff: bool,
    pub adopt: bool,
    /// The address the app asked to listen on (`listening`'s `host`).
    pub host: Option<String>,
    /// As pairs: a map with number keys does not survive the tagged enum
    /// (serde buffers it with string keys).
    #[serde(with = "pairs")]
    pub sockets: BTreeMap<usize, PathBuf>,
    pub listening: Vec<u16>,
    /// Worker mode: the Workers (threads) that listen.
    pub threads: Vec<usize>,
}

mod pairs {
    use serde::{Deserialize, Deserializer, Serializer};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    pub fn serialize<S: Serializer>(m: &BTreeMap<usize, PathBuf>, s: S) -> Result<S::Ok, S::Error> {
        s.collect_seq(m.iter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<usize, PathBuf>, D::Error> {
        Ok(Vec::<(usize, PathBuf)>::deserialize(d)?.into_iter().collect())
    }
}

/// How a process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exit {
    pub code: Option<i32>,
    pub signal: Option<i32>,
}

/// One frame of the channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    // ------------------------------------------------ supervisor → keeper
    /// First thing a supervisor sends: how its workers are stopped (the
    /// keeper stops them so when it gives up).
    Hello { v: u32, stop_signal: i32, grace_ms: u64 },
    /// A worker started; carries a copy of each descriptor in `fds`.
    Spawned { pid: u32, meta: Meta, fds: Vec<FdKind> },
    /// What the supervisor knows of a worker changed.
    Meta { pid: u32, meta: Meta },
    /// A worker's exit was handled: its copies are closed.
    Gone { pid: u32 },
    /// A listening socket the supervisor owns (macOS handoff); one descriptor.
    Listener { name: String },
    /// sd_notify: the keeper is the unit's main process.
    Notify { state: String },
    /// The supervisor is exiting on purpose (its workers are stopped).
    Bye,
    // ------------------------------------------------ keeper → supervisor
    /// A worker kept through a supervisor's death: `fds`, then the buffered
    /// output of each stream in `logs` (an unlinked file each), attached.
    Kept {
        pid: u32,
        start: Option<u64>,
        /// How long ago the worker started (the keeper heard of it).
        #[serde(default)]
        age_ms: u64,
        meta: Meta,
        fds: Vec<FdKind>,
        logs: Vec<FdKind>,
        dropped: u64,
        exited: Option<Exit>,
    },
    /// A listening socket kept; one descriptor.
    KeptListener { name: String },
    /// Everything kept has been sent. `restarts`: supervisor restarts so far.
    End { restarts: u32 },
    /// A kept worker exited (it is no longer the supervisor's child).
    Exited { pid: u32, code: Option<i32>, signal: Option<i32> },
}

impl Msg {
    /// How many descriptors ride on this message.
    fn fd_count(&self) -> usize {
        match self {
            Msg::Spawned { fds, .. } => fds.len(),
            Msg::Kept { fds, logs, .. } => fds.len() + logs.len(),
            Msg::Listener { .. } | Msg::KeptListener { .. } => 1,
            _ => 0,
        }
    }
}

/// Should this `warden run` be a keeper (and start the supervisor as its
/// child)? Not when it is that child, not when the config or
/// `WARDEN_KEEPER=0` says no.
pub fn wanted(cfg: &Config) -> bool {
    std::env::var_os(FD_ENV).is_none()
        && cfg.restart.keep_workers_on_crash
        && std::env::var(OFF_ENV).map(|v| v != "0").unwrap_or(true)
}

/// Before [`run`]. Linux: the workers of a dead supervisor must become this
/// process's children (a child subreaper), or nothing could tell a running
/// one from one that exited. If the kernel or a sandbox refuses, the app
/// runs without a keeper ([`no_keeper`] says so once logging is up).
pub fn prepare() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    crate::sys::set_child_subreaper()?;
    Ok(())
}

/// [`prepare`] failed: the supervisor runs alone.
pub fn no_keeper(e: io::Error) {
    warn!(
        "running without a keeper: this process cannot become the reaper of its workers, so they stop if the \
         supervisor crashes",
        error = e,
        hint = "needs Linux 3.4 or newer and prctl(PR_SET_CHILD_SUBREAPER) allowed by the sandbox; set [restart] \
                keep_workers_on_crash = false to silence this",
    );
}

// ------------------------------------------------------------------ frames

/// Send one frame, `fds` attached, on a socket that may be non-blocking
/// (waits up to 5 s for room, then gives up: the other side is stuck).
fn send_frame(sock: BorrowedFd<'_>, msg: &Msg, fds: &[BorrowedFd<'_>]) -> io::Result<()> {
    debug_assert_eq!(msg.fd_count(), fds.len());
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    let mut frame = Vec::with_capacity(body.len() + 4);
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    let mut sent = 0;
    let mut attach = fds;
    while sent < frame.len() {
        match crate::sys::send_fds(sock, &frame[sent..], attach) {
            Ok(n) => {
                sent += n;
                attach = &[];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                if !crate::sys::wait_writable(sock, 5000)? {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "the other side reads nothing"));
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// The receiving side of a channel: bytes and descriptors as they arrive,
/// cut into frames.
#[derive(Default)]
struct Frames {
    buf: Vec<u8>,
    fds: VecDeque<OwnedFd>,
}

impl Frames {
    /// Read what is there (non-blocking socket). `Ok(false)`: end of stream.
    fn fill(&mut self, sock: BorrowedFd<'_>) -> io::Result<bool> {
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match crate::sys::recv_fds(sock, &mut chunk) {
                Ok((0, fds)) => {
                    self.fds.extend(fds);
                    return Ok(false);
                }
                Ok((n, fds)) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    self.fds.extend(fds);
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) => return Err(e),
            }
        }
    }

    /// The next whole frame and its descriptors, if one is there.
    fn next(&mut self) -> io::Result<Option<(Msg, Vec<OwnedFd>)>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("a frame of {len} bytes")));
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg: Result<Msg, _> = serde_json::from_slice(&self.buf[4..4 + len]);
        self.buf.drain(..4 + len);
        let msg = msg.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let n = msg.fd_count();
        if self.fds.len() < n {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "a frame came without its descriptors"));
        }
        Ok(Some((msg, self.fds.drain(..n).collect())))
    }
}

// ------------------------------------------------------------------ keeper

/// What the keeper holds of one worker.
struct Kept {
    start: Option<u64>,
    /// When the keeper heard of it (`Spawned`): its age for the next supervisor.
    since: Instant,
    meta: Meta,
    fds: Vec<(FdKind, OwnedFd)>,
    /// Its supervisor died: the keeper reports its exit (it is the keeper's
    /// child on Linux, nobody's it can wait for on macOS).
    orphan: bool,
    exited: Option<Exit>,
    /// Output read while no supervisor did: per stream.
    rings: Vec<(FdKind, std::rc::Rc<std::cell::RefCell<Ring>>)>,
    /// Drain tasks are reading its output into `rings`.
    drained: bool,
}

#[derive(Default)]
struct Ring {
    data: VecDeque<u8>,
    dropped: u64,
}

impl Ring {
    fn push(&mut self, bytes: &[u8]) {
        self.data.extend(bytes);
        let over = self.data.len().saturating_sub(RING);
        if over > 0 {
            self.data.drain(..over);
            self.dropped += over as u64;
        }
    }
}

/// The supervisor the keeper runs now.
struct Sup {
    pid: u32,
    sock: tokio::io::unix::AsyncFd<OwnedFd>,
    frames: Frames,
    /// It said `hello` (and got what was kept).
    hello: bool,
    /// It said `bye`: its exit is on purpose.
    bye: bool,
    /// Its socket ended.
    eof: bool,
    /// Reaped: its pid may be someone else's now.
    reaped: bool,
}

struct Keeper {
    exe: PathBuf,
    args: Vec<std::ffi::OsString>,
    app: String,
    runtime_dir: PathBuf,
    started_ms: u64,
    unit: Option<String>,
    sup: Option<Sup>,
    kept: BTreeMap<u32, Kept>,
    listeners: Vec<(String, OwnedFd)>,
    stop_signal: i32,
    grace: Duration,
    /// Drain tasks reading kept workers' output while no supervisor does.
    draining: Vec<tokio::task::AbortHandle>,
    crashes: VecDeque<Instant>,
    restarts: u32,
    restart_at: Option<Instant>,
    /// The last supervisor's end: how the keeper ends when it stops.
    last_exit: Exit,
    /// SIGTERM, SIGINT or SIGQUIT came: no restart; the workers are stopped.
    term: bool,
    /// Stopping the kept workers itself: SIGKILL at the first instant, give up
    /// waiting at the second.
    stopping: Option<(Instant, Instant)>,
    killed: bool,
}

/// Run as the keeper of the app `cfg` describes: start the supervisor (this
/// same command, as a child), and keep its workers through its crashes.
/// Returns only on failure to start; otherwise exits like the supervisor.
pub fn run(rt: &tokio::runtime::Runtime, cfg: &Config) -> i32 {
    let local = tokio::task::LocalSet::new();
    match rt.block_on(local.run_until(keep(cfg))) {
        Ok(never) => match never {},
        Err(e) => {
            error!(
                "the keeper could not start the supervisor",
                app = cfg.app.name,
                error = e,
                hint = "set [restart] keep_workers_on_crash = false to run without a keeper; `warden doctor` checks \
                        the usual causes",
            );
            1
        }
    }
}

enum Never {}

async fn keep(cfg: &Config) -> io::Result<Never> {
    use tokio::signal::unix::{SignalKind, signal};
    // Linux: /proc/self/exe runs this very binary even after an upgrade
    // replaced the file (the supervisor must speak this keeper's protocol).
    #[cfg(target_os = "linux")]
    let exe = PathBuf::from("/proc/self/exe");
    #[cfg(not(target_os = "linux"))]
    let exe = std::env::current_exe()?;
    let started_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let mut k = Keeper {
        exe,
        args: std::env::args_os().skip(1).collect(),
        app: cfg.app.name.clone(),
        runtime_dir: cfg.socket_path().parent().map(PathBuf::from).unwrap_or_else(std::env::temp_dir),
        started_ms,
        unit: crate::systemd::own_unit(),
        sup: None,
        kept: BTreeMap::new(),
        listeners: Vec::new(),
        stop_signal: cfg.stop_signal(),
        grace: cfg.grace_period(),
        draining: Vec::new(),
        crashes: VecDeque::new(),
        restarts: 0,
        restart_at: None,
        last_exit: Exit { code: Some(0), signal: None },
        term: false,
        stopping: None,
        killed: false,
    };
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut quit = signal(SignalKind::quit())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut usr2 = signal(SignalKind::user_defined2())?;
    let mut child = signal(SignalKind::child())?;
    k.start_supervisor()?;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let restart = k.restart_at;
        let ev = {
            let sock = k.sup.as_ref().filter(|s| !s.eof).map(|s| &s.sock);
            tokio::select! {
                _ = term.recv() => Ev::Signal(libc::SIGTERM),
                _ = int.recv() => Ev::Signal(libc::SIGINT),
                _ = quit.recv() => Ev::Signal(libc::SIGQUIT),
                _ = hup.recv() => Ev::Signal(libc::SIGHUP),
                _ = usr1.recv() => Ev::Signal(libc::SIGUSR1),
                _ = usr2.recv() => Ev::Signal(libc::SIGUSR2),
                _ = child.recv() => Ev::Child,
                r = async { sock.expect("checked").readable().await.map(|mut g| g.clear_ready()) }, if sock.is_some() => Ev::Readable(r),
                _ = async { tokio::time::sleep_until(restart.expect("checked").into()).await }, if restart.is_some() => Ev::Restart,
                _ = tick.tick() => Ev::Tick,
            }
        };
        match ev {
            Ev::Signal(sig) => k.on_signal(sig),
            Ev::Child => k.reap(),
            Ev::Readable(Ok(())) => k.read_supervisor(),
            Ev::Readable(Err(e)) => {
                warn!("lost the channel to the supervisor", error = e, hint = "its exit follows");
                if let Some(s) = k.sup.as_mut() {
                    s.eof = true;
                }
            }
            Ev::Restart => {
                k.restart_at = None;
                if let Err(e) = k.start_supervisor() {
                    error!(
                        "the keeper could not start the supervisor again",
                        app = k.app,
                        error = e,
                        hint = "its workers keep running; the keeper tries again",
                    );
                    k.schedule_restart(None, "could not start");
                }
            }
            Ev::Tick => {
                // A SIGCHLD can be coalesced with another: look anyway.
                k.reap();
                k.poll_orphans();
                k.step_stop();
            }
        }
    }
}

enum Ev {
    Signal(i32),
    Child,
    Readable(io::Result<()>),
    Restart,
    Tick,
}

impl Keeper {
    fn start_supervisor(&mut self) -> io::Result<()> {
        let (ours, theirs) = crate::sys::socketpair_cloexec()?;
        crate::sys::set_nonblocking(ours.as_fd(), true)?;
        let _ = crate::sys::set_nosigpipe(ours.as_fd());
        let mut cmd = std::process::Command::new(&self.exe);
        // `ps` shows the command the keeper was started with, not /proc/self/exe.
        if let Some(arg0) = std::env::args_os().next() {
            std::os::unix::process::CommandExt::arg0(&mut cmd, arg0);
        }
        cmd.args(&self.args)
            .env(FD_ENV, theirs.as_raw_fd().to_string())
            .env(PID_ENV, std::process::id().to_string())
            .env(STARTED_ENV, self.started_ms.to_string())
            .env(UNIT_ENV, self.unit.clone().unwrap_or_default())
            // systemd's watchdog pings come from the keeper (`Notify`).
            .env_remove("WATCHDOG_PID");
        crate::sys::pre_exec_supervisor(&mut cmd, theirs.as_raw_fd());
        let child = cmd.spawn()?;
        drop(theirs);
        let pid = child.id();
        // Reaped by `reap` (waitpid), never through the handle.
        drop(child);
        self.sup = Some(Sup {
            pid,
            sock: tokio::io::unix::AsyncFd::new(ours)?,
            frames: Frames::default(),
            hello: false,
            bye: false,
            eof: false,
            reaped: false,
        });
        Ok(())
    }

    fn on_signal(&mut self, sig: i32) {
        let stop = matches!(sig, libc::SIGTERM | libc::SIGINT | libc::SIGQUIT);
        if stop {
            self.term = true;
        }
        match &self.sup {
            Some(s) => {
                let _ = crate::sys::kill(s.pid as i32, sig);
            }
            None if stop => {
                self.restart_at = None;
                self.begin_stop();
            }
            None => {}
        }
    }

    fn reap(&mut self) {
        while let Some((pid, code, signal)) = crate::sys::reap_any() {
            let exit = Exit { code, signal };
            if self.sup.as_ref().is_some_and(|s| s.pid == pid) {
                self.supervisor_died(exit);
            } else {
                // Our child: its supervisor is gone (it may have died just
                // before the supervisor did, whose exit comes next).
                self.worker_exited(pid, exit, true);
            }
        }
    }

    fn worker_exited(&mut self, pid: u32, exit: Exit, reaped: bool) {
        let Some(k) = self.kept.get_mut(&pid) else { return };
        if reaped {
            k.orphan = true;
        }
        if !k.orphan || k.exited.is_some() {
            return;
        }
        k.exited = Some(exit);
        if let Some(s) = self.sup.as_ref().filter(|s| s.hello) {
            let m = Msg::Exited { pid, code: exit.code, signal: exit.signal };
            let _ = send_frame(s.sock.get_ref().as_fd(), &m, &[]);
        }
        if self.stopping.is_some() && self.live() == 0 {
            self.finish();
        }
    }

    /// Whether the workers the supervisor left are still there. Linux: they
    /// are our children now, or gone (one the supervisor reaped before it
    /// could send `Gone`). macOS: not our children; the same process is still
    /// there if its start time is.
    fn poll_orphans(&mut self) {
        let orphans: Vec<(u32, Option<u64>)> =
            self.kept.iter().filter(|(_, k)| k.orphan && k.exited.is_none()).map(|(pid, k)| (*pid, k.start)).collect();
        for (pid, start) in orphans {
            if cfg!(target_os = "linux") {
                match crate::sys::try_reap(pid) {
                    Ok(None) => {}
                    Ok(Some((code, signal))) => self.worker_exited(pid, Exit { code, signal }, true),
                    Err(_) => self.worker_exited(pid, Exit { code: None, signal: None }, false),
                }
            } else if crate::platform::proc_identity(pid).is_none_or(|id| Some(id.start) != start) {
                self.worker_exited(pid, Exit { code: None, signal: None }, false);
            }
        }
    }

    fn live(&self) -> usize {
        self.kept.values().filter(|k| k.orphan && k.exited.is_none()).count()
    }

    fn read_supervisor(&mut self) {
        let Some(s) = self.sup.as_mut() else { return };
        let open = match s.frames.fill(s.sock.get_ref().as_fd()) {
            Ok(open) => open,
            Err(e) => {
                warn!("lost the channel to the supervisor", error = e, hint = "its exit follows");
                false
            }
        };
        loop {
            let Some(s) = self.sup.as_mut() else { return };
            match s.frames.next() {
                Ok(Some((msg, fds))) => self.on_msg(msg, fds),
                Ok(None) => break,
                Err(e) => {
                    error!(
                        "the supervisor sent the keeper something it cannot read; it no longer keeps that supervisor's workers",
                        error = e,
                        hint = "this is a Warden bug: please report it",
                    );
                    s.eof = true;
                    return;
                }
            }
        }
        if !open {
            if let Some(s) = self.sup.as_mut() {
                s.eof = true;
            }
        }
    }

    fn on_msg(&mut self, msg: Msg, fds: Vec<OwnedFd>) {
        match msg {
            Msg::Hello { stop_signal, grace_ms, .. } => {
                self.stop_signal = stop_signal;
                self.grace = Duration::from_millis(grace_ms);
                // Read after it died: nobody to hand anything to.
                if self.sup.as_ref().is_some_and(|s| !s.reaped) {
                    self.hand_over();
                }
            }
            Msg::Spawned { pid, meta, fds: kinds } => {
                let start = crate::platform::proc_identity(pid).map(|id| id.start);
                let fds = kinds.into_iter().zip(fds).collect();
                self.kept.insert(
                    pid,
                    Kept {
                        start,
                        since: Instant::now(),
                        meta,
                        fds,
                        orphan: false,
                        exited: None,
                        rings: Vec::new(),
                        drained: false,
                    },
                );
            }
            Msg::Meta { pid, meta } => {
                if let Some(k) = self.kept.get_mut(&pid) {
                    k.meta = meta;
                }
            }
            Msg::Gone { pid } => {
                self.kept.remove(&pid);
            }
            Msg::Listener { name } => {
                if let Some(fd) = fds.into_iter().next() {
                    self.listeners.retain(|(n, _)| *n != name);
                    self.listeners.push((name, fd));
                }
            }
            Msg::Notify { state } => crate::systemd::notify(&state),
            Msg::Bye => {
                if let Some(s) = self.sup.as_mut() {
                    s.bye = true;
                }
            }
            Msg::Kept { .. } | Msg::KeptListener { .. } | Msg::End { .. } | Msg::Exited { .. } => {}
        }
    }

    /// A supervisor said hello: give it what was kept.
    fn hand_over(&mut self) {
        // From here the supervisor reads the pipes; the rings keep what was
        // read until it has them (a failed hand-over drains again into them).
        for d in self.draining.drain(..) {
            d.abort();
        }
        for k in self.kept.values_mut() {
            k.drained = false;
        }
        let Some(s) = self.sup.as_mut() else { return };
        s.hello = true;
        let sock = s.sock.get_ref().try_clone();
        let Ok(sock) = sock else { return };
        let mut seq = 0;
        let mut done: Vec<u32> = Vec::new();
        let mut failed = None;
        for (pid, k) in &self.kept {
            if !k.orphan {
                continue;
            }
            let mut logs: Vec<(FdKind, std::fs::File)> = Vec::new();
            let mut dropped = 0;
            for (kind, ring) in &k.rings {
                let mut ring = ring.borrow_mut();
                dropped += ring.dropped;
                if ring.data.is_empty() {
                    continue;
                }
                seq += 1;
                match self.ring_file(seq, ring.data.make_contiguous()) {
                    Ok(f) => logs.push((*kind, f)),
                    Err(e) => warn!(
                        "cannot hand the output buffered by the keeper to the supervisor; it is lost",
                        pid = pid,
                        error = e,
                        hint = "the app's runtime directory must be writable",
                    ),
                }
            }
            let msg = Msg::Kept {
                pid: *pid,
                start: k.start,
                age_ms: k.since.elapsed().as_millis() as u64,
                meta: k.meta.clone(),
                fds: k.fds.iter().map(|(kind, _)| *kind).collect(),
                logs: logs.iter().map(|(kind, _)| *kind).collect(),
                dropped,
                exited: k.exited,
            };
            let mut attach: Vec<BorrowedFd<'_>> = k.fds.iter().map(|(_, f)| f.as_fd()).collect();
            attach.extend(logs.iter().map(|(_, f)| f.as_fd()));
            if let Err(e) = send_frame(sock.as_fd(), &msg, &attach) {
                failed = Some(e);
                break;
            }
            if k.exited.is_some() {
                done.push(*pid);
            }
        }
        for (name, fd) in &self.listeners {
            if failed.is_some() {
                break;
            }
            if let Err(e) = send_frame(sock.as_fd(), &Msg::KeptListener { name: name.clone() }, &[fd.as_fd()]) {
                failed = Some(e);
            }
        }
        if failed.is_none() {
            if let Err(e) = send_frame(sock.as_fd(), &Msg::End { restarts: self.restarts }, &[]) {
                failed = Some(e);
            }
        }
        if let Some(e) = failed {
            error!(
                "the keeper could not hand the workers to the restarted supervisor",
                error = e,
                hint = "that supervisor stops; the keeper starts another",
            );
            if let Some(s) = self.sup.as_ref().filter(|s| !s.reaped) {
                let _ = crate::sys::kill(s.pid as i32, libc::SIGKILL);
            }
            // Until its exit is seen (then again until the next one's hello).
            self.drain_output();
            return;
        }
        // An exit the supervisor now knows of: nothing more to keep for it.
        for pid in done {
            self.kept.remove(&pid);
        }
        for k in self.kept.values_mut() {
            k.rings.clear();
        }
    }

    /// The buffered output of one stream, in an unlinked file of the
    /// runtime directory (owner only), read from its start.
    fn ring_file(&self, seq: u32, data: &[u8]) -> io::Result<std::fs::File> {
        use std::io::{Seek, Write};
        use std::os::unix::fs::OpenOptionsExt;
        let path = self.runtime_dir.join(format!(".{}-kept-{}-{seq}.log", self.app, std::process::id()));
        let mut f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
        let _ = std::fs::remove_file(&path);
        f.write_all(data)?;
        f.seek(io::SeekFrom::Start(0))?;
        Ok(f)
    }

    fn supervisor_died(&mut self, exit: Exit) {
        if let Some(s) = self.sup.as_mut() {
            s.reaped = true;
        }
        // What it sent before it went (a `bye` among it).
        if self.sup.as_ref().is_some_and(|s| !s.eof) {
            self.read_supervisor();
        }
        let Some(s) = self.sup.take() else { return };
        self.last_exit = exit;
        if s.bye {
            // On purpose: its workers are stopped. End as it did.
            crate::logging::flush(Duration::from_secs(1));
            crate::sys::exit_like(exit.code, exit.signal);
        }
        for k in self.kept.values_mut() {
            k.orphan = true;
        }
        // One it reaped before it could say `Gone` is not ours to keep.
        self.poll_orphans();
        self.drain_output();
        let how = crate::worker::describe_exit(exit.code, exit.signal);
        let live = self.live();
        if self.term {
            if live > 0 {
                warn!(
                    "the supervisor died while stopping; the keeper stops its workers",
                    app = self.app,
                    pid = s.pid,
                    exit = how,
                    workers = live,
                    hint = "its log lines above say why it died",
                );
            }
            return self.begin_stop();
        }
        if live == 0 {
            // Nothing to keep: end as it did, and whoever started us decides.
            crate::logging::flush(Duration::from_secs(1));
            crate::sys::exit_like(exit.code, exit.signal);
        }
        self.schedule_restart(Some(s.pid), &how);
    }

    /// The supervisor is gone (`pid`) or could not start: start it again,
    /// right away the first time, then with backoff; or give up after too
    /// many crashes in a row and stop the workers.
    fn schedule_restart(&mut self, pid: Option<u32>, how: &str) {
        let now = Instant::now();
        self.crashes.push_back(now);
        while self.crashes.front().is_some_and(|t| now.duration_since(*t) > CRASH_WINDOW) {
            self.crashes.pop_front();
        }
        if self.crashes.len() > MAX_CRASHES {
            error!(
                "the supervisor keeps dying; the keeper stops its workers and gives up",
                app = self.app,
                exit = how,
                crashes = self.crashes.len(),
                window_s = CRASH_WINDOW.as_secs(),
                hint = "this is a Warden bug: please report it with the log lines above; wardend or systemd starts the \
                        app again",
            );
            return self.begin_stop();
        }
        let delay = match self.crashes.len() {
            0 | 1 => Duration::ZERO,
            n => Duration::from_millis(250 << (n - 2).min(4)),
        };
        self.restarts += 1;
        error!(
            "the supervisor died; its workers keep serving and it is starting again",
            app = self.app,
            pid = pid.unwrap_or(0),
            exit = how,
            workers = self.live(),
            delay_ms = delay.as_millis(),
            hint = "this is a Warden bug: please report it with the log lines above; nothing was restarted but the \
                    supervisor",
        );
        self.restart_at = Some(now + delay);
    }

    /// Read the kept workers' output while no supervisor does.
    fn drain_output(&mut self) {
        for k in self.kept.values_mut() {
            if k.exited.is_some() || k.drained || !k.orphan {
                continue;
            }
            k.drained = true;
            for (kind, fd) in &k.fds {
                if !matches!(kind, FdKind::Out | FdKind::Err) {
                    continue;
                }
                let Ok(copy) = fd.try_clone() else { continue };
                // After a failed hand-over: on top of what is there.
                let ring = match k.rings.iter().find(|(n, _)| n == kind) {
                    Some((_, r)) => r.clone(),
                    None => {
                        let r = std::rc::Rc::new(std::cell::RefCell::new(Ring::default()));
                        k.rings.push((*kind, r.clone()));
                        r
                    }
                };
                let task = tokio::task::spawn_local(drain(copy, ring));
                self.draining.push(task.abort_handle());
            }
        }
    }

    /// Stop the kept workers the way the supervisor would: the stop signal to
    /// each one's process group, SIGKILL after the grace period.
    fn begin_stop(&mut self) {
        if self.stopping.is_some() {
            return;
        }
        let live: Vec<(u32, Option<u64>)> =
            self.kept.iter().filter(|(_, k)| k.orphan && k.exited.is_none()).map(|(p, k)| (*p, k.start)).collect();
        if live.is_empty() {
            self.finish();
        }
        info!("stopping the workers the supervisor left", app = self.app, workers = live.len());
        for (pid, start) in live {
            signal_kept(pid, start, self.stop_signal);
        }
        let now = Instant::now();
        self.stopping = Some((now + self.grace, now + self.grace + Duration::from_secs(5)));
    }

    fn step_stop(&mut self) {
        let Some((kill_at, give_up)) = self.stopping else { return };
        let now = Instant::now();
        if self.live() == 0 || now >= give_up {
            self.finish();
        }
        if now >= kill_at && !self.killed {
            self.killed = true;
            for (pid, k) in &self.kept {
                if k.orphan && k.exited.is_none() {
                    signal_kept(*pid, k.start, libc::SIGKILL);
                }
            }
        }
    }

    /// The end: as the last supervisor ended (0 when stopped on request).
    fn finish(&mut self) -> ! {
        let exit = if self.term { Exit { code: Some(0), signal: None } } else { self.last_exit };
        crate::logging::flush(Duration::from_secs(1));
        crate::sys::exit_like(exit.code, exit.signal);
    }
}

/// Signal a kept worker's process group, if it is still the process that was
/// kept (on Linux an unreaped child of ours: its pid can't be reused).
fn signal_kept(pid: u32, start: Option<u64>, sig: i32) {
    let same =
        cfg!(target_os = "linux") || crate::platform::proc_identity(pid).is_some_and(|id| Some(id.start) == start);
    if same {
        crate::sys::signal_child(pid, sig, true);
    }
}

/// Read one output pipe into `ring` until it ends (or the task is aborted at
/// the hand-over: what was read is in the ring by then).
async fn drain(fd: OwnedFd, ring: std::rc::Rc<std::cell::RefCell<Ring>>) {
    if crate::sys::set_nonblocking(fd.as_fd(), true).is_err() {
        return;
    }
    let Ok(afd) = tokio::io::unix::AsyncFd::new(std::fs::File::from(fd)) else { return };
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let Ok(mut guard) = afd.readable().await else { return };
        match guard.try_io(|f| std::io::Read::read(&mut f.get_ref(), &mut buf)) {
            Ok(Ok(0)) => return,
            Ok(Ok(n)) => ring.borrow_mut().push(&buf[..n]),
            Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(_)) => return,
            Err(_would_block) => {}
        }
    }
}

// ------------------------------------------------------------------ client

/// The supervisor's side of the channel.
pub mod client {
    use super::*;
    use std::cell::RefCell;
    use std::sync::OnceLock;

    struct Client {
        sock: OwnedFd,
        pid: u32,
        started_ms: u64,
        unit: Option<String>,
        /// Sends failed: said once.
        broken: std::sync::atomic::AtomicBool,
        /// What came after `End` in the same reads, for `listen`.
        rest: std::sync::Mutex<Option<Frames>>,
        /// Serializes frames from several threads (sd_notify).
        lock: std::sync::Mutex<()>,
    }

    static CLIENT: OnceLock<Client> = OnceLock::new();

    /// A worker the keeper kept through a supervisor's death.
    pub struct KeptWorker {
        pub pid: u32,
        pub start: Option<u64>,
        pub age: Duration,
        pub meta: Meta,
        pub fds: Vec<(FdKind, OwnedFd)>,
        /// Its output the keeper read meanwhile, per stream.
        pub logs: Vec<(FdKind, Vec<u8>)>,
        pub dropped: u64,
        pub exited: Option<Exit>,
    }

    /// What the keeper handed over at the start.
    #[derive(Default)]
    pub struct Attach {
        pub kept: Vec<KeptWorker>,
        pub listeners: Vec<(String, OwnedFd)>,
        pub restarts: u32,
    }

    /// Under a keeper: connect, say hello, and take what it kept. `None`
    /// without a keeper (or when its channel fails: then as without one).
    pub fn connect(stop_signal: i32, grace: Duration) -> Option<Attach> {
        let raw: i32 = std::env::var(FD_ENV).ok()?.parse().ok()?;
        // The keeper put this descriptor here for us alone; nothing else in
        // this process reads the variable.
        let sock = crate::sys::take_inherited_socket(raw)?;
        let pid = std::env::var(PID_ENV).ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let started_ms = std::env::var(STARTED_ENV).ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let unit = std::env::var(UNIT_ENV).ok().filter(|u| !u.is_empty());
        // The keeper may have handed us workers already: going on without it
        // would run a second set next to them. Exit; it starts another.
        let fail = |e: io::Error| -> ! {
            error!(
                "cannot talk to the keeper; this supervisor exits and the keeper starts another",
                error = e,
                hint = "this is a Warden bug: please report it",
            );
            crate::logging::flush_lines(Duration::from_millis(200));
            std::process::exit(1);
        };
        if let Err(e) = crate::sys::set_nonblocking(sock.as_fd(), true) {
            fail(e);
        }
        let _ = crate::sys::set_nosigpipe(sock.as_fd());
        let hello = Msg::Hello { v: VERSION, stop_signal, grace_ms: grace.as_millis() as u64 };
        if let Err(e) = send_frame(sock.as_fd(), &hello, &[]) {
            fail(e);
        }
        let mut attach = Attach::default();
        let mut frames = Frames::default();
        let deadline = Instant::now() + Duration::from_secs(10);
        'read: loop {
            loop {
                match frames.next() {
                    Ok(Some((Msg::End { restarts }, _))) => {
                        attach.restarts = restarts;
                        break 'read;
                    }
                    Ok(Some((msg, fds))) => take(&mut attach, msg, fds),
                    Ok(None) => break,
                    Err(e) => {
                        fail(e);
                    }
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let ready = crate::sys::wait_readable(sock.as_fd(), left.as_millis() as i32);
            match ready {
                Ok(true) => match frames.fill(sock.as_fd()) {
                    Ok(true) => {}
                    Ok(false) => {
                        fail(io::Error::new(io::ErrorKind::UnexpectedEof, "the keeper closed the channel"));
                    }
                    Err(e) => {
                        fail(e);
                    }
                },
                Ok(false) => {
                    fail(io::Error::new(io::ErrorKind::TimedOut, "the keeper sent nothing for 10 s"));
                }
                Err(e) => fail(e),
            }
        }
        let _ = CLIENT.set(Client {
            sock,
            pid,
            started_ms,
            unit,
            broken: Default::default(),
            rest: std::sync::Mutex::new(Some(frames)),
            lock: Default::default(),
        });
        Some(attach)
    }

    fn take(attach: &mut Attach, msg: Msg, fds: Vec<OwnedFd>) {
        match msg {
            Msg::Kept { pid, start, age_ms, meta, fds: kinds, logs, dropped, exited } => {
                let mut fds = fds.into_iter();
                let mine: Vec<(FdKind, OwnedFd)> = kinds.iter().copied().zip(fds.by_ref()).collect();
                let logs = logs
                    .into_iter()
                    .zip(fds)
                    .map(|(kind, fd)| {
                        let mut data = Vec::new();
                        let _ = std::io::Read::read_to_end(&mut std::fs::File::from(fd), &mut data);
                        (kind, data)
                    })
                    .collect();
                attach.kept.push(KeptWorker {
                    pid,
                    start,
                    age: Duration::from_millis(age_ms),
                    meta,
                    fds: mine,
                    logs,
                    dropped,
                    exited,
                });
            }
            Msg::KeptListener { name } => {
                if let Some(fd) = fds.into_iter().next() {
                    attach.listeners.push((name, fd));
                }
            }
            _ => {}
        }
    }

    /// Is this supervisor run by a keeper?
    pub fn active() -> bool {
        CLIENT.get().is_some()
    }

    /// The keeper's pid: the app's process as wardend and systemd see it.
    pub fn keeper_pid() -> Option<u32> {
        CLIENT.get().map(|c| c.pid).filter(|p| *p > 0)
    }

    /// When the keeper started (unix ms).
    pub fn started_ms() -> Option<u64> {
        CLIENT.get().map(|c| c.started_ms).filter(|t| *t > 0)
    }

    /// Under a keeper: the systemd unit the keeper runs as (`Some(None)`: none).
    pub fn unit() -> Option<Option<String>> {
        if CLIENT.get().is_some() {
            return Some(CLIENT.get().and_then(|c| c.unit.clone()));
        }
        // Asked before `connect` (systemd::own_unit is asked first thing).
        std::env::var_os(FD_ENV)?;
        Some(std::env::var(UNIT_ENV).ok().filter(|u| !u.is_empty()))
    }

    /// Send a frame to the keeper (nothing without one). A failure is said
    /// once: the workers then won't survive a crash of this supervisor.
    pub fn send(msg: &Msg, fds: &[BorrowedFd<'_>]) {
        let Some(c) = CLIENT.get() else { return };
        let _held = c.lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Err(e) = send_frame(c.sock.as_fd(), msg, fds) {
            if !c.broken.swap(true, std::sync::atomic::Ordering::Relaxed) {
                error!(
                    "cannot reach the keeper; if this supervisor crashes now, its workers may not be kept",
                    error = e,
                    hint = "this is a Warden bug: please report it",
                );
            }
        }
    }

    /// sd_notify through the keeper (the unit's main process). False without
    /// a keeper: the caller notifies itself.
    pub fn notify(state: &str) -> bool {
        if !active() {
            return false;
        }
        send(&Msg::Notify { state: state.to_string() }, &[]);
        true
    }

    thread_local! {
        /// Kept workers' exits: a waiter for each, or the exit when it came first.
        static EXITS: RefCell<HashMap<u32, Waiting>> = RefCell::new(HashMap::new());
    }

    enum Waiting {
        Waiter(tokio::sync::oneshot::Sender<Exit>),
        Came(Exit),
    }

    /// The exit of kept worker `pid`, when the keeper reports it.
    pub fn exit_of(pid: u32) -> tokio::sync::oneshot::Receiver<Exit> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        EXITS.with(|e| {
            let mut e = e.borrow_mut();
            match e.remove(&pid) {
                Some(Waiting::Came(exit)) => {
                    let _ = tx.send(exit);
                }
                _ => {
                    e.insert(pid, Waiting::Waiter(tx));
                }
            }
        });
        rx
    }

    fn exited(pid: u32, exit: Exit) {
        EXITS.with(|e| {
            let mut e = e.borrow_mut();
            match e.remove(&pid) {
                Some(Waiting::Waiter(tx)) => {
                    let _ = tx.send(exit);
                }
                _ => {
                    e.insert(pid, Waiting::Came(exit));
                }
            }
        });
    }

    /// Read what the keeper sends from now on (on this thread's LocalSet):
    /// kept workers' exits; `gone` runs when the keeper's channel ends.
    pub fn listen(gone: impl FnOnce() + 'static) {
        let Some(c) = CLIENT.get() else { return };
        let Ok(copy) = c.sock.try_clone() else { return };
        tokio::task::spawn_local(async move {
            let Ok(afd) = tokio::io::unix::AsyncFd::new(copy) else { return };
            let mut frames = c.rest.lock().ok().and_then(|mut r| r.take()).unwrap_or_default();
            let mut open = true;
            loop {
                // First what `connect` read past `End` (an exit right after
                // the hand-over), then what comes.
                while let Ok(Some((msg, _))) = frames.next() {
                    if let Msg::Exited { pid, code, signal } = msg {
                        exited(pid, Exit { code, signal });
                    }
                }
                if !open {
                    break;
                }
                let Ok(mut guard) = afd.readable().await else { break };
                guard.clear_ready();
                drop(guard);
                open = frames.fill(afd.get_ref().as_fd()).unwrap_or(false);
            }
            gone();
        });
    }

    /// The supervisor is exiting on purpose.
    pub fn bye() {
        send(&Msg::Bye, &[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair() -> (OwnedFd, OwnedFd) {
        let (a, b) = crate::sys::socketpair_cloexec().unwrap();
        crate::sys::set_nonblocking(b.as_fd(), true).unwrap();
        (a, b)
    }

    /// Frames arrive whole, in order, each with its own descriptors, however
    /// the bytes are cut.
    #[test]
    fn frames_carry_their_descriptors() {
        let (a, b) = pair();
        let (r1, _w1) = crate::sys::pipe_cloexec().unwrap();
        let (r2, _w2) = crate::sys::pipe_cloexec().unwrap();
        let spawned =
            Msg::Spawned { pid: 42, meta: Meta { slot: 1, ..Meta::default() }, fds: vec![FdKind::Out, FdKind::Ipc] };
        send_frame(a.as_fd(), &Msg::Hello { v: 1, stop_signal: 15, grace_ms: 30_000 }, &[]).unwrap();
        send_frame(a.as_fd(), &spawned, &[r1.as_fd(), r2.as_fd()]).unwrap();
        send_frame(a.as_fd(), &Msg::Gone { pid: 42 }, &[]).unwrap();
        let mut f = Frames::default();
        assert!(f.fill(b.as_fd()).unwrap());
        let (m, fds) = f.next().unwrap().unwrap();
        assert!(matches!(m, Msg::Hello { grace_ms: 30_000, .. }) && fds.is_empty());
        let (m, fds) = f.next().unwrap().unwrap();
        assert_eq!(m, spawned);
        assert_eq!(fds.len(), 2);
        let (m, fds) = f.next().unwrap().unwrap();
        assert_eq!((m, fds.len()), (Msg::Gone { pid: 42 }, 0));
        assert!(f.next().unwrap().is_none());
        drop(a);
        assert!(!f.fill(b.as_fd()).unwrap(), "end of stream");
    }

    /// A frame cut anywhere waits for the rest.
    #[test]
    fn a_partial_frame_waits() {
        let msg = Msg::Exited { pid: 7, code: Some(1), signal: None };
        let body = serde_json::to_vec(&msg).unwrap();
        let mut bytes = (body.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&body);
        for cut in 0..bytes.len() {
            let mut f = Frames::default();
            f.buf.extend_from_slice(&bytes[..cut]);
            assert!(f.next().unwrap().is_none(), "cut at {cut}");
            f.buf.extend_from_slice(&bytes[cut..]);
            assert_eq!(f.next().unwrap().unwrap().0, msg);
        }
        let mut f = Frames::default();
        f.buf.extend_from_slice(&(MAX_FRAME as u32 + 1).to_be_bytes());
        assert!(f.next().is_err(), "an absurd length is an error, not a wait");
    }

    /// The buffer keeps the newest megabyte and counts what it dropped.
    #[test]
    fn the_ring_keeps_the_newest_output() {
        let mut r = Ring::default();
        r.push(&vec![b'a'; RING - 10]);
        r.push(&[b'b'; 30]);
        assert_eq!(r.data.len(), RING);
        assert_eq!(r.dropped, 20);
        assert_eq!(r.data.back(), Some(&b'b'));
        assert_eq!(r.data.front(), Some(&b'a'));
    }

    #[test]
    fn messages_say_how_many_descriptors_they_carry() {
        let kept = Msg::Kept {
            pid: 1,
            start: None,
            age_ms: 0,
            meta: Meta::default(),
            fds: vec![FdKind::Out, FdKind::Err, FdKind::Ipc],
            logs: vec![FdKind::Out],
            dropped: 0,
            exited: None,
        };
        assert_eq!(kept.fd_count(), 4);
        assert_eq!(Msg::Listener { name: "handoff".into() }.fd_count(), 1);
        assert_eq!(Msg::Bye.fd_count(), 0);
        // Every field survives the trip, the sockets map too.
        let mut meta =
            Meta { slot: 2, role: "current".into(), host: Some("::".into()), listening: vec![80], ..Meta::default() };
        meta.sockets.insert(1, PathBuf::from("/run/a.sock"));
        let m = Msg::Spawned { pid: 7, meta, fds: vec![FdKind::Out] };
        let back: Msg = serde_json::from_slice(&serde_json::to_vec(&m).unwrap()).unwrap();
        assert_eq!(back, m);
        // Unknown fields from a newer side are ignored, missing ones default.
        let m: Msg = serde_json::from_str(r#"{"t":"meta","pid":3,"meta":{"slot":2,"later":true}}"#).unwrap();
        assert_eq!(m, Msg::Meta { pid: 3, meta: Meta { slot: 2, ..Meta::default() } });
    }
}

//! The listening side: taking connections off the listener, and pacing the
//! retries when taking one fails.

use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

/// Where TCP connections come from. The listening socket is registered with
/// tokio's reactor (epoll; kqueue outside Linux) so the loop can wait for it,
/// but connections are taken off it here, as bare descriptors: a connection
/// that is answered at once never needs to be registered with the reactor at
/// all (see `first_request`).
pub(super) struct Listener(tokio::io::unix::AsyncFd<std::net::TcpListener>);

impl Listener {
    pub(super) fn new(l: std::net::TcpListener) -> Result<Listener, String> {
        tokio::io::unix::AsyncFd::with_interest(l, tokio::io::Interest::READABLE)
            .map(Listener)
            .map_err(|e| e.to_string())
    }

    pub(super) fn name(&self) -> &'static str {
        if cfg!(target_os = "linux") { "epoll" } else { "kqueue" }
    }

    pub(super) async fn accept(&self) -> std::io::Result<OwnedFd> {
        loop {
            // `AsyncFd::readable` does not count against tokio's cooperative
            // budget (its own `TcpListener::accept` does), and a connection
            // that is answered inline never awaits. With the backlog never
            // empty this loop would then never give the thread back: no
            // heartbeat, no SIGTERM, no idle sweep, no established
            // connection served. Spend one unit here, before anything is
            // accepted, so the future can be suspended (and dropped by the
            // `select!`) without holding a descriptor.
            tokio::task::consume_budget().await;
            let mut ready = self.0.readable().await?;
            if let Ok(r) = ready.try_io(|l| crate::sys::accept_nonblocking(l.get_ref().as_fd())) {
                return r;
            }
        }
    }
}

/// After a failed accept, the next comes at once for the first two in a
/// row, then after a pause that doubles from this …
const ACCEPT_PAUSE_MIN: Duration = Duration::from_millis(5);
/// … to this: a lack of descriptors or memory must not spin a CPU (the
/// listener stays ready, the connection waiting in its backlog).
const ACCEPT_PAUSE_MAX: Duration = Duration::from_secs(1);
/// Failed accepts are logged at most this often, with how many there were.
const ACCEPT_LOG_EVERY: Duration = Duration::from_secs(10);

/// Failed accepts on one listener (both I/O modes, and the health socket).
pub(super) struct AcceptErrors {
    /// What it accepts ("connections").
    what: &'static str,
    /// How ("epoll", "kqueue").
    via: &'static str,
    /// Failures in a row.
    streak: u32,
    /// Failures since the last log line.
    unlogged: u64,
    logged_at: Option<Instant>,
    /// A failure of this streak was logged: say when it ends.
    told: bool,
    /// Accepting is paused until then.
    pub(super) resume_at: Option<tokio::time::Instant>,
}

impl AcceptErrors {
    pub(super) fn new(what: &'static str, via: &'static str) -> AcceptErrors {
        AcceptErrors { what, via, streak: 0, unlogged: 0, logged_at: None, told: false, resume_at: None }
    }

    /// Until `until` (never, without one).
    pub(super) async fn pause(until: Option<tokio::time::Instant>) {
        match until {
            Some(t) => tokio::time::sleep_until(t).await,
            None => std::future::pending().await,
        }
    }

    pub(super) fn accepted(&mut self) {
        if self.told {
            crate::info!(
                format!("accepting {} again", self.what),
                via = self.via,
                after_failures = self.streak,
                pid = std::process::id()
            );
            self.told = false;
        }
        self.streak = 0;
    }

    pub(super) fn failed(&mut self, e: &std::io::Error) {
        self.streak = self.streak.saturating_add(1);
        let pause = accept_pause(self.streak);
        if !pause.is_zero() {
            self.resume_at = Some(tokio::time::Instant::now() + pause);
        }
        // A client that left before it was accepted is routine, unless it
        // keeps happening.
        if accept_error_is_transient(e) && self.streak < 10 {
            return;
        }
        self.unlogged += 1;
        if self.logged_at.is_some_and(|t| t.elapsed() < ACCEPT_LOG_EVERY) {
            return;
        }
        crate::warn!(
            format!("cannot accept {}; they wait in the listen backlog while this worker retries", self.what),
            error = e,
            via = self.via,
            failed = self.unlogged,
            retry_in_ms = pause.as_millis(),
            open_files_limit = crate::sys::nofile_limit().0,
            pid = std::process::id(),
            hint = accept_hint(e),
        );
        self.unlogged = 0;
        self.logged_at = Some(Instant::now());
        self.told = true;
    }
}

/// The pause before accepting again after `streak` failures in a row.
fn accept_pause(streak: u32) -> Duration {
    if streak <= 2 {
        return Duration::ZERO;
    }
    (ACCEPT_PAUSE_MIN * (1u32 << (streak - 3).min(16))).min(ACCEPT_PAUSE_MAX)
}

/// accept(2) errors about one connection, not the listener (Linux passes
/// on the new socket's pending network errors): the next one may well work.
fn accept_error_is_transient(e: &std::io::Error) -> bool {
    matches!(
        e.raw_os_error(),
        Some(
            libc::ECONNABORTED
                | libc::EINTR
                | libc::EAGAIN
                | libc::EPROTO
                | libc::EPERM
                | libc::ENETDOWN
                | libc::ENETUNREACH
                | libc::EHOSTUNREACH
                | libc::ENOPROTOOPT
                | libc::EOPNOTSUPP
        )
    )
}

fn accept_hint(e: &std::io::Error) -> &'static str {
    match e.raw_os_error() {
        Some(libc::EMFILE) => {
            "this worker has used up its open-file limit (each connection takes a descriptor; up to 10000 per \
             worker): raise the limit Warden and its workers run with, LimitNOFILE=65536 in the systemd unit \
             (`systemctl edit <unit>`) or `ulimit -n 65536` in the shell that runs `warden start`; `warden doctor` \
             shows it"
        }
        Some(libc::ENFILE) => {
            "the whole system is out of file descriptors: raise fs.file-max (`sysctl fs.file-max`), or find what \
             holds them (`lsof -n | wc -l`)"
        }
        Some(libc::ENOBUFS | libc::ENOMEM) => {
            "the kernel is short of memory for sockets: check the host's and the cgroup's memory (`free -m`, \
             memory.max) and `sysctl net.ipv4.tcp_mem`"
        }
        _ => "accept(2) keeps failing on this listener; if it persists, `warden restart <app>` starts fresh workers",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn failed_accepts_back_off_and_are_logged_sparingly() {
        let ms = |n| accept_pause(n).as_millis();
        assert_eq!([ms(1), ms(2), ms(3), ms(4), ms(5), ms(9)], [0, 0, 5, 10, 20, 320]);
        assert_eq!((ms(10), ms(11), ms(u32::MAX)), (640, 1000, 1000));
        let e = |n| std::io::Error::from_raw_os_error(n);
        assert!(accept_error_is_transient(&e(libc::ECONNABORTED)));
        assert!(!accept_error_is_transient(&e(libc::EMFILE)));
        assert!(accept_hint(&e(libc::EMFILE)).contains("LimitNOFILE=65536"));
        assert!(accept_hint(&e(libc::ENFILE)).contains("fs.file-max"));

        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let mut a = AcceptErrors::new("connections", "epoll");
            a.failed(&e(libc::EMFILE));
            assert!(a.resume_at.is_none(), "the first retry is at once");
            a.failed(&e(libc::EMFILE));
            a.failed(&e(libc::EMFILE));
            assert!(a.resume_at.is_some(), "then it pauses");
            assert_eq!((a.unlogged, a.told), (2, true), "logged once, the rest counted for the next line");
            a.accepted();
            assert_eq!((a.streak, a.told), (0, false));
            // A client gone before its accept: retried at once, not logged.
            let mut b = AcceptErrors::new("connections", "epoll");
            b.failed(&e(libc::ECONNABORTED));
            assert!(b.resume_at.is_none() && b.logged_at.is_none());
        });
    }

    /// With the backlog never empty and every connection answered at once,
    /// the accept loop must still give the thread back now and then.
    #[tokio::test(flavor = "current_thread")]
    async fn accepting_from_a_full_backlog_lets_other_tasks_run() {
        let l = crate::sys::listen_tcp("127.0.0.1:0".parse().unwrap(), false, 1024).unwrap();
        let addr = l.local_addr().unwrap();
        let mut clients = Vec::new();
        for _ in 0..400 {
            match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(1)) {
                Ok(c) => clients.push(c),
                Err(_) => break,
            }
        }
        if clients.len() < 300 {
            eprintln!("skipped: this system queues only {} connections", clients.len());
            return;
        }
        let listener = Listener::new(l).unwrap();
        // The first wait for readiness suspends the task whatever else is
        // done (the reactor has not looked yet); start counting after it.
        let mut taken = vec![listener.accept().await.unwrap()];
        let ran = Arc::new(AtomicBool::new(false));
        let flag = ran.clone();
        tokio::spawn(async move { flag.store(true, Ordering::SeqCst) });
        while !ran.load(Ordering::SeqCst) && taken.len() < clients.len() {
            taken.push(listener.accept().await.unwrap());
        }
        assert!(ran.load(Ordering::SeqCst), "{} connections taken before any other task had a turn", taken.len());
        assert!(taken.len() < 200, "{} connections taken before another task ran", taken.len());
    }
}

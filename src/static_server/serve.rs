//! The worker's life: bind the listener, report to the supervisor, accept
//! connections, and drain on the stop signal.

use super::Site;
use super::accept::{AcceptErrors, Listener};
use super::conn::Conn;
use super::connection::{First, Resume, connection, first_request};
use super::head::HeadBuf;
use super::open::{OPEN_BENEATH, OPEN_CACHED, OPEN_LEGACY};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Connections one worker serves at once; more are dropped when accepted.
const MAX_CONNECTIONS: usize = 10_000;

/// One JSON line to Warden on the IPC socket (fd from WARDEN_IPC_FD). The
/// socket never blocks (Warden may be stopped, `sys::prepare_ipc_socket`):
/// a heartbeat that finds it full is dropped (the next one says the same),
/// anything else waits in `IPC_BACKLOG`, in order, until there is room.
fn report(msg: serde_json::Value) {
    let Some(fd) = std::env::var("WARDEN_IPC_FD").ok().and_then(|v| v.parse::<i32>().ok()) else { return };
    let droppable = msg["ev"] == "heartbeat";
    let mut line = msg.to_string();
    line.push('\n');
    let mut backlog = IPC_BACKLOG.lock().unwrap_or_else(|e| e.into_inner());
    let was_empty = backlog.is_empty();
    if ipc_queue(fd, &mut backlog, line.as_bytes(), droppable) && was_empty {
        // Retried off the event loop until it is all out.
        let _ = std::thread::Builder::new().name("ipc-retry".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let mut backlog = IPC_BACKLOG.lock().unwrap_or_else(|e| e.into_inner());
                if !ipc_flush(fd, &mut backlog) {
                    return;
                }
            }
        });
    }
}

static IPC_BACKLOG: std::sync::Mutex<Vec<u8>> = std::sync::Mutex::new(Vec::new());

/// Lines beyond this many waiting bytes are dropped (they never pile up in
/// practice: the static server sends few that are not heartbeats).
const IPC_BACKLOG_MAX: usize = 1 << 20;

/// Send `line` after whatever `backlog` holds, keeping in `backlog` what
/// does not fit (a droppable line that finds no room is dropped whole).
/// True when something is left waiting.
fn ipc_queue(fd: i32, backlog: &mut Vec<u8>, line: &[u8], droppable: bool) -> bool {
    if ipc_flush(fd, backlog) {
        if !droppable && backlog.len() < IPC_BACKLOG_MAX {
            backlog.extend_from_slice(line);
        }
        return true;
    }
    let sent = ipc_write(fd, line);
    if sent < line.len() && !(droppable && sent == 0) {
        backlog.extend_from_slice(&line[sent..]);
    }
    !backlog.is_empty()
}

/// Send what `backlog` holds; true when some of it is still waiting.
fn ipc_flush(fd: i32, backlog: &mut Vec<u8>) -> bool {
    let sent = ipc_write(fd, backlog);
    backlog.drain(..sent);
    !backlog.is_empty()
}

/// Write as much of `buf` as the socket takes now. An error other than a
/// full socket (Warden is gone) counts as all sent: there is no one to wait for.
fn ipc_write(fd: i32, buf: &[u8]) -> usize {
    let mut sent = 0;
    while sent < buf.len() {
        match crate::sys::write_fd(fd, &buf[sent..]) {
            Ok(0) => return buf.len(),
            Ok(n) => sent += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return sent,
            Err(_) => return buf.len(),
        }
    }
    sent
}

/// A number from the environment, when the variable is set and holds one.
fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok()?.parse().ok()
}

/// The TCP listener, and what binding it found out about the system.
struct Tcp {
    listener: Listener,
    /// The request of a new connection is already in the socket when it is
    /// accepted (Linux, TCP_DEFER_ACCEPT: the connection is handed over once
    /// its data has arrived), so the accept loop itself can answer it
    /// (`first_request`), with no task and no epoll registration for a
    /// connection that only needs one answer. `WARDEN_STATIC_INLINE=0` turns
    /// that off (tests, troubleshooting).
    inline_first: bool,
    /// TCP_NODELAY (responses go out at once, never held back for Nagle's
    /// algorithm) is set once on the listener, and Linux hands it to every
    /// connection it accepts, so there is no setsockopt per connection. Where
    /// that is not the rule (or setting it failed) each connection gets it
    /// when accepted.
    nodelay_inherited: bool,
}

impl Tcp {
    fn bind(site: &Site, port: u16) -> Result<Tcp, String> {
        let host = &site.cfg.host;
        let ip: std::net::IpAddr = host.parse().map_err(|_| format!("static.host {host:?} is not an IP address"))?;
        let addr = std::net::SocketAddr::new(ip, port);
        let reuse_port = std::env::var("WARDEN_REUSE_PORT").is_ok_and(|v| v == "1");
        let std_listener =
            crate::sys::listen_tcp(addr, reuse_port, 1024).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
        // Wake up for a connection only once its request has arrived.
        // Optional: without it the server is just as correct, a little busier.
        let deferred = crate::sys::tcp_defer_accept(std_listener.as_fd(), site.head_timeout.as_secs() as i32).is_ok();
        let inline_first =
            cfg!(target_os = "linux") && deferred && std::env::var("WARDEN_STATIC_INLINE").as_deref() != Ok("0");
        let nodelay_inherited =
            crate::sys::NODELAY_INHERITED && crate::sys::set_tcp_nodelay(std_listener.as_fd(), true).is_ok();
        Ok(Tcp { listener: Listener::new(std_listener)?, inline_first, nodelay_inherited })
    }
}

/// The private socket for this worker's health checks, when Warden asked for
/// one (WARDEN_HEALTH_DIR).
struct Health {
    listener: tokio::net::UnixListener,
    path: PathBuf,
}

impl Health {
    fn bind(worker: u64) -> Option<Health> {
        let dir = std::env::var("WARDEN_HEALTH_DIR").ok()?;
        let app = std::env::var("WARDEN_APP").unwrap_or_else(|_| "app".into());
        let inst = std::env::var("WARDEN_INSTANCE").unwrap_or_else(|_| std::process::id().to_string());
        let path = PathBuf::from(dir).join(format!("{app}.h{inst}-{worker}.sock"));
        // A socket path must fit sockaddr_un.
        if path.as_os_str().len() > 100 {
            return None;
        }
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).ok()?;
        Some(Health { listener, path })
    }

    async fn accept(this: Option<&Health>) -> std::io::Result<tokio::net::UnixStream> {
        match this {
            Some(h) => h.listener.accept().await.map(|(stream, _)| stream),
            None => std::future::pending().await,
        }
    }
}

pub(super) async fn serve(site: Arc<Site>, port: u16) -> Result<(), String> {
    let tcp = Tcp::bind(&site, port)?;
    let worker: u64 = env_number("WARDEN_WORKER_ID").unwrap_or(0);
    let health = Health::bind(worker);
    announce(&site, &tcp, port, worker, health.as_ref());
    spawn_timers(&site, worker);
    if let Some(c) = &site.compressor {
        tokio::spawn(c.clone().run());
    }

    let stop_name = std::env::var("WARDEN_STOP_SIGNAL").unwrap_or_else(|_| "SIGTERM".into());
    let stop_sig = crate::signals::parse(&stop_name).unwrap_or(libc::SIGTERM);
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(stop_sig))
        .map_err(|e| format!("installing the {stop_name} handler: {e}"))?;

    accept_loop(&site, &tcp, health.as_ref(), &mut stop).await;
    drain(&site, tcp, health, worker).await;
    if let Some(c) = &site.compressor {
        c.stop().await;
    }
    summary(&site);
    Ok(())
}

/// Tell Warden the worker is listening, and say so on stdout.
fn announce(site: &Site, tcp: &Tcp, port: u16, worker: u64, health: Option<&Health>) {
    let mut msg = serde_json::json!({"ev": "listening", "port": port, "worker": worker});
    if let Some(h) = health {
        msg["socket"] = h.path.display().to_string().into();
    }
    report(msg);
    let how = match site.open_mode.load(Ordering::Relaxed) {
        OPEN_CACHED => "openat2, cache-first",
        OPEN_BENEATH => "openat2",
        _ => super::open::legacy_open_name(),
    };
    let cache = match &site.cache {
        Some(_) => format!("cache {} KB per worker", site.cfg.cache_size >> 10),
        None => "no cache".to_string(),
    };
    println!(
        "serving {} on {}:{port} (files opened with {how}) via {}, {cache}",
        site.root.display(),
        site.cfg.host,
        tcp.listener.name()
    );
    if let Some(c) = &site.compressor {
        println!(
            "compressing files in the background: up to {} processes at a time at low priority, copies in {} (up to {} MB)",
            c.jobs(),
            c.store().path.display(),
            site.cfg.compress_dir_size >> 20
        );
    }
}

/// The heartbeat Warden asks for (WARDEN_HEARTBEAT_MS), and the one sweep a
/// second that enforces the limits on connections waiting for a request
/// (idle.rs) in place of a timer per request.
fn spawn_timers(site: &Site, worker: u64) {
    let beat_ms: u64 = env_number("WARDEN_HEARTBEAT_MS").unwrap_or(0);
    if beat_ms > 0 {
        let responses = site.responses.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(beat_ms));
            loop {
                t.tick().await;
                let mut beat = serde_json::json!({"ev": "heartbeat", "worker": worker});
                // The responses since the worker started, by status.
                if let Some(r) = &responses {
                    beat["req"] = r.json();
                }
                report(beat);
            }
        });
    }
    let idle = site.idle.clone();
    tokio::spawn(async move {
        let mut t = tokio::time::interval(Duration::from_secs(1));
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            idle.sweep();
        }
    });
}

/// Take connections until the stop signal.
async fn accept_loop(site: &Arc<Site>, tcp: &Tcp, health: Option<&Health>, stop: &mut tokio::signal::unix::Signal) {
    let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let mut tcp_errors = AcceptErrors::new("connections", tcp.listener.name());
    let mut health_errors = AcceptErrors::new("health checks", "its health socket");
    loop {
        tokio::select! {
            _ = stop.recv() => break,
            _ = AcceptErrors::pause(tcp_errors.resume_at), if tcp_errors.resume_at.is_some() => tcp_errors.resume_at = None,
            _ = AcceptErrors::pause(health_errors.resume_at), if health_errors.resume_at.is_some() => health_errors.resume_at = None,
            accepted = tcp.listener.accept(), if tcp_errors.resume_at.is_none() => match accepted {
                Ok(fd) => {
                    tcp_errors.accepted();
                    new_connection(site, tcp, &slots, fd);
                }
                Err(e) => tcp_errors.failed(&e),
            },
            accepted = Health::accept(health), if health_errors.resume_at.is_none() => match accepted {
                Ok(stream) => {
                    health_errors.accepted();
                    new_health_connection(site, stream);
                }
                Err(e) => health_errors.failed(&e),
            },
        }
    }
}

/// A connection just accepted: answered on the spot when it can be
/// (`first_request`), else served by a task of its own.
fn new_connection(site: &Arc<Site>, tcp: &Tcp, slots: &Arc<Semaphore>, fd: OwnedFd) {
    // At the limit a connection is dropped before anything is sent (an
    // answer promising keep-alive, then a close, is worse). Only this loop
    // takes permits, so one free now is still free below.
    if slots.available_permits() == 0 {
        return;
    }
    if !tcp.nodelay_inherited {
        let _ = crate::sys::set_tcp_nodelay(fd.as_fd(), true);
    }
    let _ = crate::sys::set_nosigpipe(fd.as_fd());
    // Without openat2 every open is a call on a thread (`open_checked`): the
    // quick path would wait for it, give up, and the task would do it a
    // second time.
    let inline = tcp.inline_first && site.open_mode.load(Ordering::Relaxed) != OPEN_LEGACY;
    let (fd, resume) = match first_request(site, fd, inline) {
        First::Done => return,
        First::Go { fd, resume } => (fd, resume),
    };
    let Ok(permit) = slots.clone().try_acquire_owned() else { return };
    let Ok(stream) = tokio::net::TcpStream::from_std(std::net::TcpStream::from(fd)) else { return };
    let site = site.clone();
    let guard = site.idle.register(stream.as_raw_fd());
    tokio::spawn(async move {
        let _permit = permit;
        // Locals drop in reverse order: the guard (which takes the connection
        // out of the idle sweep) goes before the stream closes its descriptor.
        let mut stream = stream;
        let guard = guard;
        // Borrowed halves: dropping them does nothing, and the stream's own
        // drop is just close(2). Owned halves shut the write side down first,
        // one more system call for every connection (the FIN is the same
        // either way).
        let (r, w) = stream.split();
        connection(r, Conn::Tcp(w), site, &guard, resume).await;
    });
}

/// A health check on the private socket.
fn new_health_connection(site: &Arc<Site>, stream: tokio::net::UnixStream) {
    let site = site.clone();
    let guard = site.idle.register(stream.as_raw_fd());
    tokio::spawn(async move {
        let mut stream = stream;
        let guard = guard;
        let (r, w) = stream.split();
        let head = HeadBuf::take(&site);
        connection(r, Conn::Unix(w), site, &guard, Resume::new(head)).await;
    });
}

/// Stop accepting; answer whatever arrives on open connections with
/// `Connection: close` (see `connection`), and let requests in flight finish
/// (at least WARDEN_DRAIN_MS, at most ~grace).
async fn drain(site: &Site, tcp: Tcp, health: Option<Health>, worker: u64) {
    drop(tcp);
    site.draining.store(true, Ordering::SeqCst);
    report(serde_json::json!({"ev": "draining", "worker": worker}));
    let drain_ms: u64 = env_number("WARDEN_DRAIN_MS").unwrap_or(500);
    // A connection spawned in the last turn of the accept loop has not run
    // yet, so it has not counted itself as in flight: let it take its first
    // step.
    tokio::task::yield_now().await;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(drain_ms) || site.active.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Some(h) = health {
        let _ = std::fs::remove_file(h.path);
    }
}

/// What the worker counted, on stdout as it exits.
fn summary(site: &Site) {
    if let Some(c) = &site.cache {
        let (files, used, cap) = c.usage();
        println!(
            "static cache: {} hits, {} misses, {} dropped as changed on disk, {} evicted; {files} files in {} of {} KB \
             ({} of them in memfds)",
            c.hits.load(Ordering::Relaxed),
            c.misses.load(Ordering::Relaxed),
            c.stale.load(Ordering::Relaxed),
            c.evicted.load(Ordering::Relaxed),
            used.div_ceil(1024),
            cap >> 10,
            c.memfds()
        );
    }
    if let Some(c) = &site.compressor {
        println!("static compression: {}", c.report());
    }
    println!("static: {} requests answered in the accept loop", site.inline.load(Ordering::Relaxed));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// With Warden not reading, a full IPC socket drops heartbeats and keeps
    /// every other line, whole and in order, for when Warden reads again.
    #[test]
    fn a_full_ipc_socket_drops_heartbeats_and_keeps_the_rest() {
        let (ours, theirs) = crate::sys::socketpair_cloexec().unwrap();
        crate::sys::prepare_ipc_socket(ours.as_fd(), theirs.as_fd(), true).unwrap();
        let fd = theirs.as_raw_fd();
        let filler = [b'.'; 4096];
        let mut filled = 0;
        while ipc_write(fd, &filler) == filler.len() {
            filled += filler.len();
        }
        let mut backlog = Vec::new();
        assert!(!ipc_queue(fd, &mut backlog, b"{\"ev\":\"heartbeat\"}\n", true), "a heartbeat is dropped");
        assert!(ipc_queue(fd, &mut backlog, b"{\"ev\":\"draining\"}\n", false));
        assert!(ipc_queue(fd, &mut backlog, b"{\"ev\":\"heartbeat\"}\n", true), "still waiting");
        assert!(ipc_queue(fd, &mut backlog, b"{\"ev\":\"bye\"}\n", false));
        // Warden reads everything; the backlog goes out.
        let mut warden = std::os::unix::net::UnixStream::from(ours);
        warden.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 1 << 16];
        for _ in 0..10_000 {
            match warden.read(&mut buf) {
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(_) => {
                    if !ipc_flush(fd, &mut backlog) && got.ends_with(b"{\"ev\":\"bye\"}\n") {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        assert!(got.len() >= filled);
        let text = String::from_utf8_lossy(&got);
        assert_eq!(text.trim_start_matches('.'), "{\"ev\":\"draining\"}\n{\"ev\":\"bye\"}\n");
    }
}

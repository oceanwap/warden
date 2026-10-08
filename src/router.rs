//! Warden's hostname router: the worker of an app with a `[route]` section
//! (`docs/routing.md`). It listens on the app's port (443), reads the
//! hostname a TLS connection asks for from its ClientHello (`hello`), and
//! passes the still-encrypted connection to the port of the app that serves
//! that hostname (`pipe`). Nothing is decrypted: each app does its own TLS
//! (and HTTP/2), with its own certificate.
//!
//! With `client_ip` (Linux, the default there) the connection to the app is
//! made from the visitor's own address (IP_TRANSPARENT), so the app sees the
//! visitor's IP as its peer. The app's answers come back to the router
//! through two routing rules the supervisor keeps (`rules`).
//!
//! One thread per worker; Warden runs one worker per core by default and the
//! kernel spreads connections over them (SO_REUSEPORT).

mod hello;
mod pipe;
#[cfg(target_os = "linux")]
pub(crate) mod rules;

use crate::config::{Config, Route, RouteTarget};
use crate::static_server::serve::{Health, env_number, report};
use hello::Hello;
use std::collections::BTreeMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Interest};
use tokio::net::{TcpSocket, TcpStream};

/// The largest first record a client may send (TLS caps a plaintext record
/// at 2^14 bytes). A ClientHello is a few hundred bytes to a few KB.
const MAX_HELLO: usize = hello::RECORD_HEADER + (1 << 14);
/// How long a new connection may take to send its ClientHello.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
/// How long connecting to an app may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What a router worker routes: `[route]` with app names turned into ports.
/// Made by the supervisor at each worker start (`WARDEN_ROUTE`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Table {
    /// Hostname (`a.example.com`, `*.example.com`, `*`) → where it goes.
    pub routes: BTreeMap<String, Dest>,
    /// Apps named in `route.hosts` with no config (or no `app.port`) here.
    #[serde(default)]
    pub missing: Vec<String>,
    pub client_ip: bool,
    pub host: String,
}

/// Where a hostname's connections go.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Dest {
    pub port: u16,
    /// The app's own address (`[app] address`): bytes passed go there, and
    /// never from the visitor's address (routing rules for its port would
    /// also catch the answers to the visitors who reach the app directly).
    /// Several apps can share a port this way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub address: Option<IpAddr>,
    /// The `route.sock` of the app: its connections are handed to the app's
    /// workers when its supervisor takes them (`crate::handoff`), and passed
    /// byte by byte otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handoff: Option<PathBuf>,
}

impl Dest {
    fn port(port: u16) -> Dest {
        Dest { port, ..Dest::default() }
    }
}

impl std::fmt::Display for Dest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.address {
            Some(ip) => write!(f, "{}", SocketAddr::new(ip, self.port)),
            None => write!(f, "{}", self.port),
        }
    }
}

/// The table for `[route]`: app names are looked up in this host's app
/// configs (their `app.port`).
pub fn table(r: &Route) -> Table {
    table_with(r, &apps())
}

fn table_with(r: &Route, apps: &[Config]) -> Table {
    let mut t = Table { client_ip: r.client_ip(), host: r.host.clone(), ..Table::default() };
    let app = |c: &Config, port: u16| Dest { port, address: c.app.address, handoff: Some(handoff_socket(c)) };
    for (host, target) in &r.hosts {
        let dest = match target {
            // A port: the app on it, if one here has it (and no address of its own).
            RouteTarget::Port(p) => apps
                .iter()
                .find(|c| c.app.port == Some(*p) && c.route.is_none() && c.app.address.is_none())
                .map_or_else(|| Dest::port(*p), |c| app(c, *p)),
            RouteTarget::App(name) => match apps.iter().find(|c| &c.app.name == name && c.route.is_none()) {
                Some(c) if c.app.port.is_some() => app(c, c.app.port.unwrap_or(0)),
                _ => {
                    if !t.missing.contains(name) {
                        t.missing.push(name.clone());
                    }
                    continue;
                }
            },
        };
        t.routes.insert(host.clone(), dest);
    }
    t
}

/// Every app config on this host (that loads).
fn apps() -> Vec<Config> {
    crate::fleet::discover().into_iter().filter_map(|a| Config::load(a.config.as_ref()?).ok()).collect()
}

/// Where an app's supervisor takes connections from the router.
pub fn handoff_socket(cfg: &Config) -> PathBuf {
    cfg.socket_path().with_file_name("route.sock")
}

/// Whether a `[route]` app on this host sends connections to `cfg`'s app
/// (by its name or its port): its workers then take them handed over.
/// Read when the app's supervisor starts.
pub fn routed(cfg: &Config) -> bool {
    if cfg.route.is_some() || cfg.app.port.is_none() {
        return false;
    }
    apps().iter().filter_map(|c| c.route.as_ref()).any(|r| {
        r.hosts.values().any(|t| match t {
            RouteTarget::App(n) => *n == cfg.app.name,
            RouteTarget::Port(p) => Some(*p) == cfg.app.port,
        })
    })
}

impl Table {
    /// Where a hostname goes: the exact name, then `*.` and the name
    /// without its first label, then `*` (which also takes connections that
    /// name no host).
    fn lookup(&self, host: Option<&str>) -> Option<&Dest> {
        if let Some(h) = host {
            if let Some(d) = self.routes.get(h) {
                return Some(d);
            }
            if let Some((_, rest)) = h.split_once('.') {
                if let Some(d) = self.routes.get(&format!("*.{rest}")) {
                    return Some(d);
                }
            }
        }
        self.routes.get("*")
    }

    /// The ports `client_ip`'s routing rules are for: those connections go
    /// to on loopback (not the apps with their own address).
    pub fn rule_ports(&self) -> Vec<u16> {
        let mut p: Vec<u16> = self.routes.values().filter(|d| d.address.is_none()).map(|d| d.port).collect();
        p.sort_unstable();
        p.dedup();
        p
    }
}

pub fn main() -> i32 {
    let run = || -> Result<(), (i32, String)> {
        let text = std::env::var("WARDEN_ROUTE")
            .map_err(|_| (78, "WARDEN_ROUTE is not set: the router is started by Warden ([route])".to_string()))?;
        let table: Table = serde_json::from_str(&text).map_err(|e| (78, format!("WARDEN_ROUTE: {e}")))?;
        let port: u16 = env_number("PORT").ok_or((78, "PORT is not set; set app.port in the config".to_string()))?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| (1, format!("cannot start the runtime: {e}")))?;
        rt.block_on(serve(Arc::new(table), port)).map_err(|e| (1, e))
    };
    match run() {
        Ok(()) => 0,
        Err((code, e)) => {
            eprintln!("warden route: {e}");
            code
        }
    }
}

async fn serve(table: Arc<Table>, port: u16) -> Result<(), String> {
    let ip: IpAddr = table.host.parse().map_err(|_| format!("route.host {:?} is not an IP address", table.host))?;
    let addr = SocketAddr::new(ip, port);
    let reuse_port = std::env::var("WARDEN_REUSE_PORT").is_ok_and(|v| v == "1");
    let std_listener =
        crate::sys::listen_tcp(addr, reuse_port, 4096).map_err(|e| format!("cannot listen on {addr}: {e}"))?;
    // Accepted connections inherit it on Linux; elsewhere each gets it below.
    let _ = crate::sys::set_tcp_nodelay(std_listener.as_fd(), true);
    // Wake up for a connection once its ClientHello has arrived (Linux).
    let _ = crate::sys::tcp_defer_accept(std_listener.as_fd(), HELLO_TIMEOUT.as_secs() as i32);
    std_listener.set_nonblocking(true).map_err(|e| format!("{addr}: {e}"))?;
    let listener = tokio::net::TcpListener::from_std(std_listener).map_err(|e| format!("{addr}: {e}"))?;
    let transparent = table.client_ip && cfg!(target_os = "linux");
    if transparent {
        can_be_transparent().map_err(|e| {
            format!(
                "route.client_ip: cannot connect from the visitor's address ({e}): run Warden as root \
                 (CAP_NET_ADMIN), or set client_ip = false under [route]"
            )
        })?;
    }

    let worker: u64 = env_number("WARDEN_WORKER_ID").unwrap_or(0);
    let health = Health::bind(worker);
    let mut msg = serde_json::json!({"ev": "listening", "port": port, "worker": worker});
    if let Some(h) = &health {
        msg["socket"] = h.path.display().to_string().into();
    }
    report(msg);
    let names: Vec<String> = table.routes.iter().map(|(h, p)| format!("{h} → {p}")).collect();
    println!(
        "routing {addr} by hostname: {}{}",
        names.join(", "),
        if transparent { " (apps see the visitor's address)" } else { "" }
    );

    let handovers = Arc::new(Handovers::new(&table));
    let active = Arc::new(AtomicUsize::new(0));
    let beat_ms: u64 = env_number("WARDEN_HEARTBEAT_MS").unwrap_or(0);
    if beat_ms > 0 {
        let active = active.clone();
        tokio::spawn(async move {
            let mut t = tokio::time::interval(Duration::from_millis(beat_ms));
            loop {
                t.tick().await;
                report(
                    serde_json::json!({"ev": "heartbeat", "worker": worker, "conns": active.load(Ordering::Relaxed)}),
                );
            }
        });
    }

    let stop_name = std::env::var("WARDEN_STOP_SIGNAL").unwrap_or_else(|_| "SIGTERM".into());
    let stop_sig = crate::signals::parse(&stop_name).unwrap_or(libc::SIGTERM);
    let mut stop = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::from_raw(stop_sig))
        .map_err(|e| format!("installing the {stop_name} handler: {e}"))?;

    loop {
        tokio::select! {
            _ = stop.recv() => break,
            accepted = listener.accept() => match accepted {
                Ok((c, peer)) => {
                    let (table, handovers) = (table.clone(), handovers.clone());
                    let guard = Active::new(&active);
                    tokio::spawn(async move {
                        let _guard = guard;
                        let _ = connection(c, peer, &table, &handovers, transparent).await;
                    });
                }
                // Out of descriptors, or a connection reset before it was
                // taken: wait a moment instead of spinning on the error.
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            },
            accepted = Health::accept(health.as_ref()) => {
                if let Ok(s) = accepted {
                    tokio::spawn(health_check(s));
                }
            }
        }
    }

    // Drain: no new connections; the open ones run until they end (a
    // pass-through cannot ask a client to go away), at least WARDEN_DRAIN_MS
    // and at most until Warden's grace period runs out.
    drop(listener);
    report(serde_json::json!({"ev": "draining", "worker": worker}));
    let drain_ms: u64 = env_number("WARDEN_DRAIN_MS").unwrap_or(500);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(drain_ms) || active.load(Ordering::SeqCst) > 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    if let Some(h) = health {
        let _ = std::fs::remove_file(h.path);
    }
    Ok(())
}

/// Counts a connection as open for as long as it lives.
struct Active(Arc<AtomicUsize>);

impl Active {
    fn new(n: &Arc<AtomicUsize>) -> Active {
        n.fetch_add(1, Ordering::SeqCst);
        Active(n.clone())
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Warden's health check on the private socket: the router is up if it
/// answers. Any request gets 200.
async fn health_check(mut s: tokio::net::UnixStream) {
    let mut buf = [0u8; 1024];
    let mut seen = Vec::new();
    let read = async {
        loop {
            match s.read(&mut buf).await {
                Ok(0) | Err(_) => return false,
                Ok(n) => {
                    seen.extend_from_slice(&buf[..n]);
                    if memchr::memmem::find(&seen, b"\r\n\r\n").is_some() || seen.len() > 16 * 1024 {
                        return true;
                    }
                }
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(10), read).await == Ok(true) {
        let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n").await;
    }
}

/// One visitor's connection: peek at the ClientHello (nothing is read off
/// the socket), then hand the connection to the app's workers when the app
/// takes it, else connect to the app and pass bytes both ways until both
/// are done.
async fn connection(
    c: TcpStream,
    peer: SocketAddr,
    table: &Table,
    handovers: &Handovers,
    transparent: bool,
) -> io::Result<()> {
    if !crate::sys::NODELAY_INHERITED {
        let _ = c.set_nodelay(true);
    }
    let host = tokio::time::timeout(HELLO_TIMEOUT, peek_hello(&c))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let Some(dest) = table.lookup(host.as_deref()) else { return Ok(()) };
    let port = dest.port;
    if handovers.give(dest, &c).await {
        // The app's worker has it; our copy closes, the connection stays.
        return Ok(());
    }

    let peer_ip = peer.ip().to_canonical();
    let (target, from) = if let Some(ip) = dest.address {
        // An app on its own address: the router's own address as the source.
        (SocketAddr::new(ip, port), None)
    } else if transparent && !peer_ip.is_loopback() {
        // The app's port on the address the visitor reached: from the
        // visitor's address, a loopback target would be a martian.
        (SocketAddr::new(c.local_addr()?.ip().to_canonical(), port), Some(peer_ip))
    } else {
        (SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port), None)
    };
    let sock = if target.is_ipv4() { TcpSocket::new_v4()? } else { TcpSocket::new_v6()? };
    if let Some(ip) = from {
        bind_transparent(&sock, ip)?;
    }
    let up = tokio::time::timeout(CONNECT_TIMEOUT, sock.connect(target))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    up.set_nodelay(true)?;
    // The ClientHello is still in `c`: the pipe passes it on first.
    pipe::both(&c, &up).await;
    Ok(())
}

/// The hostname a connection's ClientHello asks for, read with MSG_PEEK so
/// the hello stays in the socket for whoever serves it.
async fn peek_hello(c: &TcpStream) -> io::Result<Option<String>> {
    let mut buf = vec![0u8; 2048];
    loop {
        c.readable().await?;
        let n = match c.try_io(Interest::READABLE, || crate::sys::peek(c.as_fd(), &mut buf)) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        };
        match hello::parse(&buf[..n]) {
            Hello::Done(host) => return Ok(host),
            Hello::NotTls => return Err(io::ErrorKind::InvalidData.into()),
            Hello::Need(need) if need > MAX_HELLO => return Err(io::ErrorKind::InvalidData.into()),
            // More of the hello is already there: look again, with room.
            Hello::Need(need) if n == buf.len() => buf.resize(need, 0),
            // The rest is on its way (a hello in two segments). A peek
            // leaves the socket readable, so wait a moment, not for readiness.
            Hello::Need(_) => tokio::time::sleep(Duration::from_millis(1)).await,
        }
    }
}

/// The router worker's connections to app supervisors' `route.sock`, one
/// per app port, opened at first use.
struct Handovers {
    /// Each app's `route.sock` → the link to its supervisor.
    apps: BTreeMap<PathBuf, tokio::sync::Mutex<Link>>,
}

#[derive(Default)]
struct Link {
    stream: Option<tokio::net::UnixStream>,
    /// After a failed connect: bytes are passed until then, without trying.
    retry_at: Option<Instant>,
}

/// How long the router passes bytes before trying an app's socket again.
const HANDOFF_RETRY: Duration = Duration::from_secs(1);

impl Handovers {
    fn new(t: &Table) -> Handovers {
        let apps = t.routes.values().filter_map(|d| d.handoff.clone()).map(|p| (p, Default::default())).collect();
        Handovers { apps }
    }

    /// Hand `c` to the supervisor of the app `dest` is: true once it has
    /// queued it for a worker. False (the caller passes bytes instead)
    /// when the app takes no connections this way (its workers listen on
    /// the port themselves) or its supervisor is not there.
    async fn give(&self, dest: &Dest, c: &TcpStream) -> bool {
        let Some(path) = &dest.handoff else { return false };
        let Some(link) = self.apps.get(path) else { return false };
        let mut link = link.lock().await;
        if link.retry_at.is_some_and(|t| Instant::now() < t) {
            return false;
        }
        if link.stream.is_none() {
            match tokio::net::UnixStream::connect(path).await {
                Ok(s) => link.stream = Some(s),
                Err(_) => {
                    link.retry_at = Some(Instant::now() + HANDOFF_RETRY);
                    return false;
                }
            }
        }
        let s = link.stream.as_ref().expect("connected above");
        match tokio::time::timeout(CONNECT_TIMEOUT, send_and_confirm(s, c)).await {
            Ok(Ok(())) => true,
            _ => {
                // Unknown whether it took it: start over with a new link.
                // The connection is passed byte by byte this time only if
                // the supervisor never got it (a failed send).
                link.stream = None;
                false
            }
        }
    }
}

/// One byte with the socket attached, then the supervisor's one byte back.
async fn send_and_confirm(s: &tokio::net::UnixStream, c: &TcpStream) -> io::Result<()> {
    loop {
        s.writable().await?;
        match s.try_io(Interest::WRITABLE, || crate::sys::send_with_fd(s.as_fd(), b"c", c.as_fd())) {
            Ok(1) => break,
            Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
    let mut b = [0u8; 1];
    loop {
        s.readable().await?;
        match s.try_read(&mut b) {
            Ok(1) if b[0] == b'y' => return Ok(()),
            Ok(_) => return Err(io::ErrorKind::InvalidData.into()),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Bind a socket to a visitor's (non-local) address (Linux, IP_TRANSPARENT).
#[cfg(target_os = "linux")]
fn bind_transparent(sock: &TcpSocket, ip: IpAddr) -> io::Result<()> {
    crate::sys::set_ip_transparent(sock.as_fd(), ip.is_ipv6())?;
    sock.bind(SocketAddr::new(ip, 0))
}

#[cfg(not(target_os = "linux"))]
fn bind_transparent(_sock: &TcpSocket, _ip: IpAddr) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Whether this process may connect from a visitor's address (needs
/// CAP_NET_ADMIN): found out at start rather than at the first visitor.
fn can_be_transparent() -> io::Result<()> {
    let s = TcpSocket::new_v4()?;
    bind_transparent(&s, IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(routes: &[(&str, u16)]) -> Table {
        Table { routes: routes.iter().map(|(h, p)| (h.to_string(), Dest::port(*p))).collect(), ..Table::default() }
    }

    fn port(t: &Table, host: Option<&str>) -> Option<u16> {
        t.lookup(host).map(|d| d.port)
    }

    #[test]
    fn exact_names_win_then_one_label_wildcards_then_the_catch_all() {
        let t = t(&[("api.example.com", 1), ("*.example.com", 2), ("*", 3)]);
        assert_eq!(port(&t, Some("api.example.com")), Some(1));
        assert_eq!(port(&t, Some("www.example.com")), Some(2));
        assert_eq!(port(&t, Some("a.b.example.com")), Some(3), "a wildcard matches one label");
        assert_eq!(port(&t, Some("example.com")), Some(3));
        assert_eq!(port(&t, None), Some(3), "no server_name: the catch-all");
    }

    #[test]
    fn without_a_catch_all_unknown_names_go_nowhere() {
        let t = t(&[("a.test", 1)]);
        assert_eq!(port(&t, Some("b.test")), None);
        assert_eq!(port(&t, None), None);
    }

    #[test]
    fn apps_on_routed_ports_get_a_handoff_socket() {
        let app = |text: &str| Config::parse(text).unwrap();
        let apps = [
            app("[app]\nname = \"api\"\nport = 8443\n"),
            app("[app]\nname = \"edge\"\nport = 443\n[route]\nhosts = { \"a.test\" = 9 }\n"),
        ];
        let r: Route =
            toml::from_str("hosts = { \"a.test\" = \"api\", \"b.test\" = 9000, \"c.test\" = 8443 }").unwrap();
        let t = table_with(&r, &apps);
        assert_eq!(t.routes["a.test"].port, 8443);
        assert_eq!(t.routes["a.test"].handoff, Some(handoff_socket(&apps[0])));
        assert_eq!(t.routes["c.test"].handoff, Some(handoff_socket(&apps[0])), "the app on that port");
        assert_eq!(t.routes["b.test"].handoff, None, "a port no app config has: bytes are passed");
        assert!(handoff_socket(&apps[0]).ends_with("api/route.sock"));
    }

    #[test]
    fn apps_on_their_own_address_can_share_a_port() {
        let app = |text: &str| Config::parse(text).unwrap();
        let apps = [
            app("[app]\nname = \"api\"\nport = 443\naddress = \"2001:db8::10\"\n"),
            app("[app]\nname = \"www\"\nport = 443\naddress = \"2001:db8::11\"\n"),
            app("[app]\nname = \"web\"\nport = 8443\n"),
        ];
        let r: Route =
            toml::from_str("hosts = { \"a.test\" = \"api\", \"b.test\" = \"www\", \"c.test\" = \"web\" }").unwrap();
        let t = table_with(&r, &apps);
        assert_eq!(t.routes["a.test"].to_string(), "[2001:db8::10]:443");
        assert_eq!(t.routes["b.test"].to_string(), "[2001:db8::11]:443");
        assert_eq!(t.routes["b.test"].handoff, Some(handoff_socket(&apps[1])), "each app its own supervisor");
        assert_eq!(t.routes["c.test"].to_string(), "8443");
        assert_eq!(t.rule_ports(), vec![8443], "rules on 443 would catch the answers to its direct visitors");
    }

    #[test]
    fn rule_ports_are_listed_once() {
        assert_eq!(t(&[("a.test", 9), ("b.test", 4), ("*", 9)]).rule_ports(), vec![4, 9]);
    }

    #[test]
    fn ports_in_the_config_need_no_lookup_and_unknown_apps_are_reported() {
        let r: Route = toml::from_str("hosts = { \"a.test\" = 8443 }").unwrap();
        let tb = table(&r);
        assert_eq!(tb.routes.get("a.test").map(|d| d.port), Some(8443));
        assert!(tb.missing.is_empty());
    }
}

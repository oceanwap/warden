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
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    /// Hostname (`a.example.com`, `*.example.com`, `*`) → port.
    pub routes: BTreeMap<String, u16>,
    /// Apps named in `route.hosts` with no config (or no `app.port`) here.
    #[serde(default)]
    pub missing: Vec<String>,
    pub client_ip: bool,
    pub host: String,
}

/// The table for `[route]`: app names are looked up in this host's app
/// configs (their `app.port`).
pub fn table(r: &Route) -> Table {
    let mut apps: Option<Vec<(String, u16)>> = None;
    let mut t = Table { client_ip: r.client_ip(), host: r.host.clone(), ..Table::default() };
    for (host, target) in &r.hosts {
        match target {
            RouteTarget::Port(p) => {
                t.routes.insert(host.clone(), *p);
            }
            RouteTarget::App(name) => {
                let apps = apps.get_or_insert_with(app_ports);
                match apps.iter().find(|(n, _)| n == name) {
                    Some((_, p)) => {
                        t.routes.insert(host.clone(), *p);
                    }
                    None if !t.missing.contains(name) => t.missing.push(name.clone()),
                    None => {}
                }
            }
        }
    }
    t
}

/// Every app config on this host that has a port: (name, port).
fn app_ports() -> Vec<(String, u16)> {
    crate::fleet::discover()
        .into_iter()
        .filter_map(|a| {
            let c = Config::load(a.config.as_ref()?).ok()?;
            Some((c.app.name, c.app.port?))
        })
        .collect()
}

impl Table {
    /// The port for a hostname: the exact name, then `*.` and the name
    /// without its first label, then `*` (which also takes connections that
    /// name no host).
    fn lookup(&self, host: Option<&str>) -> Option<u16> {
        if let Some(h) = host {
            if let Some(p) = self.routes.get(h) {
                return Some(*p);
            }
            if let Some((_, rest)) = h.split_once('.') {
                if let Some(p) = self.routes.get(&format!("*.{rest}")) {
                    return Some(*p);
                }
            }
        }
        self.routes.get("*").copied()
    }

    /// The ports connections go to (for the routing rules, Linux).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn ports(&self) -> Vec<u16> {
        let mut p: Vec<u16> = self.routes.values().copied().collect();
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
                    let table = table.clone();
                    let guard = Active::new(&active);
                    tokio::spawn(async move {
                        let _guard = guard;
                        let _ = connection(c, peer, &table, transparent).await;
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

/// One visitor's connection: read the ClientHello, connect to the app, send
/// it the bytes read so far, then pass bytes both ways until both are done.
async fn connection(mut c: TcpStream, peer: SocketAddr, table: &Table, transparent: bool) -> io::Result<()> {
    if !crate::sys::NODELAY_INHERITED {
        let _ = c.set_nodelay(true);
    }
    let mut buf = vec![0u8; 2048];
    let mut n = 0;
    let host = tokio::time::timeout(HELLO_TIMEOUT, async {
        loop {
            match hello::parse(&buf[..n]) {
                Hello::Done(host) => return Ok::<_, io::Error>(host),
                Hello::NotTls => return Err(io::ErrorKind::InvalidData.into()),
                Hello::Need(need) if need > MAX_HELLO => return Err(io::ErrorKind::InvalidData.into()),
                Hello::Need(need) => {
                    if buf.len() < need {
                        buf.resize(need, 0);
                    }
                }
            }
            let r = c.read(&mut buf[n..]).await?;
            if r == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            n += r;
        }
    })
    .await
    .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    let Some(port) = table.lookup(host.as_deref()) else { return Ok(()) };

    let peer_ip = peer.ip().to_canonical();
    let (target, from) = if transparent && !peer_ip.is_loopback() {
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
    let mut up = tokio::time::timeout(CONNECT_TIMEOUT, sock.connect(target))
        .await
        .map_err(|_| io::Error::from(io::ErrorKind::TimedOut))??;
    up.set_nodelay(true)?;
    up.write_all(&buf[..n]).await?;
    drop(buf);
    pipe::both(&c, &up).await;
    Ok(())
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
        Table { routes: routes.iter().map(|(h, p)| (h.to_string(), *p)).collect(), ..Table::default() }
    }

    #[test]
    fn exact_names_win_then_one_label_wildcards_then_the_catch_all() {
        let t = t(&[("api.example.com", 1), ("*.example.com", 2), ("*", 3)]);
        assert_eq!(t.lookup(Some("api.example.com")), Some(1));
        assert_eq!(t.lookup(Some("www.example.com")), Some(2));
        assert_eq!(t.lookup(Some("a.b.example.com")), Some(3), "a wildcard matches one label");
        assert_eq!(t.lookup(Some("example.com")), Some(3));
        assert_eq!(t.lookup(None), Some(3), "no server_name: the catch-all");
    }

    #[test]
    fn without_a_catch_all_unknown_names_go_nowhere() {
        let t = t(&[("a.test", 1)]);
        assert_eq!(t.lookup(Some("b.test")), None);
        assert_eq!(t.lookup(None), None);
    }

    #[test]
    fn ports_are_listed_once() {
        assert_eq!(t(&[("a.test", 9), ("b.test", 4), ("*", 9)]).ports(), vec![4, 9]);
    }

    #[test]
    fn ports_in_the_config_need_no_lookup_and_unknown_apps_are_reported() {
        let r: Route = toml::from_str("hosts = { \"a.test\" = 8443 }").unwrap();
        let tb = table(&r);
        assert_eq!(tb.routes.get("a.test"), Some(&8443));
        assert!(tb.missing.is_empty());
    }
}

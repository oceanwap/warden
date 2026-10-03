//! How the sockets an app listens on are shown: the `ports` column of
//! `warden list`, the `ports` row of `warden status` and `describe`, and the
//! `warden ports` table (and its JSON).

use crate::control::{Listener, Status};
use crate::fleet::App;
use crate::table::{Cell, Fmt, KEY, NAME, boxed, clip};
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The `ports` column of `list` holds one socket per line, each cut to this
/// many characters, so one long address does not widen the table.
const CELL_MAX: usize = 26;

/// And at most this many lines (the last one `+N more`): a monorepo's ten
/// ports must not make a row ten lines tall. `warden ports` has all of them.
const LINES_MAX: usize = 4;

/// Which interfaces a TCP listener is reachable on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Scope {
    /// `0.0.0.0` or `::`.
    All,
    /// `127.0.0.1` or `::1`: only this machine.
    Local,
    /// One interface's address.
    Addr(String),
}

pub(super) fn scope_of(addr: &str) -> Scope {
    match addr.parse::<IpAddr>().map(|a| a.to_canonical()) {
        Ok(a) if a.is_unspecified() => Scope::All,
        Ok(a) if a.is_loopback() => Scope::Local,
        _ => Scope::Addr(addr.to_string()),
    }
}

/// An address as it goes before `:port` in a URL.
fn host(addr: &str) -> String {
    if addr.contains(':') { format!("[{addr}]") } else { addr.to_string() }
}

/// The listeners in the order they are shown: TCP by port, then Unix by
/// path; a kind this Warden does not know is left out; none twice.
pub(super) fn sorted(mut listeners: Vec<Listener>) -> Vec<Listener> {
    let key = |l: &Listener| match l {
        Listener::Tcp { addr, port } => (0, *port, scope_of(addr), addr.clone()),
        Listener::Unix { path } => (1, 0, Scope::All, path.clone()),
        Listener::Other => (2, 0, Scope::All, String::new()),
    };
    listeners.retain(|l| *l != Listener::Other);
    listeners.sort_by_key(key);
    listeners.dedup();
    listeners
}

/// What every worker of an app listens on, sorted, each socket once (the
/// workers of a shared port all listen on it).
pub(super) fn app_listeners(s: &Status) -> Vec<Listener> {
    sorted(s.workers.iter().flat_map(|w| w.listening.iter().cloned()).collect())
}

/// The `ports` column, one socket per line (the table stacks them):
/// `3000`, `localhost:3001`, `unix:/tmp/a.sock`. A port everyone can reach is
/// the bare number; anything narrower says where. `-` when there is nothing.
pub(super) fn cell(listening: &[Listener]) -> String {
    let mut seen: Vec<(u16, Scope)> = Vec::new();
    let mut items: Vec<String> = Vec::new();
    let mut unix: Vec<String> = Vec::new();
    for l in sorted(listening.to_vec()) {
        match l {
            Listener::Tcp { addr, port } => {
                let scope = scope_of(&addr);
                // 0.0.0.0 and :: (or 127.0.0.1 and ::1) are one port to the reader.
                if seen.contains(&(port, scope.clone())) {
                    continue;
                }
                items.push(match &scope {
                    Scope::All => port.to_string(),
                    Scope::Local => format!("localhost:{port}"),
                    Scope::Addr(a) => format!("{}:{port}", host(a)),
                });
                seen.push((port, scope));
            }
            Listener::Unix { path } => unix.push(printable(&path)),
            Listener::Other => {}
        }
    }
    items.extend(unix.into_iter().map(|p| {
        let full = format!("unix:{p}");
        if full.chars().count() <= CELL_MAX {
            full
        } else {
            // The end of a path says more than its start.
            let base = p.rsplit('/').next().unwrap_or(&p);
            clip(&format!("unix:…/{base}"), CELL_MAX)
        }
    }));
    stack(&items)
}

/// `items` one per line, each cut to [`CELL_MAX`], at most [`LINES_MAX`]
/// lines: when there are more, the last line says how many are left out.
fn stack(items: &[String]) -> String {
    if items.is_empty() {
        return "-".into();
    }
    let shown = if items.len() <= LINES_MAX { items.len() } else { LINES_MAX - 1 };
    let mut lines: Vec<String> = items[..shown].iter().map(|i| clip(i, CELL_MAX)).collect();
    if shown < items.len() {
        lines.push(format!("+{} more", items.len() - shown));
    }
    lines.join("\n")
}

/// A path as it is printed: a control character (an escape sequence in a
/// socket's name) must not reach the terminal.
fn printable(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

/// The `ports` row of `describe` and `status`: every socket with where it
/// is reachable. `3000 (all interfaces), 3001 (localhost only), unix
/// /tmp/app.sock`.
pub(super) fn detail(listeners: &[Listener]) -> String {
    let mut tcp: BTreeMap<u16, Vec<Scope>> = BTreeMap::new();
    let mut unix: Vec<&str> = Vec::new();
    for l in listeners {
        match l {
            Listener::Tcp { addr, port } => {
                let scopes = tcp.entry(*port).or_default();
                let s = scope_of(addr);
                if !scopes.contains(&s) {
                    scopes.push(s);
                }
            }
            Listener::Unix { path } => unix.push(path),
            Listener::Other => {}
        }
    }
    let mut parts: Vec<String> = tcp
        .into_iter()
        .map(|(port, mut scopes)| {
            scopes.sort();
            let where_ = scopes
                .iter()
                .map(|s| match s {
                    Scope::All => "all interfaces".to_string(),
                    Scope::Local => "localhost only".to_string(),
                    Scope::Addr(a) => a.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{port} ({where_})")
        })
        .collect();
    parts.extend(unix.into_iter().map(|p| format!("unix {}", printable(p))));
    parts.join(", ")
}

/// A listening socket of an app and the workers that hold it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortRow {
    pub id: Option<u32>,
    pub app: String,
    pub namespace: String,
    pub listener: Listener,
    pub workers: Vec<usize>,
}

/// Every socket of every app that answered, each with its workers, apps in
/// the order given and sockets in show order.
pub fn collect(all: &[(App, Result<Status, String>)]) -> Vec<PortRow> {
    let mut rows = Vec::new();
    for (app, st) in all {
        let Ok(s) = st else { continue };
        for l in app_listeners(s) {
            let workers = s.workers.iter().filter(|w| w.listening.contains(&l)).map(|w| w.id).collect();
            rows.push(PortRow { id: app.id, app: s.app.clone(), namespace: s.namespace.clone(), listener: l, workers });
        }
    }
    rows
}

/// `warden ports --json`: one object per socket.
pub fn json(rows: &[PortRow]) -> serde_json::Value {
    rows.iter()
        .map(|r| {
            let mut o = serde_json::to_value(&r.listener).unwrap_or(serde_json::Value::Null);
            if let Some(m) = o.as_object_mut() {
                m.insert("id".into(), serde_json::json!(r.id));
                m.insert("app".into(), serde_json::json!(r.app));
                m.insert("namespace".into(), serde_json::json!(r.namespace));
                m.insert("workers".into(), serde_json::json!(r.workers));
                if let Listener::Tcp { addr, port } = &r.listener {
                    m.insert("url".into(), serde_json::json!(url(addr, *port)));
                }
            }
            o
        })
        .collect()
}

/// Where a browser or `curl` reaches a TCP listener on this machine.
fn url(addr: &str, port: u16) -> String {
    match scope_of(addr) {
        Scope::All | Scope::Local => format!("http://localhost:{port}"),
        Scope::Addr(a) => format!("http://{}:{port}", host(&a)),
    }
}

/// `warden ports`: a box with one row per socket (a port on `0.0.0.0` and on
/// `::` is one row), then what could not be read.
pub fn render(all: &[(App, Result<Status, String>)], fmt: &Fmt) -> String {
    let rows = collect(all);
    let mut notes: Vec<String> = Vec::new();
    let offline = all.iter().filter(|(_, s)| matches!(s, Err(e) if e == "not running")).count();
    if offline > 0 {
        notes.push(format!("{offline} app(s) offline: `warden list` shows them"));
    }
    for (app, st) in all {
        if let Err(e) = st {
            if e != "not running" {
                notes.push(format!("{}: {e}", app.name));
            }
        }
    }
    // One line per (app, scope, port): the v4 and v6 sockets of a port are one.
    type Key = (usize, u8, u16, Scope, String);
    let mut merged: BTreeMap<Key, (&PortRow, Vec<usize>)> = BTreeMap::new();
    for (i, r) in rows.iter().enumerate() {
        let app_at = all.iter().position(|(a, _)| a.name == r.app).unwrap_or(i);
        let key = match &r.listener {
            Listener::Tcp { addr, port } => (app_at, 0, *port, scope_of(addr), String::new()),
            Listener::Unix { path } => (app_at, 1, 0, Scope::All, path.clone()),
            Listener::Other => continue,
        };
        let e = merged.entry(key).or_insert_with(|| (r, Vec::new()));
        for w in &r.workers {
            if !e.1.contains(w) {
                e.1.push(*w);
            }
        }
    }
    if merged.is_empty() {
        let mut o = String::from(
            "nothing is listening: no app is running, or the running ones listen on nothing \
             (a port shows a second or two after the app binds it; `warden list` shows their state)\n",
        );
        for n in notes {
            o += &format!("  {n}\n");
        }
        return o;
    }
    let mut table: Vec<Vec<Cell>> = Vec::new();
    for ((_, _, _, scope, _), (r, workers)) in &merged {
        let ids: Vec<u32> = workers.iter().map(|w| *w as u32).collect();
        let (proto, on, port, link) = match &r.listener {
            Listener::Tcp { addr, port } => (
                "tcp",
                match scope {
                    Scope::All => "all interfaces".to_string(),
                    Scope::Local => "localhost only".to_string(),
                    Scope::Addr(a) => a.clone(),
                },
                port.to_string(),
                url(addr, *port),
            ),
            Listener::Unix { path } => ("unix", printable(path), "-".to_string(), "-".to_string()),
            Listener::Other => continue,
        };
        table.push(vec![
            Cell::styled(r.id.map(|i| i.to_string()).unwrap_or_else(|| "-".into()), KEY),
            Cell::styled(r.app.clone(), NAME),
            Cell::plain(proto),
            Cell::plain(on),
            Cell::plain(port),
            Cell::plain(crate::ids::compact(&ids)),
            Cell::plain(link),
        ]);
    }
    let mut o = boxed(Some(&["id", "name", "proto", "listening on", "port", "workers", "http url"]), &table, fmt, None);
    if merged.values().any(|(r, _)| matches!(r.listener, Listener::Unix { .. })) {
        o += "  unix sockets: curl --unix-socket PATH http://localhost/\n";
    }
    for n in notes {
        o += &format!("  {n}\n");
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tcp(addr: &str, port: u16) -> Listener {
        Listener::Tcp { addr: addr.into(), port }
    }

    fn unix(path: &str) -> Listener {
        Listener::Unix { path: path.into() }
    }

    #[test]
    fn where_a_listener_is_reachable() {
        assert_eq!(scope_of("0.0.0.0"), Scope::All);
        assert_eq!(scope_of("::"), Scope::All);
        assert_eq!(scope_of("127.0.0.1"), Scope::Local);
        assert_eq!(scope_of("::1"), Scope::Local);
        assert_eq!(scope_of("::ffff:127.0.0.1"), Scope::Local, "an IPv4 loopback in IPv6 clothes");
        assert_eq!(scope_of("192.168.1.5"), Scope::Addr("192.168.1.5".into()));
        assert_eq!(scope_of("not an address"), Scope::Addr("not an address".into()));
    }

    #[test]
    fn the_ports_cell_is_numbers_then_sockets_one_per_line_and_says_where_when_it_is_not_everywhere() {
        assert_eq!(cell(&[]), "-");
        assert_eq!(cell(&[tcp("0.0.0.0", 3000)]), "3000");
        // v4 and v6 of one port: one number; order by port, not by arrival.
        assert_eq!(cell(&[tcp("::", 3001), tcp("0.0.0.0", 3001), tcp("0.0.0.0", 3000)]), "3000\n3001");
        assert_eq!(cell(&[tcp("127.0.0.1", 9229), tcp("::1", 9229)]), "localhost:9229");
        assert_eq!(cell(&[tcp("10.0.0.5", 80)]), "10.0.0.5:80");
        assert_eq!(cell(&[tcp("fe80::1", 80)]), "[fe80::1]:80");
        assert_eq!(cell(&[unix("/tmp/a.sock"), tcp("0.0.0.0", 3000)]), "3000\nunix:/tmp/a.sock");
        assert_eq!(
            cell(&[tcp("0.0.0.0", 4001), tcp("127.0.0.1", 5173), tcp("0.0.0.0", 4000)]),
            "4000\n4001\nlocalhost:5173"
        );
    }

    #[test]
    fn a_long_list_is_four_lines_at_most_with_the_count_left_over() {
        let four: Vec<Listener> = (3000..3004).map(|p| tcp("0.0.0.0", p)).collect();
        assert_eq!(cell(&four), "3000\n3001\n3002\n3003", "four fit");
        let ten: Vec<Listener> = (3000..3010).map(|p| tcp("0.0.0.0", p)).collect();
        assert_eq!(cell(&ten), "3000\n3001\n3002\n+7 more");
        let five: Vec<Listener> = (3000..3005).map(|p| tcp("0.0.0.0", p)).collect();
        assert_eq!(cell(&five), "3000\n3001\n3002\n+2 more", "never a line for one left out alone");
    }

    #[test]
    fn a_long_unix_path_keeps_its_end() {
        let c = cell(&[unix("/var/run/some/very/long/directory/api.sock")]);
        assert_eq!(c, "unix:…/api.sock");
        let weird = cell(&[unix(&format!("/x/{}", "n".repeat(80)))]);
        assert!(weird.chars().count() <= CELL_MAX, "{weird}");
    }

    #[test]
    fn the_detail_names_every_scope_and_every_socket() {
        let d = detail(&[
            tcp("0.0.0.0", 3000),
            tcp("::", 3000),
            tcp("127.0.0.1", 3001),
            unix("/tmp/app.sock"),
            tcp("10.0.0.5", 3002),
            Listener::Other,
        ]);
        assert_eq!(d, "3000 (all interfaces), 3001 (localhost only), 3002 (10.0.0.5), unix /tmp/app.sock");
        assert_eq!(detail(&[]), "");
    }

    #[test]
    fn a_long_item_is_cut_to_the_cell_and_a_control_character_never_reaches_the_terminal() {
        let c = cell(&[tcp("fe80::e558:fff:d8b4:ff6:1234", 80), tcp("0.0.0.0", 81)]);
        let lines: Vec<&str> = c.lines().collect();
        // Sorted by port: the long address of 80, then 81.
        assert!(lines[0].chars().count() == CELL_MAX && lines[0].ends_with('…'), "{c}");
        assert_eq!(lines[1], "81", "{c}");
        let c = cell(&[unix("/tmp/\u{1b}[31mred.sock")]);
        assert!(!c.contains('\u{1b}') && c.contains("?[31mred"), "{c:?}");
        let d = detail(&[unix("/tmp/\u{1b}]0;x\u{7}")]);
        assert!(!d.chars().any(char::is_control), "{d:?}");
    }

    #[test]
    fn urls() {
        assert_eq!(url("0.0.0.0", 80), "http://localhost:80");
        assert_eq!(url("::1", 80), "http://localhost:80");
        assert_eq!(url("10.0.0.5", 8080), "http://10.0.0.5:8080");
        assert_eq!(url("fe80::1", 8080), "http://[fe80::1]:8080");
    }
}

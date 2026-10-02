//! How the sockets an app listens on are shown: the `ports` column of
//! `warden list`, the `ports` row of `warden status` and `describe`, and the
//! `warden ports` table (and its JSON).

use crate::control::{Listener, Status};
use crate::fleet::App;
use crate::table::{Cell, Fmt, KEY, NAME, boxed, clip};
use std::collections::BTreeMap;
use std::net::IpAddr;

/// The `ports` column of `list` is cut here (the rest is `,+N`): a
/// monorepo's six ports must not make the table wider than a terminal.
/// `warden ports` has all of them.
const CELL_MAX: usize = 26;

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

/// The `ports` column: `3000,localhost:3001,unix:/tmp/a.sock`. A port
/// everyone can reach is the bare number; anything narrower says where.
/// `-` when there is nothing.
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
    fit(&items, CELL_MAX)
}

/// All of `items` when they fit in `max` characters, else as many whole ones
/// as fit with `,+N` for the rest (the first is cut if it alone is too long).
fn fit(items: &[String], max: usize) -> String {
    if items.is_empty() {
        return "-".into();
    }
    let joined = items.join(",");
    if joined.chars().count() <= max {
        return joined;
    }
    let mut shown = 0;
    let mut len = 0;
    for item in items {
        let add = item.chars().count() + usize::from(shown > 0);
        // What follows the last one shown: `,+N` for the N left out.
        let left = items.len() - shown - 1;
        let tail = if left == 0 { 0 } else { format!(",+{left}").chars().count() };
        if shown > 0 && len + add + tail > max {
            break;
        }
        len += add;
        shown += 1;
    }
    let left = items.len() - shown;
    let tail = if left == 0 { String::new() } else { format!(",+{left}") };
    let head = items.get(..shown).unwrap_or(items).join(",");
    // The first item alone can be longer than the cell.
    let room = max.saturating_sub(tail.chars().count());
    format!("{}{tail}", if shown == 1 { clip(&head, room) } else { head })
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
    fn the_ports_cell_is_numbers_then_sockets_and_says_where_when_it_is_not_everywhere() {
        assert_eq!(cell(&[]), "-");
        assert_eq!(cell(&[tcp("0.0.0.0", 3000)]), "3000");
        // v4 and v6 of one port: one number; order by port, not by arrival.
        assert_eq!(cell(&[tcp("::", 3001), tcp("0.0.0.0", 3001), tcp("0.0.0.0", 3000)]), "3000,3001");
        assert_eq!(cell(&[tcp("127.0.0.1", 9229), tcp("::1", 9229)]), "localhost:9229");
        assert_eq!(cell(&[tcp("10.0.0.5", 80)]), "10.0.0.5:80");
        assert_eq!(cell(&[tcp("fe80::1", 80)]), "[fe80::1]:80");
        assert_eq!(cell(&[unix("/tmp/a.sock"), tcp("0.0.0.0", 3000)]), "3000,unix:/tmp/a.sock");
    }

    #[test]
    fn a_long_list_is_cut_with_the_count_left_over() {
        let many: Vec<Listener> = (3000..3010).map(|p| tcp("0.0.0.0", p)).collect();
        let c = cell(&many);
        assert!(c.chars().count() <= CELL_MAX && c.starts_with("3000,3001,") && c.contains(",+"), "{c}");
        let shown = c.split(',').filter(|p| !p.starts_with('+')).count();
        assert!(c.ends_with(&format!(",+{}", 10 - shown)), "{c}: {shown} shown of 10");
        // One that fits exactly is not cut.
        let five: Vec<Listener> = (3000..3005).map(|p| tcp("0.0.0.0", p)).collect();
        assert_eq!(cell(&five), "3000,3001,3002,3003,3004");
        // Exactly the width: whole. One character more: cut, with the count.
        let six: Vec<Listener> = (3000..3005).map(|p| tcp("0.0.0.0", p)).chain([tcp("0.0.0.0", 5)]).collect();
        // (ports are sorted: the 5 comes first)
        assert_eq!(cell(&six), "5,3000,3001,3002,3003,3004");
        assert_eq!(cell(&six).chars().count(), CELL_MAX);
        let seven: Vec<Listener> =
            (3000..3005).map(|p| tcp("0.0.0.0", p)).chain([tcp("0.0.0.0", 55), tcp("0.0.0.0", 66)]).collect();
        let c = cell(&seven);
        // Sorted: 55,66,3000,3001,3002 fit with ",+2" (20 + 3 characters); 3003 would not.
        assert_eq!(c, "55,66,3000,3001,3002,+2");
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
    fn a_long_first_item_is_cut_to_the_cell_and_a_control_character_never_reaches_the_terminal() {
        let c = cell(&[tcp("fe80::e558:fff:d8b4:ff6", 80), tcp("0.0.0.0", 81)]);
        assert!(c.chars().count() <= CELL_MAX && c.ends_with(",+1"), "{c}");
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

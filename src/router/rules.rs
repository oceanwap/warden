//! The routing rules `client_ip` needs (Linux). The router connects to an app
//! from the visitor's address, so the app answers to that address: without
//! help those packets would leave for the internet. Two rules bring them
//! back to the router, for each port the router sends connections to:
//!
//! ```text
//! ip rule add pref 22356 ipproto tcp sport <port> lookup 22356
//! ip route replace local 0.0.0.0/0 dev lo table 22356     (and ::/0 for IPv6)
//! ```
//!
//! Every packet the app sends from that port is then delivered locally, to
//! the router's socket bound to the visitor's address. The same as nginx's
//! `proxy_bind $remote_addr transparent` needs. A consequence: the app's port
//! only answers through the router (a visitor connecting to it directly gets
//! no answer). The supervisor keeps the rules in line with the routes at each
//! worker start and removes them when it stops.

use std::process::Command;
use std::sync::Mutex;

/// Rule priority and routing table number (otherwise unused numbers).
const PREF: &str = "22356";
const TABLE: &str = "22356";

/// The ports the rules were last made for, so a worker start with the same
/// routes runs no command.
static SYNCED: Mutex<Option<Vec<u16>>> = Mutex::new(None);

/// Make the rules match `ports` (sorted). IPv4 must work; IPv6 is best
/// effort (a host without it).
pub(crate) fn sync(ports: &[u16]) -> Result<(), String> {
    let mut synced = SYNCED.lock().unwrap_or_else(|e| e.into_inner());
    if synced.as_deref() == Some(ports) {
        return Ok(());
    }
    sync_family("-4", "0.0.0.0/0", ports)?;
    let _ = sync_family("-6", "::/0", ports);
    *synced = Some(ports.to_vec());
    Ok(())
}

fn sync_family(fam: &str, all: &str, ports: &[u16]) -> Result<(), String> {
    ip(&[fam, "route", "replace", "local", all, "dev", "lo", "table", TABLE])?;
    let have = current(fam)?;
    for p in ports.iter().filter(|p| !have.contains(p)) {
        let p = p.to_string();
        ip(&[fam, "rule", "add", "pref", PREF, "ipproto", "tcp", "sport", &p, "lookup", TABLE])?;
    }
    for p in have.iter().filter(|p| !ports.contains(p)) {
        let p = p.to_string();
        ip(&[fam, "rule", "del", "pref", PREF, "ipproto", "tcp", "sport", &p, "lookup", TABLE])?;
    }
    Ok(())
}

/// The ports of our rules now in place.
fn current(fam: &str) -> Result<Vec<u16>, String> {
    Ok(ports_in(&ip(&[fam, "rule", "show", "pref", PREF])?))
}

/// `22356: from all ipproto tcp sport 8443 lookup 22356` → 8443.
fn ports_in(text: &str) -> Vec<u16> {
    text.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            w.find(|x| *x == "sport")?;
            w.next()?.parse().ok()
        })
        .collect()
}

/// Whether this process has rules in place (synced and not cleared since).
pub(crate) fn in_place() -> bool {
    SYNCED.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

/// Remove every rule and route this module adds (the router stopped).
pub(crate) fn clear() {
    for (fam, all) in [("-4", "0.0.0.0/0"), ("-6", "::/0")] {
        for p in current(fam).unwrap_or_default() {
            let p = p.to_string();
            let _ = ip(&[fam, "rule", "del", "pref", PREF, "ipproto", "tcp", "sport", &p, "lookup", TABLE]);
        }
        let _ = ip(&[fam, "route", "del", "local", all, "dev", "lo", "table", TABLE]);
    }
    *SYNCED.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn ip(args: &[&str]) -> Result<String, String> {
    let out = Command::new("ip").args(args).output().map_err(|e| format!("running `ip`: {e} (install iproute2)"))?;
    if !out.status.success() {
        return Err(format!("`ip {}`: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_ports_of_the_rules_ip_shows() {
        let text = "22356:\tfrom all ipproto tcp sport 8443 lookup 22356\n22356:\tfrom all ipproto tcp sport 9443 lookup 22356\n";
        assert_eq!(super::ports_in(text), vec![8443, 9443]);
        assert!(super::ports_in("").is_empty());
    }
}

//! `[app] address` (Linux): the app's own IP address on this server, so its
//! workers can listen on `address:443` next to other apps on theirs. The
//! kernel picks the app by the address a visitor connects to: nothing sits
//! between them (`docs/routing.md`).
//!
//! When the address is not on the server yet, the supervisor adds it to the
//! interface of the default route, the way you would by hand:
//!
//! ```text
//! ip -6 addr add <address>/128 dev <iface> nodad preferred_lft 0
//! ip -4 addr add <address>/32 dev <iface>
//! ```
//!
//! `nodad` makes an IPv6 address usable at once (duplicate detection would
//! hold it back for a second or two), and `preferred_lft 0` keeps the server's
//! own outgoing connections on its main address: the app's address answers
//! the visitors who connect to it and is never picked as a source otherwise.
//! An address Warden added goes when the app stops; one that was already
//! there (the provider's, or your own netplan) is never touched.

use std::net::IpAddr;
use std::process::Command;
use std::sync::Mutex;

/// The address this supervisor added (one app per supervisor), to remove
/// when the app stops.
static ADDED: Mutex<Option<(IpAddr, String)>> = Mutex::new(None);

/// Make sure `ip` is on this server. `Ok(true)` when it was added now.
pub(crate) fn ensure(ip: IpAddr) -> Result<bool, String> {
    let mut added = ADDED.lock().unwrap_or_else(|e| e.into_inner());
    // Every 127.x address is this host's already (and ::1).
    if ip.is_loopback() || added.as_ref().is_some_and(|(a, _)| *a == ip) || present(ip)? {
        return Ok(false);
    }
    let fam = family(ip);
    let dev = default_device(fam)?;
    let prefix = if ip.is_ipv6() { format!("{ip}/128") } else { format!("{ip}/32") };
    let mut args = vec![fam, "addr", "add", &prefix, "dev", &dev];
    if ip.is_ipv6() {
        args.extend(["nodad", "preferred_lft", "0"]);
    }
    run(&args)?;
    *added = Some((ip, dev));
    Ok(true)
}

/// Remove the address this supervisor added, if any (the app stopped).
pub(crate) fn release() {
    let Some((ip, dev)) = ADDED.lock().unwrap_or_else(|e| e.into_inner()).take() else { return };
    let prefix = if ip.is_ipv6() { format!("{ip}/128") } else { format!("{ip}/32") };
    let _ = run(&[family(ip), "addr", "del", &prefix, "dev", &dev]);
}

/// Whether this supervisor added an address that is still in place.
pub(crate) fn added() -> bool {
    ADDED.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn family(ip: IpAddr) -> &'static str {
    if ip.is_ipv6() { "-6" } else { "-4" }
}

/// Whether some interface has `ip` already.
fn present(ip: IpAddr) -> Result<bool, String> {
    Ok(!run(&[family(ip), "-o", "addr", "show", "to", &ip.to_string()])?.trim().is_empty())
}

/// The interface of the default route for that family.
fn default_device(fam: &str) -> Result<String, String> {
    let out = run(&[fam, "route", "show", "default"])?;
    device_in(&out).ok_or_else(|| {
        format!(
            "no default {} route to add the address to: add it to an interface yourself",
            if fam == "-6" { "IPv6" } else { "IPv4" }
        )
    })
}

/// `default via fe80::1 dev eth0 proto ra metric 1024` → `eth0`.
fn device_in(text: &str) -> Option<String> {
    let line = text.lines().next()?;
    let mut w = line.split_whitespace();
    w.find(|x| *x == "dev")?;
    w.next().map(str::to_string)
}

fn run(args: &[&str]) -> Result<String, String> {
    let out = Command::new("ip").args(args).output().map_err(|e| format!("running `ip`: {e} (install iproute2)"))?;
    if !out.status.success() {
        return Err(format!("`ip {}`: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_the_device_of_the_default_route() {
        assert_eq!(
            super::device_in("default via fe80::1 dev eth0 proto ra metric 1024 pref medium\n").as_deref(),
            Some("eth0")
        );
        assert_eq!(
            super::device_in("default via 10.0.0.1 dev ens3 proto dhcp src 10.0.0.5\n").as_deref(),
            Some("ens3")
        );
        assert_eq!(super::device_in(""), None);
        assert_eq!(super::device_in("default via 10.0.0.1\n"), None);
    }
}

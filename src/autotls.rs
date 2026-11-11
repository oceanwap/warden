//! `[tls]`: automatic HTTPS (`docs/tls.md`). Warden gets and renews the
//! certificate; the shim hands it to `Bun.serve` when the workers start. Nothing
//! here touches a request: the app terminates TLS itself.
//!
//! The supervisor of an app with `[tls]` runs [`run`]: it answers the CA's
//! HTTP-01 challenge on port 80 (`responder`), orders a certificate when none
//! is due (`order`), stores it (`store`) and tells the supervisor, which
//! rolls the workers so the new ones read it.

mod order;
mod responder;
pub mod store;

use crate::config::Tls;
use crate::{error, info, warn};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a certificate is looked at when all is well.
const CHECK_EVERY: Duration = Duration::from_secs(12 * 3600);
/// Seconds before expiry at which a failing renewal is an error, not a warning.
const URGENT: i64 = 14 * 86400;

/// `<state>/tls`: accounts, challenges and one directory per app (by its first hostname).
pub fn base() -> PathBuf {
    crate::fleet::state_dir().join("tls")
}

/// Where this app's certificates live.
pub fn app_dir(t: &Tls) -> PathBuf {
    base().join(t.hosts[0].replacen("*.", "wildcard.", 1))
}

/// The directory the workers read `fullchain.pem` and `privkey.pem` from.
pub fn live(t: &Tls) -> PathBuf {
    app_dir(t).join("live")
}

/// Before the first worker starts: a certificate must exist, so the app can
/// listen and be health-checked. A short-lived self-signed one stands in until
/// the real one is issued (the supervisor then rolls the workers).
pub fn ensure_placeholder(t: &Tls) -> Result<(), String> {
    let dir = app_dir(t);
    if store::read_meta(&dir).is_some() {
        return Ok(());
    }
    let (chain, key) = store::placeholder(&t.hosts)?;
    store::install(&dir, &chain, &key, false)?;
    warn!(
        "no certificate yet: workers start with a self-signed placeholder until the real one is issued",
        hosts = t.hosts.join(","),
        hint = "the real certificate follows within a minute when port 80 reaches this server; `warden tls` shows the state",
    );
    Ok(())
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Retry delay after `failures` failed orders in a row: 1, 5, 15 minutes, then
/// an hour doubling to 12 (the CA's failed-validation limit is 5 an hour).
fn backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 | 1 => 60,
        2 => 300,
        3 => 900,
        n => (3600u64 << (n - 4).min(4)).min(12 * 3600),
    })
}

/// Runs for the life of the supervisor. `renewed` is called after a new
/// certificate is in place.
pub async fn run(
    mut cfg: tokio::sync::watch::Receiver<Tls>,
    force: std::sync::Arc<tokio::sync::Notify>,
    renewed: impl Fn(),
) {
    // The ACME client's TLS (the process default, once; the static server does the same).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let base = base();
    let mut t = cfg.borrow_and_update().clone();
    let challenges = base.join("challenges");
    // The port and the redirect are fixed for the life of the supervisor.
    tokio::task::spawn_local(responder::serve(t.http_port, challenges.clone(), t.redirect));
    let mut failures = 0u32;
    // `warden tls renew`: order even though the certificate is not due; kept until an order succeeds.
    let mut forced = false;
    loop {
        let dir = app_dir(&t);
        let meta = store::read_meta(&dir);
        let wait = if forced || store::due(meta.as_ref(), &t.hosts, now()) {
            match order::issue(&t, &base, &challenges)
                .await
                .and_then(|(chain, key)| store::install(&dir, &chain, &key, true))
            {
                Ok(()) => {
                    failures = 0;
                    forced = false;
                    let m = store::read_meta(&dir);
                    info!(
                        "certificate issued",
                        hosts = t.hosts.join(","),
                        expires = m.as_ref().map_or(0, |m| m.not_after),
                    );
                    renewed();
                    Duration::ZERO
                }
                Err(e) => {
                    failures += 1;
                    let left = meta.as_ref().filter(|m| !m.placeholder).map_or(-1, |m| m.not_after - now());
                    let (hint, wait) = (
                        "port 80 must reach this server and the names must resolve to it; `staging = true` tries without the rate limits",
                        backoff(failures),
                    );
                    if (0..URGENT).contains(&left) {
                        error!(
                            "certificate renewal failed and the certificate expires soon",
                            reason = e,
                            days_left = left / 86400,
                            hint = hint
                        );
                    } else {
                        warn!(
                            "certificate order failed; trying again later",
                            reason = e,
                            retry_in_s = wait.as_secs(),
                            hint = hint
                        );
                    }
                    wait
                }
            }
        } else {
            let due = store::renew_at(meta.as_ref().expect("not due means a certificate"));
            Duration::from_secs((due - now()).max(0) as u64).min(CHECK_EVERY)
        };
        // A little jitter so many apps do not order in the same second.
        let jitter = Duration::from_millis(u64::from(std::process::id()) % 60_000);
        tokio::select! {
            () = tokio::time::sleep(wait + jitter.min(wait / 10 + Duration::from_secs(1))) => {}
            () = force.notified() => forced = true,
            changed = cfg.changed() => {
                if changed.is_err() {
                    return;
                }
                t = cfg.borrow_and_update().clone();
                failures = 0;
            }
        }
    }
}

/// `warden tls`: every certificate Warden holds, soonest expiry first.
pub fn print_status() -> i32 {
    let mut all = status(&crate::fleet::state_dir());
    if all.is_empty() {
        println!("no certificates (an app gets one with a [tls] section: docs/tls.md)");
        return 0;
    }
    all.sort_by_key(|(_, m)| m.not_after);
    let now = now();
    for (_, m) in all {
        let days = (m.not_after - now) / 86400;
        let what = if m.placeholder { "placeholder (self-signed, waiting for the real one)" } else { "issued" };
        println!(
            "{:<40} {what}, {}",
            m.hosts.join(","),
            if days < 0 { format!("EXPIRED {} days ago", -days) } else { format!("{days} days left") }
        );
    }
    0
}

fn status(state: &Path) -> Vec<(String, store::Meta)> {
    let mut out = vec![];
    let Ok(rd) = std::fs::read_dir(state.join("tls")) else { return out };
    for e in rd.flatten() {
        if let Some(m) = store::read_meta(&e.path()) {
            out.push((e.file_name().to_string_lossy().into_owned(), m));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

//! `[unavailable]`: the 503 page (`crate::unavailable`) is on the app's port
//! only while no worker serves. Looked at once a tick; it appears after
//! `after` seconds without a serving worker (a crash restart or a reload is
//! shorter and never sees it) and goes the moment one serves.

use super::*;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[derive(Default)]
pub(super) struct State {
    down_since: Option<Instant>,
    page: Option<crate::unavailable::Responder>,
    /// Said why the page cannot be had (once per outage).
    told: bool,
}

impl Supervisor {
    pub(super) fn check_unavailable(&mut self) {
        let u = &self.cfg.unavailable;
        let serving = self.insts.values().any(|i| i.role == Role::Current && !i.stopping && i.ready_at.is_some());
        // `warden stop` and shutdown are the operator's doing, not an outage.
        if !u.enabled || serving || self.stopped || self.shutting_down || self.handoff_on {
            self.unavail = State::default();
            return;
        }
        let since = *self.unavail.down_since.get_or_insert_with(Instant::now);
        if self.unavail.page.is_some() || since.elapsed() < Duration::from_secs(u.after) {
            return;
        }
        let (Some(port), after) = (self.cfg.app.port, u.after) else { return };
        let ip = self.cfg.app.address.or(u.host).unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let body = u.page.as_ref().map(|p| read_page(p));
        let started = match body {
            Some(Err(e)) => Err(e),
            Some(Ok(b)) => crate::unavailable::Responder::start(SocketAddr::new(ip, port), Some(b), u.retry_after)
                .map_err(|e| e.to_string()),
            None => crate::unavailable::Responder::start(SocketAddr::new(ip, port), None, u.retry_after)
                .map_err(|e| e.to_string()),
        };
        match started {
            Ok(r) => {
                info!(
                    "no worker serving: answering with the 503 page",
                    address = SocketAddr::new(ip, port),
                    after_s = after,
                    hint = "it goes as soon as a worker serves again; `[unavailable]` configures it",
                );
                self.unavail.page = Some(r);
            }
            Err(e) if !self.unavail.told => {
                self.unavail.told = true;
                warn!(
                    "the 503 page could not be started",
                    address = SocketAddr::new(ip, port),
                    reason = e,
                    hint = "[unavailable] host must be the address the app listens on, and its page file readable",
                );
            }
            Err(_) => {}
        }
    }
}

fn read_page(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let f = std::fs::File::open(path).map_err(|e| format!("unavailable.page {}: {e}", path.display()))?;
    let mut b = Vec::new();
    f.take(crate::unavailable::MAX_PAGE + 1).read_to_end(&mut b).map_err(|e| format!("{}: {e}", path.display()))?;
    if b.len() as u64 > crate::unavailable::MAX_PAGE {
        return Err(format!("unavailable.page {} is over 64 KB", path.display()));
    }
    Ok(b)
}

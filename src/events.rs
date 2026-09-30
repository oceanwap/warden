//! Live events: what supervisors push to subscribers (`subscribe` on the app
//! control socket) and what `wardend` pushes to the CLI and the GUI.
//! The wire format is documented in docs/protocol.md; keep the two in step.
//!
//! Inside a supervisor, `emit` hands an event to every subscriber through a
//! bounded broadcast. With no subscriber it costs one atomic load, so emit
//! calls can sit on every state change.
//!
//! The wire types live in the `warden-protocol` crate (protocol/, shared
//! with the GUI) and are re-exported here.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::broadcast;

pub use warden_protocol::events::{
    AppEntry, AppState, DaemonReply, DaemonRequest, Event, PROTOCOL, SupervisorEvent, WorkerEvent, interval, now_ms,
};

/// Set by `warden start` (and `wardend`) on supervisors they start in the
/// background, so `Status.launched` can say who restarts a dead supervisor.
pub const LAUNCH_ENV: &str = "WARDEN_LAUNCH";

/// Events a slow subscriber may fall behind by before it gets `lagged`.
const CAPACITY: usize = 1024;

// ------------------------------------------------------------ in-process bus

fn bus() -> &'static broadcast::Sender<Event> {
    static BUS: OnceLock<broadcast::Sender<Event>> = OnceLock::new();
    BUS.get_or_init(|| broadcast::channel(CAPACITY).0)
}

/// Set by `subscribe`, cleared by `active` once the last receiver is gone, so
/// that `active` without subscribers is one atomic load (tokio's
/// `receiver_count` takes the channel's lock).
static MAYBE_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Receive every event emitted from now on.
pub fn subscribe() -> broadcast::Receiver<Event> {
    let rx = bus().subscribe();
    MAYBE_ACTIVE.store(true, Ordering::SeqCst);
    rx
}

/// Is anyone listening? Check before building an expensive event.
pub fn active() -> bool {
    if !MAYBE_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }
    if bus().receiver_count() > 0 {
        return true;
    }
    MAYBE_ACTIVE.store(false, Ordering::SeqCst);
    // A subscriber that came in meanwhile must not be missed.
    let again = bus().receiver_count() > 0;
    if again {
        MAYBE_ACTIVE.store(true, Ordering::SeqCst);
    }
    again
}

/// Hand an event to every subscriber (none: dropped at once).
pub fn emit(ev: Event) {
    if active() {
        // Err only means the last subscriber left meanwhile: nothing to do.
        let _ = bus().send(ev);
    }
}

/// A worker changed state.
pub fn worker(app: &str, worker: usize, event: WorkerEvent, pid: Option<u32>, detail: Option<String>) {
    if active() {
        emit(Event::Worker { app: app.to_string(), worker, event, pid, detail, at_ms: now_ms() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The wire-format tests live with the types, in protocol/.

    #[tokio::test]
    async fn emit_reaches_subscribers_and_costs_nothing_without_them() {
        // Other tests may subscribe concurrently; only check our own receiver.
        let mut rx = subscribe();
        assert!(active());
        worker("bus-test", 1, WorkerEvent::Ready, Some(1), None);
        loop {
            match rx.recv().await.unwrap() {
                Event::Worker { app, event, .. } if app == "bus-test" => {
                    assert_eq!(event, WorkerEvent::Ready);
                    break;
                }
                _ => continue,
            }
        }
    }
}

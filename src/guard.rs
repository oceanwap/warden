//! Panic containment for Warden's own tasks (CP4).
//!
//! Tokio isolates a panicking task and carries on, which is the dangerous
//! kind of failure for a supervisor: the task that waits on a worker could
//! die and the slot would look alive forever. So every task runs inside
//! [`CatchUnwind`], and its panic is turned into an explicit decision:
//!
//! - a task that **owns a worker** (exit waiter, output readers, IPC reader)
//!   gets that worker SIGKILLed and reported as exited, so normal crash
//!   handling starts a fresh one;
//! - an **essential loop** (signals, control socket, tick, health, metrics)
//!   takes Warden down with a clear log line: systemd restarts it, which is
//!   better than running half-deaf;
//! - a **per-request** task (one CLI connection) is just logged.
//!
//! A panic in the main event loop itself unwinds out of `main` (exit 101)
//! after the panic hook has logged it; workers drain via PDEATHSIG and
//! systemd restarts Warden.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A future that turns a panic during `poll` into `Err(message)`.
pub struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = Result<F::Output, String>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.get_mut().0.as_mut();
        match std::panic::catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Ready(v)) => Poll::Ready(Ok(v)),
            Ok(Poll::Pending) => Poll::Pending,
            Err(payload) => Poll::Ready(Err(panic_message(&*payload))),
        }
    }
}

pub fn catch_unwind<F: Future>(f: F) -> CatchUnwind<F> {
    CatchUnwind(Box::pin(f))
}

pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Run an essential loop; if it panics, log why and exit so systemd restarts us.
pub fn spawn_essential<F: Future<Output = ()> + 'static>(name: &'static str, f: F) {
    tokio::task::spawn_local(async move {
        if let Err(msg) = catch_unwind(f).await {
            crate::error!(
                "essential task failed; exiting so systemd restarts Warden",
                task = name,
                panic = msg,
                hint = "this is a Warden bug: please report it with the log lines above",
            );
            crate::logging::flush(std::time::Duration::from_millis(500));
            std::process::exit(70);
        }
    });
}

/// Run a per-request task; a panic only ends that request.
pub fn spawn_request<F: Future<Output = ()> + 'static>(name: &'static str, f: F) {
    tokio::task::spawn_local(async move {
        if let Err(msg) = catch_unwind(f).await {
            crate::error!("request handler failed; the request was dropped", task = name, panic = msg);
        }
    });
}

/// Install a panic hook that logs one clear line (location + message) before
/// the default unwinding. Printed to stderr directly: the logger itself may be
/// what panicked.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let msg = panic_message(info.payload());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "?".into());
        eprintln!(
            "{} ERROR warden panicked at {at}: {msg} (this is a bug; the log lines above show what led to it)",
            crate::logging::timestamp_now()
        );
    }));
}

/// Debug-build fault injection for tests: `WARDEN_FAULT=waiter:1,tick:3`
/// panics at the named point on its Nth hit. Compiled out of release builds.
pub fn fault(point: &str) {
    #[cfg(debug_assertions)]
    {
        use std::collections::HashMap;
        use std::sync::Mutex;
        static HITS: Mutex<Option<HashMap<String, u32>>> = Mutex::new(None);
        let Ok(spec) = std::env::var("WARDEN_FAULT") else { return };
        for item in spec.split(',') {
            let (name, nth) = item.split_once(':').unwrap_or((item, "1"));
            if name != point {
                continue;
            }
            let nth: u32 = nth.parse().unwrap_or(1);
            let hit = {
                let mut g = HITS.lock().unwrap_or_else(|e| e.into_inner());
                let n = g.get_or_insert_with(HashMap::new).entry(point.to_string()).or_insert(0);
                *n += 1;
                *n
            };
            if hit == nth {
                panic!("injected fault at {point} (hit {hit})");
            }
        }
    }
    #[cfg(not(debug_assertions))]
    let _ = point;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn catches_panics() {
        let ok = catch_unwind(async { 7 }).await;
        assert_eq!(ok, Ok(7));
        let err = catch_unwind(async { panic!("boom {}", 1) }).await;
        assert_eq!(err, Err("boom 1".to_string()));
        let err: Result<(), String> = catch_unwind(async {
            tokio::task::yield_now().await;
            panic!("after await")
        })
        .await;
        assert_eq!(err, Err("after await".to_string()));
    }
}

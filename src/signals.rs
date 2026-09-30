//! Unix signals → supervisor events.
//!
//! SIGTERM / SIGINT: graceful shutdown (a second one forces SIGKILL).
//! SIGHUP: rolling reload. SIGUSR1 / SIGUSR2: forwarded to every worker.

use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sig {
    Term,
    Int,
    Hup,
    Usr1,
    Usr2,
}

impl Sig {
    pub fn raw(self) -> i32 {
        match self {
            Sig::Term => libc::SIGTERM,
            Sig::Int => libc::SIGINT,
            Sig::Hup => libc::SIGHUP,
            Sig::Usr1 => libc::SIGUSR1,
            Sig::Usr2 => libc::SIGUSR2,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Sig::Term => "SIGTERM",
            Sig::Int => "SIGINT",
            Sig::Hup => "SIGHUP",
            Sig::Usr1 => "SIGUSR1",
            Sig::Usr2 => "SIGUSR2",
        }
    }
}

/// Install handlers and forward signals to `tx` until the receiver is gone.
pub fn listen(tx: mpsc::UnboundedSender<Sig>) -> std::io::Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    let mut int = signal(SignalKind::interrupt())?;
    let mut hup = signal(SignalKind::hangup())?;
    let mut usr1 = signal(SignalKind::user_defined1())?;
    let mut usr2 = signal(SignalKind::user_defined2())?;
    tokio::task::spawn_local(async move {
        loop {
            let s = tokio::select! {
                _ = term.recv() => Sig::Term,
                _ = int.recv() => Sig::Int,
                _ = hup.recv() => Sig::Hup,
                _ = usr1.recv() => Sig::Usr1,
                _ = usr2.recv() => Sig::Usr2,
            };
            if tx.send(s).is_err() {
                return;
            }
        }
    });
    Ok(())
}

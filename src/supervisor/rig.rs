//! Unit tests' supervisor: real processes (`sh` scripts speaking the shim's
//! fd-3 protocol), the event loop driven by hand until a condition holds.

use super::*;

pub(super) struct Rig {
    pub(super) sup: Supervisor,
    rx: mpsc::UnboundedReceiver<Event>,
    proc_rx: mpsc::UnboundedReceiver<ProcEvent>,
    dir: PathBuf,
}

impl Rig {
    /// A supervisor for app `name` on port 1 whose workers run `script`
    /// (`sh -c`); `sections` is the rest of the config (`[workers]`, …).
    pub(super) fn new(name: &str, sections: &str, script: &str) -> Rig {
        let dir = std::env::temp_dir().join(format!("warden-rig-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The control socket is never bound here; a short path keeps the
        // socket-length checks happy where temp_dir is long (macOS).
        let toml = format!(
            "[app]\nname = \"{name}\"\nargs = [\"x.js\"]\nport = 1\n{sections}\n[control]\nsocket = \"/tmp/wp.sock\"\n"
        );
        let mut cfg = Config::parse(&toml).unwrap();
        cfg.app.command = "sh".into();
        cfg.app.args = vec!["-c".into(), script.into()];
        let (tx, rx) = mpsc::unbounded_channel();
        let (proc_tx, proc_rx) = mpsc::unbounded_channel();
        let sup = Supervisor::new(cfg, None, dir.clone(), (None, None), tx, proc_tx);
        Rig { sup, rx, proc_rx, dir }
    }

    /// The supervisor's event loop, until `f` holds (checked after each event).
    pub(super) async fn until(&mut self, what: &str, f: impl Fn(&Supervisor) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f(&self.sup) {
            tokio::select! {
                Some(ev) = self.rx.recv() => self.sup.on_event(ev),
                Some(pe) = self.proc_rx.recv() => self.sup.on_proc(pe),
                _ = tokio::time::sleep_until(deadline) => panic!("timed out waiting for {what}"),
            }
        }
    }

    pub(super) fn kill(&self, inst: u64) {
        self.sup.insts[&inst].handle.signal(libc::SIGKILL);
    }

    pub(super) async fn shutdown(mut self) {
        self.sup.begin_shutdown("test");
        self.until("every process gone", |s| s.insts.is_empty()).await;
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(super) async fn local(f: impl std::future::Future<Output = ()>) {
    tokio::task::LocalSet::new().run_until(f).await
}

//! Release pinning (`[app] pin_release`, on by default).
//!
//! Deploys usually unpack a release next to the others and swap a `current`
//! symlink, then reload. A worker that crashes between the swap and the
//! reload must not come back on the new release while the others still run
//! the old one. So the working directory is resolved to its real path when
//! the supervisor starts and when a reload, safe-reload or restart of every
//! worker begins, and workers are started there. The command and arguments
//! that go through the same symlink are rewritten the same way. Crash
//! restarts and recycling (health, memory, lifetime) reuse the pin; a failed
//! rollout puts the previous pin back, with the previous config.
//!
//! If the pinned directory is gone (old releases cleaned up), the next start
//! re-resolves the symlink and says so, rather than failing to spawn.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Pin {
    /// `working_directory` as configured, made absolute.
    configured: PathBuf,
    /// Its real path when the pin was taken: where workers start.
    pub(super) real: PathBuf,
    /// The deepest symlink on the configured path, and where it pointed.
    link: Option<(PathBuf, PathBuf)>,
}

impl Pin {
    pub(super) fn resolve(configured: &Path) -> std::io::Result<Pin> {
        let configured = std::path::absolute(configured)?;
        let real = std::fs::canonicalize(&configured)?;
        if !real.is_dir() {
            return Err(std::io::Error::other(format!("{} is not a directory", real.display())));
        }
        let link = configured
            .ancestors()
            .find(|a| std::fs::symlink_metadata(a).is_ok_and(|m| m.file_type().is_symlink()))
            .and_then(|a| Some((a.to_path_buf(), std::fs::canonicalize(a).ok()?)));
        Ok(Pin { configured, real, link })
    }

    /// `p` through the pinned symlink, if it goes through it.
    pub(super) fn path(&self, p: &Path) -> Option<PathBuf> {
        let (from, to) = self.link.as_ref()?;
        p.strip_prefix(from).ok().map(|rest| to.join(rest))
    }

    /// A command or argument: an absolute path, or `--flag=/path`.
    pub(super) fn arg(&self, a: &str) -> String {
        if a.starts_with('/') {
            if let Some(p) = self.path(Path::new(a)) {
                return p.display().to_string();
            }
        } else if let Some((flag, value)) = a.split_once('=').filter(|(_, v)| v.starts_with('/')) {
            if let Some(p) = self.path(Path::new(value)) {
                return format!("{flag}={}", p.display());
            }
        }
        a.to_string()
    }

    /// Workers don't start in the configured path itself (a symlink is involved).
    fn moved(&self) -> bool {
        self.real != self.configured
    }
}

impl Supervisor {
    /// Take the pin (start, reload, safe-reload, restart of every worker).
    /// `Err` when the working directory can't be resolved; the pin is then
    /// left alone.
    pub(super) fn pin_release(&mut self) -> Result<(), String> {
        let Some(pin) = self.resolve_pin() else {
            self.release = None;
            return Ok(());
        };
        let pin = pin?;
        let wd = &pin.configured;
        if self.release.as_ref().map(|p| &p.real) != Some(&pin.real) && pin.moved() {
            match &self.release {
                Some(old) => info!(
                    "release pinned",
                    release = pin.real.display(),
                    previous = old.real.display(),
                    working_directory = wd.display(),
                ),
                None => info!(
                    "release pinned",
                    release = pin.real.display(),
                    working_directory = wd.display(),
                    hint = "crash restarts stay on this release; reload, safe-reload or restart move to a new one",
                ),
            }
        }
        self.release = Some(pin);
        Ok(())
    }

    /// The pin `working_directory` gives now; None when pinning is off.
    fn resolve_pin(&self) -> Option<Result<Pin, String>> {
        let wd = self.cfg.app.working_directory.as_ref().filter(|_| self.cfg.app.pin_release)?;
        Some(Pin::resolve(wd).map_err(|e| format!("working_directory {} cannot be resolved: {e}", wd.display())))
    }

    /// Before each spawn: the pinned release must still exist. If it was
    /// deleted, re-resolve (the worker then starts on whatever `current`
    /// points to now) and say so.
    pub(super) fn check_release(&mut self) {
        let gone = match &self.release {
            Some(p) if p.real.is_dir() => return,
            Some(p) => p.real.clone(),
            // Not resolvable at start (missing directory): try again now;
            // if it still fails, the spawn fails and says why.
            None => {
                if let Some(Ok(_)) = self.resolve_pin() {
                    let _ = self.pin_release();
                }
                return;
            }
        };
        match self.resolve_pin() {
            Some(Ok(pin)) => {
                warn!(
                    "the pinned release directory is gone; starting the worker in the current release instead",
                    pinned = gone.display(),
                    now = pin.real.display(),
                    reason = "it was deleted or moved after the pin was taken",
                    hint = "delete old releases only after a reload has moved every worker off them; this worker may \
                            now run another release than the rest (`warden reload` brings them all to it)",
                );
                self.release = Some(pin);
            }
            Some(Err(e)) => warn!(
                "the pinned release directory is gone, and working_directory can't be resolved either",
                pinned = gone.display(),
                error = e,
                hint = "restore the release or fix the `current` symlink; starting the worker fails until then",
            ),
            None => self.release = None, // pinning was turned off by a reload
        }
    }

    /// Where workers start: the pinned release, else the configured path.
    pub(super) fn worker_dir(&self) -> Option<PathBuf> {
        match &self.release {
            Some(p) => Some(p.real.clone()),
            None => self.cfg.app.working_directory.clone(),
        }
    }

    /// A command or argument, rewritten into the pinned release.
    pub(super) fn pinned_arg(&self, a: &str) -> String {
        match &self.release {
            Some(p) => p.arg(a),
            None => a.to_string(),
        }
    }

    /// A path, rewritten into the pinned release.
    pub(super) fn pinned_path(&self, p: PathBuf) -> PathBuf {
        self.release.as_ref().and_then(|r| r.path(&p)).unwrap_or(p)
    }

    /// For `status`: the pinned release.
    pub(super) fn release_text(&self) -> Option<String> {
        self.release.as_ref().map(|p| p.real.display().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pins_through_a_current_symlink() {
        let dir = std::env::temp_dir().join(format!("warden-pin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for r in ["releases/v1/api", "releases/v2/api"] {
            std::fs::create_dir_all(dir.join(r)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::os::unix::fs::symlink(dir.join("releases/v1"), dir.join("current")).unwrap();

        // The symlink is the working directory itself.
        let p = Pin::resolve(&dir.join("current")).unwrap();
        assert_eq!(p.real, dir.join("releases/v1"));
        assert!(p.moved());
        let arg = dir.join("current/dist/main.js").display().to_string();
        assert_eq!(p.arg(&arg), dir.join("releases/v1/dist/main.js").display().to_string());
        let flag = format!("--import={}", dir.join("current/shim.mjs").display());
        assert_eq!(p.arg(&flag), format!("--import={}", dir.join("releases/v1/shim.mjs").display()));
        assert_eq!(p.arg("dist/main.js"), "dist/main.js", "relative: resolved from the pinned cwd");
        assert_eq!(p.arg("/usr/bin/bun"), "/usr/bin/bun");
        assert_eq!(p.arg("--max-old-space-size=512"), "--max-old-space-size=512");

        // The symlink is an ancestor: paths through it (not only below the
        // working directory) follow the pin.
        let p = Pin::resolve(&dir.join("current/api")).unwrap();
        assert_eq!(p.real, dir.join("releases/v1/api"));
        let shared = dir.join("current/shared/x.js").display().to_string();
        assert_eq!(p.arg(&shared), dir.join("releases/v1/shared/x.js").display().to_string());

        // Swapping the symlink doesn't move a pin already taken.
        std::fs::remove_file(dir.join("current")).unwrap();
        std::os::unix::fs::symlink(dir.join("releases/v2"), dir.join("current")).unwrap();
        assert_eq!(p.real, dir.join("releases/v1/api"));
        assert_eq!(Pin::resolve(&dir.join("current/api")).unwrap().real, dir.join("releases/v2/api"));

        // No symlink: nothing to rewrite.
        let p = Pin::resolve(&dir.join("releases/v2")).unwrap();
        assert!(!p.moved() && p.link.is_none());
        assert_eq!(p.arg(&arg), arg);

        assert!(Pin::resolve(&dir.join("missing")).is_err());
        std::fs::write(dir.join("file"), "x").unwrap();
        assert!(Pin::resolve(&dir.join("file")).is_err(), "not a directory");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

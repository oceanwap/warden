//! The remote machines the window knows (the connection menu) and the Settings
//! choice of colors, kept in
//! `~/.config/warden/gui.json` so they are there next time. This machine is
//! never listed: it needs no configuration, and the window always starts on it.
//!
//! Reading and writing are best effort: a missing, unreadable or broken file
//! is an empty list, and a failed save only costs the next start its list.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One SSH machine: what `Connection` needs to reach its wardend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// `user@host`, or a Host of ~/.ssh/config: also the machine's name here.
    pub dest: String,
    /// wardend's socket there.
    pub remote_socket: String,
    /// The `warden` CLI there.
    pub remote_warden: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    pub machines: Vec<Machine>,
    /// The Settings choice of colors and mode.
    #[serde(default)]
    pub appearance: crate::system::Source,
}

impl Saved {
    /// Add `m`, or replace the machine with its `dest` (an edit).
    pub fn remember(&mut self, m: Machine) {
        match self.machines.iter_mut().find(|x| x.dest == m.dest) {
            Some(x) => *x = m,
            None => self.machines.push(m),
        }
    }

    pub fn forget(&mut self, dest: &str) {
        self.machines.retain(|m| m.dest != dest);
    }

    pub fn get(&self, dest: &str) -> Option<&Machine> {
        self.machines.iter().find(|m| m.dest == dest)
    }
}

/// `$XDG_CONFIG_HOME/warden/gui.json`, else `~/.config/warden/gui.json` (where
/// Warden keeps its other per-user files).
pub fn path() -> Option<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => PathBuf::from(std::env::var_os("HOME").filter(|v| !v.is_empty())?).join(".config"),
    };
    Some(base.join("warden").join("gui.json"))
}

pub fn load(path: &Path) -> Saved {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Write the list (atomically: a temporary file, then a rename).
pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(saved).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(dest: &str) -> Machine {
        Machine { dest: dest.into(), remote_socket: "/run/warden/wardend.sock".into(), remote_warden: "warden".into() }
    }

    #[test]
    fn remembering_a_machine_again_replaces_it() {
        let mut s = Saved::default();
        s.remember(m("deploy@web-1"));
        s.remember(m("deploy@web-2"));
        let mut changed = m("deploy@web-1");
        changed.remote_socket = "/run/user/1000/warden/wardend.sock".into();
        s.remember(changed.clone());
        assert_eq!(s.machines.len(), 2, "an edit does not duplicate");
        assert_eq!(s.get("deploy@web-1"), Some(&changed));
        assert_eq!(s.machines[0].dest, "deploy@web-1", "and keeps its place");
    }

    #[test]
    fn forgetting_a_machine() {
        let mut s = Saved::default();
        s.remember(m("a@x"));
        s.remember(m("b@y"));
        s.forget("a@x");
        assert_eq!(s.machines.iter().map(|m| m.dest.as_str()).collect::<Vec<_>>(), ["b@y"]);
        s.forget("nobody@nowhere"); // not there: nothing happens
        assert_eq!(s.machines.len(), 1);
    }

    #[test]
    fn the_list_survives_a_restart_and_a_broken_file_is_an_empty_list() {
        let dir = std::env::temp_dir().join(format!("wg-hosts-{}", std::process::id()));
        let file = dir.join("warden").join("gui.json");
        let mut s = Saved::default();
        s.remember(m("deploy@web-1"));
        save(&file, &s).unwrap();
        assert_eq!(load(&file), s);
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load(&file), Saved::default());
        assert_eq!(load(&dir.join("missing.json")), Saved::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_from_a_newer_gui_with_more_fields_still_loads() {
        let dir = std::env::temp_dir().join(format!("wg-hosts-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("gui.json");
        std::fs::write(
            &file,
            r#"{"machines":[{"dest":"a@x","remote_socket":"/s","remote_warden":"warden","color":"red"}],"last":"a@x","theme":"light"}"#,
        )
        .unwrap();
        assert_eq!(load(&file).machines.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

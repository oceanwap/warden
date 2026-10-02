//! The remote machines the window knows (the connection menu), the Settings
//! choice of colors, and whether the "install the command line tool" banner was
//! turned away, kept in
//! `~/.config/warden/gui.json` so they are there next time. This machine is
//! never listed: it needs no configuration, and the window always starts on it.
//!
//! A missing file is an empty list. A file that is damaged is never silently
//! replaced: whatever parts of it are good are kept (one bad machine, or one
//! value a newer window wrote, does not drop the others), and the original is
//! kept as `gui.json.bad` before the next save writes a new `gui.json`; the
//! window tells what was wrong. A file that cannot be read at all is not
//! written over. A save keeps the keys this version does not know, and is
//! atomic (a temporary file, synced, then a rename).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

/// One SSH machine: what `Connection` needs to reach its wardend.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// `user@host`, or a Host of ~/.ssh/config: also the machine's name here.
    pub dest: String,
    /// wardend's socket there.
    #[serde(default = "default_remote_socket")]
    pub remote_socket: String,
    /// The `warden` CLI there.
    #[serde(default = "default_remote_warden")]
    pub remote_warden: String,
}

fn default_remote_socket() -> String {
    crate::ssh::default_remote_socket()
}

fn default_remote_warden() -> String {
    "warden".into()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub machines: Vec<Machine>,
    /// The Settings choice of colors and mode.
    #[serde(default)]
    pub appearance: crate::system::Source,
    /// "Not now" on the first-run banner that offers the command line tool: it stays away.
    #[serde(default)]
    pub cli_banner_dismissed: bool,
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

/// What reading the file found.
#[derive(Debug, Default)]
pub struct Loaded {
    pub saved: Saved,
    /// What was wrong with the file, in words (and where the original was kept);
    /// `None` when it was fine, or not there yet.
    pub problem: Option<String>,
    /// False when the file could not be read or kept: writing would destroy it.
    pub writable: bool,
}

/// What the file holds, whatever is wrong with it (nothing is moved or written).
pub fn load(path: &Path) -> Saved {
    std::fs::read_to_string(path).ok().and_then(|t| parse(&t).ok()).map(|(s, _)| s).unwrap_or_default()
}

/// Read the file at start: like `load`, and when it is damaged, keep the
/// original as `gui.json.bad` (moved when nothing of it could be used, copied
/// when the good parts were) so that the next save cannot destroy it.
pub fn load_checked(path: &Path) -> Loaded {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Loaded { writable: true, ..Loaded::default() },
        Err(e) => {
            return Loaded {
                saved: Saved::default(),
                problem: Some(format!(
                    "{} could not be read ({e}); the settings are not saved this time",
                    path.display()
                )),
                writable: false,
            };
        }
    };
    let (saved, what, moved) = match parse(&text) {
        Ok((saved, notes)) if notes.is_empty() => return Loaded { saved, problem: None, writable: true },
        Ok((saved, notes)) => (saved, notes.join("; "), false),
        Err(e) => (Saved::default(), e, true),
    };
    let bad = bad_path(path);
    let kept = if moved { std::fs::rename(path, &bad) } else { std::fs::copy(path, &bad).map(|_| ()) };
    let (problem, writable) = match kept {
        Ok(()) => (
            format!("{} was not read in full ({what}); the original is kept as {}", path.display(), bad.display()),
            true,
        ),
        Err(e) => (
            format!(
                "{} was not read in full ({what}) and could not be kept as {} ({e}); the settings are not saved this time",
                path.display(),
                bad.display()
            ),
            false,
        ),
    };
    Loaded { saved, problem: Some(problem), writable }
}

/// Where a damaged file is kept: `gui.json.bad` beside it.
pub fn bad_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_else(|| "gui.json".into());
    name.push(".bad");
    path.with_file_name(name)
}

/// What can be used of `text`, and what could not (one note each). Only text
/// that is not a JSON object is an error: a machine that does not parse, or a
/// value this version does not know, costs that machine or that value only.
fn parse(text: &str) -> Result<(Saved, Vec<String>), String> {
    let Value::Object(doc) = serde_json::from_str::<Value>(text).map_err(|e| e.to_string())? else {
        return Err("it is not a JSON object".into());
    };
    let mut saved = Saved::default();
    let mut notes = Vec::new();
    match doc.get("machines") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for (i, m) in list.iter().enumerate() {
                match serde_json::from_value::<Machine>(m.clone()) {
                    Ok(m) if !m.dest.trim().is_empty() => saved.remember(m),
                    Ok(_) => notes.push(format!("machine {} has no address", i + 1)),
                    Err(e) => notes.push(format!("machine {}: {e}", i + 1)),
                }
            }
        }
        Some(_) => notes.push("\"machines\" is not a list".into()),
    }
    match doc.get("appearance") {
        None | Some(Value::Null) => {}
        Some(Value::Object(a)) => {
            if let Some(v) = a.get("colors") {
                match serde_json::from_value(v.clone()) {
                    Ok(c) => saved.appearance.colors = c,
                    Err(e) => notes.push(format!("colors: {e}")),
                }
            }
            if let Some(v) = a.get("mode") {
                match serde_json::from_value(v.clone()) {
                    Ok(m) => saved.appearance.mode = m,
                    Err(e) => notes.push(format!("mode: {e}")),
                }
            }
        }
        Some(_) => notes.push("\"appearance\" is not an object".into()),
    }
    match doc.get("cli_banner_dismissed") {
        None | Some(Value::Null) => {}
        Some(Value::Bool(b)) => saved.cli_banner_dismissed = *b,
        Some(_) => notes.push("\"cli_banner_dismissed\" is not true or false".into()),
    }
    Ok((saved, notes))
}

/// Write the list (atomically: a temporary file of this process, synced, then a
/// rename). Keys of the file that this version does not know (a newer window
/// wrote them) stay.
pub fn save(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut doc = match std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    if let Value::Object(mine) = serde_json::to_value(saved).map_err(std::io::Error::other)? {
        doc.extend(mine);
    }
    let bytes = serde_json::to_vec_pretty(&doc).map_err(std::io::Error::other)?;
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(&bytes).and_then(|()| f.sync_all()));
    if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
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
    fn the_banner_turned_away_is_remembered_and_an_older_file_has_it_not_turned_away() {
        let dir = std::env::temp_dir().join(format!("wg-hosts-banner-{}", std::process::id()));
        let file = dir.join("gui.json");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&file, r#"{"machines":[]}"#).unwrap();
        assert!(!load(&file).cli_banner_dismissed, "a file from before the banner existed");
        let mut s = load(&file);
        s.cli_banner_dismissed = true;
        save(&file, &s).unwrap();
        assert!(load(&file).cli_banner_dismissed);
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

    /// A folder of this test, removed when the test ends (also when it fails).
    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Dir {
            let d = std::env::temp_dir().join(format!("wg-hosts-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Dir(d)
        }
        fn file(&self) -> PathBuf {
            self.0.join("gui.json")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_file_that_is_not_json_is_kept_as_bad_and_the_next_save_leaves_it_alone() {
        let d = Dir::new("notjson");
        let text = r#"{"machines":[{"dest":"deploy@web-1","remote_sock"#; // cut off in the middle
        std::fs::write(d.file(), text).unwrap();
        let loaded = load_checked(&d.file());
        assert!(loaded.saved.machines.is_empty());
        assert!(loaded.writable);
        let said = loaded.problem.expect("it says what happened");
        assert!(said.contains("gui.json.bad") && said.contains("EOF"), "{said}");
        assert_eq!(std::fs::read_to_string(bad_path(&d.file())).unwrap(), text, "the original is kept as it was");
        assert!(!d.file().exists(), "and moved, not copied: nothing of it was usable");
        // The next save writes a new file and does not touch the kept one.
        let mut s = Saved::default();
        s.remember(m("deploy@web-2"));
        save(&d.file(), &s).unwrap();
        assert_eq!(load(&d.file()), s);
        assert_eq!(std::fs::read_to_string(bad_path(&d.file())).unwrap(), text);
    }

    #[test]
    fn one_value_this_version_does_not_know_does_not_drop_the_machines() {
        let d = Dir::new("unknown");
        let text = r#"{"machines":[{"dest":"a@x","remote_socket":"/s","remote_warden":"w"}],
            "appearance":{"colors":"sepia","mode":"dark"},"cli_banner_dismissed":true}"#;
        std::fs::write(d.file(), text).unwrap();
        let loaded = load_checked(&d.file());
        assert_eq!(loaded.saved.machines.len(), 1, "the machine survives");
        assert_eq!(loaded.saved.appearance.mode, crate::system::Mode::Dark, "so does the good half of the choice");
        assert_eq!(loaded.saved.appearance.colors, crate::system::Colors::Warden, "the unknown one is the default");
        assert!(loaded.saved.cli_banner_dismissed);
        let said = loaded.problem.expect("told");
        assert!(said.contains("sepia") && said.contains("gui.json.bad"), "{said}");
        assert_eq!(
            std::fs::read_to_string(bad_path(&d.file())).unwrap(),
            text,
            "the original is copied before a save changes it"
        );
        assert!(d.file().exists(), "a file that is mostly good stays in place");
    }

    #[test]
    fn a_machine_with_a_field_missing_keeps_the_others_and_one_without_an_address_is_skipped() {
        let d = Dir::new("missing");
        std::fs::write(
            d.file(),
            r#"{"machines":[{"dest":"a@x"},{"remote_socket":"/s"},{"dest":"c@z","remote_socket":5},{"dest":"d@w","remote_socket":"/d","remote_warden":"/opt/warden"}]}"#,
        )
        .unwrap();
        let loaded = load_checked(&d.file());
        let names: Vec<_> = loaded.saved.machines.iter().map(|m| m.dest.as_str()).collect();
        assert_eq!(names, ["a@x", "d@w"]);
        let a = loaded.saved.get("a@x").unwrap();
        assert_eq!(a.remote_warden, "warden", "a missing field has its usual value");
        assert_eq!(a.remote_socket, crate::ssh::default_remote_socket());
        let said = loaded.problem.expect("told");
        assert!(said.contains("machine 2") && said.contains("machine 3"), "{said}");
        assert!(bad_path(&d.file()).exists());
    }

    #[test]
    fn a_good_file_says_nothing_and_a_missing_one_is_empty_and_writable() {
        let d = Dir::new("good");
        let mut s = Saved::default();
        s.remember(m("a@x"));
        s.cli_banner_dismissed = true;
        save(&d.file(), &s).unwrap();
        let loaded = load_checked(&d.file());
        assert!(loaded.problem.is_none() && loaded.writable);
        assert_eq!(loaded.saved, s);
        assert!(!bad_path(&d.file()).exists());
        let none = load_checked(&d.0.join("not-there.json"));
        assert!(none.problem.is_none() && none.writable && none.saved == Saved::default());
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_written_over() {
        let d = Dir::new("unreadable");
        // A directory where the file should be: reading it fails with something other than "not found".
        std::fs::create_dir(d.file()).unwrap();
        let loaded = load_checked(&d.file());
        assert!(!loaded.writable, "so the window will not write there");
        assert!(loaded.problem.unwrap().contains("not saved"));
    }

    #[test]
    fn a_save_keeps_what_a_newer_window_wrote_and_leaves_no_temporary_file() {
        let d = Dir::new("newer");
        std::fs::write(d.file(), r#"{"machines":[],"layout":{"sidebar":240},"theme":"light"}"#).unwrap();
        let mut s = load(&d.file());
        s.remember(m("a@x"));
        save(&d.file(), &s).unwrap();
        let doc: Value = serde_json::from_str(&std::fs::read_to_string(d.file()).unwrap()).unwrap();
        assert_eq!(doc["layout"]["sidebar"], 240, "a key this version does not know stays");
        assert_eq!(doc["theme"], "light");
        assert_eq!(doc["machines"].as_array().unwrap().len(), 1, "and this version's keys are the new ones");
        let left: Vec<_> = std::fs::read_dir(&d.0).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, [std::ffi::OsString::from("gui.json")], "{left:?}");
    }
}

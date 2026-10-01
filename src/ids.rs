//! Numeric app ids, as `pm2 list` shows them, so `warden restart 3` and
//! `warden start 0,1,2` work between commands.
//!
//! An app's id is assigned the first time the CLI sees it: the lowest number
//! not taken, in the order the configs sort (so a first `warden list` numbers
//! the apps alphabetically, and `warden start server.js --name api` after
//! that gets the next free number). It is written to `ids.json` in the state
//! directory and never changes after that, whichever apps come and go. It is
//! given up only by `warden delete` (the number is then free again); an app
//! whose config merely disappeared keeps its number reserved, so restoring
//! the file brings the same id back.
//!
//! Ids are for typing. They are per user (the state directory is), not part
//! of any config, and nothing in a supervisor, a worker or the wire protocol
//! uses them; scripts should name apps.
//!
//! The file is rewritten atomically under a lock, so two commands that both
//! meet a new app cannot give the same number to two apps. If the file cannot
//! be read or written the CLI still works: the ids shown are worked out in
//! memory (the same way) and `list` says they may change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const FILE: &str = "ids.json";
const LOCK: &str = "ids.lock";
/// How long to wait for another command that is updating the file.
const LOCK_WAIT: Duration = Duration::from_millis(1500);

/// The ids for a set of app names, and why they might not be saved.
#[derive(Debug, Default, PartialEq)]
pub struct Ids {
    pub map: BTreeMap<String, u32>,
    /// Set when the file was damaged, or could not be read, locked or
    /// written: the ids are right for now, but may not be remembered.
    pub warning: Option<String>,
}

impl Ids {
    pub fn get(&self, name: &str) -> Option<u32> {
        self.map.get(name).copied()
    }
}

fn file(dir: &Path) -> PathBuf {
    dir.join(FILE)
}

/// What reading `ids.json` found. Reading never changes anything on disk.
#[derive(Debug, PartialEq)]
enum Problem {
    /// Not valid JSON, or not text: set aside (under the lock) and started again.
    Damaged(String),
    /// Could not be read at all (I/O error): left exactly as it is.
    Unreadable(String),
}

/// The saved ids (a missing file is an empty registry) and what is wrong
/// with the file, if anything. A damaged or unreadable file reads as empty.
fn read(dir: &Path) -> (BTreeMap<String, u32>, Option<Problem>) {
    let path = file(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (BTreeMap::new(), None),
        Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
            return (BTreeMap::new(), Some(Problem::Damaged("it is not text".into())));
        }
        Err(e) => return (BTreeMap::new(), Some(Problem::Unreadable(format!("cannot read {}: {e}", path.display())))),
    };
    match serde_json::from_str::<BTreeMap<String, u32>>(&text) {
        Ok(m) => {
            // Hand-edited duplicates: the first (by name) keeps the number.
            let mut seen = std::collections::BTreeSet::new();
            (m.into_iter().filter(|(_, id)| seen.insert(*id)).collect(), None)
        }
        Err(e) => (BTreeMap::new(), Some(Problem::Damaged(e.to_string()))),
    }
}

/// Set a damaged file aside as `ids.json.bad` (the caller holds the lock) and
/// say so.
fn set_aside(dir: &Path, why: &str) -> String {
    let path = file(dir);
    let bad = path.with_extension("json.bad");
    let moved = match std::fs::rename(&path, &bad) {
        Ok(()) => format!("moved it to {}", bad.display()),
        Err(_) => "could not move it aside".into(),
    };
    format!("{} is damaged ({why}); {moved} and numbered the apps again", path.display())
}

fn write(dir: &Path, map: &BTreeMap<String, u32>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())?;
    // A temp file left by a crashed run with this pid would make the
    // exclusive create fail forever; under the lock nobody else is using it.
    let _ = std::fs::remove_file(file(dir).with_extension(format!("tmp{}", std::process::id())));
    crate::fleet::write_private(&file(dir), &(text + "\n"), 0o644)
}

/// Run `f` while holding the lock. `Err`: the lock could not be had
/// (directory not writable, or another command held it for too long); the
/// caller then works in memory.
fn locked<T>(dir: &Path, f: impl FnOnce() -> T) -> Result<T, String> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let lock = dir.join(LOCK);
    let handle = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o644)
        .open(&lock)
        .map_err(|e| format!("cannot open {}: {e}", lock.display()))?;
    let start = Instant::now();
    loop {
        match crate::sys::try_lock_exclusive(&handle) {
            Ok(true) => break,
            Ok(false) if start.elapsed() < LOCK_WAIT => std::thread::sleep(Duration::from_millis(10)),
            Ok(false) => return Err(format!("{} is locked by another warden command", lock.display())),
            Err(e) => return Err(format!("cannot lock {}: {e}", lock.display())),
        }
    }
    let out = f();
    drop(handle); // releases the lock
    Ok(out)
}

/// The lowest number not in `taken`.
fn free(taken: &BTreeMap<String, u32>) -> u32 {
    let used: std::collections::BTreeSet<u32> = taken.values().copied().collect();
    (0..).find(|n| !used.contains(n)).unwrap_or(0)
}

/// Number every name in `map` that has no id yet, in order; true if any was new.
fn number(map: &mut BTreeMap<String, u32>, names: &[&str]) -> bool {
    let mut added = false;
    for n in names {
        if !map.contains_key(*n) {
            let id = free(map);
            map.insert((*n).to_string(), id);
            added = true;
        }
    }
    added
}

/// Give every name in `names` (in this order) an id it does not have yet and
/// return the ids of exactly those names. Saves when something new was
/// numbered.
pub fn assign(dir: &Path, names: &[&str]) -> Ids {
    // The usual case: every app already has its number. Read the file (it is
    // replaced atomically, so no lock is needed to read it) and touch nothing,
    // so `list` works on a read-only state directory too.
    let (saved, problem) = read(dir);
    if problem.is_none() && names.iter().all(|n| saved.contains_key(*n)) {
        let map = names.iter().filter_map(|n| saved.get(*n).map(|id| ((*n).to_string(), *id))).collect();
        return Ids { map, warning: None };
    }
    let mut notes: Vec<String> = Vec::new();
    let result = locked(dir, || {
        // Read again now that nobody else can change it, and only then act.
        let (mut map, problem) = read(dir);
        let mut notes = Vec::new();
        let mut writable = true;
        match problem {
            Some(Problem::Damaged(why)) => notes.push(set_aside(dir, &why)),
            Some(Problem::Unreadable(e)) => {
                // Do not write over a file we could not read: number in memory.
                notes.push(format!("{e}; the ids shown may change"));
                writable = false;
            }
            None => {}
        }
        let added = number(&mut map, names);
        let saved = if writable && (added || !notes.is_empty()) { write(dir, &map) } else { Ok(()) };
        if let Err(e) = saved {
            notes.push(format!("cannot save app ids: {e}; they may change"));
        }
        (map, notes)
    });
    let map = match result {
        Ok((map, n)) => {
            notes.extend(n);
            map
        }
        Err(e) => {
            // No lock, so no saving and no moving files: work it out in memory.
            let (mut map, problem) = read(dir);
            if let Some(Problem::Damaged(why)) = problem {
                notes.push(format!("{} is damaged ({why})", file(dir).display()));
            } else if let Some(Problem::Unreadable(m)) = problem {
                notes.push(m);
            }
            number(&mut map, names);
            notes.push(format!("cannot save app ids: {e}; they may change"));
            map
        }
    };
    let map = names.iter().filter_map(|n| map.get(*n).map(|id| ((*n).to_string(), *id))).collect();
    Ids { map, warning: (!notes.is_empty()).then(|| notes.join("; ")) }
}

/// The ids already saved for `names`, without numbering anything new (a
/// config given with `-c` is not one of the host's apps).
pub fn lookup(dir: &Path, names: &[&str]) -> BTreeMap<String, u32> {
    let (map, _) = read(dir);
    names.iter().filter_map(|n| map.get(*n).map(|id| ((*n).to_string(), *id))).collect()
}

/// Give `name` its id now (a new app was just created) and return it.
pub fn register(dir: &Path, name: &str) -> Option<u32> {
    assign(dir, &[name]).get(name)
}

/// `warden delete`: the app's number is free again.
pub fn forget(dir: &Path, name: &str) -> Result<(), String> {
    locked(dir, || match read(dir) {
        (_, Some(Problem::Unreadable(e))) => Err(e),
        (mut map, _) => {
            // (A damaged file reads as empty: nothing to forget, and `assign`
            // sets it aside next time.)
            if map.remove(name).is_some() { write(dir, &map) } else { Ok(()) }
        }
    })?
}

/// `1,2,3,5` -> `1-3,5`: the ids as `warden start` takes them, runs of
/// three or more as a range.
pub fn compact(ids: &[u32]) -> String {
    let mut v: Vec<u32> = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let mut j = i;
        while j + 1 < v.len() && v[j + 1] == v[j] + 1 {
            j += 1;
        }
        if j - i >= 2 {
            parts.push(format!("{}-{}", v[i], v[j]));
        } else {
            parts.extend(v[i..=j].iter().map(u32::to_string));
        }
        i = j + 1;
    }
    parts.join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("warden-ids-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn first_sight_numbers_in_order_and_sticks() {
        let d = tmp("order");
        let a = assign(&d, &["api", "queue", "web"]);
        assert_eq!(a.warning, None);
        assert_eq!((a.get("api"), a.get("queue"), a.get("web")), (Some(0), Some(1), Some(2)));
        // Same ids on the next command, and a new app gets the next number
        // whatever its name sorts like.
        let b = assign(&d, &["aaa", "api", "queue", "web"]);
        assert_eq!((b.get("aaa"), b.get("api"), b.get("web")), (Some(3), Some(0), Some(2)));
        // An app that is gone keeps its number: nothing else takes it.
        let c = assign(&d, &["api", "new"]);
        assert_eq!((c.get("api"), c.get("new")), (Some(0), Some(4)));
        let again = assign(&d, &["web"]);
        assert_eq!(again.get("web"), Some(2));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn delete_frees_the_lowest_number() {
        let d = tmp("forget");
        assign(&d, &["a", "b", "c"]);
        forget(&d, "b").unwrap();
        forget(&d, "never-existed").unwrap();
        assert_eq!(register(&d, "d"), Some(1));
        assert_eq!(lookup(&d, &["a", "b", "c", "d"]).len(), 3);
        assert_eq!(lookup(&d, &["b"]).len(), 0, "lookup never numbers anything");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_damaged_file_is_set_aside_and_a_read_only_dir_still_numbers() {
        let d = tmp("bad");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(file(&d), "{ not json").unwrap();
        let a = assign(&d, &["x", "y"]);
        assert_eq!((a.get("x"), a.get("y")), (Some(0), Some(1)));
        // Numbered again from scratch, and said so (the file is replaced).
        assert!(a.warning.as_deref().is_some_and(|m| m.contains("damaged")), "{:?}", a.warning);
        assert!(d.join("ids.json.bad").exists());
        assert!(std::fs::read_to_string(file(&d)).unwrap().contains("\"x\": 0"));

        // Duplicated numbers in a hand-edited file: the first name keeps it.
        std::fs::write(file(&d), r#"{"a":1,"b":1}"#).unwrap();
        let dup = assign(&d, &["a", "b"]);
        assert_eq!((dup.get("a"), dup.get("b")), (Some(1), Some(0)));

        // A directory that cannot be created: still numbers, says it cannot save.
        let blocked = d.join("ids.json").join("nested");
        let w = assign(&blocked, &["p", "q"]);
        assert_eq!((w.get("p"), w.get("q")), (Some(0), Some(1)));
        assert!(w.warning.as_deref().is_some_and(|m| m.contains("cannot save app ids")), "{:?}", w.warning);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A file we cannot read (here: a directory in its place) is never written
    /// over, and a file that is not text is set aside like a damaged one; a
    /// command with nothing new to number only reads.
    #[test]
    fn unreadable_is_left_alone_and_nothing_new_means_no_writes() {
        let d = tmp("unreadable");
        std::fs::create_dir_all(file(&d)).unwrap();
        let a = assign(&d, &["x", "y"]);
        assert_eq!((a.get("x"), a.get("y")), (Some(0), Some(1)), "still numbered, in memory");
        assert!(
            a.warning.as_deref().is_some_and(|m| m.contains("cannot read") && m.contains("may change")),
            "{:?}",
            a.warning
        );
        assert!(file(&d).is_dir(), "the unreadable file was not replaced");
        std::fs::remove_dir_all(file(&d)).unwrap();

        std::fs::write(file(&d), [0xffu8, 0xfe, 0x00]).unwrap();
        let b = assign(&d, &["x"]);
        assert_eq!(b.get("x"), Some(0));
        assert!(b.warning.as_deref().is_some_and(|m| m.contains("not text")), "{:?}", b.warning);
        assert!(d.join("ids.json.bad").exists());

        // Everything already numbered: no lock file is touched, no warning.
        let _ = std::fs::remove_file(d.join(LOCK));
        let c = assign(&d, &["x"]);
        assert_eq!((c.get("x"), c.warning), (Some(0), None));
        assert!(!d.join(LOCK).exists(), "a command with nothing new only reads");
        // A stale temp file from a crashed run with our pid does not block saving.
        std::fs::write(file(&d).with_extension(format!("tmp{}", std::process::id())), "junk").unwrap();
        assert_eq!(assign(&d, &["x", "late"]).warning, None);
        assert_eq!(lookup(&d, &["late"]).get("late"), Some(&1));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn two_commands_at_once_never_share_a_number() {
        let d = tmp("race");
        std::fs::create_dir_all(&d).unwrap();
        let handles: Vec<_> = (0..8)
            .map(|i| {
                let d = d.clone();
                std::thread::spawn(move || {
                    let name = format!("app{i}");
                    assign(&d, &[name.as_str()]).get(&name).unwrap()
                })
            })
            .collect();
        let mut ids: Vec<u32> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        ids.sort_unstable();
        assert_eq!(ids, (0..8).collect::<Vec<_>>());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn compact_writes_ranges_warden_start_takes() {
        assert_eq!(compact(&[]), "");
        assert_eq!(compact(&[2]), "2");
        assert_eq!(compact(&[0, 1]), "0,1");
        assert_eq!(compact(&[0, 1, 2]), "0-2");
        assert_eq!(compact(&[5, 1, 2, 3, 3, 9, 7, 8]), "1-3,5,7-9");
    }
}

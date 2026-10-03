//! Where compressed copies are kept: a private directory with a folder for
//! each source path and, in it, a file for each version of the source and
//! each encoding:
//!
//! ```text
//! <dir>/<hash of the path>/<version>.br
//!                          <version>.gz
//! ```
//!
//! The version is the source file's device, inode, size and modification and
//! change times, written out in the name. A request opens the file it is
//! about to send anyway, which gives it that version, so finding the copy of
//! the version in hand is the whole check that the copy is current: a file
//! that was edited, replaced or touched has another name, and nothing ever
//! serves the copy of an older one. Old copies are removed when the new one
//! is made, and the oldest go first when the directory outgrows its room.

use std::collections::HashSet;
use std::fs::Metadata;
use std::hash::Hash;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

use super::text::hex;

/// One version of a file: what a compressed copy belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub(super) struct Version {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Version {
    pub(super) fn of(m: &Metadata) -> Version {
        Version {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    pub(super) fn size(&self) -> u64 {
        self.size
    }

    /// A version no file has, long still: for tests of what is done with versions.
    #[cfg(test)]
    pub(super) fn fake(n: u64) -> Version {
        Version { dev: 1, ino: n, size: 5000, mtime: (1_700_000_000, 0), ctime: (1_700_000_000, 0) }
    }

    /// The modification time, which a copy is given too.
    pub(super) fn modified(&self) -> Option<SystemTime> {
        let secs = u64::try_from(self.mtime.0).ok()?;
        Some(SystemTime::UNIX_EPOCH + std::time::Duration::new(secs, u32::try_from(self.mtime.1).ok()?))
    }

    /// Not changed in the last two seconds. A file written to within the
    /// clock's granularity can change without its times doing so, and a file
    /// being uploaded is not worth compressing before it is complete: both
    /// wait until it has been still for a while.
    pub(super) fn quiet(&self, now: SystemTime) -> bool {
        const QUIET: i64 = 2;
        let Ok(now) = now.duration_since(SystemTime::UNIX_EPOCH) else { return false };
        self.mtime.0.max(self.ctime.0) + QUIET <= now.as_secs() as i64
    }
}

/// The path of a copy inside the store, `<hash of the path>/<version>.<suffix>`,
/// built on the stack: a request that finds its copy allocates nothing for it.
pub(super) struct Entry {
    buf: [u8; 160],
    len: usize,
    /// Where the directory part ends.
    dir_len: usize,
    /// Where the suffix starts.
    suffix_at: usize,
}

impl Entry {
    pub(super) fn new(rel: &str, v: &Version, suffix: &str) -> Entry {
        let mut e = Entry { buf: [0; 160], len: 0, dir_len: 0, suffix_at: 0 };
        // FNV-1a: the same in every build, so a new Warden finds the folders of an old one.
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in rel.bytes() {
            h = (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3);
        }
        e.push(hex(h, &mut [0; 16]));
        e.dir_len = e.len;
        e.push(b"/");
        for (i, n) in [v.dev, v.ino, v.size].into_iter().enumerate() {
            let sep: &[u8] = if i == 0 { b"" } else { b"-" };
            e.push(sep);
            e.push(hex(n, &mut [0; 16]));
        }
        for (t, ns) in [v.mtime, v.ctime] {
            e.push(b"-");
            e.push(hex(t as u64, &mut [0; 16]));
            e.push(b".");
            e.push(hex(ns as u64, &mut [0; 16]));
        }
        e.push(b".");
        e.suffix_at = e.len;
        e.push(suffix.as_bytes());
        e
    }

    fn push(&mut self, b: &[u8]) {
        // The longest name made here is 122 bytes; a longer one is cut, not overrun.
        let n = b.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&b[..n]);
        self.len += n;
    }

    /// The whole path.
    pub(super) fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }

    /// The folder of the source path.
    pub(super) fn dir(&self) -> &str {
        std::str::from_utf8(&self.buf[..self.dir_len]).unwrap_or("")
    }

    /// `<version>.`: what every copy of this version starts with.
    pub(super) fn version_prefix(&self) -> &str {
        std::str::from_utf8(&self.buf[self.dir_len + 1..self.suffix_at]).unwrap_or("")
    }
}

/// A private directory of compressed copies.
pub(super) struct Store {
    /// The directory, for opening copies relative to it.
    pub(super) dir: Arc<OwnedFd>,
    pub(super) path: PathBuf,
    /// The room for copies, about: workers sharing the directory each count
    /// their own, and a pass over the directory finds the truth.
    cap: u64,
    /// Bytes of copies, as far as this worker knows.
    used: AtomicU64,
    /// Bytes this worker has had made since it last looked at the directory.
    added: AtomicU64,
}

/// A folder in the store that has not been touched for this long and holds
/// a half-made copy: its maker is gone.
const STALE_TMP_SECS: u64 = 3600;

impl Store {
    /// Open `path`, making it (mode 0700) when it is not there. It must be a
    /// directory, not a link, owned by this user, closed to everyone else, and
    /// outside `root` (which is served).
    pub(super) fn open(path: &Path, root: &Path, cap: u64) -> Result<Store, String> {
        let shown = path.display();
        // Before anything is made: a folder that is named inside the site is not made there.
        if std::path::absolute(path).is_ok_and(|p| p.starts_with(root)) {
            return Err(format!("{shown} is inside static.root: it would be served"));
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .map_err(|e| format!("cannot make {shown}: {e}"))?;
        // The check is made on the open directory, so nothing can be swapped in
        // between the check and the use.
        let dir = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| format!("cannot open {shown} (a link is not accepted): {e}"))?;
        let meta = dir.metadata().map_err(|e| format!("cannot look at {shown}: {e}"))?;
        if meta.uid() != crate::sys::euid() {
            return Err(format!("{shown} belongs to another user"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(format!("{shown} is open to other users (mode {:o}); use chmod 700", meta.mode() & 0o777));
        }
        let canonical = std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {shown}: {e}"))?;
        if canonical.starts_with(root) {
            return Err(format!("{shown} is inside static.root: it would be served"));
        }
        Ok(Store {
            dir: Arc::new(OwnedFd::from(dir)),
            path: canonical,
            cap,
            used: AtomicU64::new(0),
            added: AtomicU64::new(0),
        })
    }

    /// The copy at `entry`, as a path (for the work that is not done relative to `dir`).
    pub(super) fn path_of(&self, entry: &Entry) -> PathBuf {
        self.path.join(entry.as_str())
    }

    /// The folder of `entry`'s source path, made if it is not there.
    pub(super) fn folder(&self, entry: &Entry) -> std::io::Result<PathBuf> {
        let folder = self.path.join(entry.dir());
        match std::fs::DirBuilder::new().mode(0o700).create(&folder) {
            Ok(()) => Ok(folder),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(folder),
            Err(e) => Err(e),
        }
    }

    /// Remove the other versions in `entry`'s folder (and what is left of
    /// earlier attempts), now that the copies of `entry`'s version are in
    /// place. Returns the bytes freed.
    pub(super) fn prune(&self, entry: &Entry) -> u64 {
        let folder = self.path.join(entry.dir());
        let Ok(list) = std::fs::read_dir(&folder) else { return 0 };
        let keep = entry.version_prefix();
        let mut freed = 0;
        for item in list.flatten() {
            let name = item.file_name();
            let name = name.to_string_lossy();
            // A copy being made by someone else is left alone.
            if name.starts_with(keep) || name.ends_with(".tmp") {
                continue;
            }
            let size = item.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(item.path()).is_ok() {
                freed += size;
            }
        }
        freed
    }

    /// `wrote` bytes of copies were made and `freed` removed (by a job, which
    /// says so); true when a pass over the directory is due.
    pub(super) fn account(&self, wrote: u64, freed: u64) -> bool {
        let _ =
            self.used.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |u| Some((u + wrote).saturating_sub(freed)));
        let added = self.added.fetch_add(wrote, Ordering::Relaxed) + wrote;
        self.used.load(Ordering::Relaxed) > self.cap || added > self.cap / 8
    }

    /// A pass over the directory found `total` bytes of copies.
    pub(super) fn recount(&self, total: u64) {
        self.used.store(total, Ordering::Relaxed);
        self.added.store(0, Ordering::Relaxed);
    }

    /// Bytes of copies, as far as this worker knows.
    pub(super) fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    /// Look at the directory: count what is in it, and while it holds more than
    /// its room, remove the copies that were made first (down to nine tenths,
    /// so the next pass is not due at once). Half-made copies of a worker that
    /// is gone, and folders left empty, go too. Returns the bytes of copies
    /// left. Not for the request path.
    pub(super) fn sweep(&self) -> u64 {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut copies: Vec<((i64, i64), u64, PathBuf)> = Vec::new();
        let mut total = 0u64;
        let mut folders = HashSet::new();
        if let Ok(top) = std::fs::read_dir(&self.path) {
            for folder in top.flatten().filter(|f| f.file_type().is_ok_and(|t| t.is_dir())) {
                folders.insert(folder.path());
                let Ok(inside) = std::fs::read_dir(folder.path()) else { continue };
                for item in inside.flatten() {
                    let Ok(m) = item.metadata() else { continue };
                    let name = item.file_name();
                    let name = name.to_string_lossy();
                    if name.ends_with(".tmp") {
                        if now.saturating_sub(m.mtime().max(0) as u64) > STALE_TMP_SECS {
                            let _ = std::fs::remove_file(item.path());
                        }
                    } else if m.is_file() {
                        total += m.len();
                        copies.push(((m.ctime(), m.ctime_nsec()), m.len(), item.path()));
                    }
                }
            }
        }
        if total > self.cap {
            copies.sort_unstable_by_key(|c| c.0);
            let target = self.cap / 10 * 9;
            for (_, size, path) in copies {
                if total <= target {
                    break;
                }
                if std::fs::remove_file(path).is_ok() {
                    total -= size;
                }
            }
        }
        for folder in folders {
            let _ = std::fs::remove_dir(folder); // only when empty
        }
        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("warden-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn version(n: u64) -> Version {
        Version { dev: 1, ino: n, size: 1000, mtime: (1_700_000_000, 5), ctime: (1_700_000_000, 6) }
    }

    #[test]
    fn a_name_says_the_version_and_only_the_version() {
        let v = version(7);
        let e = Entry::new("assets/app.js", &v, "br");
        assert_eq!(e.version_prefix(), "1-7-3e8-6553f100.5-6553f100.6.");
        assert_eq!(e.as_str(), format!("{}/{}br", e.dir(), e.version_prefix()));
        assert!((1..=16).contains(&e.dir().len()), "{}", e.dir());
        // Another encoding of the same version is a sibling; another version, another name.
        let gz = Entry::new("assets/app.js", &v, "gz");
        assert_eq!((gz.dir(), gz.version_prefix()), (e.dir(), e.version_prefix()));
        for other in [
            Version { ino: 8, ..v },
            Version { size: 1001, ..v },
            Version { mtime: (v.mtime.0, 6), ..v },
            Version { ctime: (v.ctime.0 + 1, 6), ..v },
            Version { dev: 2, ..v },
        ] {
            assert_ne!(Entry::new("assets/app.js", &other, "br").as_str(), e.as_str());
            assert_eq!(Entry::new("assets/app.js", &other, "br").dir(), e.dir(), "one folder per path");
        }
        assert_ne!(Entry::new("assets/other.js", &v, "br").dir(), e.dir());
        // Extreme values still fit.
        let big = Version {
            dev: u64::MAX,
            ino: u64::MAX,
            size: u64::MAX,
            mtime: (i64::MIN, 999_999_999),
            ctime: (i64::MIN, 999_999_999),
        };
        assert!(Entry::new("x", &big, "br").as_str().ends_with(".br"));
    }

    #[test]
    fn a_file_is_quiet_after_two_seconds() {
        let v = Version { mtime: (1000, 0), ctime: (999, 0), ..version(1) };
        let at = |s: u64| SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s);
        assert!(!v.quiet(at(1000)) && !v.quiet(at(1001)));
        assert!(v.quiet(at(1002)));
        // The later of the two times counts.
        let v = Version { mtime: (900, 0), ctime: (1000, 0), ..v };
        assert!(!v.quiet(at(1001)) && v.quiet(at(1002)));
        assert!(!v.quiet(SystemTime::UNIX_EPOCH), "a clock before the epoch is no evidence of quiet");
    }

    #[test]
    fn the_directory_must_be_private_and_ours_and_outside_the_site() {
        let base = tmp("open");
        let root = base.join("site");
        std::fs::create_dir_all(&root).unwrap();
        // Made when missing, with parents, closed to others.
        let store = Store::open(&base.join("a/b/store"), &root, 1 << 20).unwrap();
        assert_eq!(std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777, 0o700);
        // Open again: fine. Opened to others: refused.
        assert!(Store::open(&base.join("a/b/store"), &root, 1 << 20).is_ok());
        std::fs::set_permissions(base.join("a/b/store"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let e = Store::open(&base.join("a/b/store"), &root, 1 << 20).err().unwrap();
        assert!(e.contains("open to other users"), "{e}");
        // A link is refused, even to a good directory.
        std::os::unix::fs::symlink(base.join("a"), base.join("link")).unwrap();
        let e = Store::open(&base.join("link"), &root, 1 << 20).err().unwrap();
        assert!(e.contains("link"), "{e}");
        // Inside the served folder: refused.
        let e = Store::open(&root.join(".cache"), &root, 1 << 20).err().unwrap();
        assert!(e.contains("inside static.root"), "{e}");
        assert!(!root.join(".cache").exists(), "and not made there");
        // The same through a link that leads into it.
        std::os::unix::fs::symlink(&root, base.join("into")).unwrap();
        let e = Store::open(&base.join("into"), &root, 1 << 20).err().unwrap();
        assert!(e.contains("link"), "{e}");
        let _ = std::fs::remove_dir_all(&base);
    }

    fn put(store: &Store, rel: &str, v: &Version, suffix: &str, len: usize) -> Entry {
        let e = Entry::new(rel, v, suffix);
        store.folder(&e).unwrap();
        std::fs::write(store.path_of(&e), vec![b'x'; len]).unwrap();
        store.account(len as u64, 0);
        e
    }

    #[test]
    fn a_new_version_replaces_the_old_ones_and_nothing_else() {
        let base = tmp("prune");
        let store = Store::open(&base.join("s"), &base.join("site"), 1 << 20).unwrap();
        let (v1, v2) = (version(1), version(2));
        let old_br = put(&store, "a.js", &v1, "br", 100);
        let old_gz = put(&store, "a.js", &v1, "gz", 120);
        let other = put(&store, "b.js", &v1, "br", 50);
        let new_br = put(&store, "a.js", &v2, "br", 90);
        let new_gz = put(&store, "a.js", &v2, "gz", 110);
        // A copy being made (by another worker) is not touched.
        let making = store
            .path
            .join(new_br.dir())
            .join(format!("{}.gz.tmp", Entry::new("a.js", &version(3), "").version_prefix()));
        std::fs::write(&making, b"partial").unwrap();
        assert_eq!(store.used(), 470);
        let freed = store.prune(&new_br);
        let exists = |e: &Entry| store.path_of(e).exists();
        assert!(!exists(&old_br) && !exists(&old_gz));
        assert!(exists(&new_br) && exists(&new_gz) && exists(&other) && making.exists());
        assert_eq!(freed, 100 + 120);
        // The job says what it freed; the count follows.
        store.account(0, freed);
        assert_eq!(store.used(), 470 - 100 - 120);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_sweep_counts_and_removes_the_oldest_first_when_over() {
        let base = tmp("sweep");
        let store = Store::open(&base.join("s"), &base.join("site"), 1000).unwrap();
        let mut entries = Vec::new();
        for n in 0..6u64 {
            entries.push(put(&store, &format!("f{n}.js"), &version(n), "br", 200));
            // ctime orders them; make sure each is a tick apart.
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
        assert!(store.account(0, 0), "1200 bytes in a store of 1000 is over");
        assert_eq!(store.sweep(), 800);
        store.recount(800);
        // 6 x 200 = 1200, down to nine tenths of 1000: the two oldest go, which leaves 800.
        let alive: Vec<bool> = entries.iter().map(|e| store.path_of(e).exists()).collect();
        assert_eq!(alive, [false, false, true, true, true, true], "{alive:?}");
        assert_eq!(store.used(), 800);
        // A folder with nothing left is removed; one that was made too long ago to be in use is cleaned of its tmp.
        assert!(!store.path.join(entries[0].dir()).exists());
        let stale = store.path.join(entries[2].dir()).join("x.br.tmp");
        std::fs::write(&stale, b"partial").unwrap();
        let long_ago = SystemTime::now() - std::time::Duration::from_secs(2 * STALE_TMP_SECS);
        std::fs::File::options().write(true).open(&stale).unwrap().set_modified(long_ago).unwrap();
        assert_eq!(store.sweep(), 800);
        assert!(!stale.exists());
        let _ = std::fs::remove_dir_all(&base);
    }
}

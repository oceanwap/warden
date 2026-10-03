//! In-memory cache of small hot files for `warden serve`: one complete,
//! prebuilt response per file and variant (status line, headers, body), so
//! a keep-alive GET hit is a single send(2) with no open, fstat or read; a
//! body of MEMFD_MIN or more lives in a sealed memfd instead, and the hit is
//! a single sendfile(2), which hands the kernel the pages without copying.
//!
//! Bounded per worker by `[static] cache_size` (LRU eviction; every byte an
//! entry holds is counted: response, 304 head, key, validators and the
//! bookkeeping around them) and `cache_max_file`.
//!
//! Validity is a simple TTL: an entry is re-checked against the disk at
//! most once per `cache_valid_ms`, by re-opening every path its response
//! depended on through the server's normal open path (so the same root
//! confinement applies) and comparing device, inode, size, mtime and ctime.
//! Anything changed, deleted or swapped (a symlink now pointing outside the
//! root) drops the entry and the request takes the normal path. inotify was
//! the alternative and was not chosen: watches are per directory and
//! limited (`max_user_watches`, per worker), renames and atomic replaces
//! need directory watches and careful event pairing, an overflowed queue
//! means flushing everything, and network or overlay filesystems don't
//! deliver events for changes made elsewhere. The TTL bounds staleness on
//! every filesystem with no new failure mode; the cost is a few syscalls
//! per hot file per second.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Identity and version of a file: if any of these differ, the cached
/// response is stale. ctime matters: `rsync --inplace` (and anything that
/// restores an old mtime) changes the content and ctime, not the mtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

impl Stamp {
    pub fn of(m: &std::fs::Metadata) -> Stamp {
        use std::os::unix::fs::MetadataExt;
        Stamp {
            dev: m.dev(),
            ino: m.ino(),
            size: m.size(),
            mtime: (m.mtime(), m.mtime_nsec()),
            ctime: (m.ctime(), m.ctime_nsec()),
        }
    }

    /// Changed so recently that a write in the same timestamp tick could go
    /// unnoticed (the "racy" case git also guards against): such a file is
    /// served, not cached, until it has been quiet for a while.
    pub fn racy(&self, now: std::time::SystemTime) -> bool {
        const QUIET: i64 = 2;
        let Ok(now) = now.duration_since(std::time::UNIX_EPOCH) else { return true };
        let latest = self.mtime.0.max(self.ctime.0);
        latest + QUIET > now.as_secs() as i64
    }
}

/// What a lookup on the way to a response found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seen {
    /// A regular file, this version.
    File(Stamp),
    /// A directory (its index file is a dependency of its own).
    Dir,
    /// Not a regular file: missing, refused, a directory, a FIFO...
    /// (a precompressed sibling that was not there).
    NotFile,
}

impl Seen {
    /// Does what a lookup finds now still match?
    pub fn matches(&self, now: Option<&std::fs::Metadata>) -> bool {
        match self {
            Seen::File(s) => now.is_some_and(|m| m.is_file() && Stamp::of(m) == *s),
            Seen::Dir => now.is_some_and(|m| m.is_dir()),
            Seen::NotFile => !now.is_some_and(|m| m.is_file()),
        }
    }
}

/// One path (relative to the root) the response depended on.
#[derive(Debug, Clone)]
pub struct Dep {
    pub path: Box<str>,
    pub seen: Seen,
}

/// Responses with a body at least this big are kept in a sealed memfd and
/// sent with sendfile(2): the kernel takes the pages by reference instead of
/// copying them into the socket, which is what a send(2) from memory costs
/// (measured: docs/benchmarks.md, "memfd"). Smaller ones stay in memory,
/// where one send(2) is cheaper than sendfile's page handling.
pub const MEMFD_MIN: u64 = 8 * 1024;

/// A cached response (head and body) in a sealed memfd, counted against
/// the cache's limit on open memfds while it lives.
#[derive(Debug)]
pub struct MemFile {
    pub file: std::fs::File,
    open: Arc<AtomicUsize>,
}

impl Drop for MemFile {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A complete response for one file and variant, as the server sends it
/// with `Connection: keep-alive`; the other forms are derived from it.
#[derive(Debug)]
pub struct Entry {
    /// Head then body: a keep-alive GET is exactly these bytes. With
    /// `file`, only the head (the whole response is in the file).
    pub resp: Arc<[u8]>,
    /// The whole response (head then body) in a memfd, for bodies of at
    /// least MEMFD_MIN: a keep-alive GET is one sendfile(2) of it.
    pub file: Option<Arc<MemFile>>,
    pub head_len: usize,
    /// Where the Connection header's value ("keep-alive") starts in `resp`.
    pub conn_at: usize,
    /// The 304 head (keep-alive), and where its Connection value starts.
    pub not_modified: Arc<[u8]>,
    pub nm_conn_at: usize,
    pub etag: Box<str>,
    /// Last-Modified in seconds (If-Modified-Since compares against it).
    pub mtime: u64,
    pub body_len: u64,
    pub deps: Vec<Dep>,
}

pub const KEEP_ALIVE: &[u8] = b"keep-alive";

impl Entry {
    /// Bytes this entry holds, honestly: the buffers, the strings, and the
    /// fixed-size parts of the entry, its LRU node and its map slot.
    fn cost(&self, key: &str) -> usize {
        const ARC_HEADER: usize = 2 * std::mem::size_of::<usize>();
        let deps: usize = self.deps.iter().map(|d| d.path.len() + std::mem::size_of::<Dep>()).sum();
        // A memfd's pages are memory like the heap's.
        let in_file = if self.file.is_some() { self.head_len + self.body_len as usize } else { 0 };
        self.resp.len()
            + in_file
            + self.not_modified.len()
            + 3 * ARC_HEADER
            + self.etag.len()
            + deps
            + std::mem::size_of::<Entry>()
            + 2 * key.len() // in the map and in the LRU node
            + std::mem::size_of::<Node<String, Slot>>()
            // a hash map slot (key + index), at the table's 7/8 load factor
            + (std::mem::size_of::<(String, usize)>() + 1) * 8 / 7
    }
}

// ----------------------------------------------------------------- the LRU

const NIL: usize = usize::MAX;

struct Node<K, V> {
    key: K,
    val: Option<V>,
    cost: usize,
    prev: usize,
    next: usize,
}

/// A least-recently-used map bounded by the total cost of its values.
/// O(1) get, insert and eviction; nodes live in a slab (freed slots are
/// reused), linked most recent first.
pub struct Lru<K, V> {
    map: HashMap<K, usize>,
    nodes: Vec<Node<K, V>>,
    free: Vec<usize>,
    head: usize,
    tail: usize,
    used: usize,
    cap: usize,
}

impl<K: std::hash::Hash + Eq + Clone + Default, V> Lru<K, V> {
    pub fn new(cap: usize) -> Self {
        Lru { map: HashMap::new(), nodes: Vec::new(), free: Vec::new(), head: NIL, tail: NIL, used: 0, cap }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn used(&self) -> usize {
        self.used
    }

    fn unlink(&mut self, i: usize) {
        let (prev, next) = (self.nodes[i].prev, self.nodes[i].next);
        if prev == NIL {
            self.head = next
        } else {
            self.nodes[prev].next = next
        }
        if next == NIL { self.tail = prev } else { self.nodes[next].prev = prev }
    }

    fn push_front(&mut self, i: usize) {
        self.nodes[i].prev = NIL;
        self.nodes[i].next = self.head;
        if self.head != NIL {
            self.nodes[self.head].prev = i;
        }
        self.head = i;
        if self.tail == NIL {
            self.tail = i;
        }
    }

    /// The value for `key`, now the most recently used.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let i = *self.map.get(key)?;
        if self.head != i {
            self.unlink(i);
            self.push_front(i);
        }
        self.nodes[i].val.as_mut()
    }

    pub fn remove<Q>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
        Q: std::hash::Hash + Eq + ?Sized,
    {
        let i = self.map.remove(key)?;
        self.release(i)
    }

    /// Unlink node `i` (already out of the map) and free its slot.
    fn release(&mut self, i: usize) -> Option<V> {
        self.unlink(i);
        self.used -= self.nodes[i].cost;
        self.nodes[i].cost = 0;
        self.nodes[i].key = K::default();
        self.free.push(i);
        self.nodes[i].val.take()
    }

    /// Insert (or replace) `key`, evicting least recently used entries
    /// until it fits. Returns how many were evicted, or None (nothing
    /// changed) when `cost` alone is over the budget.
    pub fn insert(&mut self, key: K, val: V, cost: usize) -> Option<usize> {
        if cost > self.cap {
            return None;
        }
        self.remove(&key);
        let mut evicted = 0;
        while self.used + cost > self.cap && self.tail != NIL {
            let t = self.tail;
            let k = self.nodes[t].key.clone();
            self.map.remove(&k);
            self.release(t);
            evicted += 1;
        }
        let node = Node { key: key.clone(), val: Some(val), cost, prev: NIL, next: NIL };
        let i = match self.free.pop() {
            Some(i) => {
                self.nodes[i] = node;
                i
            }
            None => {
                self.nodes.push(node);
                self.nodes.len() - 1
            }
        };
        self.map.insert(key, i);
        self.push_front(i);
        self.used += cost;
        Some(evicted)
    }

    /// Keys from most to least recently used (tests and diagnostics).
    #[cfg(test)]
    pub fn keys(&self) -> Vec<K> {
        let mut out = Vec::new();
        let mut i = self.head;
        while i != NIL {
            out.push(self.nodes[i].key.clone());
            i = self.nodes[i].next;
        }
        out
    }
}

// --------------------------------------------------------------- the cache

struct Slot {
    entry: Arc<Entry>,
    /// When the entry was last known to match the disk.
    checked: Instant,
}

pub enum Lookup {
    /// Checked within `cache_valid_ms`: serve it.
    Fresh(Arc<Entry>),
    /// Due for a check: serve it only if its dependencies still match.
    Stale(Arc<Entry>),
    Miss,
}

/// The per-worker cache. Its lock is never held across an await.
pub struct Cache {
    lru: Mutex<Lru<String, Slot>>,
    pub max_file: u64,
    pub valid: Duration,
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    /// Entries dropped because the file changed (or went away).
    pub stale: AtomicU64,
    pub evicted: AtomicU64,
    /// Memfds alive (cached or still being sent), and how many may be:
    /// each is a descriptor, and connections need theirs.
    memfds: Arc<AtomicUsize>,
    max_memfds: usize,
}

impl Cache {
    /// `nofile`: the soft limit on open descriptors (memfds take at most an
    /// eighth of it).
    pub fn new(size: u64, max_file: u64, valid_ms: u64, nofile: u64) -> Option<Cache> {
        if size == 0 {
            return None;
        }
        let max_memfds = (size / MEMFD_MIN).min(nofile / 8).min(4096) as usize;
        Some(Cache {
            lru: Mutex::new(Lru::new(usize::try_from(size).unwrap_or(usize::MAX))),
            // A file the whole budget can't hold is never read in to try.
            max_file: max_file.min(size),
            valid: Duration::from_millis(valid_ms),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale: AtomicU64::new(0),
            evicted: AtomicU64::new(0),
            memfds: Arc::new(AtomicUsize::new(0)),
            max_memfds,
        })
    }

    /// `resp` (a whole response) in a sealed memfd, if its body is big
    /// enough to be worth it and fewer than the limit are open. None: keep
    /// it in memory (also where there is no memfd, or creating one fails:
    /// out of descriptors or memory, which the next accept reports).
    pub fn memfd(&self, resp: &[u8], body_len: u64) -> Option<MemFile> {
        if body_len < MEMFD_MIN {
            return None;
        }
        // Take a slot; give it back if over the limit or creation fails.
        if self.memfds.fetch_add(1, Ordering::Relaxed) >= self.max_memfds {
            self.memfds.fetch_sub(1, Ordering::Relaxed);
            return None;
        }
        match crate::sys::sealed_memfd(c"warden-static", resp) {
            Ok(file) => Some(MemFile { file, open: self.memfds.clone() }),
            Err(_) => {
                self.memfds.fetch_sub(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Memfds open now.
    pub fn memfds(&self) -> usize {
        self.memfds.load(Ordering::Relaxed)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Lru<String, Slot>> {
        // Nothing panics while holding it; a poisoned lock still has a
        // consistent LRU (every mutation completes before unlocking).
        self.lru.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn lookup(&self, key: &str, now: Instant) -> Lookup {
        let mut lru = self.lock();
        match lru.get_mut(key) {
            Some(s) if now.saturating_duration_since(s.checked) < self.valid => Lookup::Fresh(s.entry.clone()),
            Some(s) => Lookup::Stale(s.entry.clone()),
            None => Lookup::Miss,
        }
    }

    /// `entry` (still the one cached under `key`) matched the disk at `now`.
    pub fn confirm(&self, key: &str, entry: &Arc<Entry>, now: Instant) {
        let mut lru = self.lock();
        if let Some(s) = lru.get_mut(key) {
            if Arc::ptr_eq(&s.entry, entry) {
                s.checked = now;
            }
        }
    }

    /// Drop `entry` (if it is still the one under `key`): its file changed.
    pub fn invalidate(&self, key: &str, entry: &Arc<Entry>) {
        let mut lru = self.lock();
        let same = lru.get_mut(key).is_some_and(|s| Arc::ptr_eq(&s.entry, entry));
        if same {
            lru.remove(key);
            self.stale.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn insert(&self, key: String, entry: Arc<Entry>, now: Instant) {
        let cost = entry.cost(&key);
        let mut lru = self.lock();
        if let Some(n) = lru.insert(key, Slot { entry, checked: now }, cost) {
            self.evicted.fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// (entries, bytes used, budget)
    pub fn usage(&self) -> (usize, usize, usize) {
        let lru = self.lock();
        (lru.len(), lru.used(), lru.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_orders_by_use_and_evicts_the_oldest() {
        let mut l: Lru<String, u32> = Lru::new(100);
        assert_eq!(l.insert("a".into(), 1, 40), Some(0));
        assert_eq!(l.insert("b".into(), 2, 40), Some(0));
        assert_eq!(l.keys(), ["b", "a"]);
        // Using `a` makes `b` the oldest.
        assert_eq!(l.get_mut("a").copied(), Some(1));
        assert_eq!(l.keys(), ["a", "b"]);
        assert_eq!(l.insert("c".into(), 3, 40), Some(1), "b goes");
        assert_eq!(l.keys(), ["c", "a"]);
        assert!(l.get_mut("b").is_none());
        assert_eq!((l.len(), l.used()), (2, 80));
        // Replacing a key frees its old cost first.
        assert_eq!(l.insert("a".into(), 10, 60), Some(0));
        assert_eq!((l.len(), l.used()), (2, 100));
        assert_eq!(l.keys(), ["a", "c"]);
        // One value bigger than the whole budget is refused, nothing evicted.
        assert_eq!(l.insert("huge".into(), 0, 101), None);
        assert_eq!((l.len(), l.used()), (2, 100));
        // Exactly the budget evicts everything else.
        assert_eq!(l.insert("all".into(), 4, 100), Some(2));
        assert_eq!((l.keys(), l.used()), (vec!["all".to_string()], 100));
        assert_eq!(l.remove("all"), Some(4));
        assert_eq!((l.len(), l.used(), l.keys()), (0, 0, Vec::<String>::new()));
        assert_eq!(l.remove("all"), None);
    }

    #[test]
    fn lru_budget_holds_under_random_use() {
        // Model check against a plain list: same order, same contents, and
        // the budget is never exceeded.
        let mut l: Lru<u32, u32> = Lru::new(1000);
        let mut model: Vec<(u32, u32)> = Vec::new(); // (key, cost), most recent first
        let mut seed = 12345u64;
        let mut rnd = |n: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 33) % n
        };
        for _ in 0..20_000 {
            let k = rnd(40) as u32;
            match rnd(3) {
                0 => {
                    let cost = rnd(300) as u32 + 1;
                    l.insert(k, k, cost as usize);
                    model.retain(|(mk, _)| *mk != k);
                    model.insert(0, (k, cost));
                    while model.iter().map(|(_, c)| *c).sum::<u32>() > 1000 {
                        model.pop();
                    }
                }
                1 => {
                    let got = l.get_mut(&k).copied();
                    let pos = model.iter().position(|(mk, _)| *mk == k);
                    assert_eq!(got.is_some(), pos.is_some());
                    if let Some(p) = pos {
                        let e = model.remove(p);
                        model.insert(0, e);
                    }
                }
                _ => {
                    assert_eq!(l.remove(&k).is_some(), model.iter().any(|(mk, _)| *mk == k));
                    model.retain(|(mk, _)| *mk != k);
                }
            }
            assert!(l.used() <= 1000);
            assert_eq!(l.used() as u32, model.iter().map(|(_, c)| *c).sum::<u32>());
            assert_eq!(l.keys(), model.iter().map(|(k, _)| *k).collect::<Vec<_>>());
        }
        // Slots are reused: the slab never outgrows the distinct keys.
        assert!(l.nodes.len() <= 40, "{}", l.nodes.len());
    }

    fn entry(body: usize) -> Arc<Entry> {
        Arc::new(Entry {
            resp: vec![b'x'; 100 + body].into(),
            file: None,
            head_len: 100,
            conn_at: 50,
            not_modified: vec![b'y'; 80].into(),
            nm_conn_at: 40,
            etag: "W/\"1-2\"".into(),
            mtime: 2,
            body_len: body as u64,
            deps: vec![Dep { path: "a.css".into(), seen: Seen::Dir }],
        })
    }

    #[test]
    fn cache_counts_every_byte_and_stays_in_budget() {
        let e = entry(1000);
        let cost = e.cost("a.css\0" /* key */);
        // At least the buffers, the strings and the key twice.
        assert!(cost >= 1100 + 80 + 7 + 5 + 2 * 6, "{cost}");
        assert!(cost < 1100 + 80 + 1024, "overhead stays modest: {cost}");
        let c = Cache::new((cost * 3) as u64, 1 << 20, 1000, 1024).unwrap();
        let now = Instant::now();
        for i in 0..10 {
            c.insert(format!("f{i}.css"), entry(1000), now);
            let (n, used, cap) = c.usage();
            assert!(used <= cap && n <= 3, "{n} entries, {used} of {cap}");
        }
        assert!(c.evicted.load(Ordering::Relaxed) >= 7);
        assert!(matches!(c.lookup("f9.css", now), Lookup::Fresh(_)));
        assert!(matches!(c.lookup("f0.css", now), Lookup::Miss));
        assert!(Cache::new(0, 1, 1, 1024).is_none(), "cache_size = 0 turns it off");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn big_bodies_go_into_memfds_up_to_the_limit() {
        use std::os::unix::fs::FileExt;
        // A budget for 4 memfd bodies; a descriptor limit allowing 2.
        let c = Cache::new(4 * MEMFD_MIN, 1 << 20, 1000, 16).unwrap();
        let resp: Vec<u8> = (0..(MEMFD_MIN + 300) as u32).map(|i| (i % 251) as u8).collect();
        let body = MEMFD_MIN;
        assert!(c.memfd(&resp, body - 1).is_none(), "smaller bodies stay in memory");
        let a = c.memfd(&resp, body).expect("a memfd");
        let mut back = vec![0u8; resp.len()];
        a.file.read_exact_at(&mut back, 0).unwrap();
        assert!(back == resp, "the exact response");
        let b = c.memfd(&resp, body).expect("a second");
        assert_eq!(c.memfds(), 2);
        assert!(c.memfd(&resp, body).is_none(), "the third is over nofile / 8");
        assert_eq!(c.memfds(), 2, "a refused one takes no slot");
        drop(a);
        assert_eq!(c.memfds(), 1, "dropping one gives its slot back");
        let d = c.memfd(&resp, body).expect("room again");
        // The cost counts the memfd's bytes like memory.
        let head = 300;
        let e = Entry {
            resp: resp[..head].into(),
            file: Some(Arc::new(d)),
            head_len: head,
            conn_at: 10,
            not_modified: vec![b'y'; 80].into(),
            nm_conn_at: 40,
            etag: "W/\"1-2\"".into(),
            mtime: 2,
            body_len: body,
            deps: Vec::new(),
        };
        assert!(e.cost("k") >= head + resp.len() + 80, "{}", e.cost("k"));
        drop((b, e));
        assert_eq!(c.memfds(), 0);
    }

    #[test]
    fn entries_go_stale_after_the_validity_window() {
        let c = Cache::new(1 << 20, 1 << 20, 1000, 1024).unwrap();
        let t0 = Instant::now();
        let e = entry(10);
        c.insert("k".into(), e.clone(), t0);
        assert!(matches!(c.lookup("k", t0 + Duration::from_millis(999)), Lookup::Fresh(_)));
        let Lookup::Stale(s) = c.lookup("k", t0 + Duration::from_millis(1000)) else { panic!("due for a check") };
        assert!(Arc::ptr_eq(&s, &e));
        // Confirmed: fresh for another window.
        c.confirm("k", &e, t0 + Duration::from_millis(1500));
        assert!(matches!(c.lookup("k", t0 + Duration::from_millis(2400)), Lookup::Fresh(_)));
        // A confirmation or invalidation for an entry that was replaced
        // meanwhile leaves the new one alone.
        let newer = entry(20);
        c.insert("k".into(), newer.clone(), t0);
        c.confirm("k", &e, t0 + Duration::from_millis(5000));
        assert!(matches!(c.lookup("k", t0 + Duration::from_millis(5000)), Lookup::Stale(_)));
        c.invalidate("k", &e);
        assert!(matches!(c.lookup("k", t0), Lookup::Fresh(_)));
        c.invalidate("k", &newer);
        assert!(matches!(c.lookup("k", t0), Lookup::Miss));
        assert_eq!(c.stale.load(Ordering::Relaxed), 1);
        // cache_valid_ms = 0: every use is checked.
        let c = Cache::new(1 << 20, 1 << 20, 0, 1024).unwrap();
        c.insert("k".into(), entry(1), t0);
        assert!(matches!(c.lookup("k", t0), Lookup::Stale(_)));
    }

    #[test]
    fn stamps_and_matching() {
        let dir = std::env::temp_dir().join(format!("warden-cache-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f.txt");
        std::fs::write(&p, "one").unwrap();
        let m1 = std::fs::metadata(&p).unwrap();
        let s1 = Stamp::of(&m1);
        assert!(s1.racy(std::time::SystemTime::now()), "just written");
        assert!(!s1.racy(std::time::SystemTime::now() + Duration::from_secs(3)));
        assert!(Seen::File(s1).matches(Some(&m1)));
        assert!(!Seen::NotFile.matches(Some(&m1)));
        assert!(Seen::NotFile.matches(None));
        assert!(!Seen::File(s1).matches(None));
        // Same size, mtime put back: ctime still tells.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&p, "two").unwrap();
        let f = std::fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(m1.modified().unwrap()).unwrap();
        let m2 = std::fs::metadata(&p).unwrap();
        assert_eq!(m2.modified().unwrap(), m1.modified().unwrap());
        assert!(!Seen::File(s1).matches(Some(&m2)), "content changed under an old mtime");
        let dm = std::fs::metadata(&dir).unwrap();
        assert!(Seen::Dir.matches(Some(&dm)) && Seen::NotFile.matches(Some(&dm)));
        assert!(!Seen::Dir.matches(Some(&m2)));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! Opening files under the root, and reading them.

use super::Site;
use super::store::{Entry, Store};
use std::io::{Error, ErrorKind};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// How files are opened. Starts at OPEN_CACHED and steps down if the kernel
// or filesystem can't do better.
/// openat2(RESOLVE_BENEATH | RESOLVE_CACHED) inline: one syscall, the
/// kernel keeps the lookup inside the root, and it never waits for the disk
/// (a lookup not in the dentry cache goes to a thread instead).
pub(super) const OPEN_CACHED: u8 = 0;

/// openat2(RESOLVE_BENEATH) inline (kernels 5.6–5.11, or filesystems whose
/// lookups are never served from cache).
pub(super) const OPEN_BENEATH: u8 = 1;

/// No openat2, on a thread: on Linux (kernel before 5.6, or blocked by
/// seccomp) a realpath check then the open; on macOS see `open_checked`.
pub(super) const OPEN_LEGACY: u8 = 2;

/// An open file (or directory) under the root and its metadata.
pub(super) struct Opened {
    pub(super) file: std::fs::File,
    pub(super) meta: std::fs::Metadata,
}

/// Flags for every open: O_NONBLOCK so a FIFO in the root can't hang the
/// worker (regular-file reads ignore it); O_NOCTTY for devices.
const OPEN_FLAGS: i32 = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOCTTY;

fn opened(fd: OwnedFd) -> std::io::Result<Opened> {
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    Ok(Opened { file, meta })
}

/// openat2 of `rel` (empty: the root itself) relative to `dir`. The path
/// is made a C string on the stack when it fits, so a request does not
/// allocate for it.
fn openat2_rel(dir: BorrowedFd<'_>, rel: &str, resolve: u64) -> std::io::Result<OwnedFd> {
    use std::io::{Error, ErrorKind};
    let name = if rel.is_empty() { "." } else { rel };
    let mut stack = [0u8; 256];
    if name.len() < stack.len() {
        stack[..name.len()].copy_from_slice(name.as_bytes());
        // The NUL after the name is already there; a NUL inside it is an error.
        let path = std::ffi::CStr::from_bytes_with_nul(&stack[..=name.len()])
            .map_err(|_| Error::from(ErrorKind::InvalidInput))?;
        return crate::sys::openat2(dir, path, OPEN_FLAGS, resolve);
    }
    let path = std::ffi::CString::new(name).map_err(|_| Error::from(ErrorKind::InvalidInput))?;
    crate::sys::openat2(dir, &path, OPEN_FLAGS, resolve)
}

/// Open `rel` under the root. Errors: NotFound (also for paths the kernel
/// or the realpath check says leave the root), PermissionDenied, others.
pub(super) async fn open(site: &Site, rel: &str) -> std::io::Result<Opened> {
    if site.open_mode.load(Ordering::Relaxed) != OPEN_LEGACY {
        match open_beneath(site, &site.dir, rel).await {
            Ok(fd) => return opened(fd),
            Err(e) => match e.raw_os_error() {
                Some(libc::ENOSYS | libc::EPERM) => {
                    site.open_mode.store(OPEN_LEGACY, Ordering::Relaxed);
                    eprintln!(
                        "warden serve-static: openat2 is unavailable ({e}); checking paths with realpath instead"
                    );
                }
                // The lookup left the root: `..` in a symlink, or an absolute
                // symlink. Absolute symlinks that point back inside the root
                // were always allowed, so the realpath check below decides.
                Some(libc::EXDEV) => {}
                _ => return Err(e),
            },
        }
    }
    let beneath = site.open_mode.load(Ordering::Relaxed) != OPEN_LEGACY;
    open_on_thread(site, rel, beneath).await
}

/// openat2 with the kernel keeping the lookup inside the root, the way the
/// open mode says (and stepping the mode down when the kernel cannot).
async fn open_beneath(site: &Site, dir: &Arc<OwnedFd>, rel: &str) -> std::io::Result<OwnedFd> {
    let beneath = crate::sys::RESOLVE_BENEATH | crate::sys::RESOLVE_NO_MAGICLINKS;
    if site.open_mode.load(Ordering::Relaxed) != OPEN_CACHED {
        return openat2_rel(dir.as_fd(), rel, beneath);
    }
    match openat2_rel(dir.as_fd(), rel, beneath | crate::sys::RESOLVE_CACHED) {
        Ok(fd) => {
            site.cached_hits.fetch_add(1, Ordering::Relaxed);
            Ok(fd)
        }
        // Not all in the dentry cache: the lookup may wait for the disk, so
        // it runs on a thread.
        Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => {
            let misses = site.cached_misses.fetch_add(1, Ordering::Relaxed) + 1;
            if misses >= 256 && site.cached_hits.load(Ordering::Relaxed) == 0 {
                // This filesystem never answers from cache (e.g. it
                // revalidates every lookup): stop asking.
                site.open_mode.store(OPEN_BENEATH, Ordering::Relaxed);
            }
            let dir = dir.clone();
            let rel = rel.to_string();
            tokio::task::spawn_blocking(move || openat2_rel(dir.as_fd(), &rel, beneath)).await.map_err(Error::other)?
        }
        // RESOLVE_CACHED is newer (5.12) than openat2 (5.6).
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
            site.open_mode.store(OPEN_BENEATH, Ordering::Relaxed);
            openat2_rel(dir.as_fd(), rel, beneath)
        }
        Err(e) => Err(e),
    }
}

/// How files are opened in OPEN_LEGACY, for the line `warden serve` starts with.
pub(super) fn legacy_open_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        if crate::sys::resolve_beneath_works() {
            return "openat, O_RESOLVE_BENEATH";
        }
        if crate::sys::nofollow_any_works() {
            return "openat, O_NOFOLLOW_ANY";
        }
        "openat, then F_GETPATH"
    }
    #[cfg(not(target_os = "macos"))]
    "realpath check"
}

/// `open_checked`, on a thread. `beneath`: openat2 works here.
async fn open_on_thread(site: &Site, rel: &str, beneath: bool) -> std::io::Result<Opened> {
    let dir = site.dir.clone();
    let root = site.root.clone();
    let rel = rel.to_string();
    tokio::task::spawn_blocking(move || open_checked(&dir, &root, &rel, beneath)).await.map_err(Error::other)?
}

/// Where `rel` leads once every symlink is followed (realpath), relative
/// to the root and without symlinks; NotFound if that is outside the root.
fn resolved(root: &Path, rel: &str) -> std::io::Result<std::path::PathBuf> {
    let path = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    match std::fs::canonicalize(&path).map(|c| c.strip_prefix(root).map(Path::to_path_buf)) {
        Ok(Ok(real)) => Ok(real),
        _ => Err(Error::from(ErrorKind::NotFound)),
    }
}

/// `path` (empty: the root itself) as a C string for a system call.
fn c_path(path: &Path) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = if path.as_os_str().is_empty() { b".".as_slice() } else { path.as_os_str().as_bytes() };
    std::ffi::CString::new(bytes).map_err(|_| Error::from(ErrorKind::InvalidInput))
}

/// Linux, `rel` through a symlink openat2 refused (an absolute one, which
/// may point back inside the root), or anything without openat2: served if
/// its real path is inside the root. With openat2 (`beneath`), that real
/// path (no symlinks left) is then opened kept inside the root, so a
/// symlink swapped in after the check cannot lead out (it is refused).
/// Without, the check and the open are two steps.
#[cfg(not(target_os = "macos"))]
fn open_checked(dir: &OwnedFd, root: &Path, rel: &str, beneath: bool) -> std::io::Result<Opened> {
    let real = resolved(root, rel)?;
    if beneath {
        let resolve = crate::sys::RESOLVE_BENEATH | crate::sys::RESOLVE_NO_MAGICLINKS;
        return match crate::sys::openat2(dir.as_fd(), &c_path(&real)?, OPEN_FLAGS, resolve) {
            Ok(fd) => opened(fd),
            Err(e) if e.raw_os_error() == Some(libc::EXDEV) => Err(Error::from(ErrorKind::NotFound)),
            Err(e) => Err(e),
        };
    }
    let path = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(&path)?;
    let meta = file.metadata()?;
    Ok(Opened { file, meta })
}

/// macOS: `rel` under the root `dir`, with no window between a check and
/// the open (a symlink swapped in between could otherwise lead outside).
/// One openat, confined by the kernel:
/// - macOS 15+: O_RESOLVE_BENEATH keeps the lookup inside the root, as
///   openat2 does on Linux;
/// - macOS 11–14: O_NOFOLLOW_ANY refuses any symlink on the way, so with no
///   `..` in `rel` it cannot leave the root.
///
/// A path through a symlink the flag refuses (one leading out of the root or
/// absolute from 15, any before) is resolved with realpath; if that ends
/// inside the root, the real path (no symlinks left) is opened confined the
/// same way, so a symlink swapped in after the check is refused. NotFound
/// for a path that leaves the root. (`beneath` is for Linux.)
#[cfg(target_os = "macos")]
fn open_checked(dir: &OwnedFd, root: &Path, rel: &str, beneath: bool) -> std::io::Result<Opened> {
    let _ = beneath;
    let confine = if crate::sys::resolve_beneath_works() {
        Some((crate::sys::O_RESOLVE_BENEATH, libc::ENOTCAPABLE))
    } else if crate::sys::nofollow_any_works() {
        Some((libc::O_NOFOLLOW_ANY, libc::ELOOP))
    } else {
        None
    };
    // (Request paths never have `..`; a configured name might.)
    let direct = confine.filter(|(flag, _)| *flag != libc::O_NOFOLLOW_ANY || !rel.split('/').any(|c| c == ".."));
    if let Some((flag, refused)) = direct {
        match crate::sys::openat(dir.as_fd(), &c_path(Path::new(rel))?, OPEN_FLAGS | flag) {
            Ok(fd) => return opened(fd),
            // (ELOOP is also a symlink loop: realpath fails on it below.)
            Err(e) if e.raw_os_error() == Some(refused) => {}
            Err(e) => return Err(e),
        }
    }
    let real = c_path(&resolved(root, rel)?)?;
    match confine {
        Some((flag, refused)) => match crate::sys::openat(dir.as_fd(), &real, OPEN_FLAGS | flag) {
            Ok(fd) => opened(fd),
            Err(e) if e.raw_os_error() == Some(refused) => Err(Error::from(ErrorKind::NotFound)),
            Err(e) => Err(e),
        },
        // Neither flag (not expected on macOS 11+): ask the open descriptor
        // where its file is, which no swap can change any more.
        None => {
            let fd = crate::sys::openat(dir.as_fd(), &real, OPEN_FLAGS)?;
            if !fd_beneath(dir, root, &fd)? {
                return Err(Error::from(ErrorKind::NotFound));
            }
            opened(fd)
        }
    }
}

/// Whether the file open at `fd` is under the root (`root`, open at
/// `dir`), by the path the kernel has for each (F_GETPATH). The root's is
/// asked only when `root` (from realpath) does not match: the two may
/// spell it differently (letter case on a case-insensitive volume).
#[cfg(target_os = "macos")]
fn fd_beneath(dir: &OwnedFd, root: &Path, fd: &OwnedFd) -> std::io::Result<bool> {
    let at = crate::sys::fd_path(fd.as_fd())?;
    Ok(at.starts_with(root) || at.starts_with(crate::sys::fd_path(dir.as_fd())?))
}

/// `rel` under the root for a thread of its own, which may wait: the way
/// requests open files (openat2 keeping the lookup inside the root, or the
/// realpath check where there is no openat2).
pub(super) fn open_source(dir: &OwnedFd, root: &Path, rel: &str) -> std::io::Result<Opened> {
    let beneath = crate::sys::RESOLVE_BENEATH | crate::sys::RESOLVE_NO_MAGICLINKS;
    match openat2_rel(dir.as_fd(), rel, beneath) {
        Ok(fd) => opened(fd),
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => open_checked(dir, root, rel, true),
        Err(e)
            if e.kind() == ErrorKind::Unsupported || matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM)) =>
        {
            open_checked(dir, root, rel, false)
        }
        Err(e) => Err(e),
    }
}

/// The compressed copy at `entry` in `store`: one system call when its path is
/// in the kernel's cache, as for a file under the root (and the same
/// fallbacks). NotFound when there is none (yet).
pub(super) async fn open_stored(site: &Site, store: &Store, entry: &Entry) -> std::io::Result<Opened> {
    if site.open_mode.load(Ordering::Relaxed) != OPEN_LEGACY {
        match open_beneath(site, &store.dir, entry.as_str()).await {
            Ok(fd) => return opened(fd),
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM | libc::EXDEV)) => {}
            Err(e) => return Err(e),
        }
    }
    let path = store.path_of(entry);
    tokio::task::spawn_blocking(move || {
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(path)?;
        let meta = file.metadata()?;
        Ok(Opened { file, meta })
    })
    .await
    .map_err(Error::other)?
}

/// Read `buf[from..]` from `file` at `offset`. Cached data is read inline
/// (preadv2 RWF_NOWAIT never waits for the disk); anything else is read on
/// a thread, so a cold file can't stall the other connections. The file
/// comes back too (the cache checks it did not change meanwhile).
pub(super) async fn read_body(
    file: std::fs::File,
    mut buf: Vec<u8>,
    from: usize,
    offset: u64,
) -> std::io::Result<(std::fs::File, Vec<u8>)> {
    use std::io::{Error, ErrorKind};
    let mut done = 0usize;
    while from + done < buf.len() {
        match crate::sys::pread_nowait(file.as_fd(), &mut buf[from + done..], offset + done as u64) {
            Ok(0) => return Err(Error::new(ErrorKind::UnexpectedEof, "file truncated while reading")),
            Ok(n) => done += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e)
                if e.kind() == ErrorKind::WouldBlock
                    || e.kind() == ErrorKind::Unsupported
                    || e.raw_os_error() == Some(libc::EOPNOTSUPP) =>
            {
                return tokio::task::spawn_blocking(move || {
                    file.read_exact_at(&mut buf[from + done..], offset + done as u64).map(|()| (file, buf))
                })
                .await
                .map_err(Error::other)?;
            }
            Err(e) => return Err(e),
        }
    }
    Ok((file, buf))
}

/// Response status for a failed open.
pub(super) fn open_error_status(e: &std::io::Error) -> u16 {
    if e.kind() == std::io::ErrorKind::PermissionDenied { 403 } else { 404 }
}

/// A resolved path is only served if, after following symlinks, it is still
/// inside root.
pub(super) fn inside(root: &Path, p: &Path) -> bool {
    std::fs::canonicalize(p).is_ok_and(|c| c.starts_with(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;

    /// root/{a.txt, sub/b.txt, rel -> a.txt, abs -> <root>/a.txt,
    /// subabs -> <root>/sub, up -> ../outside.txt, out -> <base>/outside.txt,
    /// outdir -> <base>}, outside.txt next to the root. (root, its fd, base)
    fn tree(name: &str) -> (std::path::PathBuf, OwnedFd, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("warden-open-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("root/sub")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let root = base.join("root");
        std::fs::write(root.join("a.txt"), "A").unwrap();
        std::fs::write(root.join("sub/b.txt"), "B").unwrap();
        std::fs::write(base.join("outside.txt"), "secret").unwrap();
        std::os::unix::fs::symlink("a.txt", root.join("rel")).unwrap();
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("abs")).unwrap();
        std::os::unix::fs::symlink(root.join("sub"), root.join("subabs")).unwrap();
        std::os::unix::fs::symlink("../outside.txt", root.join("up")).unwrap();
        std::os::unix::fs::symlink(base.join("outside.txt"), root.join("out")).unwrap();
        std::os::unix::fs::symlink(&base, root.join("outdir")).unwrap();
        let dir = OwnedFd::from(std::fs::File::open(&root).unwrap());
        (root, dir, base)
    }

    fn content(o: std::io::Result<Opened>) -> std::io::Result<String> {
        let mut s = String::new();
        o?.file.read_to_string(&mut s)?;
        Ok(s)
    }

    /// Every platform's way of opening by path (openat2 on Linux, falling
    /// back to the realpath check; O_RESOLVE_BENEATH or O_NOFOLLOW_ANY then
    /// the descriptor's path on macOS) serves symlinks that stay inside the
    /// root and refuses the ones that leave it.
    #[test]
    fn symlinks_inside_the_root_work_and_the_others_do_not() {
        let (root, dir, base) = tree("semantics");
        let open = |rel: &str| content(open_source(&dir, &root, rel));
        assert_eq!(open("a.txt").unwrap(), "A");
        assert_eq!(open("sub/b.txt").unwrap(), "B");
        assert_eq!(open("rel").unwrap(), "A", "relative symlink inside");
        assert_eq!(open("abs").unwrap(), "A", "absolute symlink inside");
        assert_eq!(open("subabs/b.txt").unwrap(), "B", "absolute directory symlink inside");
        assert!(open_source(&dir, &root, "").unwrap().meta.is_dir());
        for bad in ["up", "out", "outdir/outside.txt", "missing"] {
            assert_eq!(open(bad).unwrap_err().kind(), ErrorKind::NotFound, "{bad}");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    /// The race the check-then-open had: a symlink flipped between a target
    /// inside the root and one outside while requests open it. Whatever the
    /// timing, nothing outside is ever served.
    #[test]
    fn a_symlink_swapped_during_opens_never_leads_outside() {
        let (root, dir, base) = tree("swap");
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let swapper = {
            let (root, base, stop) = (root.clone(), base.clone(), stop.clone());
            std::thread::spawn(move || {
                let (link, tmp) = (root.join("flip"), root.join("flip.tmp"));
                let mut i = 0u64;
                while !stop.load(Ordering::Relaxed) {
                    let target = if i % 2 == 0 { root.join("a.txt") } else { base.join("outside.txt") };
                    let _ = std::fs::remove_file(&tmp);
                    std::os::unix::fs::symlink(target, &tmp).unwrap();
                    std::fs::rename(&tmp, &link).unwrap(); // an atomic swap
                    i += 1;
                }
            })
        };
        let mut served = 0;
        for _ in 0..20_000 {
            // Refusals are fine (NotFound, or the kernel's EAGAIN for a
            // confined lookup racing a rename). So is the directory holding
            // the link: Linux can see a link being replaced as empty, which
            // resolves to that directory, still inside the root.
            let Ok(o) = open_source(&dir, &root, "flip") else { continue };
            if o.meta.is_dir() {
                assert_eq!(o.meta.ino(), std::fs::metadata(&root).unwrap().ino(), "a directory inside the root");
                continue;
            }
            assert_eq!(content(Ok(o)).unwrap(), "A", "served a file outside the root");
            served += 1;
        }
        stop.store(true, Ordering::Relaxed);
        swapper.join().unwrap();
        assert!(served > 0, "the inside target was served at times");
        let _ = std::fs::remove_dir_all(base);
    }

    /// macOS: the check of an open descriptor (the last resort, where
    /// neither O_RESOLVE_BENEATH nor O_NOFOLLOW_ANY works) tells a file
    /// inside the root from one outside.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_open_descriptor_is_placed_inside_or_outside() {
        let (root, dir, base) = tree("darwin");
        let outside = OwnedFd::from(std::fs::File::open(base.join("outside.txt")).unwrap());
        assert!(!fd_beneath(&dir, &root, &outside).unwrap());
        let inside = OwnedFd::from(std::fs::File::open(root.join("sub/b.txt")).unwrap());
        assert!(fd_beneath(&dir, &root, &inside).unwrap());
        let _ = std::fs::remove_dir_all(base);
    }
}

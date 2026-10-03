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

/// No openat2 (kernel before 5.6, or blocked by seccomp): realpath check
/// then open, on a thread.
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
    open_by_realpath(site, rel).await
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

/// Without openat2: the path is resolved and checked to be inside the root
/// (`inside`), then opened, on a thread.
async fn open_by_realpath(site: &Site, rel: &str) -> std::io::Result<Opened> {
    let root = site.root.clone();
    let rel = rel.to_string();
    tokio::task::spawn_blocking(move || open_checked(&root, &rel)).await.map_err(Error::other)?
}

/// `rel` under `root`, once its real path is known to be inside the root.
fn open_checked(root: &Path, rel: &str) -> std::io::Result<Opened> {
    let path = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    if !inside(root, &path) {
        return Err(Error::from(ErrorKind::NotFound));
    }
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY).open(&path)?;
    let meta = file.metadata()?;
    Ok(Opened { file, meta })
}

/// `rel` under the root for a thread of its own, which may wait: the way
/// requests open files (openat2 keeping the lookup inside the root, or the
/// realpath check where there is no openat2).
pub(super) fn open_source(dir: &OwnedFd, root: &Path, rel: &str) -> std::io::Result<Opened> {
    let beneath = crate::sys::RESOLVE_BENEATH | crate::sys::RESOLVE_NO_MAGICLINKS;
    match openat2_rel(dir.as_fd(), rel, beneath) {
        Ok(fd) => opened(fd),
        Err(e)
            if e.kind() == ErrorKind::Unsupported
                || matches!(e.raw_os_error(), Some(libc::ENOSYS | libc::EPERM | libc::EXDEV)) =>
        {
            open_checked(root, rel)
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

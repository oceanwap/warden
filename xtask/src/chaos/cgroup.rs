//! A memory cgroup for the chaos soak's `oom` app (Linux, root): Warden does
//! not limit memory itself; it reads the OOM-kill counter of the cgroup it
//! runs in (`memory.events`, v1 `memory.oom_control`), as under a systemd
//! unit's `MemoryMax=` or a container's limit. The harness makes one with a
//! limit and starts that app's supervisor inside it.

use super::fleet::Cgroup;
use std::path::{Path, PathBuf};

/// The cgroup files that set a limit: (memory limit, swap limit; the second
/// one may be missing without swap accounting).
type Files = (&'static str, &'static str);
const V1: Files = ("memory.limit_in_bytes", "memory.memsw.limit_in_bytes");
const V2: Files = ("memory.max", "memory.swap.max");

/// Make `name` with a limit of `limit_mb` (no swap beyond it): a child of
/// our own memory cgroup (v1 or v2), else, on cgroup v2, of the root cgroup
/// (our own may hold processes and so cannot give its children the memory
/// controller). Err: every place tried, and why it did not work.
pub fn make(name: &str, limit_mb: u64) -> Result<Cgroup, String> {
    if !cfg!(target_os = "linux") {
        return Err("memory cgroups are Linux's".into());
    }
    let own = std::fs::read_to_string("/proc/self/cgroup").map_err(|e| format!("/proc/self/cgroup: {e}"))?;
    let v2_root = Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    let mut places: Vec<(PathBuf, Files)> = Vec::new();
    for line in own.lines() {
        let mut f = line.splitn(3, ':');
        let (Some(_), Some(ctl), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
        let rel = path.trim_start_matches('/');
        if ctl.split(',').any(|c| c == "memory") {
            places.push((Path::new("/sys/fs/cgroup/memory").join(rel), V1));
        } else if ctl.is_empty() && v2_root {
            places.push((Path::new("/sys/fs/cgroup").join(rel), V2));
            places.push((PathBuf::from("/sys/fs/cgroup"), V2));
        }
    }
    let mut tried = Vec::new();
    for (parent, files) in places {
        let dir = parent.join(name);
        match try_make(&dir, files, limit_mb << 20) {
            Ok(()) => return Ok(Cgroup { procs: dir.join("cgroup.procs"), dir, limit_mb }),
            Err(e) => tried.push(format!("{}: {e}", dir.display())),
        }
    }
    if tried.is_empty() {
        return Err("no memory controller in /proc/self/cgroup".into());
    }
    Err(tried.join("; "))
}

fn try_make(dir: &Path, (limit_file, swap_file): Files, bytes: u64) -> Result<(), String> {
    if !dir.exists() {
        std::fs::create_dir(dir).map_err(|e| format!("mkdir: {e}"))?;
    }
    if let Err(e) = std::fs::write(dir.join(limit_file), bytes.to_string()) {
        let _ = std::fs::remove_dir(dir);
        return Err(format!("{limit_file}: {e}"));
    }
    // v1 memsw counts memory + swap: the same value means no swap; v2 swap.max is swap alone.
    let swap = if swap_file == V2.1 { "0".to_string() } else { bytes.to_string() };
    let _ = std::fs::write(dir.join(swap_file), swap);
    Ok(())
}

/// Remove it once its processes are gone (a few tries: the last ones may
/// still be exiting). Err: why it is still there.
pub fn remove(cg: &Cgroup) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..50 {
        match std::fs::remove_dir(&cg.dir) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let left = std::fs::read_to_string(&cg.procs).unwrap_or_default().split_whitespace().collect::<Vec<_>>().join(" ");
    Err(format!("rmdir {}: {last} (pids still in it: {left})", cg.dir.display()))
}

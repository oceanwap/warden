//! `warden doctor`: the environment problems we know about, each with the
//! command or setting that fixes it. Read-only: it creates, changes and
//! starts nothing. Exit 1 when something will break, 0 otherwise (warnings
//! are things that work but cost reliability or speed).

use crate::cli::Args;
use crate::fleet;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Finding {
    pub level: Level,
    pub check: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

fn f(level: Level, check: &str, detail: impl Into<String>, fix: Option<&str>) -> Finding {
    Finding { level, check: check.into(), detail: detail.into(), fix: fix.map(String::from) }
}

pub async fn run(args: &Args) -> i32 {
    let mut out = Vec::new();
    out.extend(kernel());
    out.push(runtime("bun", &["--version"], None));
    out.push(runtime("node", &["--version"], Some((22, 12))));
    out.push(file_limit());
    out.push(runtime_dir());
    out.extend(pid_one());
    out.extend(apps(args).await);
    out.extend(boot());
    out.extend(pm2());
    if args.json {
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        print!("{}", render(&out));
    }
    if out.iter().any(|x| x.level == Level::Fail) { 1 } else { 0 }
}

pub fn render(findings: &[Finding]) -> String {
    let width = findings.iter().map(|x| x.check.len()).max().unwrap_or(0);
    let mut s = String::new();
    for x in findings {
        let tag = match x.level {
            Level::Ok => "ok  ",
            Level::Info => "info",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        s += &format!("{tag}  {:<width$}  {}\n", x.check, x.detail);
        if let Some(fix) = &x.fix {
            s += &format!("      {:<width$}  fix: {fix}\n", "");
        }
    }
    let count = |l| findings.iter().filter(|x| x.level == l).count();
    let (fails, warns) = (count(Level::Fail), count(Level::Warn));
    s += &match (fails, warns) {
        (0, 0) => "\nNo problems found.\n".to_string(),
        (0, w) => format!("\n{w} warning(s): everything works, the fixes above make it sturdier.\n"),
        (p, w) => format!("\n{p} problem(s) that will break something, {w} warning(s).\n"),
    };
    s
}

// ------------------------------------------------------------------ kernel

fn kernel() -> Vec<Finding> {
    let mut v = Vec::new();
    if !cfg!(target_os = "linux") {
        v.push(f(
            Level::Warn,
            "kernel",
            "not Linux: fine for development, but SO_REUSEPORT does not balance connections across workers \
             (use count = 1), no parent-death signal (workers outlive a SIGKILLed supervisor), \
             no /proc readiness or CPU/RSS metrics",
            Some("run production on Linux"),
        ));
        v.push(f(
            Level::Info,
            "openat2",
            "not Linux: unavailable; `warden serve` checks paths with realpath instead (slower, same safety)",
            None,
        ));
        return v;
    }
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string();
    let shared = crate::sys::listen_tcp("127.0.0.1:0".parse().expect("valid address"), true, 8).and_then(|a| {
        let port = a.local_addr()?.port();
        crate::sys::listen_tcp(std::net::SocketAddr::from(([127, 0, 0, 1], port)), true, 8).map(|b| (a, b))
    });
    v.push(match shared {
        Ok(_) => f(Level::Ok, "kernel", format!("Linux {release}; SO_REUSEPORT works"), None),
        Err(e) => f(
            Level::Fail,
            "kernel",
            format!("two listeners can't share a port with SO_REUSEPORT: {e}"),
            Some("use workers.port_strategy = \"offset\" (one port per worker behind a proxy)"),
        ),
    });
    v.push(match std::fs::read_to_string("/proc/sys/net/ipv4/tcp_migrate_req").map(|s| s.trim().to_string()) {
        Ok(x) if x == "1" => {
            f(Level::Ok, "tcp_migrate_req", "1: restarts move queued connections to a live worker", None)
        }
        Ok(_) => f(
            Level::Warn,
            "tcp_migrate_req",
            "0: a rolling restart can reset connections queued on a stopping worker",
            Some(
                "sysctl -w net.ipv4.tcp_migrate_req=1, and keep it with contrib/99-warden.conf in /etc/sysctl.d \
                 (`warden startup` installs it); in a container: docker run --sysctl net.ipv4.tcp_migrate_req=1",
            ),
        ),
        Err(_) => f(
            Level::Warn,
            "tcp_migrate_req",
            format!("not available on Linux {release} (added in 5.14): restarts can reset queued connections"),
            Some("a 5.14+ kernel"),
        ),
    });
    let root = std::fs::File::open("/");
    v.push(
        match root.map_err(|e| e.to_string()).and_then(|d| {
            use std::os::fd::AsFd;
            crate::sys::openat2(d.as_fd(), c".", libc::O_RDONLY, crate::sys::RESOLVE_BENEATH).map_err(|e| e.to_string())
        }) {
            Ok(_) => {
                f(Level::Ok, "openat2", "available: `warden serve` opens files in one syscall, kernel-confined", None)
            }
            Err(e) => f(
                Level::Info,
                "openat2",
                format!("unavailable ({e}): `warden serve` checks paths with realpath instead (slower, same safety)"),
                Some("a 5.6+ kernel, or allow openat2 in the container's seccomp profile"),
            ),
        },
    );
    v
}

// ---------------------------------------------------------------- runtimes

/// `v22.12.0` / `1.3.13` → (22, 12).
pub fn major_minor(v: &str) -> Option<(u32, u32)> {
    let mut it = v.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().ok());
    Some((it.next()??, it.next()??))
}

fn runtime(cmd: &str, args: &[&str], min: Option<(u32, u32)>) -> Finding {
    let out = std::process::Command::new(cmd).args(args).output();
    let version = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => {
            return f(Level::Info, cmd, format!("not on PATH (needed only for {cmd} apps)"), None);
        }
    };
    match (min, major_minor(&version)) {
        (Some(m), Some(v)) if v < m => f(
            Level::Warn,
            cmd,
            format!("{version}: older than {}.{}, so workers can't share a port (reusePort)", m.0, m.1),
            Some("upgrade to Node 22.12+, or use workers.port_strategy = \"offset\" for Node apps"),
        ),
        _ => f(Level::Ok, cmd, version, None),
    }
}

// ------------------------------------------------------------------ limits

fn file_limit() -> Finding {
    let (soft, hard) = crate::sys::nofile_limit();
    if soft >= 16_384 {
        f(Level::Ok, "open files", format!("limit {soft}"), None)
    } else {
        f(
            Level::Warn,
            "open files",
            format!("limit {soft} (hard {hard}): a busy static site or many workers can run out"),
            Some("LimitNOFILE=65536 in the systemd unit (contrib/warden@.service has it), or `ulimit -n 65536`"),
        )
    }
}

fn runtime_dir() -> Finding {
    let dir = crate::config::runtime_dir();
    match check_private_dir(&dir) {
        Ok(true) => f(Level::Ok, "runtime dir", format!("{} is private", dir.display()), None),
        Ok(false) => f(Level::Ok, "runtime dir", format!("{} (created on first start)", dir.display()), None),
        Err(e) => f(
            Level::Fail,
            "runtime dir",
            e,
            Some("remove it or `chmod 700` it as the user Warden runs as; it holds the sockets and the shim"),
        ),
    }
}

/// Like the check at startup, without creating anything: Ok(false) = missing.
pub fn check_private_dir(dir: &Path) -> Result<bool, String> {
    use std::os::unix::fs::MetadataExt;
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(format!("{} is a symlink or not a directory: Warden refuses to use it", dir.display()));
    }
    let euid = crate::sys::euid();
    if meta.uid() != euid {
        return Err(format!(
            "{} is owned by uid {}, not uid {euid}: Warden refuses to use it",
            dir.display(),
            meta.uid()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is writable by group/others (mode {:o}): Warden refuses to use it",
            dir.display(),
            meta.mode() & 0o777
        ));
    }
    Ok(true)
}

fn pid_one() -> Option<Finding> {
    // A container whose PID 1 is Warden itself: nothing reaps orphaned
    // grandchildren (a `bun run` wrapper's children), so zombies pile up.
    let comm = std::fs::read_to_string("/proc/1/comm").ok()?;
    (comm.trim() == "warden").then(|| {
        f(
            Level::Fail,
            "pid 1",
            "Warden runs as PID 1: orphaned processes are never reaped",
            Some("docker run --init, or tini as the entrypoint (`tini -- warden start ...`)"),
        )
    })
}

// -------------------------------------------------------------------- apps

async fn apps(args: &Args) -> Vec<Finding> {
    let ctx = fleet::context(args);
    let mut v = Vec::new();
    if ctx.apps.is_empty() {
        v.push(f(Level::Info, "apps", format!("none in {}", fleet::config_dir().display()), None));
        return v;
    }
    for app in &ctx.apps {
        let check = format!("app {}", app.name);
        if let Some(p) = &app.problem {
            v.push(f(Level::Fail, &check, p.clone(), Some("fix the config; `warden check -c <file>` shows the error")));
            continue;
        }
        let running = std::os::unix::net::UnixStream::connect(&app.socket).is_ok();
        if running {
            v.push(f(Level::Ok, &check, "running", None));
            continue;
        }
        let cfg = app.config.as_deref().and_then(|p| crate::config::Config::load(p).ok());
        // Not running: will its port be free when it starts?
        if let Some(port) = cfg.as_ref().and_then(|c| c.app.port) {
            let host: std::net::IpAddr = cfg
                .as_ref()
                .and_then(|c| c.static_files.as_ref())
                .and_then(|s| s.host.parse().ok())
                .unwrap_or(std::net::IpAddr::from([0, 0, 0, 0]));
            if let Err(e) = crate::sys::listen_tcp(std::net::SocketAddr::new(host, port), false, 1) {
                if e.raw_os_error() == Some(libc::EADDRINUSE) {
                    v.push(f(
                        Level::Fail,
                        &check,
                        format!("stopped, and port {port} is taken by another program"),
                        Some(&format!("`ss -ltnp 'sport = :{port}'` shows who; stop it or change app.port")),
                    ));
                    continue;
                }
            }
        }
        let stale = app.socket.exists();
        v.push(f(
            Level::Info,
            &check,
            if stale { "stopped (a stale control socket is left; the next start replaces it)" } else { "stopped" },
            None,
        ));
    }
    v
}

fn boot() -> Option<Finding> {
    // Saved apps come back after a reboot through what `warden startup` installs.
    let saved = fleet::saved_names();
    if saved.is_empty() {
        return None;
    }
    let n = saved.len();
    Some(match crate::startup::installed() {
        Some(what) => f(Level::Ok, "boot", format!("{n} saved app(s); {what} bring them back after a reboot"), None),
        None if crate::startup::available() => f(
            Level::Warn,
            "boot",
            format!("{n} saved app(s), but nothing brings them back after a reboot"),
            Some("warden startup (as root for system units, else your own user units)"),
        ),
        None => f(
            Level::Info,
            "boot",
            format!("{n} saved app(s); no service manager here (no systemd)"),
            Some("run `warden resurrect` or `warden daemon --resurrect` from your init system or entrypoint"),
        ),
    })
}

fn pm2() -> Option<Finding> {
    // A PM2 daemon still running after a migration runs the same apps twice.
    let rd = std::fs::read_dir("/proc").ok()?;
    let daemons = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter(|e| {
            std::fs::read(e.path().join("cmdline")).is_ok_and(|c| {
                let c = String::from_utf8_lossy(&c);
                c.starts_with("PM2 v") && c.contains("God Daemon")
            })
        })
        .count();
    (daemons > 0).then(|| {
        f(
            Level::Info,
            "pm2",
            format!("{daemons} PM2 daemon(s) running"),
            Some("once its apps run under Warden: `pm2 kill` and `pm2 unstartup`"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(major_minor("v22.12.0"), Some((22, 12)));
        assert_eq!(major_minor("1.3.13"), Some((1, 3)));
        assert_eq!(major_minor("v20"), None);
        assert_eq!(major_minor("nonsense"), None);
        assert!(major_minor("v22.11.0").unwrap() < (22, 12));
    }

    #[test]
    fn private_dir_rules() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("warden-doctor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(check_private_dir(&base), Ok(false), "missing is fine");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(check_private_dir(&base), Ok(true));
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_private_dir(&base).unwrap_err().contains("writable by group/others"));
        let link = base.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&base, &link).unwrap();
        assert!(check_private_dir(&link).unwrap_err().contains("symlink"));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn report_counts_and_fixes() {
        let out = render(&[
            f(Level::Ok, "kernel", "fine", None),
            f(Level::Warn, "tcp_migrate_req", "0", Some("sysctl -w net.ipv4.tcp_migrate_req=1")),
        ]);
        assert!(out.contains("WARN  tcp_migrate_req  0\n"), "{out}");
        assert!(out.contains("fix: sysctl -w net.ipv4.tcp_migrate_req=1"), "{out}");
        assert!(out.contains("1 warning(s)"), "{out}");
        let out = render(&[f(Level::Fail, "pid 1", "bad", None)]);
        assert!(out.contains("1 problem(s)"), "{out}");
    }
}

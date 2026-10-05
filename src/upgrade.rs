//! `warden upgrade`: the newest release, installed over this warden, and every supervisor and
//! wardend restarted from it (`warden update`), so nothing is left to do. It runs the install.sh
//! built into this binary (as `warden gui-install` does): the download is checked against the
//! release's SHA256SUMS, and the GUI is upgraded with the CLI when it is installed.
//!
//! The newest release comes from GitHub's API through `curl` (Warden has no TLS stack of its
//! own); `WARDEN_RELEASES_URL` points elsewhere (a mirror, or a file:// in tests).

use crate::cli::{Args, Command};
use crate::gui_install::{SCRIPT, from_package, in_app, pipe_script};
use std::path::{Path, PathBuf};
use std::process::Stdio;

pub const HELP: &str = "\
warden upgrade - install the newest Warden release and restart onto it

USAGE:
    warden upgrade [--check] [--yes] [--version V] [--dry-run] [--json]

Finds the newest release, downloads it for this machine, checks it against the
release's SHA256SUMS and installs it over this warden (and the desktop app, when
it is installed). Then every app's supervisor and wardend restart on the new
version (`warden update`): the apps stop for a few seconds, and are running
again when it ends. Nothing is restarted when nothing was running.

OPTIONS:
    --check          Only say whether a newer release exists (exit 0 either way)
    --yes            Do not ask first (it asks on a terminal)
    --version V      This release instead of the newest (v0.2.0); also an older one
    --dry-run        Say what would happen; install and restart nothing
    --json           --check: {\"current\", \"latest\", \"update\"} on one line
    -h, --help       This help

ENVIRONMENT:
    WARDEN_RELEASES_URL   where the newest release is read from (default: GitHub's
                          API, releases/latest); WARDEN_DOWNLOAD_URL as install.sh
";

/// GitHub's answer for the newest release (only `tag_name` is read).
pub const LATEST_URL: &str = "https://api.github.com/repos/oceanwap/warden/releases/latest";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Opts {
    pub check: bool,
    pub yes: bool,
    pub version: Option<String>,
    pub dry_run: bool,
    pub json: bool,
    pub help: bool,
}

/// `warden upgrade [OPTIONS]`.
pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut o = Opts::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--check" => o.check = true,
            "--yes" | "-y" => o.yes = true,
            "--dry-run" => o.dry_run = true,
            "--json" => o.json = true,
            "--version" => {
                o.version =
                    Some(it.next().filter(|v| !v.is_empty()).cloned().ok_or("upgrade: --version needs a value")?)
            }
            s if s.starts_with("--version=") && s.len() > "--version=".len() => {
                o.version = Some(s["--version=".len()..].to_string())
            }
            "-h" | "--help" => o.help = true,
            s if s.starts_with('-') => return Err(format!("upgrade: unknown option {s} (warden upgrade --help)")),
            s => return Err(format!("upgrade: unexpected argument {s:?} (warden upgrade --help)")),
        }
    }
    if let Some(v) = o.version.as_ref().filter(|v| Version::parse(v).is_none()) {
        return Err(format!("upgrade: --version {v}: not a version (v0.2.0)"));
    }
    Ok(Args { command: Command::Upgrade(Box::new(o)), ..crate::cli::empty_args() })
}

/// A release's version, ordered as semver orders them (a pre-release before its release).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    core: (u64, u64, u64),
    /// `None` (a release) sorts after any pre-release of the same core.
    release: Option<()>,
    pre: Vec<PreId>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreId {
    Num(u64),
    Text(String),
}

impl Version {
    /// `v0.2.0`, `0.2.0`, `0.2.0-rc.1`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim().strip_prefix('v').unwrap_or(s.trim());
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut n = core.split('.').map(|p| p.parse::<u64>().ok());
        let core = (n.next()??, n.next()??, n.next()??);
        if n.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some("") => return None,
            Some(p) => p
                .split('.')
                .map(|id| id.parse().map(PreId::Num).unwrap_or_else(|_| PreId::Text(id.to_string())))
                .collect(),
        };
        Some(Version { core, release: pre.is_empty().then_some(()), pre })
    }
}

/// The tag of the newest release, as `v0.2.0`.
pub fn latest() -> Result<String, String> {
    let url = std::env::var("WARDEN_RELEASES_URL").ok().filter(|u| !u.is_empty()).unwrap_or(LATEST_URL.into());
    let out = std::process::Command::new("curl")
        .args(["-fsSL", "--max-time", "20", "-H", "Accept: application/vnd.github+json", &url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl to ask for the newest release: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("cannot read the newest release from {url}: {}", err.trim()));
    }
    tag_of(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| format!("{url} names no release (no tag_name)"))
}

fn tag_of(json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    Version::parse(tag)?;
    Some(if tag.starts_with('v') { tag.to_string() } else { format!("v{tag}") })
}

/// What install.sh is run with to put `version` where this warden is: the app (when this warden
/// is the one inside Warden.app), else this warden's folder, with the GUI when one is installed.
pub fn script_args(version: &str, exe: &Path, gui: Option<&Path>, dry_run: bool) -> Vec<String> {
    let mut a = Vec::new();
    match in_app(exe) {
        // The CLI inside the app comes with the app.
        Some(app) => {
            a.extend(["--gui-only".into(), "--version".into(), version.to_string()]);
            if let Some(dir) = app.parent() {
                a.extend(["--app-dir".into(), dir.display().to_string()]);
            }
        }
        None => {
            a.extend(["--version".into(), version.to_string()]);
            if let Some(dir) = exe.parent() {
                a.extend(["--dir".into(), dir.display().to_string()]);
            }
            if let Some(g) = gui {
                a.push("--gui".into());
                if let Some(dir) = g.parent().filter(|_| g.extension().is_some_and(|e| e == "app")) {
                    a.extend(["--app-dir".into(), dir.display().to_string()]);
                }
            }
        }
    }
    if dry_run {
        a.push("--dry-run".into());
    }
    a
}

/// The desktop app beside this warden, if installed: warden-gui in its folder (Linux), or
/// Warden.app in /Applications or ~/Applications (macOS).
fn installed_gui(exe: &Path) -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let dirs = [Some(PathBuf::from("/Applications")), home.map(|h| h.join("Applications"))];
        return dirs.into_iter().flatten().map(|d| d.join("Warden.app")).find(|a| a.is_dir());
    }
    exe.parent().map(|d| d.join("warden-gui")).filter(|g| g.is_file())
}

/// An app's supervisor or wardend answers: there is something to restart.
async fn anything_running(args: &Args) -> bool {
    crate::daemon::client::hello_pid(&crate::daemon::socket_path()).await.is_some() || crate::fleet::any_reachable(args)
}

pub async fn run(o: &Opts, args: &Args) -> i32 {
    if o.help {
        print!("{HELP}");
        return 0;
    }
    let current = env!("CARGO_PKG_VERSION");
    let target = match &o.version {
        Some(v) => {
            if v.starts_with('v') {
                v.clone()
            } else {
                format!("v{v}")
            }
        }
        None => match latest() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warden: {e}");
                return 1;
            }
        },
    };
    let newer = Version::parse(&target) > Version::parse(current);
    if o.check {
        if o.json {
            println!(
                "{}",
                serde_json::json!({ "current": current, "latest": target.trim_start_matches('v'), "update": newer })
            );
        } else if newer {
            println!(
                "warden {} is out (this is {current}): `warden upgrade` installs it and restarts onto it",
                target.trim_start_matches('v')
            );
        } else {
            println!("warden {current} is the newest release");
        }
        return 0;
    }
    if !newer && o.version.is_none() {
        println!("warden {current} is the newest release: nothing to do");
        return 0;
    }
    let exe = match std::env::current_exe() {
        Ok(e) => std::fs::canonicalize(&e).unwrap_or(e),
        Err(e) => {
            eprintln!("warden: cannot tell where this warden is: {e}");
            return 1;
        }
    };
    if cfg!(target_os = "linux") && from_package(&exe) {
        let v = target.trim_start_matches('v');
        eprintln!(
            "warden: this warden came from a package (/usr/bin/warden): upgrade it the same way, with the \
             package of release {v} from https://github.com/oceanwap/warden/releases/tag/{target} (sudo apt \
             install ./warden_{v}-1_….deb, or sudo dnf install ./warden-{v}-1.….rpm), then `warden update`"
        );
        return 1;
    }
    let gui = installed_gui(&exe);
    let script = script_args(&target, &exe, gui.as_deref(), o.dry_run);
    if !o.yes && !o.dry_run && crate::sys::isatty(0) {
        eprint!(
            "Install warden {} (this is {current}) and restart every app's supervisor and wardend on it? The apps \
             stop for a few seconds. [y/N] ",
            target.trim_start_matches('v')
        );
        let mut answer = String::new();
        let _ = std::io::stdin().read_line(&mut answer);
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            eprintln!("nothing installed");
            return 1;
        }
    }
    println!("upgrade: installing warden {} over {}", target.trim_start_matches('v'), exe.display());
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-s").arg("--").args(&script).stdout(Stdio::inherit()).stderr(Stdio::inherit());
    // install.sh would tell to run `warden update`: this does it next.
    cmd.env("WARDEN_RESTARTS_ITSELF", "1");
    match pipe_script(cmd, SCRIPT) {
        Ok(out) if out.status.success() => {}
        Ok(out) => {
            eprintln!("warden: the install failed, so nothing was restarted: this is still warden {current}");
            return out.status.code().unwrap_or(1);
        }
        Err(e) => {
            eprintln!("warden: upgrade: cannot run sh: {e}");
            return 1;
        }
    }
    if o.dry_run {
        println!("upgrade: would then restart every supervisor and wardend on it (`warden update --yes`)");
        return 0;
    }
    if !anything_running(args).await {
        println!("upgrade: done; nothing was running, so nothing was restarted");
        return 0;
    }
    // The new warden restarts everything onto itself: this process is the old one.
    println!("upgrade: restarting every supervisor and wardend on warden {}", target.trim_start_matches('v'));
    let mut update = std::process::Command::new(&exe);
    update.args(["update", "--yes"]);
    if let Some(c) = &args.config {
        update.arg("--config").arg(c);
    }
    match update.status() {
        Ok(st) if st.success() => {
            println!("upgrade: done: everything runs on warden {}", target.trim_start_matches('v'));
            0
        }
        Ok(st) => {
            eprintln!(
                "warden: warden {} is installed, but restarting onto it failed; `warden resurrect` starts the \
                 saved apps",
                target.trim_start_matches('v')
            );
            st.code().unwrap_or(1)
        }
        Err(e) => {
            eprintln!("warden: cannot run the new warden ({}): {e}", exe.display());
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(line: &str) -> Result<Opts, String> {
        let argv: Vec<String> = line.split_whitespace().map(String::from).collect();
        match parse_args(&argv)?.command {
            Command::Upgrade(o) => Ok(*o),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_options_are_parsed() {
        assert_eq!(opts("").unwrap(), Opts::default());
        let o = opts("--check --json --yes --dry-run --version v0.2.0").unwrap();
        assert!(o.check && o.json && o.yes && o.dry_run);
        assert_eq!(o.version.as_deref(), Some("v0.2.0"));
        assert_eq!(opts("--version=0.3.1").unwrap().version.as_deref(), Some("0.3.1"));
        assert!(opts("--version nope").unwrap_err().contains("not a version"));
        assert!(opts("--version").unwrap_err().contains("needs a value"));
        assert!(opts("--now").unwrap_err().contains("unknown option"));
        assert!(opts("now").unwrap_err().contains("unexpected argument"));
        let argv = vec!["upgrade".to_string(), "--check".to_string()];
        assert!(matches!(crate::cli::parse(&argv).unwrap().command, Command::Upgrade(o) if o.check));
    }

    #[test]
    fn versions_order_as_semver_does() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("v0.1.10") > v("0.1.9"));
        assert!(v("1.0.0") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.10") > v("1.0.0-rc.2"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-beta"));
        assert_eq!(v("v0.2.0"), v("0.2.0"));
        for bad in ["", "1.2", "1.2.3.4", "x.1.2", "1.2.3-", "latest"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_newest_tag_is_read_from_githubs_answer() {
        assert_eq!(tag_of(r#"{"tag_name":"v0.2.0","name":"Warden 0.2.0"}"#).as_deref(), Some("v0.2.0"));
        assert_eq!(tag_of(r#"{"tag_name":"0.2.0"}"#).as_deref(), Some("v0.2.0"));
        assert_eq!(tag_of(r#"{"message":"Not Found"}"#), None);
        assert_eq!(tag_of(r#"{"tag_name":"nightly"}"#), None);
        assert_eq!(tag_of("<html>"), None);
    }

    #[test]
    fn install_sh_puts_the_release_where_this_warden_is() {
        let exe = Path::new("/home/me/.local/bin/warden");
        assert_eq!(script_args("v0.2.0", exe, None, false), ["--version", "v0.2.0", "--dir", "/home/me/.local/bin"]);
        let gui = Path::new("/home/me/.local/bin/warden-gui");
        assert_eq!(
            script_args("v0.2.0", exe, Some(gui), true),
            ["--version", "v0.2.0", "--dir", "/home/me/.local/bin", "--gui", "--dry-run"]
        );
        let app = Path::new("/Users/me/Applications/Warden.app");
        assert_eq!(
            script_args("v0.2.0", Path::new("/usr/local/bin/warden"), Some(app), false),
            ["--version", "v0.2.0", "--dir", "/usr/local/bin", "--gui", "--app-dir", "/Users/me/Applications"]
        );
        // The warden inside the app: the app is upgraded, and it with it.
        let inside = Path::new("/Applications/Warden.app/Contents/MacOS/warden");
        assert_eq!(
            script_args("v0.2.0", inside, Some(app), false),
            ["--gui-only", "--version", "v0.2.0", "--app-dir", "/Applications"]
        );
    }
}

//! `warden gui-install`: the desktop app of this warden's own release, put where it belongs, with
//! the install.sh built into this binary (`install.sh --gui-only`): Warden.app on macOS (from the
//! release's zip or disk image), warden-gui with a menu entry and an icon on Linux, next to this
//! warden. Everything is checked against the release's SHA256SUMS, as `curl … | sh` would; the
//! CLI stays as it is (it is the one running).
//!
//! A warden that a Linux package installed (/usr/bin/warden) is pointed at the warden-gui package
//! instead, which holds this CLI too and takes the warden package's place (docs/packages.md):
//! files of a package are the package manager's to change.

use crate::cli::{Args, Command};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The installer, as in the repository (and in every release).
pub const SCRIPT: &str = include_str!("../install.sh");

pub const HELP: &str = "\
warden gui-install - install the desktop app of this warden's release

USAGE:
    warden gui-install [--dry-run] [--uninstall] [--version V] [--dir DIR] [--app-dir DIR]

Downloads the GUI of the release this warden is from, checks it against the
release's SHA256SUMS, and installs it: on macOS Warden.app, in /Applications (or
~/Applications); on Linux warden-gui next to this warden, with a menu entry and
an icon. This warden stays as it is. What runs is the install.sh built into
warden (`install.sh --gui-only`), so its variables work too: WARDEN_DOWNLOAD_URL
(a mirror), WARDEN_APP_DIR, XDG_DATA_HOME.

OPTIONS:
    --dry-run        Say what would happen; install nothing
    --uninstall      Remove the GUI (this warden stays)
    --version V      Another release's GUI (v0.2.0); best kept the same as the CLI's
    --dir DIR        Linux: the folder for warden-gui (default: this warden's)
    --app-dir DIR    macOS: the folder for Warden.app
    -h, --help       This help

A warden from a Linux package (/usr/bin/warden) gets the GUI from the warden-gui
package instead: it has this CLI too, and takes the warden package's place.
";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Opts {
    pub dry_run: bool,
    pub uninstall: bool,
    /// The release, as install.sh takes it (`v0.2.0` or `0.2.0`); `None`: this warden's.
    pub version: Option<String>,
    pub dir: Option<PathBuf>,
    pub app_dir: Option<PathBuf>,
    pub help: bool,
}

/// `warden gui-install [OPTIONS]`.
pub fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut o = Opts::default();
    let mut it = argv.iter();
    while let Some(a) = it.next() {
        let mut value =
            || it.next().filter(|v| !v.is_empty()).cloned().ok_or_else(|| format!("gui-install: {a} needs a value"));
        match a.as_str() {
            "--dry-run" => o.dry_run = true,
            "--uninstall" => o.uninstall = true,
            "--version" => o.version = Some(value()?),
            "--dir" | "--prefix" => o.dir = Some(value()?.into()),
            "--app-dir" => o.app_dir = Some(value()?.into()),
            "-h" | "--help" => o.help = true,
            s if s.starts_with("--version=") && s.len() > "--version=".len() => {
                o.version = Some(s["--version=".len()..].to_string())
            }
            s if s.starts_with("--dir=") && s.len() > "--dir=".len() => o.dir = Some(s["--dir=".len()..].into()),
            s if s.starts_with("--app-dir=") && s.len() > "--app-dir=".len() => {
                o.app_dir = Some(s["--app-dir=".len()..].into())
            }
            s if s.starts_with('-') => {
                return Err(format!("gui-install: unknown option {s} (warden gui-install --help)"));
            }
            s => return Err(format!("gui-install: unexpected argument {s:?} (warden gui-install --help)")),
        }
    }
    Ok(Args { command: Command::GuiInstall(Box::new(o)), ..crate::cli::empty_args() })
}

/// What install.sh is run with: the GUI alone, of this warden's release, next to this warden.
pub fn script_args(o: &Opts, exe: Option<&Path>) -> Vec<String> {
    let mut a = vec!["--gui-only".to_string(), "--version".to_string()];
    a.push(o.version.clone().unwrap_or_else(|| format!("v{}", env!("CARGO_PKG_VERSION"))));
    // Linux: warden-gui goes beside this warden (and is found there by the GUI). macOS: no
    // program goes in that folder; install.sh only looks there for the CLI the app will meet.
    if let Some(dir) = o.dir.as_deref().or_else(|| exe.and_then(Path::parent)) {
        a.push("--dir".into());
        a.push(dir.display().to_string());
    }
    if let Some(d) = &o.app_dir {
        a.push("--app-dir".into());
        a.push(d.display().to_string());
    }
    if o.dry_run {
        a.push("--dry-run".into());
    }
    if o.uninstall {
        a.push("--uninstall".into());
    }
    a
}

/// A warden that a Linux package installed: the packages put it in /usr/bin, install.sh never does
/// (it uses /usr/local/bin as root, ~/.local/bin otherwise, or the folder it is told).
pub fn from_package(exe: &Path) -> bool {
    exe == Path::new("/usr/bin/warden")
}

/// The app this warden is the command line tool of (`…/Warden.app/Contents/MacOS/warden`, which is
/// what the app's "Install command line tool" links to).
pub fn in_app(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    let named = |p: &Path, n: &str| p.file_name().is_some_and(|f| f == n);
    (named(macos, "MacOS")
        && named(contents, "Contents")
        && app.extension().is_some_and(|e| e.eq_ignore_ascii_case("app")))
    .then_some(app)
}

/// What to say to a warden from a Linux package: the package of the GUI, for this machine.
pub fn package_hint(uninstall: bool) -> String {
    let v = env!("CARGO_PKG_VERSION");
    if uninstall {
        return "warden: this warden came from a package (/usr/bin/warden), and so does the GUI: remove the \
                warden-gui package (sudo apt remove warden-gui, or sudo dnf remove warden-gui). It takes this CLI \
                with it; install the warden package to keep the CLI alone."
            .to_string();
    }
    let (deb, rpm) = match std::env::consts::ARCH {
        "aarch64" => ("arm64", "aarch64"),
        _ => ("amd64", "x86_64"),
    };
    format!(
        "warden: this warden came from a package (/usr/bin/warden): install the GUI the same way. The warden-gui \
         package has the GUI and this CLI, and takes the warden package's place:\n\n    \
         https://github.com/oceanwap/warden/releases/tag/v{v}\n    \
         sudo apt install ./warden-gui_{v}-1_{deb}.deb       (Debian, Ubuntu)\n    \
         sudo dnf install ./warden-gui-{v}-1.{rpm}.rpm      (Fedora, RHEL)\n\n\
         Or the GUI alone, beside a warden of your own: warden gui-install --dir ~/.local/bin"
    )
}

/// `sh -s -- ARGS`, the script on its standard input (as `curl … | sh -s -- ARGS` runs it: it
/// reads nothing else from there), its output where `cmd` says.
pub fn pipe_script(mut cmd: std::process::Command, script: &str) -> std::io::Result<std::process::Output> {
    let mut child = cmd.stdin(Stdio::piped()).spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        // A shell that stops reading early (it failed) is no error of ours: its status says it.
        let _ = stdin.write_all(script.as_bytes());
    }
    child.wait_with_output()
}

fn sh(args: &[String]) -> std::process::Command {
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-s").arg("--").args(args);
    cmd
}

pub fn run(o: &Opts) -> i32 {
    if o.help {
        print!("{HELP}");
        return 0;
    }
    let exe = std::env::current_exe().ok().map(|e| std::fs::canonicalize(&e).unwrap_or(e));
    if let Some(exe) = exe.as_deref() {
        if cfg!(target_os = "linux") && o.dir.is_none() && from_package(exe) {
            eprintln!("{}", package_hint(o.uninstall));
            return 1;
        }
        if let Some(app) = in_app(exe).filter(|_| !o.uninstall && o.version.is_none()) {
            println!(
                "warden: the desktop app is installed already: this warden is the command line tool of {}",
                app.display()
            );
            return 0;
        }
    }
    // The CLI inside an app is replaced with the app: install.sh is not pointed at it.
    let place = exe.as_deref().filter(|e| in_app(e).is_none());
    let args = script_args(o, place);
    let mut cmd = sh(&args);
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    match pipe_script(cmd, SCRIPT) {
        Ok(out) => out.status.code().unwrap_or(1),
        Err(e) => {
            eprintln!("warden: gui-install: cannot run sh: {e}");
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
            Command::GuiInstall(o) => Ok(*o),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_options_are_parsed_and_anything_else_is_refused() {
        assert_eq!(opts("").unwrap(), Opts::default());
        let o = opts("--dry-run --uninstall --version v0.2.0 --dir /opt/w --app-dir /Apps").unwrap();
        assert!(o.dry_run && o.uninstall);
        assert_eq!(o.version.as_deref(), Some("v0.2.0"));
        assert_eq!(o.dir.as_deref(), Some(Path::new("/opt/w")));
        assert_eq!(o.app_dir.as_deref(), Some(Path::new("/Apps")));
        let o = opts("--version=0.2.0 --dir=/d --app-dir=/a").unwrap();
        assert_eq!((o.version.as_deref(), o.dir, o.app_dir), (Some("0.2.0"), Some("/d".into()), Some("/a".into())));
        assert_eq!(opts("--prefix /p").unwrap().dir.as_deref(), Some(Path::new("/p")));
        assert!(opts("-h").unwrap().help && opts("--help").unwrap().help);
        assert!(opts("--version").unwrap_err().contains("--version needs a value"));
        assert!(opts("--dir=").unwrap_err().contains("unknown option --dir="));
        assert!(opts("--gui").unwrap_err().contains("unknown option --gui"));
        assert!(opts("now").unwrap_err().contains("unexpected argument \"now\""));
        // Through the main parser too.
        let argv = vec!["gui-install".to_string(), "--dry-run".to_string()];
        assert!(matches!(crate::cli::parse(&argv).unwrap().command, Command::GuiInstall(o) if o.dry_run));
    }

    #[test]
    fn install_sh_gets_the_gui_alone_of_this_release_next_to_this_warden() {
        let v = format!("v{}", env!("CARGO_PKG_VERSION"));
        let exe = Path::new("/home/me/.local/bin/warden");
        assert_eq!(
            script_args(&Opts::default(), Some(exe)),
            ["--gui-only", "--version", v.as_str(), "--dir", "/home/me/.local/bin"]
        );
        // Without a known place, install.sh picks its own default.
        assert_eq!(script_args(&Opts::default(), None), ["--gui-only", "--version", v.as_str()]);
        let o = Opts {
            dry_run: true,
            uninstall: true,
            version: Some("0.2.0".into()),
            dir: Some("/opt/bin".into()),
            app_dir: Some("/Users/me/Applications".into()),
            help: false,
        };
        assert_eq!(
            script_args(&o, Some(exe)),
            [
                "--gui-only",
                "--version",
                "0.2.0",
                "--dir",
                "/opt/bin",
                "--app-dir",
                "/Users/me/Applications",
                "--dry-run",
                "--uninstall"
            ]
        );
    }

    #[test]
    fn a_package_warden_and_an_app_s_warden_are_told_apart_from_one_of_install_sh() {
        assert!(from_package(Path::new("/usr/bin/warden")));
        for p in ["/usr/local/bin/warden", "/home/me/.local/bin/warden", "/usr/bin/warden-gui", "/opt/usr/bin/warden"] {
            assert!(!from_package(Path::new(p)), "{p}");
        }
        assert_eq!(
            in_app(Path::new("/Applications/Warden.app/Contents/MacOS/warden")),
            Some(Path::new("/Applications/Warden.app"))
        );
        assert_eq!(
            in_app(Path::new("/Users/me/Apps/Warden 2.APP/Contents/MacOS/warden")),
            Some(Path::new("/Users/me/Apps/Warden 2.APP"))
        );
        for p in ["/usr/local/bin/warden", "/x/Contents/MacOS/warden", "/a/Warden.app/MacOS/warden", "warden"] {
            assert_eq!(in_app(Path::new(p)), None, "{p}");
        }
        let v = env!("CARGO_PKG_VERSION");
        let hint = package_hint(false);
        assert!(hint.contains(&format!("releases/tag/v{v}")), "{hint}");
        assert!(hint.contains(&format!("./warden-gui_{v}-1_")) && hint.contains(&format!("./warden-gui-{v}-1.")));
        assert!(package_hint(true).contains("apt remove warden-gui"));
    }

    #[test]
    fn the_script_is_install_sh_and_runs_through_a_pipe() {
        assert!(SCRIPT.starts_with("#!/bin/sh\n"));
        assert!(SCRIPT.trim_end().ends_with("main \"$@\" --end-of-install-script"));
        assert!(SCRIPT.contains("--gui-only)"), "install.sh has --gui-only");
        // Its help, through the same pipe as a real run.
        let mut cmd = sh(&["--help".to_string()]);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        let out = pipe_script(cmd, SCRIPT).unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{text}{}", String::from_utf8_lossy(&out.stderr));
        assert!(text.contains("Usage: install.sh") && text.contains("--gui-only"), "{text}");
        // A script cut short runs nothing, and says so.
        let cmd = {
            let mut c = sh(&[]);
            c.stdout(Stdio::piped()).stderr(Stdio::piped());
            c
        };
        let out = pipe_script(cmd, &SCRIPT[..SCRIPT.len() - 3]).unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("incomplete"));
    }
}

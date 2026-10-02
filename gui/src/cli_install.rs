//! "Install command line tool": a link named `warden` in a folder Terminal looks in, pointing at the
//! `warden` CLI that sits next to the GUI (`Warden.app/Contents/MacOS/warden` on macOS, the unpacked
//! download elsewhere), so `warden list` works without installing anything else.
//!
//! Where the link goes, in order:
//! 1. `/usr/local/bin/warden` (macOS), made as the user when that folder lets them;
//! 2. the same through the system's administrator prompt (`osascript`: `do shell script ... with
//!    administrator privileges`) when it does not;
//! 3. `~/.local/bin/warden`, with the line that puts that folder on PATH (always on Linux, and on a
//!    Mac when the prompt is cancelled or fails).
//!
//! Only links are made and removed, and only ones that point into the app: a `warden` that is a
//! file, or a link to something else (Homebrew, install.sh, a package), is never replaced or
//! deleted. Every folder is a field of [`Places`], so the tests run on temporary ones; only
//! [`Osascript`] (the prompt) is macOS's, and the tests check the command it runs, not the prompt.

use crate::commands;
use crate::ssh::shell_quote;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// The command's name, and the link's.
pub const NAME: &str = "warden";

/// The system folder a link goes in first (macOS).
pub const SYSTEM_DIR: &str = "/usr/local/bin";

/// Where everything is: the program, the CLI beside it, and the folders to link into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Places {
    /// This program (`warden-gui`), as the system names it.
    pub exe: PathBuf,
    /// The `warden` CLI beside it: what the link points at.
    pub cli: PathBuf,
    /// A folder every user's Terminal has on PATH, that may need the administrator (macOS).
    pub system_dir: Option<PathBuf>,
    /// `~/.local/bin`: no administrator needed.
    pub user_dir: Option<PathBuf>,
    /// Every folder where a `warden` counts as installed: PATH, and the usual bin folders that a
    /// window opened from Finder or a menu has not got on its own PATH.
    pub search: Vec<PathBuf>,
    /// This program's PATH, to tell whether `user_dir` is on it.
    pub path: Vec<PathBuf>,
    /// `$SHELL` and `$HOME`, for the line that puts `user_dir` on PATH.
    pub shell: Option<String>,
    pub home: Option<PathBuf>,
    pub mac: bool,
}

impl Places {
    /// This machine: the running program, its CLI, the environment. Err says why a link cannot be
    /// made from this window (no CLI beside it, or it runs from a place that does not stay).
    pub fn here() -> Result<Places, String> {
        let exe = std::env::current_exe().map_err(|e| format!("cannot tell where this program is: {e}"))?;
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).map(PathBuf::from);
        let path: Vec<PathBuf> =
            std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
        let mac = cfg!(target_os = "macos");
        let user_dir = home.as_ref().map(|h| h.join(".local").join("bin"));
        // A window opened from Finder or a menu has launchd's short PATH: look in the usual places too.
        let mut search = path.clone();
        let mut usual = vec![PathBuf::from(SYSTEM_DIR), PathBuf::from("/opt/homebrew/bin")];
        usual.extend(user_dir.clone());
        usual.extend(home.as_ref().map(|h| h.join(".cargo").join("bin")));
        for d in usual {
            if !search.contains(&d) {
                search.push(d);
            }
        }
        let cli = exe.parent().map(|d| d.join(NAME)).unwrap_or_else(|| PathBuf::from(NAME));
        let places = Places {
            exe,
            cli,
            system_dir: mac.then(|| PathBuf::from(SYSTEM_DIR)),
            user_dir,
            search,
            path,
            shell: std::env::var("SHELL").ok(),
            home,
            mac,
        };
        places.usable()?;
        Ok(places)
    }

    /// The CLI is there, and the program is somewhere a link to it keeps working.
    pub fn usable(&self) -> Result<(), String> {
        if !is_executable(&self.cli) {
            return Err(format!(
                "There is no `warden` next to this program ({}), so there is nothing to link. Install it with \
                 install.sh, or from the GUI + CLI download.",
                self.cli.parent().map_or_else(|| "?".to_string(), |d| d.display().to_string())
            ));
        }
        if self.mac {
            stays_put(&self.exe)?;
        }
        Ok(())
    }

    /// The window runs from an app bundle (`X.app/Contents/MacOS/<program>`).
    pub fn in_bundle(&self) -> bool {
        in_bundle(&self.exe)
    }

    /// Where a first link goes, for the sentence that says so: `/usr/local/bin/warden` or `~/.local/bin/warden`.
    pub fn first_choice(&self) -> Option<PathBuf> {
        self.system_dir.as_ref().or(self.user_dir.as_ref()).map(|d| d.join(NAME))
    }

    /// The line that puts `dir` on PATH, when `dir` is `user_dir` and PATH does not have it.
    pub fn path_hint(&self, link: &Path) -> Option<Hint> {
        let dir = link.parent()?;
        (self.user_dir.as_deref() == Some(dir) && !self.path.iter().any(|d| d == dir))
            .then(|| Hint::new(dir, self.home.as_deref(), self.shell.as_deref(), self.mac))
    }
}

/// `X.app/Contents/MacOS/<program>`.
pub fn in_bundle(exe: &Path) -> bool {
    let name = |p: Option<&Path>| p.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned());
    let macos = exe.parent();
    name(macos).as_deref() == Some("MacOS")
        && name(macos.and_then(Path::parent)).as_deref() == Some("Contents")
        && name(macos.and_then(Path::parent).and_then(Path::parent)).is_some_and(|n| n.ends_with(".app"))
}

/// macOS runs an app opened from a downloaded disk image, or from Downloads before it was moved,
/// from a place that goes away (a volume that is ejected, a randomized "App Translocation" folder):
/// a link to the CLI there would be dead the next time. Err says what to do.
pub fn stays_put(exe: &Path) -> Result<(), String> {
    if exe.components().any(|c| c.as_os_str() == "AppTranslocation") {
        return Err("Warden runs from a temporary place macOS made for it (it was opened before being moved), and \
                    a link to it would stop working. Move Warden.app to Applications, open it from there, then \
                    install the command line tool."
            .into());
    }
    if exe.starts_with("/Volumes") {
        return Err("Warden runs from a disk image or another volume, which may go away, and a link to it would \
                    stop working. Drag Warden.app to Applications, eject the disk image, open Warden from \
                    Applications, then install the command line tool."
            .into());
    }
    Ok(())
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

// ------------------------------------------------------------------ status

/// What `warden` is on this machine, as Settings says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Not in a place Terminal looks, and not linked by this window.
    Missing,
    /// A link of this app's: `link` points at `target`, which is there.
    Linked { link: PathBuf, target: PathBuf },
    /// A link that points into an app, to a CLI that is not there any more (the app moved).
    Broken { link: PathBuf, target: PathBuf },
    /// A `warden` that is not a link of this app's (a package, install.sh, Homebrew, or a copy):
    /// it is left alone.
    Present { path: PathBuf },
}

/// The links this window makes are found by what they point at.
fn points_into_app(target: &Path, cli: &Path) -> bool {
    target == cli || same_file(target, cli) || in_an_app(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// `…/Warden.app/Contents/MacOS/warden`: this app wherever it was, or was before it moved.
fn in_an_app(target: &Path) -> bool {
    let last: Vec<Component<'_>> = target.components().rev().take(4).collect();
    let [n, macos, contents, app] = last.as_slice() else { return false };
    n.as_os_str() == NAME
        && macos.as_os_str() == "MacOS"
        && contents.as_os_str() == "Contents"
        && app.as_os_str().to_string_lossy().eq_ignore_ascii_case("Warden.app")
}

/// What is at `link`, when it is a symlink: where it points (relative links resolved).
fn target_of(link: &Path) -> Option<PathBuf> {
    let meta = std::fs::symlink_metadata(link).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let t = std::fs::read_link(link).ok()?;
    Some(link.parent().map_or_else(|| t.clone(), |d| d.join(&t)))
}

/// The state of the links in the two folders we link into, then of a `warden` anywhere else Terminal
/// looks.
pub fn status(p: &Places) -> Status {
    let mut broken = None;
    for dir in p.system_dir.iter().chain(p.user_dir.iter()) {
        let link = dir.join(NAME);
        match target_of(&link) {
            Some(target) if points_into_app(&target, &p.cli) => {
                if target.exists() {
                    return Status::Linked { link, target };
                }
                broken.get_or_insert(Status::Broken { link, target });
            }
            // A live link to something else, or a file: not ours (a dead link to nothing we know
            // is nobody's).
            Some(target) if target.exists() => return Status::Present { path: link },
            Some(_) => {}
            None if std::fs::symlink_metadata(&link).is_ok() => return Status::Present { path: link },
            None => {}
        }
    }
    if let Some(b) = broken {
        return b;
    }
    p.search
        .iter()
        .map(|d| d.join(NAME))
        .find(|f| is_executable(f))
        .map_or(Status::Missing, |path| Status::Present { path })
}

// -------------------------------------------------------------- PATH hint

/// How to put a folder on PATH, the way install.sh says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint {
    /// The shell's name (zsh, bash, fish, or sh).
    pub shell: String,
    /// The line for the shell's startup file: `export PATH="$HOME/.local/bin:$PATH"`.
    pub line: String,
    /// The startup file, shortened (`~/.zshrc`); fish has none (its command is saved by fish).
    pub file: Option<String>,
}

impl Hint {
    pub fn new(dir: &Path, home: Option<&Path>, shell: Option<&str>, mac: bool) -> Hint {
        let shown = match home.and_then(|h| dir.strip_prefix(h).ok()) {
            Some(rest) => format!("$HOME/{}", rest.display()),
            None => dir.display().to_string(),
        };
        let shell =
            shell.and_then(|s| s.rsplit('/').next()).filter(|s| matches!(*s, "bash" | "zsh" | "fish")).unwrap_or("sh");
        let (line, file) = match shell {
            "fish" => (format!("fish_add_path \"{shown}\""), None),
            "zsh" => (format!("export PATH=\"{shown}:$PATH\""), Some("~/.zshrc")),
            "bash" if mac => (format!("export PATH=\"{shown}:$PATH\""), Some("~/.bash_profile")),
            "bash" => (format!("export PATH=\"{shown}:$PATH\""), Some("~/.bashrc")),
            _ => (format!("export PATH=\"{shown}:$PATH\""), Some("~/.profile")),
        };
        Hint { shell: shell.to_string(), line, file: file.map(String::from) }
    }

    /// One command that does it: appends the line to the startup file (fish: the command itself).
    pub fn command(&self) -> String {
        match &self.file {
            Some(f) => format!("echo '{}' >> {f}", self.line),
            None => self.line.clone(),
        }
    }
}

// ----------------------------------------------------------------- linking

/// Why a link could not be made here.
#[derive(Debug, PartialEq, Eq)]
enum LinkError {
    /// The folder does not let this user in: the administrator can.
    Denied,
    /// Something is in the way, or the disk said no: the words to show.
    Other(String),
}

fn classify(e: &std::io::Error, what: &Path) -> LinkError {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::ReadOnlyFilesystem => LinkError::Denied,
        _ => LinkError::Other(format!("{}: {e}", what.display())),
    }
}

/// Make `dir/warden` a link to `cli`, replacing a link of ours or a dead one (never a file, never a
/// live link to something else). Made beside it and renamed over it, so there is no moment without a
/// `warden` for a Terminal that is looking.
fn link_into(dir: &Path, cli: &Path) -> Result<PathBuf, LinkError> {
    std::fs::create_dir_all(dir).map_err(|e| classify(&e, dir))?;
    let link = dir.join(NAME);
    match std::fs::symlink_metadata(&link) {
        Ok(m) if m.file_type().is_symlink() => {
            let target = target_of(&link).unwrap_or_default();
            if target.exists() && !points_into_app(&target, cli) {
                return Err(LinkError::Other(format!(
                    "{} already points to {}: it is left alone",
                    link.display(),
                    target.display()
                )));
            }
        }
        Ok(_) => {
            return Err(LinkError::Other(format!("{} is a file, not a link: it is left alone", link.display())));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(classify(&e, &link)),
    }
    let tmp = dir.join(format!(".{NAME}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(cli, &tmp).map_err(|e| classify(&e, &link))?;
    std::fs::rename(&tmp, &link).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        classify(&e, &link)
    })?;
    Ok(link)
}

// ------------------------------------------------------------ administrator

/// What the administrator prompt came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Elevated {
    Done,
    /// The person closed the prompt.
    Cancelled,
    Failed(String),
}

/// Runs a shell command line as the administrator.
pub trait Elevate {
    fn run(&self, script: &str) -> impl Future<Output = Elevated> + Send;
}

/// macOS's prompt: `osascript -e 'do shell script "…" with administrator privileges'`.
pub struct Osascript;

const OSASCRIPT: &str = "/usr/bin/osascript";
/// The person may take a while to find the password.
const PROMPT_LIMIT: Duration = Duration::from_secs(300);

impl Elevate for Osascript {
    async fn run(&self, script: &str) -> Elevated {
        let args = match osascript_args(script) {
            Ok(a) => a,
            Err(e) => return Elevated::Failed(e),
        };
        match commands::run(Path::new(OSASCRIPT), &args, None, &[], PROMPT_LIMIT).await {
            Ok(out) if out.ok => Elevated::Done,
            Ok(out) if cancelled(&out.stderr) => Elevated::Cancelled,
            Ok(out) => Elevated::Failed(out.text()),
            Err(e) => Elevated::Failed(e),
        }
    }
}

/// The arguments of `osascript` that run `script` (a POSIX shell command line) as the administrator.
/// The script is an AppleScript string: `\` and `"` are escaped, and a control character (a newline
/// in a path) is refused rather than guessed at.
pub fn osascript_args(script: &str) -> Result<Vec<String>, String> {
    if script.chars().any(char::is_control) {
        return Err("a path with a control character in it cannot be passed to the administrator prompt".into());
    }
    let escaped = script.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(vec!["-e".into(), format!("do shell script \"{escaped}\" with administrator privileges")])
}

/// osascript says `execution error: User canceled. (-128)` when the prompt is closed.
pub fn cancelled(stderr: &str) -> bool {
    stderr.contains("(-128)") || stderr.to_lowercase().contains("user canceled")
}

/// What the administrator runs to make the link: the folder, then the link, unless something that
/// is not a link is in the way (checked again here, as root).
pub fn admin_install_script(dir: &Path, cli: &Path) -> String {
    let (d, l, c) = (q(dir), q(&dir.join(NAME)), q(cli));
    format!("/bin/mkdir -p {d} && {{ [ ! -e {l} ] || [ -L {l} ]; }} && /bin/ln -sfn {c} {l}")
}

/// What the administrator runs to remove a link: only a link.
pub fn admin_remove_script(link: &Path) -> String {
    let l = q(link);
    format!("if [ -L {l} ]; then /bin/rm -- {l}; fi")
}

fn q(p: &Path) -> String {
    shell_quote(&p.to_string_lossy())
}

// ------------------------------------------------------------- the actions

/// What installing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub link: PathBuf,
    /// The administrator made it.
    pub admin: bool,
    /// Why the first choice was not used, when it was not.
    pub note: Option<String>,
    /// The line that puts the link's folder on PATH, when it is needed.
    pub hint: Option<Hint>,
}

impl Installed {
    /// The sentence the window shows.
    pub fn summary(&self, cli: &Path) -> String {
        let mut s = format!("Installed {} (a link to {}).", self.link.display(), cli.display());
        if let Some(n) = &self.note {
            s = format!("{n} {s}");
        }
        if self.hint.is_some() {
            s += " If a new Terminal does not find `warden`, put that folder on PATH: Settings has the line.";
        } else {
            s += " Open a new Terminal and run `warden list`.";
        }
        s
    }
}

/// What uninstalling did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Removed {
    pub link: PathBuf,
    pub admin: bool,
}

/// Makes `dir/warden` a link to the CLI (the real one is [`link_into`]; the tests put others in its place).
type Linker<'a> = &'a (dyn Fn(&Path, &Path) -> Result<PathBuf, LinkError> + Sync);
/// Removes a link.
type Remover<'a> = &'a (dyn Fn(&Path) -> std::io::Result<()> + Sync);

/// Link `warden` where Terminal finds it (the order is in the module's documentation).
pub async fn install(p: Places) -> Result<Installed, String> {
    install_with(&p, &Osascript).await
}

pub async fn install_with(p: &Places, admin: &impl Elevate) -> Result<Installed, String> {
    install_core(p, admin, &link_into).await
}

async fn install_core(p: &Places, admin: &impl Elevate, link: Linker<'_>) -> Result<Installed, String> {
    p.usable()?;
    match status(p) {
        Status::Present { path } => {
            return Err(format!(
                "there is a `warden` at {} already, and it is not a link to this app: it is left alone",
                path.display()
            ));
        }
        Status::Linked { link, .. } => {
            let hint = p.path_hint(&link);
            return Ok(Installed { link, admin: false, note: Some("It was installed already.".into()), hint });
        }
        Status::Missing | Status::Broken { .. } => {}
    }
    let mut note = None;
    if let Some(dir) = &p.system_dir {
        let wanted = dir.join(NAME);
        match link(dir, &p.cli) {
            Ok(link) => return Ok(Installed { link, admin: false, note: None, hint: None }),
            Err(LinkError::Other(e)) => return Err(e),
            Err(LinkError::Denied) => match admin.run(&admin_install_script(dir, &p.cli)).await {
                Elevated::Done => {
                    return match status(p) {
                        Status::Linked { link, .. } => Ok(Installed { link, admin: true, note: None, hint: None }),
                        _ => Err(format!(
                            "the administrator step ran, but {} is not a link to this app",
                            wanted.display()
                        )),
                    };
                }
                Elevated::Cancelled => {
                    note =
                        Some(format!("The administrator prompt was cancelled, so {} was not made.", wanted.display()));
                }
                Elevated::Failed(e) => {
                    note = Some(format!("{} could not be made as the administrator ({e}).", wanted.display()));
                }
            },
        }
    }
    let Some(dir) = &p.user_dir else {
        return Err(note.unwrap_or_else(|| "there is no home folder to link in".into()));
    };
    match link(dir, &p.cli) {
        Ok(link) => {
            let hint = p.path_hint(&link);
            Ok(Installed { link, admin: false, note, hint })
        }
        Err(LinkError::Denied) => Err(format!("{} does not let this user make a link", dir.display())),
        Err(LinkError::Other(e)) => Err(e),
    }
}

/// Remove the link this window made (and only that).
pub async fn uninstall(p: Places) -> Result<Removed, String> {
    uninstall_with(&p, &Osascript).await
}

pub async fn uninstall_with(p: &Places, admin: &impl Elevate) -> Result<Removed, String> {
    uninstall_core(p, admin, &|l| std::fs::remove_file(l)).await
}

async fn uninstall_core(p: &Places, admin: &impl Elevate, remove: Remover<'_>) -> Result<Removed, String> {
    let link = match status(p) {
        Status::Linked { link, .. } | Status::Broken { link, .. } => link,
        Status::Present { path } => {
            return Err(format!("{} is not a link to this app, so it is left alone", path.display()));
        }
        Status::Missing => return Err("there is no link to remove".into()),
    };
    match remove(&link) {
        Ok(()) => Ok(Removed { link, admin: false }),
        Err(e) if classify(&e, &link) == LinkError::Denied => match admin.run(&admin_remove_script(&link)).await {
            Elevated::Done if std::fs::symlink_metadata(&link).is_err() => Ok(Removed { link, admin: true }),
            Elevated::Done => Err(format!("the administrator step ran, but {} is still there", link.display())),
            Elevated::Cancelled => {
                Err(format!("the administrator prompt was cancelled: {} was not removed", link.display()))
            }
            Elevated::Failed(e) => Err(format!("{} could not be removed as the administrator: {e}", link.display())),
        },
        Err(e) => Err(format!("{}: {e}", link.display())),
    }
}

// --------------------------------------------------------------- GUI state

/// What the window is doing about the tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Work {
    Idle,
    Installing,
    Removing,
}

/// The window's view of it: where things are (or why they cannot be linked), what is there, and
/// whether an action runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub places: Result<Places, String>,
    pub status: Status,
    pub work: Work,
}

impl Default for State {
    /// Not looked at: no banner, nothing offered.
    fn default() -> State {
        State { places: Err("not looked at yet".into()), status: Status::Missing, work: Work::Idle }
    }
}

impl State {
    /// Look at this machine.
    pub fn detect() -> State {
        State::of(Places::here())
    }

    pub fn of(places: Result<Places, String>) -> State {
        let status = places.as_ref().map_or(Status::Missing, status);
        State { places, status, work: Work::Idle }
    }

    /// Look at the folders again (after an action).
    pub fn refresh(&mut self) {
        if let Ok(p) = &self.places {
            self.status = status(p);
        }
    }

    /// The first-run banner: from an app bundle, with no `warden` anywhere Terminal looks, and not
    /// turned away before.
    pub fn banner(&self, dismissed: bool) -> bool {
        !dismissed && self.status == Status::Missing && self.places.as_ref().is_ok_and(Places::in_bundle)
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::Mutex;

    /// A scratch folder with a fake `Warden.app` and the folders a Mac has.
    struct Sandbox {
        root: PathBuf,
        places: Places,
    }

    impl Sandbox {
        fn new(name: &str) -> Sandbox {
            let root = std::env::temp_dir().join(format!("wg-cli-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            let macos = root.join("Applications/Warden.app/Contents/MacOS");
            std::fs::create_dir_all(&macos).unwrap();
            for f in ["warden-gui", "warden"] {
                executable(&macos.join(f));
            }
            let home = root.join("home");
            let places = Places {
                exe: macos.join("warden-gui"),
                cli: macos.join("warden"),
                system_dir: Some(root.join("usr/local/bin")),
                user_dir: Some(home.join(".local/bin")),
                search: vec![root.join("usr/local/bin"), home.join(".local/bin"), root.join("opt/bin")],
                path: vec![root.join("usr/bin")],
                shell: Some("/bin/zsh".into()),
                home: Some(home),
                mac: true,
            };
            Sandbox { root, places }
        }

        fn system(&self) -> PathBuf {
            self.places.system_dir.clone().unwrap()
        }

        fn user(&self) -> PathBuf {
            self.places.user_dir.clone().unwrap()
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn executable(f: &Path) {
        std::fs::create_dir_all(f.parent().unwrap()).unwrap();
        std::fs::write(f, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// An administrator that records what it was asked and answers as told. `act` is what it does
    /// to the disk when it says Done (the real one runs the script as root).
    struct Admin {
        asked: Mutex<Vec<String>>,
        answer: Elevated,
        act: Box<dyn Fn() + Send + Sync>,
    }

    impl Admin {
        fn new(answer: Elevated, act: impl Fn() + Send + Sync + 'static) -> Admin {
            Admin { asked: Mutex::new(vec![]), answer, act: Box::new(act) }
        }

        /// One that must not be asked.
        fn never() -> Admin {
            Admin::new(Elevated::Failed("not expected".into()), || {})
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl Elevate for Admin {
        async fn run(&self, script: &str) -> Elevated {
            self.asked.lock().unwrap().push(script.to_string());
            if self.answer == Elevated::Done {
                (self.act)();
            }
            self.answer.clone()
        }
    }

    /// The real linking, except that `denied` (the system folder) says no, the way a folder that
    /// is root's does for a user (the tests may run as root, which no folder says no to).
    fn deny(denied: PathBuf) -> impl Fn(&Path, &Path) -> Result<PathBuf, LinkError> + Sync {
        move |dir, cli| if dir == denied { Err(LinkError::Denied) } else { link_into(dir, cli) }
    }

    fn block_on<F: Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }

    fn install(s: &Sandbox, admin: &Admin) -> Result<Installed, String> {
        block_on(install_with(&s.places, admin))
    }

    fn uninstall(s: &Sandbox, admin: &Admin) -> Result<Removed, String> {
        block_on(uninstall_with(&s.places, admin))
    }

    #[test]
    fn a_bundle_is_found_by_its_layout() {
        assert!(in_bundle(Path::new("/Applications/Warden.app/Contents/MacOS/warden-gui")));
        assert!(in_bundle(Path::new("/Users/me/Applications/Some Name.app/Contents/MacOS/warden-gui")));
        assert!(!in_bundle(Path::new("/home/me/warden-gui-0.1.0-linux-x86_64/warden-gui")));
        assert!(!in_bundle(Path::new("/Applications/Warden.app/Contents/Resources/warden-gui")));
        assert!(!in_bundle(Path::new("/opt/Warden.app/MacOS/warden-gui")));
    }

    #[test]
    fn places_that_go_away_are_refused_with_what_to_do() {
        let t = stays_put(Path::new(
            "/private/var/folders/x9/abc/T/AppTranslocation/6B1F-42/d/Warden.app/Contents/MacOS/warden-gui",
        ));
        assert!(t.unwrap_err().contains("Move Warden.app to Applications"));
        let v = stays_put(Path::new("/Volumes/Warden/Warden.app/Contents/MacOS/warden-gui"));
        assert!(v.unwrap_err().contains("eject the disk image"));
        assert!(stays_put(Path::new("/Applications/Warden.app/Contents/MacOS/warden-gui")).is_ok());
        assert!(stays_put(Path::new("/Users/me/Applications/Warden.app/Contents/MacOS/warden-gui")).is_ok());
        // The same Places refuse to install from such a place.
        let mut s = Sandbox::new("volume");
        s.places.exe = PathBuf::from("/Volumes/Warden/Warden.app/Contents/MacOS/warden-gui");
        assert!(install(&s, &Admin::never()).unwrap_err().contains("eject the disk image"));
        assert!(!s.system().exists(), "nothing was made");
    }

    #[test]
    fn without_a_cli_beside_the_program_there_is_nothing_to_link() {
        let s = Sandbox::new("nocli");
        std::fs::remove_file(&s.places.cli).unwrap();
        let why = s.places.usable().unwrap_err();
        assert!(why.contains("no `warden` next to this program"), "{why}");
        assert!(install(&s, &Admin::never()).is_err());
    }

    #[test]
    fn the_first_link_goes_in_the_system_folder_without_the_administrator_when_the_folder_lets_us() {
        let s = Sandbox::new("plain");
        std::fs::create_dir_all(s.system()).unwrap();
        assert_eq!(status(&s.places), Status::Missing);
        let admin = Admin::never();
        let done = install(&s, &admin).unwrap();
        assert_eq!(done.link, s.system().join("warden"));
        assert!(!done.admin && done.note.is_none() && done.hint.is_none());
        assert!(admin.asked().is_empty(), "no prompt when the folder is ours to write");
        assert_eq!(std::fs::read_link(&done.link).unwrap(), s.places.cli);
        assert_eq!(status(&s.places), Status::Linked { link: done.link.clone(), target: s.places.cli.clone() });
        assert!(is_executable(&done.link), "the link runs the CLI");
        assert!(done.summary(&s.places.cli).contains("run `warden list`"));
    }

    #[test]
    fn a_system_folder_that_does_not_exist_yet_is_made() {
        let s = Sandbox::new("mkdir");
        assert!(!s.system().exists());
        let done = install(&s, &Admin::never()).unwrap();
        assert!(s.system().is_dir() && done.link.exists());
    }

    #[test]
    fn installing_twice_is_one_link_and_says_so() {
        let s = Sandbox::new("twice");
        let first = install(&s, &Admin::never()).unwrap();
        let again = install(&s, &Admin::never()).unwrap();
        assert_eq!(first.link, again.link);
        assert!(again.note.as_deref().is_some_and(|n| n.contains("already")));
        assert_eq!(std::fs::read_dir(s.system()).unwrap().count(), 1, "no stray temporary link");
    }

    #[test]
    fn a_folder_that_says_no_asks_the_administrator_for_the_link() {
        let s = Sandbox::new("admin");
        let (dir, cli) = (s.system(), s.places.cli.clone());
        // What the script does, as root.
        let admin = Admin::new(Elevated::Done, move || {
            std::fs::create_dir_all(&dir).unwrap();
            symlink(&cli, dir.join("warden")).unwrap();
        });
        let done = block_on(install_core(&s.places, &admin, &deny(s.system()))).unwrap();
        assert!(done.admin && done.note.is_none() && done.hint.is_none());
        assert_eq!(done.link, s.system().join("warden"));
        assert_eq!(admin.asked(), vec![admin_install_script(&s.system(), &s.places.cli)]);
        assert!(!s.user().join("warden").exists(), "nothing in the users folder");
    }

    #[test]
    fn an_administrator_step_that_changed_nothing_is_an_error() {
        let s = Sandbox::new("liar");
        let admin = Admin::new(Elevated::Done, || {});
        let err = block_on(install_core(&s.places, &admin, &deny(s.system()))).unwrap_err();
        assert!(err.contains("is not a link to this app"), "{err}");
    }

    #[test]
    fn a_cancelled_or_failed_prompt_falls_back_to_the_users_folder_and_says_why() {
        for (answer, said) in [
            (Elevated::Cancelled, "The administrator prompt was cancelled"),
            (
                Elevated::Failed("1:2: syntax error".into()),
                "could not be made as the administrator (1:2: syntax error)",
            ),
        ] {
            let s = Sandbox::new("fallback");
            let admin = Admin::new(answer, || {});
            let done = block_on(install_core(&s.places, &admin, &deny(s.system()))).unwrap();
            assert_eq!(done.link, s.user().join("warden"));
            assert!(done.note.as_deref().is_some_and(|n| n.contains(said)), "{:?}", done.note);
            assert_eq!(admin.asked().len(), 1);
            // A window opened from Finder has no ~/.local/bin on its PATH.
            let hint = done.hint.clone().expect("the folder is not on PATH");
            assert_eq!(hint.command(), "echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc");
            let words = done.summary(&s.places.cli);
            assert!(words.contains(said) && words.contains("Settings has the line"), "{words}");
            assert!(!s.system().join("warden").exists());
        }
    }

    #[test]
    fn something_in_the_way_is_an_error_not_a_reason_to_ask_or_fall_back() {
        // A file where the system folder should be: not "denied".
        let s = Sandbox::new("inway");
        std::fs::create_dir_all(s.root.join("usr")).unwrap();
        std::fs::write(s.root.join("usr/local"), "").unwrap();
        let admin = Admin::never();
        assert!(install(&s, &admin).is_err());
        assert!(admin.asked().is_empty() && !s.user().join("warden").exists());
    }

    #[test]
    fn linux_links_in_the_users_folder_only() {
        let mut s = Sandbox::new("linux");
        s.places.system_dir = None;
        s.places.mac = false;
        s.places.shell = Some("/usr/bin/bash".into());
        let admin = Admin::never();
        let done = install(&s, &admin).unwrap();
        assert_eq!(done.link, s.user().join("warden"));
        assert!(admin.asked().is_empty() && !done.admin && done.note.is_none());
        let hint = done.hint.expect("~/.local/bin is not on PATH here");
        assert_eq!(hint.command(), "echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc");
        // With the folder on PATH there is nothing to add.
        s.places.path.push(s.user());
        assert!(s.places.path_hint(&s.user().join("warden")).is_none());
        assert_eq!(s.places.first_choice(), Some(s.user().join("warden")));
    }

    #[test]
    fn denial_is_told_from_other_failures() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let readonly = std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem);
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        let p = Path::new("/usr/local/bin");
        assert_eq!(classify(&denied, p), LinkError::Denied);
        assert_eq!(classify(&readonly, p), LinkError::Denied);
        assert!(matches!(classify(&other, p), LinkError::Other(m) if m.starts_with("/usr/local/bin: ")));
    }

    #[test]
    fn a_real_folder_that_is_not_writable_is_denied() {
        let s = Sandbox::new("perm");
        let dir = s.root.join("locked");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let r = link_into(&dir, &s.places.cli);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        // Root writes anywhere: then the link is simply made.
        match r {
            Err(e) => assert_eq!(e, LinkError::Denied),
            Ok(link) => assert_eq!(std::fs::read_link(link).unwrap(), s.places.cli),
        }
    }

    #[test]
    fn a_warden_that_is_not_ours_is_never_replaced_or_removed() {
        let s = Sandbox::new("foreign");
        // install.sh's copy: a file.
        executable(&s.system().join("warden"));
        assert_eq!(status(&s.places), Status::Present { path: s.system().join("warden") });
        let err = install(&s, &Admin::never()).unwrap_err();
        assert!(err.contains("it is left alone"), "{err}");
        assert!(!s.user().join("warden").exists(), "and no second copy goes in the users folder");
        assert!(uninstall(&s, &Admin::never()).unwrap_err().contains("left alone"));
        assert!(s.system().join("warden").is_file());
        // The same for a link to somebody else's warden (Homebrew).
        std::fs::remove_file(s.system().join("warden")).unwrap();
        let brew = s.root.join("opt/bin/warden");
        executable(&brew);
        symlink(&brew, s.system().join("warden")).unwrap();
        assert!(matches!(status(&s.places), Status::Present { .. }));
        assert!(install(&s, &Admin::never()).is_err());
        assert!(uninstall(&s, &Admin::never()).is_err());
        assert_eq!(std::fs::read_link(s.system().join("warden")).unwrap(), brew);
    }

    #[test]
    fn a_link_in_the_way_that_points_elsewhere_is_not_replaced_by_the_raw_linker_either() {
        let s = Sandbox::new("rawlink");
        let other = s.root.join("opt/bin/warden");
        executable(&other);
        std::fs::create_dir_all(s.system()).unwrap();
        symlink(&other, s.system().join("warden")).unwrap();
        assert!(matches!(link_into(&s.system(), &s.places.cli), Err(LinkError::Other(m)) if m.contains("left alone")));
        std::fs::remove_file(s.system().join("warden")).unwrap();
        std::fs::write(s.system().join("warden"), "mine").unwrap();
        assert!(matches!(link_into(&s.system(), &s.places.cli), Err(LinkError::Other(m)) if m.contains("is a file")));
        assert_eq!(std::fs::read_to_string(s.system().join("warden")).unwrap(), "mine");
    }

    #[test]
    fn a_warden_elsewhere_on_the_path_counts_as_installed_and_is_left_alone() {
        let s = Sandbox::new("onpath");
        let f = s.root.join("opt/bin/warden");
        executable(&f);
        assert_eq!(status(&s.places), Status::Present { path: f });
        assert!(!State::of(Ok(s.places.clone())).banner(false), "no banner when warden is there already");
    }

    #[test]
    fn uninstalling_removes_only_the_link_that_points_into_the_app() {
        let s = Sandbox::new("uninstall");
        let done = install(&s, &Admin::never()).unwrap();
        let gone = uninstall(&s, &Admin::never()).unwrap();
        assert_eq!(gone, Removed { link: done.link.clone(), admin: false });
        assert!(std::fs::symlink_metadata(&done.link).is_err());
        assert!(s.places.cli.exists(), "the CLI inside the app is untouched");
        assert_eq!(status(&s.places), Status::Missing);
        assert!(uninstall(&s, &Admin::never()).unwrap_err().contains("no link"));
    }

    #[test]
    fn a_link_in_a_folder_that_says_no_is_removed_by_the_administrator() {
        let s = Sandbox::new("rm-admin");
        let done = install(&s, &Admin::never()).unwrap();
        let link = done.link.clone();
        let admin = Admin::new(Elevated::Done, {
            let link = link.clone();
            move || std::fs::remove_file(&link).unwrap()
        });
        let denied = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        let gone = block_on(uninstall_core(&s.places, &admin, &denied)).unwrap();
        assert_eq!(gone, Removed { link: link.clone(), admin: true });
        assert_eq!(admin.asked(), vec![admin_remove_script(&link)]);
        // Cancelled: nothing is removed, and it says so.
        let done = install(&s, &Admin::never()).unwrap();
        let admin = Admin::new(Elevated::Cancelled, || {});
        let err = block_on(uninstall_core(&s.places, &admin, &denied)).unwrap_err();
        assert!(err.contains("cancelled") && err.contains("was not removed"), "{err}");
        assert!(std::fs::symlink_metadata(done.link).is_ok());
        // An error that is not a denial is shown as it is, with no prompt.
        let gone_already = |_: &Path| Err(std::io::Error::from(std::io::ErrorKind::NotFound));
        let admin = Admin::never();
        assert!(block_on(uninstall_core(&s.places, &admin, &gone_already)).is_err());
        assert!(admin.asked().is_empty());
    }

    #[test]
    fn a_link_to_an_app_that_moved_is_broken_and_can_be_replaced_or_removed() {
        let s = Sandbox::new("moved");
        std::fs::create_dir_all(s.system()).unwrap();
        // It pointed into Warden.app where it was before the app was moved.
        let old = s.root.join("Downloads/Warden.app/Contents/MacOS/warden");
        symlink(&old, s.system().join("warden")).unwrap();
        assert_eq!(status(&s.places), Status::Broken { link: s.system().join("warden"), target: old.clone() });
        // Installing again points it at the app that is here.
        let done = install(&s, &Admin::never()).unwrap();
        assert_eq!(std::fs::read_link(&done.link).unwrap(), s.places.cli);
        // And a broken one can be taken away.
        std::fs::remove_file(&done.link).unwrap();
        symlink(&old, s.system().join("warden")).unwrap();
        let gone = uninstall(&s, &Admin::never()).unwrap();
        assert_eq!(gone.link, s.system().join("warden"));
        assert!(std::fs::symlink_metadata(gone.link).is_err());
    }

    #[test]
    fn a_dead_link_to_nothing_of_ours_is_replaced() {
        let s = Sandbox::new("dead");
        std::fs::create_dir_all(s.system()).unwrap();
        symlink(s.root.join("gone/warden"), s.system().join("warden")).unwrap();
        assert_eq!(status(&s.places), Status::Missing, "a dead link to nothing we know is nobody's");
        let done = install(&s, &Admin::never()).unwrap();
        assert_eq!(std::fs::read_link(done.link).unwrap(), s.places.cli);
    }

    #[test]
    fn a_link_in_the_users_folder_is_found_and_removed_too() {
        let s = Sandbox::new("userlink");
        std::fs::create_dir_all(s.user()).unwrap();
        symlink(&s.places.cli, s.user().join("warden")).unwrap();
        assert!(matches!(status(&s.places), Status::Linked { link, .. } if link == s.user().join("warden")));
        assert_eq!(uninstall(&s, &Admin::never()).unwrap().link, s.user().join("warden"));
    }

    #[test]
    fn the_banner_is_for_a_bundle_with_no_warden_that_was_not_turned_away() {
        let s = Sandbox::new("banner");
        let mut state = State::of(Ok(s.places.clone()));
        assert!(state.banner(false));
        assert!(!state.banner(true), "dismissed for good");
        // Linux: the folder of an unpacked download is not a bundle.
        let mut linux = s.places.clone();
        linux.exe = s.root.join("warden-gui-0.1.0-linux-x86_64/warden-gui");
        assert!(!State::of(Ok(linux)).banner(false));
        // Installed: nothing to offer.
        install(&s, &Admin::never()).unwrap();
        state.refresh();
        assert!(!state.banner(false));
        // Not looked at, or not possible from here.
        assert!(!State::default().banner(false));
        assert!(!State::of(Err("no".into())).banner(false));
    }

    #[test]
    fn the_path_line_follows_the_shell_like_install_sh() {
        let home = Path::new("/Users/me");
        let dir = Path::new("/Users/me/.local/bin");
        let h = |shell: Option<&str>, mac| Hint::new(dir, Some(home), shell, mac);
        assert_eq!(h(Some("/bin/zsh"), true).command(), "echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc");
        assert_eq!(h(Some("/bin/bash"), true).file.as_deref(), Some("~/.bash_profile"));
        assert_eq!(h(Some("/usr/bin/bash"), false).file.as_deref(), Some("~/.bashrc"));
        assert_eq!(h(Some("/usr/bin/fish"), false).command(), "fish_add_path \"$HOME/.local/bin\"");
        assert_eq!(h(Some("/bin/tcsh"), false).file.as_deref(), Some("~/.profile"));
        assert_eq!(h(None, false).shell, "sh");
        // Outside the home folder the path stays whole.
        let other = Hint::new(Path::new("/opt/w/bin"), Some(home), Some("zsh"), true);
        assert_eq!(other.line, "export PATH=\"/opt/w/bin:$PATH\"");
    }

    #[test]
    fn the_administrator_scripts_are_quoted_for_the_shell_and_then_for_applescript() {
        let dir = Path::new("/usr/local/bin");
        let cli = Path::new("/Applications/Warden.app/Contents/MacOS/warden");
        let script = admin_install_script(dir, cli);
        assert_eq!(
            script,
            "/bin/mkdir -p /usr/local/bin && { [ ! -e /usr/local/bin/warden ] || [ -L /usr/local/bin/warden ]; } && \
             /bin/ln -sfn /Applications/Warden.app/Contents/MacOS/warden /usr/local/bin/warden"
        );
        assert_eq!(
            admin_remove_script(&dir.join("warden")),
            "if [ -L /usr/local/bin/warden ]; then /bin/rm -- /usr/local/bin/warden; fi"
        );
        let args = osascript_args(&script).unwrap();
        assert_eq!(args[0], "-e");
        assert!(args[1].starts_with("do shell script \"/bin/mkdir -p /usr/local/bin"));
        assert!(args[1].ends_with("\" with administrator privileges"));
        assert_eq!(args.len(), 2);
        // The app's folder may hold a space, a quote and a backslash: shell quoting first (so the
        // shell reads one word), then AppleScript's (so the string reads back as written).
        let odd = Path::new("/Users/o'neil/My Apps/Warden.app/Contents/MacOS/warden");
        let script = admin_install_script(dir, odd);
        assert!(script.contains("'/Users/o'\\''neil/My Apps/Warden.app/Contents/MacOS/warden'"), "{script}");
        let applescript = &osascript_args(&script).unwrap()[1];
        assert!(
            applescript.contains("o'\\\\''neil"),
            "the shell's backslash is doubled for AppleScript: {applescript}"
        );
        assert_eq!(
            osascript_args("echo \"hi\"").unwrap()[1],
            "do shell script \"echo \\\"hi\\\"\" with administrator privileges"
        );
        // A newline cannot be written into the string safely.
        assert!(osascript_args("echo a\necho b").is_err());
        // And the shell reads back the path that was meant.
        let echoed =
            std::process::Command::new("/bin/sh").args(["-c", &format!("printf %s {}", q(odd))]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&echoed.stdout), odd.to_string_lossy());
    }

    #[test]
    fn the_scripts_check_again_as_root_that_only_a_link_is_touched() {
        // /bin/mkdir and /bin/ln are macOS's: the guards are run as they are, and the removal with `rm`.
        let s = Sandbox::new("script");
        let dir = s.root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("warden");
        let sh = |script: &str| std::process::Command::new("/bin/sh").args(["-c", script]).status().unwrap();
        let guard = format!("{{ [ ! -e {l} ] || [ -L {l} ]; }}", l = q(&link));
        std::fs::write(&link, "mine").unwrap();
        assert!(!sh(&guard).success(), "a file is in the way");
        std::fs::remove_file(&link).unwrap();
        assert!(sh(&guard).success(), "nothing is in the way");
        symlink(s.root.join("nowhere"), &link).unwrap();
        assert!(sh(&guard).success(), "a dead link may be replaced");
        let rm = admin_remove_script(&link).replace("/bin/rm", "rm");
        assert!(sh(&rm).success());
        assert!(std::fs::symlink_metadata(&link).is_err(), "a link is removed");
        std::fs::write(&link, "mine").unwrap();
        assert!(sh(&rm).success());
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "mine", "a file is left");
    }

    #[test]
    fn a_closed_prompt_is_told_from_a_failure() {
        assert!(cancelled("0:56: execution error: User canceled. (-128)\n"));
        assert!(!cancelled("execution error: /bin/ln: Permission denied (1)"));
    }
}

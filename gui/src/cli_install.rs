//! "Install command line tool": a link named `warden` in a folder Terminal looks in, pointing at the
//! `warden` CLI that sits next to the GUI (`Warden.app/Contents/MacOS/warden` on macOS, the unpacked
//! download elsewhere), so `warden list` works without installing anything else.
//!
//! Where the link goes, in order:
//! 1. `/usr/local/bin/warden` (macOS), made as the user when that folder lets them;
//! 2. the same through the system's administrator prompt (`osascript`: `do shell script ... with
//!    administrator privileges`) when it does not. Closing that prompt means stop: nothing is
//!    installed, and "Install for this user only" is the way to the next choice;
//! 3. `~/.local/bin/warden`, with the line that puts that folder on PATH (always on Linux, when
//!    asked for ("for this user only") on a Mac, and on a Mac when the prompt fails, as opposed to
//!    being closed).
//!
//! Only links are made and removed, and only ones that point into an app of Warden's (this one, or
//! another copy: `Warden 2.app`, an unpacked download beside its `warden-gui`) or that point
//! nowhere: a `warden` that is a file, or a link to something else (Homebrew, install.sh, a
//! package), is never replaced or deleted. Every folder is a field of [`Places`], so the tests run
//! on temporary ones; only [`Osascript`] (the prompt) is macOS's, and the tests check the command
//! it runs, not the prompt.

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
        for d in usual_dirs(home.as_deref()) {
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

    /// The line that puts `dir` on PATH, when `dir` is `user_dir`, this window's PATH does not have
    /// it, and the shell's startup files do not already add it (a window opened from Finder never
    /// has `~/.local/bin` on its PATH, whatever the person's Terminal does).
    pub fn path_hint(&self, link: &Path) -> Option<Hint> {
        let dir = link.parent()?;
        if self.user_dir.as_deref() != Some(dir) || self.path.iter().any(|d| d == dir) {
            return None;
        }
        let hint = Hint::new(dir, self.home.as_deref(), self.shell.as_deref(), self.mac);
        (!self.startup_files_add(&hint, dir)).then_some(hint)
    }

    /// One of the shell's startup files already has a line that adds `dir` to PATH (it is looked for
    /// by its name under the home folder, `.local/bin`, on a line that is not a comment).
    fn startup_files_add(&self, hint: &Hint, dir: &Path) -> bool {
        let Some(home) = &self.home else { return false };
        let needle = match dir.strip_prefix(home) {
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => dir.to_string_lossy().into_owned(),
        };
        hint.startup_files().iter().any(|f| {
            std::fs::read_to_string(home.join(f))
                .is_ok_and(|text| text.lines().any(|l| !l.trim_start().starts_with('#') && l.contains(&needle)))
        })
    }
}

/// The bin folders where `warden` usually is, besides PATH (a window opened from Finder or a menu
/// has launchd's short PATH): the system's, Homebrew's, `~/.local/bin` and `~/.cargo/bin`.
pub fn usual_dirs(home: Option<&Path>) -> Vec<PathBuf> {
    let mut usual = vec![PathBuf::from(SYSTEM_DIR), PathBuf::from("/opt/homebrew/bin")];
    usual.extend(home.map(|h| h.join(".local").join("bin")));
    usual.extend(home.map(|h| h.join(".cargo").join("bin")));
    usual
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
/// a link to the CLI there would be dead the next time. Err says what to do. A volume that can be
/// written to (a second drive) is a place to keep an app: only a read-only one is a disk image.
pub fn stays_put(exe: &Path) -> Result<(), String> {
    stays_put_with(exe, &read_only_mount)
}

fn stays_put_with(exe: &Path, read_only: &dyn Fn(&Path) -> bool) -> Result<(), String> {
    if exe.components().any(|c| c.as_os_str() == "AppTranslocation") {
        return Err("Warden runs from a temporary place macOS made for it (it was opened before being moved), and \
                    a link to it would stop working. Move Warden.app to Applications, open it from there, then \
                    install the command line tool."
            .into());
    }
    if let Ok(rest) = exe.strip_prefix("/Volumes")
        && let Some(volume) = rest.components().next()
        && read_only(&Path::new("/Volumes").join(volume.as_os_str()))
    {
        return Err("Warden runs from a read-only disk (a disk image, most likely), which goes away when it is \
                    ejected, and a link to it would stop working. Drag Warden.app to Applications, eject the disk \
                    image, open Warden from Applications, then install the command line tool."
            .into());
    }
    Ok(())
}

/// The file system holding `dir` is mounted read-only.
fn read_only_mount(dir: &Path) -> bool {
    rustix::fs::statvfs(dir).is_ok_and(|s| s.f_flag.contains(rustix::fs::StatVfsMountFlags::RDONLY))
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
    /// A link into an app of Warden's: `link` points at `target`, which is there (this app's CLI,
    /// or another copy's: see [`Status::of_another_copy`]).
    Linked { link: PathBuf, target: PathBuf },
    /// A link to nothing: the app it pointed into was moved or removed.
    Broken { link: PathBuf, target: PathBuf },
    /// A `warden` that is not a link into an app of Warden's (a package, install.sh, Homebrew, or a
    /// copy): it is left alone.
    Present { path: PathBuf },
}

impl Status {
    /// A live link that points at another copy of Warden than this app (`Warden 2.app`, an old
    /// download): "Point it at this app" is the way to this one.
    pub fn of_another_copy(&self, cli: &Path) -> bool {
        matches!(self, Status::Linked { target, .. } if target != cli && !same_file(target, cli))
    }
}

/// The links this window makes are found by what they point at: this app's CLI, the CLI of any app
/// bundle (`Warden.app`, `Warden 2.app`: `X.app/Contents/MacOS/warden`), or a `warden` that sits
/// beside a `warden-gui` (an unpacked download).
fn points_into_app(target: &Path, cli: &Path) -> bool {
    target == cli || same_file(target, cli) || in_an_app(target) || beside_a_gui(target)
}

fn same_file(a: &Path, b: &Path) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// `…/<Name>.app/Contents/MacOS/warden`: an app wherever it is, or was before it moved.
fn in_an_app(target: &Path) -> bool {
    let last: Vec<Component<'_>> = target.components().rev().take(4).collect();
    let [n, macos, contents, app] = last.as_slice() else { return false };
    n.as_os_str() == NAME
        && macos.as_os_str() == "MacOS"
        && contents.as_os_str() == "Contents"
        && app.as_os_str().to_string_lossy().to_ascii_lowercase().ends_with(".app")
}

/// A `warden` with a `warden-gui` in its folder: the CLI of an unpacked GUI + CLI download.
fn beside_a_gui(target: &Path) -> bool {
    target.file_name().is_some_and(|n| n == NAME) && target.parent().is_some_and(|d| d.join("warden-gui").exists())
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
            // A link to nothing is nobody's: it can be replaced or removed, whatever it was.
            Some(target) if !target.exists() => {
                broken.get_or_insert(Status::Broken { link, target });
            }
            Some(target) if points_into_app(&target, &p.cli) => return Status::Linked { link, target },
            // A live link to something else, or a file: not ours.
            Some(_) => return Status::Present { path: link },
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

    /// One command that does it: appends the line to the startup file unless the file has it
    /// already (so running it twice adds it once); fish: the command itself, which is idempotent.
    pub fn command(&self) -> String {
        match &self.file {
            Some(f) => format!("grep -qsF '{l}' {f} || echo '{l}' >> {f}", l = self.line),
            None => self.line.clone(),
        }
    }

    /// The startup files of this shell, under the home folder, where a line may already add the folder.
    fn startup_files(&self) -> &'static [&'static str] {
        match self.shell.as_str() {
            "zsh" => &[".zshrc", ".zshenv", ".zprofile"],
            "bash" => &[".bashrc", ".bash_profile", ".bash_login", ".profile"],
            "fish" => &[".config/fish/config.fish"],
            _ => &[".profile"],
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

/// What the administrator runs to make the link: the folder, then the link, but only when nothing
/// is there, a link to nothing is, or a link into an app of Warden's is (checked again here, as
/// root, because a person may have made a `warden` of their own while the prompt was open): a file,
/// or a live link to somebody else's, stays.
pub fn admin_install_script(dir: &Path, cli: &Path) -> String {
    let (d, l, c) = (q(dir), q(&dir.join(NAME)), q(cli));
    let ours =
        format!("case \"$(/usr/bin/readlink {l})\" in {c}|*.app/Contents/MacOS/warden) true ;; *) false ;; esac");
    let in_the_way =
        shell_quote(&format!("{} is there already, and is not a link to Warden's app", dir.join(NAME).display()));
    format!(
        "/bin/mkdir -p {d} && if {{ [ ! -e {l} ] && [ ! -L {l} ]; }} || {{ [ -L {l} ] && {{ [ ! -e {l} ] || {ours}; }}; }}; \
         then /bin/ln -sfn {c} {l}; else echo {in_the_way} >&2; exit 1; fi"
    )
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

/// What the person asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// The usual place: `/usr/local/bin` (with the administrator prompt if it needs one) on a Mac.
    Preferred,
    /// `~/.local/bin`, with no prompt.
    ThisUserOnly,
}

/// How an install ended, short of an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Installed(Installed),
    /// The administrator prompt was closed: that is a "no", and nothing was installed (not even in
    /// the user's folder: that is its own choice).
    Cancelled {
        wanted: PathBuf,
    },
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
pub async fn install(p: Places, choice: Choice) -> Result<Outcome, String> {
    install_with(&p, &Osascript, choice).await
}

pub async fn install_with(p: &Places, admin: &impl Elevate, choice: Choice) -> Result<Outcome, String> {
    install_core(p, admin, &link_into, choice).await
}

async fn install_core(
    p: &Places,
    admin: &impl Elevate,
    link_with: Linker<'_>,
    choice: Choice,
) -> Result<Outcome, String> {
    p.usable()?;
    let st = status(p);
    match &st {
        Status::Present { path } => {
            return Err(format!(
                "there is a `warden` at {} already, and it is not a link to an app of Warden's: it is left alone",
                path.display()
            ));
        }
        Status::Linked { link, target } => {
            if !st.of_another_copy(&p.cli) {
                let hint = p.path_hint(link);
                let note = Some("It was installed already.".to_string());
                return Ok(Outcome::Installed(Installed { link: link.clone(), admin: false, note, hint }));
            }
            // A link into another copy of the app: point it at this one, where it is.
            if choice == Choice::Preferred || link.parent() == p.user_dir.as_deref() {
                let Some(dir) = link.parent() else { return Err("the link has no folder".into()) };
                let was = format!("It pointed to {}.", target.display());
                return place(p, admin, link_with, dir, Some(was)).await;
            }
        }
        Status::Missing | Status::Broken { .. } => {}
    }
    match (choice, &p.system_dir) {
        (Choice::Preferred, Some(dir)) => place(p, admin, link_with, dir, None).await,
        _ => {
            let Some(dir) = &p.user_dir else { return Err("there is no home folder to link in".into()) };
            place(p, admin, link_with, dir, None).await
        }
    }
}

/// Make the link in `dir`: as the user, else (a system folder, on a Mac) through the administrator
/// prompt. A closed prompt is a "no"; a failed one falls back to the user's folder, with the reason.
async fn place(
    p: &Places,
    admin: &impl Elevate,
    link_with: Linker<'_>,
    dir: &Path,
    was: Option<String>,
) -> Result<Outcome, String> {
    let wanted = dir.join(NAME);
    let installed = |link: PathBuf, admin: bool, note: Option<String>| {
        let hint = p.path_hint(&link);
        Ok(Outcome::Installed(Installed { link, admin, note, hint }))
    };
    let mut note = was;
    match link_with(dir, &p.cli) {
        Ok(l) => return installed(l, false, note),
        Err(LinkError::Other(e)) => return Err(e),
        Err(LinkError::Denied) if p.user_dir.as_deref() == Some(dir) => {
            return Err(format!("{} does not let this user make a link", dir.display()));
        }
        Err(LinkError::Denied) => {}
    }
    match admin.run(&admin_install_script(dir, &p.cli)).await {
        Elevated::Done => {
            let st = status(p);
            match &st {
                Status::Linked { link, .. } if !st.of_another_copy(&p.cli) => installed(link.clone(), true, note),
                _ => Err(format!("the administrator step ran, but {} is not a link to this app", wanted.display())),
            }
        }
        Elevated::Cancelled => Ok(Outcome::Cancelled { wanted }),
        Elevated::Failed(e) => {
            // Not "no", but "could not": the user's folder is next.
            let why = format!("{} could not be made as the administrator ({e}).", wanted.display());
            note = Some(note.map_or(why.clone(), |n| format!("{n} {why}")));
            let Some(user) = &p.user_dir else { return Err(why) };
            match link_with(user, &p.cli) {
                Ok(l) => installed(l, false, note),
                Err(LinkError::Denied) => Err(format!("{} does not let this user make a link", user.display())),
                Err(LinkError::Other(e)) => Err(e),
            }
        }
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
    /// The last install was stopped at the administrator prompt: Settings says so, and offers
    /// "Install for this user only".
    pub cancelled: bool,
}

impl Default for State {
    /// Not looked at: no banner, nothing offered.
    fn default() -> State {
        State { places: Err("not looked at yet".into()), status: Status::Missing, work: Work::Idle, cancelled: false }
    }
}

impl State {
    /// Look at this machine.
    pub fn detect() -> State {
        State::of(Places::here())
    }

    pub fn of(places: Result<Places, String>) -> State {
        let status = places.as_ref().map_or(Status::Missing, status);
        State { places, status, work: Work::Idle, cancelled: false }
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

    /// The usual install, expected to end with a link.
    fn install(s: &Sandbox, admin: &Admin) -> Result<Installed, String> {
        match block_on(install_with(&s.places, admin, Choice::Preferred))? {
            Outcome::Installed(i) => Ok(i),
            Outcome::Cancelled { wanted } => Err(format!("cancelled: {}", wanted.display())),
        }
    }

    fn install_as(s: &Sandbox, admin: &Admin, choice: Choice) -> Result<Outcome, String> {
        block_on(install_with(&s.places, admin, choice))
    }

    /// `install_core` with the system folder denied, expected to end with a link.
    fn install_denied(s: &Sandbox, admin: &Admin) -> Result<Outcome, String> {
        block_on(install_core(&s.places, admin, &deny(s.system()), Choice::Preferred))
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
        assert!(stays_put(Path::new("/Applications/Warden.app/Contents/MacOS/warden-gui")).is_ok());
        assert!(stays_put(Path::new("/Users/me/Applications/Warden.app/Contents/MacOS/warden-gui")).is_ok());
        // The same Places refuse to install from such a place.
        let mut s = Sandbox::new("volume");
        s.places.exe = PathBuf::from(
            "/private/var/folders/x9/abc/T/AppTranslocation/6B1F-42/d/Warden.app/Contents/MacOS/warden-gui",
        );
        assert!(install(&s, &Admin::never()).unwrap_err().contains("Move Warden.app to Applications"));
        assert!(!s.system().exists(), "nothing was made");
    }

    #[test]
    fn a_volume_is_refused_only_when_it_is_a_read_only_disk() {
        let exe = Path::new("/Volumes/Warden/Warden.app/Contents/MacOS/warden-gui");
        let asked = Mutex::new(vec![]);
        let read_only = |d: &Path| {
            asked.lock().unwrap().push(d.to_path_buf());
            true
        };
        let why = stays_put_with(exe, &read_only).unwrap_err();
        assert!(why.contains("read-only disk") && why.contains("eject the disk image"), "{why}");
        assert_eq!(*asked.lock().unwrap(), [PathBuf::from("/Volumes/Warden")], "the volume is what is looked at");
        // A second drive that can be written to is a place to keep an app.
        assert!(stays_put_with(exe, &|_| false).is_ok());
        // Other places are not asked about the volume at all.
        let before = asked.lock().unwrap().len();
        assert!(stays_put_with(Path::new("/Applications/Warden.app/Contents/MacOS/warden-gui"), &read_only).is_ok());
        assert_eq!(asked.lock().unwrap().len(), before);
        // The real check: a folder of this machine is not a read-only mount, and a missing one is not either.
        assert!(!read_only_mount(&std::env::temp_dir()));
        assert!(!read_only_mount(Path::new("/no/such/volume")));
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
        let Outcome::Installed(done) = install_denied(&s, &admin).unwrap() else { panic!("not installed") };
        assert!(done.admin && done.note.is_none() && done.hint.is_none());
        assert_eq!(done.link, s.system().join("warden"));
        assert_eq!(admin.asked(), vec![admin_install_script(&s.system(), &s.places.cli)]);
        assert!(!s.user().join("warden").exists(), "nothing in the users folder");
    }

    #[test]
    fn an_administrator_step_that_changed_nothing_is_an_error() {
        let s = Sandbox::new("liar");
        let admin = Admin::new(Elevated::Done, || {});
        let err = install_denied(&s, &admin).unwrap_err();
        assert!(err.contains("is not a link to this app"), "{err}");
    }

    #[test]
    fn a_cancelled_prompt_installs_nothing_and_says_so() {
        let s = Sandbox::new("cancel");
        let admin = Admin::new(Elevated::Cancelled, || {});
        let out = install_denied(&s, &admin).unwrap();
        assert_eq!(out, Outcome::Cancelled { wanted: s.system().join("warden") });
        assert_eq!(admin.asked().len(), 1);
        assert!(!s.system().join("warden").exists(), "no link in the system folder");
        assert!(!s.user().join("warden").exists(), "and none in the users folder: that is a choice of its own");
        assert_eq!(status(&s.places), Status::Missing);
    }

    #[test]
    fn for_this_user_only_links_in_the_users_folder_with_no_prompt() {
        let s = Sandbox::new("user-only");
        let admin = Admin::never();
        let Outcome::Installed(done) = install_as(&s, &admin, Choice::ThisUserOnly).unwrap() else {
            panic!("not installed")
        };
        assert_eq!(done.link, s.user().join("warden"));
        assert!(admin.asked().is_empty() && !done.admin && done.note.is_none());
        assert!(!s.system().exists(), "the system folder is not touched");
        let hint = done.hint.clone().expect("a Finder-launched window has no ~/.local/bin on its PATH");
        assert!(hint.command().starts_with("grep -qsF "), "{}", hint.command());
        // Installing it that way again changes nothing.
        let Outcome::Installed(again) = install_as(&s, &admin, Choice::ThisUserOnly).unwrap() else { panic!() };
        assert!(again.note.as_deref().is_some_and(|n| n.contains("already")));
    }

    #[test]
    fn a_failed_prompt_falls_back_to_the_users_folder_and_says_why() {
        let s = Sandbox::new("fallback");
        let admin = Admin::new(Elevated::Failed("1:2: syntax error".into()), || {});
        let Outcome::Installed(done) = install_denied(&s, &admin).unwrap() else { panic!("not installed") };
        let said = "could not be made as the administrator (1:2: syntax error)";
        assert_eq!(done.link, s.user().join("warden"));
        assert!(done.note.as_deref().is_some_and(|n| n.contains(said)), "{:?}", done.note);
        assert_eq!(admin.asked().len(), 1);
        // A window opened from Finder has no ~/.local/bin on its PATH.
        let hint = done.hint.clone().expect("the folder is not on PATH");
        assert_eq!(
            hint.command(),
            "grep -qsF 'export PATH=\"$HOME/.local/bin:$PATH\"' ~/.zshrc || \
             echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc"
        );
        let words = done.summary(&s.places.cli);
        assert!(words.contains(said) && words.contains("Settings has the line"), "{words}");
        assert!(!s.system().join("warden").exists());
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
        assert!(hint.command().ends_with("echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.bashrc"));
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
    fn a_dead_link_is_nobodys_and_is_replaced_or_removed_whatever_it_pointed_at() {
        let s = Sandbox::new("dead");
        std::fs::create_dir_all(s.system()).unwrap();
        let gone = s.root.join("gone/warden");
        symlink(&gone, s.system().join("warden")).unwrap();
        assert_eq!(status(&s.places), Status::Broken { link: s.system().join("warden"), target: gone });
        let done = install(&s, &Admin::never()).unwrap();
        assert_eq!(std::fs::read_link(done.link).unwrap(), s.places.cli);
        // A dead link into an app of another name (`Warden 2.app`, deleted) is told as dead, not as missing.
        let old = s.root.join("Trash/Warden 2.app/Contents/MacOS/warden");
        std::fs::remove_file(s.system().join("warden")).unwrap();
        symlink(&old, s.system().join("warden")).unwrap();
        assert!(matches!(status(&s.places), Status::Broken { target, .. } if target == old));
        assert_eq!(uninstall(&s, &Admin::never()).unwrap().link, s.system().join("warden"));
    }

    #[test]
    fn a_link_into_another_copy_of_the_app_can_be_pointed_at_this_one_or_removed() {
        for (name, other) in [
            ("renamed", "Applications/Warden 2.app/Contents/MacOS/warden"),
            ("download", "Downloads/warden-gui-0.1.0-linux-x86_64/warden"),
        ] {
            let s = Sandbox::new(&format!("copy-{name}"));
            let copy = s.root.join(other);
            executable(&copy);
            executable(&copy.with_file_name("warden-gui"));
            std::fs::create_dir_all(s.system()).unwrap();
            symlink(&copy, s.system().join("warden")).unwrap();
            let st = status(&s.places);
            assert_eq!(st, Status::Linked { link: s.system().join("warden"), target: copy.clone() }, "{name}");
            assert!(st.of_another_copy(&s.places.cli), "{name}: it is not this app's");
            // "Point it at this app": the same link, now to this app, with no prompt.
            let done = install(&s, &Admin::never()).unwrap();
            assert_eq!(done.link, s.system().join("warden"));
            assert!(done.note.as_deref().is_some_and(|n| n.contains("It pointed to")), "{:?}", done.note);
            assert_eq!(std::fs::read_link(&done.link).unwrap(), s.places.cli);
            assert!(!status(&s.places).of_another_copy(&s.places.cli));
            // The other copy is left as it was.
            assert!(copy.exists());
            // And a link to the other copy can be removed as well.
            std::fs::remove_file(&done.link).unwrap();
            symlink(&copy, s.system().join("warden")).unwrap();
            assert_eq!(uninstall(&s, &Admin::never()).unwrap().link, s.system().join("warden"));
            assert!(copy.exists(), "only the link is removed");
        }
        // A `warden` link to a folder with no `warden-gui` beside it, in no app, is somebody else's.
        let s = Sandbox::new("copy-foreign");
        let other = s.root.join("opt/tool/warden");
        executable(&other);
        std::fs::create_dir_all(s.system()).unwrap();
        symlink(&other, s.system().join("warden")).unwrap();
        assert!(matches!(status(&s.places), Status::Present { .. }));
    }

    #[test]
    fn the_path_line_is_not_offered_when_a_startup_file_has_it_and_it_adds_itself_once() {
        let mut s = Sandbox::new("hint");
        let home = s.places.home.clone().unwrap();
        let link = s.user().join("warden");
        assert!(s.places.path_hint(&link).is_some(), "nothing sets it up yet");
        // A comment is not a setting.
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".zshrc"), "# export PATH=\"$HOME/.local/bin:$PATH\"\n").unwrap();
        assert!(s.places.path_hint(&link).is_some());
        // A line the person wrote themselves (not this window's) counts all the same.
        std::fs::write(home.join(".zprofile"), "path+=(~/.local/bin)\n").unwrap();
        assert!(s.places.path_hint(&link).is_none(), "the startup files already add the folder");
        // Another shell's files are not looked in.
        s.places.shell = Some("/usr/bin/bash".into());
        assert!(s.places.path_hint(&link).is_some());
        // The command is idempotent: run twice, the line is in the file once.
        let hint = s.places.path_hint(&link).unwrap();
        let run = || {
            let st = std::process::Command::new("/bin/sh")
                .args(["-c", &hint.command()])
                .env("HOME", &home)
                .status()
                .unwrap();
            assert!(st.success());
        };
        run();
        run();
        let file = hint.file.as_deref().and_then(|f| f.strip_prefix("~/")).expect("bash has a startup file");
        let text = std::fs::read_to_string(home.join(file)).unwrap();
        assert_eq!(text.matches(".local/bin").count(), 1, "{text}");
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
        assert_eq!(
            h(Some("/bin/zsh"), true).command(),
            "grep -qsF 'export PATH=\"$HOME/.local/bin:$PATH\"' ~/.zshrc || \
             echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc"
        );
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
        assert!(script.starts_with("/bin/mkdir -p /usr/local/bin && if "), "{script}");
        assert!(script.contains("/bin/ln -sfn /Applications/Warden.app/Contents/MacOS/warden /usr/local/bin/warden;"));
        assert_eq!(
            admin_remove_script(&dir.join("warden")),
            "if [ -L /usr/local/bin/warden ]; then /bin/rm -- /usr/local/bin/warden; fi"
        );
        let args = osascript_args(&script).unwrap();
        assert_eq!(args[0], "-e");
        assert!(args[1].starts_with("do shell script \"/bin/mkdir -p /usr/local/bin"));
        assert!(args[1].ends_with("\" with administrator privileges"));
        assert_eq!(args.len(), 2);
        // The script has quotes of its own (the `case`): AppleScript sees them escaped.
        assert!(args[1].contains("readlink /usr/local/bin/warden)\\\" in"), "{}", args[1]);
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

    /// Run the install script in a folder of the sandbox, with the commands by their short names
    /// (`/bin/ln` and the rest are macOS's paths), and say whether it ended well.
    fn run_install_script(dir: &Path, cli: &Path) -> (bool, String) {
        let script = admin_install_script(dir, cli)
            .replace("/bin/mkdir", "mkdir")
            .replace("/bin/ln", "ln")
            .replace("/usr/bin/readlink", "readlink");
        let out = std::process::Command::new("/bin/sh").args(["-c", &script]).output().unwrap();
        (out.status.success(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    #[test]
    fn the_install_script_checks_again_as_root_that_only_this_apps_link_or_a_dead_one_is_replaced() {
        let s = Sandbox::new("script");
        let dir = s.root.join("bin");
        let link = dir.join("warden");
        let cli = s.places.cli.clone();
        // Nothing there (the folder too): it is made.
        let (ok, err) = run_install_script(&dir, &cli);
        assert!(ok, "{err}");
        assert_eq!(std::fs::read_link(&link).unwrap(), cli);
        // A file in the way: refused, and kept.
        std::fs::remove_file(&link).unwrap();
        std::fs::write(&link, "mine").unwrap();
        let (ok, err) = run_install_script(&dir, &cli);
        assert!(!ok && err.contains("is not a link to Warden's app"), "{err}");
        assert_eq!(std::fs::read_to_string(&link).unwrap(), "mine");
        // A live link to somebody else's (Homebrew made it while the prompt was open): kept.
        std::fs::remove_file(&link).unwrap();
        let brew = s.root.join("opt/bin/warden");
        executable(&brew);
        symlink(&brew, &link).unwrap();
        let (ok, _) = run_install_script(&dir, &cli);
        assert!(!ok);
        assert_eq!(std::fs::read_link(&link).unwrap(), brew, "a live link of somebody else's stays");
        // A dead link, whatever it was: replaced.
        std::fs::remove_file(&link).unwrap();
        symlink(s.root.join("gone/warden"), &link).unwrap();
        assert!(run_install_script(&dir, &cli).0);
        assert_eq!(std::fs::read_link(&link).unwrap(), cli);
        // A link into another copy of the app, and one to this app: replaced (pointed at this one).
        for other in ["Elsewhere/Warden 2.app/Contents/MacOS/warden", "Applications/Warden.app/Contents/MacOS/warden"] {
            let target = s.root.join(other);
            executable(&target);
            std::fs::remove_file(&link).unwrap();
            symlink(&target, &link).unwrap();
            let (ok, err) = run_install_script(&dir, &cli);
            assert!(ok, "{other}: {err}");
            assert_eq!(std::fs::read_link(&link).unwrap(), cli, "{other}");
        }
        // The path of the CLI itself is accepted even when it is not in an app (an unpacked download).
        let unpacked = s.root.join("dl/warden");
        executable(&unpacked);
        std::fs::remove_file(&link).unwrap();
        symlink(&unpacked, &link).unwrap();
        assert!(run_install_script(&dir, &unpacked).0);
    }

    #[test]
    fn the_remove_script_removes_only_a_link() {
        let s = Sandbox::new("rm-script");
        let dir = s.root.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("warden");
        let sh = |script: &str| std::process::Command::new("/bin/sh").args(["-c", script]).status().unwrap();
        symlink(s.root.join("nowhere"), &link).unwrap();
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

//! `warden startup` / `warden unstartup`: apps and wardend come back after a
//! reboot or a crash, through the service manager this host has.
//!
//! | Host | `warden startup` installs |
//! |---|---|
//! | Linux with systemd, root | `warden@.service` (enabled per saved app), `wardend.service`, the sysctl file |
//! | Linux with systemd, a user (`--user`) | the same two units in ~/.config/systemd/user, and lingering |
//! | macOS | a launchd job running `warden daemon --resurrect` (LaunchAgent; LaunchDaemon as root) |
//! | anything else | nothing: `warden resurrect` or `warden daemon --resurrect` from the init system |
//!
//! Under systemd every app has its own unit (cgroup, limits, restarts) and
//! wardend only adds live events and restarts of background supervisors: it
//! never resurrects apps there, which would start them twice. Under launchd,
//! and without a service manager, wardend is what brings the saved apps back.
//!
//! Tests replace every path and program: `WARDEN_UNIT_DIR`,
//! `WARDEN_USER_UNIT_DIR`, `WARDEN_SYSCTL_DIR`, `WARDEN_SYSTEMCTL`,
//! `WARDEN_LOGINCTL`, `WARDEN_LINGER_DIR`, `WARDEN_LAUNCHD_DIR` and
//! `WARDEN_LAUNCHCTL` (which also selects launchd, on any OS).

use crate::cli::Args;
use crate::fleet::{self, Scope};
use std::path::{Path, PathBuf};

const WARDEN_UNIT: &str = include_str!("../contrib/warden@.service");
const WARDEND_UNIT: &str = include_str!("../contrib/wardend.service");
const SYSCTL_CONF: &str = include_str!("../contrib/99-warden.conf");

/// The launchd job's label (and its plist's name).
pub const LAUNCHD_LABEL: &str = "io.github.oceanwap.warden.daemon";

/// `--system`, `--user`, or neither (root: system, anyone else: user).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    Auto,
    System,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Systemd(Scope),
    Launchd { system: bool },
    None,
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn launchd_host() -> bool {
    cfg!(target_os = "macos") || env_path("WARDEN_LAUNCHCTL").is_some()
}

fn mode(want: Want) -> Mode {
    let root = crate::sys::is_root();
    if launchd_host() {
        let system = match want {
            Want::Auto => root,
            Want::System => true,
            Want::User => false,
        };
        return Mode::Launchd { system };
    }
    if fleet::systemctl_bin().is_none() {
        return Mode::None;
    }
    Mode::Systemd(match want {
        Want::Auto => Scope::mine(),
        Want::System => Scope::System,
        Want::User => Scope::User,
    })
}

// -------------------------------------------------------------- generation

/// The variables a service needs to find what the CLI finds: `PATH` (bun,
/// node) when `path`, and whatever moves Warden's own directories.
fn carried_env(get: impl Fn(&str) -> Option<String>, path: bool) -> Vec<(String, String)> {
    let mut keys = vec!["WARDEN_HOME", "WARDEN_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_STATE_HOME"];
    if path {
        keys.insert(0, "PATH");
    }
    keys.into_iter().filter_map(|k| get(k).filter(|v| !v.is_empty()).map(|v| (k.to_string(), v))).collect()
}

fn env_now(k: &str) -> Option<String> {
    std::env::var(k).ok()
}

/// Text inside a double-quoted unit file value: `\` and `"` escaped, `%`
/// doubled (systemd would expand it as a specifier).
fn unit_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%")
}

fn unit_quote(s: &str) -> String {
    format!("\"{}\"", unit_escape(s))
}

/// A unit from one of the annotated templates in contrib/: its header
/// replaced by `header`, the lines `rewrite` returns replaced, and for the
/// user's manager everything that only works for the system's dropped
/// (`User=`, `Group=`, raising `LimitNOFILE=`, network-online.target, the
/// comments written for root).
fn render(
    template: &str,
    scope: Scope,
    header: &str,
    env: &[(String, String)],
    rewrite: impl Fn(&str) -> Option<String>,
) -> String {
    let mut out: Vec<String> = header.lines().map(String::from).collect();
    out.push(String::new());
    for line in template.lines().skip_while(|l| !l.starts_with('[')) {
        if let Some(new) = rewrite(line) {
            out.push(new);
            continue;
        }
        let key = line.split_once('=').map(|(k, _)| k.trim());
        if scope == Scope::User {
            if line.starts_with('#')
                || matches!(key, Some("User" | "Group" | "LimitNOFILE"))
                || line.contains("network-online.target")
            {
                continue;
            }
            if key == Some("WantedBy") {
                out.push("WantedBy=default.target".into());
                continue;
            }
        }
        if line.is_empty() && out.last().is_some_and(|l| l.is_empty()) {
            continue;
        }
        out.push(line.to_string());
        if line == "[Service]" {
            for (k, v) in env {
                out.push(format!("Environment={}", unit_quote(&format!("{k}={v}"))));
            }
        }
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

fn scope_word(scope: Scope) -> &'static str {
    match scope {
        Scope::System => "system",
        Scope::User => "user",
    }
}

/// `warden@.service`: one instance per app, reading `<config dir>/<app>.toml`.
pub(crate) fn warden_unit(scope: Scope, exe: &str, config_dir: &Path, env: &[(String, String)]) -> String {
    let config = format!("\"{}/%i.toml\"", unit_escape(&config_dir.display().to_string()));
    let header = format!(
        "# Written by `warden startup` ({} units): one instance per app, reading {}/<app>.toml.\n\
         # `warden unstartup` removes it; contrib/warden@.service has the annotated original.",
        scope_word(scope),
        config_dir.display()
    );
    render(WARDEN_UNIT, scope, &header, env, |line| {
        let exec = |verb: &str| format!("{} {verb} --config {config}", unit_quote(exe));
        match line.split_once('=').map(|(k, _)| k) {
            Some("ExecStart") => Some(format!("ExecStart={}", exec("start"))),
            Some("ExecReload") => Some(format!("ExecReload={}", exec("safe-reload"))),
            _ => None,
        }
    })
}

/// `wardend.service`. Never `--resurrect`: under systemd each app comes back
/// through its own unit, and resurrecting would start it twice.
pub(crate) fn wardend_unit(scope: Scope, exe: &str, env: &[(String, String)]) -> String {
    let header = format!(
        "# Written by `warden startup` ({} units): wardend, for live events and restarts of\n\
         # background supervisors. The apps come back through their own warden@<app> units.\n\
         # `warden unstartup` removes it; contrib/wardend.service has the annotated original.",
        scope_word(scope)
    );
    render(WARDEND_UNIT, scope, &header, env, |line| {
        line.starts_with("ExecStart=").then(|| format!("ExecStart={} daemon", unit_quote(exe)))
    })
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// The launchd job: `warden daemon --resurrect`, at load (login or boot) and
/// again whenever it did not exit cleanly.
pub(crate) fn launchd_plist(label: &str, exe: &str, log: &Path, env: &[(String, String)]) -> String {
    let log = xml(&log.display().to_string());
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `warden startup`; `warden unstartup` removes it. -->\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    s += &format!("  <key>Label</key>\n  <string>{}</string>\n", xml(label));
    s += "  <key>ProgramArguments</key>\n  <array>\n";
    for a in [exe, "daemon", "--resurrect"] {
        s += &format!("    <string>{}</string>\n", xml(a));
    }
    s += "  </array>\n  <key>RunAtLoad</key>\n  <true/>\n";
    s += "  <!-- Started again after a crash or kill -9, but not after a clean exit: `warden kill`\n       \
          and `warden daemon stop` keep it stopped until the next login or boot. -->\n";
    s += "  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n";
    s += &format!("  <key>StandardOutPath</key>\n  <string>{log}</string>\n");
    s += &format!("  <key>StandardErrorPath</key>\n  <string>{log}</string>\n");
    if !env.is_empty() {
        s += "  <key>EnvironmentVariables</key>\n  <dict>\n";
        for (k, v) in env {
            s += &format!("    <key>{}</key>\n    <string>{}</string>\n", xml(k), xml(v));
        }
        s += "  </dict>\n";
    }
    s += "</dict>\n</plist>\n";
    s
}

// ------------------------------------------------------------------ helpers

/// Run a program; the last line of its error output when it fails.
fn run(bin: &Path, args: &[&str]) -> Result<(), String> {
    let shown = format!("{} {}", bin.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(), args.join(" "));
    let out = std::process::Command::new(bin).args(args).output().map_err(|e| format!("`{shown}`: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let why =
        err.trim().lines().last().filter(|l| !l.is_empty()).map(String::from).unwrap_or_else(|| out.status.to_string());
    Err(format!("`{shown}` failed: {why}"))
}

/// Write a file unless it already has `text`; say which. `fix`: what to do
/// when it cannot be written.
fn install(path: &Path, text: &str, fix: &str) -> bool {
    if std::fs::read_to_string(path).is_ok_and(|t| t == text) {
        println!("{}: unchanged", path.display());
        return true;
    }
    match fleet::write_private(path, text, 0o644) {
        Ok(()) => {
            println!("{}: written", path.display());
            true
        }
        Err(e) => {
            eprintln!("warden: {e}; nothing was enabled.\n  {fix}");
            false
        }
    }
}

fn remove(path: &Path) -> i32 {
    match std::fs::remove_file(path) {
        Ok(()) => {
            println!("{}: removed", path.display());
            0
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => {
            eprintln!("warden: cannot remove {} ({e}); remove it by hand", path.display());
            1
        }
    }
}

fn exe() -> Result<String, i32> {
    fleet::own_exe().map(|p| p.display().to_string()).map_err(|e| {
        eprintln!("warden: {e}; nothing was installed");
        1
    })
}

/// Our user name: `$USER`, `$LOGNAME`, else /etc/passwd, else the uid.
fn user_name() -> String {
    let uid = crate::sys::uid();
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .filter(|u| !u.is_empty())
        .or_else(|| passwd_name(&std::fs::read_to_string("/etc/passwd").unwrap_or_default(), uid))
        .unwrap_or_else(|| uid.to_string())
}

fn passwd_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        (f.nth(1)?.parse::<u32>().ok()? == uid).then(|| name.to_string())
    })
}

fn nothing_saved_note() {
    if matches!(fleet::saved_apps(), Ok(None)) {
        println!(
            "note: nothing is saved yet ({} does not exist): once your apps run, `warden save` records \
             which ones come back after a reboot",
            fleet::dump_file().display()
        );
    }
}

/// A wardend started by `warden start` holds the socket: stop it so the
/// service's own can take over (apps keep running).
async fn hand_over_wardend(to: &str) {
    match crate::daemon::client::stop_daemon_process().await {
        Ok(Some(pid)) => println!("wardend: stopped the one running outside {to} (pid {pid}); {to} takes over"),
        Ok(None) => {}
        Err(e) => eprintln!("warden: wardend: {e}; {to} cannot start its own until it is stopped"),
    }
}

// ------------------------------------------------------------------ public

const NO_SERVICE_MANAGER: &str = "\
warden: no service manager found: systemd is not running here (a container, or another init system), \
so nothing was installed.
  To bring the saved apps back at boot, run one of these from your init system or the container's entrypoint:
    warden resurrect             start every app `warden save` recorded, in the background, then exit
    warden daemon --resurrect    the same, then stay in the foreground as wardend and restart their
                                 supervisors if they die (in a container: under an init, `docker run --init`)";

pub async fn startup(args: &Args, want: Want) -> i32 {
    match mode(want) {
        Mode::Systemd(scope) => systemd_startup(args, scope).await,
        Mode::Launchd { system } => launchd_startup(system).await,
        Mode::None => {
            eprintln!("{NO_SERVICE_MANAGER}");
            2
        }
    }
}

pub async fn unstartup(args: &Args, want: Want) -> i32 {
    match mode(want) {
        Mode::Systemd(scope) => systemd_unstartup(args, scope),
        Mode::Launchd { system } => launchd_unstartup(system),
        Mode::None => {
            eprintln!(
                "warden: no service manager found (systemd is not running here): `warden startup` installed nothing to remove"
            );
            2
        }
    }
}

/// What brings the saved apps back after a reboot here, if anything.
pub(crate) fn installed() -> Option<String> {
    if launchd_host() {
        return [false, true]
            .into_iter()
            .map(plist_path)
            .find(|p| p.exists())
            .map(|p| format!("the launchd job {}", p.display()));
    }
    if Scope::System.has_unit("warden@.service") {
        return Some("the systemd units (warden@.service)".into());
    }
    Scope::User.has_unit("warden@.service").then(|| "your systemd user units (warden@.service)".into())
}

/// Does a service manager run wardend here (so `warden start` leaves it be)?
pub(crate) fn wardend_managed() -> bool {
    if launchd_host() {
        return [false, true].into_iter().any(|s| plist_path(s).exists());
    }
    fleet::systemctl_bin().is_some() && [Scope::System, Scope::User].into_iter().any(|s| s.has_unit("wardend.service"))
}

/// Can `warden startup` do anything here?
pub(crate) fn available() -> bool {
    mode(Want::Auto) != Mode::None
}

// ----------------------------------------------------------------- systemd

fn sysctl_dir() -> PathBuf {
    env_path("WARDEN_SYSCTL_DIR").unwrap_or_else(|| PathBuf::from("/etc/sysctl.d"))
}

fn root_fix(scope: Scope) -> &'static str {
    match scope {
        Scope::System => {
            "System units need root: `sudo warden startup`, or `warden startup --user` for units of your own"
        }
        Scope::User => "Check that you own that directory (`ls -ld` it)",
    }
}

async fn systemd_startup(args: &Args, scope: Scope) -> i32 {
    let exe = match exe() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let dir = scope.unit_dir();
    let env = carried_env(env_now, scope == Scope::User);
    let units = [
        ("warden@.service", warden_unit(scope, &exe, &fleet::config_dir(), &env)),
        ("wardend.service", wardend_unit(scope, &exe, &env)),
    ];
    for (file, text) in &units {
        if !install(&dir.join(file), text, root_fix(scope)) {
            return 1;
        }
    }
    if scope == Scope::System {
        let sysctl = sysctl_dir().join("99-warden.conf");
        if !install(&sysctl, SYSCTL_CONF, root_fix(scope)) {
            return 1;
        }
        if env_path("WARDEN_SYSCTL_DIR").is_none() {
            match std::process::Command::new("sysctl").args(["-q", "-p"]).arg(&sysctl).status() {
                Ok(s) if s.success() => println!("applied {}", sysctl.display()),
                _ => eprintln!("warden: could not apply {} now; it applies at the next boot", sysctl.display()),
            }
        }
    }
    if let Err(e) = fleet::run_systemctl(scope, &["daemon-reload"]) {
        match scope {
            Scope::System => eprintln!(
                "warden: {e}\n  The units are written in {}, but nothing is enabled. Fix the error, then run \
                 `warden startup` again",
                dir.display()
            ),
            Scope::User => eprintln!(
                "warden: {e}\n  The units are written in {}, but your systemd user manager did not answer, so \
                 nothing is enabled. It runs only for a logged-in user: run `warden startup` from a login session \
                 (ssh in as {user}); if there is none, as root once: `loginctl enable-linger {user}`, then log in \
                 and run it again",
                dir.display(),
                user = user_name()
            ),
        }
        return 1;
    }
    let mut worst = enable_apps(args, scope).await;
    if !fleet::unit_active(scope, "wardend.service") {
        hand_over_wardend("wardend.service").await;
    }
    match fleet::run_systemctl(scope, &["enable", "--now", "wardend.service"]) {
        Ok(()) => println!(
            "wardend.service: enabled and started (live events, restarts of background supervisors; the apps come \
             back through their own units)"
        ),
        Err(e) => {
            eprintln!(
                "warden: {e}\n  The apps' units do not depend on wardend. `systemctl {}status wardend` and \
                 `journalctl {}-u wardend -n 50` show why",
                scope.shown(),
                scope.shown()
            );
            worst = 1;
        }
    }
    if scope == Scope::User {
        worst = worst.max(linger());
    }
    nothing_saved_note();
    worst
}

/// Enable `warden@<app>` for every saved app (every app, if none is saved).
async fn enable_apps(args: &Args, scope: Scope) -> i32 {
    let ctx = fleet::context(args);
    let apps: Vec<(String, Option<PathBuf>)> = match fleet::saved_apps() {
        Ok(Some(saved)) => saved.into_iter().map(|s| (s.name, Some(s.config))).collect(),
        _ => ctx.apps.iter().map(|a| (a.name.clone(), a.config.clone())).collect(),
    };
    let mut worst = 0;
    for (name, config) in apps {
        let expected = fleet::unit_config_path(&name);
        let matches = config.as_ref().is_some_and(|c| {
            std::fs::canonicalize(c).ok().is_some_and(|c| std::fs::canonicalize(&expected).ok() == Some(c))
        });
        if !matches {
            let actual = config.as_ref().map(|c| c.display().to_string()).unwrap_or_else(|| "unknown".into());
            eprintln!(
                "warden: {name}: warden@.service reads {}, but this app's config is {actual}, so it will not come \
                 back after a reboot. Fix: `ln -s {actual} {}`, then `warden startup` again",
                expected.display(),
                expected.display()
            );
            worst = 1;
            continue;
        }
        let unit = format!("warden@{name}.service");
        match fleet::run_systemctl(scope, &["enable", &unit]) {
            Ok(()) => println!("{name}: {unit} enabled at boot"),
            Err(e) => {
                eprintln!("warden: {name}: {e}");
                worst = 1;
                continue;
            }
        }
        let Some(app) = ctx.apps.iter().find(|a| a.name == name) else { continue };
        if let Ok(st) = fleet::status_of(app).await {
            if st.unit.is_none() {
                println!(
                    "{name}: running outside systemd now; it moves under systemd at the next boot, or now with \
                     `warden kill {name} --yes && systemctl {}start {unit}`",
                    scope.shown()
                );
            }
        }
    }
    worst
}

/// Lingering: the user's manager starts at boot and outlives their logins.
fn linger() -> i32 {
    let user = user_name();
    let dir = env_path("WARDEN_LINGER_DIR").unwrap_or_else(|| PathBuf::from("/var/lib/systemd/linger"));
    if dir.join(&user).exists() {
        println!("lingering is on for {user}: the apps start at boot, without a login");
        return 0;
    }
    let bin = env_path("WARDEN_LOGINCTL")
        .or_else(|| ["/usr/bin/loginctl", "/bin/loginctl"].iter().map(PathBuf::from).find(|p| p.exists()));
    let res = match bin {
        Some(b) => run(&b, &["enable-linger", &user]),
        None => Err("loginctl is not installed".into()),
    };
    match res {
        Ok(()) => {
            println!("lingering enabled for {user}: the apps start at boot and keep running after you log out");
            0
        }
        Err(e) => {
            eprintln!(
                "warden: could not turn on lingering for {user} ({e}).\n  Without it, systemd starts your apps only \
                 when you log in, and stops them when you log out. The units are installed and enabled.\n  \
                 Fix (needs root once): sudo loginctl enable-linger {user}"
            );
            1
        }
    }
}

fn systemd_unstartup(args: &Args, scope: Scope) -> i32 {
    let ctx = fleet::context(args);
    let mut worst = 0;
    for app in &ctx.apps {
        let unit = format!("warden@{}.service", app.name);
        match fleet::run_systemctl(scope, &["disable", &unit]) {
            Ok(()) => println!("{}: {unit} disabled (still running until stopped)", app.name),
            Err(e) => {
                eprintln!("warden: {}: {e}", app.name);
                worst = 1;
            }
        }
    }
    match fleet::run_systemctl(scope, &["disable", "--now", "wardend.service"]) {
        Ok(()) => println!("wardend.service: disabled and stopped (every app keeps running)"),
        Err(e) => {
            eprintln!("warden: {e}");
            worst = 1;
        }
    }
    let dir = scope.unit_dir();
    for file in ["warden@.service", "wardend.service"] {
        worst = worst.max(remove(&dir.join(file)));
    }
    let _ = fleet::run_systemctl(scope, &["daemon-reload"]);
    if scope == Scope::User {
        let user = user_name();
        println!("lingering stays on for {user}; `loginctl disable-linger {user}` turns it off");
    }
    worst
}

// ----------------------------------------------------------------- launchd

fn plist_path(system: bool) -> PathBuf {
    let dir = env_path("WARDEN_LAUNCHD_DIR").unwrap_or_else(|| {
        if system {
            PathBuf::from("/Library/LaunchDaemons")
        } else {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"));
            home.join("Library/LaunchAgents")
        }
    });
    dir.join(format!("{LAUNCHD_LABEL}.plist"))
}

fn launchctl() -> PathBuf {
    env_path("WARDEN_LAUNCHCTL").unwrap_or_else(|| PathBuf::from("/bin/launchctl"))
}

/// `system` (boot, no login needed) or the user's GUI session.
fn launchd_domain(system: bool) -> String {
    if system { "system".into() } else { format!("gui/{}", crate::sys::uid()) }
}

async fn launchd_startup(system: bool) -> i32 {
    let exe = match exe() {
        Ok(e) => e,
        Err(code) => return code,
    };
    let log = crate::daemon::log_path();
    if let Some(d) = log.parent() {
        if let Err(e) = std::fs::create_dir_all(d) {
            eprintln!("warden: creating {} for wardend's log: {e}; nothing was installed", d.display());
            return 1;
        }
    }
    let path = plist_path(system);
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let fix =
        if system { "LaunchDaemons need root: `sudo warden startup`" } else { "Check that you own that directory" };
    if !install(&path, &launchd_plist(LAUNCHD_LABEL, &exe, &log, &carried_env(env_now, true)), fix) {
        return 1;
    }
    let domain = launchd_domain(system);
    let target = format!("{domain}/{LAUNCHD_LABEL}");
    let lc = launchctl();
    if run(&lc, &["print", &target]).is_ok() {
        // Loaded already: load it again with this plist (wardend restarts; apps keep running).
        let _ = run(&lc, &["bootout", &target]);
    } else {
        hand_over_wardend("launchd").await;
    }
    // A job disabled earlier (`launchctl disable`) stays disabled until enabled.
    let _ = run(&lc, &["enable", &target]);
    let path_s = path.display().to_string();
    let mut res = run(&lc, &["bootstrap", &domain, &path_s]);
    for _ in 0..3 {
        // Right after a bootout the old job may still be on its way out.
        if res.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        res = run(&lc, &["bootstrap", &domain, &path_s]);
    }
    if let Err(e) = res {
        let fix = if system {
            "Fix the error, then run `sudo warden startup` again".to_string()
        } else {
            format!(
                "It loads at your next login to the Mac's desktop ({domain} needs that session). For apps that start \
                 at boot without anyone logging in: `sudo warden startup` (a LaunchDaemon)"
            )
        };
        eprintln!("warden: {e}.\n  {path_s} is written. {fix}");
        return 1;
    }
    println!(
        "{target}: loaded. launchd runs `warden daemon --resurrect` {}: wardend starts the apps `warden save` \
         recorded and restarts their supervisors when they die; launchd restarts wardend if it crashes. Log: {}",
        if system { "at boot" } else { "at login" },
        log.display()
    );
    nothing_saved_note();
    0
}

fn launchd_unstartup(system: bool) -> i32 {
    let target = format!("{}/{LAUNCHD_LABEL}", launchd_domain(system));
    let path = plist_path(system);
    match run(&launchctl(), &["bootout", &target]) {
        Ok(()) => println!("{target}: unloaded; wardend stopped, every app keeps running (`warden kill` stops them)"),
        Err(e) if path.exists() => println!("{target}: was not loaded ({e})"),
        Err(_) => {}
    }
    remove(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone())
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    #[test]
    fn system_units_keep_the_template_with_our_paths() {
        let u = warden_unit(Scope::System, "/opt/warden/bin/warden", Path::new("/etc/warden"), &[]);
        let l = lines(&u);
        assert!(l[0].starts_with("# Written by `warden startup` (system units)"), "{u}");
        assert!(l.contains(&r#"ExecStart="/opt/warden/bin/warden" start --config "/etc/warden/%i.toml""#), "{u}");
        assert!(l.contains(&r#"ExecReload="/opt/warden/bin/warden" safe-reload --config "/etc/warden/%i.toml""#));
        for kept in ["User=www-data", "LimitNOFILE=65536", "WantedBy=multi-user.target", "RuntimeDirectory=warden/%i"] {
            assert!(l.contains(&kept), "{kept} kept for system units:\n{u}");
        }
        assert!(!u.contains("/usr/local/bin/warden") && !u.contains("cp contrib"), "{u}");
        assert!(!u.contains("Environment="), "no environment for system units without overrides");
        let d = wardend_unit(Scope::System, "/opt/warden/bin/warden", &[]);
        assert!(lines(&d).contains(&r#"ExecStart="/opt/warden/bin/warden" daemon"#), "{d}");
        assert!(!d.contains("--resurrect"), "systemd brings the apps back itself");
        for kept in ["KillMode=process", "Restart=always", "RuntimeDirectoryPreserve=yes", "WantedBy=multi-user.target"]
        {
            assert!(lines(&d).contains(&kept), "{kept}:\n{d}");
        }
    }

    #[test]
    fn user_units_drop_what_only_the_system_manager_allows() {
        let env = carried_env(env(&[("PATH", "/home/me/.bun/bin:/usr/bin"), ("WARDEN_HOME", "/srv/100% apps")]), true);
        let u = warden_unit(Scope::User, "/home/me/bin/warden", Path::new("/home/me/.config/warden"), &env);
        let l = lines(&u);
        for gone in ["User=", "Group=", "LimitNOFILE=", "network-online", "multi-user.target"] {
            assert!(!u.contains(gone), "{gone} dropped for user units:\n{u}");
        }
        assert!(l.contains(&"WantedBy=default.target"), "{u}");
        assert!(l.contains(&r#"Environment="PATH=/home/me/.bun/bin:/usr/bin""#), "{u}");
        assert!(l.contains(&r#"Environment="WARDEN_HOME=/srv/100%% apps""#), "% is escaped: {u}");
        assert!(l.contains(&r#"ExecStart="/home/me/bin/warden" start --config "/home/me/.config/warden/%i.toml""#));
        // Environment goes into [Service], right after its header.
        let service = l.iter().position(|x| *x == "[Service]").unwrap();
        assert!(l[service + 1].starts_with("Environment=\"PATH="), "{u}");
        assert!(l.iter().skip(1).all(|x| !x.starts_with('#') || x.starts_with("# ")), "no root comments");
        assert!(!u.contains("\n\n\n"), "no runs of blank lines");
        let d = wardend_unit(Scope::User, "/home/me/bin/warden", &env);
        assert!(!d.contains("LimitNOFILE") && !d.contains("network-online") && d.contains("WantedBy=default.target"));
        assert!(d.contains("KillMode=process") && d.contains("RuntimeDirectory=warden"), "{d}");
    }

    #[test]
    fn unit_values_are_quoted() {
        assert_eq!(unit_quote(r#"/a b/"c"\d%i"#), r#""/a b/\"c\"\\d%%i""#);
        let u = warden_unit(Scope::System, "/x y/warden", Path::new("/etc/my warden"), &[]);
        assert!(u.contains(r#"ExecStart="/x y/warden" start --config "/etc/my warden/%i.toml""#), "{u}");
    }

    #[test]
    fn only_warden_directories_and_path_are_carried() {
        let get = env(&[("PATH", "/bin"), ("HOME", "/h"), ("WARDEN_RUNTIME_DIR", "/r"), ("XDG_STATE_HOME", "")]);
        assert_eq!(
            carried_env(&get, true),
            vec![("PATH".into(), "/bin".into()), ("WARDEN_RUNTIME_DIR".into(), "/r".into())]
        );
        assert_eq!(carried_env(&get, false), vec![("WARDEN_RUNTIME_DIR".to_string(), "/r".to_string())]);
    }

    #[test]
    fn launchd_plist_runs_wardend_with_resurrect() {
        let env = vec![("PATH".to_string(), "/opt/homebrew/bin:/usr/bin".to_string())];
        let p =
            launchd_plist(LAUNCHD_LABEL, "/Users/me/bin/warden", Path::new("/Users/me/state/logs/wardend.log"), &env);
        assert!(p.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"), "{p}");
        let flat: String = p.split_whitespace().collect();
        assert!(flat.contains("<key>Label</key><string>io.github.oceanwap.warden.daemon</string>"), "{p}");
        assert!(
            flat.contains(
                "<key>ProgramArguments</key><array><string>/Users/me/bin/warden</string><string>daemon</string>\
                 <string>--resurrect</string></array>"
            ),
            "{p}"
        );
        assert!(flat.contains("<key>RunAtLoad</key><true/>"), "{p}");
        assert!(flat.contains("<key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>"), "{p}");
        assert!(flat.contains("<key>StandardOutPath</key><string>/Users/me/state/logs/wardend.log</string>"));
        assert!(flat.contains("<key>StandardErrorPath</key><string>/Users/me/state/logs/wardend.log</string>"));
        assert!(
            flat.contains("<key>EnvironmentVariables</key><dict><key>PATH</key><string>/opt/homebrew/bin:/usr/bin")
        );
        // Tags balance (a cheap well-formedness check) and text is escaped.
        for tag in ["dict", "array", "plist"] {
            assert_eq!(p.matches(&format!("<{tag}")).count(), p.matches(&format!("</{tag}>")).count(), "{tag}");
        }
        let odd = launchd_plist("l", "/a&b/<w>", Path::new("/l"), &[]);
        assert!(odd.contains("<string>/a&amp;b/&lt;w&gt;</string>") && !odd.contains("EnvironmentVariables"));
    }

    #[test]
    fn user_names_from_passwd() {
        let p = "root:x:0:0:root:/root:/bin/sh\nbroken\nme:x:1000:1000::/home/me:/bin/bash\n";
        assert_eq!(passwd_name(p, 1000).as_deref(), Some("me"));
        assert_eq!(passwd_name(p, 0).as_deref(), Some("root"));
        assert_eq!(passwd_name(p, 5), None);
    }

    #[test]
    fn launchd_targets() {
        assert_eq!(launchd_domain(true), "system");
        assert_eq!(launchd_domain(false), format!("gui/{}", crate::sys::uid()));
        assert!(plist_path(false).ends_with("io.github.oceanwap.warden.daemon.plist"));
    }
}

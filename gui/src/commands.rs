//! The `warden` CLI as a subprocess, for what wardend's socket does not do:
//! adding an app (`warden start`), checking a config (`warden check`) and
//! starting wardend itself. On a remote host the same commands run through
//! `ssh <host> <quoted command line>`.
//!
//! Subprocesses run on the async executor (never the UI thread), with a
//! time limit, and their output is shown as it is.

use crate::ssh;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Where commands run.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Host {
    Local,
    /// `dest`: what ssh connects to; `warden`: the CLI there (on PATH, or a path).
    Ssh {
        dest: String,
        warden: String,
    },
}

impl Host {
    pub fn is_local(&self) -> bool {
        matches!(self, Host::Local)
    }
}

/// What a command printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    /// The command line, quoted for a shell (to copy and run by hand).
    pub command: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// stdout then stderr, trimmed.
    pub fn text(&self) -> String {
        let mut t = self.stdout.trim_end().to_string();
        let e = self.stderr.trim_end();
        if !e.is_empty() {
            if !t.is_empty() {
                t.push('\n');
            }
            t.push_str(e);
        }
        t
    }
}

static LOCAL_WARDEN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Use this `warden` binary on this machine (`--warden`). First call wins.
pub fn set_local_warden(path: PathBuf) {
    let _ = LOCAL_WARDEN.set(path);
}

/// The `warden` binary: `--warden` if given, else next to this program (the
/// GUI + CLI bundle, `Warden.app/Contents/MacOS`), else on PATH.
pub fn find_local_warden() -> Result<PathBuf, String> {
    if let Some(p) = LOCAL_WARDEN.get() {
        return if is_executable(p) {
            Ok(p.clone())
        } else {
            Err(format!("--warden {}: not an executable file", p.display()))
        };
    }
    let exe = std::env::current_exe().ok();
    let beside = exe.as_deref().and_then(Path::parent).map(|d| d.join("warden"));
    if let Some(p) = beside.as_ref().filter(|p| is_executable(p)) {
        return Ok(p.clone());
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("warden");
            if is_executable(&p) {
                return Ok(p);
            }
        }
    }
    Err(format!(
        "the warden CLI was not found next to the GUI ({}) or on PATH: install it (install.sh, or the GUI + CLI \
         download), or put it on PATH",
        beside.and_then(|p| p.parent().map(|d| d.display().to_string())).unwrap_or_else(|| "?".into())
    ))
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Program and arguments that run `warden <args>` on `host`.
pub fn warden_command(host: &Host, args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    match host {
        Host::Local => Ok((find_local_warden()?, args.to_vec())),
        Host::Ssh { dest, warden } => {
            let mut argv = vec![warden.clone()];
            argv.extend(args.iter().cloned());
            Ok((PathBuf::from("ssh"), ssh::exec_args(dest, &argv)?))
        }
    }
}

/// Program and arguments that run `argv` (any command) on `host`.
fn host_command(host: &Host, argv: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    match host {
        Host::Local => {
            let (prog, rest) = argv.split_first().ok_or("no command")?;
            Ok((PathBuf::from(prog), rest.to_vec()))
        }
        Host::Ssh { dest, .. } => Ok((PathBuf::from("ssh"), ssh::exec_args(dest, argv)?)),
    }
}

/// Run a program; `stdin` is written to it, `env` added to its environment.
pub async fn run(
    program: &Path,
    args: &[String],
    stdin: Option<&str>,
    env: &[(String, String)],
    limit: Duration,
) -> Result<Output, String> {
    let mut shown = vec![program.display().to_string()];
    shown.extend(args.iter().cloned());
    let command = ssh::shell_join(&shown);
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("cannot run {}: not found", program.display()),
        _ => format!("cannot run {}: {e}", program.display()),
    })?;
    if let (Some(text), Some(mut w)) = (stdin, child.stdin.take()) {
        w.write_all(text.as_bytes()).await.map_err(|e| format!("writing to {command} failed: {e}"))?;
        drop(w);
    }
    let out = tokio::time::timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| format!("`{command}` did not finish within {} s; it was stopped", limit.as_secs()))?
        .map_err(|e| format!("waiting for `{command}` failed: {e}"))?;
    Ok(Output {
        command,
        ok: out.status.success(),
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// `warden <args>` on `host`. Err also when ssh could not run it there.
pub async fn run_warden(
    host: &Host,
    args: &[String],
    stdin: Option<&str>,
    env: &[(String, String)],
    limit: Duration,
) -> Result<Output, String> {
    let (prog, argv) = warden_command(host, args)?;
    let out = run(&prog, &argv, stdin, env, limit).await?;
    match host {
        Host::Ssh { warden, .. } => remote_failure(host, &out, warden).map_or(Ok(out), Err),
        Host::Local => Ok(out),
    }
}

/// A remote command that did not get to answer: ssh exits 255 when it
/// fails itself (connect, host key, login), and the remote shell 127 when
/// it does not find `program`. The error to show instead of the output.
pub fn remote_failure(host: &Host, out: &Output, program: &str) -> Option<String> {
    let Host::Ssh { dest, .. } = host else { return None };
    match out.code {
        Some(255) => Some(ssh::explain(dest, &out.stderr, Some(255))),
        Some(127) => {
            let said = out.stderr.trim().lines().last().unwrap_or("").trim();
            Some(format!(
                "{program} was not found on {dest} ({said}): install warden there, or set the remote warden to its \
                 full path (Connection…, or --remote-warden; e.g. ~/.local/bin/warden). Commands over ssh run \
                 without your login profile, so their PATH may not have it"
            ))
        }
        _ => None,
    }
}

// ------------------------------------------------------------ start wardend

/// `warden daemon --background`.
pub async fn start_wardend(host: &Host) -> Result<Output, String> {
    run_warden(host, &["daemon".into(), "--background".into()], None, &[], Duration::from_secs(20)).await
}

// ----------------------------------------------------------------- add app

/// The "Add app" form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddApp {
    /// A script (`server.js`), a program, or a whole command line (`python3 -m http.server`).
    pub what: String,
    pub name: String,
    /// A number, `max`, or empty (one).
    pub instances: String,
    /// Empty: none (not an HTTP app).
    pub port: String,
    /// A `KEY=VALUE` file, on this machine.
    pub env_file: String,
}

impl AddApp {
    /// `start <what> --name <name> [-i <n>] [--port <p>]`, after checking each field.
    pub fn args(&self) -> Result<Vec<String>, String> {
        let what = self.what.trim();
        if what.is_empty() {
            return Err("enter a script (server.js), a program, or a command line to run".into());
        }
        if what.starts_with('-') {
            return Err(format!("{what:?} starts with '-', which warden would read as an option"));
        }
        let name = self.name.trim();
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')) {
            return Err(format!("the name {name:?} must be non-empty and use only letters, digits, '.', '_' and '-'"));
        }
        let mut a = vec!["start".to_string(), what.to_string(), "--name".into(), name.to_string()];
        let i = self.instances.trim();
        if !i.is_empty() {
            if i != "max" && !i.parse::<u32>().is_ok_and(|n| n > 0) {
                return Err(format!("instances {i:?}: a number (1 or more) or \"max\""));
            }
            a.extend(["-i".into(), i.to_string()]);
        }
        let p = self.port.trim();
        if !p.is_empty() {
            if !p.parse::<u16>().is_ok_and(|n| n > 0) {
                return Err(format!("port {p:?}: a number from 1 to 65535"));
            }
            a.extend(["--port".into(), p.to_string()]);
        }
        Ok(a)
    }
}

/// What adding an app did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub output: Output,
    /// Lines about the env file (read, recorded in the config).
    pub notes: Vec<String>,
}

/// Run `warden start` for the form. The env file's variables go into the
/// supervisor's environment (never onto a command line or into the
/// config), and `env_file` is then added to the new config so restarts from
/// the config (units, `resurrect`) keep them.
pub async fn add_app(host: &Host, form: &AddApp) -> Result<Added, String> {
    let args = form.args()?;
    let env_path = form.env_file.trim();
    let mut notes = Vec::new();
    let mut env = Vec::new();
    let mut env_abs = None;
    if !env_path.is_empty() {
        if !host.is_local() {
            return Err("an env file can be given for this machine only; on a remote host add `env_file = \"…\"` to \
                        the app's config (Edit config) and reload"
                .into());
        }
        let abs = std::path::absolute(env_path).map_err(|e| format!("env file {env_path}: {e}"))?;
        let text = tokio::fs::read_to_string(&abs)
            .await
            .map_err(|e| format!("cannot read the env file {}: {e}", abs.display()))?;
        env = parse_env_file(&text).map_err(|e| format!("env file {}: {e}", abs.display()))?;
        notes.push(format!("{} variables from {}", env.len(), abs.display()));
        env_abs = Some(abs);
    }
    // `warden start` waits until the app is up (or says why not).
    let output = run_warden(host, &args, None, &env, Duration::from_secs(180)).await?;
    if let (true, Some(abs)) = (output.ok, env_abs) {
        match written_config(&output.stdout) {
            Some(cfg) => match record_env_file(host, &cfg, &abs).await {
                Ok(()) => notes.push(format!("added env_file = {:?} to {}", abs.display().to_string(), cfg.display())),
                Err(e) => notes.push(e),
            },
            None => notes.push(format!(
                "add env_file = \"{}\" to [app] in the app's config (Edit config) so a restart from the config keeps \
                 the variables",
                abs.display()
            )),
        }
    }
    Ok(Added { output, notes })
}

/// The config `warden start` wrote: its `<name>: wrote <path> (…` line.
pub fn written_config(stdout: &str) -> Option<PathBuf> {
    stdout.lines().find_map(|l| {
        let rest = &l[l.find(": wrote ")? + ": wrote ".len()..];
        let path = rest.split(" (").next()?.trim();
        (!path.is_empty()).then(|| PathBuf::from(path))
    })
}

/// Add `env_file = "<path>"` to `[app]` in `cfg`, keeping it only if `warden check` accepts it.
async fn record_env_file(host: &Host, cfg: &Path, env_file: &Path) -> Result<(), String> {
    let why = |e: String| format!("could not add env_file to {} ({e}); add it by hand with Edit config", cfg.display());
    let text = tokio::fs::read_to_string(cfg).await.map_err(|e| why(e.to_string()))?;
    let new = with_env_file(&text, &env_file.display().to_string()).ok_or_else(|| why("no [app] section".into()))?;
    save_config(host, &cfg.display().to_string(), &new).await.map(|_| ()).map_err(why)
}

/// `text` with `env_file = "<path>"` as the first line of `[app]`; `None`
/// without an `[app]` section or when it already names an env file.
pub fn with_env_file(text: &str, path: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len() + path.len() + 16);
    let mut done = false;
    let mut in_app = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_app = t == "[app]";
        } else if in_app && t.starts_with("env_file") {
            return None;
        }
        out.push_str(line);
        out.push('\n');
        if t == "[app]" && !done {
            out.push_str(&format!("env_file = {}\n", toml_string(path)));
            done = true;
        }
    }
    done.then_some(out)
}

/// A TOML basic string.
pub fn toml_string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\t' => o.push_str("\\t"),
            '\r' => o.push_str("\\r"),
            c if c.is_control() => o.push_str(&format!("\\u{:04X}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// `KEY=VALUE` lines, with the rules of Warden's `env_file` (systemd's
/// EnvironmentFile and dotenv): blank lines and `#` comments skipped, an
/// optional `export `, values optionally in double quotes (with \n, \t, \r,
/// \", \\ escapes) or single quotes (literal).
pub fn parse_env_file(text: &str) -> Result<Vec<(String, String)>, String> {
    let mut out: Vec<(String, String)> = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else {
            return Err(format!("line {}: expected KEY=VALUE", i + 1));
        };
        let k = k.trim();
        if k.is_empty()
            || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || k.starts_with(|c: char| c.is_ascii_digit())
        {
            return Err(format!("line {}: {k:?} is not a valid variable name", i + 1));
        }
        let v = v.trim();
        let value = if let Some(inner) = v.strip_prefix('"') {
            let Some(inner) = inner.strip_suffix('"') else {
                return Err(format!("line {}: unterminated double quote", i + 1));
            };
            let mut s = String::with_capacity(inner.len());
            let mut chars = inner.chars();
            while let Some(c) = chars.next() {
                if c != '\\' {
                    s.push(c);
                    continue;
                }
                match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some(o) => s.push(o),
                    None => s.push('\\'),
                }
            }
            s
        } else if let Some(inner) = v.strip_prefix('\'') {
            match inner.strip_suffix('\'') {
                Some(s) => s.to_string(),
                None => return Err(format!("line {}: unterminated single quote", i + 1)),
            }
        } else {
            v.to_string()
        };
        match out.iter_mut().find(|(n, _)| n == k) {
            Some(slot) => slot.1 = value,
            None => out.push((k.to_string(), value)),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------- edit a config

/// Read an app's config (`AppEntry.config`) on `host`.
pub async fn read_config(host: &Host, path: &str) -> Result<String, String> {
    match host {
        Host::Local => tokio::fs::read_to_string(path).await.map_err(|e| format!("cannot read {path}: {e}")),
        Host::Ssh { dest, .. } => {
            let argv = vec!["cat".to_string(), "--".into(), path.to_string()];
            let (prog, args) = host_command(host, &argv)?;
            let out = run(&prog, &args, None, &[], Duration::from_secs(30)).await?;
            if let Some(e) = remote_failure(host, &out, "cat") {
                return Err(e);
            }
            if out.ok { Ok(out.stdout) } else { Err(format!("cannot read {path} on {dest}: {}", out.text())) }
        }
    }
}

/// A temporary file next to `path` (same directory: relative paths in the
/// config resolve the same).
fn temp_beside(path: &str) -> String {
    let p = Path::new(path);
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "config.toml".into());
    let tmp = format!(".{name}.warden-gui-{}.toml", std::process::id());
    match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.join(tmp).display().to_string(),
        _ => tmp,
    }
}

/// `warden check -c` on `text` as the config at `path` (written to a
/// temporary file beside it). Ok: warden's summary; Err: its error.
pub async fn check_config(host: &Host, path: &str, text: &str) -> Result<String, String> {
    config_script(host, path, text, false).await
}

/// Check `text`, then write it to `path` (mode and owner kept).
pub async fn save_config(host: &Host, path: &str, text: &str) -> Result<String, String> {
    config_script(host, path, text, true).await
}

async fn config_script(host: &Host, path: &str, text: &str, save: bool) -> Result<String, String> {
    let tmp = temp_beside(path);
    match host {
        Host::Local => {
            let out = {
                write_private(&tmp, text).await?;
                let r =
                    run_warden(host, &["check".into(), "-c".into(), tmp.clone()], None, &[], Duration::from_secs(30))
                        .await;
                let _ = tokio::fs::remove_file(&tmp).await;
                r?
            };
            if !out.ok {
                return Err(check_failed(&out, &tmp, path));
            }
            if save {
                write_in_place(path, text).await?;
            }
            Ok(summary(&out, &tmp, path))
        }
        Host::Ssh { warden, .. } => {
            let script = remote_config_script(&tmp, warden, path, save);
            let (prog, args) = host_command(host, &["sh".into(), "-c".into(), script])?;
            let out = run(&prog, &args, Some(text), &[], Duration::from_secs(30)).await?;
            if let Some(e) = remote_failure(host, &out, warden) {
                return Err(e);
            }
            match out.code {
                Some(0) => Ok(summary(&out, &tmp, path)),
                Some(125) => Err(format!("cannot write a temporary file beside {path}: {}", out.text())),
                Some(126) => Err(format!("the config is valid but writing {path} failed: {}", out.text())),
                _ => Err(check_failed(&out, &tmp, path)),
            }
        }
    }
}

/// One remote shell for Validate and Save: write the temporary file from
/// stdin, `warden check` it, then (Save) copy it over the config in place,
/// so its mode and owner stay. Exit 125: no temporary file; 126: the write
/// failed; else `warden check`'s status.
pub fn remote_config_script(tmp: &str, warden: &str, path: &str, save: bool) -> String {
    let q = ssh::shell_quote;
    format!(
        "umask 077; t={t}; cat > \"$t\" || exit 125; {w} check -c \"$t\"; rc=$?; \
         if [ $rc -eq 0 ] && [ {save} = 1 ]; then cat \"$t\" > {p} || rc=126; fi; rm -f \"$t\"; exit $rc",
        t = q(tmp),
        w = ssh::shell_program(warden),
        p = q(path),
        save = u8::from(save),
    )
}

/// warden's words, about `path` rather than the temporary file.
fn summary(out: &Output, tmp: &str, path: &str) -> String {
    out.text().replace(tmp, path)
}

fn check_failed(out: &Output, tmp: &str, path: &str) -> String {
    let said = out.text().replace(tmp, path);
    let said = said.strip_prefix("warden: ").unwrap_or(&said).to_string();
    if said.is_empty() {
        format!("`warden check` rejected the config (exit {:?}) without saying why", out.code)
    } else {
        format!("the config is not valid: {said}")
    }
}

async fn write_private(path: &str, text: &str) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = path.to_string();
    let text = text.to_string();
    // A blocking file write, off the UI thread.
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| format!("cannot write a temporary file {path} to check the config: {e}"))?;
        f.write_all(text.as_bytes()).map_err(|e| format!("writing {path} failed: {e}"))
    })
    .await
    .map_err(|e| format!("writing the temporary file failed: {e}"))?
}

async fn write_in_place(path: &str, text: &str) -> Result<(), String> {
    tokio::fs::write(path, text).await.map_err(|e| format!("the config is valid but writing {path} failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(what: &str, name: &str, i: &str, port: &str) -> AddApp {
        AddApp { what: what.into(), name: name.into(), instances: i.into(), port: port.into(), env_file: String::new() }
    }

    #[test]
    fn add_app_command_lines() {
        assert_eq!(
            form("server.js", "api", "4", "3000").args().unwrap(),
            ["start", "server.js", "--name", "api", "-i", "4", "--port", "3000"]
        );
        // A command line stays one argument: warden decides how to run it.
        assert_eq!(
            form(" python3 -m http.server 8000 ", "files", "", "").args().unwrap(),
            ["start", "python3 -m http.server 8000", "--name", "files"]
        );
        assert_eq!(form("app.ts", "web", "max", "").args().unwrap()[4..], ["-i", "max"]);
        assert!(form("", "api", "", "").args().unwrap_err().contains("script"));
        assert!(form("--help", "api", "", "").args().unwrap_err().contains("option"));
        assert!(form("a.js", "my app", "", "").args().unwrap_err().contains("name"));
        assert!(form("a.js", "api", "0", "").args().unwrap_err().contains("instances"));
        assert!(form("a.js", "api", "two", "").args().unwrap_err().contains("instances"));
        assert!(form("a.js", "api", "", "70000").args().unwrap_err().contains("port"));
    }

    #[test]
    fn remote_add_app_is_quoted_for_the_remote_shell() {
        let host = Host::Ssh { dest: "deploy@web-1".into(), warden: "/home/deploy/.local/bin/warden".into() };
        let args = form("node server.js --flag 'x'", "api", "2", "3000").args().unwrap();
        let (prog, argv) = warden_command(&host, &args).unwrap();
        assert_eq!(prog, PathBuf::from("ssh"));
        assert_eq!(
            argv.last().unwrap(),
            r"/home/deploy/.local/bin/warden start 'node server.js --flag '\''x'\''' --name api -i 2 --port 3000"
        );
    }

    #[test]
    fn remote_failures_are_errors_with_the_fix() {
        let out = |code: i32, stderr: &str| Output {
            command: "ssh".into(),
            ok: code == 0,
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        };
        let ssh = Host::Ssh { dest: "web-1".into(), warden: "warden".into() };
        let e = remote_failure(&ssh, &out(255, "web-1: Permission denied (publickey)."), "warden").unwrap();
        assert!(e.contains("Permission denied") && e.contains("ssh-add"), "{e}");
        let e = remote_failure(&ssh, &out(127, "bash: line 1: warden: command not found\n"), "warden").unwrap();
        assert!(e.contains("warden was not found on web-1 (bash: line 1: warden: command not found)"), "{e}");
        assert!(e.contains("--remote-warden"), "{e}");
        assert_eq!(remote_failure(&ssh, &out(1, "warden: bad config"), "warden"), None, "warden's own answer");
        assert_eq!(remote_failure(&Host::Local, &out(255, ""), "warden"), None);
    }

    #[test]
    fn env_files_parse_like_warden() {
        let e = parse_env_file("# comment\n\nexport A=1\nB = \"two words\\n\"\nC='lit\\n'\nD=\nA=override\n").unwrap();
        assert_eq!(
            e,
            [
                ("A".into(), "override".into()),
                ("B".into(), "two words\n".into()),
                ("C".into(), "lit\\n".into()),
                ("D".into(), String::new())
            ]
        );
        assert!(parse_env_file("NOEQUALS\n").unwrap_err().contains("line 1"));
        assert!(parse_env_file("1BAD=x\n").unwrap_err().contains("not a valid variable name"));
        assert!(parse_env_file("A=\"open\n").unwrap_err().contains("unterminated"));
    }

    #[test]
    fn env_file_goes_into_app_section() {
        let cfg = "[app]\nname = \"api\"\n\n[workers]\ncount = 2\n";
        let out = with_env_file(cfg, "/srv/api/.env").unwrap();
        assert_eq!(out, "[app]\nenv_file = \"/srv/api/.env\"\nname = \"api\"\n\n[workers]\ncount = 2\n");
        assert!(with_env_file(&out, "/other").is_none(), "already has one");
        assert!(with_env_file("[workers]\n", "/x").is_none());
        assert_eq!(toml_string("a\"b\\c\n"), r#""a\"b\\c\n""#);
        assert_eq!(
            written_config(
                "api: wrote /home/u/.config/warden/api.toml (edit it for health checks, limits and more)\napi: online"
            ),
            Some(PathBuf::from("/home/u/.config/warden/api.toml"))
        );
        assert_eq!(written_config("api: online"), None);
    }

    #[test]
    fn temp_files_sit_beside_the_config() {
        let t = temp_beside("/etc/warden/api.toml");
        assert!(t.starts_with("/etc/warden/.api.toml.warden-gui-") && t.ends_with(".toml"), "{t}");
    }

    #[tokio::test]
    async fn the_remote_config_script_checks_then_writes_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("wg-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sub = dir.join("it's here");
        std::fs::create_dir_all(&sub).unwrap();
        // A stand-in for `warden check -c`: "bad" configs fail.
        let fake = sub.join("fake warden");
        std::fs::write(
            &fake,
            "#!/bin/sh\n[ \"$1\" = check ] && [ \"$2\" = -c ] || exit 2\n\
             if grep -q bad \"$3\"; then echo \"warden: $3: bad config\" >&2; exit 1; fi\necho \"$3: ok\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cfg = sub.join("api.toml");
        std::fs::write(&cfg, "old\n").unwrap();
        std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (path, warden) = (cfg.display().to_string(), fake.display().to_string());
        let tmp = temp_beside(&path);
        let sh = |save: bool, text: &str| {
            let script = remote_config_script(&tmp, &warden, &path, save);
            let text = text.to_string();
            async move {
                run(Path::new("sh"), &["-c".into(), script], Some(&text), &[], Duration::from_secs(5)).await.unwrap()
            }
        };
        let out = sh(false, "good\n").await;
        assert_eq!(out.code, Some(0), "{}", out.text());
        assert_eq!(summary(&out, &tmp, &path), format!("{path}: ok"));
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "old\n", "Validate never writes");
        let out = sh(true, "bad\n").await;
        assert_eq!(out.code, Some(1));
        assert!(check_failed(&out, &tmp, &path).contains(&format!("{path}: bad config")), "{}", out.text());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "old\n");
        let out = sh(true, "new\n").await;
        assert_eq!(out.code, Some(0), "{}", out.text());
        assert_eq!(std::fs::read_to_string(&cfg).unwrap(), "new\n");
        assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o644, "mode kept");
        assert!(!Path::new(&tmp).exists(), "no temporary file left");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commands_run_off_thread_with_output_and_limits() {
        let out = run(
            Path::new("sh"),
            &["-c".into(), "echo out; echo err >&2; exit 3".into()],
            None,
            &[("X".into(), "1".into())],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!((out.ok, out.code, out.text().as_str()), (false, Some(3), "out\nerr"));
        assert_eq!(out.command, "sh -c 'echo out; echo err >&2; exit 3'");
        let cat = run(
            Path::new("sh"),
            &["-c".into(), "cat; echo $X".into()],
            Some("in\n"),
            &[("X".into(), "v w".into())],
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        assert_eq!(cat.stdout, "in\nv w\n");
        let slow = run(Path::new("sleep"), &["5".into()], None, &[], Duration::from_millis(100)).await.unwrap_err();
        assert!(slow.contains("did not finish"), "{slow}");
        let missing = run(Path::new("/no/such/prog"), &[], None, &[], Duration::from_secs(1)).await.unwrap_err();
        assert!(missing.contains("not found"), "{missing}");
    }
}

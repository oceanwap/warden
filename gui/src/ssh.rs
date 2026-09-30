//! A remote host over SSH: wardend has no TCP listener, so the GUI forwards
//! a local Unix socket to the remote `wardend.sock` (OpenSSH forwards Unix
//! sockets) and speaks the same protocol through it. Commands the GUI runs
//! on the remote host (`warden start …`) go through `ssh <host> <command>`,
//! where the remote shell parses the command line: every argument is quoted.
//!
//! No password ever passes through the GUI: ssh runs with `BatchMode=yes`
//! and uses the user's agent and keys, and its own error text is shown.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncBufReadExt;

/// Where wardend listens on a host where it runs as root (`sudo warden startup`).
pub fn default_remote_socket() -> String {
    warden_protocol::paths::root_wardend_socket().display().to_string()
}

/// How long ssh gets to log in and set up the forward.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(20);

/// A remote wardend: `user@host` (anything ssh accepts, `~/.ssh/config`
/// aliases included) and its socket path there.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Target {
    pub dest: String,
    pub remote_socket: String,
}

/// `user@host`, `host`, `host.example.com`, an ssh_config alias. Never
/// something ssh would read as an option.
pub fn validate_dest(dest: &str) -> Result<(), String> {
    if dest.is_empty() {
        return Err("the SSH target is empty: enter user@host (or a Host alias from ~/.ssh/config)".into());
    }
    if dest.starts_with('-') {
        return Err(format!(
            "the SSH target {dest:?} starts with '-', which ssh would read as an option: enter user@host"
        ));
    }
    if dest.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("the SSH target {dest:?} contains spaces: enter user@host without them"));
    }
    Ok(())
}

/// A socket path for `-L local:remote`: absolute, and without the `:` that
/// separates the two.
pub fn validate_socket_path(path: &str, which: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("the {which} socket path {path:?} must be absolute (e.g. /run/warden/wardend.sock)"));
    }
    if path.contains(':') || path.chars().any(|c| c.is_control()) {
        return Err(format!(
            "the {which} socket path {path:?} contains ':' or a control character, which ssh -L cannot forward"
        ));
    }
    // sun_path is 104 bytes on macOS, 108 on Linux.
    if path.len() > 100 {
        return Err(format!("the {which} socket path is {} bytes long; Unix sockets allow about 100", path.len()));
    }
    Ok(())
}

/// Options for every ssh the GUI runs: never prompt (agent and keys only),
/// give up on a dead network, fail fast.
fn common_options() -> Vec<String> {
    ["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=15", "-o", "ServerAliveCountMax=3"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `ssh -N -T … -L <local>:<remote> -- <dest>`.
pub fn tunnel_args(t: &Target, local: &Path) -> Result<Vec<String>, String> {
    validate_dest(&t.dest)?;
    validate_socket_path(&t.remote_socket, "remote")?;
    let local = local.to_str().ok_or_else(|| format!("the local socket path {} is not UTF-8", local.display()))?;
    validate_socket_path(local, "local")?;
    let mut a: Vec<String> = vec!["-N".into(), "-T".into()];
    a.extend(common_options());
    for o in ["ExitOnForwardFailure=yes", "StreamLocalBindUnlink=yes", "StreamLocalBindMask=0177"] {
        a.push("-o".into());
        a.push(o.into());
    }
    a.push("-L".into());
    a.push(format!("{local}:{}", t.remote_socket));
    a.push("--".into());
    a.push(t.dest.clone());
    Ok(a)
}

/// `ssh -T … -- <dest> '<quoted command line>'`: run `argv` on the remote
/// host, each argument exactly as given (the remote shell sees it quoted).
pub fn exec_args(dest: &str, argv: &[String]) -> Result<Vec<String>, String> {
    validate_dest(dest)?;
    if argv.is_empty() {
        return Err("no remote command".into());
    }
    let mut a: Vec<String> = vec!["-T".into()];
    a.extend(common_options());
    a.push("--".into());
    a.push(dest.to_string());
    let (prog, args) = argv.split_first().ok_or("no remote command")?;
    let mut line = shell_program(prog);
    for arg in args {
        line.push(' ');
        line.push_str(&shell_quote(arg));
    }
    a.push(line);
    Ok(a)
}

/// POSIX shell quoting: plain words stay as they are, anything else goes in
/// single quotes (a `'` inside becomes `'\''`).
pub fn shell_quote(s: &str) -> String {
    // A leading `=` expands in zsh (`=cmd`): quote it.
    let plain = !s.is_empty()
        && !s.starts_with('=')
        && s.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ',' | ':' | '@' | '%' | '+' | '=')
        });
    if plain {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// A program for a POSIX shell: quoted, except that a leading `~/` still
/// means the home directory (`~/.local/bin/warden`).
pub fn shell_program(p: &str) -> String {
    match p.strip_prefix("~/") {
        Some(rest) => format!("\"$HOME\"/{}", shell_quote(rest)),
        None => shell_quote(p),
    }
}

/// A command line for a POSIX shell, every argument quoted.
pub fn shell_join<S: AsRef<str>>(argv: &[S]) -> String {
    argv.iter().map(|a| shell_quote(a.as_ref())).collect::<Vec<_>>().join(" ")
}

/// ssh failed: what it said, and how to fix the usual causes.
pub fn explain(dest: &str, stderr: &str, exit: Option<i32>) -> String {
    let said = stderr.trim();
    let last = said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let fix = if said.contains("Permission denied") {
        format!(
            "the GUI logs in with your SSH agent and keys only, never a password: `ssh-add` your key, and check that \
             `ssh {dest}` works in a terminal without a password prompt"
        )
    } else if said.contains("Host key verification failed") || said.contains("REMOTE HOST IDENTIFICATION HAS CHANGED") {
        format!("connect once with `ssh {dest}` in a terminal to check and accept the host key")
    } else if said.contains("Could not resolve hostname") {
        format!("check the host name in {dest:?} (or add a Host entry to ~/.ssh/config)")
    } else if said.contains("Connection refused") || said.contains("timed out") || said.contains("No route to host") {
        "check that the host is up and that sshd listens (port 22, or the Port in ~/.ssh/config)".into()
    } else if said.contains("open failed") || said.contains("connect failed") {
        "wardend does not answer on the remote socket: start it there (`warden daemon --background`, or the \
         Start wardend button) or fix the remote socket path (root: /run/warden/wardend.sock; a user: \
         /run/user/<uid>/warden/wardend.sock)"
            .into()
    } else if said.contains("forwarding") || said.contains("bind") {
        "ssh could not set up the socket forward: check the local and remote socket paths".into()
    } else {
        format!("check that `ssh {dest}` works in a terminal")
    };
    let code = exit.map(|c| format!(" (exit {c})")).unwrap_or_default();
    if last.is_empty() {
        format!("ssh to {dest} failed{code} without saying why; {fix}")
    } else {
        format!("ssh to {dest} failed{code}: {last}; {fix}")
    }
}

/// A private directory for the local ends of tunnels: whoever can connect
/// to that socket controls the remote wardend.
pub fn private_dir() -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = rustix::process::geteuid().as_raw();
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let dir = base.join(format!("warden-gui-{uid}"));
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("cannot create {} for the SSH tunnel: {e}", dir.display()))?;
    }
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() || !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Err(format!(
            "{} must be a directory only you can use (mode 0700, owned by uid {uid}); remove it and try again",
            dir.display()
        ));
    }
    Ok(dir)
}

/// A fresh path for a tunnel's local socket.
pub fn local_socket() -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    Ok(private_dir()?.join(format!("{}-{n}.sock", std::process::id())))
}

/// A running `ssh -N -L …`. Dropping it kills ssh and removes the socket.
pub struct Tunnel {
    child: tokio::process::Child,
    pub local: PathBuf,
    pub dest: String,
    stderr: Arc<Mutex<VecDeque<String>>>,
}

impl Tunnel {
    /// Start ssh and wait until the local socket exists.
    pub async fn open(t: &Target) -> Result<Tunnel, String> {
        let local = local_socket()?;
        let args = tunnel_args(t, &local)?;
        let mut child = tokio::process::Command::new("ssh")
            .args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => {
                    "ssh is not installed (or not on PATH): install OpenSSH's client to reach a remote host".to_string()
                }
                _ => format!("cannot run ssh: {e}"),
            })?;
        let stderr = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(err) = child.stderr.take() {
            let keep = stderr.clone();
            tokio::spawn(async move {
                let mut lines = tokio::io::BufReader::new(err).lines();
                while let Ok(Some(l)) = lines.next_line().await {
                    if let Ok(mut q) = keep.lock() {
                        if q.len() == 20 {
                            q.pop_front();
                        }
                        q.push_back(l);
                    }
                }
            });
        }
        let mut tunnel = Tunnel { child, local, dest: t.dest.clone(), stderr };
        let t0 = tokio::time::Instant::now();
        loop {
            if tunnel.local.exists() {
                return Ok(tunnel);
            }
            if let Ok(Some(st)) = tunnel.child.try_wait() {
                // Let the reader take ssh's last words.
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Err(explain(&tunnel.dest, &tunnel.stderr_text(), st.code()));
            }
            if t0.elapsed() > OPEN_TIMEOUT {
                return Err(format!(
                    "ssh to {} did not set up the tunnel within {} s ({}); check that `ssh {}` works in a terminal",
                    tunnel.dest,
                    OPEN_TIMEOUT.as_secs(),
                    tunnel.stderr_text().trim().lines().last().unwrap_or("no output"),
                    tunnel.dest
                ));
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// What ssh wrote to stderr lately.
    pub fn stderr_text(&self) -> String {
        match self.stderr.lock() {
            Ok(q) => q.iter().cloned().collect::<Vec<_>>().join("\n"),
            Err(_) => String::new(),
        }
    }

    /// Wait for ssh to exit; why it did, with the fix.
    pub async fn exited(&mut self) -> String {
        let code = self.child.wait().await.ok().and_then(|s| s.code());
        tokio::time::sleep(Duration::from_millis(100)).await;
        explain(&self.dest, &self.stderr_text(), code)
    }

    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
        let _ = std::fs::remove_file(&self.local);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_a_posix_shell() {
        assert_eq!(shell_quote("warden"), "warden");
        assert_eq!(shell_quote("/srv/api/server.js"), "/srv/api/server.js");
        assert_eq!(shell_quote("KEY=v,a:b@c%d+e"), "KEY=v,a:b@c%d+e");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("python3 -m http.server"), "'python3 -m http.server'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(shell_quote("a\nb"), "'a\nb'");
        assert_eq!(shell_quote("~/x"), "'~/x'");
        assert_eq!(
            shell_join(&["warden", "start", "python3 -m http.server", "--name", "files"]),
            "warden start 'python3 -m http.server' --name files"
        );
    }

    #[test]
    fn quoted_commands_round_trip_through_sh() {
        // The remote shell must see exactly our arguments.
        let args = ["printf", "%s\\n", "plain", "two words", "it's", "$HOME", "`id`", "a\"b", "back\\slash", "", "*"];
        let line = shell_join(&args);
        let out = std::process::Command::new("sh").arg("-c").arg(&line).output().expect("sh runs");
        let got = String::from_utf8_lossy(&out.stdout);
        let want: String = args[2..].iter().map(|a| format!("{a}\n")).collect();
        assert_eq!(got, want, "{line}");
    }

    #[test]
    fn targets_and_paths_are_checked() {
        assert!(validate_dest("deploy@web-1.example.com").is_ok());
        assert!(validate_dest("prod").is_ok());
        assert!(validate_dest("").unwrap_err().contains("empty"));
        assert!(validate_dest("-oProxyCommand=evil").unwrap_err().contains("option"));
        assert!(validate_dest("user@host extra").unwrap_err().contains("spaces"));
        assert!(validate_socket_path("/run/warden/wardend.sock", "remote").is_ok());
        assert!(validate_socket_path("run/wardend.sock", "remote").unwrap_err().contains("absolute"));
        assert!(validate_socket_path("/a:b", "remote").unwrap_err().contains("':'"));
        assert!(validate_socket_path(&format!("/{}", "x".repeat(120)), "local").unwrap_err().contains("bytes"));
    }

    #[test]
    fn tunnel_and_exec_command_lines() {
        let t = Target { dest: "deploy@web-1".into(), remote_socket: "/run/warden/wardend.sock".into() };
        let a = tunnel_args(&t, Path::new("/tmp/warden-gui-1000/1-0.sock")).unwrap();
        assert_eq!(a[..2], ["-N", "-T"]);
        for o in ["BatchMode=yes", "ExitOnForwardFailure=yes", "StreamLocalBindUnlink=yes"] {
            assert!(a.windows(2).any(|w| w[0] == "-o" && w[1] == o), "{o} in {a:?}");
        }
        assert!(a.windows(2).any(|w| w[0] == "-L" && w[1] == "/tmp/warden-gui-1000/1-0.sock:/run/warden/wardend.sock"));
        assert_eq!(a[a.len() - 2..], ["--", "deploy@web-1"], "the target comes after --");
        let bad = Target { dest: "-x".into(), ..t.clone() };
        assert!(tunnel_args(&bad, Path::new("/tmp/s.sock")).is_err());

        let e = exec_args("web-1", &["warden".into(), "start".into(), "node app.js".into()]).unwrap();
        assert_eq!(e.last().unwrap(), "warden start 'node app.js'");
        let home = exec_args("web-1", &["~/.local/bin/warden".into(), "check".into()]).unwrap();
        assert_eq!(home.last().unwrap(), r#""$HOME"/.local/bin/warden check"#);
        assert_eq!(shell_program("/opt/my warden"), "'/opt/my warden'");
        assert_eq!(e[e.len() - 3..e.len() - 1], ["--", "web-1"]);
        assert!(exec_args("web-1", &[]).is_err());
    }

    #[test]
    fn ssh_errors_come_with_a_fix() {
        let e = explain("u@h", "u@h: Permission denied (publickey).", Some(255));
        assert!(e.contains("Permission denied") && e.contains("ssh-add") && e.contains("exit 255"), "{e}");
        assert!(explain("u@h", "Host key verification failed.", Some(255)).contains("accept the host key"));
        assert!(
            explain("u@h", "ssh: Could not resolve hostname h: Name or service not known", None).contains("host name")
        );
        assert!(
            explain("u@h", "channel 2: open failed: connect failed: No such file or directory", None)
                .contains("remote socket")
        );
        assert!(explain("u@h", "", Some(1)).contains("without saying why"));
    }

    #[test]
    fn the_private_dir_is_private() {
        use std::os::unix::fs::MetadataExt;
        let d = private_dir().unwrap();
        assert_eq!(std::fs::metadata(&d).unwrap().mode() & 0o777, 0o700);
        let a = local_socket().unwrap();
        let b = local_socket().unwrap();
        assert!(a != b && a.starts_with(&d));
    }
}

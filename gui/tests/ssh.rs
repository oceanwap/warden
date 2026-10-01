//! The GUI's SSH code end to end (not the window): `client::feed` through a
//! real `ssh -L` tunnel (gui/src/ssh.rs), requests and a second stream
//! through it, remote commands through `ssh <host> <command>`
//! (gui/src/commands.rs), a tunnel that drops, and the errors people hit:
//! an unreachable or unknown host, an unknown or changed host key, a key
//! the server refuses, no wardend and no warden on the remote host.
//!
//! The "remote host" is this machine: an OpenSSH server this test runs on
//! 127.0.0.1 (a free port, throwaway host and client keys, its own config;
//! nothing of the system's is used or changed) and a real wardend. The
//! GUI's `ssh` is a wrapper first on PATH that adds `-F <this test's
//! ssh_config>`, so each scenario is a host alias. Skipped, with the reason
//! printed, without `sshd`, `ssh` and `ssh-keygen` (CI installs
//! openssh-server).

use iced::futures::{Stream, StreamExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use warden_gui::client::{self, Endpoint, FeedMsg, FeedOptions};
use warden_gui::commands::{self, AddApp, Host as Runner};
use warden_gui::model::Model;
use warden_gui::protocol::control::Request;
use warden_gui::protocol::events::{AppState, DaemonRequest, Event, WorkerEvent};
use warden_gui::ssh;

const T: Duration = Duration::from_secs(30);

fn warden_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("WARDEN_BIN") {
        return PathBuf::from(p);
    }
    // target/debug/deps/ssh-<hash> → target/debug/warden
    let exe = std::env::current_exe().expect("the test's own path");
    let bin = exe.parent().and_then(Path::parent).map(|d| d.join("warden")).expect("target/debug");
    assert!(
        bin.exists(),
        "{} is missing: build it first (`cargo build --bin warden`, or run `cargo test --workspace`)",
        bin.display()
    );
    bin
}

/// `name` on PATH, or in the sbin directories (`sshd` often is not on a
/// user's PATH).
fn find(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/sbin", "/usr/local/sbin", "/sbin"].map(PathBuf::from))
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().unwrap_or_else(|e| panic!("{cmd:?}: {e}"));
    let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{cmd:?} failed:\n{text}");
    text
}

/// Everything this test runs: sshd, the remote wardend and app, in one
/// short directory (socket paths are limited to ~100 bytes). Dropping it
/// stops all of it.
struct Remote {
    dir: PathBuf,
    warden: PathBuf,
    sshd: Child,
    wardend: Option<Child>,
}

impl Remote {
    /// None (with the reason printed) when this machine can't run sshd.
    fn start() -> Option<Remote> {
        let (Some(sshd), Some(ssh), Some(keygen)) = (find("sshd"), find("ssh"), find("ssh-keygen")) else {
            eprintln!(
                "skipping: the SSH end-to-end test needs OpenSSH's sshd, ssh and ssh-keygen \
                 (`apt-get install openssh-server`)"
            );
            return None;
        };
        let root = rustix::process::geteuid().is_root();
        if root && !Path::new("/run/sshd").is_dir() {
            eprintln!(
                "skipping: sshd run as root needs its privilege separation directory /run/sshd \
                 (`mkdir -m 0755 /run/sshd`, which the ssh service creates), or run the test as another user"
            );
            return None;
        }
        let dir = PathBuf::from(format!("/tmp/wgssh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let d = dir.display().to_string();
        for (key, comment) in [("host_key", "host"), ("other_host_key", "other host"), ("id", "gui"), ("other_id", "x")]
        {
            run(Command::new(&keygen).args(["-q", "-t", "ed25519", "-N", "", "-C", comment, "-f"]).arg(dir.join(key)));
        }
        std::fs::copy(dir.join("id.pub"), dir.join("authorized_keys")).unwrap();
        let free = || std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        // `port` serves everything; `strict` refuses forwarding (`wt-no-forwarding`).
        let (port, strict) = (free(), free());
        let user = run(Command::new("id").arg("-un")).trim().to_string();
        let warden = warden_bin();
        // The remote warden's environment: sshd gives every session this
        // WARDEN_HOME and runtime directory (and nothing else of ours).
        let sshd_config = format!(
            "Port {port}\nPort {strict}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nPidFile {d}/sshd.pid\n\
             AuthorizedKeysFile {d}/authorized_keys\nPubkeyAuthentication yes\nPasswordAuthentication no\n\
             KbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\nPermitRootLogin prohibit-password\n\
             AllowUsers {user}\nX11Forwarding no\nPermitTTY no\nLogLevel VERBOSE\n\
             SetEnv WARDEN_HOME={d}/home WARDEN_RUNTIME_DIR={d}/run WARDEN_NO_DAEMON=1\n\
             Match LocalPort {strict}\n    DisableForwarding yes\n"
        );
        std::fs::write(dir.join("sshd_config"), sshd_config).unwrap();
        let pubkey = |f: &str| {
            let text = std::fs::read_to_string(dir.join(f)).unwrap();
            text.split_whitespace().take(2).collect::<Vec<_>>().join(" ")
        };
        let known = format!("[127.0.0.1]:{port} {k}\n[127.0.0.1]:{strict} {k}\n", k = pubkey("host_key.pub"));
        std::fs::write(dir.join("known_hosts"), known).unwrap();
        std::fs::write(
            dir.join("changed_known_hosts"),
            format!("[127.0.0.1]:{port} {}\n", pubkey("other_host_key.pub")),
        )
        .unwrap();
        std::fs::write(dir.join("empty_known_hosts"), "").unwrap();
        // One alias per scenario; the first value ssh finds wins, so `Host *` is last.
        let ssh_config = format!(
            "Host wt-unknown-key\n    UserKnownHostsFile {d}/empty_known_hosts\n\
             Host wt-changed-key\n    UserKnownHostsFile {d}/changed_known_hosts\n\
             Host wt-bad-key\n    IdentityFile {d}/other_id\n\
             Host wt-closed\n    Port 1\n\
             Host wt-no-forwarding\n    Port {strict}\n\
             Host *.invalid\n    HostName %h\n\
             Host * !wt-bad-key\n    IdentityFile {d}/id\n\
             Host *\n    HostName 127.0.0.1\n    Port {port}\n    User {user}\n    IdentitiesOnly yes\n    \
             IdentityAgent none\n    UserKnownHostsFile {d}/known_hosts\n    GlobalKnownHostsFile /dev/null\n    \
             StrictHostKeyChecking yes\n"
        );
        std::fs::write(dir.join("ssh_config"), ssh_config).unwrap();
        let wrapper = dir.join("bin/ssh");
        std::fs::write(&wrapper, format!("#!/bin/sh\nexec '{}' -F '{d}/ssh_config' \"$@\"\n", ssh.display())).unwrap();
        std::fs::set_permissions(&wrapper, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(dir.join("bin"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
        )
        .unwrap();
        // SAFETY: this file has one test, and it sets PATH before it starts
        // any thread or runtime: nothing reads the environment meanwhile.
        unsafe { std::env::set_var("PATH", path) };

        let log = std::fs::File::create(dir.join("sshd.log")).unwrap();
        let child = Command::new(&sshd)
            .args(["-D", "-e", "-f"])
            .arg(dir.join("sshd_config"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(log)
            .spawn()
            .expect("sshd starts");
        let r = Remote { dir, warden, sshd: child, wardend: None };
        let t0 = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(t0.elapsed() < T, "sshd does not listen:\n{}", r.sshd_log());
            std::thread::sleep(Duration::from_millis(50));
        }
        Some(r)
    }

    fn sshd_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("sshd.log")).unwrap_or_default()
    }

    fn wardend_log(&self) -> String {
        std::fs::read_to_string(self.dir.join("wardend.out")).unwrap_or_default()
    }

    /// wardend's socket on the remote host.
    fn socket(&self) -> String {
        self.dir.join("run/wardend.sock").display().to_string()
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.warden);
        c.env("WARDEN_HOME", self.dir.join("home"))
            .env("WARDEN_RUNTIME_DIR", self.dir.join("run"))
            .env_remove("WARDEN_CONFIG")
            .env("WARDEN_NO_DAEMON", "1")
            .current_dir(&self.dir);
        c
    }

    fn start_wardend(&mut self) {
        let log = std::fs::File::create(self.dir.join("wardend.out")).unwrap();
        let child = self.cmd().arg("daemon").stdout(Stdio::from(log.try_clone().unwrap())).stderr(log).spawn().unwrap();
        self.wardend = Some(child);
        let t0 = Instant::now();
        while !Path::new(&self.socket()).exists() {
            assert!(t0.elapsed() < T, "wardend did not create its socket:\n{}", self.wardend_log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Stop whichever wardend runs there (ours, or one started over SSH).
    fn stop_wardend(&mut self) {
        let _ = self.cmd().args(["daemon", "stop"]).output();
        if let Some(mut c) = self.wardend.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop_wardend();
        let _ = self.cmd().args(["kill", "--yes"]).output();
        let _ = self.sshd.kill();
        let _ = self.sshd.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

type Feed = Pin<Box<dyn Stream<Item = FeedMsg> + Send>>;

fn feed_to(dest: &str, remote_socket: &str) -> Feed {
    let target = ssh::Target { dest: dest.into(), remote_socket: remote_socket.into() };
    Box::pin(client::feed(Endpoint::Ssh(target), FeedOptions::all()))
}

async fn next(feed: &mut Feed) -> FeedMsg {
    tokio::time::timeout(T, feed.next()).await.expect("a feed message within 30 s").expect("the feed never ends")
}

/// The first `Connected` (its socket: the tunnel's local end), or panic
/// with the first error.
async fn connected(feed: &mut Feed, what: &str) -> PathBuf {
    let mut said = Vec::new();
    loop {
        match next(feed).await {
            FeedMsg::Connected { socket } => return socket,
            FeedMsg::Connecting { what, .. } => said.push(what),
            FeedMsg::Disconnected { error, .. } => panic!("{what}: {error}\n(after {said:?})"),
            FeedMsg::Batch(_) => {}
        }
    }
}

/// The first `Disconnected`: its error, `not_running` and `retry_in`.
async fn first_error(feed: &mut Feed) -> (String, bool, Duration) {
    loop {
        match next(feed).await {
            FeedMsg::Disconnected { error, not_running, retry_in, .. } => return (error, not_running, retry_in),
            FeedMsg::Connected { socket } => panic!("connected to {} where it should fail", socket.display()),
            _ => {}
        }
    }
}

/// Apply batches until `done` holds; the events seen meanwhile.
async fn until(feed: &mut Feed, model: &mut Model, what: &str, done: impl Fn(&Model, &[Event]) -> bool) -> Vec<Event> {
    let t0 = Instant::now();
    let mut seen = Vec::new();
    loop {
        assert!(t0.elapsed() < T, "timed out waiting for {what}; seen: {seen:#?}");
        match next(feed).await {
            FeedMsg::Batch(b) => {
                for ev in b.events {
                    seen.push(ev.clone());
                    model.apply(ev, 0);
                }
                for (app, line) in b.logs {
                    seen.push(Event::Log { app, line });
                }
            }
            FeedMsg::Disconnected { error, .. } => panic!("disconnected while waiting for {what}: {error}"),
            _ => {}
        }
        if done(model, &seen) {
            return seen;
        }
    }
}

/// Processes whose command line has an argument containing `needle`.
fn pids_with_arg(needle: &str) -> Vec<i32> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let Ok(cmd) = std::fs::read(e.path().join("cmdline")) else { continue };
        if cmd.split(|b| *b == 0).any(|a| String::from_utf8_lossy(a).contains(needle)) {
            out.push(pid);
        }
    }
    out
}

/// The tunnel's ssh process (`-L <local>:<remote>`), by its local socket.
fn tunnel_pids(local: &Path) -> Vec<i32> {
    pids_with_arg(&format!("{}:", local.display()))
}

fn wait_gone(local: &Path) {
    let t0 = Instant::now();
    while !tunnel_pids(local).is_empty() || local.exists() {
        assert!(t0.elapsed() < Duration::from_secs(10), "the tunnel to {} is still there", local.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_gui_drives_a_remote_wardend_over_ssh() {
    let Some(mut remote) = Remote::start() else { return };
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
    rt.block_on(scenario(&mut remote));
}

async fn scenario(remote: &mut Remote) {
    // The remote host: an app, and wardend.
    let out = run(remote.cmd().args(["start", "sleep 300", "--name", "sleeper", "-i", "2"]));
    assert!(out.contains("sleeper: online (2/2"), "{out}");
    remote.start_wardend();
    let rsock = remote.socket();
    let warden = remote.warden.display().to_string();
    let host = Runner::Ssh { dest: "wt-ok".into(), warden: warden.clone() };

    // Connect: the tunnel opens, then wardend answers through it.
    let mut feed = feed_to("wt-ok", &rsock);
    let mut steps = Vec::new();
    let local = loop {
        match next(&mut feed).await {
            FeedMsg::Connecting { what, .. } => steps.push(what),
            FeedMsg::Connected { socket } => break socket,
            other => panic!("expected to connect, got {other:?}\nsshd:\n{}", remote.sshd_log()),
        }
    };
    assert_eq!(steps[0], "opening an SSH tunnel to wt-ok", "{steps:?}");
    assert!(steps[1].contains("wt-ok") && steps[1].contains(&rsock), "{steps:?}");
    assert!(local.starts_with(ssh::private_dir().unwrap()), "{}", local.display());
    assert_eq!(tunnel_pids(&local).len(), 1, "one ssh for the tunnel");

    // The apps and their live state.
    let mut model = Model::default();
    until(&mut feed, &mut model, "sleeper running with 2 workers ready", |m, _| {
        m.apps.get("sleeper").is_some_and(|a| a.entry.state == AppState::Running && a.workers() == Some((2, 2)))
    })
    .await;
    assert_eq!(model.daemon.as_ref().map(|d| d.protocol), Some(1));
    let apps = client::daemon_request(&local, &DaemonRequest::Apps, client::APP_TIMEOUT).await.unwrap();
    assert!(apps.apps.unwrap().iter().any(|a| a.name == "sleeper"));

    // A command (a rolling restart) through the tunnel, followed live.
    let resp = client::app_request(&local, "sleeper", Request::Restart { worker: None, hard: false }).await.unwrap();
    assert!(resp.ok, "{resp:?}");
    let seen = until(&mut feed, &mut model, "the restart to finish", |_, seen| {
        seen.iter().any(|e| matches!(e, Event::RolloutDone { app, outcome } if app == "sleeper" && outcome.ok))
    })
    .await;
    assert!(
        seen.iter().any(|e| matches!(e, Event::Worker { app, event: WorkerEvent::Ready, .. } if app == "sleeper")),
        "{seen:#?}"
    );
    let h = client::history(&local, "sleeper", warden_gui::protocol::events::now_ms() - 600_000, 10).await.unwrap();
    assert_eq!(h.apps.len(), 1, "{h:?}");

    // A second stream through the same tunnel: the logs pane.
    let mut logs: Feed = Box::pin(client::feed(Endpoint::Socket(local.clone()), FeedOptions::logs_of("sleeper")));
    let mut lmodel = Model::default();
    until(&mut logs, &mut lmodel, "the logs stream", |m, _| m.daemon.is_some()).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resp = client::app_request(&local, "sleeper", Request::Restart { worker: Some(1), hard: false }).await.unwrap();
    assert!(resp.ok, "{resp:?}");
    until(&mut logs, &mut lmodel, "log lines", |_, seen| {
        seen.iter().any(|e| matches!(e, Event::Log { app, line } if app == "sleeper" && line.contains("worker=1")))
    })
    .await;
    assert!(!client::app_logs(&local, "sleeper", 20).await.unwrap().is_empty());
    drop(logs);

    // Remote commands: `ssh wt-ok '<quoted command line>'`.
    let out = commands::run_warden(&host, &["list".into()], None, &[], T).await.unwrap();
    assert!(out.ok && out.stdout.contains("sleeper"), "{}", out.text());
    let form = AddApp {
        what: "sleep 301".into(),
        name: "napper".into(),
        instances: "1".into(),
        port: String::new(),
        env_file: String::new(),
    };
    let added = commands::add_app(&host, &form).await.unwrap();
    assert!(added.output.ok, "{}", added.output.text());
    until(&mut feed, &mut model, "napper", |m, _| m.apps.get("napper").is_some_and(|a| a.workers() == Some((1, 1))))
        .await;
    let cfg = model.apps["napper"].entry.config.clone().expect("a config");
    let text = commands::read_config(&host, &cfg).await.unwrap();
    assert!(text.contains("napper"), "{text}");
    let err = commands::check_config(&host, &cfg, &format!("{text}\n[bogus]\n")).await.unwrap_err();
    assert!(err.starts_with("the config is not valid") && err.contains("bogus"), "{err}");
    let edited = format!("# edited over ssh: it's \"quoted\" $HOME `x`\n{text}");
    commands::save_config(&host, &cfg, &edited).await.unwrap();
    assert_eq!(commands::read_config(&host, &cfg).await.unwrap(), edited);

    // The tunnel drops (its ssh is killed): the feed says so, then opens a
    // new tunnel and connects again.
    let pids = tunnel_pids(&local);
    assert_eq!(pids.len(), 1);
    let _ = Command::new("kill").arg("-KILL").arg(pids[0].to_string()).status();
    let (error, not_running, _) = first_error(&mut feed).await;
    assert!(error.contains("wt-ok") && !not_running, "{error}");
    assert!(error.contains("SSH"), "says the SSH connection ended: {error}");
    let again = connected(&mut feed, "reconnecting after the tunnel dropped").await;
    assert_ne!(again, local, "a new tunnel");
    until(&mut feed, &mut model, "the apps again", |m, _| m.apps.contains_key("sleeper")).await;

    // Disconnect: dropping the feed closes the connection and the tunnel.
    drop(feed);
    wait_gone(&again);

    // No wardend there: the tunnel opens, nothing answers; "not running",
    // with the fix. Start wardend over SSH, and the next attempt connects.
    remote.stop_wardend();
    let mut feed = feed_to("wt-ok", &rsock);
    let (error, not_running, _) = first_error(&mut feed).await;
    assert!(not_running, "{error}");
    assert!(error.contains("wardend is not running there") && error.contains(&rsock), "{error}");
    assert!(error.contains("warden daemon --background") && error.contains("(ssh: channel "), "{error}");
    let out = commands::start_wardend(&host).await.unwrap();
    assert!(out.ok && out.stdout.contains("wardend started in the background"), "{}", out.text());
    // The feed retries by itself (an attempt under way may fail once more).
    let t0 = Instant::now();
    loop {
        match next(&mut feed).await {
            FeedMsg::Connected { .. } => break,
            FeedMsg::Disconnected { error, .. } => {
                assert!(t0.elapsed() < Duration::from_secs(10), "no reconnect after starting wardend: {error}")
            }
            _ => {}
        }
    }
    drop(feed);

    // No warden there (not on the PATH ssh gives commands): the reason and the fix.
    let missing = Runner::Ssh { dest: "wt-ok".into(), warden: "warden-not-installed".into() };
    let err = commands::start_wardend(&missing).await.unwrap_err();
    assert!(err.contains("not found on wt-ok") && err.contains("remote warden"), "{err}");

    // The errors people hit, each with its fix.
    // (alias, words of the error and its fix, whether retrying waits for the user)
    let cases: [(&str, &[&str], bool); 5] = [
        ("wt-closed", &["Connection refused", "sshd listens"], false),
        ("wt-unknown-key", &["Host key verification failed", "accept the host key"], true),
        ("wt-changed-key", &["host key has changed", "changed_known_hosts' -R '[127.0.0.1]:"], true),
        ("wt-bad-key", &["Permission denied", "ssh-add"], true),
        ("wt-nohost.invalid", &["Could not resolve hostname", "host name"], false),
    ];
    for (dest, want, needs_you) in cases {
        let t0 = Instant::now();
        let mut feed = feed_to(dest, &rsock);
        let (error, not_running, retry_in) = first_error(&mut feed).await;
        assert!(!not_running, "{dest}: {error}");
        if needs_you {
            // Not every few seconds: the server's auth log, fail2ban.
            assert_eq!(retry_in, client::Backoff::NEEDS_YOU, "{dest}");
            assert!(error.contains("Trying again in 10 min"), "{dest}: {error}");
        } else {
            assert!(retry_in <= Duration::from_secs(5), "{dest}: {retry_in:?}");
        }
        for w in want {
            assert!(error.contains(w), "{dest}: {w:?} not in {error:?}");
        }
        assert!(t0.elapsed() < Duration::from_secs(15), "{dest} took {:?}", t0.elapsed());
        // A remote command to that host says the same (where ssh fails).
        let host = Runner::Ssh { dest: dest.into(), warden: warden.clone() };
        let r = commands::run_warden(&host, &["list".into()], None, &[], T).await;
        let err = r.unwrap_err();
        assert!(err.contains(want[0]) && err.contains(want[1]), "{dest}: {err}");
    }
    // An sshd that does not forward sockets: it answers like a missing
    // wardend, so the error names its settings too. Commands still work.
    let mut feed = feed_to("wt-no-forwarding", &rsock);
    let (error, _, _) = first_error(&mut feed).await;
    assert!(error.contains("AllowStreamLocalForwarding") && error.contains("DisableForwarding"), "{error}");
    drop(feed);
    let host = Runner::Ssh { dest: "wt-no-forwarding".into(), warden: warden.clone() };
    assert!(commands::run_warden(&host, &["list".into()], None, &[], T).await.unwrap().ok);
    // Nothing is left behind: no ssh of ours.
    let t0 = Instant::now();
    loop {
        let leftover = pids_with_arg(&format!("{}/ssh_config", remote.dir.display()));
        if leftover.is_empty() {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(10), "ssh processes left: {leftover:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

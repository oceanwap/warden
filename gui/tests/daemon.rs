//! The GUI's client layer (not the window) against a real `warden wardend`:
//! apps and events arrive, a reload goes through and is followed to its
//! end, log lines stream, and the feed reconnects when wardend restarts.
//! Also Add app (with an env file) and Edit config's check and save, with
//! the real CLI. Uses the `warden` binary of this workspace
//! (target/debug/warden, built by `cargo test --workspace`; `$WARDEN_BIN`
//! overrides it).

use iced::futures::{Stream, StreamExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use warden_gui::client::{self, Endpoint, FeedMsg, FeedOptions};
use warden_gui::commands::{self, AddApp, Host as Runner};
use warden_gui::model::Model;
use warden_gui::protocol::control::Request;
use warden_gui::protocol::events::{AppState, Event, WorkerEvent};

const T: Duration = Duration::from_secs(30);

fn warden_bin() -> PathBuf {
    if let Some(p) = std::env::var_os("WARDEN_BIN") {
        return PathBuf::from(p);
    }
    // target/debug/deps/daemon-<hash> → target/debug/warden
    let exe = std::env::current_exe().expect("the test's own path");
    let bin = exe.parent().and_then(Path::parent).map(|d| d.join("warden")).expect("target/debug");
    assert!(
        bin.exists(),
        "{} is missing: build it first (`cargo build --bin warden`, or run `cargo test --workspace`)",
        bin.display()
    );
    bin
}

/// A private WARDEN_HOME: apps started with `warden` there, a wardend run as
/// a child. Dropping it stops everything it started.
struct Host {
    home: PathBuf,
    bin: PathBuf,
    wardend: Option<Child>,
}

impl Host {
    fn new(name: &str) -> Host {
        let home = std::env::temp_dir().join(format!("wg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        Host { home, bin: warden_bin(), wardend: None }
    }

    fn socket(&self) -> PathBuf {
        self.home.join("run").join("wardend.sock")
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(&self.bin);
        c.env("WARDEN_HOME", &self.home)
            .env("WARDEN_RUNTIME_DIR", self.home.join("run"))
            .env_remove("WARDEN_CONFIG")
            .env("WARDEN_NO_DAEMON", "1")
            .current_dir(&self.home);
        c
    }

    fn warden(&self, args: &[&str]) -> String {
        let out = self.cmd().args(args).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "warden {} failed:\n{text}", args.join(" "));
        text
    }

    fn start_wardend(&mut self) {
        let log = std::fs::File::create(self.home.join("wardend.out")).unwrap();
        let child =
            self.cmd().arg("wardend").stdout(Stdio::from(log.try_clone().unwrap())).stderr(log).spawn().unwrap();
        self.wardend = Some(child);
        let t0 = Instant::now();
        while !self.socket().exists() {
            assert!(t0.elapsed() < T, "wardend did not create its socket:\n{}", self.log());
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// SIGTERM wardend (it says bye) and reap it.
    fn stop_wardend(&mut self) {
        if let Some(mut c) = self.wardend.take() {
            let _ = Command::new("kill").arg("-TERM").arg(c.id().to_string()).status();
            let t0 = Instant::now();
            while c.try_wait().ok().flatten().is_none() && t0.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.home.join("wardend.out")).unwrap_or_default()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop_wardend();
        let _ = self.cmd().args(["kill", "--yes"]).output();
        // Something still shutting down may write into the folder while it goes: until it is gone.
        for _ in 0..40 {
            if std::fs::remove_dir_all(&self.home).is_ok() && !self.home.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

type Feed = Pin<Box<dyn Stream<Item = FeedMsg> + Send>>;

/// Next feed message within `T`.
async fn next(feed: &mut Feed) -> FeedMsg {
    tokio::time::timeout(T, feed.next()).await.expect("a feed message within 30 s").expect("the feed never ends")
}

/// Apply batches to `model` until `done` holds; the events seen meanwhile.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apps_events_actions_logs_and_reconnect() {
    let mut host = Host::new("feed");
    let out = host.warden(&["start", "sleep 300", "--name", "sleeper", "-i", "2"]);
    assert!(out.contains("sleeper: online (2/2"), "{out}");
    host.start_wardend();
    let socket = host.socket();

    // The feed: hello, the apps, their status, events.
    let mut feed: Feed = Box::pin(client::feed(Endpoint::Socket(socket.clone()), FeedOptions::all()));
    let mut model = Model::default();
    loop {
        match next(&mut feed).await {
            FeedMsg::Connected { socket: s } => {
                assert_eq!(s, socket);
                break;
            }
            FeedMsg::Connecting { .. } => {}
            other => panic!("expected to connect, got {other:?}\n{}", host.log()),
        }
    }
    until(&mut feed, &mut model, "sleeper running with 2 workers ready", |m, _| {
        m.apps.get("sleeper").is_some_and(|a| a.entry.state == AppState::Running && a.workers() == Some((2, 2)))
    })
    .await;
    let d = model.daemon.clone().expect("wardend's hello");
    assert_eq!(d.protocol, 1);
    let app = &model.apps["sleeper"];
    assert_eq!(app.entry.supervised_by, "wardend", "`warden start` runs it in the background: wardend restarts it");
    assert!(app.status.as_ref().is_some_and(|s| s.workers.iter().all(|w| w.pid.is_some())));
    if cfg!(target_os = "linux") {
        until(&mut feed, &mut model, "a host event", |m, _| m.host.as_ref().is_some_and(|h| h.mem_total_bytes > 0))
            .await;
    }

    // A reload through wardend: accepted, then followed to its end in the feed.
    let resp = client::app_request(&socket, "sleeper", Request::Reload { safe: false }).await.expect("wardend answers");
    assert!(resp.ok && resp.seq.is_some(), "{resp:?}");
    let seen = until(&mut feed, &mut model, "the reload to finish", |_, seen| {
        seen.iter().any(|e| matches!(e, Event::RolloutDone { app, outcome } if app == "sleeper" && outcome.ok))
    })
    .await;
    assert!(seen.iter().any(|e| matches!(e, Event::Rollout { app, .. } if app == "sleeper")), "{seen:#?}");
    assert!(
        seen.iter().any(|e| matches!(e, Event::Worker { app, event: WorkerEvent::Ready, .. } if app == "sleeper")),
        "the replacements' ready events: {seen:#?}"
    );
    let a = &model.apps["sleeper"];
    assert!(a.rollout.is_none() && a.last_outcome.as_ref().is_some_and(|o| o.ok));
    assert!(a.feed.iter().any(|l| l.contains("sleeper reload done")), "{:?}", a.feed);

    // The History tab's request: the app's and the host's series on one grid.
    let since = warden_protocol::events::now_ms() - 3_600_000;
    let h = client::history(&socket, "sleeper", since, 10).await.expect("wardend keeps a history");
    assert_eq!((h.step_s, h.apps.len()), (10, 1), "{h:?}");
    assert!((360..=362).contains(&h.points), "{}", h.points);
    assert_eq!(h.apps[0].app, "sleeper");
    assert_eq!(h.apps[0].cpu_percent.len(), h.points as usize);
    assert_eq!(h.host.cpu_percent.len(), h.points as usize);
    let h = client::history(&socket, "", since, 60).await.unwrap();
    assert!(h.apps.is_empty() && h.step_s == 60, "\"\": the host only");

    // Errors come back as errors, with words.
    let err = client::app_request(&socket, "nope", Request::Status).await.unwrap_err();
    assert!(err.contains("nope"), "{err}");
    let resp =
        client::app_request(&socket, "sleeper", Request::Restart { worker: Some(9), hard: false }).await.unwrap();
    assert!(!resp.ok && resp.message.is_some(), "{resp:?}");

    // Logs: recent lines, then live ones through a second, filtered stream.
    let recent = client::app_logs(&socket, "sleeper", 50).await.expect("recent lines");
    assert!(recent.iter().any(|l| l.contains("worker ready")), "{recent:?}");
    let mut logs: Feed = Box::pin(client::feed(Endpoint::Socket(socket.clone()), FeedOptions::logs_of("sleeper")));
    let mut lmodel = Model::default();
    until(&mut logs, &mut lmodel, "the logs stream", |m, _| m.daemon.is_some()).await;
    // wardend turns supervisors' log lines on for this client: give it a moment.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let resp =
        client::app_request(&socket, "sleeper", Request::Restart { worker: Some(1), hard: false }).await.unwrap();
    assert!(resp.ok, "{resp:?}");
    let seen = until(&mut logs, &mut lmodel, "log lines", |_, seen| {
        seen.iter().any(|e| matches!(e, Event::Log { app, line } if app == "sleeper" && line.contains("worker=1")))
    })
    .await;
    assert!(seen.iter().all(|e| !matches!(e, Event::Log { app, .. } if app != "sleeper")));
    drop(logs);

    // wardend restarts: the feed says why, retries, and connects again.
    host.stop_wardend();
    let t0 = Instant::now();
    let error = loop {
        assert!(t0.elapsed() < T, "no disconnect");
        match next(&mut feed).await {
            FeedMsg::Disconnected { error, retry_in, .. } => {
                assert!(retry_in <= Duration::from_secs(1), "the first retry is quick: {retry_in:?}");
                break error;
            }
            FeedMsg::Batch(b) => b.events.into_iter().for_each(|e| model.apply(e, 0)),
            _ => {}
        }
    };
    assert!(error.contains("wardend exited (SIGTERM)"), "{error}");
    assert!(model.bye.as_deref() == Some("SIGTERM"), "{:?}", model.bye);
    // While it is down: "not running", with the socket path.
    match next(&mut feed).await {
        FeedMsg::Connecting { .. } => {}
        other => panic!("{other:?}"),
    }
    match next(&mut feed).await {
        FeedMsg::Disconnected { not_running: true, error, .. } => assert!(error.contains("not running"), "{error}"),
        other => panic!("expected not running, got {other:?}"),
    }
    host.start_wardend();
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < T, "did not reconnect");
        if let FeedMsg::Connected { .. } = next(&mut feed).await {
            break;
        }
    }
    until(&mut feed, &mut model, "the apps again", |m, _| m.apps.contains_key("sleeper") && m.daemon.is_some()).await;
    drop(feed);
    // Nothing left behind: the Drop of `host` stops wardend and the app.
}

#[tokio::test]
async fn no_wardend_means_not_running_with_the_path() {
    let path = std::env::temp_dir().join(format!("wg-none-{}/wardend.sock", std::process::id()));
    let mut feed: Feed = Box::pin(client::feed(Endpoint::Socket(path.clone()), FeedOptions::all()));
    loop {
        match next(&mut feed).await {
            FeedMsg::Connecting { .. } => {}
            FeedMsg::Disconnected { error, not_running, retry_in, attempt } => {
                assert!(not_running && error.contains(&path.display().to_string()), "{error}");
                assert_eq!((retry_in, attempt), (Duration::from_millis(250), 1));
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    let err = client::app_request(&path, "api", Request::Reload { safe: false }).await.unwrap_err();
    assert!(err.contains("not running") && err.contains("Start wardend"), "{err}");
}

/// An app that prints as fast as it can (Warden keeps ~10,000 lines a
/// second of it), and a window that takes a batch only twice a second (at
/// most 4,000 lines a second): batches stay bounded, the loss is counted,
/// and the stream stays up. The reader task keeps reading meanwhile, so
/// wardend never has to drop the GUI for not reading.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_log_flood_stays_bounded_and_connected() {
    let mut host = Host::new("flood");
    host.warden(&["start", "yes flood-line-with-some-text-in-it", "--name", "flood"]);
    host.start_wardend();
    let socket = host.socket();
    let mut logs: Feed = Box::pin(client::feed(Endpoint::Socket(socket), FeedOptions::logs_of("flood")));
    let (mut lines, mut lost, mut batches) = (0usize, 0u64, 0usize);
    let mut seen: Vec<String> = Vec::new();
    // What to look at when no line comes: the events, wardend's output, the app's.
    let diagnose = |what: &str, seen: &[String]| -> String {
        let status = host
            .cmd()
            .args(["status", "flood"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr));
        let app_log = std::fs::read_to_string(host.home.join("state/logs/flood.log")).unwrap_or_default();
        let (flood, other): (Vec<&str>, Vec<&str>) = app_log.lines().partition(|l| l.contains("flood-line"));
        format!(
            "{what}\nevents seen: {seen:#?}\nwardend:\n{}\nwarden status flood:\n{}\nthe app's log ({} flood lines), the rest:\n{}",
            host.log(),
            status.unwrap_or_else(|e| e.to_string()),
            flood.len(),
            other[other.len().saturating_sub(40)..].join("\n")
        )
    };
    // Five seconds of flood, from its first line: on a busy machine the app
    // can take a while to start.
    let t0 = Instant::now();
    let mut flooding: Option<Instant> = None;
    while flooding.map_or(t0.elapsed() < T, |t| t.elapsed() < Duration::from_secs(5)) {
        // Statuses come once a minute on this feed: a quiet spell means no lines.
        let msg = match tokio::time::timeout(Duration::from_secs(10), logs.next()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => panic!("the feed ended"),
            Err(_) => panic!("{}", diagnose(&format!("no feed message for 10 s ({lines} lines so far)"), &seen)),
        };
        match msg {
            FeedMsg::Batch(b) => {
                assert!(b.logs.len() <= client::BATCH_LOG_CAP, "a batch holds {} lines", b.logs.len());
                seen.extend(b.events.iter().take(20).map(|e| format!("{e:?}").chars().take(300).collect::<String>()));
                lines += b.logs.iter().filter(|(a, l)| a == "flood" && l.contains("flood-line")).count();
                if lines == 0 {
                    continue;
                }
                flooding.get_or_insert_with(Instant::now);
                lost += b.logs_dropped;
                lost += b
                    .events
                    .iter()
                    .map(|e| if let Event::Lagged { dropped, .. } = e { *dropped } else { 0 })
                    .sum::<u64>();
                batches += 1;
                // A slow window: two frames a second.
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            FeedMsg::Disconnected { error, .. } => panic!("the flood dropped the stream: {error}\n{}", host.log()),
            _ => {}
        }
    }
    // `yes` writes far more than the supervisor forwards (its subscribers
    // lag): what is lost upstream arrives as `lagged`, counted like the
    // lines a batch drops.
    assert!(
        batches >= 5 && lines > 100,
        "{}",
        diagnose(&format!("{batches} batches, {lines} lines, {lost} lost"), &seen)
    );
    assert!(lost > 0, "a flood loses lines (counted); nothing waits for this reader");
}

#[tokio::test]
async fn add_app_with_an_env_file_then_check_and_save_its_config() {
    let host = Host::new("add");
    commands::set_local_warden(host.bin.clone());
    let script = host.home.join("greet.sh");
    std::fs::write(&script, "#!/bin/sh\necho \"greeting=$GREETING\"\nexec sleep 300\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // The env file also points `warden start` at this test's WARDEN_HOME:
    // its variables are the command's environment.
    let env = host.home.join("greeter.env");
    std::fs::write(
        &env,
        format!(
            "# test\nGREETING=\"hello world\"\nWARDEN_HOME={}\nWARDEN_RUNTIME_DIR={}\nWARDEN_NO_DAEMON=1\n",
            host.home.display(),
            host.home.join("run").display()
        ),
    )
    .unwrap();
    let form = AddApp {
        what: script.display().to_string(),
        name: "greeter".into(),
        instances: "1".into(),
        port: String::new(),
        env_file: env.display().to_string(),
    };
    let added = commands::add_app(&Runner::Local, &form).await.expect("warden start runs");
    assert!(added.output.ok, "{}\n{:?}", added.output.text(), added.notes);
    assert!(added.output.command.contains("--name greeter"), "{}", added.output.command);
    let cfg = host.home.join("greeter.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains(&format!("env_file = \"{}\"", env.display())), "{text}\n{:?}", added.notes);
    assert!(!text.contains("hello world"), "values never go into the config");
    host.warden(&["check", "-c", cfg.to_str().unwrap()]);
    let t0 = Instant::now();
    loop {
        let logs = host.warden(&["logs", "greeter", "--nostream"]);
        if logs.contains("greeting=hello world") {
            break;
        }
        assert!(t0.elapsed() < T, "the worker never printed the variable:\n{logs}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // Edit config: a bad config is refused with warden's words, about the
    // real path; a good one is saved in place, mode kept.
    let path = cfg.display().to_string();
    let bad = format!("{text}\n[workerz]\ncount = 2\n");
    let err = commands::check_config(&Runner::Local, &path, &bad).await.unwrap_err();
    assert!(err.starts_with("the config is not valid") && err.contains(&path) && !err.contains("warden-gui-"), "{err}");
    let err = commands::save_config(&Runner::Local, &path, &bad).await.unwrap_err();
    assert!(err.contains("not valid"), "{err}");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), text, "an invalid config is never written");
    let good = format!("# edited in the GUI\n{text}");
    let ok = commands::check_config(&Runner::Local, &path, &good).await.expect("valid");
    assert!(ok.contains(&path) && ok.contains("ok"), "{ok}");
    commands::save_config(&Runner::Local, &path, &good).await.expect("saved");
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), good);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&cfg).unwrap().permissions().mode() & 0o777, 0o644);
    let leftovers: Vec<_> = std::fs::read_dir(&host.home)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("warden-gui"))
        .collect();
    assert!(leftovers.is_empty(), "temporary files left: {leftovers:?}");
    assert_eq!(commands::read_config(&Runner::Local, &path).await.unwrap(), good);

    // Adding it again: warden's own refusal comes back as output, not a crash.
    let again = commands::add_app(&Runner::Local, &form).await.expect("runs");
    assert!(!again.output.ok && again.output.text().contains("already exists"), "{}", again.output.text());
}

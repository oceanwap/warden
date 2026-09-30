//! Headless rendering (iced_test, tiny-skia): the main screen with fake
//! wardend data, the "wardend is not running" screen, the logs tab and a
//! confirmation. Each is searched for the texts it must show, clicked
//! where it matters, and saved as a PNG for people to look at:
//! `$WARDEN_GUI_SNAPSHOT_DIR` (default: target/tmp/snapshots).

use iced::Theme;
use iced_test::Simulator;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use warden_gui::app::{Act, Gui, Message, Tab, Target};
use warden_gui::client::{Batch, FeedMsg};
use warden_gui::protocol::events::Event;

const SIZE: (f32, f32) = (1280.0, 820.0);

fn worker(
    id: u64,
    state: &str,
    pid: Option<u64>,
    rss_mb: u64,
    cpu: f64,
    restarts: u64,
    last_exit: Option<&str>,
) -> serde_json::Value {
    json!({
        "id": id, "state": state, "pid": pid, "uptime_secs": 3600 + id * 61, "restarts": restarts, "crashes": restarts,
        "rss_bytes": rss_mb << 20, "cpu_seconds": 12.5, "cpu_percent": cpu, "last_exit": last_exit,
        "healthy": if state == "RUNNING" { Some(true) } else { None::<bool> },
    })
}

fn status(app: &str, workers: Vec<serde_json::Value>, ready: u64, rollout: serde_json::Value) -> serde_json::Value {
    json!({
        "app": app, "namespace": "default", "mode": "process", "config_path": format!("/etc/warden/{app}.toml"),
        "launched": "background", "version": "0.1.0", "pid": 4208, "uptime_secs": 86_400 + 3_723,
        "workers_configured": workers.len(), "workers_ready": ready, "healthy": true,
        "supervisor_rss_bytes": 4u64 << 20, "host": null, "reloading": !rollout.is_null(), "shutting_down": false,
        "rollout": rollout,
        "last_rollout": {"seq": 2, "kind": "reload", "ok": true, "message": "4 workers replaced", "duration_secs": 6.2},
        "workers": workers,
    })
}

/// What wardend sends a new subscriber, as NDJSON, then some events.
fn fake_stream() -> Vec<Event> {
    let rollout =
        json!({"seq": 3, "kind": "reload", "phase": "replacing worker 3", "done": 2, "total": 4, "elapsed_secs": 4});
    let api = status(
        "api",
        vec![
            worker(1, "RUNNING", Some(4230), 41, 1.2, 0, None),
            worker(2, "RUNNING", Some(4262), 40, 0.8, 1, Some("exit code 3")),
            worker(3, "STARTING", Some(4270), 22, 9.5, 0, None),
            worker(4, "RUNNING", Some(4241), 42, 1.1, 0, None),
        ],
        3,
        rollout.clone(),
    );
    let web = status(
        "web",
        vec![
            worker(1, "RUNNING", Some(5101), 64, 3.0, 0, None),
            worker(2, "FAILED", None, 0, 0.0, 10, Some("signal 9 (SIGKILL)")),
        ],
        1,
        json!(null),
    );
    let lines = vec![
        json!({"type": "hello", "protocol": 1, "pid": 4211, "version": "0.1.0"}),
        json!({"type": "apps", "apps": [
            {"name": "api", "namespace": "default", "config": "/etc/warden/api.toml", "socket": "/run/warden/api/control.sock",
             "state": "running", "supervised_by": "wardend", "supervisor_pid": 4208, "supervisor_restarts": 0, "status": api},
            {"name": "jobs", "namespace": "default", "config": "/etc/warden/jobs.toml", "socket": "/run/warden/jobs/control.sock",
             "state": "gave_up", "supervised_by": "wardend", "supervisor_restarts": 10,
             "problem": "died 10 times in 10 minutes; wardend stopped restarting it. Last log line: `Error: ECONNREFUSED 127.0.0.1:5432`. Fix the cause, then `warden start jobs`"},
            {"name": "static", "namespace": "default", "config": "/etc/warden/static.toml", "socket": "/run/warden/static/control.sock",
             "state": "not_started", "supervised_by": "wardend", "problem": "not running; `warden start static` starts it"},
            {"name": "web", "namespace": "default", "config": "/etc/warden/web.toml", "socket": "/run/warden/web/control.sock",
             "state": "running", "supervised_by": "systemd", "supervisor_pid": 5100, "status": web},
        ]}),
        json!({"type": "status", "app": "api", "status": api}),
        json!({"type": "host", "cpu_percent": 23.4, "mem_used_bytes": 3_328_000_000u64, "mem_total_bytes": 8_220_000_000u64, "load": [0.52, 0.40, 0.31], "at_ms": 1}),
        json!({"type": "worker", "app": "api", "worker": 2, "event": "crashed", "pid": 4230, "detail": "exit code 3", "at_ms": 1_790_000_000_000u64}),
        json!({"type": "worker", "app": "api", "worker": 2, "event": "restarting", "detail": "in_ms=0", "at_ms": 1_790_000_000_010u64}),
        json!({"type": "worker", "app": "api", "worker": 2, "event": "starting", "pid": 4262, "at_ms": 1_790_000_000_020u64}),
        json!({"type": "worker", "app": "api", "worker": 2, "event": "ready", "pid": 4262, "detail": "startup_ms=31", "at_ms": 1_790_000_000_051u64}),
        json!({"type": "rollout", "app": "api", "rollout": rollout}),
        json!({"type": "supervisor", "app": "jobs", "event": "gave_up", "pid": 6001, "detail": "died 10 times in 10 minutes", "at_ms": 1_790_000_000_100u64}),
    ];
    lines.into_iter().map(|v| serde_json::from_value(v).expect("the fake stream follows docs/protocol.md")).collect()
}

fn connected() -> Gui {
    let mut g = Gui::with_target(Target::local(Some("/run/warden/wardend.sock".into())));
    let _ = g.update(Message::Feed(FeedMsg::Connected { socket: "/run/warden/wardend.sock".into() }));
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: fake_stream(), ..Batch::default() })));
    g
}

fn snapshot_dir() -> PathBuf {
    std::env::var_os("WARDEN_GUI_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("snapshots"))
}

/// Render, save as `<name>-tiny-skia.png`, and check it is not blank.
fn save(ui: &mut Simulator<'_, Message>, name: &str) {
    let dir = snapshot_dir();
    let base = dir.join(name);
    let png = dir.join(format!("{name}-tiny-skia.png"));
    let _ = std::fs::remove_file(&png);
    let snap = ui.snapshot(&Theme::Dark).expect("renders");
    assert!(snap.matches_image(&base).expect("writes the PNG"));
    let bytes = std::fs::metadata(&png).map(|m| m.len()).unwrap_or(0);
    // A blank 2560x1640 PNG compresses to a few KB; a screen with text is far larger.
    assert!(bytes > 40_000, "{} is only {bytes} bytes: nothing drawn?", png.display());
    eprintln!("snapshot: {}", png.display());
}

fn sim(g: &Gui) -> Simulator<'_, Message> {
    Simulator::with_size(warden_gui::settings(), SIZE, warden_gui::view::view(g))
}

#[test]
fn main_screen_with_fake_data() {
    let g = connected();
    assert_eq!(g.selected.as_deref(), Some("api"));
    let mut ui = sim(&g);
    for t in [
        "wardend 0.1.0 · pid 4211",
        "CPU 23.4% · Mem 3.10 GB / 7.66 GB · Load 0.52 0.40 0.31",
        "api",
        "jobs",
        "gave up",
        "web",
        "degraded",
        "reload 2/4: replacing worker 3 (4s)",
        "STARTING",
        "exit code 3",
        "Reload",
        "Hard restart",
        "4 workers",
        "Edit config",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the main screen");
    }
    save(&mut ui, "main-screen");

    // Clicking Reload asks nothing and sends the request; Stop asks first.
    let _ = ui.click("Reload").expect("a Reload button");
    let _ = ui.click("Stop").expect("a Stop button");
    let _ = ui.click("jobs").expect("the jobs row");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&msgs[0], Message::Act(app, Act::Reload) if app == "api"), "{msgs:?}");
    assert!(matches!(&msgs[1], Message::Act(app, Act::Stop) if app == "api"), "{msgs:?}");
    assert!(matches!(&msgs[2], Message::Select(app) if app == "jobs"), "{msgs:?}");
}

#[test]
fn an_app_that_gave_up_shows_its_problem_and_start() {
    let mut g = connected();
    let _ = g.update(Message::Select("jobs".into()));
    let mut ui = sim(&g);
    assert!(ui.find("No workers to show: the supervisor is not running (or wardend does not watch it yet).").is_ok());
    assert!(ui.find("api jobs supervisor gave up pid=6001 died 10 times in 10 minutes").is_err(), "lines have a clock");
    let _ = ui.click("Start").expect("Start is offered");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(matches!(&msgs[..], [Message::Act(app, Act::Start)] if app == "jobs"), "{msgs:?}");
}

#[test]
fn stop_asks_for_confirmation() {
    let mut g = connected();
    let _ = g.update(Message::Act("api".into(), Act::Stop));
    let mut ui = sim(&g);
    assert!(ui.find(Act::Stop.question("api").as_str()).is_ok());
    save(&mut ui, "confirm-stop");
    let _ = ui.click("Cancel").expect("Cancel");
    assert!(matches!(ui.into_messages().last(), Some(Message::Cancelled)));
}

#[test]
fn logs_tab() {
    let mut g = connected();
    let _ = g.update(Message::Tab(Tab::Logs));
    let _ = g.update(Message::LogFeed(FeedMsg::Connected { socket: "/run/warden/wardend.sock".into() }));
    let mut b = Batch::default();
    for i in 0..300 {
        let line = match i % 3 {
            0 => format!(
                "2026-09-30T12:00:{:02}.000Z OUT   worker={} stdout: GET /api/trips/{i} 200 4ms",
                i % 60,
                i % 4 + 1
            ),
            1 => format!("2026-09-30T12:00:{:02}.000Z OUT   worker=2 stderr: warn: slow query ({i} ms)", i % 60),
            _ => format!("2026-09-30T12:00:{:02}.000Z INFO  worker ready worker=3 pid=4270 startup_ms={i}", i % 60),
        };
        b.logs.push_back(("api".into(), line));
    }
    b.logs_dropped = 1200;
    let _ = g.update(Message::LogFeed(FeedMsg::Batch(b)));
    let _ = g.update(Message::LogHistory { app: "api".into(), result: Ok(vec![]) });
    let mut ui = sim(&g);
    assert!(ui.find("Pause").is_ok() && ui.find("stdout").is_ok());
    assert!(
        ui.find("2026-09-30T12:00:59.000Z INFO  worker ready worker=3 pid=4270 startup_ms=299").is_ok(),
        "the newest line shows"
    );
    assert!(
        ui.find("2026-09-30T12:00:00.000Z OUT   worker=1 stdout: GET /api/trips/0 200 4ms").is_err(),
        "old lines are not drawn"
    );
    save(&mut ui, "logs-tab");
    let _ = ui.click("Pause").expect("Pause");
    assert!(matches!(ui.into_messages().last(), Some(Message::LogsPause(true))));
}

#[test]
fn wardend_not_running() {
    let mut g = Gui::with_target(Target::local(Some("/run/user/1000/warden/wardend.sock".into())));
    let _ = g.update(Message::Feed(FeedMsg::Disconnected {
        error: "wardend is not running: there is no socket at /run/user/1000/warden/wardend.sock".into(),
        not_running: true,
        retry_in: Duration::from_secs(5),
        attempt: 6,
    }));
    let mut ui = sim(&g);
    assert!(ui.find("wardend is not running").is_ok());
    save(&mut ui, "not-running");
    let _ = ui.click("Start wardend").expect("the Start wardend button");
    assert!(matches!(ui.into_messages().last(), Some(Message::StartWardend)));
}

#[test]
fn dialogs_add_app_edit_config_and_connection() {
    let mut g = connected();
    let _ = g.update(Message::OpenAdd);
    for m in [
        Message::AddWhat("python3 -m http.server 8000".into()),
        Message::AddName("files".into()),
        Message::AddInstances("2".into()),
        Message::AddPort("8000".into()),
    ] {
        let _ = g.update(m);
    }
    let mut ui = sim(&g);
    assert!(ui.find("$ warden start 'python3 -m http.server 8000' --name files -i 2 --port 8000").is_ok());
    save(&mut ui, "add-app");
    let _ = ui.click("Add").expect("the Add button");
    assert!(matches!(ui.into_messages().last(), Some(Message::SubmitAdd)));

    let mut g = connected();
    let _ = g.update(Message::OpenEditor("api".into()));
    let text = "[app]\nname = \"api\"\nargs = [\"run\", \"dist/main.js\"]\nport = 3000\n\n[workerz]\ncount = 4\n";
    let _ = g.update(Message::EditorLoaded { app: "api".into(), result: Ok(text.into()) });
    let _ = g.update(Message::Validated(Err(
        "the config is not valid: /etc/warden/api.toml: unknown field `workerz`, expected one of `app`, `workers`, …"
            .into(),
    )));
    let mut ui = sim(&g);
    assert!(ui.find("api: /etc/warden/api.toml").is_ok());
    save(&mut ui, "edit-config");

    let mut g = connected();
    let _ = g.update(Message::OpenConnection);
    let _ = g.update(Message::ConnSsh(true));
    let _ = g.update(Message::ConnDest("deploy@web-1".into()));
    let mut ui = sim(&g);
    assert!(ui.find("Connect to").is_ok() && ui.find("SSH target").is_ok());
    save(&mut ui, "connection");
}

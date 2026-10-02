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
use warden_gui::app::{Act, Gui, MenuKind, Message, Tab, Target};
use warden_gui::client::{Batch, FeedMsg};
use warden_gui::history::Range;
use warden_gui::hosts::Machine;
use warden_gui::protocol::events::{AppHistory, Event, HostHistory, ResourceHistory};

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

/// `w` listening on these sockets (`status.workers[].listening`).
fn listening(mut w: serde_json::Value, on: serde_json::Value) -> serde_json::Value {
    w["listening"] = on;
    w
}

fn tcp(addr: &str, port: u16) -> serde_json::Value {
    json!({"kind": "tcp", "addr": addr, "port": port})
}

fn status(app: &str, workers: Vec<serde_json::Value>, ready: u64, rollout: serde_json::Value) -> serde_json::Value {
    json!({
        "app": app, "namespace": "default", "mode": "process", "config_path": format!("/etc/warden/{app}.toml"),
        "launched": "background", "version": "0.1.0", "pid": 4208, "uptime_secs": 86_400 + 3_723,
        "workers_configured": workers.len(), "workers_ready": ready, "healthy": true,
        "supervisor_rss_bytes": 4u64 << 20, "host": null, "reloading": !rollout.is_null(), "shutting_down": false, "user": "deploy",
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
            listening(worker(1, "RUNNING", Some(4230), 41, 1.2, 0, None), json!([tcp("0.0.0.0", 3000)])),
            listening(worker(2, "RUNNING", Some(4262), 40, 0.8, 1, Some("exit code 3")), json!([tcp("0.0.0.0", 3000)])),
            worker(3, "STARTING", Some(4270), 22, 9.5, 0, None),
            listening(
                worker(4, "RUNNING", Some(4241), 42, 1.1, 0, None),
                json!([tcp("0.0.0.0", 3000), tcp("127.0.0.1", 9229)]),
            ),
        ],
        3,
        rollout.clone(),
    );
    let web = status(
        "web",
        vec![
            listening(
                worker(1, "RUNNING", Some(5101), 64, 3.0, 0, None),
                json!([tcp("127.0.0.1", 8080), {"kind": "unix", "path": "/run/web/web.sock"}]),
            ),
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
    let _ = g.update(Message::HostHistoryLoaded(Ok(fake_history("", Range::Hour))));
    g
}

/// A deterministic wobble in [0, 1).
fn noise(i: usize, salt: u64) -> f32 {
    let mut x = (i as u64).wrapping_mul(6364136223846793005).wrapping_add(salt.wrapping_mul(1442695040888963407));
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    (x % 10_000) as f32 / 10_000.0
}

/// What wardend's `history` would answer for a busy day of `app` (and the
/// host), ending now: CPU with a deploy spike, memory that creeps up and is
/// recycled, a few restarts, dips in workers ready, and a gap where
/// wardend itself was restarted.
fn fake_history(app: &str, range: Range) -> ResourceHistory {
    let step = range.step_s();
    let n = 360usize;
    let end = warden_protocol::events::now_ms() / 1000 / step * step + step;
    let day = range == Range::Day;
    let gap = if day { 135..143 } else { 60..66 };
    let mb = |m: f32| Some((m * 1048576.0) as u64);
    let mut a = AppHistory { app: app.into(), ..AppHistory::default() };
    let mut host = HostHistory { mem_total_bytes: Some(8_220_000_000), ..HostHistory::default() };
    let mut mem = 150.0f32;
    for i in 0..n {
        if gap.contains(&i) {
            for v in [&mut a.cpu_percent, &mut host.cpu_percent, &mut host.load1] {
                v.push(None);
            }
            for v in [&mut a.workers_ready, &mut a.workers_configured, &mut a.restarts] {
                v.push(None);
            }
            a.rss_bytes.push(None);
            host.mem_used_bytes.push(None);
            continue;
        }
        let x = i as f32;
        let (cpu, restarts, ready) = if day {
            // Traffic: quiet at night, busy in the afternoon; recycles every ~6 h, a crash cluster.
            let traffic = (((x / n as f32) * std::f32::consts::TAU) - 1.9).sin().max(0.0);
            let recycle = i % 90 == 89;
            let crash = (250..253).contains(&i);
            mem = if recycle { 152.0 } else { mem + 1.15 + noise(i, 7) * 0.6 };
            let restarts = if crash { 2 } else { u32::from(recycle) };
            (3.0 + 22.0 * traffic + noise(i, 3) * 4.0, restarts, if crash || recycle { 3 } else { 4 })
        } else {
            let deploy = (210..226).contains(&i);
            let crash = i == 132 || i == 290 || i == 291;
            mem = if i == 132 {
                142.0
            } else if i == 211 {
                150.0
            } else {
                mem + 0.09 + noise(i, 11) * 0.12
            };
            let cpu = if deploy { 26.0 + noise(i, 5) * 14.0 } else { 2.2 + 1.4 * (x / 9.0).sin() + noise(i, 3) * 1.6 };
            (cpu, u32::from(crash), if crash || (212..224).contains(&i) { 3 } else { 4 })
        };
        a.cpu_percent.push(Some((cpu * 10.0).round() / 10.0));
        a.rss_bytes.push(mb(mem + noise(i, 13) * 2.0));
        a.restarts.push(Some(restarts));
        a.workers_ready.push(Some(ready));
        a.workers_configured.push(Some(4));
        host.cpu_percent.push(Some(((18.0 + 8.0 * (x / 23.0).sin() + noise(i, 17) * 6.0) * 10.0).round() / 10.0));
        host.mem_used_bytes.push(Some(3_150_000_000 + (noise(i, 19) * 180_000_000.0) as u64 + i as u64 * 200_000));
        host.load1.push(Some(0.5));
    }
    ResourceHistory {
        start_ms: (end - n as u64 * step) * 1000,
        step_s: step as u32,
        points: n as u32,
        host,
        apps: if app.is_empty() { vec![] } else { vec![a] },
    }
}

fn snapshot_dir() -> PathBuf {
    std::env::var_os("WARDEN_GUI_SNAPSHOT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("snapshots"))
}

/// Render, save as `<name>-tiny-skia.png`, and check it is not blank.
fn save(ui: &mut Simulator<'_, Message>, name: &str) {
    save_with(ui, name, &warden_gui::look::theme(false));
}

fn save_with(ui: &mut Simulator<'_, Message>, name: &str, theme: &Theme) {
    let dir = snapshot_dir();
    let base = dir.join(name);
    let png = dir.join(format!("{name}-tiny-skia.png"));
    let _ = std::fs::remove_file(&png);
    let snap = ui.snapshot(theme).expect("renders");
    assert!(snap.matches_image(&base).expect("writes the PNG"));
    let bytes = std::fs::metadata(&png).map(|m| m.len()).unwrap_or(0);
    // A blank 2560x1640 PNG compresses to a few KB; a screen with text is far larger.
    assert!(bytes > 40_000, "{} is only {bytes} bytes: nothing drawn?", png.display());
    eprintln!("snapshot: {}", png.display());
}

/// What the clicks sent, without the lists' own scroll reports.
fn messages(ui: Simulator<'_, Message>) -> Vec<Message> {
    ui.into_messages().filter(|m| !matches!(m, Message::FeedScrolled(_) | Message::LogsScrolled(_))).collect()
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
        "CPU 23.4%",
        "Mem 3.10 GB / 7.66 GB",
        "Load 0.52 0.40 0.31",
        "api",
        "jobs",
        "gave up",
        "web",
        "degraded",
        "reload 2/4: replacing worker 3 (4s)",
        "STARTING",
        "exit code 3",
        "Restart",
        "Stop",
        "Edit config",
        "3 / 4",
        "as deploy",
        "3000",
        "9229",
        "LISTENING",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the main screen");
    }
    save(&mut ui, "main-screen");
    save_with(&mut ui, "main-screen-light", &warden_gui::look::theme(true));

    // Clicking Restart is the safe reload and asks nothing; Stop asks first.
    let _ = ui.click("Restart").expect("a Restart button");
    let _ = ui.click("Stop").expect("a Stop button");
    let _ = ui.click("jobs").expect("the jobs row");
    let msgs = messages(ui);
    assert!(matches!(&msgs[0], Message::Act(app, Act::SafeReload) if app == "api"), "{msgs:?}");
    assert!(matches!(&msgs[1], Message::Act(app, Act::Stop) if app == "api"), "{msgs:?}");
    assert!(matches!(&msgs[2], Message::Select(app) if app == "jobs"), "{msgs:?}");
}

/// Hot standbys (`status.standbys`) are listed after the workers, as `s1`
/// (the name `warden status`, the logs and the events give them); old
/// processes still draining after a rollout (`status.draining`) come
/// between, as `N (old)`.
#[test]
fn standbys_and_draining_processes_are_listed_after_the_workers() {
    let mut g = connected();
    let mut web = status("web", vec![worker(1, "RUNNING", Some(5101), 64, 3.0, 0, None)], 1, json!(null));
    web["standbys"] = json!([worker(1, "STANDBY", Some(5150), 61, 0.0, 0, None)]);
    web["draining"] = json!([worker(1, "DRAINING", Some(5090), 66, 0.5, 0, None)]);
    let ev: Event = serde_json::from_value(json!({"type": "status", "app": "web", "status": web})).unwrap();
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![ev], ..Batch::default() })));
    let _ = g.update(Message::Select("web".into()));
    let mut ui = sim(&g);
    for t in ["s1 (standby)", "STANDBY", "5150", "5101", "1 (old)", "DRAINING", "5090"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the web app's screen");
    }
}

#[test]
fn an_app_that_gave_up_shows_its_problem_and_start() {
    let mut g = connected();
    let _ = g.update(Message::Select("jobs".into()));
    let mut ui = sim(&g);
    assert!(ui.find("Stopped trying").is_ok(), "the idle card says why");
    assert!(ui.find("Restart").is_err(), "only Start applies to an app that is not running");
    assert!(ui.find("api jobs supervisor gave up pid=6001 died 10 times in 10 minutes").is_err(), "lines have a clock");
    let _ = ui.click("Start").expect("Start is offered");
    let msgs = messages(ui);
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
fn history_tab_last_hour_with_a_crosshair() {
    let mut g = connected();
    let _ = g.update(Message::Tab(Tab::History));
    assert!(g.chart.as_ref().is_some_and(|c| c.app == "api" && c.range == Range::Hour));
    let _ = g.update(Message::HistoryLoaded {
        app: "api".into(),
        range: Range::Hour,
        result: Ok(fake_history("api", Range::Hour)),
    });
    // Tall enough for all four charts.
    let mut ui = Simulator::with_size(warden_gui::settings(), (1280.0, 1100.0), warden_gui::view::view(&g));
    for t in [
        "CPU",
        "% of one core",
        "Memory",
        "Restarts",
        "3 in 1 h",
        "Workers ready",
        "fewest 3 of 4",
        "1 h",
        "6 h",
        "24 h",
        "10 s per point · from wardend, then live",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the History tab");
    }
    // The pointer over the memory chart: a crosshair and the value there.
    let title = ui.find("Memory").expect("the memory chart").bounds();
    ui.point_at(iced::Point::new(title.x + 330.0, title.y + 90.0));
    save(&mut ui, "history-1h");
    let _ = ui.click("24 h").expect("the 24 h button");
    assert!(matches!(ui.into_messages().last(), Some(Message::HistoryRange(Range::Day))));
}

#[test]
fn history_tab_last_day_light() {
    let mut g = connected();
    let _ = g.update(Message::Tab(Tab::History));
    let _ = g.update(Message::HistoryRange(Range::Day));
    let _ = g.update(Message::HistoryLoaded {
        app: "api".into(),
        range: Range::Day,
        result: Ok(fake_history("api", Range::Day)),
    });
    let mut ui = sim(&g);
    assert!(ui.find("4 min per point · from wardend, then live").is_ok());
    assert!(ui.find("10 in 24 h").is_ok());
    save_with(&mut ui, "history-24h-light", &warden_gui::look::theme(true));
}

#[test]
fn history_tab_says_why_it_has_nothing() {
    let mut g = connected();
    let _ = g.update(Message::Tab(Tab::History));
    let _ = g.update(Message::HistoryLoaded {
        app: "api".into(),
        range: Range::Hour,
        result: Err("this wardend keeps no history: it is older than this GUI".into()),
    });
    let mut ui = sim(&g);
    assert!(ui.find("this wardend keeps no history: it is older than this GUI").is_ok());
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
    let _ = g.update(Message::AddMachine);
    let _ = g.update(Message::MachineDest("deploy@web-1".into()));
    let mut ui = sim(&g);
    assert!(ui.find("Add SSH machine").is_ok() && ui.find("deploy@web-1").is_ok());
    save(&mut ui, "machine");
}

/// The Restart dropdown: the safe reload is the button, the others are in the menu.
#[test]
fn restart_menu_lists_the_other_restarts() {
    let mut g = connected();
    let _ = g.update(Message::ToggleMenu(MenuKind::Restart));
    let mut ui = sim(&g);
    for t in ["Safe reload", "Reload", "Rolling restart", "Hard restart"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in the Restart menu");
    }
    save(&mut ui, "restart-menu");
    let _ = ui.click("Rolling restart").expect("a menu entry");
    let msgs = messages(ui);
    assert!(matches!(&msgs[..], [Message::Act(app, Act::RollingRestart)] if app == "api"), "{msgs:?}");
}

/// The Connection dropdown: this machine, the saved SSH machines, and a way to add one.
#[test]
fn connection_menu_lists_this_machine_and_the_saved_ones() {
    let mut g = connected();
    for dest in ["deploy@web-1", "deploy@web-2"] {
        g.saved.remember(Machine {
            dest: dest.into(),
            remote_socket: "/run/warden/wardend.sock".into(),
            remote_warden: "warden".into(),
        });
    }
    let _ = g.update(Message::ToggleMenu(MenuKind::Machines));
    let mut ui = sim(&g);
    for t in ["This machine", "deploy@web-1", "deploy@web-2", "Add SSH machine"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in the Connection menu");
    }
    save(&mut ui, "connection-menu");
    let _ = ui.click("deploy@web-2").expect("a saved machine");
    let msgs = messages(ui);
    assert!(matches!(&msgs[..], [Message::UseMachine(d)] if d == "deploy@web-2"), "{msgs:?}");
}

/// A column nobody fills in (loop delay without the shim, health without a check, last
/// exit before any exit) is left out; it is there as soon as one worker has a value.
#[test]
fn worker_table_leaves_out_columns_nothing_fills_in() {
    let mut g = connected();
    let w = |last_exit: Option<&str>, healthy: bool| {
        let mut w = worker(1, "RUNNING", Some(5101), 64, 3.0, 0, last_exit);
        w["healthy"] = json!(if healthy { Some(true) } else { None::<bool> });
        w
    };
    let only = status("web", vec![w(None, false)], 1, json!(null));
    let ev: Event = serde_json::from_value(json!({"type": "status", "app": "web", "status": only})).unwrap();
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![ev], ..Batch::default() })));
    let _ = g.update(Message::Select("web".into()));
    let mut ui = sim(&g);
    for t in ["Worker", "State", "PID", "Ports", "Uptime", "Restarts", "CPU", "RSS"] {
        assert!(ui.find(t).is_ok(), "{t:?} is always there");
    }
    for t in ["Loop p99", "Health", "Last exit"] {
        assert!(ui.find(t).is_err(), "{t:?} has nothing to show");
    }
    drop(ui);
    let mut busy = w(Some("signal 9 (SIGKILL)"), true);
    busy["loop_delay"] = json!({"p50_ms": 1.0, "p99_ms": 4.5, "max_ms": 9.0});
    let all = status("web", vec![busy], 1, json!(null));
    let ev: Event = serde_json::from_value(json!({"type": "status", "app": "web", "status": all})).unwrap();
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![ev], ..Batch::default() })));
    let mut ui = sim(&g);
    for t in ["Loop p99", "Health", "Last exit", "signal 9 (SIGKILL)"] {
        assert!(ui.find(t).is_ok(), "{t:?} is missing");
    }
    save(&mut ui, "workers-all-columns");
}

/// A window too narrow for the table: the card scrolls sideways instead of cutting columns.
#[test]
fn narrow_window_still_draws_the_table() {
    let mut g = connected();
    let _ = g.update(Message::Resized(iced::Size::new(900.0, 700.0)));
    let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, 700.0), warden_gui::view::view(&g));
    for t in ["Worker", "State", "RSS", "RUNNING", "Connected", "Add app", "CPU 23.4%"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the narrow screen");
    }
    for t in ["Load 0.52 0.40 0.31", "wardend 0.1.0 · pid 4211"] {
        assert!(ui.find(t).is_err(), "{t:?} is dropped from the top bar when there is no room");
    }
    save(&mut ui, "main-screen-narrow");
}

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
use warden_gui::cli_install::{self, Places, State, Status, Work};
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
        "build": {"path": "/usr/local/bin/warden", "stamp": "5489384:1790925000", "replaced": false},
        "cwd": format!("/srv/{app}/current"),
        "supervisor_rss_bytes": 4u64 << 20, "host": null, "reloading": !rollout.is_null(), "shutting_down": false, "user": "deploy",
        "rollout": rollout,
        "last_rollout": {"seq": 2, "kind": "reload", "ok": true, "message": "4 workers replaced", "duration_secs": 6.2},
        "workers": workers,
    })
}

/// What wardend sends a new subscriber, as NDJSON, then some events.
/// Responses as a worker or an app reports them: per second, the last minute, since start.
fn requests(rate: f64, minute: [u64; 5], total: [u64; 5]) -> serde_json::Value {
    let counts = |c: [u64; 5]| json!({"2xx": c[0], "3xx": c[1], "4xx": c[2], "404": c[3], "5xx": c[4]});
    json!({"rate": rate, "minute": counts(minute), "total": counts(total)})
}

fn fake_stream() -> Vec<Event> {
    let rollout =
        json!({"seq": 3, "kind": "reload", "phase": "replacing worker 3", "done": 2, "total": 4, "elapsed_secs": 4});
    let mut api = status(
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
    // Request health: the app's responses, and its port as the kernel sees it.
    for (i, (rate, five)) in [(14.1, 0), (13.7, 3), (0.0, 0), (13.4, 0)].into_iter().enumerate() {
        if i != 2 {
            api["workers"][i]["requests"] = requests(rate, [800, 10, 4, 3, five], [52_000, 400, 210, 180, five]);
        }
    }
    api["requests"] = requests(41.2, [2400, 30, 12, 9, 3], [156_000, 1200, 630, 540, 3]);
    api["ports"] = json!([{"port": 3000, "connections": 128, "backlog": 0, "max_backlog": 1533, "drops": 0}]);
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
        "CPU",
        "23.4%",
        "3.10 GB / 7.66 GB",
        "0.52 0.40 0.31",
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
        "Requests",
        "41.2/s",
        "1 min: 3 5xx · 12 4xx",
        "all interfaces · 128 open",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the main screen");
    }
    // Each worker's requests need a wider table: here the health and the last exit keep their place.
    assert!(ui.find("Req/s").is_err() && ui.find("Last exit").is_ok());
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

/// A window with room for it shows each worker's requests: per second, and the
/// 4xx and 5xx of the last minute (the worker with 5xx in red).
#[test]
fn a_wide_window_shows_each_workers_requests() {
    let g = connected();
    let mut ui = Simulator::with_size(warden_gui::settings(), (1680.0, 900.0), warden_gui::view::view(&g));
    for t in ["Req/s", "4xx/5xx", "14.1", "4 / 3", "4 / 0", "Health", "Last exit", "exit code 3"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in the wide window");
    }
    save(&mut ui, "main-screen-wide");
}

/// The window in the desktop's own colors: macOS and GNOME/Ubuntu, light and dark,
/// with their accents. Each is drawn and saved (`native-*`), and the texts that
/// matter are still on it; the theme is what the desktop said, nothing else.
#[test]
fn the_desktop_colors_draw_the_main_screen() {
    use warden_gui::system::{Accent, Colors, Flavor, Mode, Source, System, theme};
    let desktop = Source { colors: Colors::System, mode: Mode::Auto };
    let g = connected();
    for (name, flavor, dark, accent) in [
        ("native-mac-light", Flavor::Mac, false, Accent::Blue),
        ("native-mac-dark", Flavor::Mac, true, Accent::Purple),
        ("native-ubuntu-light", Flavor::Gnome, false, Accent::Orange),
        ("native-gnome-dark", Flavor::Gnome, true, Accent::Teal),
    ] {
        let system = System { flavor, dark: Some(dark), accent };
        let t = theme(desktop, &system);
        let mut ui = sim(&g);
        for text in ["api", "gave up", "Restart", "RUNNING", "LISTENING"] {
            assert!(ui.find(text).is_ok(), "{name}: {text:?} is not on the screen");
        }
        save_with(&mut ui, name, &t);
    }
}

/// The charts take the accent of the desktop, and keep their series apart.
#[test]
fn the_charts_follow_the_desktop_accent() {
    use warden_gui::system::{Accent, Colors, Flavor, Mode, Source, System, theme};
    let desktop = Source { colors: Colors::System, mode: Mode::Auto };
    let mut g = connected();
    let _ = g.update(Message::Tab(Tab::History));
    let _ = g.update(Message::HistoryLoaded {
        app: "api".into(),
        range: Range::Hour,
        result: Ok(fake_history("api", Range::Hour)),
    });
    let t = theme(desktop, &System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple });
    let mut ui = Simulator::with_size(warden_gui::settings(), (1280.0, 1100.0), warden_gui::view::view(&g));
    assert!(ui.find("Workers ready").is_ok());
    save_with(&mut ui, "native-mac-dark-history", &t);
    // Four series, four colors, in every look.
    for t in [
        theme(desktop, &System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple }),
        theme(desktop, &System { flavor: Flavor::Gnome, dark: Some(false), accent: Accent::Green }),
        warden_gui::look::theme(true),
        warden_gui::look::theme(false),
    ] {
        use warden_gui::charts::Hue;
        let c = [Hue::Blue, Hue::Aqua, Hue::Orange, Hue::Violet].map(|h| h.color(&t));
        for i in 0..4 {
            for j in i + 1..4 {
                let d = (c[i].r - c[j].r).abs() + (c[i].g - c[j].g).abs() + (c[i].b - c[j].b).abs();
                assert!(d > 0.15, "{t}: series {i} and {j} are alike: {c:?}");
            }
        }
    }
}

/// Settings: Colors (Warden or System) and Mode (Auto, Light or Dark), the gear in the top
/// bar. The window is drawn in each choice, with the dialog over it.
#[test]
fn settings_dialog_offers_colors_and_mode() {
    use warden_gui::system::{Accent, Colors, Flavor, Mode, Source, System, theme};
    let mut g = connected();
    // As on a Mac with Warden.app in Applications and no `warden` on PATH yet (the banner was
    // turned away, so the main screen behind the dialog is the usual one).
    g.cli = State { places: Ok(mac_places()), status: Status::Missing, work: Work::Idle, cancelled: false };
    g.saved.cli_banner_dismissed = true;
    let mut ui = sim(&g);
    let _ = ui.click(warden_gui::icons::Icon::Settings.glyph().to_string().as_str());
    assert!(matches!(messages(ui).as_slice(), [Message::OpenSettings]), "the gear opens Settings");

    let _ = g.update(Message::OpenSettings);
    let mac = System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple };
    let _ = g.update(Message::System(mac));
    let mut ui = sim(&g);
    for t in [
        "Settings",
        "COLORS",
        "MODE",
        "COMMAND LINE TOOL",
        "Install command line tool",
        "System",
        "Auto",
        "Light",
        "Dark",
        "Done",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in Settings");
    }
    assert!(ui.find("This desktop: macOS, dark, purple accent.").is_ok());
    save(&mut ui, "settings");
    let _ = ui.click("System").expect("the System choice");
    let _ = ui.click("Light").expect("the Light choice");
    let msgs = messages(ui);
    assert!(matches!(&msgs[0], Message::SetColors(Colors::System)), "{msgs:?}");
    assert!(matches!(&msgs[1], Message::SetMode(Mode::Light)), "{msgs:?}");

    // And with the desktop's colors chosen.
    let _ = g.update(Message::SetColors(Colors::System));
    let mut ui = sim(&g);
    save_with(&mut ui, "settings-system", &theme(Source { colors: Colors::System, mode: Mode::Auto }, &mac));
}

/// Settings also restarts everything (`warden update`): one click asks, the second runs it, and
/// nothing runs from the first.
#[test]
fn settings_restarts_everything_after_asking() {
    use warden_gui::commands::{Restart, Who};
    let mut g = connected();
    let _ = g.update(Message::OpenSettings);
    let mut ui = sim(&g);
    assert!(ui.find("RESTART EVERYTHING").is_ok() && ui.find("Restart every app now?").is_err());
    let _ = ui.click("Restart everything\u{2026}").expect("the button");
    assert!(matches!(messages(ui).as_slice(), [Message::AskRestartAll]));

    // Asking: before it is known which warden would run, "Restart now" waits.
    let _ = g.update(Message::AskRestartAll);
    let mut ui = sim(&g);
    assert!(ui.find("Restart every app now?").is_ok());
    assert!(ui.find("Looking for the warden that would run\u{2026}").is_ok());
    let _ = ui.click("Restart now").expect("the button is there, not pressable");
    assert!(messages(ui).is_empty(), "a restart that has not said what it runs sends nothing");

    // Then the binary and its version, the wardend it replaces, and the one it is aimed at.
    let who = Who { command: "/usr/local/bin/warden".into(), version: "0.2.0".into() };
    let _ = g.update(Message::RestartWho(Ok(who)));
    let mut ui = sim(&g);
    for t in [
        "Runs /usr/local/bin/warden (warden 0.2.0). It replaces wardend 0.1.0.",
        "On the wardend in /run/warden (the one this window shows).",
    ] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in the dialog");
    }
    save(&mut ui, "settings-restart");
    let _ = ui.click("Restart now").expect("the confirm button");
    assert!(matches!(messages(ui).as_slice(), [Message::RestartAll]));
    let mut ui = sim(&g);
    let _ = ui.click("Cancel").expect("cancel");
    assert!(matches!(messages(ui).as_slice(), [Message::CancelRestartAll]));

    // Running: what it does and for how long, and no second start.
    let _ = g.update(Message::RestartAll);
    let _ = g.update(Message::RestartTick);
    let _ = g.update(Message::RestartTick);
    let mut ui = sim(&g);
    let running = "Restarting: saving what runs, stopping every supervisor and wardend, starting them again\u{2026} 2 s. \
                   The window reconnects when wardend is back. Wait for it to finish: closing the window now would \
                   interrupt it.";
    assert!(ui.find(running).is_ok(), "the running line is not in the dialog");
    assert!(ui.find("Restart every app now?").is_err() && ui.find("Restart everything\u{2026}").is_err());
    drop(ui);

    // Not finished: the apps may be stopped, and the way back is a button.
    let lost = Restart {
        ok: false,
        text:
            "it failed: `warden update --yes` did not finish within 900 s; it was stopped. Apps may be stopped: what \
               was running was saved first, so starting it again brings them back (use \"Start the saved apps again\" \
               in Settings, or run `warden resurrect`)."
                .into(),
        stopped: true,
    };
    let note = lost.text.clone();
    let _ = g.update(Message::RestartedAll(lost));
    let mut ui = sim(&g);
    assert!(ui.find(note.as_str()).is_ok(), "the warning stays in Settings");
    save(&mut ui, "settings-restart-failed");
    let _ = ui.click("Start the saved apps again").expect("the way back");
    assert!(matches!(messages(ui).as_slice(), [Message::ResurrectAll]));
}

/// A supervisor keeps the code it started with: after a rebuild it shows `-` for what a newer
/// Warden knows, and the page says so, with a button that restarts everything (asking first).
#[test]
fn an_older_supervisor_is_named_with_the_button_that_restarts_it() {
    const BUTTON: &str = "Restart all apps\u{2026}";
    let mut g = connected();
    let _ = g.update(Message::Select("web".into()));
    let mut ui = sim(&g);
    assert!(ui.find("web").is_ok() && !text_has(&mut ui, "supervisor was"), "no banner for a current one");
    drop(ui);
    for (what, edit) in [
        ("was started by an older warden", json!(null)),
        ("was started before warden was rebuilt or upgraded", json!({"replaced": true})),
    ] {
        let mut g = connected();
        let mut old = status("web", vec![worker(1, "RUNNING", Some(5101), 64, 3.0, 0, None)], 1, json!(null));
        if edit.is_null() {
            old.as_object_mut().unwrap().remove("build");
        } else {
            old["build"]["replaced"] = edit["replaced"].clone();
        }
        let ev: Event = serde_json::from_value(json!({"type": "status", "app": "web", "status": old})).unwrap();
        let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![ev], ..Batch::default() })));
        let _ = g.update(Message::Select("web".into()));
        let mut ui = sim(&g);
        let line = format!("This app's supervisor {what}: CPU, memory and ports may show \u{2013}.");
        assert!(ui.find(line.as_str()).is_ok(), "the banner for {what:?} is missing");
        if edit.is_null() {
            save(&mut ui, "older-supervisor");
        }
        let _ = ui.click(BUTTON).expect("and the button");
        assert!(matches!(messages(ui).as_slice(), [Message::AskRestartAll]), "it asks, as Settings does");
    }
}

/// The folder a project lives in sits under its name (the folder a static site serves, for
/// one), and is not there for a supervisor that does not report it yet.
#[test]
fn the_project_folder_is_shown_under_its_name() {
    let mut g = connected();
    let _ = g.update(Message::Select("web".into()));
    let mut ui = sim(&g);
    assert!(ui.find("/srv/web/current").is_ok(), "the folder under the name");
    drop(ui);

    let mut g = connected();
    let mut old = status("web", vec![worker(1, "RUNNING", Some(5101), 64, 3.0, 0, None)], 1, json!(null));
    old.as_object_mut().unwrap().remove("cwd");
    let ev: Event = serde_json::from_value(json!({"type": "status", "app": "web", "status": old})).unwrap();
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![ev], ..Batch::default() })));
    let _ = g.update(Message::Select("web".into()));
    let mut ui = sim(&g);
    assert!(ui.find("/srv/web/current").is_err(), "nothing to show without it");
}

fn text_has(ui: &mut Simulator<'_, Message>, needle: &str) -> bool {
    ui.find(needle).is_ok()
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

/// Many saved machines in the smallest window: the list scrolls, and "Add SSH machine" stays in view.
#[test]
fn connection_menu_scrolls_when_there_are_more_machines_than_room() {
    let mut g = connected();
    for i in 1..=14 {
        g.saved.remember(Machine {
            dest: format!("deploy@web-{i}"),
            remote_socket: "/run/warden/wardend.sock".into(),
            remote_warden: "warden".into(),
        });
    }
    let _ = g.update(Message::ToggleMenu(MenuKind::Machines));
    for height in [560.0, 640.0, 900.0] {
        let _ = g.update(Message::Resized(iced::Size::new(900.0, height)));
        let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, height), warden_gui::view::view(&g));
        let add = ui.find("Add SSH machine").expect("the way to add one").bounds();
        assert!(add.y + add.height <= height, "at {height}: Add SSH machine is at {add:?}, below the window");
        let first = ui.find("deploy@web-1").expect("the first machine").bounds();
        assert!(first.y + first.height <= add.y, "at {height}: the list is above Add SSH machine");
        let this = ui.find("This machine").expect("this machine");
        assert!(this.bounds().y >= 0.0);
        if height == 560.0 {
            save(&mut ui, "connection-menu-many");
        }
    }
    // The rows themselves are still pressed to connect.
    let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, 560.0), warden_gui::view::view(&g));
    let _ = ui.click("deploy@web-2").expect("a machine in the list");
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
    for t in ["Worker", "State", "RSS", "RUNNING", "Connected", "Add app", "23.4%"] {
        assert!(ui.find(t).is_ok(), "{t:?} is not on the narrow screen");
    }
    for t in ["0.52 0.40 0.31", "wardend 0.1.0 · pid 4211"] {
        assert!(ui.find(t).is_err(), "{t:?} is dropped from the top bar when there is no room");
    }
    save(&mut ui, "main-screen-narrow");
}

/// A long app name is cut to what its row leaves (the state label stays), in the list and in the
/// header of the page (the state and Edit config stay), and the whole name is in a tooltip. Drawn
/// at the smallest window.
#[test]
fn a_long_app_name_leaves_room_for_its_state_and_edit_config() {
    let long = "customer-facing-checkout-and-billing-api-service-v2";
    let mut g = connected();
    let apps: Event = serde_json::from_value(json!({"type": "apps", "apps": [
        {"name": long, "namespace": "default", "config": "/srv/customer-facing-checkout-and-billing/deploy/warden.toml",
         "socket": "/run/warden/long/control.sock", "state": "gave_up", "supervised_by": "wardend", "supervisor_restarts": 10,
         "problem": "died 10 times in 10 minutes; wardend stopped restarting it"},
        {"name": "api", "namespace": "default", "config": "/etc/warden/api.toml", "socket": "/run/warden/api/control.sock",
         "state": "running", "supervised_by": "wardend"},
    ]}))
    .unwrap();
    let _ = g.update(Message::Feed(FeedMsg::Batch(Batch { events: vec![apps], ..Batch::default() })));
    let _ = g.update(Message::Select(long.into()));
    let size = warden_gui::app::WINDOW_MIN;
    let _ = g.update(Message::Resized(size));
    let mut ui = Simulator::with_size(warden_gui::settings(), (size.width, size.height), warden_gui::view::view(&g));
    assert!(ui.find(long).is_err(), "the name is cut, not drawn over its neighbours");
    for t in ["gave up", "Edit config", "Add app"] {
        assert!(ui.find(t).is_ok(), "{t:?} is still on the screen");
    }
    assert!(ui.find("api").is_ok(), "and the other app's row");
    save(&mut ui, "long-app-name");
}

/// What a Mac with `Warden.app` in Applications knows about itself, with no file touched: the
/// folders are only names here.
fn mac_places() -> Places {
    Places {
        exe: "/Applications/Warden.app/Contents/MacOS/warden-gui".into(),
        cli: "/Applications/Warden.app/Contents/MacOS/warden".into(),
        system_dir: Some("/usr/local/bin".into()),
        user_dir: Some("/Users/ana/.local/bin".into()),
        search: vec!["/usr/local/bin".into(), "/opt/homebrew/bin".into()],
        path: vec!["/usr/bin".into(), "/bin".into()],
        shell: Some("/bin/zsh".into()),
        home: Some("/Users/ana".into()),
        mac: true,
    }
}

fn with_cli(status: Status, work: Work) -> Gui {
    let mut g = connected();
    g.cli = State { places: Ok(mac_places()), status, work, cancelled: false };
    g
}

/// Settings open over the main screen, with the banner turned away (it has the same button).
fn settings_with(status: Status, work: Work) -> Gui {
    let mut g = with_cli(status, work);
    g.saved.cli_banner_dismissed = true;
    let _ = g.update(Message::OpenSettings);
    g
}

/// Settings offers "Install command line tool": what `warden` is here, the action that fits
/// (install, uninstall, install again), and the line that puts a folder on PATH when it needs one.
#[test]
fn settings_installs_and_removes_the_command_line_tool() {
    // Not there: the explanation, and one button.
    let g = settings_with(Status::Missing, Work::Idle);
    let mut ui = sim(&g);
    assert!(ui.find("COMMAND LINE TOOL").is_ok());
    assert!(ui.find("Uninstall").is_err());
    save(&mut ui, "settings-cli");
    let _ = ui.click("Install command line tool").expect("the install button");
    let m = messages(ui);
    assert!(matches!(m.as_slice(), [Message::InstallCli]), "{m:?}");

    // Installed in /usr/local/bin: said, and removable.
    let link = std::path::PathBuf::from("/usr/local/bin/warden");
    let target = std::path::PathBuf::from("/Applications/Warden.app/Contents/MacOS/warden");
    let g = settings_with(Status::Linked { link: link.clone(), target: target.clone() }, Work::Idle);
    let mut ui = sim(&g);
    for t in ["Installed: /usr/local/bin/warden", "A link to /Applications/Warden.app/Contents/MacOS/warden."] {
        assert!(ui.find(t).is_ok(), "{t:?} is not in Settings");
    }
    assert!(ui.find("Install command line tool").is_err(), "nothing to install");
    save(&mut ui, "settings-cli-installed");
    let _ = ui.click("Uninstall").expect("the uninstall button");
    assert!(matches!(messages(ui).as_slice(), [Message::UninstallCli]));

    // Linked in ~/.local/bin, which a window opened from Finder does not have on its PATH: the line.
    let mut g = settings_with(
        Status::Linked { link: "/Users/ana/.local/bin/warden".into(), target: target.clone() },
        Work::Idle,
    );
    let mut ui = sim(&g);
    // One command, and the same one run twice adds the line once.
    let line = "grep -qsF 'export PATH=\"$HOME/.local/bin:$PATH\"' ~/.zshrc \
                || echo 'export PATH=\"$HOME/.local/bin:$PATH\"' >> ~/.zshrc";
    assert!(ui.find(line).is_ok(), "the line to add is shown");
    assert!(ui.find("If a new Terminal does not find `warden`, put /Users/ana/.local/bin on your PATH (zsh):").is_ok());
    save(&mut ui, "settings-cli-path");
    let _ = ui.click(line).expect("the line is a button that copies");
    assert!(matches!(messages(ui).as_slice(), [Message::Copy(c)] if c == line));
    // The folder is on PATH: no line.
    let mut places = mac_places();
    places.path.push("/Users/ana/.local/bin".into());
    g.cli.places = Ok(places);
    let mut ui = sim(&g);
    assert!(ui.find(line).is_err());
    drop(ui);

    // The app moved: the link is dead, and can be made again or removed.
    let g = settings_with(
        Status::Broken { link, target: "/Users/ana/Downloads/Warden.app/Contents/MacOS/warden".into() },
        Work::Idle,
    );
    let mut ui = sim(&g);
    assert!(ui.find("Install again").is_ok() && ui.find("Remove the link").is_ok());
    let _ = ui.click("Install again").expect("install again");
    let _ = ui.click("Remove the link").expect("remove it");
    assert!(matches!(messages(ui).as_slice(), [Message::InstallCli, Message::UninstallCli]));

    // Somebody else's warden (Homebrew, a package, install.sh): said, and left alone.
    let g = settings_with(Status::Present { path: "/opt/homebrew/bin/warden".into() }, Work::Idle);
    let mut ui = sim(&g);
    assert!(ui.find("`warden` is installed: /opt/homebrew/bin/warden").is_ok());
    assert!(ui.find("Install command line tool").is_err() && ui.find("Uninstall").is_err());

    // On a Mac that can ask for the password there is a second way that never does, and a cancelled
    // prompt installs nothing and says so.
    let mut g = settings_with(Status::Missing, Work::Idle);
    let mut ui = sim(&g);
    assert!(ui.find("Install for this user only").is_ok(), "the choice that needs no password");
    let _ = ui.click("Install for this user only").expect("the button");
    assert!(matches!(messages(ui).as_slice(), [Message::InstallCliForMe]));
    g.cli.cancelled = true;
    let mut ui = sim(&g);
    assert!(ui.find("The administrator prompt was cancelled, so nothing was installed.").is_ok());
    assert!(ui.find("Install command line tool").is_ok() && ui.find("Install for this user only").is_ok());
    save(&mut ui, "settings-cli-cancelled");
    drop(ui);

    // A link into another copy of the app (an older download, `Warden 2.app`): said, and it can be
    // pointed at this one or removed.
    let g = settings_with(
        Status::Linked {
            link: "/usr/local/bin/warden".into(),
            target: "/Users/ana/Downloads/Warden 2.app/Contents/MacOS/warden".into(),
        },
        Work::Idle,
    );
    let mut ui = sim(&g);
    assert!(
        ui.find(
            "/usr/local/bin/warden is a link to another copy of Warden: \
             /Users/ana/Downloads/Warden 2.app/Contents/MacOS/warden"
        )
        .is_ok()
    );
    assert!(ui.find("This app's command is /Applications/Warden.app/Contents/MacOS/warden.").is_ok());
    save(&mut ui, "settings-cli-other-copy");
    let _ = ui.click("Point it at this app").expect("repoint");
    let _ = ui.click("Uninstall").expect("remove it");
    assert!(matches!(messages(ui).as_slice(), [Message::InstallCli, Message::UninstallCli]));

    // Working: the button says so and does nothing.
    let g = settings_with(Status::Missing, Work::Installing);
    let mut ui = sim(&g);
    let _ = ui.click("Installing\u{2026}").expect("the busy button is there");
    assert!(messages(ui).is_empty(), "a button that is busy sends nothing");

    // Not possible from here (the CLI is not beside the window): the reason, no button.
    let mut g = settings_with(Status::Missing, Work::Idle);
    g.cli = State::of(Err("There is no `warden` next to this program (/opt/w), so there is nothing to link.".into()));
    let mut ui = sim(&g);
    assert!(ui.find("There is no `warden` next to this program (/opt/w), so there is nothing to link.").is_ok());
    assert!(ui.find("Install command line tool").is_err());
}

/// The first-run banner: only from an app bundle with no `warden` anywhere, and "Not now" is for good.
#[test]
fn first_run_banner_offers_the_command_line_tool_once() {
    const SENTENCE: &str =
        "Use Warden from Terminal: install the `warden` command, so `warden list` and `warden start` work there.";
    let g = with_cli(Status::Missing, Work::Idle);
    assert!(g.cli.banner(g.saved.cli_banner_dismissed));
    let mut ui = sim(&g);
    assert!(ui.find(SENTENCE).is_ok());
    save(&mut ui, "cli-banner");
    let _ = ui.click("Install command line tool").expect("the install button");
    let _ = ui.click("Not now").expect("Not now");
    assert!(matches!(messages(ui).as_slice(), [Message::InstallCli, Message::DismissCliBanner]));

    // In the narrowest window the sentence wraps and the buttons stay.
    let mut narrow = with_cli(Status::Missing, Work::Idle);
    let _ = narrow.update(Message::Resized(iced::Size::new(900.0, 700.0)));
    let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, 700.0), warden_gui::view::view(&narrow));
    let install = ui.find("Install command line tool").expect("the button").bounds();
    let not_now = ui.find("Not now").expect("Not now").bounds();
    assert!(not_now.x + not_now.width <= 900.0 && install.x + install.width <= not_now.x, "{install:?} {not_now:?}");
    save(&mut ui, "cli-banner-narrow");
    drop(ui);

    // Working: the button says so.
    let g = with_cli(Status::Missing, Work::Installing);
    let mut ui = sim(&g);
    let _ = ui.click("Installing\u{2026}").expect("the busy button");
    assert!(messages(ui).is_empty());

    // Turned away: gone, and it stays gone (it is in gui.json).
    let mut g = with_cli(Status::Missing, Work::Idle);
    let dir = std::env::temp_dir().join(format!("wg-render-banner-{}", std::process::id()));
    g.saved_path = Some(dir.join("gui.json"));
    let _ = g.update(Message::DismissCliBanner);
    assert!(sim(&g).find(SENTENCE).is_err());
    assert!(warden_gui::hosts::load(&dir.join("gui.json")).cli_banner_dismissed);
    let _ = std::fs::remove_dir_all(&dir);

    // Not offered: from a plain folder (Linux, a checkout), with a `warden` already, or when it
    // cannot be done from here; and the main screen of a window that looked at nothing has none.
    let mut linux = mac_places();
    linux.exe = "/home/ana/warden-gui-0.1.0-linux-x86_64/warden-gui".into();
    for g in [
        {
            let mut g = connected();
            g.cli = State { places: Ok(linux), status: Status::Missing, work: Work::Idle, cancelled: false };
            g
        },
        with_cli(Status::Present { path: "/opt/homebrew/bin/warden".into() }, Work::Idle),
        with_cli(Status::Linked { link: "/usr/local/bin/warden".into(), target: "/x/warden".into() }, Work::Idle),
        {
            let mut g = connected();
            g.cli = State::of(Err("no".into()));
            g
        },
        connected(),
    ] {
        assert!(sim(&g).find(SENTENCE).is_err());
    }
    let _ = cli_install::NAME;
}

/// Settings fits a short window: its sections scroll, and the heading and Done stay in view.
#[test]
fn settings_scrolls_in_a_short_window_and_keeps_done_in_view() {
    let mut g = connected();
    g.saved.cli_banner_dismissed = true;
    let _ = g.update(Message::OpenSettings);
    // Tall: nothing needs to scroll, so Done sits below the last section.
    let mut tall = sim(&g);
    let tall_done = tall.find("Done").expect("Done").bounds();
    let tall_restart = tall.find("Restart everything\u{2026}").expect("the button").bounds();
    assert!(tall_done.y > tall_restart.y);
    drop(tall);
    for height in [560.0, 640.0] {
        let _ = g.update(Message::Resized(iced::Size::new(900.0, height)));
        let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, height), warden_gui::view::view(&g));
        let done = ui.find("Done").expect("Done").bounds();
        let head = ui.find("Settings").expect("the heading").bounds();
        assert!(head.y >= 0.0 && done.y + done.height <= height, "at {height}: Done is at {done:?}, heading {head:?}");
        if height == 560.0 {
            save(&mut ui, "settings-short");
        }
    }
}

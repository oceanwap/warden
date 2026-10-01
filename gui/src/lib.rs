//! Warden's native GUI: a client of wardend (docs/protocol.md).
//!
//! It runs as its own process and never touches an app directly: every app,
//! its workers and its events come from wardend's `subscribe` stream, and
//! every action is a short request on wardend's socket (locally, or through
//! an SSH tunnel to a remote host). Commands that wardend does not serve
//! (adding an app, checking a config, starting wardend) run the `warden` CLI.
//!
//! Layout:
//! - `client`: wardend's socket (one long-lived subscription with reconnect
//!   and backoff, short requests), all off the UI thread;
//! - `ssh`: the tunnel to a remote wardend, and shell quoting for remote commands;
//! - `commands`: the `warden` CLI as a subprocess (local, or over SSH);
//! - `model`, `logs`, `history`, `format`, `ring`: state and text, no I/O
//!   (unit-tested);
//! - `app`, `view`, `charts`: the iced program (update, subscriptions,
//!   widgets, the canvas charts).

#![forbid(unsafe_code)]

pub mod app;
pub mod charts;
pub mod client;
pub mod commands;
pub mod format;
pub mod history;
pub mod logs;
pub mod model;
pub mod ring;
pub mod ssh;
pub mod view;

pub use warden_protocol as protocol;

/// Text size and fonts: the system's sans-serif and monospace fonts.
pub fn settings() -> iced::Settings {
    iced::Settings { default_text_size: 14.into(), ..iced::Settings::default() }
}

/// Dark, unless `WARDEN_GUI_THEME=light`.
pub fn theme() -> iced::Theme {
    match std::env::var("WARDEN_GUI_THEME").as_deref() {
        Ok("light") => iced::Theme::Light,
        _ => iced::Theme::Dark,
    }
}

/// Open the window and run until it closes.
pub fn run(opts: app::Options) -> iced::Result {
    let theme = theme();
    iced::application(move || app::Gui::new(opts.clone()), app::Gui::update, view::view)
        .title(app::Gui::title)
        .subscription(app::Gui::subscription)
        .theme(move |_: &app::Gui| theme.clone())
        .settings(settings())
        .window(iced::window::Settings {
            size: iced::Size::new(1280.0, 820.0),
            min_size: Some(iced::Size::new(900.0, 560.0)),
            ..iced::window::Settings::default()
        })
        .run()
}

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
pub mod dropdown;
pub mod format;
pub mod history;
pub mod hosts;
pub mod icons;
pub mod logs;
pub mod look;
pub mod model;
pub mod ring;
pub mod ssh;
pub mod view;

pub use warden_protocol as protocol;

/// The embedded fonts (gui/assets/fonts/LICENSES.txt: OFL and ISC): Inter for
/// text, JetBrains Mono for logs, Lucide for icons. Any other script falls
/// back to the system's fonts.
fn fonts() -> Vec<std::borrow::Cow<'static, [u8]>> {
    use std::borrow::Cow::Borrowed;
    vec![
        Borrowed(include_bytes!("../assets/fonts/Inter-Regular.ttf")),
        Borrowed(include_bytes!("../assets/fonts/Inter-Medium.ttf")),
        Borrowed(include_bytes!("../assets/fonts/Inter-SemiBold.ttf")),
        Borrowed(include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf")),
        Borrowed(include_bytes!("../assets/fonts/lucide.ttf")),
    ]
}

/// Text size and fonts.
pub fn settings() -> iced::Settings {
    iced::Settings {
        default_text_size: 14.into(),
        default_font: look::SANS,
        fonts: fonts(),
        ..iced::Settings::default()
    }
}

/// Dark, unless `WARDEN_GUI_THEME=light`.
pub fn theme() -> iced::Theme {
    look::theme(std::env::var("WARDEN_GUI_THEME").as_deref() == Ok("light"))
}

/// The window icon (assets/icon/build.py makes it): 128 x 128 raw RGBA.
/// macOS shows an app's icon from its bundle (Warden.app), not from the window.
fn window_icon() -> Option<iced::window::Icon> {
    iced::window::icon::from_rgba(include_bytes!("../assets/icon-128.rgba").to_vec(), 128, 128).ok()
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
            size: app::WINDOW,
            min_size: Some(app::WINDOW_MIN),
            icon: window_icon(),
            // Wayland and X11 match the window to assets/warden-gui.desktop (and its icon) by this name.
            #[cfg(target_os = "linux")]
            platform_specific: iced::window::settings::PlatformSpecific {
                application_id: "warden-gui".into(),
                ..iced::window::settings::PlatformSpecific::default()
            },
            ..iced::window::Settings::default()
        })
        .run()
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_window_icon_is_a_128_pixel_square_with_something_drawn() {
        let rgba = include_bytes!("../assets/icon-128.rgba");
        assert_eq!(rgba.len(), 128 * 128 * 4);
        assert!(super::window_icon().is_some());
        let opaque = rgba.chunks(4).filter(|p| p[3] == 255).count();
        assert!(opaque > 128 * 128 / 2, "the plate fills most of the icon: {opaque} opaque pixels");
    }
}

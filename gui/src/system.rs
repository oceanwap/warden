//! The system's own look: light or dark, and the accent color, on macOS and on
//! GNOME and Ubuntu, so the window sits in the desktop it runs on.
//!
//! They are read by asking the desktop (`defaults` on macOS, `gsettings` on
//! GNOME and Ubuntu): no unsafe code, no library for each desktop. Anything
//! else, or a desktop that does not answer, keeps the Warden palette, light or
//! dark as far as it can be told (dark when not). The window asks again when
//! it is focused and when iced reports a change of mode, and on GNOME a
//! `gsettings monitor` tells it at once.
//!
//! - macOS: `AppleInterfaceStyle` (`Dark`, absent when light) and
//!   `AppleAccentColor` (-1 graphite, 0 red, 1 orange, 2 yellow, 3 green,
//!   4 blue, 5 purple, 6 pink; absent is multicolor, which draws blue). The
//!   colors are Apple's system colors (the accessible ones for text on light).
//! - GNOME, Ubuntu: `color-scheme` (GNOME 42), `accent-color` (GNOME 47 and
//!   Ubuntu 24.10), else the accent in the name of Ubuntu's Yaru theme
//!   (`Yaru-purple-dark`), and libadwaita's own colors.

use crate::look;
use iced::theme::Palette;
use iced::{Color, Theme, color};
use serde::{Deserialize, Serialize};

/// Which desktop the window is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Mac,
    Gnome,
    /// Anything else: the Warden palette.
    Other,
}

/// The accent colors the desktops offer (macOS and GNOME, and Ubuntu's Yaru).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accent {
    Blue,
    Teal,
    Green,
    Yellow,
    Orange,
    Red,
    Pink,
    Purple,
    /// Graphite on macOS, slate on GNOME.
    Slate,
    // Yaru's own.
    Magenta,
    Bark,
    Sage,
    Olive,
    Viridian,
    PrussianGreen,
}

/// What the desktop says about its look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct System {
    pub flavor: Flavor,
    /// `None` when it cannot be told.
    pub dark: Option<bool>,
    pub accent: Accent,
}

impl System {
    pub const UNKNOWN: System = System { flavor: Flavor::Other, dark: None, accent: Accent::Blue };

    /// What Settings tells: the desktop, its mode and its accent.
    pub fn describe(&self) -> String {
        let mode = match self.dark {
            Some(true) => "dark",
            Some(false) => "light",
            None => "light or dark not told",
        };
        match self.flavor {
            Flavor::Mac => format!("macOS, {mode}, {} accent", self.accent.name()),
            Flavor::Gnome => format!("GNOME or Ubuntu, {mode}, {} accent", self.accent.name()),
            Flavor::Other => format!("no desktop colors to follow here ({mode}); Warden's are used"),
        }
    }
}

impl Accent {
    pub fn name(self) -> &'static str {
        match self {
            Accent::Blue => "blue",
            Accent::Teal => "teal",
            Accent::Green => "green",
            Accent::Yellow => "yellow",
            Accent::Orange => "orange",
            Accent::Red => "red",
            Accent::Pink => "pink",
            Accent::Purple => "purple",
            Accent::Slate => "slate",
            Accent::Magenta => "magenta",
            Accent::Bark => "bark",
            Accent::Sage => "sage",
            Accent::Olive => "olive",
            Accent::Viridian => "viridian",
            Accent::PrussianGreen => "prussian green",
        }
    }
}

/// Which colors the window has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Colors {
    /// Warden's own palette: warm paper, forest green, brick red and amber.
    #[default]
    Warden,
    /// The desktop's: its surfaces, text, status colors and accent, on macOS and
    /// GNOME/Ubuntu. Elsewhere, or when the desktop does not say, Warden's.
    System,
}

/// Light or dark.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// As the desktop is, live (dark when it cannot be told).
    #[default]
    Auto,
    Light,
    Dark,
}

/// What the window's colors follow: the Settings choice, or `--theme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Source {
    #[serde(default)]
    pub colors: Colors,
    #[serde(default)]
    pub mode: Mode,
}

impl Source {
    /// `system`: the desktop's colors and mode; `warden`: Warden's palette, the
    /// desktop's mode; `light`, `dark`: Warden's palette, that mode.
    pub fn parse(s: &str) -> Option<Source> {
        let (colors, mode) = match s.trim().to_ascii_lowercase().as_str() {
            "system" | "native" => (Colors::System, Mode::Auto),
            "warden" | "auto" => (Colors::Warden, Mode::Auto),
            "light" => (Colors::Warden, Mode::Light),
            "dark" => (Colors::Warden, Mode::Dark),
            _ => return None,
        };
        Some(Source { colors, mode })
    }

    /// `WARDEN_GUI_THEME`, when it is set to something that makes sense.
    pub fn from_env() -> Option<Source> {
        std::env::var("WARDEN_GUI_THEME").ok().and_then(|v| Source::parse(&v))
    }

    /// Does the window need to ask the desktop?
    pub fn follows_system(self) -> bool {
        self.colors == Colors::System || self.mode == Mode::Auto
    }
}

/// The theme for a source, given what the desktop says.
pub fn theme(source: Source, system: &System) -> Theme {
    let dark = match source.mode {
        Mode::Light => false,
        Mode::Dark => true,
        Mode::Auto => system.dark.unwrap_or(true),
    };
    match (source.colors, system.flavor) {
        (Colors::System, Flavor::Mac | Flavor::Gnome) => native(&System { dark: Some(dark), ..*system }),
        _ => look::theme(!dark),
    }
}

/// The desktop's own palette: its surfaces, its text, its status colors and
/// its accent.
pub fn native(system: &System) -> Theme {
    let dark = system.dark.unwrap_or(true);
    let (background, text, success, warning, danger) = match (system.flavor, dark) {
        // NSColor: windowBackground, label, and the system colors. They are the vivid
        // ones: the window darkens one only as far as it needs to be read on the page.
        (Flavor::Mac, false) => {
            (color!(0xececec), color!(0x1d1d1f), color!(0x34c759), color!(0xff9500), color!(0xff3b30))
        }
        (Flavor::Mac, true) => {
            (color!(0x1e1e1e), color!(0xe8e8e8), color!(0x32d74b), color!(0xff9f0a), color!(0xff453a))
        }
        // libadwaita: window_bg_color, window_fg_color, and its palette's green, yellow and red.
        (_, false) => (color!(0xfafafa), color!(0x323232), color!(0x2ec27e), color!(0xe5a50a), color!(0xe01b24)),
        (_, true) => (color!(0x242424), color!(0xeeeeee), color!(0x8ff0a4), color!(0xf8e45c), color!(0xff7b63)),
    };
    let primary = soften(system.accent.fill(system.flavor, dark), background, dark);
    let name = format!("{:?} {} {:?}", system.flavor, if dark { "dark" } else { "light" }, system.accent);
    look::custom(name, Palette { background, text, primary, success, warning, danger })
}

/// The desktop's accent, mixed with the window's own surface color. Every button, bar
/// and tab fill is the accent, and the accents are the loudest colors there are (a
/// purple or a pink fills the screen); mixed with the surface they sit on they belong
/// to the window instead of shouting from it, and stay the hue the person chose. A
/// light surface takes a little less: it is pale already.
const SOFTEN_DARK: f32 = 0.20;
const SOFTEN_LIGHT: f32 = 0.14;

pub fn soften(accent: Color, surface: Color, dark: bool) -> Color {
    look::mix(accent, surface, if dark { SOFTEN_DARK } else { SOFTEN_LIGHT })
}

impl Accent {
    /// The accent as a fill, with `on_accent` written on it.
    pub fn fill(self, flavor: Flavor, dark: bool) -> Color {
        use Accent::*;
        if flavor == Flavor::Mac {
            // NSColor's systemBlue and the rest (light, dark).
            let (l, d) = match self {
                Blue => (color!(0x007aff), color!(0x0a84ff)),
                Purple => (color!(0xaf52de), color!(0xbf5af2)),
                Pink => (color!(0xff2d55), color!(0xff375f)),
                Red => (color!(0xff3b30), color!(0xff453a)),
                Orange => (color!(0xff9500), color!(0xff9f0a)),
                Yellow => (color!(0xffcc00), color!(0xffd60a)),
                Green => (color!(0x28cd41), color!(0x32d74b)),
                Slate => (color!(0x8e8e93), color!(0x98989d)),
                Teal => (color!(0x30b0c7), color!(0x40c8e0)),
                _ => return Blue.fill(flavor, dark),
            };
            return if dark { d } else { l };
        }
        match self {
            // libadwaita's accent colors.
            Blue => color!(0x3584e4),
            Teal => color!(0x2190a4),
            Green => color!(0x3a944a),
            Yellow => color!(0xc88800),
            Orange => color!(0xed5b00),
            Red => color!(0xe62d42),
            Pink => color!(0xd56199),
            Purple => color!(0x9141ac),
            Slate => color!(0x6f8396),
            // Ubuntu's Yaru.
            Magenta => color!(0xb34cb3),
            Bark => color!(0x787859),
            Sage => color!(0x657b69),
            Olive => color!(0x4b8501),
            Viridian => color!(0x03875b),
            PrussianGreen => color!(0x308280),
        }
    }
}

// ------------------------------------------------------------- reading

/// Runs a program and gives what it printed, when it succeeded.
pub type Run<'a> = &'a dyn Fn(&str, &[&str]) -> Option<String>;

fn run(program: &str, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// What the desktop says now. It runs a few small programs (about 20 ms): call
/// it off the UI thread, except once at the start.
pub fn detect() -> System {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    detect_with(std::env::consts::OS, &desktop, &run)
}

pub fn detect_with(os: &str, desktop: &str, run: Run<'_>) -> System {
    if os == "macos" {
        // Light is the absence of the key, so `defaults` failing is an answer.
        let dark =
            run("defaults", &["read", "-g", "AppleInterfaceStyle"]).is_some_and(|s| s.eq_ignore_ascii_case("dark"));
        let accent =
            run("defaults", &["read", "-g", "AppleAccentColor"]).and_then(|s| mac_accent(&s)).unwrap_or(Accent::Blue);
        return System { flavor: Flavor::Mac, dark: Some(dark), accent };
    }
    if os != "linux" {
        return System::UNKNOWN;
    }
    let get =
        |key: &str| run("gsettings", &["get", "org.gnome.desktop.interface", key]).map(|s| unquote(&s).to_string());
    let (scheme, theme, accent) = (get("color-scheme"), get("gtk-theme"), get("accent-color"));
    // GNOME and its derivatives (Ubuntu, Pop, Budgie, Unity, Cinnamon...) answer
    // to gsettings; a desktop that names itself otherwise (KDE, Xfce, ...) keeps
    // its own look, with Warden's colors.
    let gnomey = desktop.split(':').any(|d| {
        ["gnome", "ubuntu", "unity", "budgie", "pop", "x-cinnamon", "cinnamon", "pantheon", "zorin", "gnome-classic"]
            .contains(&d.to_ascii_lowercase().as_str())
    });
    // Only a desktop that names itself GNOME's, or one that names nothing but answers
    // with a color-scheme (a session without XDG_CURRENT_DESKTOP), is read.
    if !(gnomey || (desktop.is_empty() && scheme.is_some())) {
        return System::UNKNOWN;
    }
    let dark = match scheme.as_deref() {
        Some("prefer-dark") => Some(true),
        Some("prefer-light") => Some(false),
        // "default" is light on GNOME; older themes say dark in their name.
        _ => theme.as_deref().map(is_dark_theme_name).or(Some(false)),
    };
    let accent = accent
        .as_deref()
        .and_then(gnome_accent)
        .or_else(|| theme.as_deref().and_then(yaru_accent))
        .unwrap_or(Accent::Blue);
    System { flavor: Flavor::Gnome, dark, accent }
}

/// `'blue'` as gsettings prints it.
fn unquote(s: &str) -> &str {
    s.trim().trim_matches('\'').trim_matches('"')
}

fn is_dark_theme_name(name: &str) -> bool {
    name.to_ascii_lowercase().contains("dark")
}

/// `AppleAccentColor`.
pub fn mac_accent(s: &str) -> Option<Accent> {
    Some(match s.trim().parse::<i32>().ok()? {
        -1 => Accent::Slate,
        0 => Accent::Red,
        1 => Accent::Orange,
        2 => Accent::Yellow,
        3 => Accent::Green,
        4 => Accent::Blue,
        5 => Accent::Purple,
        6 => Accent::Pink,
        _ => return None,
    })
}

/// GNOME 47's `accent-color`.
pub fn gnome_accent(s: &str) -> Option<Accent> {
    Some(match s {
        "blue" => Accent::Blue,
        "teal" => Accent::Teal,
        "green" => Accent::Green,
        "yellow" => Accent::Yellow,
        "orange" => Accent::Orange,
        "red" => Accent::Red,
        "pink" => Accent::Pink,
        "purple" => Accent::Purple,
        "slate" => Accent::Slate,
        _ => return None,
    })
}

/// The accent in the name of an Ubuntu theme: `Yaru` is orange, `Yaru-purple-dark` purple.
pub fn yaru_accent(theme: &str) -> Option<Accent> {
    let lower = theme.to_ascii_lowercase();
    let rest = lower.strip_prefix("yaru")?;
    let rest = rest.trim_start_matches('-');
    let color = rest.strip_suffix("dark").unwrap_or(rest).trim_end_matches('-');
    Some(match color {
        "" => Accent::Orange,
        "bark" => Accent::Bark,
        "sage" => Accent::Sage,
        "olive" => Accent::Olive,
        "viridian" => Accent::Viridian,
        "prussiangreen" => Accent::PrussianGreen,
        "blue" => Accent::Blue,
        "purple" => Accent::Purple,
        "magenta" => Accent::Magenta,
        "red" => Accent::Red,
        _ => return None,
    })
}

// ------------------------------------------------------------- watching

/// A stream that yields the desktop's look whenever GNOME says it changed
/// (`gsettings monitor`). Elsewhere it never yields: the window asks again when
/// it is focused.
pub fn watch() -> impl iced::futures::Stream<Item = System> + Send + 'static {
    use iced::futures::SinkExt;
    use tokio::io::{AsyncBufReadExt, BufReader};
    iced::stream::channel(4, async move |mut out: iced::futures::channel::mpsc::Sender<System>| {
        let child = tokio::process::Command::new("gsettings")
            .args(["monitor", "org.gnome.desktop.interface"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = child else { return std::future::pending().await };
        let Some(stdout) = child.stdout.take() else { return std::future::pending().await };
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let relevant = ["color-scheme", "accent-color", "gtk-theme"].iter().any(|k| line.starts_with(k));
            if !relevant {
                continue;
            }
            // A change comes as several lines at once: wait for the rest.
            while let Ok(Ok(Some(_))) =
                tokio::time::timeout(std::time::Duration::from_millis(120), lines.next_line()).await
            {}
            if let Ok(system) = tokio::task::spawn_blocking(detect).await
                && out.send(system).await.is_err()
            {
                return;
            }
        }
        // gsettings went away: nothing more to say (the window still asks on focus).
        std::future::pending::<()>().await;
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn fake(answers: &[(&str, &str)]) -> impl Fn(&str, &[&str]) -> Option<String> {
        let m: HashMap<String, String> = answers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |program, args| m.get(&format!("{program} {}", args.join(" "))).cloned()
    }

    const STYLE: &str = "defaults read -g AppleInterfaceStyle";
    const ACCENT: &str = "defaults read -g AppleAccentColor";
    const SCHEME: &str = "gsettings get org.gnome.desktop.interface color-scheme";
    const GTK: &str = "gsettings get org.gnome.desktop.interface gtk-theme";
    const GACCENT: &str = "gsettings get org.gnome.desktop.interface accent-color";

    #[test]
    fn macos_dark_is_a_key_and_light_is_its_absence() {
        let dark = detect_with("macos", "", &fake(&[(STYLE, "Dark"), (ACCENT, "5")]));
        assert_eq!(dark, System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple });
        // Light: `defaults` fails; no accent key is multicolor, drawn blue.
        let light = detect_with("macos", "", &fake(&[]));
        assert_eq!(light, System { flavor: Flavor::Mac, dark: Some(false), accent: Accent::Blue });
        assert_eq!(mac_accent("-1"), Some(Accent::Slate));
        assert_eq!(mac_accent("0"), Some(Accent::Red));
        assert_eq!(mac_accent("6"), Some(Accent::Pink));
        assert_eq!(mac_accent("x"), None);
    }

    #[test]
    fn gnome_reads_scheme_and_accent() {
        let r = fake(&[(SCHEME, "'prefer-dark'"), (GACCENT, "'teal'"), (GTK, "'Adwaita'")]);
        assert_eq!(
            detect_with("linux", "GNOME", &r),
            System { flavor: Flavor::Gnome, dark: Some(true), accent: Accent::Teal }
        );
        let r = fake(&[(SCHEME, "'prefer-light'"), (GACCENT, "'red'")]);
        assert_eq!(detect_with("linux", "ubuntu:GNOME", &r).dark, Some(false));
        // "default" is light, unless the theme says dark (an older Ubuntu).
        let r = fake(&[(SCHEME, "'default'"), (GTK, "'Yaru-purple-dark'")]);
        let s = detect_with("linux", "ubuntu:GNOME", &r);
        assert_eq!((s.dark, s.accent), (Some(true), Accent::Purple));
        let r = fake(&[(SCHEME, "'default'"), (GTK, "'Yaru'")]);
        let s = detect_with("linux", "ubuntu:GNOME", &r);
        assert_eq!((s.dark, s.accent), (Some(false), Accent::Orange));
    }

    #[test]
    fn yaru_names_carry_the_accent() {
        assert_eq!(yaru_accent("Yaru"), Some(Accent::Orange));
        assert_eq!(yaru_accent("Yaru-dark"), Some(Accent::Orange));
        assert_eq!(yaru_accent("Yaru-sage"), Some(Accent::Sage));
        assert_eq!(yaru_accent("Yaru-prussiangreen-dark"), Some(Accent::PrussianGreen));
        assert_eq!(yaru_accent("Yaru-magenta"), Some(Accent::Magenta));
        assert_eq!(yaru_accent("Adwaita"), None);
        assert_eq!(yaru_accent("Yaru-nothing"), None);
        assert_eq!(gnome_accent("slate"), Some(Accent::Slate));
        assert_eq!(gnome_accent("mauve"), None);
    }

    const SYSTEM: Source = Source { colors: Colors::System, mode: Mode::Auto };
    const WARDEN: Source = Source { colors: Colors::Warden, mode: Mode::Auto };

    #[test]
    fn other_desktops_and_systems_keep_the_warden_palette() {
        // No gsettings, a desktop that is not GNOME's.
        assert_eq!(detect_with("linux", "KDE", &fake(&[])), System::UNKNOWN);
        assert_eq!(detect_with("windows", "", &fake(&[])), System::UNKNOWN);
        assert_eq!(detect_with("linux", "", &fake(&[])), System::UNKNOWN);
        let s = System::UNKNOWN;
        assert_eq!(theme(SYSTEM, &s).to_string(), "Warden dark");
        let light = System { dark: Some(false), ..System::UNKNOWN };
        assert_eq!(theme(SYSTEM, &light).to_string(), "Warden light");
        assert_eq!(theme(WARDEN, &light).to_string(), "Warden light");
        // A fixed mode does not depend on the desktop.
        let dark_only = Source { colors: Colors::Warden, mode: Mode::Dark };
        let light_only = Source { colors: Colors::Warden, mode: Mode::Light };
        assert_eq!(theme(dark_only, &light).to_string(), "Warden dark");
        assert_eq!(theme(light_only, &s).to_string(), "Warden light");
    }

    #[test]
    fn warden_colors_are_the_default_and_the_desktops_are_a_choice() {
        assert_eq!(Source::default(), WARDEN);
        let mac = System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple };
        assert_eq!(theme(WARDEN, &mac).to_string(), "Warden dark");
        assert_eq!(theme(SYSTEM, &mac).to_string(), "Mac dark Purple");
        // The mode applies to either palette.
        let light = Source { colors: Colors::System, mode: Mode::Light };
        assert_eq!(theme(light, &mac).to_string(), "Mac light Purple");
        // A desktop that is not known has no colors to give: Warden's.
        assert_eq!(theme(SYSTEM, &System { dark: Some(false), ..System::UNKNOWN }).to_string(), "Warden light");
    }

    #[test]
    fn the_source_is_named_on_the_command_line_and_saved_as_json() {
        assert_eq!(Source::parse("system"), Some(SYSTEM));
        assert_eq!(Source::parse("native"), Some(SYSTEM));
        assert_eq!(Source::parse(" Light "), Some(Source { colors: Colors::Warden, mode: Mode::Light }));
        assert_eq!(Source::parse("dark"), Some(Source { colors: Colors::Warden, mode: Mode::Dark }));
        assert_eq!(Source::parse("warden"), Some(WARDEN));
        assert_eq!(Source::parse("blue"), None);
        assert!(SYSTEM.follows_system() && WARDEN.follows_system());
        assert!(!Source { colors: Colors::Warden, mode: Mode::Dark }.follows_system());
        assert!(Source { colors: Colors::System, mode: Mode::Dark }.follows_system(), "its accent still comes from it");
        let j = serde_json::to_string(&SYSTEM).unwrap();
        assert_eq!(j, r#"{"colors":"system","mode":"auto"}"#);
        assert_eq!(serde_json::from_str::<Source>(&j).unwrap(), SYSTEM);
        // A file from before there were settings, or with a part missing, still reads.
        assert_eq!(serde_json::from_str::<Source>("{}").unwrap(), WARDEN);
        assert_eq!(serde_json::from_str::<Source>(r#"{"mode":"light"}"#).unwrap().mode, Mode::Light);
    }

    #[test]
    fn the_accent_is_mixed_with_the_surface_not_shouted() {
        let loud = color!(0xbf5af2); // macOS purple, dark
        let soft = soften(loud, color!(0x1e1e1e), true);
        // Between the accent and the surface's dark: a calmer purple, still purple.
        assert!(soft.r < loud.r && soft.g < loud.g && soft.b < loud.b);
        assert!(soft.b > soft.g && soft.r > soft.g, "{soft:?} is still a purple");
        // On a light window it is mixed with that window's surface; its hue stays.
        let blue = color!(0x007aff);
        let pale = soften(blue, color!(0xececec), false);
        assert!(pale.r > blue.r && pale.g > blue.g);
        assert!(pale.b > pale.r && pale.b > pale.g, "{pale:?} is still a blue");
        // The theme has it: macOS dark purple is not the raw system purple.
        let t = native(&System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Purple });
        assert_eq!(t.palette().primary, soft);
    }

    #[test]
    fn every_desktop_palette_reads() {
        // Text in every role on every surface, the tones on their washes, and what is written on a
        // fill, in every combination, read at 4.5, and the lines and shapes at 3: the system's
        // colors are not chosen for this window, so the window moves them when they do not.
        let accents = [
            Accent::Blue,
            Accent::Teal,
            Accent::Green,
            Accent::Yellow,
            Accent::Orange,
            Accent::Red,
            Accent::Pink,
            Accent::Purple,
            Accent::Slate,
            Accent::Magenta,
            Accent::Bark,
            Accent::Sage,
            Accent::Olive,
            Accent::Viridian,
            Accent::PrussianGreen,
        ];
        for flavor in [Flavor::Mac, Flavor::Gnome] {
            for dark in [false, true] {
                for accent in accents {
                    let theme = native(&System { flavor, dark: Some(dark), accent });
                    let p = look::pal(&theme);
                    assert_eq!(p.dark, dark, "{flavor:?} {accent:?}");
                    let ctx = format!("{flavor:?} dark={dark} {accent:?}");
                    assert!(look::contrast(p.ink, p.paper) >= 7.0, "text on the page: {ctx}");
                    // Every text pair at 4.5 and every shape at 3 (look::audit lists them).
                    let bad = look::audit(&p);
                    assert!(bad.is_empty(), "{ctx}: {bad:#?}");
                }
            }
        }
    }

    #[test]
    fn the_warden_palette_keeps_its_colors() {
        let light = look::pal(&look::theme(true));
        assert!(!light.dark);
        // The accent and its text on it were chosen: nothing moves them.
        assert_eq!(light.accent, color!(0x1b6b45));
        assert_eq!(light.accent_text, light.accent);
        assert_eq!(light.danger, color!(0xa8362e));
        assert_eq!(light.card, Color::WHITE);
        let dark = look::pal(&look::theme(false));
        assert!(dark.dark);
        assert_eq!(dark.accent_text, color!(0x4eae78));
        assert_eq!(dark.warn, color!(0xe2b15a));
        assert_eq!(dark.danger, color!(0xe07a70));
    }

    #[test]
    fn the_theme_has_its_own_name_for_each_look() {
        let a = native(&System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Blue });
        let b = native(&System { flavor: Flavor::Mac, dark: Some(true), accent: Accent::Red });
        let c = native(&System { flavor: Flavor::Gnome, dark: Some(true), accent: Accent::Blue });
        assert_ne!(a.to_string(), b.to_string());
        assert_ne!(a.to_string(), c.to_string());
    }
}

//! How the window looks: the theme, fonts, colors by meaning (`Tone`), the
//! button and card styles, and the small widgets (icon, dot, badge, tile)
//! that `view` is built from.
//!
//! The look is warm and soft: paper-colored pages with cards on them, one
//! deep green accent, pill-shaped controls and a heavy rounded display face
//! for the headings. Colors come from `Pal`, one for each of light and dark.

use crate::icons::Icon;
use iced::font::{Family, Weight};
use iced::theme::Palette;
use iced::widget::text::Wrapping;
use iced::widget::{
    Text, button, checkbox, container, progress_bar, row, scrollable, space, text, text_editor, text_input, tooltip,
};
use iced::{Background, Border, Center, Color, Element, Font, Length, Theme, border, color};

/// Inter (text), Plus Jakarta Sans (headings), JetBrains Mono (code) and
/// Lucide (icons) are embedded (`lib.rs`).
pub const SANS: Font = Font::with_name("Inter");
pub const MEDIUM: Font = Font { weight: Weight::Medium, ..SANS };
pub const SEMIBOLD: Font = Font { weight: Weight::Semibold, ..SANS };
pub const DISPLAY: Font =
    Font { family: Family::Name("Plus Jakarta Sans"), weight: Weight::ExtraBold, ..Font::DEFAULT };
pub const DISPLAY_BOLD: Font = Font { weight: Weight::Bold, ..DISPLAY };
pub const MONO: Font = Font::with_name("JetBrains Mono");
pub const ICONS: Font = Font::with_name("lucide");

/// Cards, tiles and the menus.
pub const RADIUS: f32 = 16.0;
/// A control's corners: a pill (the radius is capped at half the height).
pub const PILL: f32 = 999.0;

// ------------------------------------------------------------------ colors

/// The colors of one appearance. Six come with the theme (`Palette`: the page,
/// the text, the accent, good, warning and danger); the rest is derived from
/// them, so the same styles serve the Warden palette and the system's.
#[derive(Debug, Clone, Copy)]
pub struct Pal {
    pub dark: bool,
    /// Text.
    pub ink: Color,
    /// Quiet text and icons.
    pub muted: Color,
    /// Hairlines and the borders of cards.
    pub line: Color,
    /// The page.
    pub paper: Color,
    /// What sits on the page (and over it: dialogs, menus).
    pub card: Color,
    /// A resting field of the page: tiles, the track of a segmented control.
    pub chip: Color,
    /// The fill of a box on the page (a card, a list row): see-through, so the
    /// page shows a little through it.
    pub box_bg: Color,
    /// The fill of a tile or an input: a little stronger than a box.
    pub tile_bg: Color,
    /// How much of its color a tone's wash takes (the rest is what is behind it).
    pub wash: f32,
    /// The accent as a fill (the main button, a checked box), and
    /// what is written on it.
    pub accent: Color,
    pub on_accent: Color,
    /// The accent as text or an icon: the fill, moved until it can be read on the page.
    pub accent_text: Color,
    /// The accent as a line or a bar (the focus ring of a field, a progress bar): the fill, moved
    /// until it stands out from the page and the dialogs at 3 (WCAG 1.4.11).
    pub ring: Color,
    /// What is well (as text, a dot, a line), and as a fill with what is written on it.
    pub good: Color,
    pub good_fill: Color,
    pub on_good: Color,
    pub warn: Color,
    pub warn_fill: Color,
    pub on_warn: Color,
    pub danger: Color,
    pub danger_fill: Color,
    pub on_danger: Color,
    /// The outline of a field or a checkbox: it reads against the page and the dialogs (WCAG 1.4.11).
    pub edge: Color,
    /// The chosen one of a segmented control.
    pub pill: Color,
    pub on_pill: Color,
}

/// The contrast text needs against what it is on (WCAG AA, 4.5), and the lines and shapes that
/// carry meaning (3).
pub const TEXT_MIN: f32 = 4.5;
pub const GRAPHIC_MIN: f32 = 3.0;

impl Pal {
    pub fn from_palette(p: Palette, dark: bool) -> Pal {
        let (paper, ink) = (p.background, p.text);
        let chip = mix(paper, ink, if dark { 0.09 } else { 0.045 });
        let veil = |a: f32| Color { a, ..Color::WHITE };
        let card = if dark { mix(paper, Color::WHITE, 0.045) } else { Color::WHITE };
        let box_bg = veil(if dark { 0.035 } else { 0.55 });
        let tile_bg = if dark { veil(0.055) } else { Color { a: 0.6, ..chip } };
        let wash = if dark { 0.13 } else { 0.10 };
        let surfaces = surfaces_of(paper, card, chip, box_bg, tile_bg);
        // Quiet text: moved toward the ink until it reads on every surface it sits on.
        let muted = toward(mix(ink, paper, if dark { 0.38 } else { 0.40 }), ink, |c| {
            surfaces.iter().all(|s| contrast(c, *s) >= TEXT_MIN)
        });
        // A color as text: moved toward the end that reads until it does, on every surface and on its own wash.
        let as_text = |c: Color| tone_text(c, paper, &surfaces, &washed_on(&surfaces), wash);
        let (accent, on_accent) = fill_and_text(p.primary);
        let (good, warn, danger) = (as_text(p.success), as_text(p.warning), as_text(p.danger));
        let (good_fill, on_good) = fill_and_text(good);
        let (warn_fill, on_warn) = fill_and_text(warn);
        let (danger_fill, on_danger) = fill_and_text(danger);
        Pal {
            dark,
            ink,
            paper,
            muted,
            line: mix(paper, ink, if dark { 0.12 } else { 0.07 }),
            card,
            chip,
            box_bg,
            tile_bg,
            wash,
            accent,
            on_accent,
            accent_text: as_text(p.primary),
            ring: toward(accent, ink, |c| contrast(c, paper) >= GRAPHIC_MIN && contrast(c, card) >= GRAPHIC_MIN),
            good,
            good_fill,
            on_good,
            warn,
            warn_fill,
            on_warn,
            danger,
            danger_fill,
            on_danger,
            edge: toward(mix(paper, ink, 0.4), ink, |c| {
                contrast(c, paper) >= GRAPHIC_MIN && contrast(c, card) >= GRAPHIC_MIN
            }),
            pill: if dark { mix(ink, paper, 0.05) } else { Color::WHITE },
            on_pill: if dark { paper } else { ink },
        }
    }

    /// Everything text sits on: the page, a card or dialog, a chip, a tile, a box and a tile in a box.
    pub fn surfaces(&self) -> [Color; 6] {
        surfaces_of(self.paper, self.card, self.chip, self.box_bg, self.tile_bg)
    }
}

fn surfaces_of(paper: Color, card: Color, chip: Color, box_bg: Color, tile_bg: Color) -> [Color; 6] {
    let boxed = over(box_bg, paper);
    [paper, card, chip, over(tile_bg, paper), boxed, over(tile_bg, boxed)]
}

/// `top` (with its alpha) over `base`.
pub fn over(top: Color, base: Color) -> Color {
    Color {
        r: base.r + (top.r - base.r) * top.a,
        g: base.g + (top.g - base.g) * top.a,
        b: base.b + (top.b - base.b) * top.a,
        a: 1.0,
    }
}

/// `c` as it is when `fine` says so, else moved toward `end` in small steps until it is (`end` when
/// nothing before it was).
fn toward(c: Color, end: Color, fine: impl Fn(Color) -> bool) -> Color {
    if fine(c) {
        return c;
    }
    (1..=40).map(|i| mix(c, end, i as f32 * 0.025)).find(|t| fine(*t)).unwrap_or(end)
}

/// The surfaces a tone's wash lies on (a banner on the page, a badge in a dialog, a card or a
/// selected row): not the resting chips and tiles, which hold figures and text but no badges.
fn washed_on(surfaces: &[Color; 6]) -> [Color; 4] {
    [surfaces[0], surfaces[1], surfaces[3], surfaces[4]]
}

/// A tone's color as text: readable on every surface, and on its own wash over the surfaces that
/// have one (a badge, a banner), moved toward white on a dark page and black on a light one.
fn tone_text(c: Color, paper: Color, surfaces: &[Color], washed: &[Color], wash: f32) -> Color {
    let end = if luminance(paper) < 0.5 { Color::WHITE } else { Color::BLACK };
    toward(c, end, |t| {
        surfaces.iter().all(|s| contrast(t, *s) >= TEXT_MIN)
            && washed.iter().all(|s| contrast(t, over(Color { a: wash, ..t }, *s)) >= TEXT_MIN)
    })
}

/// `a` moved toward `b`: 0 is `a`, 1 is `b`.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    Color { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1.0 }
}

/// The relative luminance of a color (WCAG).
pub fn luminance(c: Color) -> f32 {
    let f = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b)
}

/// The contrast ratio of two colors (WCAG): 1 to 21.
pub fn contrast(a: Color, b: Color) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

/// A fill and what is written on it, at 4.5: white when it reads on `fill`, white on a deeper shade
/// of it when it nearly does (the desktops put white on their blues, purples and reds), else near-black.
pub fn fill_and_text(fill: Color) -> (Color, Color) {
    const DARK: Color = Color { r: 0.01, g: 0.012, b: 0.01, a: 1.0 };
    if contrast(Color::WHITE, fill) >= TEXT_MIN {
        return (fill, Color::WHITE);
    }
    if contrast(Color::WHITE, fill) >= 3.5
        && let Some(deeper) =
            (1..=20).map(|i| mix(fill, Color::BLACK, i as f32 * 0.02)).find(|f| contrast(Color::WHITE, *f) >= TEXT_MIN)
    {
        return (deeper, Color::WHITE);
    }
    (fill, DARK)
}

pub fn pal(theme: &Theme) -> Pal {
    Pal::from_palette(theme.palette(), theme.extended_palette().is_dark)
}

/// The Warden palette: warm paper, one forest green, brick red and amber.
pub fn warden_palette(light: bool) -> Palette {
    if light {
        Palette {
            background: color!(0xf7f4ee),
            text: color!(0x12110f),
            primary: color!(0x1b6b45),
            success: color!(0x1b6b45),
            warning: color!(0x8a5310),
            danger: color!(0xa8362e),
        }
    } else {
        Palette {
            background: color!(0x121510),
            text: color!(0xf1f0ea),
            primary: color!(0x4eae78),
            success: color!(0x4eae78),
            warning: color!(0xe2b15a),
            danger: color!(0xe07a70),
        }
    }
}

/// The Warden theme, light or dark.
pub fn theme(light: bool) -> Theme {
    custom(if light { "Warden light" } else { "Warden dark" }, warden_palette(light))
}

/// A theme of a palette; the name tells themes apart (widgets compare it to
/// notice a change), so give each palette its own.
pub fn custom(name: impl Into<String>, palette: Palette) -> Theme {
    Theme::custom(name.into(), palette)
}

// ------------------------------------------------------------------- tones

/// A color by what it means, not by what it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Muted,
    Good,
    Warn,
    Bad,
    Accent,
}

impl Tone {
    pub fn color(self, theme: &Theme) -> Color {
        let p = pal(theme);
        match self {
            Tone::Plain => p.ink,
            Tone::Muted => p.muted,
            Tone::Good => p.good,
            Tone::Accent => p.accent_text,
            Tone::Warn => p.warn,
            Tone::Bad => p.danger,
        }
    }

    /// A pale wash of the color, for what sits behind text of the color.
    pub fn soft(self, theme: &Theme) -> Color {
        let p = pal(theme);
        match self {
            Tone::Plain | Tone::Muted => p.chip,
            Tone::Good | Tone::Accent | Tone::Warn | Tone::Bad => Color { a: p.wash, ..self.color(theme) },
        }
    }

    pub fn style(self) -> impl Fn(&Theme) -> text::Style {
        move |theme| text::Style { color: Some(self.color(theme)) }
    }
}

pub fn muted(theme: &Theme) -> text::Style {
    Tone::Muted.style()(theme)
}

pub fn good(theme: &Theme) -> text::Style {
    Tone::Good.style()(theme)
}

// ------------------------------------------------------------------ widgets

/// An icon of the embedded font.
pub fn icon<'a>(i: Icon) -> Text<'a> {
    text(i.glyph().to_string()).font(ICONS).size(15).line_height(1.0).align_x(Center).align_y(Center)
}

pub fn icon_sized<'a>(i: Icon, size: f32) -> Text<'a> {
    icon(i).size(size)
}

/// A filled circle.
pub fn dot<'a, Message: 'a>(tone: Tone, size: f32) -> Element<'a, Message> {
    container(space())
        .width(size)
        .height(size)
        .style(move |theme: &Theme| container::Style {
            background: Some(Background::Color(tone.color(theme))),
            border: border::rounded(size / 2.0),
            ..container::Style::default()
        })
        .into()
}

/// A label in a pill washed with the tone: `running`, `degraded`, `gave up`.
pub fn badge<'a, Message: 'a>(label: impl Into<String>, tone: Tone) -> Element<'a, Message> {
    container(text(label.into()).size(11).font(DISPLAY).style(tone.style()))
        .padding([3, 10])
        .style(move |theme: &Theme| tinted(tone, theme, PILL))
        .into()
}

/// A dot and a label in a pill: the state of a worker.
pub fn state_pill<'a, Message: 'a>(label: impl Into<String>, tone: Tone) -> Element<'a, Message> {
    container(
        row![dot(tone, 6.0), text(label.into()).size(11).font(DISPLAY).style(tone.style())].spacing(6).align_y(Center),
    )
    .padding([3, 10])
    .style(move |theme: &Theme| tinted(tone, theme, PILL))
    .into()
}

/// A small section label above a group: `WORKERS`, `LISTENING`.
pub fn section<'a>(label: &str) -> Text<'a> {
    text(label.to_uppercase()).size(11).font(DISPLAY).style(muted)
}

/// A section label with an icon before it: `>_ COMMAND LINE TOOL`.
pub fn section_icon<'a, Message: 'a>(i: Icon, label: &str) -> Element<'a, Message> {
    row![icon(i).size(13).style(muted), section(label)].spacing(6).align_y(Center).into()
}

/// The mark of the app: the shield on a rounded square of the accent.
pub fn brand_mark<'a, Message: 'a>(size: f32) -> Element<'a, Message> {
    container(
        icon(Icon::Shield).size(size * 0.58).style(|theme: &Theme| text::Style { color: Some(pal(theme).on_accent) }),
    )
    .width(size)
    .height(size)
    .center_x(size)
    .center_y(size)
    .style(move |theme: &Theme| container::Style {
        background: Some(Background::Color(pal(theme).accent)),
        border: border::rounded(size * 0.32),
        ..container::Style::default()
    })
    .into()
}

/// Text with a tooltip under it.
pub fn tip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<String>,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(label.into()).size(12).font(MEDIUM)).padding([5, 10]).style(|theme: &Theme| {
            let p = pal(theme);
            container::Style {
                background: Some(Background::Color(p.pill)),
                text_color: Some(p.on_pill),
                border: border::rounded(10),
                shadow: iced::Shadow {
                    color: Color { a: 0.25, ..Color::BLACK },
                    offset: iced::Vector::new(0.0, 3.0),
                    blur_radius: 10.0,
                },
                ..container::Style::default()
            }
        }),
        tooltip::Position::Bottom,
    )
    .gap(6)
    .into()
}

/// An icon, then a label: the content of a button.
pub fn labeled<'a, Message: 'a>(i: Icon, label: impl Into<String>) -> Element<'a, Message> {
    row![icon(i).size(14), text(label.into()).size(13).font(SEMIBOLD).wrapping(Wrapping::None)]
        .spacing(7)
        .align_y(Center)
        .into()
}

/// A figure with a title above and a line below, on a resting field.
pub fn tile<'a, Message: 'a>(i: Icon, title: &'a str, value: String, sub: String, tone: Tone) -> Element<'a, Message> {
    container(
        iced::widget::column![
            row![icon(i).size(13).style(muted), text(title).size(12).font(MEDIUM).style(muted)]
                .spacing(6)
                .align_y(Center),
            text(value).size(24).font(DISPLAY).style(tone.style()).wrapping(Wrapping::None),
            text(sub).size(12).style(muted).wrapping(Wrapping::None),
        ]
        .spacing(4),
    )
    .padding([14, 16])
    .width(Length::FillPortion(1))
    .clip(true)
    .style(field)
    .into()
}

// ------------------------------------------------------------------ styles

/// The page's cards and panels.
pub fn card(theme: &Theme) -> container::Style {
    let p = pal(theme);
    container::Style {
        background: Some(Background::Color(p.box_bg)),
        border: Border { radius: RADIUS.into(), width: 1.0, color: p.line },
        ..container::Style::default()
    }
}

/// A resting field of the page, with no outline: a tile.
pub fn field(theme: &Theme) -> container::Style {
    let p = pal(theme);
    container::Style {
        background: Some(Background::Color(p.tile_bg)),
        border: border::rounded(RADIUS + 2.0),
        ..container::Style::default()
    }
}

/// The background of a column beside the page (the app list).
pub fn sidebar(theme: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(pal(theme).paper)), ..container::Style::default() }
}

/// The page itself.
pub fn page(theme: &Theme) -> container::Style {
    container::Style { background: Some(Background::Color(pal(theme).paper)), ..container::Style::default() }
}

pub fn dialog(theme: &Theme) -> container::Style {
    let p = pal(theme);
    container::Style {
        background: Some(Background::Color(p.card)),
        border: Border { radius: 24.0.into(), width: 1.0, color: p.line },
        shadow: iced::Shadow {
            color: Color { a: 0.35, ..Color::BLACK },
            offset: iced::Vector::new(0.0, 12.0),
            blur_radius: 32.0,
        },
        ..container::Style::default()
    }
}

/// The floating menu of a dropdown.
pub fn menu(theme: &Theme) -> container::Style {
    let p = pal(theme);
    container::Style {
        background: Some(Background::Color(p.card)),
        border: Border { radius: RADIUS.into(), width: 1.0, color: p.line },
        shadow: iced::Shadow {
            color: Color { a: if p.dark { 0.5 } else { 0.16 }, ..Color::BLACK },
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

/// A wash of the tone's pale color, with no outline.
pub fn tinted(tone: Tone, theme: &Theme, radius: f32) -> container::Style {
    container::Style {
        background: Some(Background::Color(tone.soft(theme))),
        border: border::rounded(radius),
        ..container::Style::default()
    }
}

/// A wash with a border of the tone, for a message that must be seen.
pub fn banner(tone: Tone) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let mut s = tinted(tone, theme, 14.0);
        s.border = Border { radius: 14.0.into(), width: 1.0, color: Color { a: 0.35, ..tone.color(theme) } };
        s
    }
}

fn disabled_text(theme: &Theme) -> Color {
    Color { a: 0.4, ..pal(theme).ink }
}

fn lift(c: Color, by: f32) -> Color {
    Color { r: c.r + (1.0 - c.r) * by, g: c.g + (1.0 - c.g) * by, b: c.b + (1.0 - c.b) * by, a: c.a }
}

fn sink(c: Color, by: f32) -> Color {
    Color { r: c.r * (1.0 - by), g: c.g * (1.0 - by), b: c.b * (1.0 - by), a: c.a }
}

/// A button filled with a tone's color: green for the main action, red for a
/// harmful one, black (cream in the dark) for `Plain`.
pub fn solid(tone: Tone) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| solid_with(tone, theme, status, PILL.into())
}

fn solid_with(tone: Tone, theme: &Theme, status: button::Status, radius: border::Radius) -> button::Style {
    let p = pal(theme);
    let (base, text_color) = match tone {
        Tone::Plain | Tone::Muted => (p.ink, p.paper),
        Tone::Bad => (p.danger_fill, p.on_danger),
        Tone::Warn => (p.warn_fill, p.on_warn),
        Tone::Good => (p.good_fill, p.on_good),
        Tone::Accent => (p.accent, p.on_accent),
    };
    // Under the pointer the fill moves away from the text, so what is written stays as readable
    // as it was: light text on a deeper fill, dark text on a lighter one.
    let light_text = luminance(text_color) > 0.5;
    let bg = match status {
        button::Status::Active => base,
        button::Status::Hovered => {
            if light_text {
                sink(base, 0.14)
            } else {
                lift(base, 0.1)
            }
        }
        button::Status::Pressed => sink(base, if light_text { 0.26 } else { 0.1 }),
        button::Status::Disabled => Color { a: 0.3, ..base },
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: if status == button::Status::Disabled { Color { a: 0.7, ..text_color } } else { text_color },
        border: Border { radius, ..Border::default() },
        ..button::Style::default()
    }
}

/// The left half of a joined button.
pub fn split_left(theme: &Theme, status: button::Status) -> button::Style {
    solid_with(Tone::Accent, theme, status, border::left(PILL))
}

/// The right half: a line on its left sets it apart.
pub fn split_right(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = solid_with(Tone::Accent, theme, status, border::right(PILL));
    s.border.width = 0.0;
    s
}

/// The left half of a quiet joined button.
pub fn split_quiet_left(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = quiet(theme, status);
    s.border.radius = border::left(PILL);
    s
}

pub fn split_quiet_right(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = quiet(theme, status);
    s.border.radius = border::right(PILL);
    s
}

/// An outlined pill.
pub fn quiet(theme: &Theme, status: button::Status) -> button::Style {
    let p = pal(theme);
    let bg = match status {
        button::Status::Hovered => Some(p.chip),
        button::Status::Pressed => Some(p.line),
        _ => None,
    };
    button::Style {
        background: bg.map(Background::Color),
        text_color: if status == button::Status::Disabled { disabled_text(theme) } else { p.ink },
        border: Border {
            radius: PILL.into(),
            width: 1.25,
            color: if status == button::Status::Disabled { p.line } else { Color { a: 0.9, ..p.line } },
        },
        ..button::Style::default()
    }
}

/// An outlined pill whose color is a warning of what it does.
pub fn quiet_tone(tone: Tone) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let c = tone.color(theme);
        let (bg, line) = match status {
            button::Status::Active => (None, Color { a: 0.45, ..c }),
            button::Status::Hovered => (Some(tone.soft(theme)), Color { a: 0.7, ..c }),
            button::Status::Pressed => (Some(Color { a: 0.22, ..c }), c),
            button::Status::Disabled => (None, Color { a: 0.2, ..c }),
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: if status == button::Status::Disabled { disabled_text(theme) } else { c },
            border: Border { radius: PILL.into(), width: 1.25, color: line },
            ..button::Style::default()
        }
    }
}

/// No outline: a row or an icon that lights up under the pointer.
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let p = pal(theme);
    let bg = match status {
        button::Status::Hovered => Some(p.chip),
        button::Status::Pressed => Some(p.line),
        _ => None,
    };
    button::Style {
        background: bg.map(Background::Color),
        text_color: if status == button::Status::Disabled { disabled_text(theme) } else { p.ink },
        border: border::rounded(12),
        ..button::Style::default()
    }
}

/// A row of the app list: flat, edge to edge, with nothing around it. The app
/// that is shown has a veil over the row and a thick bar at its left edge
/// (`marker`), in the color of its status like the dot and the state label.
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = pal(theme);
        let bg = match (selected, status) {
            (true, _) => Some(p.tile_bg),
            (false, button::Status::Hovered | button::Status::Pressed) => Some(p.box_bg),
            (false, _) => None,
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: p.ink,
            border: Border::default(),
            ..button::Style::default()
        }
    }
}

/// The thick bar at the left edge of a row of the app list: on the shown app, in
/// the color of its status (the dot's), and an empty space of the same width on
/// the others, so every row's content starts at the same place.
pub fn marker<'a, Message: 'a>(on: Option<Tone>) -> Element<'a, Message> {
    container(space())
        .width(4)
        .height(Length::Fill)
        .style(move |theme: &Theme| container::Style {
            background: on.map(|t| Background::Color(t.color(theme))),
            ..container::Style::default()
        })
        .into()
}

/// A line across a list, between its rows.
pub fn hairline<'a, Message: 'a>() -> Element<'a, Message> {
    container(space())
        .width(Length::Fill)
        .height(1)
        .style(|theme: &Theme| container::Style {
            background: Some(Background::Color(pal(theme).line)),
            ..container::Style::default()
        })
        .into()
}

/// One choice of a segmented control: the chosen one is a raised pill.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = pal(theme);
        if active {
            button::Style {
                background: Some(Background::Color(p.pill)),
                text_color: p.on_pill,
                border: Border { radius: PILL.into(), width: if p.dark { 0.0 } else { 1.0 }, color: p.line },
                shadow: iced::Shadow {
                    color: Color { a: if p.dark { 0.0 } else { 0.07 }, ..Color::BLACK },
                    offset: iced::Vector::new(0.0, 1.0),
                    blur_radius: 3.0,
                },
                ..button::Style::default()
            }
        } else {
            let mut s = ghost(theme, status);
            s.background = if status == button::Status::Hovered {
                Some(Background::Color(Color { a: 0.6, ..p.line }))
            } else {
                None
            };
            s.text_color = p.muted;
            s.border = border::rounded(PILL);
            s
        }
    }
}

/// The track a segmented control's choices sit on.
pub fn track(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(pal(theme).chip)),
        border: border::rounded(PILL),
        ..container::Style::default()
    }
}

/// A small pill that copies something when pressed.
pub fn chip(theme: &Theme, status: button::Status) -> button::Style {
    let p = pal(theme);
    let bg = match status {
        button::Status::Hovered => p.line,
        button::Status::Pressed => sink(p.line, 0.1),
        _ => p.chip,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: p.ink,
        border: border::rounded(PILL),
        ..button::Style::default()
    }
}

/// A one-pixel line, drawn as a container (for the split between joined buttons).
pub fn divider<'a, Message: 'a>(tone: Tone) -> Element<'a, Message> {
    container(space())
        .width(1)
        .height(Length::Fill)
        .style(move |theme: &Theme| container::Style {
            background: Some(Background::Color(match tone {
                Tone::Accent => sink(pal(theme).accent, 0.3),
                _ => pal(theme).line,
            })),
            ..container::Style::default()
        })
        .into()
}

pub fn heading<'a>(t: impl Into<String>) -> Text<'a> {
    text(t.into()).font(DISPLAY)
}

// ------------------------------------------------------- inputs and bars

/// A line of text the user types: a pill, the accent on focus.
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = pal(theme);
    let line = match status {
        text_input::Status::Active => p.edge,
        text_input::Status::Hovered => p.muted,
        text_input::Status::Focused { .. } => p.ring,
        text_input::Status::Disabled => p.line,
    };
    text_input::Style {
        background: Background::Color(p.tile_bg),
        border: Border { radius: PILL.into(), width: 1.25, color: line },
        icon: p.muted,
        placeholder: p.muted,
        value: if matches!(status, text_input::Status::Disabled) { Color { a: 0.5, ..p.ink } } else { p.ink },
        selection: Color { a: 0.3, ..p.accent },
    }
}

/// The config editor: the same field, larger.
pub fn editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let p = pal(theme);
    let line = match status {
        text_editor::Status::Active => p.edge,
        text_editor::Status::Hovered => p.muted,
        text_editor::Status::Focused { .. } => p.ring,
        text_editor::Status::Disabled => p.line,
    };
    text_editor::Style {
        background: Background::Color(p.tile_bg),
        border: Border { radius: 14.0.into(), width: 1.25, color: line },
        placeholder: p.muted,
        value: p.ink,
        selection: Color { a: 0.3, ..p.accent },
    }
}

/// A checkbox with softly rounded corners.
pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let p = pal(theme);
    let (checked, hovered) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true),
        checkbox::Status::Disabled { is_checked } => (is_checked, false),
    };
    // Under the pointer the fill moves away from what is drawn on it (see `solid_with`).
    let accent = match (hovered, luminance(p.on_accent) > 0.5) {
        (false, _) => p.accent,
        (true, true) => sink(p.accent, 0.1),
        (true, false) => lift(p.accent, 0.1),
    };
    checkbox::Style {
        background: Background::Color(if checked { accent } else { p.card }),
        icon_color: p.on_accent,
        border: Border { radius: 6.0.into(), width: 1.5, color: if checked { accent } else { p.edge } },
        text_color: None,
    }
}

/// A progress bar with round ends.
pub fn progress(theme: &Theme) -> progress_bar::Style {
    let p = pal(theme);
    progress_bar::Style {
        background: Background::Color(p.chip),
        bar: Background::Color(p.ring),
        border: border::rounded(PILL),
    }
}

/// Scrollbars with round ends, in the page's colors.
pub fn scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let p = pal(theme);
    let mut s = scrollable::default(theme, status);
    for rail in [&mut s.vertical_rail, &mut s.horizontal_rail] {
        rail.background = None;
        rail.border.radius = PILL.into();
        rail.scroller.background = Background::Color(Color { a: 0.45, ..p.muted });
        rail.scroller.border.radius = PILL.into();
    }
    s
}

/// Every pair of colors the window draws text or a meaningful shape with, as the words of each one
/// that is below what it needs (WCAG AA: 4.5 for text, 3 for shapes). Empty when all read.
#[cfg(test)]
pub(crate) fn audit(p: &Pal) -> Vec<String> {
    let mut bad = Vec::new();
    let mut check = |what: &str, fg: Color, bg: Color, min: f32| {
        let got = contrast(fg, bg);
        if got < min {
            bad.push(format!("{what}: {got:.2} < {min}"));
        }
    };
    let names = ["page", "card", "chip", "tile", "box", "tile in a box"];
    for (name, s) in names.iter().zip(p.surfaces()) {
        for (what, c) in [
            ("ink", p.ink),
            ("muted", p.muted),
            ("placeholder", p.muted),
            ("accent text", p.accent_text),
            ("good", p.good),
            ("warn", p.warn),
            ("danger", p.danger),
        ] {
            check(&format!("{what} on the {name}"), c, s, TEXT_MIN);
        }
    }
    // The tones as text on their own wash (a badge, a banner, a hovered quiet button).
    let surfaces = p.surfaces();
    for (name, s) in ["page", "card", "tile", "box"].iter().zip(washed_on(&surfaces)) {
        for (what, c) in [("accent text", p.accent_text), ("good", p.good), ("warn", p.warn), ("danger", p.danger)] {
            check(&format!("{what} on its wash over the {name}"), c, over(Color { a: p.wash, ..c }, s), TEXT_MIN);
        }
    }
    // What is written on a fill.
    for (what, text, fill) in [
        ("the main button", p.on_accent, p.accent),
        ("a good button", p.on_good, p.good_fill),
        ("a warning button", p.on_warn, p.warn_fill),
        ("a danger button", p.on_danger, p.danger_fill),
        ("the chosen tab and a tooltip", p.on_pill, p.pill),
        ("the dark button", p.paper, p.ink),
    ] {
        check(what, text, fill, TEXT_MIN);
    }
    // Lines and shapes that mean something.
    for (name, s) in [("page", p.paper), ("card", p.card)] {
        check(&format!("the outline of a field on the {name}"), p.edge, s, GRAPHIC_MIN);
        check(&format!("a focus ring or a progress bar on the {name}"), p.ring, s, GRAPHIC_MIN);
    }
    bad
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::widget::button::Status;

    #[test]
    fn the_warden_palette_reads_in_light_and_dark() {
        for light in [true, false] {
            let p = pal(&theme(light));
            let bad = audit(&p);
            assert!(bad.is_empty(), "Warden {}: {bad:#?}", if light { "light" } else { "dark" });
        }
    }

    #[test]
    fn every_solid_button_reads_at_rest_under_the_pointer_and_pressed() {
        for light in [true, false] {
            let t = theme(light);
            for tone in [Tone::Plain, Tone::Good, Tone::Warn, Tone::Bad, Tone::Accent] {
                for status in [Status::Active, Status::Hovered, Status::Pressed] {
                    let s = solid(tone)(&t, status);
                    let Some(Background::Color(fill)) = s.background else { panic!("a fill") };
                    let got = contrast(s.text_color, fill);
                    assert!(got >= TEXT_MIN, "{tone:?} {status:?} light={light}: {got:.2}");
                }
            }
        }
    }

    #[test]
    fn a_fill_gets_white_when_it_reads_a_deeper_shade_when_it_nearly_does_else_near_black() {
        // Warden's green: white.
        let (fill, text) = fill_and_text(color!(0x1b6b45));
        assert_eq!((fill, text), (color!(0x1b6b45), Color::WHITE));
        // macOS blue: white reads at 4.0 only, so the blue is a little deeper and the text stays white.
        let blue = color!(0x007aff);
        let (fill, text) = fill_and_text(blue);
        assert_eq!(text, Color::WHITE);
        assert!(fill.b < blue.b && fill.b > fill.r, "{fill:?} is still a blue, deeper");
        assert!(contrast(text, fill) >= TEXT_MIN);
        // A light fill (yellow, the light greens): near-black, and the fill is as it was.
        let yellow = color!(0xffd60a);
        let (fill, text) = fill_and_text(yellow);
        assert_eq!(fill, yellow);
        assert!(luminance(text) < 0.01 && contrast(text, fill) >= TEXT_MIN);
    }

    #[test]
    fn a_color_is_moved_only_as_far_as_it_needs() {
        // Already readable: untouched. Not readable: moved toward the end, a little at a time.
        let paper = color!(0xf7f4ee);
        let ok = color!(0x1b6b45);
        assert_eq!(tone_text(ok, paper, &[paper], &[paper], 0.1), ok);
        let pale = color!(0x6fae8f);
        let moved = tone_text(pale, paper, &[paper], &[paper], 0.0);
        assert!(contrast(moved, paper) >= TEXT_MIN && contrast(pale, paper) < TEXT_MIN);
        assert!(luminance(moved) < luminance(pale), "darker on a light page");
        // The least that reads: one step (2.5% of the way to black) less would not.
        let steps = (1..=40).find(|i| contrast(mix(pale, Color::BLACK, *i as f32 * 0.025), paper) >= TEXT_MIN).unwrap();
        assert_eq!(moved, mix(pale, Color::BLACK, steps as f32 * 0.025));
        assert!(contrast(mix(pale, Color::BLACK, (steps - 1) as f32 * 0.025), paper) < TEXT_MIN);
        // Over its own wash (a badge) it has to read there as well, so it goes a little further.
        let on_wash = tone_text(pale, paper, &[paper], &[paper], 0.1);
        assert!(contrast(on_wash, over(Color { a: 0.1, ..on_wash }, paper)) >= TEXT_MIN);
        assert!(luminance(on_wash) < luminance(moved));
        assert!(steps < 20, "and nowhere near black");
    }
}

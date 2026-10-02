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

/// The colors of one appearance.
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
    /// What sits on the page.
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
    pub accent: Color,
    pub on_accent: Color,
    pub warn: Color,
    pub danger: Color,
    pub on_danger: Color,
    /// The chosen one of a segmented control.
    pub pill: Color,
    pub on_pill: Color,
}

static LIGHT: Pal = Pal {
    dark: false,
    ink: color!(0x12110f),
    muted: color!(0x6d675e),
    line: color!(0xece7df),
    paper: color!(0xf7f4ee),
    card: color!(0xffffff),
    chip: color!(0xefeae1),
    box_bg: color!(0xffffff, 0.55),
    tile_bg: color!(0xefeae1, 0.6),
    wash: 0.10,
    accent: color!(0x1b6b45),
    on_accent: color!(0xffffff),
    warn: color!(0x8a5310),
    danger: color!(0xa8362e),
    on_danger: color!(0xffffff),
    pill: color!(0xffffff),
    on_pill: color!(0x12110f),
};

static DARK: Pal = Pal {
    dark: true,
    ink: color!(0xf1f0ea),
    muted: color!(0x9ca39a),
    line: color!(0x2a2f2b),
    paper: color!(0x121510),
    card: color!(0x191d1b),
    chip: color!(0x242924),
    box_bg: color!(0xffffff, 0.035),
    tile_bg: color!(0xffffff, 0.055),
    wash: 0.13,
    accent: color!(0x4eae78),
    on_accent: color!(0x07140c),
    warn: color!(0xe2b15a),
    danger: color!(0xe07a70),
    on_danger: color!(0x1f0b09),
    pill: color!(0xe8e2d6),
    on_pill: color!(0x12110f),
};

pub fn pal(theme: &Theme) -> &'static Pal {
    if theme.extended_palette().is_dark { &DARK } else { &LIGHT }
}

pub fn theme(light: bool) -> Theme {
    let (name, p) = if light { ("Warden light", &LIGHT) } else { ("Warden dark", &DARK) };
    Theme::custom(
        name,
        Palette {
            background: p.paper,
            text: p.ink,
            primary: p.accent,
            success: p.accent,
            warning: p.warn,
            danger: p.danger,
        },
    )
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
            Tone::Good | Tone::Accent => p.accent,
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
    row![icon(i).size(14), text(label.into()).size(13).font(SEMIBOLD)].spacing(7).align_y(Center).into()
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
        Tone::Bad => (p.danger, p.on_danger),
        Tone::Warn => (p.warn, if p.dark { p.on_accent } else { Color::WHITE }),
        Tone::Good | Tone::Accent => (p.accent, p.on_accent),
    };
    let bg = match status {
        button::Status::Active => base,
        button::Status::Hovered => {
            if p.dark {
                lift(base, 0.1)
            } else {
                sink(base, 0.14)
            }
        }
        button::Status::Pressed => {
            if p.dark {
                sink(base, 0.1)
            } else {
                sink(base, 0.26)
            }
        }
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
        text_input::Status::Active => p.line,
        text_input::Status::Hovered => Color { a: 0.9, ..p.muted },
        text_input::Status::Focused { .. } => p.accent,
        text_input::Status::Disabled => p.line,
    };
    text_input::Style {
        background: Background::Color(p.tile_bg),
        border: Border { radius: PILL.into(), width: 1.25, color: line },
        icon: p.muted,
        placeholder: Color { a: 0.8, ..p.muted },
        value: if matches!(status, text_input::Status::Disabled) { Color { a: 0.5, ..p.ink } } else { p.ink },
        selection: Color { a: 0.3, ..p.accent },
    }
}

/// The config editor: the same field, larger.
pub fn editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let p = pal(theme);
    let line = match status {
        text_editor::Status::Active => p.line,
        text_editor::Status::Hovered => Color { a: 0.9, ..p.muted },
        text_editor::Status::Focused { .. } => p.accent,
        text_editor::Status::Disabled => p.line,
    };
    text_editor::Style {
        background: Background::Color(p.tile_bg),
        border: Border { radius: 14.0.into(), width: 1.25, color: line },
        placeholder: Color { a: 0.8, ..p.muted },
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
    let accent = if hovered { lift(p.accent, 0.1) } else { p.accent };
    checkbox::Style {
        background: Background::Color(if checked { accent } else { p.card }),
        icon_color: p.on_accent,
        border: Border {
            radius: 6.0.into(),
            width: 1.5,
            color: if checked { accent } else { Color { a: 0.7, ..p.muted } },
        },
        text_color: None,
    }
}

/// A progress bar with round ends.
pub fn progress(theme: &Theme) -> progress_bar::Style {
    let p = pal(theme);
    progress_bar::Style {
        background: Background::Color(p.chip),
        bar: Background::Color(p.accent),
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

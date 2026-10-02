//! How the window looks: the theme, fonts, colors by meaning (`Tone`), the
//! button and card styles, and the small widgets (icon, dot, badge, tile)
//! that `view` is built from.

use crate::icons::Icon;
use iced::font::Weight;
use iced::theme::Palette;
use iced::widget::text::Wrapping;
use iced::widget::{
    Text, button, checkbox, container, progress_bar, row, scrollable, space, text, text_editor, text_input, tooltip,
};
use iced::{Background, Border, Center, Color, Element, Font, Length, Theme, border, color};

/// Inter, JetBrains Mono and Lucide are embedded (`lib.rs`).
pub const SANS: Font = Font::with_name("Inter");
pub const MEDIUM: Font = Font { weight: Weight::Medium, ..SANS };
pub const SEMIBOLD: Font = Font { weight: Weight::Semibold, ..SANS };
pub const MONO: Font = Font::with_name("JetBrains Mono");
pub const ICONS: Font = Font::with_name("lucide");

/// The corner radius of controls and cards.
pub const RADIUS: f32 = 7.0;

pub fn theme(light: bool) -> Theme {
    if light {
        Theme::custom(
            "Warden light",
            Palette {
                background: color!(0xf4f5f8),
                text: color!(0x1c212b),
                primary: color!(0x4a58d8),
                success: color!(0x12805a),
                warning: color!(0xb06a00),
                danger: color!(0xc63a47),
            },
        )
    } else {
        Theme::custom(
            "Warden dark",
            Palette {
                background: color!(0x16181d),
                text: color!(0xe7e9ef),
                primary: color!(0x6572f5),
                success: color!(0x3ecf8e),
                warning: color!(0xf2b94b),
                danger: color!(0xf0626e),
            },
        )
    }
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
        let p = theme.extended_palette();
        match self {
            Tone::Plain => p.background.base.text,
            Tone::Muted => Color { a: 0.6, ..p.background.base.text },
            Tone::Good => p.success.base.color,
            Tone::Warn => p.warning.base.color,
            Tone::Bad => p.danger.base.color,
            Tone::Accent => p.primary.strong.color,
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

/// A label in a tinted pill: `running`, `degraded`, `gave up`.
pub fn badge<'a, Message: 'a>(label: impl Into<String>, tone: Tone) -> Element<'a, Message> {
    container(text(label.into()).size(12).font(MEDIUM).style(tone.style()))
        .padding([2, 9])
        .style(move |theme: &Theme| tinted(tone, theme, 999.0))
        .into()
}

/// Text with a tooltip under it.
pub fn tip<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<String>,
) -> Element<'a, Message> {
    tooltip(
        content,
        container(text(label.into()).size(12)).padding([4, 8]).style(|theme: &Theme| {
            let p = theme.extended_palette();
            container::Style {
                background: Some(Background::Color(p.background.strong.color)),
                text_color: Some(p.background.strong.text),
                border: border::rounded(6),
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
    row![icon(i).size(14), text(label.into()).size(13).font(MEDIUM)].spacing(7).align_y(Center).into()
}

/// A figure with a title above and a line below, in a card.
pub fn tile<'a, Message: 'a>(i: Icon, title: &'a str, value: String, sub: String, tone: Tone) -> Element<'a, Message> {
    container(
        iced::widget::column![
            row![icon(i).size(13).style(muted), text(title).size(12).style(muted)].spacing(6).align_y(Center),
            text(value).size(21).font(SEMIBOLD).style(tone.style()).wrapping(Wrapping::None),
            text(sub).size(12).style(muted).wrapping(Wrapping::None),
        ]
        .spacing(3),
    )
    .padding([10, 14])
    .width(Length::FillPortion(1))
    .clip(true)
    .style(card)
    .into()
}

// ------------------------------------------------------------------ styles

/// The page's cards and panels.
pub fn card(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(p.background.weakest.color)),
        border: Border { radius: RADIUS.into(), width: 1.0, color: p.background.weak.color },
        ..container::Style::default()
    }
}

/// The background of a column beside the page (the app list).
pub fn sidebar(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style { background: Some(Background::Color(p.background.weakest.color)), ..container::Style::default() }
}

pub fn dialog(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(p.background.base.color)),
        border: Border { radius: 12.0.into(), width: 1.0, color: p.background.strong.color },
        shadow: iced::Shadow {
            color: Color { a: 0.4, ..Color::BLACK },
            offset: iced::Vector::new(0.0, 8.0),
            blur_radius: 24.0,
        },
        ..container::Style::default()
    }
}

/// The floating menu of a dropdown.
pub fn menu(theme: &Theme) -> container::Style {
    let p = theme.extended_palette();
    container::Style {
        background: Some(Background::Color(p.background.base.color)),
        border: Border { radius: 9.0.into(), width: 1.0, color: p.background.strong.color },
        shadow: iced::Shadow {
            color: Color { a: 0.35, ..Color::BLACK },
            offset: iced::Vector::new(0.0, 6.0),
            blur_radius: 18.0,
        },
        ..container::Style::default()
    }
}

/// A tinted wash with a border of the same color.
pub fn tinted(tone: Tone, theme: &Theme, radius: f32) -> container::Style {
    let c = tone.color(theme);
    container::Style {
        background: Some(Background::Color(Color { a: 0.14, ..c })),
        border: Border { radius: radius.into(), width: 1.0, color: Color { a: 0.4, ..c } },
        ..container::Style::default()
    }
}

pub fn banner(tone: Tone) -> impl Fn(&Theme) -> container::Style {
    move |theme| tinted(tone, theme, RADIUS)
}

fn disabled_text(theme: &Theme) -> Color {
    Color { a: 0.38, ..theme.extended_palette().background.base.text }
}

fn lift(c: Color, by: f32) -> Color {
    Color { r: c.r + (1.0 - c.r) * by, g: c.g + (1.0 - c.g) * by, b: c.b + (1.0 - c.b) * by, a: c.a }
}

fn sink(c: Color, by: f32) -> Color {
    Color { r: c.r * (1.0 - by), g: c.g * (1.0 - by), b: c.b * (1.0 - by), a: c.a }
}

/// A button filled with a tone's color.
pub fn solid(tone: Tone) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| solid_with(tone, theme, status, RADIUS.into())
}

fn solid_with(tone: Tone, theme: &Theme, status: button::Status, radius: border::Radius) -> button::Style {
    let p = theme.extended_palette();
    let base = match tone {
        Tone::Good => p.success.base.color,
        Tone::Bad => p.danger.base.color,
        Tone::Warn => p.warning.base.color,
        _ => p.primary.base.color,
    };
    let text_color = match tone {
        Tone::Good => p.success.base.text,
        Tone::Bad => p.danger.base.text,
        Tone::Warn => p.warning.base.text,
        _ => p.primary.base.text,
    };
    let bg = match status {
        button::Status::Active => base,
        button::Status::Hovered => lift(base, 0.12),
        button::Status::Pressed => sink(base, 0.12),
        button::Status::Disabled => Color { a: 0.35, ..base },
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
    solid_with(Tone::Accent, theme, status, border::left(RADIUS))
}

/// The right half: a line on its left sets it apart.
pub fn split_right(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = solid_with(Tone::Accent, theme, status, border::right(RADIUS));
    s.border.width = 0.0;
    s
}

/// The left half of a quiet joined button.
pub fn split_quiet_left(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = quiet(theme, status);
    s.border.radius = border::left(RADIUS);
    s
}

pub fn split_quiet_right(theme: &Theme, status: button::Status) -> button::Style {
    let mut s = quiet(theme, status);
    s.border.radius = border::right(RADIUS);
    s
}

/// An outlined button.
pub fn quiet(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let (bg, line) = match status {
        button::Status::Active => (None, p.background.strong.color),
        button::Status::Hovered => (Some(p.background.weak.color), p.background.strong.color),
        button::Status::Pressed => (Some(p.background.strong.color), p.background.strong.color),
        button::Status::Disabled => (None, p.background.weak.color),
    };
    button::Style {
        background: bg.map(Background::Color),
        text_color: if status == button::Status::Disabled { disabled_text(theme) } else { p.background.base.text },
        border: Border { radius: RADIUS.into(), width: 1.0, color: line },
        ..button::Style::default()
    }
}

/// An outlined button whose color is a warning of what it does.
pub fn quiet_tone(tone: Tone) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let c = tone.color(theme);
        let (bg, line) = match status {
            button::Status::Active => (None, Color { a: 0.55, ..c }),
            button::Status::Hovered => (Some(Color { a: 0.14, ..c }), c),
            button::Status::Pressed => (Some(Color { a: 0.24, ..c }), c),
            button::Status::Disabled => (None, Color { a: 0.2, ..c }),
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: if status == button::Status::Disabled { disabled_text(theme) } else { c },
            border: Border { radius: RADIUS.into(), width: 1.0, color: line },
            ..button::Style::default()
        }
    }
}

/// No outline: a row or an icon that lights up under the pointer.
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => Some(p.background.weak.color),
        button::Status::Pressed => Some(p.background.strong.color),
        _ => None,
    };
    button::Style {
        background: bg.map(Background::Color),
        text_color: if status == button::Status::Disabled { disabled_text(theme) } else { p.background.base.text },
        border: border::rounded(RADIUS),
        ..button::Style::default()
    }
}

/// A row of the app list.
pub fn list_row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = theme.extended_palette();
        let bg = match (selected, status) {
            (true, _) => Some(Color { a: 0.16, ..p.primary.base.color }),
            (false, button::Status::Hovered) => Some(p.background.weak.color),
            (false, button::Status::Pressed) => Some(p.background.strong.color),
            _ => None,
        };
        button::Style {
            background: bg.map(Background::Color),
            text_color: p.background.base.text,
            border: Border {
                radius: RADIUS.into(),
                width: if selected { 1.0 } else { 0.0 },
                color: Color { a: 0.45, ..p.primary.base.color },
            },
            ..button::Style::default()
        }
    }
}

/// A tab: lit when it is the one shown.
pub fn tab(active: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let p = theme.extended_palette();
        if active {
            button::Style {
                background: Some(Background::Color(Color { a: 0.16, ..p.primary.base.color })),
                text_color: p.primary.strong.color,
                border: border::rounded(RADIUS),
                ..button::Style::default()
            }
        } else {
            let mut s = ghost(theme, status);
            s.text_color = Color { a: 0.7, ..p.background.base.text };
            s
        }
    }
}

/// A small pill that copies something when pressed.
pub fn chip(theme: &Theme, status: button::Status) -> button::Style {
    let p = theme.extended_palette();
    let bg = match status {
        button::Status::Hovered => p.background.strong.color,
        button::Status::Pressed => p.background.strongest.color,
        _ => p.background.weak.color,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: p.background.base.text,
        border: border::rounded(999),
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
                Tone::Accent => sink(theme.extended_palette().primary.base.color, 0.3),
                _ => theme.extended_palette().background.strong.color,
            })),
            ..container::Style::default()
        })
        .into()
}

pub fn heading<'a>(t: impl Into<String>) -> Text<'a> {
    text(t.into()).font(SEMIBOLD)
}

// ------------------------------------------------------- inputs and bars

/// A line of text the user types: rounded like every other control, the accent on focus.
pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let p = theme.extended_palette();
    let line = match status {
        text_input::Status::Active => p.background.strong.color,
        text_input::Status::Hovered => p.background.strongest.color,
        text_input::Status::Focused { .. } => p.primary.strong.color,
        text_input::Status::Disabled => p.background.weak.color,
    };
    let text_color = p.background.base.text;
    text_input::Style {
        background: Background::Color(p.background.base.color),
        border: Border { radius: RADIUS.into(), width: 1.0, color: line },
        icon: Color { a: 0.6, ..text_color },
        placeholder: Color { a: 0.45, ..text_color },
        value: if matches!(status, text_input::Status::Disabled) { Color { a: 0.5, ..text_color } } else { text_color },
        selection: Color { a: 0.35, ..p.primary.strong.color },
    }
}

/// The config editor: the same rounded field, larger.
pub fn editor(theme: &Theme, status: text_editor::Status) -> text_editor::Style {
    let p = theme.extended_palette();
    let line = match status {
        text_editor::Status::Active => p.background.strong.color,
        text_editor::Status::Hovered => p.background.strongest.color,
        text_editor::Status::Focused { .. } => p.primary.strong.color,
        text_editor::Status::Disabled => p.background.weak.color,
    };
    let text_color = p.background.base.text;
    text_editor::Style {
        background: Background::Color(p.background.base.color),
        border: Border { radius: RADIUS.into(), width: 1.0, color: line },
        placeholder: Color { a: 0.45, ..text_color },
        value: text_color,
        selection: Color { a: 0.35, ..p.primary.strong.color },
    }
}

/// A checkbox with softly rounded corners.
pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let p = theme.extended_palette();
    let (checked, hovered) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true),
        checkbox::Status::Disabled { is_checked } => (is_checked, false),
    };
    let accent = if hovered { p.primary.base.color } else { p.primary.strong.color };
    checkbox::Style {
        background: Background::Color(if checked { accent } else { p.background.base.color }),
        icon_color: p.primary.strong.text,
        border: Border {
            radius: 4.0.into(),
            width: 1.0,
            color: if checked { accent } else { p.background.strongest.color },
        },
        text_color: None,
    }
}

/// A progress bar with round ends.
pub fn progress(theme: &Theme) -> progress_bar::Style {
    let p = theme.extended_palette();
    progress_bar::Style {
        background: Background::Color(p.background.strong.color),
        bar: Background::Color(p.primary.strong.color),
        border: border::rounded(999),
    }
}

/// Scrollbars with round ends, over the default colors.
pub fn scroll(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let mut s = scrollable::default(theme, status);
    for rail in [&mut s.vertical_rail, &mut s.horizontal_rail] {
        rail.border.radius = 999.0.into();
        rail.scroller.border.radius = 999.0.into();
    }
    s
}

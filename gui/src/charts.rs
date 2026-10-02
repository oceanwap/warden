//! The History tab's charts and the header's sparklines, drawn on an iced
//! `canvas`: one series per chart (its title names it, so no legend), a 2 px
//! line over a 10% wash (bars for restarts), hairline grid, axis labels with
//! units, and a crosshair with the value under the pointer.
//!
//! Colors: blue (CPU), aqua (memory), orange (restarts), violet (workers
//! ready), stepped for the light and dark surfaces and checked for
//! color-blind separation; text is drawn in the theme's text color, never
//! the series color.

use crate::history::Series;
use iced::mouse;
use iced::widget::canvas::{self, Frame, Path, Stroke, Text};
use iced::{Color, Pixels, Point, Rectangle, Renderer, Size, Theme, alignment};
use std::cell::Cell;

/// What the numbers are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// CPU, in % (of one core for an app, of all CPUs for the host).
    Percent,
    Bytes,
    /// Restarts per point.
    Count,
}

/// The series' hue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hue {
    Blue,
    Aqua,
    Orange,
    Violet,
}

impl Hue {
    pub fn color(self, theme: &Theme) -> Color {
        let dark = theme.extended_palette().is_dark;
        // The accent green, amber, terracotta and a dusty blue: the window's own colors.
        let hex = match (self, dark) {
            (Hue::Blue, false) => 0x1b6b45,
            (Hue::Blue, true) => 0x4eae78,
            (Hue::Aqua, false) => 0xb7791f,
            (Hue::Aqua, true) => 0xe2b15a,
            (Hue::Orange, false) => 0xc2410c,
            (Hue::Orange, true) => 0xe07a5f,
            (Hue::Violet, false) => 0x3a6ea5,
            (Hue::Violet, true) => 0x7aa7d9,
        };
        Color::from_rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
    }
}

const LEFT: f32 = 58.0;
const RIGHT: f32 = 10.0;
const TOP: f32 = 8.0;
const BOTTOM: f32 = 20.0;
const LABEL: f32 = 11.0;

/// One chart: `values` on the grid `start_s` + i × `step_s`, shown over
/// `[end_s - span_s, end_s]`.
pub struct Chart<'a> {
    pub series: &'a Series,
    pub start_s: u64,
    pub step_s: u64,
    pub end_s: u64,
    pub span_s: u64,
    pub unit: Unit,
    pub hue: Hue,
    pub bars: bool,
    /// A 10% wash under the line (not for a level that sits at the top).
    pub area: bool,
    /// Data being replaced (another range loading): drawn faded.
    pub faded: bool,
}

/// What a canvas keeps between frames: its drawing, redrawn only when what
/// it shows changes (statuses come every second; a pixel moves far less
/// often), and whether the pointer was over it.
#[derive(Default)]
pub struct Drawn {
    cache: canvas::Cache,
    /// Fingerprint of what `cache` shows.
    shown: Cell<u64>,
    inside: bool,
}

impl Drawn {
    /// The cached drawing, redrawn by `draw` if `key` differs from what it shows.
    fn get(&self, renderer: &Renderer, size: Size, key: u64, draw: impl Fn(&mut Frame)) -> canvas::Geometry {
        if self.shown.replace(key) != key {
            self.cache.clear();
        }
        self.cache.draw(renderer, size, |frame| {
            // tiny-skia repaints only the damaged parts of the window, and it
            // takes a canvas text's damage to start at its anchor: a changed
            // right-aligned or centered label would keep pieces of the old
            // one. A rebuilt drawing counts as damaged over the whole of
            // its bounds, so this invisible rectangle makes them the canvas.
            frame.fill_rectangle(Point::ORIGIN, size, Color::TRANSPARENT);
            draw(frame);
        })
    }
}

/// Where everything goes, for the drawing, the crosshair and the fingerprint.
struct Layout {
    plot: Rectangle,
    top: f32,
    step: f32,
    /// The points with a value, in the plot: (index, position).
    points: Vec<(usize, Point)>,
}

impl<Message> canvas::Program<Message> for Chart<'_> {
    type State = Drawn;

    fn update(
        &self,
        state: &mut Drawn,
        event: &canvas::Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<Message>> {
        let canvas::Event::Mouse(mouse::Event::CursorMoved { .. } | mouse::Event::CursorLeft) = event else {
            return None;
        };
        let inside = cursor.is_over(bounds);
        if inside || state.inside {
            state.inside = inside;
            return Some(canvas::Action::request_redraw());
        }
        None
    }

    fn draw(
        &self,
        state: &Drawn,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let l = self.layout(bounds.size());
        let key = self.fingerprint(&l, theme);
        let mut layers = vec![state.get(renderer, bounds.size(), key, |frame| self.draw_base(frame, theme, &l))];
        if let Some(c) = cursor.position_in(bounds).filter(|c| l.plot.contains(*c) && !self.faded) {
            let mut frame = Frame::new(renderer, bounds.size());
            self.draw_crosshair(&mut frame, theme, &l, c);
            layers.push(frame.into_geometry());
        }
        layers
    }

    fn mouse_interaction(&self, _state: &Drawn, bounds: Rectangle, cursor: mouse::Cursor) -> mouse::Interaction {
        match cursor.position_in(bounds) {
            Some(p) if plot_area(bounds.size()).contains(p) => mouse::Interaction::Crosshair,
            _ => mouse::Interaction::default(),
        }
    }
}

fn plot_area(size: Size) -> Rectangle {
    Rectangle {
        x: LEFT,
        y: TOP,
        width: (size.width - LEFT - RIGHT).max(1.0),
        height: (size.height - TOP - BOTTOM).max(1.0),
    }
}

/// Text colors: secondary (labels) and muted (grid).
fn ink(theme: &Theme, alpha: f32) -> Color {
    Color { a: alpha, ..crate::look::pal(theme).ink }
}

fn label(content: String, position: Point, color: Color, ax: alignment::Horizontal, ay: alignment::Vertical) -> Text {
    Text { content, position, color, size: Pixels(LABEL), align_x: ax.into(), align_y: ay, ..Text::default() }
}

impl Chart<'_> {
    fn x_of(&self, plot: Rectangle, t: f64) -> f32 {
        let from = self.end_s.saturating_sub(self.span_s) as f64;
        plot.x + ((t - from) / self.span_s.max(1) as f64) as f32 * plot.width
    }

    /// The middle of point `i`.
    fn t_of(&self, i: usize) -> f64 {
        self.start_s as f64 + (i as f64 + 0.5) * self.step_s as f64
    }

    fn layout(&self, size: Size) -> Layout {
        let plot = plot_area(size);
        let max = self.series.values.iter().flatten().fold(0.0f32, |m, v| m.max(*v));
        let (top, step) = y_scale(max, self.unit);
        let y_of = |v: f32| plot.y + plot.height - (v / top).clamp(0.0, 1.0) * plot.height;
        let points = self
            .series
            .values
            .iter()
            .enumerate()
            .filter_map(|(i, v)| v.map(|v| (i, Point::new(self.x_of(plot, self.t_of(i)), y_of(v)))))
            .filter(|(_, p)| p.x >= plot.x - 0.5 && p.x <= plot.x + plot.width + 0.5)
            .collect();
        Layout { plot, top, step, points }
    }

    /// Everything the drawing depends on, positions to the half pixel.
    fn fingerprint(&self, l: &Layout, theme: &Theme) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (self.start_s, self.step_s, self.end_s, self.span_s, self.unit as u8, self.hue as u8).hash(&mut h);
        (self.bars, self.area, self.faded, theme.extended_palette().is_dark).hash(&mut h);
        (l.plot.width.to_bits(), l.plot.height.to_bits(), l.top.to_bits()).hash(&mut h);
        for (i, p) in &l.points {
            (*i, (p.x * 2.0) as i32, (p.y * 2.0) as i32).hash(&mut h);
        }
        h.finish()
    }

    fn draw_base(&self, frame: &mut Frame, theme: &Theme, l: &Layout) {
        let (plot, top, step) = (l.plot, l.top, l.step);
        let color = Hue::color(self.hue, theme);
        let fade = if self.faded { 0.35 } else { 1.0 };
        let y_of = |v: f32| plot.y + plot.height - (v / top).clamp(0.0, 1.0) * plot.height;

        // Grid: hairlines at the y ticks (the baseline a step stronger), x ticks.
        let mut v = 0.0;
        while v <= top + step * 0.01 {
            let y = y_of(v).round() + 0.5;
            let line = Path::line(Point::new(plot.x, y), Point::new(plot.x + plot.width, y));
            frame.stroke(
                &line,
                Stroke::default().with_width(1.0).with_color(ink(theme, if v == 0.0 { 0.22 } else { 0.08 })),
            );
            let text = format_value(v, self.unit, step);
            frame.fill_text(label(
                text,
                Point::new(plot.x - 8.0, y),
                ink(theme, 0.62),
                alignment::Horizontal::Right,
                alignment::Vertical::Center,
            ));
            v += step;
        }
        let from = self.end_s.saturating_sub(self.span_s);
        let every = x_tick_every(self.span_s);
        let offset = local_offset_s(self.end_s);
        let mut t = from - (from as i64 + offset).rem_euclid(every as i64) as u64;
        while t <= self.end_s {
            if t >= from {
                let x = self.x_of(plot, t as f64).round() + 0.5;
                let tick = Path::line(Point::new(x, plot.y), Point::new(x, plot.y + plot.height));
                frame.stroke(&tick, Stroke::default().with_width(1.0).with_color(ink(theme, 0.05)));
                // Not under the y axis' labels, nor cut at the right edge.
                if x >= plot.x + 20.0 && x <= plot.x + plot.width - 14.0 {
                    frame.fill_text(label(
                        clock(t, false),
                        Point::new(x, plot.y + plot.height + 5.0),
                        ink(theme, 0.62),
                        alignment::Horizontal::Center,
                        alignment::Vertical::Top,
                    ));
                }
            }
            t += every;
        }

        let points = &l.points;
        if points.is_empty() {
            frame.fill_text(label(
                if self.faded { "loading…".into() } else { "no samples in this period".into() },
                Point::new(plot.center_x(), plot.center_y()),
                ink(theme, 0.5),
                alignment::Horizontal::Center,
                alignment::Vertical::Center,
            ));
            return;
        }
        let base = plot.y + plot.height;
        if self.bars {
            let slot = plot.width * self.step_s as f32 / self.span_s.max(1) as f32;
            // Restarts are rare spikes: wide enough to see, never wider than 24 px.
            let w = (slot - 1.0).clamp(3.0, 24.0);
            for (_, p) in points.iter().filter(|(_, p)| p.y < base - 0.1) {
                let r = Rectangle { x: p.x - w / 2.0, y: p.y, width: w, height: base - p.y };
                frame.fill_rectangle(r.position(), r.size(), Color { a: fade, ..color });
            }
        } else {
            // Runs of consecutive points: a gap in the samples is a gap in the line.
            let mut runs: Vec<Vec<Point>> = Vec::new();
            let mut prev: Option<usize> = None;
            for (i, p) in points {
                match (prev, runs.last_mut()) {
                    (Some(j), Some(run)) if *i == j + 1 => run.push(*p),
                    _ => runs.push(vec![*p]),
                }
                prev = Some(*i);
            }
            for run in &runs {
                if run.len() == 1 {
                    frame.fill(&Path::circle(run[0], 2.0), Color { a: fade, ..color });
                    continue;
                }
                if self.area {
                    let area = Path::new(|b| {
                        b.move_to(Point::new(run[0].x, base));
                        for p in run {
                            b.line_to(*p);
                        }
                        b.line_to(Point::new(run[run.len() - 1].x, base));
                        b.close();
                    });
                    frame.fill(&area, Color { a: 0.10 * fade, ..color });
                }
                let line = Path::new(|b| {
                    b.move_to(run[0]);
                    for p in &run[1..] {
                        b.line_to(*p);
                    }
                });
                frame.stroke(
                    &line,
                    Stroke::default()
                        .with_width(2.0)
                        .with_color(Color { a: fade, ..color })
                        .with_line_join(canvas::LineJoin::Round)
                        .with_line_cap(canvas::LineCap::Round),
                );
            }
        }
    }

    /// The crosshair: the point nearest the pointer `c`, its time and value.
    fn draw_crosshair(&self, frame: &mut Frame, theme: &Theme, l: &Layout, c: Point) {
        let (plot, step) = (l.plot, l.step);
        let base = plot.y + plot.height;
        let color = Hue::color(self.hue, theme);
        let Some((i, p)) = l.points.iter().min_by(|a, b| (a.1.x - c.x).abs().total_cmp(&(b.1.x - c.x).abs())) else {
            return;
        };
        let hair = Path::line(Point::new(p.x, plot.y), Point::new(p.x, base));
        frame.stroke(&hair, Stroke::default().with_width(1.0).with_color(ink(theme, 0.45)));
        let surface = crate::look::pal(theme).card;
        frame.fill(&Path::circle(*p, 6.0), surface);
        frame.fill(&Path::circle(*p, 4.0), color);
        let value = self.series.values.get(*i).copied().flatten().unwrap_or(0.0);
        let at = self.start_s + *i as u64 * self.step_s;
        let what = format_value(value, self.unit, step / 10.0);
        let when = if self.step_s >= 60 {
            format!("{}–{}", clock(at, false), clock(at + self.step_s, false))
        } else {
            clock(at, true)
        };
        let (w, h) = (12.0 + 7.0 * (what.len() + when.len() + 3) as f32, 22.0);
        let left = if p.x + 12.0 + w > plot.x + plot.width { p.x - 12.0 - w } else { p.x + 12.0 };
        // At the top, unless the point is there: then at the bottom.
        let top = if p.y < plot.y + h + 12.0 { base - h - 2.0 } else { plot.y + 2.0 };
        let bx = Rectangle { x: left, y: top, width: w, height: h };
        let pal = crate::look::pal(theme);
        frame.fill(&Path::rounded_rectangle(bx.position(), bx.size(), 7.0.into()), pal.pill);
        frame.fill_text(Text {
            content: what,
            position: Point::new(bx.x + 8.0, bx.center_y()),
            color: pal.on_pill,
            size: Pixels(12.0),
            font: iced::Font { weight: iced::font::Weight::Bold, ..iced::Font::default() },
            align_y: alignment::Vertical::Center,
            ..Text::default()
        });
        frame.fill_text(label(
            when,
            Point::new(bx.x + bx.width - 8.0, bx.center_y()),
            ink(theme, 0.7),
            alignment::Horizontal::Right,
            alignment::Vertical::Center,
        ));
    }
}

/// The y axis' top and tick step: a round number above the data, at least
/// one unit (1%, 1 MB, 1 restart), 2 to 5 steps.
pub fn y_scale(max: f32, unit: Unit) -> (f32, f32) {
    let floor = match unit {
        Unit::Percent => 1.0,
        Unit::Bytes => 1024.0 * 1024.0,
        Unit::Count => 2.0,
    };
    let max = max.max(floor);
    let raw = max / 3.0;
    let mag = 10f32.powi(raw.log10().floor() as i32);
    let mut step = [1.0, 2.0, 2.5, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    if unit == Unit::Count {
        step = step.ceil().max(1.0);
    }
    if unit == Unit::Bytes {
        // Binary-friendly steps: 256 MB rather than 250 MB.
        let mb = 1024.0 * 1024.0;
        let steps =
            [1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 512.0, 1024.0, 2048.0, 4096.0, 8192.0, 16384.0];
        step = steps.iter().map(|s| s * mb).find(|s| *s >= raw).unwrap_or(step);
    }
    ((max / step).ceil().max(1.0) * step, step)
}

/// A tick or readout value with its unit.
pub fn format_value(v: f32, unit: Unit, step: f32) -> String {
    match unit {
        Unit::Percent if step < 0.1 => format!("{v:.2}%"),
        Unit::Percent if step < 1.0 => format!("{v:.1}%"),
        Unit::Percent => format!("{v:.0}%"),
        Unit::Count => format!("{v:.0}"),
        Unit::Bytes => {
            let mb = v / (1024.0 * 1024.0);
            let gb = mb / 1024.0;
            if gb >= 1.0 {
                if (gb * 10.0).fract().abs() < 0.01 || step >= 1024.0 * 1024.0 * 1024.0 {
                    format!("{gb:.1} GB")
                } else {
                    format!("{gb:.2} GB")
                }
            } else if mb >= 10.0 || mb == 0.0 {
                format!("{mb:.0} MB")
            } else {
                format!("{mb:.1} MB")
            }
        }
    }
}

/// Seconds between x ticks: about six across.
fn x_tick_every(span_s: u64) -> u64 {
    [60, 300, 600, 900, 1800, 3600, 2 * 3600, 3 * 3600, 4 * 3600, 6 * 3600, 12 * 3600]
        .into_iter()
        .find(|s| span_s / s <= 6)
        .unwrap_or(86_400)
}

/// This machine's offset from UTC at `t_s` (ticks fall on local hours,
/// India's :30 included).
fn local_offset_s(t_s: u64) -> i64 {
    use chrono::{Offset, TimeZone};
    chrono::Local
        .timestamp_opt(t_s.min(i64::MAX as u64) as i64, 0)
        .single()
        .map(|t| i64::from(t.offset().fix().local_minus_utc()))
        .unwrap_or(0)
}

/// `14:30` (or `14:30:10`), local time.
fn clock(t_s: u64, seconds: bool) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(t_s.min(i64::MAX as u64) as i64, 0).single() {
        Some(t) => t.format(if seconds { "%H:%M:%S" } else { "%H:%M" }).to_string(),
        None => "--:--".into(),
    }
}

/// A small trend line for the header: no axes, the last hour.
pub struct Sparkline<'a> {
    pub series: &'a Series,
    pub hue: Hue,
    /// The value at the top (memory: the host's total); none: the data's max.
    pub top: Option<f32>,
    pub floor: f32,
}

impl<Message> canvas::Program<Message> for Sparkline<'_> {
    type State = Drawn;

    fn draw(
        &self,
        state: &Drawn,
        renderer: &Renderer,
        theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let (w, h) = (bounds.width, bounds.height);
        let n = self.series.values.len().max(2);
        let max = self.series.values.iter().flatten().fold(self.floor, |m, v| m.max(*v));
        let top = self.top.filter(|t| *t > 0.0).unwrap_or(max);
        let pt = |i: usize, v: f32| {
            Point::new(1.0 + (w - 2.0) * i as f32 / (n - 1) as f32, h - 1.0 - (v / top).clamp(0.0, 1.0) * (h - 2.0))
        };
        let key = {
            use std::hash::{Hash, Hasher};
            let mut k = std::collections::hash_map::DefaultHasher::new();
            (w.to_bits(), h.to_bits(), self.hue as u8, theme.extended_palette().is_dark).hash(&mut k);
            for (i, v) in self.series.values.iter().enumerate() {
                v.map(|v| (pt(i, v).y * 2.0) as i32).hash(&mut k);
            }
            k.finish()
        };
        vec![state.get(renderer, bounds.size(), key, |frame| self.draw_line(frame, theme, w, h, &pt))]
    }
}

impl Sparkline<'_> {
    fn draw_line(&self, frame: &mut Frame, theme: &Theme, w: f32, h: f32, pt: &dyn Fn(usize, f32) -> Point) {
        frame.fill_rectangle(Point::new(0.0, h - 1.0), Size::new(w, 1.0), ink(theme, 0.15));
        let color = Hue::color(self.hue, theme);
        let mut run: Vec<Point> = Vec::new();
        let flush = |run: &mut Vec<Point>, frame: &mut Frame| {
            if run.len() >= 2 {
                let area = Path::new(|b| {
                    b.move_to(Point::new(run[0].x, h));
                    for p in run.iter() {
                        b.line_to(*p);
                    }
                    b.line_to(Point::new(run[run.len() - 1].x, h));
                    b.close();
                });
                frame.fill(&area, Color { a: 0.25, ..color });
                let line = Path::new(|b| {
                    b.move_to(run[0]);
                    for p in &run[1..] {
                        b.line_to(*p);
                    }
                });
                frame.stroke(&line, Stroke::default().with_width(1.5).with_color(color));
            }
            run.clear();
        };
        for (i, v) in self.series.values.iter().enumerate() {
            match v {
                Some(v) => run.push(pt(i, *v)),
                None => flush(&mut run, frame),
            }
        }
        flush(&mut run, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn y_scales_are_round() {
        let close = |(a, b): (f32, f32), (x, y): (f32, f32)| (a - x).abs() <= x * 1e-5 && (b - y).abs() <= y * 1e-5;
        for (max, unit, want) in [
            (0.0, Unit::Percent, (1.0, 0.5)),
            (3.2, Unit::Percent, (4.0, 2.0)),
            (87.0, Unit::Percent, (100.0, 50.0)),
            (140.0, Unit::Percent, (150.0, 50.0)),
            (0.0, Unit::Count, (2.0, 1.0)),
            (7.0, Unit::Count, (9.0, 3.0)),
            (130.0 * 1048576.0, Unit::Bytes, (192.0 * 1048576.0, 64.0 * 1048576.0)),
            (3.1 * 1073741824.0, Unit::Bytes, (4096.0 * 1048576.0, 2048.0 * 1048576.0)),
        ] {
            let got = y_scale(max, unit);
            assert!(close(got, want), "{max} {unit:?}: {got:?}, not {want:?}");
        }
    }

    #[test]
    fn values_have_units() {
        let mb = 1024.0 * 1024.0;
        assert_eq!(format_value(64.0 * mb, Unit::Bytes, 64.0 * mb), "64 MB");
        assert_eq!(format_value(1.5 * mb, Unit::Bytes, mb), "1.5 MB");
        assert_eq!(format_value(2048.0 * mb, Unit::Bytes, 1024.0 * mb), "2.0 GB");
        assert_eq!(format_value(1.0, Unit::Percent, 0.5), "1.0%");
        assert_eq!(format_value(30.0, Unit::Percent, 10.0), "30%");
        assert_eq!(format_value(3.0, Unit::Count, 1.0), "3");
        assert_eq!(x_tick_every(3600), 600);
        assert_eq!(x_tick_every(6 * 3600), 3600);
        assert_eq!(x_tick_every(24 * 3600), 4 * 3600);
    }
}

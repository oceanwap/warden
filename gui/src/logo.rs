//! The Warden logo, drawn: the app icon (`assets/icon/warden.svg`, as the macOS icon draws it,
//! edge to edge) on a `canvas`, so the window's header shows the same mark as the Dock. The shapes
//! are the SVG's, in its 1024 px coordinates.

use iced::mouse;
use iced::widget::canvas::{self, Frame, Path, Stroke, gradient::Linear};
use iced::{Color, Point, Rectangle, Renderer, Size, Theme, color};

/// The logo, `canvas(Logo)` sized square.
#[derive(Debug, Clone, Copy, Default)]
pub struct Logo;

/// The SVG's content is scaled by this about the middle when the plate fills the icon
/// (`full_bleed_svg` in `assets/icon/build.py`).
const BLEED: f32 = 1.2427;

/// The shield's outline: corners and curves, as in the SVG.
const SHIELD: [(f32, f32); 13] = [
    (512.0, 238.0),
    (578.0, 274.0),
    (654.0, 292.0),
    (732.0, 294.0),
    (732.0, 522.0),
    (732.0, 648.0),
    (636.0, 742.0),
    (512.0, 800.0),
    (388.0, 742.0),
    (292.0, 648.0),
    (292.0, 522.0),
    (292.0, 294.0),
    (370.0, 292.0),
];

const W: [(f32, f32); 5] = [(384.0, 410.0), (448.0, 624.0), (512.0, 466.0), (576.0, 624.0), (640.0, 410.0)];

impl<Message> canvas::Program<Message> for Logo {
    type State = canvas::Cache;

    fn draw(
        &self,
        cache: &canvas::Cache,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        vec![cache.draw(renderer, bounds.size(), |frame| draw(frame, bounds.width.min(bounds.height)))]
    }
}

fn draw(frame: &mut Frame, size: f32) {
    let k = size / 1024.0;
    // A point of the SVG, where it lands.
    let at = |x: f32, y: f32| Point::new(((x - 512.0) * BLEED + 512.0) * k, ((y - 512.0) * BLEED + 512.0) * k);
    let linear = |from: (f32, f32), to: (f32, f32), stops: &[(f32, Color)]| {
        stops.iter().fold(Linear::new(at(from.0, from.1), at(to.0, to.1)), |g, (o, c)| g.add_stop(*o, *c))
    };

    let plate = Path::rounded_rectangle(Point::ORIGIN, Size::new(size, size), (size * 0.225).into());
    frame.fill(&plate, linear((512.0, 0.0), (512.0, 1024.0), &[(0.0, color!(0x202220)), (1.0, color!(0x0d0e0d))]));

    let shield = |scale: f32| {
        // The face is the outline scaled 0.9 about (512, 519).
        let p = |(x, y): (f32, f32)| at(512.0 + (x - 512.0) * scale, 519.0 + (y - 519.0) * scale);
        Path::new(|b| {
            b.move_to(p(SHIELD[0]));
            b.bezier_curve_to(p(SHIELD[1]), p(SHIELD[2]), p(SHIELD[3]));
            b.line_to(p(SHIELD[4]));
            b.bezier_curve_to(p(SHIELD[5]), p(SHIELD[6]), p(SHIELD[7]));
            b.bezier_curve_to(p(SHIELD[8]), p(SHIELD[9]), p(SHIELD[10]));
            b.line_to(p(SHIELD[11]));
            b.bezier_curve_to(p(SHIELD[12]), p((446.0, 274.0)), p(SHIELD[0]));
            b.close();
        })
    };
    frame.fill(
        &shield(1.0),
        linear(
            (358.0, 238.0),
            (666.0, 800.0),
            &[(0.0, color!(0x8ee6b0)), (0.55, color!(0x4eae78)), (1.0, color!(0x1d7246))],
        ),
    );
    frame.fill(
        &shield(0.9),
        linear((512.0, 238.0), (512.0, 800.0), &[(0.0, color!(0x1b241e)), (1.0, color!(0x101411))]),
    );

    let w = Path::new(|b| {
        b.move_to(at(W[0].0, W[0].1));
        for (x, y) in &W[1..] {
            b.line_to(at(*x, *y));
        }
    });
    frame.stroke(
        &w,
        Stroke {
            style: canvas::Style::Gradient(
                linear((512.0, 410.0), (512.0, 624.0), &[(0.0, Color::WHITE), (1.0, color!(0xc8f0d8))]).into(),
            ),
            width: 48.0 * BLEED * k,
            line_cap: canvas::LineCap::Round,
            line_join: canvas::LineJoin::Round,
            ..Stroke::default()
        },
    );

    // The status light, its glow a wider faint disc.
    let led = at(680.0, 342.0);
    frame.fill(&Path::circle(led, 30.0 * BLEED * k), Color { a: 0.35, ..color!(0x34d399) });
    frame.fill(&Path::circle(led, 19.0 * BLEED * k), color!(0x4ade80));
    frame.fill(&Path::circle(at(673.0, 335.0), 6.0 * BLEED * k), Color { a: 0.55, ..Color::WHITE });
}

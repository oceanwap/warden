//! The embedded fonts resolve as the look asks: each weight of Inter is its own face (a request
//! for Medium that fell back to Regular, or to SemiBold, would draw the same pixels), the
//! headings and the logs have their own faces, and every icon is a glyph of the icon font.

use iced::widget::{column, text};
use iced::{Element, Font};
use iced_test::Simulator;
use warden_gui::look::{self, DISPLAY, DISPLAY_BOLD, ICONS, MEDIUM, MONO, SANS, SEMIBOLD};

/// What `font` draws for `sample`, as a hash of the pixels.
fn drawn(font: Font, sample: &str) -> String {
    let view: Element<'_, ()> = column![text(sample.to_string()).font(font).size(28)].padding(12).into();
    let mut ui = Simulator::with_size(warden_gui::settings(), (900.0, 80.0), view);
    let snap = ui.snapshot(&look::theme(true)).expect("renders");
    let dir = std::env::temp_dir().join(format!("wg-fonts-{}-{}", std::process::id(), rand_name()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(snap.matches_hash(dir.join("s")).expect("writes the hash"));
    let hash = std::fs::read_to_string(dir.join("s-tiny-skia.sha256")).expect("the hash file");
    let _ = std::fs::remove_dir_all(&dir);
    hash
}

fn rand_name() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    N.fetch_add(1, Ordering::Relaxed)
}

const SAMPLE: &str = "Restarting api: 3 / 4 ready, Hamburgefonstiv 0123456789";

#[test]
fn every_weight_of_inter_is_its_own_face() {
    let faces = [("Regular", SANS), ("Medium", MEDIUM), ("SemiBold", SEMIBOLD)];
    let hashes: Vec<String> = faces.iter().map(|(_, f)| drawn(*f, SAMPLE)).collect();
    for i in 0..faces.len() {
        for j in i + 1..faces.len() {
            assert_ne!(
                hashes[i], hashes[j],
                "Inter {} and {} draw alike: one fell back to the other",
                faces[i].0, faces[j].0
            );
        }
    }
}

#[test]
fn headings_and_logs_have_faces_of_their_own() {
    let sans = drawn(SANS, SAMPLE);
    let display = drawn(DISPLAY, SAMPLE);
    let bold = drawn(DISPLAY_BOLD, SAMPLE);
    let mono = drawn(MONO, SAMPLE);
    let unknown = drawn(Font::with_name("No Such Family"), SAMPLE);
    assert_ne!(display, bold, "Plus Jakarta Sans ExtraBold and Bold draw alike");
    for (name, h) in
        [("Plus Jakarta Sans ExtraBold", &display), ("Plus Jakarta Sans Bold", &bold), ("JetBrains Mono", &mono)]
    {
        assert_ne!(*h, sans, "{name} draws like Inter: it was not found");
        assert_ne!(*h, unknown, "{name} draws like a family that does not exist: it was not found");
    }
    assert_ne!(sans, unknown, "Inter draws like a family that does not exist: it was not found");
}

#[test]
fn every_icon_is_drawn_from_the_icon_font() {
    use warden_gui::icons::Icon;
    // What the font draws for a code point it does not have, and for nothing.
    let missing = drawn(ICONS, "\u{f8ff}");
    let blank = drawn(ICONS, " ");
    for i in Icon::ALL {
        let h = drawn(ICONS, &i.glyph().to_string());
        assert_ne!(h, missing, "{i:?} ({:?}) is not in the icon font", i.glyph());
        assert_ne!(h, blank, "{i:?} draws nothing");
    }
}

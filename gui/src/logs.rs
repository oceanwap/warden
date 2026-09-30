//! The logs pane: a bounded ring of one app's log lines, with pause, a text
//! filter and stdout / stderr / events toggles. Floods cost lines, never
//! the UI: the client batches lines per frame and trims each batch, and a
//! "N lines skipped" note marks every gap, as `warden logs -f` does.

use crate::ring::Ring;

/// Lines kept (the pane shows the newest).
pub const LOG_CAP: usize = 2000;
/// Lines of history fetched when the pane opens.
pub const HISTORY_LINES: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
    /// Warden's own lines (`INFO  worker ready …`).
    Event,
    /// Ours: a gap in the stream.
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub stream: Stream,
    pub text: String,
}

impl LogLine {
    pub fn new(text: String) -> LogLine {
        LogLine { stream: classify(&text), text }
    }

    fn note(text: String) -> LogLine {
        LogLine { stream: Stream::Note, text }
    }
}

/// Worker output is `<ts> OUT   worker=<w> <stdout|stderr>: <text>`
/// (docs/protocol.md; `control::stream_filter`); anything else is Warden's.
pub fn classify(line: &str) -> Stream {
    let rest = match line.find(" OUT   worker=") {
        Some(i) => &line[i + " OUT   worker=".len()..],
        None => match line.strip_prefix("OUT   worker=") {
            Some(r) => r,
            None => return Stream::Event,
        },
    };
    match rest.split_once(' ') {
        Some((_, r)) if r.starts_with("stderr: ") => Stream::Stderr,
        Some((_, r)) if r.starts_with("stdout: ") => Stream::Stdout,
        _ => Stream::Event,
    }
}

/// Scroll state of a list drawn only where it is visible (`view::lines`).
/// The list is anchored at the bottom, like a terminal: new lines appear
/// there, and the offset is counted from the newest line, so it stays right
/// however many lines arrive.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Scroll {
    /// Pixels between the bottom of the view and the newest line.
    pub from_bottom: f32,
}

impl Scroll {
    /// The rows to draw out of `total`, each `line_h` tall, in a view
    /// `height` tall: `start..end` (with a row to spare on each side).
    pub fn window(&self, total: usize, line_h: f32, height: f32) -> (usize, usize) {
        let rows = (height / line_h).ceil().max(1.0) as usize;
        let skip = (self.from_bottom / line_h).floor().max(0.0) as usize;
        let end = total.saturating_sub(skip.saturating_sub(1));
        (end.saturating_sub(rows + 2), end)
    }

    pub fn scrolled(&mut self, from_bottom: f32) {
        self.from_bottom = from_bottom.max(0.0);
    }

    /// Showing the newest lines.
    pub fn at_bottom(&self) -> bool {
        self.from_bottom < 1.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum History {
    Loading,
    Loaded,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct LogPane {
    pub app: String,
    ring: Ring<LogLine>,
    /// Lines that came while paused, shown on resume.
    held: Ring<LogLine>,
    pub paused: bool,
    pub filter: String,
    pub stdout: bool,
    pub stderr: bool,
    pub events: bool,
    pub scroll: Scroll,
    pub history: History,
    /// Lines lost to floods and lagging, all told.
    pub skipped: u64,
}

impl LogPane {
    pub fn new(app: &str) -> LogPane {
        LogPane {
            app: app.to_string(),
            ring: Ring::new(LOG_CAP),
            // One less than the ring: the "skipped while paused" note stays in front.
            held: Ring::new(LOG_CAP - 1),
            paused: false,
            filter: String::new(),
            stdout: true,
            stderr: true,
            events: true,
            scroll: Scroll::default(),
            history: History::Loading,
            skipped: 0,
        }
    }

    /// Lines from one batch; `skipped` were dropped before it (flood, lag).
    pub fn push(&mut self, lines: Vec<String>, skipped: u64) {
        let target = if self.paused { &mut self.held } else { &mut self.ring };
        if skipped > 0 {
            self.skipped += skipped;
            target.push(LogLine::note(format!(
                "… {skipped} lines skipped (they came faster than this window shows them; `warden logs {} --history` has them all)",
                self.app
            )));
        }
        target.extend(lines.into_iter().map(LogLine::new));
    }

    /// A line of ours (a gap, a reconnect), shown whatever the filters.
    pub fn note(&mut self, text: String) {
        let target = if self.paused { &mut self.held } else { &mut self.ring };
        target.push(LogLine::note(text));
    }

    pub fn set_paused(&mut self, paused: bool) {
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        if !paused {
            let lost = self.held.dropped();
            if lost > 0 {
                self.skipped += lost;
                self.ring.push(LogLine::note(format!("… {lost} lines skipped while paused")));
            }
            let held: Vec<LogLine> = self.held.drain().collect();
            self.held.clear();
            self.ring.extend(held);
        }
    }

    /// Lines held while paused.
    pub fn held(&self) -> usize {
        self.held.len()
    }

    /// The history fetched when the pane opened: lines older than the first
    /// live line go in front (the two may overlap by a few lines).
    pub fn merge_history(&mut self, history: Vec<String>) {
        self.history = History::Loaded;
        let first_live = self.ring.iter().chain(self.held.iter()).find(|l| l.stream != Stream::Note).map(|l| &l.text);
        let cut = match first_live {
            Some(first) => history.iter().rposition(|h| h == first).unwrap_or(history.len()),
            None => history.len(),
        };
        let older: Vec<LogLine> = history.into_iter().take(cut).map(LogLine::new).collect();
        self.ring.prepend(older);
    }

    fn shown(&self, l: &LogLine) -> bool {
        let kind = match l.stream {
            Stream::Stdout => self.stdout,
            Stream::Stderr => self.stderr,
            Stream::Event => self.events,
            Stream::Note => true,
        };
        kind && (self.filter.is_empty() || contains_ignore_case(&l.text, &self.filter))
    }

    /// The lines that pass the toggles and the filter, oldest first.
    pub fn visible(&self) -> Vec<&LogLine> {
        self.ring.iter().filter(|l| self.shown(l)).collect()
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    pub fn clear(&mut self) {
        self.ring.clear();
        self.held.clear();
    }
}

/// Case-insensitive `contains` for ASCII (log lines are mostly ASCII; other
/// characters must match exactly).
pub fn contains_ignore_case(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.len() > h.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUT: &str = "2026-09-30T12:00:00.000Z OUT   worker=1 stdout: hello";
    const ERR: &str = "2026-09-30T12:00:00.000Z OUT   worker=12 stderr: boom: bad";
    const EV: &str = "2026-09-30T12:00:00.000Z INFO  worker ready worker=1 pid=5";

    #[test]
    fn lines_are_classified_like_the_server_filters() {
        assert_eq!(classify(OUT), Stream::Stdout);
        assert_eq!(classify(ERR), Stream::Stderr);
        assert_eq!(classify(EV), Stream::Event);
        assert_eq!(classify("OUT   worker=2 stderr: no timestamp"), Stream::Stderr);
        assert_eq!(classify("INFO  said OUT   worker= in text"), Stream::Event);
    }

    #[test]
    fn toggles_and_filter() {
        let mut p = LogPane::new("api");
        p.push(vec![OUT.into(), ERR.into(), EV.into()], 0);
        assert_eq!(p.visible().len(), 3);
        p.stdout = false;
        assert_eq!(p.visible().iter().map(|l| l.stream).collect::<Vec<_>>(), [Stream::Stderr, Stream::Event]);
        p.filter = "BOOM".into();
        assert_eq!(p.visible().len(), 1);
        p.filter = "nothing".into();
        assert!(p.visible().is_empty());
        assert!(contains_ignore_case("Hello World", "o w") && !contains_ignore_case("ab", "abc"));
    }

    #[test]
    fn a_flood_stays_bounded_and_says_what_was_skipped() {
        let mut p = LogPane::new("api");
        p.push((0..LOG_CAP * 3).map(|i| format!("line {i}")).collect(), 0);
        assert_eq!(p.len(), LOG_CAP);
        assert_eq!(p.visible().last().unwrap().text, format!("line {}", LOG_CAP * 3 - 1));
        p.push(vec!["after".into()], 120);
        let v = p.visible();
        let note = &v[v.len() - 2];
        assert_eq!(note.stream, Stream::Note);
        assert!(note.text.starts_with("… 120 lines skipped"), "{}", note.text);
        assert_eq!(p.skipped, 120);
    }

    #[test]
    fn pause_holds_lines_until_resumed() {
        let mut p = LogPane::new("api");
        p.push(vec!["a".into()], 0);
        p.set_paused(true);
        p.push(vec!["b".into(), "c".into()], 0);
        assert_eq!(p.visible().len(), 1);
        assert_eq!(p.held(), 2);
        p.set_paused(false);
        assert_eq!(p.visible().iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        // Too much while paused: the oldest held lines go, with a note.
        p.set_paused(true);
        p.push((0..LOG_CAP + 10).map(|i| format!("h{i}")).collect(), 0);
        p.set_paused(false);
        let v = p.visible();
        assert_eq!(v[0].text, "… 11 lines skipped while paused", "the gap is marked before the held lines");
        assert_eq!(v[1].text, "h11");
        assert_eq!(p.len(), LOG_CAP);
    }

    #[test]
    fn history_goes_in_front_without_duplicates() {
        let mut p = LogPane::new("api");
        p.push(vec!["l3".into(), "l4".into()], 0);
        p.merge_history(vec!["l1".into(), "l2".into(), "l3".into()]);
        let texts: Vec<&str> = p.visible().iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["l1", "l2", "l3", "l4"]);
        assert_eq!(p.history, History::Loaded);
        let mut empty = LogPane::new("api");
        empty.merge_history(vec!["x".into()]);
        assert_eq!(empty.len(), 1);
    }

    #[test]
    fn scroll_window() {
        let mut s = Scroll::default();
        assert!(s.at_bottom());
        assert_eq!(s.window(1000, 16.0, 160.0), (988, 1000));
        assert_eq!(s.window(5, 16.0, 160.0), (0, 5));
        s.scrolled(160.0);
        assert!(!s.at_bottom());
        assert_eq!(s.window(1000, 16.0, 160.0), (979, 991));
        s.scrolled(1_000_000.0);
        assert_eq!(s.window(1000, 16.0, 160.0), (0, 0), "past the top: nothing but spacers");
        s.scrolled(-5.0);
        assert_eq!(s.from_bottom, 0.0);
    }
}

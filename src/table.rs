//! Tables for the terminal, like PM2's: a box of `┌─┬─┐` rules with a header
//! row (`warden list`, the worker table), or a two-column key | value box
//! (`warden describe`). Colors only on a terminal; wrapping only when the
//! width is known, and only at spaces, so a path or a URL is never cut.

use std::os::fd::RawFd;

/// SGR codes for styled cells (only used when colors are on).
pub(crate) const HEAD: &str = "1;35";
pub(crate) const KEY: &str = "1;36";
pub(crate) const NAME: &str = "1";
pub(crate) const DIM: &str = "2";
pub(crate) const GREEN: &str = "32";
pub(crate) const YELLOW: &str = "33";
pub(crate) const RED: &str = "31";

/// The narrowest a wrapping column gets, however small the terminal.
const MIN_WRAP: usize = 24;

/// How to print: with colors, and wrapped to this many columns.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Fmt {
    pub color: bool,
    pub width: Option<usize>,
}

impl Fmt {
    /// Plain text, never wrapped (a pipe, a file, the tests).
    pub const PLAIN: Fmt = Fmt { color: false, width: None };

    /// What suits standard output right now.
    pub fn stdout() -> Fmt {
        Fmt { color: use_color(1), width: term_width(1) }
    }

    /// `text` in bold when colors are on (a heading).
    pub fn bold(&self, text: &str) -> String {
        if self.color { format!("\x1b[1m{text}\x1b[0m") } else { text.to_string() }
    }
}

/// Should this descriptor get colors? A terminal, unless NO_COLOR is set
/// (no-color.org) or TERM is `dumb`.
pub fn use_color(fd: RawFd) -> bool {
    crate::sys::isatty(fd)
        && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// The width to wrap to: the terminal's, else `$COLUMNS` (someone who sets
/// it wants it, a pipe or not); `None` means don't wrap.
pub fn term_width(fd: RawFd) -> Option<usize> {
    crate::sys::terminal_width(fd)
        .or_else(|| std::env::var("COLUMNS").ok().and_then(|c| c.trim().parse().ok()).filter(|c| *c > 0))
}

/// One table cell: its text and, with colors on, how to paint it.
#[derive(Debug, Clone)]
pub(crate) struct Cell {
    pub(crate) text: String,
    pub(crate) style: &'static str,
    /// The text is several lines (`\n`), shown one under the other in any
    /// column: the row grows to fit. Otherwise a cell is one line.
    pub(crate) lines: bool,
}

impl Cell {
    pub(crate) fn plain(text: impl Into<String>) -> Cell {
        Cell { text: text.into(), style: "", lines: false }
    }

    pub(crate) fn styled(text: impl Into<String>, style: &'static str) -> Cell {
        Cell { text: text.into(), style, lines: false }
    }

    /// A cell of several lines, `\n` between them (a column of short items
    /// takes less width one under the other than side by side).
    pub(crate) fn lines(text: impl Into<String>) -> Cell {
        Cell { text: text.into(), style: "", lines: true }
    }
}

/// `text` cut to `max` characters, ending in `…` when it was longer.
pub(crate) fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut s: String = text.chars().take(max.saturating_sub(1)).collect();
    s.push('…');
    s
}

/// `text` broken into lines of at most `width` characters at spaces. A word
/// longer than that stays whole on its own line (paths are not cut).
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        let len = cur.chars().count();
        if !cur.is_empty() && len + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    lines.push(cur);
    lines
}

/// A box-drawn table. `header`: a heading row and a rule under it (none for
/// a key | value box). `flex`: the column whose cells may hold several lines
/// (`\n`) and are wrapped to fit `fmt.width`; any other cell is one line,
/// unless it was made with [`Cell::lines`].
/// Cells are left-aligned with a space each side; widths come from the text,
/// never from the colors.
pub(crate) fn boxed(header: Option<&[&str]>, rows: &[Vec<Cell>], fmt: &Fmt, flex: Option<usize>) -> String {
    let ncols = header.map(<[&str]>::len).unwrap_or_else(|| rows.iter().map(Vec::len).max().unwrap_or(0));
    if ncols == 0 {
        return String::new();
    }
    let one_line = |t: &str| t.replace(['\n', '\r', '\t'], " ");
    let head: Option<Vec<String>> = header.map(|h| h.iter().map(|t| one_line(t)).collect());
    // Each cell as lines.
    let mut body: Vec<Vec<Vec<String>>> = rows
        .iter()
        .map(|r| {
            (0..ncols)
                .map(|c| {
                    let t = r.get(c).map(|c| c.text.as_str()).unwrap_or("");
                    if Some(c) == flex || r.get(c).is_some_and(|c| c.lines) {
                        t.split('\n').map(|l| l.replace(['\r', '\t'], " ")).collect()
                    } else {
                        vec![one_line(t)]
                    }
                })
                .collect()
        })
        .collect();
    let width_of = |body: &[Vec<Vec<String>>], c: usize| {
        body.iter()
            .flat_map(|r| r[c].iter())
            .map(|l| l.chars().count())
            .chain(head.iter().map(|h| h[c].chars().count()))
            .max()
            .unwrap_or(0)
    };
    let mut widths: Vec<usize> = (0..ncols).map(|c| width_of(&body, c)).collect();
    if let (Some(total), Some(f)) = (fmt.width, flex) {
        let used: usize = widths.iter().sum::<usize>() + 3 * ncols + 1;
        if used > total {
            let target = total.saturating_sub(used - widths[f]).max(MIN_WRAP);
            if target < widths[f] {
                for r in &mut body {
                    r[f] = r[f].iter().flat_map(|l| wrap(l, target)).collect();
                }
                widths[f] = width_of(&body, f);
            }
        }
    }
    let rule = |l: &str, m: &str, r: &str| {
        let mut o = String::from(l);
        for (i, w) in widths.iter().enumerate() {
            o += &"─".repeat(w + 2);
            o += if i + 1 == widths.len() { r } else { m };
        }
        o + "\n"
    };
    let paint = |text: &str, style: &str| {
        if fmt.color && !style.is_empty() { format!("\x1b[{style}m{text}\x1b[0m") } else { text.to_string() }
    };
    let line = |cells: Vec<(&str, &str)>| {
        let mut o = String::from("│");
        for (i, (text, style)) in cells.iter().enumerate() {
            let pad = widths[i].saturating_sub(text.chars().count());
            o += &format!(" {}{} │", paint(text, style), " ".repeat(pad));
        }
        o + "\n"
    };
    let mut o = rule("┌", "┬", "┐");
    if let Some(h) = &head {
        o += &line(h.iter().map(|t| (t.as_str(), HEAD)).collect());
        o += &rule("├", "┼", "┤");
    }
    for (r, cells) in rows.iter().zip(&body) {
        let height = cells.iter().map(Vec::len).max().unwrap_or(1);
        for n in 0..height {
            o += &line(
                (0..ncols)
                    .map(|c| (cells[c].get(n).map(String::as_str).unwrap_or(""), r.get(c).map_or("", |x| x.style)))
                    .collect(),
            );
        }
    }
    o += &rule("└", "┴", "┘");
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> String {
        // Drop SGR sequences (`ESC [ … m`).
        let mut out = String::new();
        let mut it = s.chars();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for c in it.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    fn widths_of(text: &str) -> Vec<usize> {
        text.lines().map(|l| l.chars().count()).collect()
    }

    #[test]
    fn wrap_breaks_at_spaces_and_never_cuts_a_word() {
        assert_eq!(wrap("a b c", 10), ["a b c"]);
        assert_eq!(wrap("aaa bbb ccc", 7), ["aaa bbb", "ccc"]);
        assert_eq!(
            wrap("aaa /very/long/path/that/does/not/fit ccc", 10),
            ["aaa", "/very/long/path/that/does/not/fit", "ccc"]
        );
        assert_eq!(wrap("", 5), [""]);
        // Spaces in the text survive a wrap that doesn't touch them.
        assert_eq!(wrap("a  b", 10), ["a  b"]);
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip("abc", 3), "abc");
    }

    #[test]
    fn a_header_box_is_one_rectangle_and_colors_change_nothing() {
        let rows = vec![
            vec![Cell::styled("1", KEY), Cell::styled("RUNNING", GREEN)],
            vec![Cell::plain("22"), Cell::plain("a\nb")],
        ];
        let text = boxed(Some(&["id", "state"]), &rows, &Fmt::PLAIN, None);
        assert_eq!(
            text,
            "┌────┬─────────┐\n│ id │ state   │\n├────┼─────────┤\n│ 1  │ RUNNING │\n│ 22 │ a b     │\n└────┴─────────┘\n"
        );
        let painted = boxed(Some(&["id", "state"]), &rows, &Fmt { color: true, width: None }, None);
        assert!(painted.contains("\x1b[32mRUNNING\x1b[0m") && !text.contains('\x1b'));
        assert_eq!(plain(&painted), text);
        assert_eq!(boxed(Some(&[]), &[], &Fmt::PLAIN, None), "");
    }

    #[test]
    fn a_key_value_box_has_no_header_and_wraps_its_flex_column() {
        let long = "word ".repeat(30).trim_end().to_string();
        let rows = vec![
            vec![Cell::styled("name", KEY), Cell::plain("api")],
            vec![Cell::styled("restart", KEY), Cell::plain(long.clone())],
            vec![Cell::styled("env", KEY), Cell::plain("A=1\nB=2")],
        ];
        // Unwrapped: one line per row, as wide as the longest value.
        let wide = boxed(None, &rows, &Fmt::PLAIN, Some(1));
        assert!(wide.starts_with("┌─────────┬") && !wide.contains("├"), "{wide}");
        assert_eq!(wide.lines().count(), 1 + 1 + 1 + 2 + 1, "a multi-line cell is one line per line:\n{wide}");
        assert!(wide.contains(&long));
        // Wrapped to 60 columns: still one rectangle, never wider than 60, nothing lost.
        let fmt = Fmt { color: false, width: Some(60) };
        let text = boxed(None, &rows, &fmt, Some(1));
        let w = widths_of(&text);
        assert!(w.iter().all(|x| *x == w[0]) && w[0] <= 60, "{w:?}\n{text}");
        let joined: String =
            text.lines().filter_map(|l| l.split('│').nth(2)).map(str::trim).collect::<Vec<_>>().join(" ");
        assert!(joined.contains(&long), "{joined}");
        // The key shows once, on the row's first line; continuation lines are blank there.
        assert_eq!(text.matches("restart").count(), 1);
        assert!(text.lines().any(|l| l.starts_with("│         │ ")), "{text}");
        // An unbreakable value is left whole (the box is as wide as it needs).
        let path = "/a/very/long/path/".repeat(8);
        let t = boxed(None, &[vec![Cell::plain("config"), Cell::plain(path.clone())]], &fmt, Some(1));
        assert!(t.contains(&path), "{t}");
    }

    #[test]
    fn a_cell_of_lines_stacks_them_in_any_column_and_is_as_wide_as_its_longest() {
        let rows = vec![
            vec![Cell::plain("api"), Cell::lines("4000\n4001\nlocalhost:5173"), Cell::plain("ok")],
            vec![Cell::plain("web"), Cell::lines("3000"), Cell::plain("ok\nnot split")],
        ];
        let t = boxed(Some(&["name", "ports", "health"]), &rows, &Fmt::PLAIN, None);
        let lines: Vec<&str> = t.lines().collect();
        // Rule, header, rule, 3 lines for api, 1 for web, rule.
        assert_eq!(lines.len(), 1 + 1 + 1 + 3 + 1 + 1, "{t}");
        assert_eq!(lines[3], "│ api  │ 4000           │ ok           │", "{t}");
        assert_eq!(lines[4], "│      │ 4001           │              │", "{t}");
        assert_eq!(lines[5], "│      │ localhost:5173 │              │", "{t}");
        // A plain cell stays one line: its newline is a space.
        assert_eq!(lines[6], "│ web  │ 3000           │ ok not split │", "{t}");
        assert!(widths_of(&t).windows(2).all(|w| w[0] == w[1]), "every line as wide:\n{t}");
    }
}

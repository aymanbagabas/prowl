//! Dashboard rendering: the styled, aligned view is painted **directly onto an
//! `uncurses` surface** — an offscreen [`TextBuffer`](uncurses::buffer::TextBuffer)
//! for one-shot output, or the watch [`Screen`](uncurses::screen::Screen). There is
//! one painter, so the layout lives in exactly one place.
//!
//! Width math uses the surface's own [`str_width`](uncurses::text::TextSurface::str_width)
//! and column gaps are implicit (unpainted cells stay blank, so no padding is
//! emitted). Each cell's OSC-8 link rides in its style, and the surface's color
//! [`Profile`](uncurses::color::Profile) downsamples styling at encode/render
//! time — so piped output (a `Disabled` profile) degrades to plain text with no
//! special-casing here.

use crate::cli::View;
use crate::status;
use uncurses::ansi::truncate::truncate as truncate_tail;
use uncurses::buffer::{Bounded, Surface, SurfaceMut, TextBuffer, View as BufView};
use uncurses::color::{Color, Profile};
use uncurses::layout::{Position, Rect};
use uncurses::style::Style;
use uncurses::text::{Encode, TextSurface};

/// The whole dashboard is kept within this many display columns; the flexible
/// title column is truncated (with an ellipsis) to make every table fit.
pub const MAX_WIDTH: usize = 120;

/// Two blank columns separate adjacent table columns.
const SEP: usize = 2;

/// One table cell: visible text plus its style. The style carries any OSC-8
/// link (uncurses styles hold the hyperlink), so there is no separate field.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub style: Style,
}

/// A per-URL OSC-8 `id=` parameter: the first 7 hex chars of the URL's hash, so
/// terminals treat each link (e.g. a PR number) as one distinct hyperlink and
/// don't merge adjacent ones.
fn link_params(url: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let hex = format!("{:016x}", h.finish());
    format!("id={}", &hex[..7])
}

impl Cell {
    pub fn plain(text: impl Into<String>) -> Cell {
        Cell {
            text: text.into(),
            style: Style::new(),
        }
    }

    pub fn styled(text: impl Into<String>, style: impl Into<Style>) -> Cell {
        Cell {
            text: text.into(),
            style: style.into(),
        }
    }

    /// A dim + underlined OSC-8 hyperlink whose visible text is `text`.
    pub fn link(text: impl Into<String>, url: impl Into<String>) -> Cell {
        let url = url.into();
        let params = link_params(&url);
        Cell {
            text: text.into(),
            style: Style::new().faint().underline().link(url, params),
        }
    }

    /// An OSC-8 hyperlink carrying an explicit style (e.g. a colored, clickable
    /// PR number). Underlined so it reads as a link even when colored.
    pub fn link_styled(
        text: impl Into<String>,
        url: impl Into<String>,
        style: impl Into<Style>,
    ) -> Cell {
        let url = url.into();
        let params = link_params(&url);
        Cell {
            text: text.into(),
            style: style.into().underline().link(url, params),
        }
    }

    /// A styled, clickable `#<number>` PR link.
    pub fn pr(number: i64, url: impl Into<String>, style: impl Into<Style>) -> Cell {
        Cell::link_styled(format!("#{number}"), url, style)
    }
}

/// A table is a fixed header plus styled rows.
pub struct Table {
    pub header: Vec<&'static str>,
    pub rows: Vec<Vec<Cell>>,
}

/// Truncate `s` to at most `max` display columns, marking the cut with an
/// ellipsis (`\u{22ef}`, or `...` in ASCII mode). Delegates to the uncurses
/// width-aware truncator, so it counts display columns, not bytes.
pub fn truncate(s: &str, max: usize, ascii: bool) -> String {
    truncate_tail(s, max, if ascii { "..." } else { "\u{22ef}" })
}

/// The display width of table column `c`: its header and widest cell.
fn col_width(s: &impl TextSurface, table: &Table, c: usize) -> usize {
    let mut w = s.str_width(table.header[c]) as usize;
    for row in &table.rows {
        if let Some(cell) = row.get(c) {
            w = w.max(s.str_width(&cell.text) as usize);
        }
    }
    w
}

/// The width of every column of `table` except `skip`, plus the separators —
/// i.e. how wide a row is without its flexible column.
fn fixed_width(s: &impl TextSurface, table: &Table, skip: usize) -> usize {
    let cols = table.header.len();
    let total: usize = (0..cols)
        .filter(|&c| c != skip)
        .map(|c| col_width(s, table, c))
        .sum();
    total + SEP * cols.saturating_sub(1)
}

/// The shared `TITLE` column width across `tables`, capped so the widest row of
/// every table fits within [`MAX_WIDTH`]. Pass this to [`paint_table`] so the
/// section tables line up and the whole view stays within the budget.
pub fn title_width(s: &impl TextSurface, tables: &[&Table]) -> usize {
    let mut natural = 0;
    let mut fixed = 0;
    for t in tables {
        if let Some(ti) = t.header.iter().position(|h| *h == "TITLE") {
            natural = natural.max(col_width(s, t, ti));
            fixed = fixed.max(fixed_width(s, t, ti));
        }
    }
    natural.min(MAX_WIDTH.saturating_sub(fixed))
}

/// Paint `table` onto `s` starting at row `top`, forcing the `TITLE` column to
/// `title_w` columns when present (titles longer than that are ellipsized).
/// Columns are left-aligned and separated by two blank columns; the header row
/// is bold. Returns the next free row.
pub fn paint_table(
    s: &mut impl TextSurface,
    table: &Table,
    title_w: usize,
    ascii: bool,
    top: u16,
) -> u16 {
    let cols = table.header.len();
    let title_idx = table.header.iter().position(|h| *h == "TITLE");

    let mut widths: Vec<usize> = (0..cols).map(|c| col_width(s, table, c)).collect();
    if let Some(ti) = title_idx {
        widths[ti] = title_w;
    }

    // Column start positions: running sum of widths plus the separators.
    let mut xs = vec![0u16; cols];
    let mut acc = 0usize;
    for i in 0..cols {
        xs[i] = acc as u16;
        acc += widths[i] + SEP;
    }

    let bold = Style::new().bold();
    for (i, h) in table.header.iter().enumerate() {
        if !h.is_empty() {
            s.set_str((xs[i], top), h, &bold);
        }
    }

    for (r, row) in table.rows.iter().enumerate() {
        let y = top + 1 + r as u16;
        for (i, cell) in row.iter().enumerate() {
            let text = if Some(i) == title_idx {
                truncate(&cell.text, widths[i], ascii)
            } else {
                cell.text.clone()
            };
            s.set_str((xs[i], y), &text, &cell.style);
        }
    }
    top + 1 + table.rows.len() as u16
}

/// Paint `table` alone onto an offscreen buffer and encode it to a string,
/// either styled (SGR + OSC-8) or plain. Plain output implies ASCII mode, as it
/// does for the dashboard. A convenience for checking a single section's output
/// without standing up a whole dashboard.
#[must_use]
pub fn render_table(table: &Table, styled: bool) -> String {
    let height = table.rows.len() as u16 + 1;
    let mut buf = TextBuffer::new(MAX_WIDTH as u16, height);
    let title_w = title_width(&buf, &[table]);
    paint_table(&mut buf, table, title_w, !styled, 0);
    let mut out = Vec::new();
    let profile = if styled {
        Profile::TrueColor
    } else {
        Profile::Disabled
    };
    buf.encode_with(&mut out, profile)
        .expect("encoding to a Vec cannot fail");
    String::from_utf8(out).expect("uncurses encodes valid UTF-8")
}

/// Paint a dim one-liner (an empty-section placeholder, the error line) at row
/// `y`. Returns y + 1.
pub fn paint_dim(s: &mut impl TextSurface, msg: &str, y: u16) -> u16 {
    s.set_str((0, y), msg, Style::new().faint());
    y + 1
}

/// Paint a section header at row `y`: a colored bold accent bar, the title, an
/// optional dim count badge (or `Title (count)` in ASCII mode), and an optional
/// dim trailing note (e.g. the merge-queue ETA). Returns y + 1.
pub fn paint_header(
    s: &mut impl TextSurface,
    title: &str,
    accent: Color,
    count: Option<&str>,
    note: Option<&str>,
    ascii: bool,
    y: u16,
) -> u16 {
    let dim = Style::new().faint();
    let mut end = if ascii {
        let text = match count {
            Some(c) => format!("{title} ({c})"),
            None => title.to_string(),
        };
        s.set_str((0, y), &text, None)
    } else {
        let end = s.set_str(
            (0, y),
            &format!("\u{258c} {title}"),
            status::fg(accent).bold(),
        );
        match count {
            Some(c) => s.set_str((end.x + 2, y), c, &dim),
            None => end,
        }
    };
    if let Some(n) = note {
        end = s.set_str((end.x + 2, y), n, &dim);
    }
    let _ = end;
    y + 1
}

/// A status glyph cell: the Nerd Font glyph (or ASCII letter) in the status's
/// palette color.
pub fn status_cell(status: status::Status, ascii: bool) -> Cell {
    Cell::styled(
        status::glyph(status, ascii).to_string(),
        status::fg(status::status_style(status).1),
    )
}

/// A leading cell marking a row that changed since the previous refresh.
pub fn change_marker(highlighted: bool, ascii: bool) -> Cell {
    if highlighted {
        let m = if ascii { ">" } else { "\u{25b8}" };
        Cell::styled(m, status::fg(status::PINK).bold())
    } else {
        Cell::plain(" ")
    }
}

/// The selection caret for the row the navigation cursor is on. It sits in the
/// same leading column as the change marker, overriding it when a row is both
/// changed and selected.
pub fn select_marker(ascii: bool) -> Cell {
    let m = if ascii { ">" } else { "\u{276f}" };
    Cell::styled(m, status::fg(status::PEACH).bold())
}

/// Paint the watch-mode key-hint footer at row `y`, folding the constant
/// refresh interval into the refresh hint: `r refresh (every 5m) - tab switch
/// view - enter open - / search - ? help`. While a refresh is in flight the
/// first hint becomes `r refreshing` (the interval is dropped and the `r` glyph
/// is dimmed, since `r` is inert until the fetch finishes). Each key glyph is a
/// bold muted accent, its labels dim; plain in ASCII mode. Returns y + 1.
pub fn paint_footer(
    s: &mut impl TextSurface,
    interval: &str,
    refreshing: bool,
    ascii: bool,
    y: u16,
) -> u16 {
    let refresh = if refreshing {
        "refreshing".to_string()
    } else {
        format!("refresh (every {interval})")
    };
    let hints = [
        ("r", refresh.as_str()),
        ("tab", "switch view"),
        ("enter", "open"),
        ("/", "search"),
        ("?", "help"),
    ];
    if ascii {
        let line = hints
            .iter()
            .map(|(k, l)| format!("{k} {l}"))
            .collect::<Vec<_>>()
            .join(" - ");
        s.set_str((0, y), &line, None);
        return y + 1;
    }
    let key = status::fg(status::OVERLAY).bold();
    let dim = Style::new().faint();
    let mut x = 0u16;
    for (i, (k, label)) in hints.iter().enumerate() {
        if i > 0 {
            x = s.set_str((x, y), " - ", &dim).x;
        }
        // `r` is inert while a fetch is in flight, so its glyph fades to dim.
        let kstyle = if i == 0 && refreshing { &dim } else { &key };
        let p = s.set_str((x, y), k, kstyle);
        x = s.set_str((p.x + 1, y), label, &dim).x;
    }
    y + 1
}

/// Paint the view switcher at row `y`: both view names, the active one accented
/// with a section-style bar, the other dim (the active one is bracketed in ASCII
/// mode). Returns y + 1.
pub fn paint_tabs(s: &mut impl TextSurface, view: View, ascii: bool, y: u16) -> u16 {
    let names = [(View::Mine, "my PRs"), (View::Reviews, "reviews")];
    let bar = status::fg(status::LAVENDER).bold();
    let dim = Style::new().faint();
    let mut x = 0u16;
    for (v, n) in names {
        let active = v == view;
        x = if ascii {
            let text = if active {
                format!("[{n}]")
            } else {
                n.to_string()
            };
            s.set_str((x, y), &text, None).x
        } else if active {
            s.set_str((x, y), &format!("\u{258c}{n}"), &bar).x
        } else {
            s.set_str((x, y), n, &dim).x
        };
        x += 2;
    }
    y + 1
}

/// Paint the search prompt at row `y`: an accented `/`, the query, and a dim
/// match count. Returns the next free row and the cell just past the query — the
/// caret position, for the caller to park the terminal's own cursor on while the
/// prompt is capturing (this paints no cursor of its own).
pub fn paint_search_prompt(
    s: &mut impl TextSurface,
    query: &str,
    matches: usize,
    ascii: bool,
    y: u16,
) -> (u16, Position) {
    let count = if matches == 1 {
        "1 match".to_string()
    } else {
        format!("{matches} matches")
    };
    let prompt = format!("/{query}");
    let style = if ascii {
        Style::new()
    } else {
        status::fg(status::PEACH).bold()
    };
    let caret = s.set_str((0, y), &prompt, style);
    // Two blank columns keep the count clear of the cursor's cell.
    s.set_str(
        (caret.x + 2, y),
        &format!("({count})"),
        Style::new().faint(),
    );
    (y + 1, caret)
}

/// Compose one screen of exactly `rows` rows: as much of `body` as fits at the
/// top, then `bottom` glued to the last rows. When the body is taller than the
/// space left over it scrolls, keeping `caret` (a row index into `body`) centered
/// in view. `screen` is assumed already cleared, so the gap between the two is
/// blank padding. Returns the row the bottom block starts on, which callers need
/// to translate positions inside it (the search caret) into screen coordinates.
pub fn compose<T: SurfaceMut + Bounded + ?Sized>(
    screen: &mut T,
    body: &mut TextBuffer,
    body_h: u16,
    bottom: &mut TextBuffer,
    bottom_h: u16,
    rows: u16,
    caret: Option<u16>,
) -> u16 {
    // The bottom block wins the space it needs; if it alone overflows (a short
    // terminal with the help legend open) it keeps its head — the search prompt,
    // error line and footer — and the legend below them is what gets cut.
    let shown_bottom = bottom_h.min(rows);
    let avail = rows - shown_bottom;

    // Centering the caret scrolls the body a row at a time in either direction;
    // with no caret (or nothing to scroll) we sit at the top.
    let off = match caret {
        Some(c) if body_h > avail => c.saturating_sub(avail / 2).min(body_h - avail),
        _ => 0,
    };
    if avail > 0 {
        // A `View` clips without translating, so drawing it maps its top-left —
        // the first visible body row — onto the top of the screen.
        let w = body.width();
        BufView::new(body, Rect::new(0, off, w, avail)).draw(screen, Position::new(0, 0));
    }
    let top = rows - shown_bottom;
    bottom.draw(screen, Position::new(0, top));
    top
}

/// Paint one indented `glyph  meaning` legend row at `y`: the glyph in `gstyle`,
/// two blank columns, then the meaning in `dim`.
fn legend_row(
    s: &mut impl TextSurface,
    glyph: &str,
    gstyle: Style,
    meaning: &str,
    dim: &Style,
    y: u16,
) {
    let p = s.set_str((2, y), glyph, gstyle);
    s.set_str((p.x + 2, y), meaning, dim);
}

/// Paint the help legend for `view` at row `top`: the navigation keys, then only
/// the glyphs and values that view actually uses, so a glyph the other view
/// reuses for something else can't muddy it. The Mine view lists the status
/// glyphs + every `mergeStateStatus` value; the Reviews view lists the
/// review-state glyphs + the merged glyph (its only shared icon). Returns the
/// next free row.
pub fn paint_help(s: &mut impl TextSurface, view: View, ascii: bool, top: u16) -> u16 {
    let dim = Style::new().faint();
    let mut y = paint_header(s, "Help", status::OVERLAY, None, None, ascii, top);

    // The footer only lists the action keys, so document the movement cursor here.
    let sep = if ascii { " | " } else { "  \u{b7}  " };
    let keys =
        format!("j/k move{sep}g/G first/last{sep}^D/^U half page{sep}enter open{sep}/ filter");
    s.set_str((2, y), &keys, &dim);
    y += 1;

    match view {
        View::Mine => {
            for st in status::ORDER {
                let glyph = status::glyph(st, ascii).to_string();
                let color = status::fg(status::status_style(st).1);
                legend_row(s, &glyph, color, status::status_meaning(st), &dim, y);
                y += 1;
            }
            s.set_str((2, y), "- no checks reported yet", &dim);
            y += 1;

            for st in status::STATE_ORDER {
                let meaning = status::state_meaning(st);
                let c = status::state_style(st);
                if ascii {
                    // Label form (matches the ASCII/piped STATE column).
                    let p = s.set_str((2, y), status::state_label(st), c);
                    if !meaning.is_empty() {
                        s.set_str((p.x, y), &format!(" \u{2014} {meaning}"), &dim);
                    }
                } else {
                    // Glyph form (matches the Nerd Font STATE column).
                    legend_row(s, &status::state_glyph(st).to_string(), c, meaning, &dim, y);
                }
                y += 1;
            }
        }
        View::Reviews => {
            for r in status::REVIEW_ORDER {
                let glyph = status::review_glyph(r, ascii).to_string();
                let color = status::fg(status::review_style(r).1);
                legend_row(s, &glyph, color, status::review_meaning(r), &dim, y);
                y += 1;
            }
            // The "Reviewed & merged" section leads each row with the merged glyph.
            let merged = status::Status::Merged;
            let glyph = status::glyph(merged, ascii).to_string();
            let color = status::fg(status::status_style(merged).1);
            legend_row(s, &glyph, color, status::status_meaning(merged), &dim, y);
            y += 1;
        }
    }
    y
}

/// The number of legend rows [`paint_help`] paints for `view` (header + the key
/// line + one row per entry), so callers can size a surface before painting.
pub fn help_height(view: View) -> usize {
    2 + match view {
        View::Mine => status::ORDER.len() + 1 + status::STATE_ORDER.len(),
        View::Reviews => status::REVIEW_ORDER.len() + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uncurses::buffer::TextBuffer;
    use uncurses::color::Profile;
    use uncurses::text::Encode;

    /// Paint `f` into a fresh buffer and return its encoded form at `profile`.
    fn encode(
        width: u16,
        height: u16,
        profile: Profile,
        f: impl FnOnce(&mut TextBuffer),
    ) -> String {
        let mut canvas = TextBuffer::new(width, height);
        f(&mut canvas);
        let mut out = Vec::new();
        canvas.encode_with(&mut out, profile).unwrap();
        String::from_utf8(out).unwrap()
    }

    /// Paint one text row per line of `lines` into a buffer of that height.
    fn buf(lines: &[&str]) -> (TextBuffer, u16) {
        let mut b = TextBuffer::new(8, lines.len().max(1) as u16);
        for (y, l) in lines.iter().enumerate() {
            b.set_str((0, y as u16), l, None);
        }
        (b, lines.len() as u16)
    }

    /// Compose onto a `rows`-tall screen and read the result back as trimmed rows.
    fn screen_rows(body: &[&str], bottom: &[&str], rows: u16, caret: Option<u16>) -> Vec<String> {
        let (mut b, bh) = buf(body);
        let (mut bot, both) = buf(bottom);
        let mut screen = TextBuffer::new(8, rows);
        compose(&mut screen, &mut b, bh, &mut bot, both, rows, caret);
        let mut out: Vec<String> = screen
            .display_with(Profile::Disabled)
            .to_string()
            .lines()
            .map(|l| l.trim_end().to_string())
            .collect();
        // The display drops trailing all-blank rows; the buffer still has them.
        out.resize(rows as usize, String::new());
        out
    }

    #[test]
    fn compose_pins_the_bottom_block_to_the_last_rows() {
        // A short body is padded out so the footer lands on the last row.
        assert_eq!(
            screen_rows(&["a", "b"], &["footer"], 6, None),
            ["a", "b", "", "", "", "footer"]
        );
    }

    #[test]
    fn compose_scrolls_the_body_to_keep_the_caret_in_view() {
        let body: Vec<String> = (0..20).map(|i| i.to_string()).collect();
        let body: Vec<&str> = body.iter().map(String::as_str).collect();
        // 6 rows, one of them the footer, leaves 5 for the body.
        let five = |caret| screen_rows(&body, &["footer"], 6, caret);

        // With no selection the body starts at the top...
        assert_eq!(five(None), ["0", "1", "2", "3", "4", "footer"]);
        // ...a caret past the fold scrolls into view, centered...
        assert_eq!(five(Some(10)), ["8", "9", "10", "11", "12", "footer"]);
        // ...and the last row can't scroll past the end of the body.
        assert_eq!(five(Some(19)), ["15", "16", "17", "18", "19", "footer"]);
    }

    #[test]
    fn compose_keeps_the_footer_when_the_bottom_block_overflows() {
        // Too short for both: the body goes, and the bottom block is cut from
        // the end so its first lines — up to and including the footer — stay.
        assert_eq!(
            screen_rows(&["a", "b"], &["footer", "h1", "h2"], 2, None),
            ["footer", "h1"]
        );
    }

    #[test]
    fn compose_never_panics_and_always_keeps_the_footer() {
        // Degenerate shapes must not panic and must never drop the head of the
        // bottom block, where the footer lives.
        let bodies: [&[&str]; 4] = [&[], &["a"], &["a", "b", "c", "d", "e", "f"], &["x", "", ""]];
        let bottoms: [&[&str]; 3] = [&[], &["f"], &["f", "h1", "h2", "h3"]];
        for body in bodies {
            for bottom in bottoms {
                for rows in 1..12u16 {
                    for caret in [None, Some(0), Some(1), Some(5), Some(100)] {
                        let out = screen_rows(body, bottom, rows, caret);
                        assert_eq!(out.len(), rows as usize, "{body:?}/{bottom:?}/{rows}");
                        if let Some(footer) = bottom.first() {
                            assert!(out.contains(&(*footer).to_string()));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn search_prompt_reports_the_caret_and_paints_none() {
        let mut canvas = TextBuffer::new(40, 1);
        let (next, caret) = paint_search_prompt(&mut canvas, "café", 1, false, 0);
        assert_eq!(next, 1);
        // Just past `/café` — width, not byte length.
        assert_eq!(caret.x, 5);
        let mut out = Vec::new();
        canvas.encode_with(&mut out, Profile::TrueColor).unwrap();
        let s = String::from_utf8(out).unwrap();
        assert!(s.contains("/café"));
        assert!(s.contains("(1 match)"));
        // No painted stand-in cursor: nothing is reverse-video.
        assert!(!s.contains("\x1b[7m"));
    }

    #[test]
    fn plain_table_aligns_and_pads() {
        let table = Table {
            header: vec!["PR", "TITLE"],
            rows: vec![
                vec![Cell::plain("#1"), Cell::plain("short")],
                vec![Cell::plain("#42"), Cell::plain("x")],
            ],
        };
        let out = encode(20, 3, Profile::Disabled, |b| {
            paint_table(b, &table, 5, true, 0);
        });
        // Header, then two rows; columns line up by display width, no escapes.
        assert_eq!(out, "PR   TITLE\r\n#1   short\r\n#42  x");
    }

    #[test]
    fn padding_uses_display_width_for_glyphs() {
        // The check-circle glyph is one display column but several bytes; the
        // following column must still line up by display width.
        let glyph = status::status_style(status::Status::Pass).0;
        let table = Table {
            header: vec!["ST", "PR"],
            rows: vec![
                vec![Cell::plain(glyph.to_string()), Cell::plain("#1")],
                vec![Cell::plain("xx"), Cell::plain("#2")],
            ],
        };
        let out = encode(10, 3, Profile::Disabled, |b| {
            paint_table(b, &table, 0, true, 0);
        });
        let lines: Vec<&str> = out.split("\r\n").collect();
        let col = |line: &str| line.find('#').map_or("", |i| &line[..i]).chars().count();
        // The "#" starts at the same display column on both rows.
        assert_eq!(col(lines[1]), col(lines[2]));
    }

    #[test]
    fn styled_url_is_an_osc8_hyperlink() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = encode(12, 2, Profile::TrueColor, |b| {
            paint_table(b, &table, 0, false, 0);
        });
        // OSC-8 framing: the opener carries a per-URL `id=` param, the closer is
        // empty; dim + underline SGR.
        assert!(out.contains("\x1b]8;id="));
        assert!(out.contains(";https://x/1\x1b\\"));
        assert!(out.contains("\x1b]8;;\x1b\\"));
        assert!(out.contains("\x1b[2;4m"));
    }

    #[test]
    fn link_params_is_a_7_hex_char_id() {
        let p = link_params("https://github.com/o/r/pull/42");
        let id = p.strip_prefix("id=").expect("id= prefix");
        assert_eq!(id.len(), 7);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn disabled_profile_drops_styling_and_links() {
        let table = Table {
            header: vec!["URL"],
            rows: vec![vec![Cell::link("https://x/1", "https://x/1")]],
        };
        let out = encode(12, 2, Profile::Disabled, |b| {
            paint_table(b, &table, 0, false, 0);
        });
        assert!(!out.contains('\x1b'));
        assert!(out.contains("https://x/1"));
    }

    #[test]
    fn footer_is_plain_or_styled_key_hints() {
        let plain = encode(80, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", false, true, 0);
        });
        assert_eq!(
            plain,
            "r refresh (every 5m) - tab switch view - enter open - / search - ? help"
        );

        // While a refresh is in flight the first hint says so instead.
        let refreshing = encode(80, 1, Profile::Disabled, |b| {
            paint_footer(b, "5m", true, true, 0);
        });
        assert!(refreshing.starts_with("r refreshing"));

        let styled = encode(80, 1, Profile::TrueColor, |b| {
            paint_footer(b, "5m", false, false, 0);
        });
        assert!(styled.contains("refresh (every 5m)"));
        assert!(styled.contains("help"));
        // Bold key accent (combined with the muted color) and a dim label.
        assert!(styled.contains("\x1b[1;"));
        assert!(styled.contains("\x1b[2m"));
    }

    #[test]
    fn truncate_marks_cut_with_ellipsis() {
        assert_eq!(truncate("short", 10, false), "short");
        assert_eq!(truncate("hello world", 8, false), "hello w\u{22ef}");
        assert_eq!(truncate("hello world", 8, true), "hello...");
    }

    #[test]
    fn title_column_is_capped_to_max_width() {
        let long = "x".repeat(200);
        let table = Table {
            header: vec!["", "PR", "TITLE", "BASE"],
            rows: vec![vec![
                Cell::plain(" "),
                Cell::plain("#1"),
                Cell::plain(long),
                Cell::plain("main"),
            ]],
        };
        let mut canvas = TextBuffer::new(MAX_WIDTH as u16, 2);
        let tw = title_width(&canvas, &[&table]);
        paint_table(&mut canvas, &table, tw, false, 0);
        let out = canvas.display_with(Profile::Disabled).to_string();
        for line in out.split("\r\n") {
            assert!(line.chars().count() <= MAX_WIDTH, "line exceeds MAX_WIDTH");
        }
        // The title was truncated with the ellipsis.
        assert!(out.contains('\u{22ef}'));
    }
}

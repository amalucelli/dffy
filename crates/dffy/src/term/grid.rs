//! The screen as a grid of styled cells, double-buffered: each flush sends
//! the terminal only the cells that changed since the previous frame, inside
//! one synchronized update.

use std::fmt::Write as _;
use std::io::Write as _;

use unicode_width::UnicodeWidthChar;

use crate::theme::{ATTRIBUTES, Style};

/// UTF-8 bytes a cell holds: a character plus a few combining marks.
const CELL_BYTES: usize = 14;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cell {
    bytes: [u8; CELL_BYTES],
    len: u8,
    /// 1, or 2 for the left half of a wide character; 0 marks the right half.
    width: u8,
    style: Style,
}

impl Cell {
    const BLANK: Self = Self::blank(Style::PLAIN);

    const fn blank(style: Style) -> Self {
        let mut bytes = [0; CELL_BYTES];
        bytes[0] = b' ';
        Self {
            bytes,
            len: 1,
            width: 1,
            style,
        }
    }

    fn new(c: char, width: u8, style: Style) -> Self {
        let mut bytes = [0; CELL_BYTES];
        c.encode_utf8(&mut bytes);
        Self {
            bytes,
            len: utf8_len(c),
            width,
            style,
        }
    }

    fn continuation(style: Style) -> Self {
        Self {
            bytes: [0; CELL_BYTES],
            len: 0,
            width: 0,
            style,
        }
    }

    pub(crate) fn text(&self) -> &str {
        std::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or(" ")
    }

    #[cfg(test)]
    pub(crate) fn style(&self) -> Style {
        self.style
    }
}

fn utf8_len(c: char) -> u8 {
    u8::try_from(c.len_utf8()).unwrap_or(4)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Grid {
    width: usize,
    height: usize,
    cells: Vec<Cell>,
}

impl Grid {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            width,
            height,
            cells: vec![Cell::BLANK; width * height],
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.width
    }

    pub(crate) fn height(&self) -> usize {
        self.height
    }

    pub(crate) fn clear(&mut self) {
        self.cells.fill(Cell::BLANK);
    }

    pub(crate) fn row(&self, y: usize) -> &[Cell] {
        &self.cells[y * self.width..(y + 1) * self.width]
    }

    /// Fills columns `x..end` of row `y` with blanks in `style`.
    pub(crate) fn fill(&mut self, x: usize, y: usize, end: usize, style: Style) {
        for x in x..end.min(self.width) {
            self.set(x, y, Cell::blank(style));
        }
    }

    /// Flips `attributes` on the cells in columns `x..end` of row `y`, so
    /// reverse video over text already reversed shows it plain again.
    pub(crate) fn toggle(&mut self, x: usize, y: usize, end: usize, attributes: u8) {
        if y >= self.height {
            return;
        }
        let row = y * self.width;
        for cell in &mut self.cells[row + x.min(self.width)..row + end.min(self.width)] {
            cell.style.attributes ^= attributes;
        }
    }

    /// Gives the cells in columns `x..end` of row `y` `style`, keeping
    /// their text.
    pub(crate) fn restyle(&mut self, x: usize, y: usize, end: usize, style: Style) {
        if y >= self.height {
            return;
        }
        let row = y * self.width;
        for cell in &mut self.cells[row + x.min(self.width)..row + end.min(self.width)] {
            cell.style = style;
        }
    }

    /// Passes the style of the cells in columns `x..end` of row `y`
    /// through `f`, keeping their text.
    pub(crate) fn map_styles(
        &mut self,
        x: usize,
        y: usize,
        end: usize,
        f: impl Fn(Style) -> Style,
    ) {
        if y >= self.height {
            return;
        }
        let row = y * self.width;
        for cell in &mut self.cells[row + x.min(self.width)..row + end.min(self.width)] {
            cell.style = f(cell.style);
        }
    }

    /// Writes `c` at column `x` of row `y`, left of column `end`, and
    /// returns the column after it. A zero-width character joins the cell
    /// before it; a wide character that doesn't fit becomes a blank.
    pub(crate) fn put_char(
        &mut self,
        x: usize,
        y: usize,
        end: usize,
        c: char,
        style: Style,
    ) -> usize {
        let end = end.min(self.width);
        if y >= self.height || x >= end {
            return x;
        }
        let Some(w) = c.width() else {
            return x;
        };
        if w == 0 {
            self.join(x, y, c);
            return x;
        }
        if x + w > end {
            self.set(x, y, Cell::blank(style));
            return x + 1;
        }
        let wide = w == 2;
        self.set(x, y, Cell::new(c, if wide { 2 } else { 1 }, style));
        if wide {
            self.set(x + 1, y, Cell::continuation(style));
        }
        x + w
    }

    pub(crate) fn put_str(
        &mut self,
        mut x: usize,
        y: usize,
        end: usize,
        s: &str,
        style: Style,
    ) -> usize {
        for c in s.chars() {
            x = self.put_char(x, y, end, c, style);
        }
        x
    }

    /// Appends a combining character to the cell left of column `x`.
    fn join(&mut self, x: usize, y: usize, c: char) {
        let row = y * self.width;
        let Some(mut p) = x.checked_sub(1) else {
            return;
        };
        if self.cells[row + p].width == 0 && p > 0 {
            p -= 1;
        }
        let cell = &mut self.cells[row + p];
        let len = usize::from(cell.len);
        if cell.width > 0 && len + c.len_utf8() <= CELL_BYTES {
            c.encode_utf8(&mut cell.bytes[len..]);
            cell.len += utf8_len(c);
        }
    }

    /// Stores a cell, blanking the other half of any wide character it
    /// overwrites half of.
    fn set(&mut self, x: usize, y: usize, cell: Cell) {
        let i = y * self.width + x;
        let old = self.cells[i];
        if old.width == 0 && cell.width != 0 && x > 0 {
            self.cells[i - 1] = Cell::blank(self.cells[i - 1].style);
        }
        if old.width == 2 && cell.width != 2 && x + 1 < self.width {
            self.cells[i + 1] = Cell::blank(self.cells[i + 1].style);
        }
        self.cells[i] = cell;
    }

    /// Row `y` as plain text, for tests.
    #[cfg(test)]
    pub(crate) fn row_text(&self, y: usize) -> String {
        self.row(y).iter().map(Cell::text).collect()
    }
}

/// Begin and end of a synchronized update (DEC mode 2026): the terminal
/// shows the frame at once instead of as it arrives. Terminals without it
/// ignore both.
const SYNC_BEGIN: &[u8] = b"\x1b[?2026h";
const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Two grids: the one the terminal shows and the one being drawn.
#[derive(Debug)]
pub(crate) struct Screen {
    shown: Grid,
    next: Grid,
    /// What the terminal shows is unknown (first frame, resize, a child
    /// process had the terminal): clear it before drawing.
    stale: bool,
    out: Vec<u8>,
}

impl Screen {
    pub(crate) fn new(width: usize, height: usize) -> Self {
        Self {
            shown: Grid::new(width, height),
            next: Grid::new(width, height),
            stale: true,
            out: Vec::new(),
        }
    }

    pub(crate) fn width(&self) -> usize {
        self.next.width
    }

    pub(crate) fn height(&self) -> usize {
        self.next.height
    }

    pub(crate) fn resize(&mut self, width: usize, height: usize) {
        self.shown = Grid::new(width, height);
        self.next = Grid::new(width, height);
        self.stale = true;
    }

    pub(crate) fn invalidate(&mut self) {
        self.stale = true;
    }

    /// The cells the terminal shows after the last flush, for tests.
    #[cfg(test)]
    pub(crate) fn shown_grid(&self) -> &Grid {
        &self.shown
    }

    /// What the terminal shows after the last flush, for tests.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        (0..self.shown.height)
            .map(|y| self.shown.row_text(y).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A blank grid to draw the next frame into.
    pub(crate) fn frame(&mut self) -> &mut Grid {
        self.next.clear();
        &mut self.next
    }

    /// The bytes that bring the terminal to the frame drawn since
    /// [`Screen::frame`]; empty when nothing changed.
    pub(crate) fn flush(&mut self) -> &[u8] {
        self.out.clear();
        self.out.extend_from_slice(SYNC_BEGIN);
        if self.stale {
            self.shown.clear();
            self.out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
        }
        let before = self.out.len();
        diff(&self.shown, &self.next, &mut self.out);
        if !self.stale && self.out.len() == before {
            self.out.clear();
        } else {
            self.out.extend_from_slice(SYNC_END);
        }
        std::mem::swap(&mut self.shown, &mut self.next);
        self.stale = false;
        &self.out
    }
}

/// Unchanged cells shorter than this between two changed ones are written
/// again rather than skipped, when that's cheaper than a cursor move.
const MAX_REWRITE_GAP: usize = 4;

/// Appends the bytes that turn a terminal showing `old` into one showing
/// `new`, which has the same size. The SGR state is the default on entry
/// and on return.
pub(super) fn diff(old: &Grid, new: &Grid, out: &mut Vec<u8>) {
    let width = new.width;
    let mut cursor: Option<(usize, usize)> = None;
    let mut pen = Style::PLAIN;
    let mut sgr = String::new();
    for y in 0..new.height {
        let (o, n) = (old.row(y), new.row(y));
        if o == n {
            continue;
        }
        // From here on the new row is blank, so one erase covers it.
        let tail = n
            .iter()
            .rposition(|c| *c != Cell::BLANK)
            .map_or(0, |i| i + 1);
        let mut x = 0;
        while x < width {
            if n[x] == o[x] || n[x].width == 0 {
                x += 1;
                continue;
            }
            match cursor {
                Some((cx, cy))
                    if cy == y
                        && cx < x
                        && x - cx <= MAX_REWRITE_GAP
                        && n[cx..x]
                            .iter()
                            .all(|c| c.width == 1 && c.len == 1 && c.style == pen) =>
                {
                    for c in &n[cx..x] {
                        out.extend_from_slice(c.text().as_bytes());
                    }
                }
                _ => move_to(out, cursor, x, y),
            }
            if x >= tail {
                set_pen(out, &mut pen, Style::PLAIN, &mut sgr);
                out.extend_from_slice(b"\x1b[K");
                cursor = Some((x, y));
                break;
            }
            set_pen(out, &mut pen, n[x].style, &mut sgr);
            out.extend_from_slice(n[x].text().as_bytes());
            x += usize::from(n[x].width);
            // Past the last column the cursor position depends on the
            // terminal's wrap handling, so it's treated as unknown.
            cursor = (x < width).then_some((x, y));
        }
    }
    set_pen(out, &mut pen, Style::PLAIN, &mut sgr);
}

fn move_to(out: &mut Vec<u8>, cursor: Option<(usize, usize)>, x: usize, y: usize) {
    // Writing to a Vec can't fail.
    match cursor {
        Some((cx, cy)) if cy == y && cx == x => Ok(()),
        Some((cx, cy)) if cy == y && cx + 1 == x => out.write_all(b"\x1b[C"),
        Some((cx, cy)) if cy == y && cx < x => write!(out, "\x1b[{}C", x - cx),
        Some((_, cy)) if cy + 1 == y && x == 0 => out.write_all(b"\r\n"),
        _ if x == 0 => write!(out, "\x1b[{}H", y + 1),
        _ => write!(out, "\x1b[{};{}H", y + 1, x + 1),
    }
    .ok();
}

/// Switches the terminal's SGR state from `pen` to `to`. Turning an
/// attribute off needs a reset (bold and dim share one "off" code), and so
/// does adding bold or dim to a style holding both: terminals with a single
/// intensity keep whichever came last, so the pair always goes out in one
/// order. Anything else changes only what differs.
fn set_pen(out: &mut Vec<u8>, pen: &mut Style, to: Style, sgr: &mut String) {
    const INTENSITY: u8 = 0b11;
    if *pen == to {
        return;
    }
    sgr.clear();
    let intensity_added =
        to.attributes & INTENSITY == INTENSITY && pen.attributes & INTENSITY != INTENSITY;
    let from = if pen.attributes & !to.attributes != 0 || to == Style::PLAIN || intensity_added {
        sgr.push('0');
        Style::PLAIN
    } else {
        *pen
    };
    for (i, (_, code)) in ATTRIBUTES.iter().enumerate() {
        if to.attributes & !from.attributes & (1 << i) != 0 {
            let _ = write!(sgr, ";{code}");
        }
    }
    if to.fg != from.fg {
        match to.fg {
            Some(c) => c.sgr(false, sgr),
            None => sgr.push_str(";39"),
        }
    }
    if to.bg != from.bg {
        match to.bg {
            Some(c) => c.sgr(true, sgr),
            None => sgr.push_str(";49"),
        }
    }
    *pen = to;
    out.extend_from_slice(b"\x1b[");
    out.extend_from_slice(sgr.trim_start_matches(';').as_bytes());
    out.push(b'm');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Color;

    const GREEN: Style = Style {
        fg: Some(Color::Ansi(2)),
        bg: None,
        attributes: 0,
    };
    const BOLD: u8 = 1;
    const GREEN_BOLD: Style = Style {
        attributes: BOLD,
        ..GREEN
    };

    fn grid(width: usize, rows: &[&str]) -> Grid {
        let mut g = Grid::new(width, rows.len());
        for (y, row) in rows.iter().enumerate() {
            g.put_str(0, y, width, row, Style::PLAIN);
        }
        g
    }

    fn diffed(old: &Grid, new: &Grid) -> String {
        let mut out = Vec::new();
        diff(old, new, &mut out);
        String::from_utf8(out).expect("utf-8")
    }

    #[test]
    fn unchanged_frames_emit_nothing_and_changes_are_synchronized() {
        let mut screen = Screen::new(5, 2);
        screen.frame().put_str(0, 0, 5, "hi", Style::PLAIN);
        assert_eq!(
            screen.flush(),
            b"\x1b[?2026h\x1b[0m\x1b[H\x1b[2J\x1b[1Hhi\x1b[?2026l"
        );
        screen.frame().put_str(0, 0, 5, "hi", Style::PLAIN);
        assert_eq!(screen.flush(), b"");
        screen.frame().put_str(0, 1, 5, "yo", Style::PLAIN);
        assert_eq!(screen.flush(), b"\x1b[?2026h\x1b[1H\x1b[K\r\nyo\x1b[?2026l");
    }

    #[test]
    fn one_changed_cell_is_one_move_and_one_character() {
        let old = grid(5, &["hello", "world"]);
        let new = grid(5, &["hello", "wOrld"]);
        assert_eq!(diffed(&old, &new), "\x1b[2;2HO");
    }

    #[test]
    fn short_gaps_are_rewritten_and_long_ones_skipped() {
        let old = grid(16, &["abcdefghijklmnop"]);
        let new = grid(16, &["XbcDefghijklMnop"]);
        assert_eq!(diffed(&old, &new), "\x1b[1HXbcD\x1b[8CM");
    }

    #[test]
    fn styles_change_only_what_differs() {
        let old = Grid::new(4, 1);
        let mut new = Grid::new(4, 1);
        new.put_char(0, 0, 4, 'a', GREEN);
        new.put_char(1, 0, 4, 'b', GREEN_BOLD);
        new.put_char(2, 0, 4, 'c', GREEN);
        assert_eq!(
            diffed(&old, &new),
            "\x1b[1H\x1b[32ma\x1b[1mb\x1b[0;32mc\x1b[0m"
        );
    }

    /// Terminals with one intensity state keep the last of bold and dim, so
    /// bold and dim together are always sent in the same order, whatever
    /// the pen held before.
    #[test]
    fn bold_and_dim_together_go_out_in_one_order() {
        const DIM: u8 = 2;
        let old = Grid::new(2, 1);
        let mut new = Grid::new(2, 1);
        let style = |attributes| Style {
            attributes,
            ..Style::PLAIN
        };
        new.put_char(0, 0, 2, 'a', style(DIM));
        new.put_char(1, 0, 2, 'b', style(BOLD | DIM));
        assert_eq!(diffed(&old, &new), "\x1b[1H\x1b[2ma\x1b[0;1;2mb\x1b[0m");
    }

    #[test]
    fn a_shortened_row_ends_with_one_erase() {
        let old = grid(11, &["hello world"]);
        let new = grid(11, &["hi"]);
        assert_eq!(diffed(&old, &new), "\x1b[1;2Hi\x1b[K");
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut g = grid(5, &["日本"]);
        assert_eq!(g.row_text(0), "日本 ");
        let before = g.clone();
        // Overwriting the right half of 日 blanks its left half.
        g.put_char(1, 0, 5, 'a', Style::PLAIN);
        assert_eq!(g.row_text(0), " a本 ");
        assert_eq!(diffed(&before, &g), "\x1b[1H a");
        // A wide character with one column left becomes a blank.
        let mut g = Grid::new(5, 1);
        assert_eq!(g.put_str(3, 0, 5, "a日", Style::PLAIN), 5);
        assert_eq!(g.row_text(0), "   a ");
    }

    #[test]
    fn combining_marks_join_the_previous_cell() {
        let mut g = Grid::new(3, 1);
        assert_eq!(g.put_str(0, 0, 3, "e\u{301}x", Style::PLAIN), 2);
        assert_eq!(g.row(0)[0].text(), "e\u{301}");
        assert_eq!(g.row_text(0), "e\u{301}x ");
    }

    #[test]
    fn flushed_frames_reproduce_each_frame_in_an_emulator() {
        let (w, h) = (12u16, 4u16);
        let mut screen = Screen::new(w.into(), h.into());
        let size = |n| std::num::NonZero::new(n).expect("non-zero");
        let mut vt = vt100::Parser::new(size(h), size(w), 0);
        let (w, h) = (usize::from(w), usize::from(h));
        let frames: [&[(&str, Style)]; 4] = [
            &[("let x = 1;", GREEN), ("日本語", GREEN_BOLD)],
            &[
                ("let y = 1;", GREEN),
                ("", Style::PLAIN),
                ("tail", GREEN_BOLD),
            ],
            &[
                ("le", Style::PLAIN),
                ("日x本", GREEN),
                ("", GREEN),
                ("tailing", GREEN),
            ],
            &[("", Style::PLAIN)],
        ];
        for frame in frames {
            let grid = screen.frame();
            for (y, (text, style)) in frame.iter().enumerate() {
                let end = grid.put_str(0, y, w, text, *style);
                if y == 1 {
                    grid.fill(end, y, end + 2, *style);
                }
            }
            let expected = grid.clone();
            vt.process(screen.flush());
            for y in 0..h {
                let mut x = 0;
                while x < w {
                    let want = expected.row(y)[x];
                    let at = |n: usize| u16::try_from(n).expect("small");
                    let got = vt.screen().cell(at(y), at(x)).expect("in range");
                    let text = if got.has_contents() {
                        got.contents()
                    } else {
                        " "
                    };
                    assert_eq!(text, want.text(), "text at {x},{y}");
                    assert_eq!(got.bold(), want.style.attributes & BOLD != 0);
                    let fg = match want.style.fg {
                        Some(Color::Ansi(n)) => vt100::Color::Idx(n),
                        _ => vt100::Color::Default,
                    };
                    assert_eq!(got.fgcolor(), fg, "color at {x},{y}");
                    x += usize::from(want.width.max(1));
                }
            }
        }
    }
}

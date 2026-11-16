//! A grid of character cells to draw diagrams on. Lines aren't drawn as
//! characters but as the directions each cell connects to, so where lines
//! meet or cross the cell becomes the right junction (`┬`, `┼`, ...)
//! whatever order they were drawn in. Text written over a cell replaces
//! its line.

use unicode_width::UnicodeWidthChar as _;

use crate::{Line, Span};

/// Style names from the protocol's vocabulary.
pub(crate) type Style = &'static [&'static str];

pub(crate) const PLAIN: Style = &[];
/// Node, participant, note and frame outlines, and lifelines.
pub(crate) const BORDER: Style = &["dim"];
/// Edges, message arrows and their heads.
pub(crate) const EDGE: Style = &["syntax-operator"];
pub(crate) const EDGE_LABEL: Style = &["italic"];
/// Subgraph titles and participant names.
pub(crate) const TITLE: Style = &["bold"];
/// The word that opens a sequence frame (`loop`, `alt`, `else`...).
pub(crate) const KEYWORD: Style = &["syntax-keyword"];
pub(crate) const NUMBER: Style = &["syntax-number"];

pub(crate) const UP: u8 = 1;
pub(crate) const DOWN: u8 = 2;
pub(crate) const LEFT: u8 = 4;
pub(crate) const RIGHT: u8 = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stroke {
    Light,
    Dotted,
    Thick,
}

impl Stroke {
    const fn bit(self) -> u8 {
        match self {
            Self::Light => 1,
            Self::Dotted => 2,
            Self::Thick => 4,
        }
    }
}

#[derive(Clone, Default)]
struct Cell {
    dirs: u8,
    /// The strokes of the lines through the cell, as [`Stroke::bit`]s.
    strokes: u8,
    /// Text drawn over the cell; empty for the second column of a wide
    /// character.
    text: Option<String>,
    style: Style,
}

/// The display width of `text`, as the canvas lays it out.
pub(crate) fn width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

#[derive(Default)]
pub(crate) struct Canvas {
    rows: Vec<Vec<Cell>>,
}

impl Canvas {
    fn cell(&mut self, x: usize, y: usize) -> &mut Cell {
        if self.rows.len() <= y {
            self.rows.resize_with(y + 1, Vec::new);
        }
        let row = &mut self.rows[y];
        if row.len() <= x {
            row.resize_with(x + 1, Cell::default);
        }
        &mut row[x]
    }

    /// Makes the canvas at least `height` rows tall, blank ones included.
    pub(crate) fn grow(&mut self, height: usize) {
        if self.rows.len() < height {
            self.rows.resize_with(height, Vec::new);
        }
    }

    pub(crate) fn dirs(&self, x: usize, y: usize) -> u8 {
        self.rows
            .get(y)
            .and_then(|row| row.get(x))
            .map_or(0, |c| c.dirs)
    }

    /// Joins the cell at `(x, y)` to its neighbors in `dirs`. The last
    /// style drawn through a cell is its style.
    pub(crate) fn mark(&mut self, (x, y): (usize, usize), dirs: u8, stroke: Stroke, style: Style) {
        let cell = self.cell(x, y);
        cell.dirs |= dirs;
        cell.strokes |= stroke.bit();
        cell.style = style;
    }

    /// A straight line from `a` to `b`, both included; they share a row or
    /// a column.
    pub(crate) fn line(
        &mut self,
        a: (usize, usize),
        b: (usize, usize),
        stroke: Stroke,
        style: Style,
    ) {
        if a.1 == b.1 {
            for x in a.0.min(b.0)..a.0.max(b.0) {
                self.mark((x, a.1), RIGHT, stroke, style);
                self.mark((x + 1, a.1), LEFT, stroke, style);
            }
        } else if a.0 == b.0 {
            for y in a.1.min(b.1)..a.1.max(b.1) {
                self.mark((a.0, y), DOWN, stroke, style);
                self.mark((a.0, y + 1), UP, stroke, style);
            }
        }
    }

    /// A box with corners `(x0, y0)` and `(x1, y1)`.
    pub(crate) fn rect(
        &mut self,
        (x0, y0): (usize, usize),
        (x1, y1): (usize, usize),
        style: Style,
    ) {
        self.line((x0, y0), (x1, y0), Stroke::Light, style);
        self.line((x0, y1), (x1, y1), Stroke::Light, style);
        self.line((x0, y0), (x0, y1), Stroke::Light, style);
        self.line((x1, y0), (x1, y1), Stroke::Light, style);
    }

    /// Blanks the cells from `(x0, y0)` to `(x1, y1)`.
    pub(crate) fn clear(&mut self, (x0, y0): (usize, usize), (x1, y1): (usize, usize)) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                *self.cell(x, y) = Cell::default();
            }
        }
    }

    /// Writes `text` from `(x, y)` on; returns the columns it took.
    pub(crate) fn text(&mut self, (x, y): (usize, usize), text: &str, style: Style) -> usize {
        let mut at = x;
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if w == 0 {
                if at > x
                    && let Some(previous) = self.cell(at - 1, y).text.as_mut()
                {
                    previous.push(c);
                }
                continue;
            }
            let cell = self.cell(at, y);
            cell.text = Some(c.to_string());
            cell.style = style;
            for k in 1..w {
                self.cell(at + k, y).text = Some(String::new());
            }
            at += w;
        }
        at - x
    }

    pub(crate) fn put(&mut self, at: (usize, usize), c: char, style: Style) {
        self.text(at, c.encode_utf8(&mut [0; 4]), style);
    }

    /// Each row as styled spans, trailing blanks dropped.
    pub(crate) fn lines(&self) -> Vec<Line> {
        self.rows.iter().map(|row| line(row)).collect()
    }
}

fn line(row: &[Cell]) -> Line {
    let mut line: Line = Vec::new();
    for cell in row {
        let (text, style) = match &cell.text {
            Some(text) => (text.clone(), cell.style),
            None if cell.dirs == 0 => (" ".to_owned(), PLAIN),
            None => (glyph(cell.dirs, cell.strokes).to_string(), cell.style),
        };
        let style = if text.trim().is_empty() && !text.is_empty() {
            PLAIN
        } else {
            style
        };
        match line.last_mut() {
            Some(last) if last.style == style => last.text.push_str(&text),
            _ => line.push(Span { text, style }),
        }
    }
    if let Some(last) = line.last_mut() {
        let kept = last.text.trim_end().len();
        last.text.truncate(kept);
        if last.text.is_empty() {
            line.pop();
        }
    }
    line
}

/// The box-drawing character joining a cell to its neighbors in `dirs`.
/// Straight lines show their stroke; junctions are drawn thick only when
/// every line through them is thick.
fn glyph(dirs: u8, strokes: u8) -> char {
    let thick = strokes == Stroke::Thick.bit();
    let dotted = strokes == Stroke::Dotted.bit();
    let across = dirs & (LEFT | RIGHT) != 0;
    let along = dirs & (UP | DOWN) != 0;
    if along && !across {
        return match (thick, dotted) {
            (true, _) => '┃',
            (_, true) => '┆',
            _ => '│',
        };
    }
    if across && !along {
        return match (thick, dotted) {
            (true, _) => '━',
            (_, true) => '┄',
            _ => '─',
        };
    }
    let (light, heavy) = match dirs {
        d if d == DOWN | RIGHT => ('┌', '┏'),
        d if d == DOWN | LEFT => ('┐', '┓'),
        d if d == UP | RIGHT => ('└', '┗'),
        d if d == UP | LEFT => ('┘', '┛'),
        d if d == UP | DOWN | RIGHT => ('├', '┣'),
        d if d == UP | DOWN | LEFT => ('┤', '┫'),
        d if d == LEFT | RIGHT | DOWN => ('┬', '┳'),
        d if d == LEFT | RIGHT | UP => ('┴', '┻'),
        _ => ('┼', '╋'),
    };
    if thick { heavy } else { light }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(canvas: &Canvas) -> Vec<String> {
        canvas
            .lines()
            .iter()
            .map(|line| line.iter().map(|s| s.text.as_str()).collect())
            .collect()
    }

    #[test]
    fn lines_meeting_become_junctions_and_text_covers_them() {
        let mut canvas = Canvas::default();
        canvas.rect((0, 0), (5, 2), BORDER);
        canvas.line((2, 2), (2, 4), Stroke::Thick, EDGE);
        canvas.line((0, 4), (4, 4), Stroke::Dotted, EDGE);
        canvas.text((1, 1), " 漢 ", PLAIN);
        assert_eq!(
            plain(&canvas),
            ["┌────┐", "│ 漢 │", "└─┬──┘", "  ┃", "┄┄┴┄┄"]
        );
    }
}

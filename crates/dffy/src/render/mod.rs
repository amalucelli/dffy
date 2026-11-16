//! Layout of diffs into styled lines. Renderers write rows of role-tagged
//! text into a [`Canvas`]; turning roles into escape sequences happens only
//! when a canvas is written out, so the TUI can reuse the layout code with
//! its own cell grid.
//!
//! Print mode (from the core's hunks) and pager mode (from a parsed patch)
//! both describe a diff with the core's [`FileHeader`], [`HunkHeader`] and
//! [`Line`]; this module decides only how they look.

mod split;
pub(crate) mod stat;
mod unified;

use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::{ControlFlow, Range};

use dffy_core::view::{
    self, Arrangement, Attributes, FileHeader, FileStatus, HunkHeader, Kind, Line, LineState,
    Paint, Run,
};
use unicode_width::UnicodeWidthChar;

use crate::config::Indicators;
use crate::theme::{Ink, Role, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    /// End offset of the span's text in [`Canvas::text`]; it starts where
    /// the previous span ends.
    end: usize,
    ink: Ink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    /// The end index of the row's spans.
    end: usize,
    /// Drawn from the end of the text to the edge of the screen, for
    /// line backgrounds.
    fill: Option<Ink>,
}

/// Rows of role-tagged text.
#[derive(Debug, Default, Clone)]
pub(crate) struct Canvas {
    text: String,
    spans: Vec<Span>,
    rows: Vec<Row>,
    /// The fill of the row being written.
    fill: Option<Ink>,
    map: Option<Box<CodeMap>>,
}

/// Where a canvas's code went, cell by cell, so a screen cell can be
/// traced back to the byte of the line it shows (mouse selection). The
/// layout code records it as it writes, so tabs, wide characters, the
/// sideways scroll and wrapping are counted the way they're drawn.
#[derive(Debug, Default, Clone)]
pub(crate) struct CodeMap {
    pub glyphs: Vec<Mapped>,
    /// Where each side's code starts on each row; left of it is gutter.
    pub areas: Vec<Area>,
    /// The column the next text goes to on the current row.
    col: usize,
    /// The side code is written for now.
    side: usize,
    /// How much of the canvas's text `col` counts.
    counted: usize,
}

/// A glyph of code: on `row` of the canvas at column `col`, showing
/// `bytes` of its line on `side` (0 old, 1 new).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mapped {
    pub row: usize,
    pub side: usize,
    pub col: usize,
    pub width: usize,
    pub bytes: Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Area {
    pub row: usize,
    pub side: usize,
    pub col: usize,
}

impl Canvas {
    /// A canvas that keeps a [`CodeMap`].
    pub(crate) fn mapped() -> Self {
        Self {
            map: Some(Box::default()),
            ..Self::default()
        }
    }

    pub(crate) fn map(&self) -> Option<&CodeMap> {
        self.map.as_deref()
    }

    /// Code written from now on is `side`'s (0 old, 1 new).
    pub(crate) fn set_side(&mut self, side: usize) {
        if let Some(map) = &mut self.map {
            map.side = side;
        }
    }

    /// Records that the current side's code starts here.
    fn note_area(&mut self) {
        let row = self.rows.len();
        if let Some(map) = &mut self.map {
            map.col += str_width(&self.text[map.counted..]);
            map.counted = self.text.len();
            map.areas.push(Area {
                row,
                side: map.side,
                col: map.col,
            });
        }
    }

    /// Records that the glyph written next shows `bytes` and takes
    /// `width` columns; a zero-width one joins the glyph before it.
    fn note_glyph(&mut self, bytes: Range<usize>, width: usize) {
        let row = self.rows.len();
        let Some(map) = &mut self.map else {
            return;
        };
        map.col += str_width(&self.text[map.counted..]);
        map.counted = self.text.len();
        if width == 0 {
            if let Some(last) = map.glyphs.last_mut().filter(|g| g.row == row) {
                last.bytes.end = bytes.end;
            }
            return;
        }
        map.glyphs.push(Mapped {
            row,
            side: map.side,
            col: map.col,
            width,
            bytes,
        });
    }

    pub(crate) fn push(&mut self, role: Role, s: &str) {
        self.push_ink(role.into(), s);
    }

    pub(crate) fn push_ink(&mut self, ink: Ink, s: &str) {
        if s.is_empty() {
            return;
        }
        self.text.push_str(s);
        self.close_span(ink);
    }

    pub(crate) fn push_char(&mut self, ink: impl Into<Ink>, c: char) {
        self.text.push(c);
        self.close_span(ink.into());
    }

    pub(crate) fn push_fmt(&mut self, role: Role, args: std::fmt::Arguments) {
        let _ = self.text.write_fmt(args);
        self.close_span(role.into());
    }

    pub(crate) fn spaces(&mut self, ink: impl Into<Ink>, n: usize) {
        if n > 0 {
            self.text.extend(std::iter::repeat_n(' ', n));
            self.close_span(ink.into());
        }
    }

    /// Extends the current row's last span to the end of the text when it
    /// has the same ink, or starts a new span.
    fn close_span(&mut self, ink: Ink) {
        let row_start = self.rows.last().map_or(0, |r| r.end);
        let in_row = self.spans.len() > row_start;
        let end = self.text.len();
        match self.spans.last_mut() {
            Some(last) if in_row && last.ink == ink => last.end = end,
            Some(last) if last.end == end => {}
            None if end == 0 => {}
            _ => self.spans.push(Span { end, ink }),
        }
    }

    /// Makes the rest of the current row, past its text, take `ink`'s
    /// background.
    pub(crate) fn fill_rest(&mut self, ink: impl Into<Ink>) {
        self.fill = Some(ink.into());
    }

    pub(crate) fn end_line(&mut self) {
        self.rows.push(Row {
            end: self.spans.len(),
            fill: self.fill.take(),
        });
        if let Some(map) = &mut self.map {
            map.col = 0;
            map.counted = self.text.len();
        }
    }

    pub(crate) fn fill(&self, i: usize) -> Option<Ink> {
        self.rows.get(i).and_then(|r| r.fill)
    }

    pub(crate) fn line(&mut self, role: Role, s: &str) {
        self.push(role, s);
        self.end_line();
    }

    pub(crate) fn clear(&mut self) {
        self.text.clear();
        self.spans.clear();
        self.rows.clear();
        self.fill = None;
        if let Some(map) = &mut self.map {
            **map = CodeMap::default();
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The number of finished rows.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Each finished row as its `(ink, text)` spans.
    pub(crate) fn rows(&self) -> impl Iterator<Item = impl Iterator<Item = (Ink, &str)>> {
        let mut first_span = 0;
        self.rows.iter().map(move |&Row { end, .. }| {
            let spans = &self.spans[first_span..end];
            let mut start = if first_span == 0 {
                0
            } else {
                self.spans[first_span - 1].end
            };
            first_span = end;
            spans.iter().map(move |span| {
                let text = &self.text[start..span.end];
                start = span.end;
                (span.ink, text)
            })
        })
    }

    /// Writes every finished row, styled by `theme`, or plain without one.
    pub(crate) fn write_to(&self, w: &mut impl Write, theme: Option<&Theme>) -> io::Result<()> {
        for (i, row) in self.rows().enumerate() {
            let mut current = "";
            for (ink, text) in row {
                if let Some(theme) = theme {
                    let sgr = theme.sgr(ink);
                    if sgr != current {
                        w.write_all(if sgr.is_empty() {
                            b"\x1b[0m"
                        } else {
                            sgr.as_bytes()
                        })?;
                        current = sgr;
                    }
                }
                w.write_all(text.as_bytes())?;
            }
            if let (Some(theme), Some(fill)) = (theme, self.fill(i))
                && theme.ink_style(fill).bg.is_some()
            {
                // Erasing to the end of the line paints it with the
                // current background.
                current = theme.sgr(fill);
                w.write_all(current.as_bytes())?;
                w.write_all(b"\x1b[K")?;
            }
            if !current.is_empty() {
                w.write_all(b"\x1b[0m")?;
            }
            w.write_all(b"\n")?;
        }
        Ok(())
    }

    /// The rows with each styled span wrapped as `<role>text</>`, for tests.
    #[cfg(test)]
    pub(crate) fn markup(&self) -> String {
        let mut out = String::new();
        for row in self.rows() {
            for (ink, text) in row {
                let role = match ink.role {
                    Role::Plain | Role::Context => None,
                    role => Some(role.key()),
                };
                let syntax = ink.syntax.map(view::Syntax::key);
                match (role, syntax) {
                    (None, None) => out.push_str(text),
                    (Some(r), None) => {
                        let _ = write!(out, "<{r}>{text}</>");
                    }
                    (None, Some(s)) => {
                        let _ = write!(out, "<:{s}>{text}</>");
                    }
                    (Some(r), Some(s)) => {
                        let _ = write!(out, "<{r}:{s}>{text}</>");
                    }
                }
            }
            out.push('\n');
        }
        out
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent layout choices, not a state machine"
)]
pub(crate) struct Layout {
    pub split: bool,
    /// Columns available; `None` writes lines in full (unified only).
    pub width: Option<usize>,
    pub wrap: bool,
    /// Wrapped rows break between words, after a space, hyphen or slash or
    /// beside a wide character, and inside a word only when it's wider
    /// than the row: for prose, such as a rendered view's paragraphs.
    pub words: bool,
    pub tab_width: usize,
    /// Already resolved for color: classic whenever output is plain.
    pub indicators: Indicators,
    /// The file exists on one side only (added, untracked or deleted): one
    /// full-width pane with one column of line numbers, even when split.
    pub one_sided: bool,
    /// The file is unmerged: conflict markers among its added and
    /// unchanged lines take the warning color, over syntax colors.
    pub unmerged: bool,
    /// Columns for line numbers across a whole file, so they line up from
    /// hunk to hunk; 0 sizes them per hunk (a piped diff's file lengths are
    /// unknown).
    pub digits: usize,
    /// Classic signs go right before the text (`│-text`), as git prints
    /// them, rather than in a column of their own (`-│ text`). For output
    /// without color, which has to keep reading as a diff.
    pub inline_signs: bool,
    /// Display columns cut from the left of every line of code when not
    /// wrapping: the viewer's horizontal scroll. Gutters stay put.
    pub scroll: usize,
    /// Line numbers in the gutter; without them the change marks stay.
    pub line_numbers: bool,
    /// The `@@ -a,b +c,d @@` numbers of hunks; without them a hunk row
    /// shows its function alone and a separator its hidden lines.
    pub hunk_headers: bool,
    /// Lines paired with one on the other side show as modified
    /// (`modified-lines = changed`), not as removed and added.
    pub modified_lines: bool,
}

/// The narrowest text column split view accepts before falling back to
/// unified for a hunk.
const MIN_SPLIT_TEXT: usize = 12;

/// What styles a line's text besides its role: emphasis ranges, syntax
/// runs and a renderer's paint.
type Marks<'a> = (&'a [Range<usize>], &'a [Run], &'a [Paint]);

const NO_MARKS: Marks<'static> = (&[], &[], &[]);

/// Columns of the largest line number `header`'s hunk shows.
fn header_digits(header: &HunkHeader) -> usize {
    digits((header.old_start + header.old_len).max(header.new_start + header.new_len))
}

/// Decimal digits of `n`, at least 1.
fn digits(n: usize) -> usize {
    n.max(1).ilog10() as usize + 1
}

impl Layout {
    /// This layout for the hunks of a file with `status` whose longer side
    /// has `lines` lines (0 when unknown).
    #[must_use]
    pub(crate) fn for_file(self, status: FileStatus, lines: usize) -> Self {
        Self {
            one_sided: matches!(
                status,
                FileStatus::Added | FileStatus::Untracked | FileStatus::Deleted
            ),
            unmerged: status == FileStatus::Unmerged,
            digits: if lines == 0 { 0 } else { digits(lines) },
            ..self
        }
    }

    /// Whether `line` is a conflict marker git wrote into an unmerged file:
    /// seven `<`, `|`, `=` or `>`, then the end of the line or a space and
    /// a label (`=======` takes none). Removed lines never count.
    fn conflict_marker(&self, line: &Line) -> bool {
        if !self.unmerged || !matches!(line.state.kind(), Kind::Context | Kind::Added) {
            return false;
        }
        let Some(&c @ (b'<' | b'|' | b'=' | b'>')) = line.text.first() else {
            return false;
        };
        line.text
            .get(..7)
            .is_some_and(|run| run.iter().all(|&b| b == c))
            && match line.text.get(7) {
                None => true,
                Some(b' ') => c != b'=',
                Some(_) => false,
            }
    }

    /// Columns of each line number; 0 without line numbers.
    fn number_width(&self, header: &HunkHeader) -> usize {
        if !self.line_numbers {
            0
        } else if self.digits > 0 {
            self.digits
        } else {
            header_digits(header)
        }
    }

    /// Columns between the line numbers and the code: three in every mode
    /// (` │ `, ` ▌ `), four for classic signs in their own column (` -│ `).
    pub(crate) fn mark_width(&self) -> usize {
        if self.indicators == Indicators::Classic && !self.inline_signs {
            4
        } else {
            3
        }
    }

    /// Columns before the line numbers: under background tints, a cell
    /// that pads the tinted number cell on the left as the cell after the
    /// numbers pads it on the right.
    pub(crate) fn lead_width(&self) -> usize {
        usize::from(self.indicators == Indicators::Background)
    }

    fn lead(&self, c: &mut Canvas, role: Role) {
        c.spaces(role, self.lead_width());
    }

    /// The gutter after the numbers of a row without a line of code, up to
    /// and including the rule; the cell after the rule is the caller's.
    fn blank_mark(&self, c: &mut Canvas) {
        c.push(
            Role::Dim,
            if self.mark_width() == 4 {
                "  │"
            } else {
                " │"
            },
        );
    }

    /// The row standing for `hidden` unchanged lines, with the numbers and
    /// heading of the hunk after them when there is one. `target` marks it
    /// as the one `x` would reveal.
    pub(crate) fn separator(
        &self,
        hidden: usize,
        next: Option<&HunkHeader>,
        target: bool,
        c: &mut Canvas,
    ) {
        c.push(if target { Role::Hint } else { Role::Dim }, "⋯");
        let s = if hidden == 1 { "" } else { "s" };
        let text = format!(" {hidden} unchanged line{s}");
        let mut used = 1 + text.len();
        c.push(Role::Dim, &text);
        if let Some(h) = next {
            let numbers = if self.hunk_headers {
                format!("  {}", h.numbers())
            } else if h.heading.is_empty() {
                String::new()
            } else {
                " ·".to_owned()
            };
            used += numbers.chars().count();
            c.push(Role::Dim, &numbers);
            if !h.heading.is_empty() {
                c.push(Role::Dim, " ");
                let max = self
                    .width
                    .map_or(usize::MAX, |w| w.saturating_sub(used + 1));
                self.put_fitted(c, h.heading, NO_MARKS, (Role::Dim, Role::Dim), max);
            }
        }
        c.end_line();
    }

    pub(crate) fn file_header(&self, h: &FileHeader, c: &mut Canvas) {
        let status = h.status.word();
        let label = match h
            .staging
            .filter(|_| !matches!(h.status, FileStatus::Untracked | FileStatus::Unmerged))
        {
            Some(staging) => format!("{status} · {}: ", stat::staging_word(staging)),
            None => format!("{status}: "),
        };
        let max = self.width.unwrap_or(usize::MAX);
        c.push(Role::FileHeader, &label);
        let mut used = label.chars().count();
        let path = if h.status == FileStatus::Deleted {
            h.old_path
        } else {
            h.new_path
        };
        if matches!(h.status, FileStatus::Renamed | FileStatus::Copied) && h.old_path != h.new_path
        {
            used += self.put_fitted(
                c,
                h.old_path,
                NO_MARKS,
                (Role::FileHeader, Role::FileHeader),
                max.saturating_sub(used),
            );
            c.push(Role::FileHeader, " → ");
            used += 3;
        }
        used += self.put_fitted(
            c,
            path,
            NO_MARKS,
            (Role::FileHeader, Role::FileHeader),
            max.saturating_sub(used),
        );
        let mut extra = String::new();
        if let Some(similarity) = h.similarity {
            let _ = write!(extra, " ({similarity}% similar)");
        }
        if let (Some(old), Some(new)) = (h.old_mode, h.new_mode)
            && old != new
        {
            let _ = write!(extra, " (mode {old} → {new})");
        }
        if !extra.is_empty() && used + extra.chars().count() <= max {
            used += extra.chars().count();
            c.push(Role::Dim, &extra);
        }
        c.end_line();
        c.push(Role::FileHeader, &"─".repeat(used.min(max)));
        c.end_line();
        if let Some(note) = h.note {
            // Notes can carry text from the repository, such as a filter's
            // error naming a value from .gitattributes.
            c.line(Role::Dim, &escape(note.as_bytes()));
        }
    }

    /// Renders a hunk, or a later part of one when `show_header` is false
    /// (pager mode flushes very long hunks in parts).
    pub(crate) fn hunk(
        &self,
        header: &HunkHeader,
        show_header: bool,
        lines: &[Line],
        c: &mut Canvas,
    ) {
        if show_header {
            self.hunk_header(header, c);
        }
        let mut rows = Vec::new();
        view::rows(
            self.arrangement(header),
            lines.len(),
            |i| (lines[i].state.kind(), lines[i].block_start),
            &mut rows,
        );
        for row in rows {
            self.hunk_row(header, row, |i| lines[i], c);
        }
    }

    /// How this layout arranges a hunk: side by side when split, except
    /// for hunks whose line numbers leave too little room.
    pub(crate) fn arrangement(&self, header: &HunkHeader) -> Arrangement {
        let half = self.width.unwrap_or(80).saturating_sub(1) / 2;
        if self.split
            && !self.one_sided
            && half
                >= self.lead_width()
                    + self.number_width(header)
                    + self.mark_width()
                    + MIN_SPLIT_TEXT
        {
            Arrangement::Split
        } else {
            Arrangement::Unified
        }
    }

    /// Draws one hunk row from [`hunk_rows`] (or the header), as one screen
    /// row or several when it wraps. `line(i)` is line `i` of the body.
    pub(crate) fn hunk_row<'a>(
        &self,
        header: &HunkHeader,
        row: view::Row,
        line: impl Fn(usize) -> Line<'a>,
        c: &mut Canvas,
    ) {
        let w = self.number_width(header);
        let line = |i| {
            let l = line(i);
            if self.conflict_marker(&l) {
                Line {
                    emphasis: &[],
                    syntax: None,
                    paint: &[],
                    ..l
                }
            } else {
                l
            }
        };
        match row {
            view::Row::Header => self.hunk_header(header, c),
            view::Row::Line(i) => unified::line(self, w, &line(i), c),
            view::Row::Pair(old, new) => {
                let (old, new) = (old.map(&line), new.map(&line));
                split::row(self, w, [old.as_ref(), new.as_ref()], c);
            }
            view::Row::Marker => split::marker(self, c, w),
        }
    }

    fn hunk_header(&self, h: &HunkHeader, c: &mut Canvas) {
        if h.hidden > 0 {
            self.separator(h.hidden, Some(h), false, c);
            return;
        }
        if !self.hunk_headers {
            // The row stays, so rows don't shift; it shows the function.
            if h.heading.is_empty() {
                c.push(Role::Dim, "⋯");
            } else {
                let max = self.width.unwrap_or(usize::MAX);
                self.put_fitted(c, h.heading, NO_MARKS, (Role::Dim, Role::Dim), max);
            }
            c.end_line();
            return;
        }
        let numbers = h.numbers();
        c.push(Role::HunkHeader, &numbers);
        if !h.heading.is_empty() {
            c.push(Role::Plain, " ");
            let max = self
                .width
                .map_or(usize::MAX, |w| w.saturating_sub(numbers.len() + 1));
            self.put_fitted(c, h.heading, NO_MARKS, (Role::Plain, Role::Plain), max);
        }
        c.end_line();
    }

    /// Writes `text` within `max` columns, ending in `…` when it doesn't fit.
    /// `roles` are its normal and emphasized roles. Returns the columns used.
    fn put_fitted(
        &self,
        c: &mut Canvas,
        text: &[u8],
        marks: Marks,
        roles: (Role, Role),
        max: usize,
    ) -> usize {
        self.put_fitted_from(c, text, marks, roles, max, Cursor::default())
    }

    /// [`Layout::put_fitted`] for the text from `cursor` on.
    fn put_fitted_from(
        &self,
        c: &mut Canvas,
        text: &[u8],
        marks: Marks,
        roles: (Role, Role),
        max: usize,
        mut cursor: Cursor,
    ) -> usize {
        if max == usize::MAX || text_width(text, cursor, self.tab_width, max) <= max {
            return put_text(c, text, marks, roles, self.tab_width, max, &mut cursor);
        }
        let used = put_text(
            c,
            text,
            marks,
            roles,
            self.tab_width,
            max.saturating_sub(1),
            &mut cursor,
        );
        if max == 0 {
            return 0;
        }
        c.push(Role::Dim, "…");
        used + 1
    }

    /// Writes one row's worth of `text` from `cursor`: up to `max` columns,
    /// wrapping (the cursor stops where the row ends) or truncating with `…`.
    /// Returns the columns used.
    fn put_row(
        &self,
        c: &mut Canvas,
        line: &Line,
        roles: (Role, Role),
        max: usize,
        cursor: &mut Cursor,
    ) -> usize {
        let marks = (line.emphasis, line.syntax.unwrap_or_default(), line.paint);
        c.note_area();
        if self.wrap && self.words {
            let end = match word_break(line.text, *cursor, self.tab_width, max) {
                // Nothing fits; put_text makes progress anyway.
                end if end <= cursor.pos => line.text.len(),
                end => end,
            };
            let used = put_text(
                c,
                &line.text[..end],
                marks,
                roles,
                self.tab_width,
                max,
                cursor,
            );
            // The spaces a row breaks at don't start the next one.
            while line.text.get(cursor.pos) == Some(&b' ') {
                cursor.pos += 1;
                cursor.col += 1;
            }
            return used;
        }
        if self.wrap {
            return put_text(c, line.text, marks, roles, self.tab_width, max, cursor);
        }
        cursor.pos = line.text.len();
        if self.scroll == 0 {
            return self.put_fitted(c, line.text, marks, roles, max);
        }
        // Scrolled: the glyphs left of the scroll column go, and a `…` says
        // so on every line that had any.
        let mut from = Cursor::default();
        let mut start = line.text.len();
        for_each_glyph(line.text, 0, |offset, glyph| {
            if from.col >= self.scroll {
                start = offset;
                return ControlFlow::Break(());
            }
            from.col += glyph.width(from.col, self.tab_width);
            ControlFlow::Continue(())
        });
        if from.col == 0 || max == 0 {
            return 0;
        }
        from.pos = start;
        c.push(Role::Dim, "…");
        // What's left of a wide glyph or tab cut in two.
        let pad = from.col.saturating_sub(self.scroll).min(max - 1);
        c.spaces(roles.0, pad);
        1 + pad + self.put_fitted_from(c, line.text, marks, roles, max - 1 - pad, from)
    }
}

/// How a diff line is drawn: the roles of its text, emphasized text, sign
/// and line numbers, and the sign itself. A highlighted line's text takes
/// the line role, whose foreground the syntax colors replace, so the sign
/// and numbers alone carry the added or removed color.
struct LineStyle {
    text: Role,
    emph: Role,
    sign: Role,
    number: Role,
    sign_char: char,
    /// An added or removed line, which bars mark.
    changed: bool,
}

impl LineStyle {
    /// The role of the gutter's blanks: tinted like the numbers on a
    /// changed line under background tints.
    fn gutter(&self, indicators: Indicators) -> Role {
        if indicators == Indicators::Background && self.changed {
            self.number
        } else {
            Role::Plain
        }
    }

    /// Whether the line's background reaches the edge of its pane.
    fn fills(&self, line: &Line, indicators: Indicators) -> bool {
        line.state != LineState::Context
            && (line.syntax.is_some()
                || !line.paint.is_empty()
                || (indicators == Indicators::Background && self.changed))
    }

    /// The separator after the line numbers and the sign cell
    /// ([`Layout::mark_width`] columns). Bars replace the separator of a
    /// changed line with a bar in its sign's color, on every row it wraps
    /// to. Background tints keep the rule where every other row has it,
    /// in the gutter's colors so the tinted edge runs on unbroken: the
    /// number cell's padding, the rule, then a cell of the line's tint.
    /// Classic signs sit left of the rule, so everything right of it is
    /// code; without color they go right before the text instead.
    fn mark(&self, c: &mut Canvas, layout: &Layout, first: bool) {
        let sign = if first { self.sign_char } else { ' ' };
        match layout.indicators {
            Indicators::Background if self.changed => {
                c.spaces(self.number, 1);
                c.push(self.number, "│");
                c.spaces(self.text, 1);
            }
            Indicators::Bars if self.changed => {
                c.push(Role::Plain, " ");
                c.push(self.sign, "▌");
                c.push(Role::Plain, " ");
            }
            Indicators::Classic if layout.inline_signs => {
                c.push(Role::Dim, " │");
                c.push_char(self.sign, sign);
            }
            Indicators::Classic => {
                c.push(Role::Plain, " ");
                c.push_char(self.sign, sign);
                c.push(Role::Dim, "│");
                c.push(Role::Plain, " ");
            }
            Indicators::Auto | Indicators::Background | Indicators::Bars | Indicators::None => {
                c.push(Role::Dim, " │");
                c.push(Role::Plain, " ");
            }
        }
    }
}

/// How `line` is drawn. Reformatted lines are dim with `~` for a sign,
/// moved ones in the moved color with `»`. Under background tints a
/// changed line's text always takes the line role, whose background
/// carries the change. With the layout's `modified_lines`, a paired line
/// takes the modified roles, and only its changed tokens keep the removed
/// or added color. An unmerged file's conflict markers are in the warning
/// color whatever else they are.
fn line_style(line: &Line, layout: &Layout) -> LineStyle {
    if layout.conflict_marker(line) {
        let added = line.state.kind() == Kind::Added;
        let role = if added {
            Role::ConflictMarker
        } else {
            Role::Warn
        };
        return LineStyle {
            text: role,
            emph: role,
            sign: role,
            number: if added {
                Role::AddedNumber
            } else {
                Role::LineNumber
            },
            sign_char: if added { '+' } else { ' ' },
            changed: added,
        };
    }
    let indicators = layout.indicators;
    let unchanged = |role, sign_char| LineStyle {
        text: role,
        emph: role,
        sign: role,
        number: role,
        sign_char,
        changed: true,
    };
    let highlighted =
        line.syntax.is_some() || !line.paint.is_empty() || indicators == Indicators::Background;
    let modified = |removed: bool| LineStyle {
        text: if highlighted {
            Role::ModifiedLine
        } else {
            Role::Modified
        },
        emph: if removed {
            Role::RemovedEmph
        } else {
            Role::AddedEmph
        },
        sign: Role::Modified,
        number: Role::ModifiedNumber,
        sign_char: if removed { '-' } else { '+' },
        changed: true,
    };
    let (text, emph, sign, number, sign_char) = match line.state {
        LineState::RemovedReformatted | LineState::AddedReformatted => {
            return unchanged(Role::Dim, '~');
        }
        LineState::RemovedMoved | LineState::AddedMoved => return unchanged(Role::Moved, '»'),
        LineState::RemovedPaired if layout.modified_lines => return modified(true),
        LineState::AddedPaired if layout.modified_lines => return modified(false),
        LineState::Context | LineState::NoNewline => (
            Role::Context,
            Role::Context,
            Role::Context,
            Role::LineNumber,
            ' ',
        ),
        LineState::Removed | LineState::RemovedPaired => (
            if highlighted {
                Role::RemovedLine
            } else {
                Role::Removed
            },
            Role::RemovedEmph,
            Role::Removed,
            Role::RemovedNumber,
            '-',
        ),
        LineState::Added | LineState::AddedPaired => (
            if highlighted {
                Role::AddedLine
            } else {
                Role::Added
            },
            Role::AddedEmph,
            Role::Added,
            Role::AddedNumber,
            '+',
        ),
    };
    LineStyle {
        text,
        emph,
        sign,
        number,
        sign_char,
        changed: matches!(line.state.kind(), Kind::Removed | Kind::Added),
    }
}

const NO_NEWLINE: &str = "\\ No newline at end of file";

/// Position within a line's text across wrapped rows.
#[derive(Debug, Default, Clone, Copy)]
struct Cursor {
    pos: usize,
    /// Display column within the whole line, for tab stops.
    col: usize,
}

#[derive(Debug, Clone, Copy)]
enum Glyph {
    Char(char),
    Tab,
    /// A C0 control or DEL, shown in caret notation (`^[`).
    Control(u8),
    /// Invalid UTF-8 or a C1 control, shown as U+FFFD; the bytes it
    /// stands for.
    Replacement(usize),
    /// A bidirectional override or isolate, shown by its code point
    /// (`<U+202E>`): passed through, it would reorder the text around it,
    /// so a line could read differently from what it holds.
    Bidi(char),
}

impl Glyph {
    fn width(self, col: usize, tab_width: usize) -> usize {
        match self {
            Self::Char(c) => c.width().unwrap_or(0),
            Self::Tab => tab_width - col % tab_width,
            Self::Control(_) => 2,
            Self::Replacement(_) => 1,
            Self::Bidi(_) => BIDI_WIDTH,
        }
    }

    /// The bytes of the text it shows.
    fn len(self) -> usize {
        match self {
            Self::Char(c) | Self::Bidi(c) => c.len_utf8(),
            Self::Tab | Self::Control(_) => 1,
            Self::Replacement(len) => len,
        }
    }
}

/// Columns of a [`Glyph::Bidi`]'s label: `<U+` and `>` around four hex
/// digits, as every such control is in U+2000–U+2FFF.
const BIDI_WIDTH: usize = 8;

/// The label a [`Glyph::Bidi`] shows.
fn bidi_label(c: char) -> String {
    format!("<U+{:04X}>", u32::from(c))
}

/// Calls `f(offset, glyph)` for each displayed glyph of `text[from..]` until
/// it breaks. Content never reaches the terminal raw: controls are escaped
/// and invalid UTF-8 is replaced, so a diff can't inject escape sequences,
/// and bidirectional controls are labeled, so it can't reorder what shows.
fn for_each_glyph(text: &[u8], from: usize, mut f: impl FnMut(usize, Glyph) -> ControlFlow<()>) {
    for_each_piece(text, from, |offset, piece| {
        let glyph = match piece {
            Piece::Printable(run) => Glyph::Char(char::from(run.as_bytes()[0])),
            Piece::Glyph(glyph) => glyph,
        };
        f(offset, glyph)?;
        ControlFlow::Continue(1)
    });
}

/// A stretch of displayed text: a run of printable ASCII, one column per
/// byte, or any other glyph.
#[derive(Debug, Clone, Copy)]
enum Piece<'a> {
    Printable(&'a str),
    Glyph(Glyph),
}

/// [`for_each_glyph`] with runs of printable ASCII handed over whole, so a
/// caller can take many glyphs at once. `f` returns how many bytes of a
/// run it took, at least 1; the rest of the run comes next. A glyph is
/// always taken whole.
fn for_each_piece(
    text: &[u8],
    from: usize,
    mut f: impl FnMut(usize, Piece) -> ControlFlow<(), usize>,
) {
    let mut offset = from;
    for chunk in text[from..].utf8_chunks() {
        let valid = chunk.valid();
        let mut i = 0;
        while i < valid.len() {
            let printable = valid.as_bytes()[i..]
                .iter()
                .position(|b| !matches!(b, b' '..=b'~'))
                .unwrap_or(valid.len() - i);
            if printable > 0 {
                let end = i + printable;
                while i < end {
                    match f(offset + i, Piece::Printable(&valid[i..end])) {
                        ControlFlow::Continue(n) => i += n.max(1),
                        ControlFlow::Break(()) => return,
                    }
                }
                continue;
            }
            let Some(ch) = valid[i..].chars().next() else {
                break;
            };
            let glyph = match ch {
                '\t' => Glyph::Tab,
                '\0'..='\x1f' | '\x7f' => Glyph::Control(ch as u8),
                '\u{80}'..='\u{9f}' => Glyph::Replacement(2),
                c @ ('\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') => Glyph::Bidi(c),
                c => Glyph::Char(c),
            };
            if f(offset + i, Piece::Glyph(glyph)).is_break() {
                return;
            }
            i += ch.len_utf8();
        }
        offset += valid.len();
        if !chunk.invalid().is_empty() {
            let glyph = Glyph::Replacement(chunk.invalid().len());
            if f(offset, Piece::Glyph(glyph)).is_break() {
                return;
            }
            offset += chunk.invalid().len();
        }
    }
}

/// Columns `s` takes on the grid, which drops characters without a width.
fn str_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Display width of `text` as drawn.
pub(crate) fn width(text: &[u8], tab_width: usize) -> usize {
    text_width(text, Cursor::default(), tab_width, usize::MAX)
}

/// Display width of `text` from `from` on, counting no further than just
/// past `limit`.
fn text_width(text: &[u8], from: Cursor, tab_width: usize, limit: usize) -> usize {
    let mut col = from.col;
    for_each_piece(text, from.pos, |_, piece| {
        let taken = match piece {
            Piece::Printable(run) => {
                let n = run.len().min((limit - (col - from.col)).saturating_add(1));
                col += n;
                n
            }
            Piece::Glyph(g) => {
                col += g.width(col, tab_width);
                1
            }
        };
        if col - from.col > limit {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(taken)
        }
    });
    col - from.col
}

/// Where a row of at most `max` columns starting at `from` ends when rows
/// break between words: the text's end when the rest fits; else before the
/// first glyph that doesn't fit when that's a space; else after the last
/// spaces, unless the word after them is wider than a row by itself; else
/// inside that word, after its last hyphen or slash that fits or beside a
/// wide character; else after the last spaces; else before the first glyph
/// that doesn't fit. Always past `from` unless nothing fits at all.
fn word_break(text: &[u8], from: Cursor, tab_width: usize, max: usize) -> usize {
    let space = |g: Glyph| matches!(g, Glyph::Char(' ') | Glyph::Tab);
    let word_fits = |at: usize| {
        let mut width = 0;
        for_each_glyph(text, at, |_, g| {
            if space(g) || width > max {
                return ControlFlow::Break(());
            }
            width += g.width(0, tab_width);
            ControlFlow::Continue(())
        });
        width <= max
    };
    let (mut col, mut used) = (from.col, 0);
    let mut end = text.len();
    let (mut after_space, mut inside) = (None, None);
    // The glyph before the current one, its width, and whether a glyph
    // other than a space came before it in its word.
    let mut prev: Option<(Glyph, usize, bool)> = None;
    for_each_glyph(text, from.pos, |offset, glyph| {
        let width = glyph.width(col, tab_width);
        if let Some((p, p_width, worded)) = prev
            && !space(glyph)
        {
            if space(p) {
                after_space = Some(offset);
            } else if worded && matches!(p, Glyph::Char('-' | '/')) || p_width > 1 || width > 1 {
                inside = Some(offset);
            }
        }
        if used + width > max {
            end = match (after_space, inside) {
                _ if space(glyph) => offset,
                (Some(at), _) if word_fits(at) => at,
                (s, Some(at)) if s.is_none_or(|s| at > s) => at,
                (Some(at), _) => at,
                (None, _) => offset,
            };
            return ControlFlow::Break(());
        }
        let worded = !space(glyph) && prev.is_some_and(|(p, _, w)| w || !space(p));
        prev = Some((glyph, width, worded));
        used += width;
        col += width;
        ControlFlow::Continue(())
    });
    end
}

/// Writes `text` from `cursor` until the next glyph would exceed `max`
/// columns or the text ends, advancing the cursor. `roles` are the normal
/// and emphasized roles; `marks` are the emphasis ranges, syntax runs and
/// paint, all in bytes of `text`. Returns the columns written.
fn put_text(
    c: &mut Canvas,
    text: &[u8],
    (emphasis, syntax, paint): Marks,
    roles: (Role, Role),
    tab_width: usize,
    max: usize,
    cursor: &mut Cursor,
) -> usize {
    // Two copies of the glyph loop: lines without paint, every line but a
    // rendered view's, get one with no paint lookups compiled in, and the
    // painted one stays out of line so it doesn't change how the hot one
    // inlines; paint lookups in the shared loop cost 2-3% of a frame.
    if !paint.is_empty() {
        return put_painted(
            c,
            text,
            (emphasis, syntax, paint),
            roles,
            tab_width,
            max,
            cursor,
        );
    }
    put_glyphs(
        c,
        text,
        (emphasis, syntax),
        None::<fn(usize) -> Option<Paint>>,
        roles,
        (tab_width, max),
        cursor,
    )
}

/// [`put_text`] for a line with paint.
#[inline(never)]
fn put_painted(
    c: &mut Canvas,
    text: &[u8],
    (emphasis, syntax, paint): Marks,
    roles: (Role, Role),
    tab_width: usize,
    max: usize,
    cursor: &mut Cursor,
) -> usize {
    let mut next = 0;
    let painted = |offset: usize| {
        while paint.get(next).is_some_and(|p| p.end as usize <= offset) {
            next += 1;
        }
        paint
            .get(next)
            .filter(|p| p.start as usize <= offset)
            .copied()
    };
    put_glyphs(
        c,
        text,
        (emphasis, syntax),
        Some(painted),
        roles,
        (tab_width, max),
        cursor,
    )
}

/// [`put_text`]'s loop, with `painted(offset)` the paint of the glyph at
/// `offset` on a line with paint.
#[expect(
    clippy::inline_always,
    reason = "measured: the two loops without this and put_painted's inline(never) drew a full frame of unpainted lines 2-3% slower"
)]
#[inline(always)]
fn put_glyphs(
    c: &mut Canvas,
    text: &[u8],
    (emphasis, syntax): (&[Range<usize>], &[Run]),
    mut painted: Option<impl FnMut(usize) -> Option<Paint>>,
    roles: (Role, Role),
    (tab_width, max): (usize, usize),
    cursor: &mut Cursor,
) -> usize {
    let mut used = 0;
    let mut next_range = 0;
    let mut next_run = 0;
    let mut end = text.len();
    // Runs of printable ASCII in one ink go in whole, a byte per column,
    // unless paint can change the ink at any byte or the canvas maps each
    // glyph.
    let runs = painted.is_none() && c.map.is_none();
    for_each_piece(text, cursor.pos, |offset, piece| {
        while emphasis.get(next_range).is_some_and(|r| r.end <= offset) {
            next_range += 1;
        }
        let (role, role_end) = match emphasis.get(next_range) {
            Some(r) if r.start <= offset => (roles.1, r.end),
            Some(r) => (roles.0, r.start),
            None => (roles.0, usize::MAX),
        };
        while syntax
            .get(next_run)
            .is_some_and(|r| r.end as usize <= offset)
        {
            next_run += 1;
        }
        let (run_syntax, syntax_end) = match syntax.get(next_run) {
            Some(r) if r.start as usize <= offset => (Some(r.syntax), r.end as usize),
            Some(r) => (None, r.start as usize),
            None => (None, usize::MAX),
        };
        let glyph = match piece {
            Piece::Printable(run) if runs && used < max => {
                let n = run
                    .len()
                    .min(role_end.min(syntax_end) - offset)
                    .min(max - used);
                c.text.push_str(&run[..n]);
                c.close_span(Ink {
                    role,
                    syntax: run_syntax,
                    attributes: Attributes::NONE,
                });
                used += n;
                cursor.col += n;
                return ControlFlow::Continue(n);
            }
            Piece::Printable(run) => Glyph::Char(char::from(run.as_bytes()[0])),
            Piece::Glyph(glyph) => glyph,
        };
        let width = glyph.width(cursor.col, tab_width);
        if used + width > max {
            if used == 0 && max > 0 {
                // Nothing fits on this row (a wide glyph in a one-column
                // space); consume it as a blank so wrapping makes progress.
                c.note_glyph(offset..offset + glyph.len(), 1);
                c.spaces(Ink::from(roles.0), 1);
                used = 1;
                cursor.col += width;
                return ControlFlow::Continue(1);
            }
            end = offset;
            return ControlFlow::Break(());
        }
        let painted = painted.as_mut().and_then(|p| p(offset));
        let ink = Ink {
            role,
            syntax: run_syntax.or_else(|| painted.and_then(|p| p.syntax)),
            attributes: painted.map_or(Attributes::NONE, |p| p.attributes),
        };
        c.note_glyph(offset..offset + glyph.len(), width);
        match glyph {
            Glyph::Char(ch) => c.push_char(ink, ch),
            Glyph::Tab => c.spaces(ink, width),
            Glyph::Control(b) => {
                c.push_char(ink, '^');
                c.push_char(ink, char::from(b ^ 0x40));
            }
            Glyph::Replacement(_) => c.push_char(ink, char::REPLACEMENT_CHARACTER),
            Glyph::Bidi(b) => {
                for ch in bidi_label(b).chars() {
                    c.push_char(ink, ch);
                }
            }
        }
        used += width;
        cursor.col += width;
        ControlFlow::Continue(1)
    });
    cursor.pos = end;
    used
}

impl Layout {
    /// A line of free text as one row: cut to the width with `…` and
    /// scrolled sideways like code, so the viewer's commit headers and
    /// combined diffs show where they go on and let them be reached.
    pub(crate) fn text_row(&self, role: Role, text: &[u8], c: &mut Canvas) {
        let line = Line {
            state: LineState::Context,
            old: None,
            new: None,
            text,
            emphasis: &[],
            syntax: None,
            paint: &[],
            block_start: false,
        };
        let layout = Self {
            wrap: false,
            ..*self
        };
        let max = self.width.unwrap_or(usize::MAX);
        layout.put_row(c, &line, (role, role), max, &mut Cursor::default());
        c.end_line();
    }

    /// A line of free text (commit messages, passthrough lines). Written in
    /// full unless wrapping is on: it has no gutter to keep aligned, and
    /// cutting it would lose output of whatever git command dffy pages.
    pub(crate) fn text_line(&self, role: Role, text: &[u8], c: &mut Canvas) {
        let max = match self.width {
            Some(width) if self.wrap => width,
            _ => usize::MAX,
        };
        let mut cursor = Cursor::default();
        loop {
            put_text(
                c,
                text,
                NO_MARKS,
                (role, role),
                self.tab_width,
                max,
                &mut cursor,
            );
            c.end_line();
            if cursor.pos >= text.len() {
                break;
            }
        }
    }
}

/// Lossy UTF-8 with controls in caret notation, for paths and metadata
/// printed as plain strings.
pub(crate) fn escape(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for_each_glyph(bytes, 0, |_, glyph| {
        match glyph {
            Glyph::Char(c) => out.push(c),
            Glyph::Tab => out.push('\t'),
            Glyph::Control(b) => {
                out.push('^');
                out.push(char::from(b ^ 0x40));
            }
            Glyph::Replacement(_) => out.push(char::REPLACEMENT_CHARACTER),
            Glyph::Bidi(c) => out.push_str(&bidi_label(c)),
        }
        ControlFlow::Continue(())
    });
    out
}

/// [`escape`] for a message of one or more lines, keeping its line breaks.
/// Errors and warnings echo what the user typed (revisions, paths, theme
/// names), which mustn't reach the terminal raw.
pub(crate) fn escape_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| escape(line.as_bytes()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `13003` as `13,003`.
pub(crate) fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests;

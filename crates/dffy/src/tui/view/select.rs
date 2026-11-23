//! Selections in the diff: lines selected with `v` or the mouse, text
//! selected in a comment, what the cell under the pointer is, and what a
//! copy takes.

use std::ops::Range;

use dffy_core::diff::LineKind;
use dffy_core::document::{self, BlockId, Pos, Side};
use dffy_core::view::{Attributes, display_text};

use super::{Block, CommentSelection, Copied, Doc, Hit, Selection, arrange, rendered};
use crate::render::Layout;
use crate::term::Grid;
use crate::tui::comments;

impl Doc {
    /// Shows the selected lines in reverse video, on their side's half of
    /// a split row. `layout` is the drawn file's.
    pub(super) fn draw_selection(
        &mut self,
        grid: &mut Grid,
        (x, end, top): (usize, usize, usize),
        layout: &Layout,
    ) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let (lo, hi) = selection.range();
        let bytes = selection.byte_range();
        let side = selection.side.index();
        let mut previous = None;
        let mut lines = [None, None];
        let mut mapped = std::mem::take(&mut self.mapped);
        for y in 0..self.drawn.len() {
            let Some(pos) = self.drawn[y] else { continue };
            if pos.file != selection.anchor.file {
                continue;
            }
            let logical = Pos { offset: 0, ..pos };
            if previous != Some(logical) {
                lines = self
                    .document
                    .row_lines(logical, arrange(layout, &self.boxes));
                previous = Some(logical);
                if bytes.is_some() {
                    mapped.clear();
                    self.render_code(logical, layout, &mut mapped);
                }
            }
            let Some(line) = lines[side].filter(|l| (lo..=hi).contains(l)) else {
                continue;
            };
            if let (Some((from, to)), Some(map)) = (bytes, mapped.map()) {
                let split = self.split_row(logical, layout);
                for g in &map.glyphs {
                    let shown = g.row == pos.offset && (!split || g.side == side);
                    if shown
                        && (line > lo || g.bytes.start >= from)
                        && (line < hi || g.bytes.start < to)
                    {
                        let left = (x + g.col).min(end);
                        grid.toggle(
                            left,
                            top + y,
                            (left + g.width).min(end),
                            Attributes::REVERSE,
                        );
                    }
                }
                continue;
            }
            let columns = if self.split_row(logical, layout) {
                let left = (end - x).saturating_sub(1) / 2;
                match selection.side {
                    Side::Old => x..x + left,
                    Side::New => x + left + 1..end,
                }
            } else {
                x..end
            };
            grid.toggle(columns.start, top + y, columns.end, Attributes::REVERSE);
        }
        self.mapped = mapped;
    }

    /// The selection's file, side, and first and last lines (0-based).
    pub(crate) fn selected_lines(&self) -> Option<(usize, Side, usize, usize)> {
        let s = self.selection.as_ref()?;
        let (lo, hi) = s.range();
        Some((s.anchor.file, s.side, lo, hi))
    }

    /// Which side of the row at `pos` column `column` (from the pane's
    /// left) is on: the half it's in for a split row, else the side the
    /// row has a line on, the one read alone or the new one first.
    pub(crate) fn side_at(&mut self, pos: Pos, column: usize) -> Option<Side> {
        let layout = self.file_layout(pos.file);
        let lines = self.document.row_lines(pos, arrange(&layout, &self.boxes));
        if self.split_row(pos, &layout) {
            let left = self.layout.width.unwrap_or(0).saturating_sub(1) / 2;
            let side = if column < left { Side::Old } else { Side::New };
            return lines[side.index()].map(|_| side);
        }
        // A hidden run's row in file view stands for the other side's lines,
        // and an opened change's rule for the change: staging there takes
        // their hunk.
        let side = (self.side_of(pos.file, lines)).or_else(|| {
            let marked = self.document.mark(pos, arrange(&layout, &self.boxes));
            marked.map(|(side, _)| side)
        });
        side.or_else(|| {
            self.run_at(pos)?;
            self.document.reading_of(pos.file)
        })
    }

    /// The side of a row in `file` with `lines` that a selection or a
    /// comment takes: the one read alone, or else the new one, when the
    /// row has its line.
    fn side_of(&self, file: usize, lines: [Option<usize>; 2]) -> Option<Side> {
        let first = self.document.reading_of(file).unwrap_or(Side::New);
        [first, first.other()]
            .into_iter()
            .find(|side| lines[side.index()].is_some())
    }

    /// What the cell at row `y` of the pane, `column` from its left, shows
    /// as of the last frame. With `side`, only that side counts (a drag
    /// stays on the side it started on): a cell left of its code is the
    /// row's start, one right of it the row's end.
    pub(crate) fn hit(&mut self, y: usize, column: usize, side: Option<Side>) -> Option<Hit> {
        let drawn = self.drawn.get(y).copied().flatten()?;
        let pos = Pos { offset: 0, ..drawn };
        let layout = self.file_layout(pos.file);
        // A box's rows have no lines.
        let lines = self.document.row_lines(pos, arrange(&layout, &self.boxes));
        let confined = side.is_some();
        let side = match side {
            Some(side) => side,
            None => self.side_at(pos, column)?,
        };
        let line = lines[side.index()]?;
        let split = self.split_row(pos, &layout);
        let mut c = std::mem::take(&mut self.mapped);
        c.clear();
        self.render_code(pos, &layout, &mut c);
        let ours = |row: usize, s: usize| row == drawn.offset && (!split || s == side.index());
        let map = c.map()?;
        let area = map.areas.iter().find(|a| ours(a.row, a.side)).copied();
        let row: Vec<_> = map.glyphs.iter().filter(|g| ours(g.row, g.side)).collect();
        let earlier = map
            .glyphs
            .iter()
            .any(|g| g.row < drawn.offset && (!split || g.side == side.index()));
        let later = map
            .glyphs
            .iter()
            .any(|g| g.row > drawn.offset && (!split || g.side == side.index()));
        let gutter = !confined && area.is_some_and(|a| column < a.col);
        let bytes = row
            .iter()
            .find(|g| (g.col..g.col + g.width).contains(&column))
            .map(|g| g.bytes.clone());
        let first = row.first().map(|g| g.bytes.start);
        let last = row.last().map(|g| (g.col + g.width, g.bytes.end));
        self.mapped = c;
        if gutter {
            return Some(Hit::Gutter { pos, side, line });
        }
        let end = self.line_text(pos.file, side, line).map_or(0, <[u8]>::len);
        let bytes = match (bytes, first, last) {
            (Some(bytes), ..) => bytes,
            // Left of the row's first glyph: a scroll mark, or the gutter.
            (None, Some(first), Some((right, _))) if column < right => first..first,
            // Right of the row's last glyph: where the next row goes on,
            // or the end of the line on its last row (cut off or not).
            (None, _, Some((_, last))) if later => last..last,
            (None, _, Some(_)) => end..end,
            // No glyph on the row: an empty line, or a side whose wrapped
            // rows ran out before the other's.
            (None, ..) if earlier => end..end,
            (None, ..) => 0..0,
        };
        Some(Hit::Code {
            pos,
            side,
            line,
            bytes,
        })
    }

    /// Line `line` of `side` of `file` as shown, without its terminator.
    fn line_text(&self, file: usize, side: Side, line: usize) -> Option<&[u8]> {
        let diff = self.document.diff_of(file)?;
        let text = match side {
            Side::Old => &diff.old,
            Side::New => &diff.new,
        };
        let raw = text.line(line);
        Some(display_text(raw.strip_suffix(b"\n").unwrap_or(raw)))
    }

    /// Starts a selection where `hit` is: by characters on code and in a
    /// comment, by lines on the gutter.
    pub(crate) fn select_hit(&mut self, hit: &Hit) {
        let (pos, side, line, bytes) = match hit {
            Hit::Code {
                pos,
                side,
                line,
                bytes,
            } => (*pos, *side, *line, Some((bytes.clone(), bytes.clone()))),
            Hit::Gutter { pos, side, line } => (*pos, *side, *line, None),
            Hit::Comment {
                thread,
                comment,
                bytes,
            } => {
                self.select_comment(*thread, *comment, bytes.clone()..bytes.clone(), false);
                return;
            }
        };
        self.comment_selection = None;
        self.selection = Some(Selection {
            side,
            anchor: pos,
            cursor: pos,
            lines: (line, line),
            bytes,
        });
    }

    /// Selects in comment `comment` of thread `thread` from `range.start` to
    /// `range.end`, each what [`markdown::SourceMap::at`] gave.
    fn select_comment(
        &mut self,
        thread: usize,
        comment: usize,
        range: Range<Range<usize>>,
        word: bool,
    ) {
        let Some(t) = self.boxes.threads.get(thread) else {
            return;
        };
        let Some(n) = t.comments.get(comment) else {
            return;
        };
        self.selection = None;
        self.comment_selection = Some(CommentSelection {
            root: t.root.clone(),
            comment: n.id.clone(),
            anchor: range.start,
            cursor: range.end,
            word,
        });
    }

    /// Moves the selection's end to `hit`, on the selection's side, or in
    /// the comment it's in.
    pub(crate) fn select_to_hit(&mut self, hit: &Hit) {
        if let Hit::Comment {
            thread,
            comment,
            bytes,
        } = hit
        {
            if let Some(selection) = &mut self.comment_selection
                && let Some(t) = self.boxes.threads.get(*thread)
                && t.root == selection.root
                && t.comments
                    .get(*comment)
                    .is_some_and(|n| n.id == selection.comment)
            {
                selection.cursor = bytes.clone();
                selection.word = false;
            }
            return;
        }
        let Some(selection) = &mut self.selection else {
            return;
        };
        let (pos, side, line, bytes) = match hit {
            Hit::Code {
                pos,
                side,
                line,
                bytes,
            } => (*pos, *side, *line, bytes.clone()),
            Hit::Gutter { pos, side, line } => (*pos, *side, *line, 0..0),
            Hit::Comment { .. } => return,
        };
        if side != selection.side || pos.file != selection.anchor.file {
            return;
        }
        selection.cursor = pos;
        selection.lines.1 = line;
        if let Some((_, cursor)) = &mut selection.bytes {
            *cursor = bytes;
        }
    }

    /// Selects the word at `hit` (letters, digits and `_`), the run of
    /// blanks there, or the one other character; with `whole`, the line's
    /// code instead, or in a comment, the whole comment.
    pub(crate) fn select_word(&mut self, hit: &Hit, whole: bool) {
        if let Hit::Comment {
            thread,
            comment,
            bytes,
        } = hit
        {
            let Some(text) = self
                .boxes
                .threads
                .get(*thread)
                .and_then(|t| t.comments.get(*comment))
            else {
                return;
            };
            let len = text.text.len();
            let range = if whole {
                (0..0)..(len..len)
            } else {
                let (start, end) = document::word(text.text.as_bytes(), bytes.start);
                // A glyph drawn for a whole construct, an entity, comes whole.
                let word = start.min(bytes.start)..end.max(bytes.end);
                word.clone()..word
            };
            self.select_comment(*thread, *comment, range, !whole);
            return;
        }
        let Hit::Code {
            pos,
            side,
            line,
            bytes,
        } = hit
        else {
            return;
        };
        let Some(text) = self.line_text(pos.file, *side, *line) else {
            return;
        };
        let (start, end) = if whole {
            (0, text.len())
        } else {
            document::word(text, bytes.start)
        };
        self.comment_selection = None;
        self.selection = Some(Selection {
            side: *side,
            anchor: *pos,
            cursor: *pos,
            lines: (*line, *line),
            bytes: Some((start..start, end..end)),
        });
    }

    /// The block drawn on row `y` of the pane in the last frame, and which
    /// of its rows it is there.
    pub(super) fn block_line_at(&mut self, y: usize) -> Option<(BlockId, usize)> {
        let drawn = self.row_at(y)?;
        self.block_line_at_pos(drawn)
    }

    /// What the cell at row `y` of the pane, `column` from its left, shows
    /// of a comment's text as of the last frame: on a row of the text, from
    /// its first column to the box's padding. `within` (a thread and its
    /// comment, as a drag stays in) takes any cell instead: one on the
    /// comment's rows as there, one before them as its text's start, and
    /// one after as its end.
    pub(crate) fn comment_hit(
        &mut self,
        y: usize,
        column: usize,
        within: Option<(usize, usize)>,
    ) -> Option<Hit> {
        let (thread, line) = if let Some((i, _)) = within {
            (i, None)
        } else {
            let (id, line) = self.block_line_at(y)?;
            let Some(Block::Thread(i)) = self.boxes.block(id) else {
                return None;
            };
            (i, Some(line))
        };
        let file = self.boxes.file;
        let layout = self.file_layout(file);
        let (left, boxed, comment_rows) = self.boxes.comment_rows(thread, &layout)?;
        let (comment, row, col) = if let Some((_, k)) = within {
            let text = comment_rows.get(k)?.text.clone();
            let id = self.boxes.id(Block::Thread(thread))?;
            let (top, _) = (self.document).block_span(file, id, arrange(&layout, &self.boxes))?;
            let at = |line: usize| Pos {
                row: top.row + line,
                ..top
            };
            match self.row_at(y) {
                Some(d) if d < at(text.start) => (k, None, 0),
                Some(d) if d < at(text.end) => (
                    k,
                    Some(d.row - top.row - text.start),
                    column.saturating_sub(left),
                ),
                _ => (k, Some(usize::MAX), 0),
            }
        } else {
            let line = line?;
            let k = comment_rows.iter().position(|n| n.text.contains(&line))?;
            let col = column
                .checked_sub(left)
                .filter(|&c| c < comments::inner(boxed))?;
            (k, Some(line - comment_rows[k].text.start), col)
        };
        let bytes = self
            .boxes
            .with_body(thread, comment, boxed, |body| match row {
                Some(row) => body.map.at(row, col),
                None => body.map.text().start..body.map.text().start,
            })?;
        Some(Hit::Comment {
            thread,
            comment,
            bytes,
        })
    }

    /// The thread and comment drawn on row `y` of the pane in the last
    /// frame, by root and id: the comment whose heading is there or above;
    /// a folded thread's first.
    pub(crate) fn comment_at(&mut self, y: usize) -> Option<(String, String)> {
        let (id, line) = self.block_line_at(y)?;
        let Some(Block::Thread(i)) = self.boxes.block(id) else {
            return None;
        };
        let layout = self.file_layout(self.boxes.file);
        let k = self
            .boxes
            .comment_rows(i, &layout)
            .map_or(0, |(_, _, comment_rows)| {
                comment_rows
                    .iter()
                    .rposition(|n| n.first <= line)
                    .unwrap_or(0)
            });
        let thread = &self.boxes.threads[i];
        Some((thread.root.clone(), thread.comments.get(k)?.id.clone()))
    }

    /// The thread and comment the comment selection is in, while they're
    /// on the file shown.
    fn selected_comment(&self) -> Option<(usize, usize)> {
        let selection = self.comment_selection.as_ref()?;
        let i = self
            .threads()
            .iter()
            .position(|t| t.root == selection.root)?;
        let k = self.boxes.threads[i]
            .comments
            .iter()
            .position(|n| n.id == selection.comment)?;
        Some((i, k))
    }

    /// Shows the text selected in a comment reversed, glyph by glyph, on
    /// the rows of it on screen.
    pub(super) fn draw_comment_selection(
        &mut self,
        grid: &mut Grid,
        (x, end, top): (usize, usize, usize),
    ) {
        let Some(range) = self.comment_selection.as_ref().map(CommentSelection::range) else {
            return;
        };
        let Some((i, k)) = self.selected_comment() else {
            return;
        };
        let file = self.boxes.file;
        let layout = self.file_layout(file);
        let Some(id) = self.boxes.id(Block::Thread(i)) else {
            return;
        };
        let Some((first, _)) = (self.document).block_span(file, id, arrange(&layout, &self.boxes))
        else {
            return;
        };
        let Some((left, boxed, comment_rows)) = self.boxes.comment_rows(i, &layout) else {
            return;
        };
        let text = comment_rows[k].text.clone();
        let shown: Vec<(usize, usize)> = (0..self.drawn.len())
            .filter_map(|y| {
                let d = self.drawn[y]?;
                let line = (d.file == file && d.part == first.part)
                    .then(|| d.row.checked_sub(first.row))??;
                text.contains(&line).then(|| (y, line - text.start))
            })
            .collect();
        let inner = comments::inner(boxed);
        for (y, row) in shown {
            // Past the box's padding, a row is cut.
            let cells: Vec<Range<usize>> = (self.boxes)
                .with_body(i, k, boxed, |body| {
                    let cells = body.map.cells(row, range.clone());
                    cells.filter(|c| c.end <= inner).collect()
                })
                .unwrap_or_default();
            for cells in cells {
                grid.toggle(
                    (x + left + cells.start).min(end),
                    top + y,
                    (x + left + cells.end).min(end),
                    Attributes::REVERSE,
                );
            }
        }
    }

    /// The Markdown the comment selection copies, less its line breaks at
    /// the end, and whether that's the whole comment.
    pub(crate) fn comment_selected(&mut self) -> Option<(String, bool)> {
        let selection = self.comment_selection.clone()?;
        let (i, k) = self.selected_comment()?;
        let range = if selection.word {
            selection.range()
        } else {
            let layout = self.file_layout(self.boxes.file);
            let (_, boxed, _) = self.boxes.comment_rows(i, &layout)?;
            (self.boxes).with_body(i, k, boxed, |body| {
                body.map.span(&selection.anchor, &selection.cursor)
            })?
        };
        let text = &self.boxes.threads[i].comments[k].text;
        let all = text.trim_end_matches(['\n', '\r']);
        let copied = text.get(range.clone())?.trim_end_matches(['\n', '\r']);
        Some((
            copied.to_owned(),
            range.start == 0 && copied.len() == all.len(),
        ))
    }

    /// Whether the selection holds the row at `pos`, on `side` when the
    /// row is split.
    pub(crate) fn selection_holds(&mut self, pos: Pos, side: Side) -> bool {
        let Some(s) = &self.selection else {
            return false;
        };
        let (selected, file, (lo, hi)) = (s.side, s.anchor.file, s.range());
        if pos.file != file {
            return false;
        }
        let layout = self.file_layout(file);
        if side != selected && self.split_row(pos, &layout) {
            return false;
        }
        self.row_lines(pos)[selected.index()].is_some_and(|line| (lo..=hi).contains(&line))
    }

    /// Starts a selection on the row at `pos` and `side`, or with none, on
    /// the first row on screen holding code, on its new side when it has
    /// one. Returns whether there was a line to start on.
    pub(crate) fn select(&mut self, at: Option<(Pos, Side)>) -> bool {
        let Some((pos, side, line)) = self.start(at) else {
            return false;
        };
        self.selection = Some(Selection {
            side,
            anchor: pos,
            cursor: pos,
            lines: (line, line),
            bytes: None,
        });
        true
    }

    /// Where [`Doc::select`] starts: the row and line there.
    pub(super) fn start(&mut self, at: Option<(Pos, Side)>) -> Option<(Pos, Side, usize)> {
        let start = at.or_else(|| {
            let rows: Vec<Pos> = (0..self.drawn.len())
                .filter_map(|y| self.row_at(y))
                .collect();
            rows.into_iter().find_map(|pos| {
                let lines = self.row_lines(pos);
                Some((pos, self.side_of(pos.file, lines)?))
            })
        });
        let (pos, side) = start?;
        let line = self.row_lines(pos)[side.index()]?;
        Some((pos, side, line))
    }

    /// The next row after `from` (before it, with `back`) that holds a line
    /// of `side`, within the file.
    fn step_line(&mut self, from: Pos, side: Side, back: bool) -> Option<(Pos, usize)> {
        let layout = self.file_layout(from.file);
        let mut pos = from;
        loop {
            let moved = if back {
                self.prev_row(&mut pos, &layout)
            } else {
                self.next_row(&mut pos, &layout)
            };
            if !moved {
                return None;
            }
            pos.offset = 0;
            if let Some(line) =
                self.document.row_lines(pos, arrange(&layout, &self.boxes))[side.index()]
            {
                return Some((pos, line));
            }
        }
    }

    /// Moves the selection's cursor by `lines` lines of its side (up when
    /// negative), as far as the file goes, and scrolls it into view. A
    /// selection by characters keeps its ends' columns, its cursor moving
    /// to the start of a line going up and to the end going down.
    pub(crate) fn select_by(&mut self, lines: isize) {
        let Some(mut selection) = self.selection.take() else {
            return;
        };
        for _ in 0..lines.unsigned_abs() {
            let Some((pos, line)) = self.step_line(selection.cursor, selection.side, lines < 0)
            else {
                break;
            };
            selection.cursor = pos;
            selection.lines.1 = line;
        }
        if selection.bytes.is_some() {
            let end = if lines < 0 {
                0
            } else {
                self.line_text(selection.anchor.file, selection.side, selection.lines.1)
                    .map_or(0, <[u8]>::len)
            };
            if let Some((_, cursor)) = &mut selection.bytes {
                *cursor = end..end;
            }
        }
        let cursor = selection.cursor;
        self.selection = Some(selection);
        self.reveal(cursor);
    }

    /// Switches the selection to the other side, from the nearest rows
    /// holding lines of it between its ends. Returns false when there are
    /// none.
    pub(crate) fn select_other_side(&mut self) -> bool {
        let Some(selection) = self.selection.clone() else {
            return false;
        };
        let side = selection.side.other();
        let (first, last) = if selection.anchor <= selection.cursor {
            (selection.anchor, selection.cursor)
        } else {
            (selection.cursor, selection.anchor)
        };
        let found = |doc: &mut Self, from: Pos, back: bool| match doc.row_lines(from)[side.index()]
        {
            Some(line) => Some((from, line)),
            None => doc
                .step_line(from, side, back)
                .filter(|(p, _)| if back { *p >= first } else { *p <= last }),
        };
        let (Some(low), Some(high)) = (found(self, first, false), found(self, last, true)) else {
            return false;
        };
        let forward = selection.anchor <= selection.cursor;
        let (anchor, cursor) = if forward { (low, high) } else { (high, low) };
        // Columns on one side mean nothing on the other: whole lines.
        self.selection = Some(Selection {
            side,
            anchor: anchor.0,
            cursor: cursor.0,
            lines: (anchor.1, cursor.1),
            bytes: None,
        });
        true
    }

    /// The selected lines of their side, as the file has them, or the
    /// selected part of them. With `decorations`, each line starts with its
    /// number and its sign as a unified diff has it (`12 +code`).
    pub(crate) fn selected(&mut self, decorations: bool) -> Option<Copied> {
        let selection = self.selection.clone()?;
        let file = selection.anchor.file;
        let (lo, hi) = selection.range();
        let document = &self.document;
        let loaded = document.file(file)?;
        if let Some(rendered) = loaded.rendered() {
            return Some(rendered::copy(rendered, selection.side, lo, hi));
        }
        let partial = loaded.is_partial();
        let mut hidden = 0;
        // A piped file's hidden lines aren't known, so they're left out.
        let mut unknown = Vec::new();
        for (g, &span) in loaded.gaps().iter().enumerate() {
            let start = match selection.side {
                Side::Old => span.old,
                Side::New => span.new,
            };
            for run in document.shown(file, g)?.hidden_lines() {
                let (from, to) = ((start + run.start).max(lo), (start + run.end).min(hi + 1));
                hidden += to.saturating_sub(from);
                if partial && to > from {
                    unknown.push(from..to);
                }
            }
        }
        let diff = loaded.diff();
        let text = match selection.side {
            Side::Old => &diff.old,
            Side::New => &diff.new,
        };
        let bytes = selection.byte_range();
        // The side's changed lines, for the signs.
        let changed: Vec<usize> = if decorations {
            diff.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter_map(|l| match (l.kind, selection.side) {
                    (LineKind::Removed, Side::Old) => Some(l.old),
                    (LineKind::Added, Side::New) => Some(l.new),
                    _ => None,
                })
                .collect()
        } else {
            Vec::new()
        };
        let digits = (hi + 1).to_string().len();
        let mut out = Vec::new();
        let mut code_chars = 0;
        for i in lo..(hi + 1).min(text.len()) {
            if unknown.iter().any(|r| r.contains(&i)) {
                continue;
            }
            if decorations {
                // In line order, as the hunks are.
                let sign = match (changed.binary_search(&i).is_ok(), selection.side) {
                    (false, _) => ' ',
                    (true, Side::Old) => '-',
                    (true, Side::New) => '+',
                };
                out.extend_from_slice(format!("{:>digits$} {sign}", i + 1).as_bytes());
            }
            let before = out.len();
            let line = text.line(i);
            match bytes {
                Some((from, to)) => {
                    let content = display_text(line.strip_suffix(b"\n").unwrap_or(line));
                    let start = if i == lo { from.min(content.len()) } else { 0 };
                    if i == hi {
                        out.extend_from_slice(&content[start..to.clamp(start, content.len())]);
                    } else {
                        out.extend_from_slice(&line[start..]);
                    }
                }
                None => out.extend_from_slice(line),
            }
            code_chars += String::from_utf8_lossy(&out[before..])
                .trim_end_matches(['\n', '\r'])
                .chars()
                .count();
        }
        // No line break after the last line: pasted into a shell, it would
        // run what was copied.
        if out.ends_with(b"\n") {
            out.pop();
            if out.ends_with(b"\r") {
                out.pop();
            }
        }
        let missing = if partial { hidden } else { 0 };
        Some(Copied {
            chars: (bytes.is_some() && lo == hi).then_some(code_chars),
            decorated: decorations,
            text: out,
            lines: hi - lo + 1 - missing,
            hidden: hidden - missing,
            missing,
        })
    }
}

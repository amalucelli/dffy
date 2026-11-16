//! The viewer's document on screen: the core's [`Document`] (every file of
//! the diff, one shown at a time, and where navigation by row goes) plus
//! what depends on the terminal: how many screen rows a row takes when it
//! wraps, scrolling and keeping the screen full, opening a file a few rows
//! above its first change, drawing, the overview column, selections and the
//! mouse. The file's name, and the hunk or function the top row is in, are
//! the viewer's to show in its frame; nothing here takes a row for them.
//!
//! Rows come from the same [`Layout`] code print mode uses, so both
//! frontends draw identical lines, less print's file headers.

use std::ops::Range;
use std::sync::Arc;

use dffy_core::bstr::BString;
use dffy_core::diff::LineKind;
use dffy_core::document::{
    self, Content, Counts, Document, File, Mark, Missing, NO_NEWLINE_LINE, Pos, Render, Segment,
    Settle, Side, Source,
};
use dffy_core::git::{FileDiff, Update};
use dffy_core::patch;
use dffy_core::view::{Arrangement, Attributes, HunkHeader, Row, display_text};

use super::piped::Body;
use crate::pager;
use crate::render::{Canvas, Layout};
use crate::term::Grid;
use crate::theme::{Ink, Role, Style, Theme};

pub(crate) mod rendered;

/// Rows kept above a file's first change when it opens there.
const OPEN_CONTEXT: isize = 3;

/// An entry of the viewer's document: a piped file's hunks wait in the
/// spill until it's drawn.
pub(crate) type Entry = document::Entry<Body>;

/// `layout`'s arrangement of a hunk, for the core's row queries. Every call
/// goes through this one function so the row code is built once.
#[inline]
fn arrange(layout: &Layout) -> impl Fn(&HunkHeader<'_>) -> Arrangement + '_ {
    move |header| layout.arrangement(header)
}

/// Lines selected for copying, on one side of a file: from the row the
/// selection started on to the row of its cursor, both rows holding a line
/// of that side. What's copied is that side's text from the one line to
/// the other, hidden lines between them included.
///
/// A selection made by dragging over code is by characters: it also holds
/// the bytes of the glyph under the press and under the pointer, within
/// their lines (empty past a line's end), and runs from the earlier
/// glyph's start to the later one's end. Without them it's whole lines,
/// as `v` and a drag over line numbers select.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    pub side: Side,
    anchor: Pos,
    cursor: Pos,
    /// The lines (0-based) of `side` on the anchor's and the cursor's rows.
    lines: (usize, usize),
    bytes: Option<(Range<usize>, Range<usize>)>,
}

impl Selection {
    fn range(&self) -> (usize, usize) {
        (
            self.lines.0.min(self.lines.1),
            self.lines.0.max(self.lines.1),
        )
    }

    /// For a selection by characters, the byte its first line starts at
    /// and the byte its last line ends at.
    fn byte_range(&self) -> Option<(usize, usize)> {
        let (a, b) = self.bytes.as_ref()?;
        Some(if (self.lines.0, a.start) <= (self.lines.1, b.start) {
            (a.start, b.end)
        } else {
            (b.start, a.end)
        })
    }

    /// Lines from the first to the last, hidden ones included.
    pub(crate) fn len(&self) -> usize {
        let (lo, hi) = self.range();
        hi - lo + 1
    }
}

/// What a cell of the pane shows, for the mouse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hit {
    /// Code: its row, side and line (0-based), and the bytes of the glyph
    /// there within the line, empty where the line has none.
    Code {
        pos: Pos,
        side: Side,
        line: usize,
        bytes: Range<usize>,
    },
    /// The line numbers and marks left of a line's code.
    Gutter { pos: Pos, side: Side, line: usize },
}

/// A copied selection, for the status bar.
pub(crate) struct Copied {
    pub text: Vec<u8>,
    pub lines: usize,
    /// Characters copied, for a selection within one line.
    pub chars: Option<usize>,
    /// Line numbers and signs came with the code.
    pub decorated: bool,
    /// Lines copied that a `⋯` separator hides.
    pub hidden: usize,
    /// Lines between the first and the last that a piped diff doesn't
    /// have, so they aren't in the copy.
    pub missing: usize,
}

/// Where the top row is, for the title: the hunk it's in, or the heading
/// above it in revealed unchanged lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Context {
    /// `@@ -a,b +c,d @@`; `None` outside a hunk.
    pub numbers: Option<String>,
    /// The function or section heading, git's kind; may be empty.
    pub heading: String,
}

pub(crate) struct Doc {
    pub document: Document<Body>,
    pub layout: Layout,
    /// Screen rows available for the document.
    pub height: usize,
    scratch: Canvas,
    /// For rows rendered again to find which byte a cell shows.
    mapped: Canvas,
    /// The last column of the drawing area is kept for the overview of the
    /// file at the top.
    pub overview: bool,
    /// Separator rows drawn in the last frame, by screen row from the top
    /// of the document.
    separators: Vec<(usize, Pos)>,
    /// The separator `x` reveals: the first one on screen.
    target: Option<Pos>,
    /// The logical row drawn on each screen row of the pane in the last
    /// frame; `None` for empty rows.
    drawn: Vec<Option<Pos>>,
    /// Where the last frame's pane was drawn; the next frame reuses its
    /// rows only when drawn there again.
    painted: Option<(usize, usize, usize)>,
    pub selection: Option<Selection>,
}

impl Doc {
    /// An empty document. Only watch mode shows one; everything that moves
    /// the position does nothing until a file arrives.
    pub(crate) fn new(layout: Layout, height: usize) -> Self {
        Self {
            document: Document::default(),
            layout,
            height,
            scratch: Canvas::default(),
            mapped: Canvas::mapped(),
            overview: false,
            separators: Vec::new(),
            target: None,
            drawn: Vec::new(),
            painted: None,
            selection: None,
        }
    }

    /// The layout of a file's hunks. A rendered view always wraps, since
    /// renderers leave paragraphs as single lines, and numbers its source.
    fn file_layout(&self, file: usize) -> Layout {
        let Some(entry) = self.document.files().get(file) else {
            return self.layout;
        };
        let loaded = entry.file();
        if let Some(rendered) = loaded.and_then(File::rendered) {
            let layout = Layout {
                wrap: true,
                words: true,
                scroll: 0,
                ..self.layout
            };
            return layout.for_file(entry.status(), rendered.source_len());
        }
        let lines = loaded.map_or(0, |l| l.diff().old.len().max(l.diff().new.len()));
        self.layout.for_file(entry.status(), lines)
    }

    /// Keeps the position inside the document after rows changed under it
    /// (a diff arrived, the layout changed).
    pub(crate) fn normalize(&mut self) {
        let layout = self.file_layout(self.document.pos.file);
        if !self.document.normalize(arrange(&layout)) {
            return;
        }
        let pos = self.document.pos;
        self.document.pos.offset = pos.offset.min(self.height(pos, &layout) - 1);
        self.clamp_in(&layout);
    }

    /// Puts the view where `settle` says, once the core moved the position.
    fn settle(&mut self, settle: Settle) {
        match settle {
            Settle::Keep => {}
            Settle::Land(at) => self.land(at),
            Settle::Open { file, landing } => self.place(file, landing),
            Settle::Edge { file, last } => self.land_edge(file, last),
            Settle::Wait { last, .. } => {
                self.document.pending_land = Some((self.document.pos, last));
            }
        }
    }

    /// Settles after a rendered view was swapped: the file shown opens
    /// anew, and positions in it are void.
    fn settle_render(&mut self, settle: Settle) {
        if let Settle::Open { .. } = settle {
            self.selection = None;
            self.settle(settle);
            self.normalize();
        }
    }

    /// Puts the files in path order (see [`Document::sort_by_path`]).
    pub(crate) fn sort_by_path(&mut self) -> bool {
        let moved = self.document.sort_by_path();
        if moved {
            self.selection = None;
        }
        moved
    }

    /// Takes the refreshed file list (see [`Document::replace`]). A
    /// selection outlives other files changing, not its own.
    pub(crate) fn replace(&mut self, update: &Update) {
        let selected = self.selection.take().and_then(|s| {
            let path = self
                .document
                .files()
                .get(s.anchor.file)?
                .change()
                .path
                .clone();
            Some((s, path))
        });
        let open = self.document.replace(update);
        let files = self.document.files();
        if let Some((mut selection, path)) = selected
            && let Some(file) = files
                .iter()
                .position(|e| !e.is_commit() && e.change().path == path)
            && !files[file].is_stale()
        {
            selection.anchor.file = file;
            selection.cursor.file = file;
            self.selection = Some(selection);
        }
        if let Some(file) = open {
            self.normalize();
            self.place(file, false);
        }
        self.normalize();
    }

    /// Stores a worker's diff (see [`Document::load`]); a selection in the
    /// file goes with the diff it was of.
    pub(crate) fn load(&mut self, file: usize, result: Result<Box<File>, String>) -> bool {
        let Some(settle) = self.document.load(file, result) else {
            return false;
        };
        if self
            .selection
            .as_ref()
            .is_some_and(|s| s.anchor.file == file)
        {
            self.selection = None;
        }
        self.settle(settle);
        // Off the blank row it had without segments.
        if self.document.pos.file == file {
            self.normalize();
        }
        true
    }

    /// Opens `file`, shown and diffed for the first time, at its first
    /// change with [`OPEN_CONTEXT`] rows above it; at its top when the
    /// change shows on the first screen from there anyway. With `landing`
    /// (`n` opened it) the next `n` goes on past that change either way.
    fn place(&mut self, file: usize, landing: bool) {
        let layout = self.file_layout(file);
        let change = self.document.open(file, arrange(&layout));
        let top = self.document.pos;
        let Some(change) = change else {
            return;
        };
        let context = OPEN_CONTEXT.unsigned_abs();
        let limit = self.height.saturating_sub(context).max(context + 1);
        let (mut pos, mut rows) = (top, 0);
        while rows < limit {
            if (pos.part, pos.row) == (change.part, change.row) {
                if landing {
                    self.document.landed(change);
                }
                return;
            }
            rows += self.height(pos, &layout);
            if !self.next_row(&mut pos, &layout) {
                return;
            }
        }
        self.document.pos = change;
        self.scroll_in(-OPEN_CONTEXT, &layout);
        self.document.landed(change);
    }

    /// Stores a worker's rendering (see [`Document::set_rendered`]).
    pub(crate) fn set_rendered(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Result<Box<File>, String>,
    ) -> bool {
        let Some(settle) = self.document.set_rendered(file, diff, result) else {
            return false;
        };
        let swapped = matches!(self.document.files()[file].render(), Some(Render::Shown(_)));
        if swapped
            && self
                .selection
                .as_ref()
                .is_some_and(|s| s.anchor.file == file)
        {
            self.selection = None;
        }
        self.settle_render(settle);
        true
    }

    /// Whether the selection's file shows a rendered view, as long as
    /// there's a selection.
    fn selection_rendered(&self) -> Option<bool> {
        let selection = self.selection.as_ref()?;
        let entry = self.document.files().get(selection.anchor.file);
        Some(matches!(
            entry.and_then(Entry::render),
            Some(Render::Shown(_))
        ))
    }

    /// Turns rendered views on or off for every file that has one (see
    /// [`Document::set_rendered_mode`]).
    pub(crate) fn set_rendered_mode(&mut self, on: bool) {
        let before = self.selection_rendered();
        let settle = self.document.set_rendered_mode(on);
        if self.selection_rendered() != before {
            self.selection = None;
        }
        self.settle_render(settle);
        self.normalize();
    }

    /// Drops rendered views drawn for another width than `width` (see
    /// [`Document::forget_renders`]).
    pub(crate) fn forget_renders(&mut self, width: usize) {
        let settle = self.document.forget_renders(width);
        self.settle_render(settle);
        self.normalize();
    }

    fn is_rendered(&self, file: usize) -> bool {
        self.document
            .file(file)
            .is_some_and(|l| l.rendered().is_some())
    }

    /// The line of the input a sheet shows at `row`: a commit's header and
    /// message, or a combined diff's line; `None` for the rows made here.
    fn sheet_text(&self, file: usize, row: usize) -> Option<(Role, BString)> {
        let (role, text) = match self.document.files().get(file)?.source() {
            Source::Commit(commit) => {
                let (kind, text) = commit.lines.get(row)?;
                (pager::text_role(*kind), &text[..])
            }
            Source::Piped(piped) => {
                let (parents, line) = piped.combined.as_ref()?.get(row)?;
                (
                    pager::combined_role(patch::combined_kind(*parents, line)),
                    &line[..],
                )
            }
            Source::Repo => return None,
        };
        Some((role, display_text(text).into()))
    }

    /// Row `row` of a sheet: a commit's lines, then how many files it has
    /// and a row per file; or a combined diff's lines. Long lines are cut
    /// and scroll sideways like code.
    fn sheet_row(&self, file: usize, row: usize, c: &mut Canvas) {
        if let Some((role, line)) = self.sheet_text(file, row) {
            self.layout.text_row(role, &line, c);
            return;
        }
        let files = self.document.files();
        let Source::Commit(commit) = files[file].source() else {
            c.end_line();
            return;
        };
        let commit_files = self.document.commit_files(file);
        match row - commit.lines.len() {
            0 => c.end_line(),
            1 => {
                let totals =
                    files[commit_files.clone()]
                        .iter()
                        .try_fold(Counts::default(), |t, e| {
                            let n = e.counts()?;
                            Some(Counts {
                                added: t.added + n.added,
                                removed: t.removed + n.removed,
                            })
                        });
                let s = if commit_files.len() == 1 { "" } else { "s" };
                c.push(Role::Dim, &format!("{} file{s} ", commit_files.len()));
                // Until every file is counted: a partial sum would read as
                // the commit's total.
                let Some(Counts { added, removed }) = totals else {
                    c.line(Role::Dim, "…");
                    return;
                };
                c.push(Role::Added, &format!("+{added}"));
                c.push(Role::Plain, " ");
                c.line(Role::Removed, &format!("-{removed}"));
            }
            n => {
                let Some(entry) = files.get(commit_files.start + n - 2) else {
                    c.end_line();
                    return;
                };
                let (letter, role) = crate::render::stat::status_letter(entry.status());
                c.push(Role::Plain, "  ");
                c.push(role, letter);
                c.push(Role::Plain, " ");
                c.push(Role::Plain, &crate::render::escape(&entry.change().path));
                if let Some(n) = entry.counts() {
                    if n.added > 0 {
                        c.push(Role::Added, &format!(" +{}", n.added));
                    }
                    if n.removed > 0 {
                        c.push(Role::Removed, &format!(" -{}", n.removed));
                    }
                }
                c.end_line();
            }
        }
    }

    /// What shows instead of hunks: loading, an error, or why there are
    /// none (binary, too large, only whitespace).
    fn note(&self, file: usize) -> Option<&str> {
        let entry = self.document.files().get(file)?;
        if entry.is_sheet() {
            return None;
        }
        match entry.state() {
            document::State::Pending => Some("loading…"),
            document::State::Loaded(l) => l.note(),
            document::State::Failed(e) => Some(e.as_str()),
        }
    }

    /// Appends the screen rows of the logical row at `pos` (ignoring its
    /// offset), drawn with its file's `layout`. Returns whether it's a
    /// gap's separator.
    fn render(&mut self, pos: Pos, layout: &Layout, c: &mut Canvas) -> bool {
        if pos.part == 0 {
            if self
                .document
                .files()
                .get(pos.file)
                .is_some_and(Entry::is_sheet)
            {
                self.sheet_row(pos.file, pos.row, c);
            } else {
                // The note in the middle of the pane says why it's empty.
                c.end_line();
            }
            return false;
        }
        let target = self.target == Some(Pos { offset: 0, ..pos });
        match self.document.row(pos, arrange(layout)) {
            Some(Content::Start(header)) => {
                layout.hunk_row(&header, Row::Header, |_| NO_NEWLINE_LINE, c);
            }
            Some(Content::Hunk { header, row, lines }) => {
                layout.hunk_row(&header, row, |i| lines.line(i), c);
            }
            Some(Content::Separator { hidden, next }) => {
                layout.separator(hidden, next.as_ref(), target, c);
                return true;
            }
            Some(Content::Gap { old, new, line }) => {
                // Any header does: the file's layout fixes the gutter.
                let header = HunkHeader {
                    old_start: old + 1,
                    old_len: 1,
                    new_start: new + 1,
                    new_len: 1,
                    heading: b"",
                    hidden: 0,
                };
                let row = match layout.arrangement(&header) {
                    Arrangement::Split => Row::Pair(Some(0), Some(0)),
                    Arrangement::Unified => Row::Line(0),
                };
                layout.hunk_row(&header, row, |_| line, c);
            }
            None => {}
        }
        false
    }

    /// Screen rows the logical row at `pos` takes, in its file's `layout`.
    fn height(&mut self, pos: Pos, layout: &Layout) -> usize {
        // Rendered views show only while the mode is on, so a line diff
        // without wrapping pays one flag here.
        if pos.part == 0
            || !(self.layout.wrap || (self.document.rendered() && self.is_rendered(pos.file)))
        {
            return 1;
        }
        let mut c = std::mem::take(&mut self.scratch);
        c.clear();
        self.render(pos, layout, &mut c);
        let n = c.rows().count().max(1);
        self.scratch = c;
        n
    }

    /// Moves `pos` to the next logical row of its file; false at the end.
    #[inline]
    fn next_row(&mut self, pos: &mut Pos, layout: &Layout) -> bool {
        self.document.next_row(pos, arrange(layout))
    }

    /// Moves `pos` to the last screen row of the previous logical row of
    /// its file; false at the start.
    fn prev_row(&mut self, pos: &mut Pos, layout: &Layout) -> bool {
        let mut prev = *pos;
        if !self.document.prev_row(&mut prev, arrange(layout)) {
            return false;
        }
        prev.offset = self.height(prev, layout) - 1;
        *pos = prev;
        true
    }

    /// Scrolls the code by `delta` screen rows, then keeps the screen full.
    pub(crate) fn scroll(&mut self, delta: isize) {
        let layout = self.file_layout(self.document.pos.file);
        self.scroll_in(delta, &layout);
    }

    /// [`Doc::scroll`] with the file's layout at hand.
    fn scroll_in(&mut self, delta: isize, layout: &Layout) {
        if self.document.files().is_empty() {
            return;
        }
        let mut pos = self.document.pos;
        for _ in 0..delta.unsigned_abs() {
            let moved = if delta > 0 {
                if pos.offset + 1 < self.height(pos, layout) {
                    pos.offset += 1;
                    true
                } else {
                    self.next_row(&mut pos, layout)
                }
            } else if pos.offset > 0 {
                pos.offset -= 1;
                true
            } else {
                self.prev_row(&mut pos, layout)
            };
            if !moved {
                break;
            }
        }
        self.document.pos = pos;
        self.clamp_in(layout);
    }

    /// Pulls the position up when the rows below it don't fill the screen.
    pub(crate) fn clamp(&mut self) {
        let layout = self.file_layout(self.document.pos.file);
        self.clamp_in(&layout);
    }

    /// [`Doc::clamp`] with the file's layout at hand.
    fn clamp_in(&mut self, layout: &Layout) {
        if self.document.files().is_empty() {
            return;
        }
        let mut pos = self.document.pos;
        let first = self.height(pos, layout);
        let mut rows = first - pos.offset.min(first - 1);
        while rows < self.height {
            if !self.next_row(&mut pos, layout) {
                break;
            }
            rows += self.height(pos, layout);
        }
        while rows < self.height {
            let mut pos = self.document.pos;
            let moved = if pos.offset > 0 {
                pos.offset -= 1;
                true
            } else {
                self.prev_row(&mut pos, layout)
            };
            if !moved {
                break;
            }
            self.document.pos = pos;
            rows += 1;
        }
    }

    /// The hunk or function the top row is in: its hunk, or in revealed
    /// unchanged lines the heading above them, else the hunk above. `None`
    /// before the first hunk and for a whole added or deleted file.
    pub(crate) fn context(&mut self) -> Option<Context> {
        Some(match self.document.context()? {
            document::Context::Heading(text) => Context {
                numbers: None,
                heading: crate::render::escape(display_text(text)),
            },
            document::Context::Hunk(header) => Context {
                numbers: Some(header.numbers()),
                heading: crate::render::escape(display_text(header.heading)),
            },
        })
    }

    pub(crate) fn bottom(&mut self) {
        if self.document.files().is_empty() {
            return;
        }
        let file = self.document.pos.file;
        let layout = self.file_layout(file);
        let part = self.document.parts(file) - 1;
        let row = self.document.rows(file, part, arrange(&layout)) - 1;
        let mut pos = Pos {
            file,
            part,
            row,
            offset: 0,
        };
        pos.offset = self.height(pos, &layout) - 1;
        self.document.pos = pos;
        self.clamp_in(&layout);
    }

    /// Shows `file` where it was left, or the first time at its first
    /// change (see [`Doc::place`]), once it's diffed.
    pub(crate) fn goto_file(&mut self, file: usize) {
        if let Some(settle) = self.document.goto_file(file) {
            self.normalize();
            self.settle(settle);
        }
    }

    pub(crate) fn next_file(&mut self) {
        self.goto_file(self.document.pos.file + 1);
    }

    pub(crate) fn prev_file(&mut self) {
        if let Some(file) = self.document.pos.file.checked_sub(1) {
            self.goto_file(file);
        }
    }

    /// Puts the row at `at`, which starts a block of changes, at the top.
    fn land(&mut self, at: Pos) {
        self.document.pos = at;
        self.clamp();
        self.document.landed(at);
    }

    /// Puts the first (or `last`) block of changes of `file`, the file
    /// shown, at the top.
    fn land_edge(&mut self, file: usize, last: bool) {
        let layout = self.file_layout(file);
        if let Some(at) = self.document.edge(file, last, arrange(&layout)) {
            self.land(at);
        }
    }

    /// Settles a move by change; returns the file it opened, if another.
    fn settle_change(&mut self, settle: Settle) -> Option<usize> {
        match settle {
            Settle::Keep => None,
            Settle::Land(at) => {
                self.land(at);
                None
            }
            Settle::Open { file, .. } | Settle::Edge { file, .. } | Settle::Wait { file, .. } => {
                self.normalize();
                self.settle(settle);
                Some(file)
            }
        }
    }

    /// Puts the next block of changed lines at the top. Past the file's
    /// last, opens the next file at its first change (or its top while it
    /// isn't diffed) and returns that file.
    pub(crate) fn next_change(&mut self) -> Option<usize> {
        let layout = self.file_layout(self.document.change_file());
        let settle = self.document.next_change(arrange(&layout))?;
        self.settle_change(settle)
    }

    /// Puts the previous block of changed lines at the top. Before the
    /// file's first, opens the previous file at its last change (or its
    /// top while it isn't diffed) and returns that file.
    pub(crate) fn prev_change(&mut self) -> Option<usize> {
        let layout = self.file_layout(self.document.change_file());
        let settle = self.document.prev_change(arrange(&layout))?;
        self.settle_change(settle)
    }

    /// Reveals more of gap `gap` of `file` (see [`Document::expand`]).
    /// Returns whether any line was hidden.
    pub(crate) fn expand(&mut self, file: usize, gap: usize) -> bool {
        if !self.document.expand(file, gap) {
            return false;
        }
        self.normalize();
        true
    }

    /// `x`: reveals the highlighted separator's gap, or with none on
    /// screen the gaps around the hunk at the top. Returns whether there
    /// was anything to reveal.
    pub(crate) fn expand_target(&mut self) -> bool {
        if let Some(t) = self.target
            && let Some(Segment::Gap(g)) = self.document.segment(t.file, t.part)
        {
            return self.expand(t.file, g);
        }
        let Pos { file, part, .. } = self.document.pos;
        match self.document.segment(file, part) {
            Some(Segment::Gap(g)) => self.expand(file, g),
            Some(Segment::Hunk(h)) => {
                let before = self.expand(file, h);
                self.expand(file, h + 1) || before
            }
            Some(Segment::Start) | None => self.expand(file, 0) || self.expand(file, 1),
        }
    }

    /// A click on screen row `y` of the document: reveals more of the gap
    /// when it's a separator.
    pub(crate) fn click(&mut self, y: usize) {
        if let Some(&(_, p)) = self.separators.iter().find(|(row, _)| *row == y)
            && let Some(Segment::Gap(g)) = self.document.segment(p.file, p.part)
        {
            self.expand(p.file, g);
        }
    }

    /// `X`: switches every file between its changes and its full text,
    /// keeping the line at the top where it is.
    pub(crate) fn toggle_full(&mut self) {
        if self.document.toggle_full() {
            self.normalize();
        }
    }

    /// The new-side line number at the top of the screen, for the editor.
    pub(crate) fn line_at_top(&mut self) -> usize {
        let line = self.line_of(self.document.pos);
        let file = self.document.file(self.document.pos.file);
        match file.and_then(File::rendered) {
            Some(rendered) => rendered.source_lines(Side::New, line - 1, line - 1).start + 1,
            None => line,
        }
    }

    /// Whether the overview column shows for `file`: in its full text, or
    /// when its text is taller than the screen.
    fn has_overview(&self, file: usize) -> bool {
        let document = &self.document;
        document.file(file).is_some_and(|l| {
            l.parts() > 1
                && (document.is_full(&document.files()[file]) || l.overview_lines() > self.height)
        })
    }

    /// Draws the overview of the file at the top into column `x`: a mark
    /// per screen row holding changed lines, and a thumb over the lines on
    /// screen, `lines` (1-based).
    fn draw_overview(
        &mut self,
        grid: &mut Grid,
        x: usize,
        top: usize,
        lines: (usize, usize),
        theme: &Theme,
    ) {
        let (file, height) = (self.document.pos.file, self.height);
        let modified = self.layout.modified_lines;
        let Some(total) = self.document.file(file).map(File::overview_lines) else {
            return;
        };
        let marks = self.document.marks(file, modified);
        let (first, last) = (lines.0.saturating_sub(1), lines.1.saturating_sub(1));
        for r in 0..height {
            let lo = r * total / height;
            let hi = ((r + 1) * total / height).max(lo + 1);
            let from = marks.partition_point(|m| m.0 < lo);
            let mark = marks[from..]
                .iter()
                .take_while(|m| m.0 < hi)
                .map(|m| m.1)
                .max();
            // On screen, marks widen to the whole cell: the thumb stays
            // visible where every row has changes.
            let thumb = lo <= last && hi > first;
            let text = if thumb { "█" } else { "▐" };
            let (text, role) = match mark {
                Some(Mark::Added) => (text, Role::Added),
                Some(Mark::Removed) => (text, Role::Removed),
                Some(Mark::Modified) => (text, Role::Modified),
                Some(Mark::Moved) => (text, Role::Moved),
                None if thumb => ("┃", Role::Dim),
                None => continue,
            };
            grid.put_str(x, top + r, x + 1, text, theme.style(role));
        }
    }

    /// A click on screen row `y` of the overview column: puts the first
    /// block of changes at or after the lines that row stands for at the
    /// top, or the last one before.
    pub(crate) fn overview_click(&mut self, y: usize) {
        let file = self.document.pos.file;
        if !self.has_overview(file) {
            return;
        }
        let Some(total) = self.document.file(file).map(File::overview_lines) else {
            return;
        };
        let target = y * total / self.height.max(1);
        let layout = self.file_layout(file);
        if let Some(at) = self.document.change_after(file, target, arrange(&layout)) {
            self.land(at);
        }
    }

    /// The new-side line number (1-based) that the row at `pos` shows, or
    /// the first after it.
    fn line_of(&mut self, pos: Pos) -> usize {
        let layout = self.file_layout(pos.file);
        self.document.line_of(pos, arrange(&layout))
    }

    /// The work missing for the rows drawn in the last frame, with the file
    /// they're of (see [`Document::missing`]).
    pub(crate) fn missing(&self) -> impl Iterator<Item = (usize, Missing)> + '_ {
        let first = self.drawn.first().copied().flatten();
        let last = self.drawn.iter().rev().find_map(|p| *p);
        first.zip(last).into_iter().flat_map(|(first, last)| {
            self.document
                .missing(first, last)
                .map(move |missing| (first.file, missing))
        })
    }

    /// Draws from the position down into rows `top..top + self.height` of
    /// `grid`, columns `x..end`. The first separator on screen is drawn
    /// highlighted, as what `x` reveals.
    ///
    /// `previous` is the last frame's grid, given when nothing but the
    /// position changed since. When the position is still within a screen
    /// of the last frame's, the rows still on screen are copied from it
    /// and only the rows that came into view are laid out.
    ///
    /// Returns the files drawn, and when rows were copied, the pane's rows
    /// of the grid and how many rows the view scrolled down by (up when
    /// negative).
    pub(crate) fn draw(
        &mut self,
        grid: &mut Grid,
        previous: Option<&Grid>,
        (x, end, top): (usize, usize, usize),
        theme: &Theme,
        frame: u64,
    ) -> (Range<usize>, Option<(Range<usize>, isize)>) {
        let target = self.target.take();
        let painted = self.painted.take();
        if self.document.files().is_empty() {
            self.separators.clear();
            return (0..0, None);
        }
        let overview = (self.overview && end > x + 1).then_some(end - 1);
        let end = overview.unwrap_or(end);
        let mut c = Canvas::default();
        let file = self.document.pos.file;
        let layout = self.file_layout(file);
        let area = (x, end, top);
        self.painted = Some(area);
        let reused = previous
            .filter(|_| painted == self.painted)
            .and_then(|previous| self.reuse(grid, previous, area, &layout));
        let (scrolled, rows, mut pos) = if let Some((by, rows, from)) = reused {
            (Some(by), rows, from)
        } else {
            self.separators.clear();
            self.drawn.clear();
            self.drawn.resize(self.height, None);
            (None, 0..self.height, self.document.pos)
        };
        let kept = self.separators.len();
        let mut y = rows.start;
        let mut last = pos;
        while y < rows.end {
            c.clear();
            if self.render(pos, &layout, &mut c) && pos.offset == 0 {
                self.separators.push((y, pos));
            }
            for (i, row) in c.rows().enumerate().skip(pos.offset) {
                if y == rows.end {
                    break;
                }
                let after = paint(grid, x, top + y, end, row, theme, Attributes::NONE);
                if let Some(fill) = c.fill(i) {
                    grid.fill(after, top + y, end, theme.ink_style(fill));
                }
                self.drawn[y] = Some(Pos { offset: i, ..pos });
                y += 1;
            }
            last = pos;
            if !self.next_row(&mut pos, &layout) {
                break;
            }
        }
        // Rows drawn above the copied ones: their separators come first.
        if rows.start == 0 {
            let new = self.separators.len() - kept;
            self.separators.rotate_right(new);
        }
        let last = self
            .drawn
            .last()
            .copied()
            .flatten()
            .map_or(last, |p| Pos { offset: 0, ..p });
        self.draw_selection(grid, area, &layout);
        self.draw_search(grid, area, theme, &layout, rows);
        self.document.touch(file, frame);
        if let Some(note) = self.note(file) {
            let text_width = unicode_width::UnicodeWidthStr::width(note);
            let left = x + end.saturating_sub(x + text_width) / 2;
            let line = top + y.max(self.height / 2).min(self.height.saturating_sub(1));
            grid.put_str(left, line, end, note, theme.style(Role::Dim));
        }
        if let Some(column) = overview
            && self.has_overview(file)
        {
            let lines = (
                self.document.line_of(self.document.pos, arrange(&layout)),
                self.document.line_of(last, arrange(&layout)),
            );
            self.draw_overview(grid, column, top, lines, theme);
        }
        let copied = target.filter(|_| scrolled.is_some());
        self.draw_target(grid, &mut c, copied, area, &layout, theme);
        (
            file..file + 1,
            scrolled.map(|by| (top..top + self.height, by)),
        )
    }

    /// Draws the first separator on screen again, highlighted as what `x`
    /// reveals. `copied` is the separator highlighted in the rows copied
    /// from the last frame: when one scrolled in above it, it's drawn
    /// plain again.
    fn draw_target(
        &mut self,
        grid: &mut Grid,
        c: &mut Canvas,
        copied: Option<Pos>,
        (x, end, top): (usize, usize, usize),
        layout: &Layout,
        theme: &Theme,
    ) {
        if let Some(old) = copied
            && let Some(&(y, _)) = self.separators.iter().skip(1).find(|(_, p)| *p == old)
        {
            c.clear();
            self.render(old, layout, c);
            if let Some(row) = c.rows().next() {
                grid.fill(x, top + y, end, Style::PLAIN);
                let after = paint(grid, x, top + y, end, row, theme, Attributes::NONE);
                if let Some(fill) = c.fill(0) {
                    grid.fill(after, top + y, end, theme.ink_style(fill));
                }
            }
        }
        if let Some(&(y, pos)) = self.separators.first() {
            self.target = Some(pos);
            c.clear();
            self.render(pos, layout, c);
            if let Some(row) = c.rows().next() {
                grid.fill(x, top + y, end, Style::PLAIN);
                paint(grid, x, top + y, end, row, theme, Attributes::NONE);
            }
        }
    }

    /// Copies the rows of `previous`, the last frame, that are still on
    /// screen to where they go now, and moves what's kept about them to
    /// match. Returns how many rows the view scrolled down by (up when
    /// negative), the rows left to draw, and the position the first of
    /// them shows. `None`, having changed nothing, when the last frame
    /// showed another file, no row of this frame, or rows other than the
    /// document's (the note), or while lines are selected: scrolling with
    /// a selection held is rare, and those frames are drawn whole.
    fn reuse(
        &mut self,
        grid: &mut Grid,
        previous: &Grid,
        (x, end, top): (usize, usize, usize),
        layout: &Layout,
    ) -> Option<(isize, Range<usize>, Pos)> {
        let (height, pos) = (self.height, self.document.pos);
        let (first, bottom) = (self.drawn.first().copied()??, self.drawn.last().copied()??);
        if self.drawn.len() != height
            || first.file != pos.file
            || self.note(pos.file).is_some()
            || self.selection.is_some()
            || (previous.width(), previous.height()) != (grid.width(), grid.height())
        {
            return None;
        }
        if let Some(k) = self.drawn.iter().position(|d| *d == Some(pos)) {
            let by = isize::try_from(k).ok()?;
            grid.copy_rows(previous, top + k..top + height, top, x..end);
            self.drawn.copy_within(k.., 0);
            self.drawn[height - k..].fill(None);
            self.separators.retain(|(y, _)| *y >= k);
            for (y, _) in &mut self.separators {
                *y -= k;
            }
            let next = Pos {
                offset: bottom.offset + 1,
                ..bottom
            };
            return Some((by, height - k..height, next));
        }
        if pos > first {
            return None;
        }
        let k = self.rows_between(pos, first, layout, height)?;
        let by = -isize::try_from(k).ok()?;
        grid.copy_rows(previous, top..top + height - k, top + k, x..end);
        self.drawn.copy_within(..height - k, k);
        self.drawn[..k].fill(None);
        self.separators.retain_mut(|(y, _)| {
            *y += k;
            *y < height
        });
        Some((by, 0..k, pos))
    }

    /// Screen rows from `from` down to `to`, a later row of the same file
    /// (offsets counted), when fewer than `limit`.
    fn rows_between(&mut self, from: Pos, to: Pos, layout: &Layout, limit: usize) -> Option<usize> {
        let logical = |p: Pos| Pos { offset: 0, ..p };
        let mut pos = from;
        let mut rows = 0;
        while logical(pos) < logical(to) {
            rows += self.height(pos, layout) - pos.offset;
            if rows >= limit || !self.next_row(&mut pos, layout) {
                return None;
            }
        }
        if logical(pos) != logical(to) {
            return None;
        }
        (rows + to.offset)
            .checked_sub(pos.offset)
            .filter(|&rows| rows < limit)
    }

    /// Shows the selected lines in reverse video, on their side's half of
    /// a split row. `layout` is the drawn file's.
    fn draw_selection(
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
                lines = self.document.row_lines(logical, arrange(layout));
                previous = Some(logical);
                if bytes.is_some() {
                    mapped.clear();
                    self.render(logical, layout, &mut mapped);
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

    /// Paints the search's matches on screen rows `rows`, the current one
    /// in its own color. Only those rows are looked up, however many
    /// matches the file has. `layout` is the drawn file's.
    fn draw_search(
        &mut self,
        grid: &mut Grid,
        (x, end, top): (usize, usize, usize),
        theme: &Theme,
        layout: &Layout,
        rows: Range<usize>,
    ) {
        let Some(marks) = self.document.search.take() else {
            return;
        };
        let mut mapped = std::mem::take(&mut self.mapped);
        let mut previous = None;
        for y in rows {
            let Some(pos) = self.drawn[y] else { continue };
            let logical = Pos { offset: 0, ..pos };
            if pos.file != marks.file {
                continue;
            }
            let lines = self.document.row_lines(logical, arrange(layout));
            let here: Vec<usize> = [Side::Old, Side::New]
                .into_iter()
                .filter_map(|side| Some((side, lines[side.index()]?)))
                .flat_map(|(side, line)| marks.matches.on(side, line))
                .collect();
            if here.is_empty() {
                continue;
            }
            if previous != Some(logical) {
                mapped.clear();
                self.render(logical, layout, &mut mapped);
                previous = Some(logical);
            }
            let split = self.split_row(logical, layout);
            let Some(map) = mapped.map() else { continue };
            for i in here {
                let found = marks.matches.list[i];
                let role = if marks.current == Some(i) {
                    Role::SearchCurrent
                } else {
                    Role::SearchMatch
                };
                let bytes = found.start as usize..found.end as usize;
                for g in &map.glyphs {
                    if g.row == pos.offset
                        && (!split || g.side == found.side.index())
                        && bytes.contains(&g.bytes.start)
                    {
                        let left = (x + g.col).min(end);
                        grid.restyle(left, top + y, (left + g.width).min(end), theme.style(role));
                    }
                }
            }
        }
        self.mapped = mapped;
        self.document.search = Some(marks);
    }

    /// The row showing match `m` of `file`.
    pub(crate) fn match_pos(&mut self, file: usize, m: &document::Match) -> Option<Pos> {
        let layout = self.file_layout(file);
        self.document.match_pos(file, m, arrange(&layout))
    }

    /// Shows the row at `pos` a few rows below the top of the pane.
    pub(crate) fn goto_match(&mut self, pos: Pos) {
        self.document.pos = pos;
        self.clamp();
        let context = isize::try_from(self.height / 4).unwrap_or(0);
        self.scroll(-context);
        self.reveal(pos);
    }

    /// The old and new lines (0-based) a logical row shows; `None` on a
    /// side without one, and on rows that aren't code.
    fn row_lines(&mut self, pos: Pos) -> [Option<usize>; 2] {
        let layout = self.file_layout(pos.file);
        self.document.row_lines(pos, arrange(&layout))
    }

    /// Whether the logical row at `pos` is drawn as two halves in its
    /// file's `layout`.
    fn split_row(&mut self, pos: Pos, layout: &Layout) -> bool {
        match self.document.segment(pos.file, pos.part) {
            Some(Segment::Hunk(_)) => {
                self.document.arrangement(pos, arrange(layout)) == Some(Arrangement::Split)
            }
            Some(Segment::Gap(_)) => {
                let header = HunkHeader {
                    old_start: 1,
                    old_len: 1,
                    new_start: 1,
                    new_len: 1,
                    heading: b"",
                    hidden: 0,
                };
                layout.arrangement(&header) == Arrangement::Split
            }
            Some(Segment::Start) | None => false,
        }
    }

    /// The logical row drawn at row `y` of the pane in the last frame.
    pub(crate) fn row_at(&self, y: usize) -> Option<Pos> {
        self.drawn
            .get(y)
            .copied()
            .flatten()
            .map(|p| Pos { offset: 0, ..p })
    }

    /// Which side of the row at `pos` column `column` (from the pane's
    /// left) is on: the half it's in for a split row, else the side the
    /// row has a line on, the new one first.
    pub(crate) fn side_at(&mut self, pos: Pos, column: usize) -> Option<Side> {
        let layout = self.file_layout(pos.file);
        let lines = self.document.row_lines(pos, arrange(&layout));
        if self.split_row(pos, &layout) {
            let left = self.layout.width.unwrap_or(0).saturating_sub(1) / 2;
            let side = if column < left { Side::Old } else { Side::New };
            return lines[side.index()].map(|_| side);
        }
        match lines {
            [_, Some(_)] => Some(Side::New),
            [Some(_), None] => Some(Side::Old),
            [None, None] => None,
        }
    }

    /// What the cell at row `y` of the pane, `column` from its left, shows
    /// as of the last frame. With `side`, only that side counts (a drag
    /// stays on the side it started on): a cell left of its code is the
    /// row's start, one right of it the row's end.
    pub(crate) fn hit(&mut self, y: usize, column: usize, side: Option<Side>) -> Option<Hit> {
        let drawn = self.drawn.get(y).copied().flatten()?;
        let pos = Pos { offset: 0, ..drawn };
        let layout = self.file_layout(pos.file);
        let lines = self.document.row_lines(pos, arrange(&layout));
        let confined = side.is_some();
        let side = match side {
            Some(side) => side,
            None => self.side_at(pos, column)?,
        };
        let line = lines[side.index()]?;
        let split = self.split_row(pos, &layout);
        let mut c = std::mem::take(&mut self.mapped);
        c.clear();
        self.render(pos, &layout, &mut c);
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

    /// Starts a selection where `hit` is: by characters on code, by lines
    /// on the gutter.
    pub(crate) fn select_hit(&mut self, hit: &Hit) {
        let (pos, side, line, bytes) = match hit {
            Hit::Code {
                pos,
                side,
                line,
                bytes,
            } => (*pos, *side, *line, Some((bytes.clone(), bytes.clone()))),
            Hit::Gutter { pos, side, line } => (*pos, *side, *line, None),
        };
        self.selection = Some(Selection {
            side,
            anchor: pos,
            cursor: pos,
            lines: (line, line),
            bytes,
        });
    }

    /// Moves the selection's end to `hit`, on the selection's side.
    pub(crate) fn select_to_hit(&mut self, hit: &Hit) {
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
    /// code instead.
    pub(crate) fn select_word(&mut self, hit: &Hit, whole: bool) {
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
        self.selection = Some(Selection {
            side: *side,
            anchor: *pos,
            cursor: *pos,
            lines: (*line, *line),
            bytes: Some((start..start, end..end)),
        });
    }

    /// Starts a selection on the row at `pos` and `side`, or with none, on
    /// the first row on screen holding code, on its new side when it has
    /// one. Returns whether there was a line to start on.
    pub(crate) fn select(&mut self, at: Option<(Pos, Side)>) -> bool {
        let start = at.or_else(|| {
            let rows: Vec<Pos> = (0..self.drawn.len())
                .filter_map(|y| self.row_at(y))
                .collect();
            rows.into_iter().find_map(|pos| {
                let side = match self.row_lines(pos) {
                    [_, Some(_)] => Side::New,
                    [Some(_), None] => Side::Old,
                    [None, None] => return None,
                };
                Some((pos, side))
            })
        });
        let Some((pos, side)) = start else {
            return false;
        };
        let Some(line) = self.row_lines(pos)[side.index()] else {
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
            if let Some(line) = self.document.row_lines(pos, arrange(&layout))[side.index()] {
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

    /// Scrolls just enough that the logical row at `target` is on screen.
    fn reveal(&mut self, target: Pos) {
        if target
            < (Pos {
                offset: 0,
                ..self.document.pos
            })
        {
            self.document.pos = target;
            self.clamp();
            return;
        }
        let layout = self.file_layout(self.document.pos.file);
        // Screen rows from the top of the pane to the end of `target`.
        let mut pos = self.document.pos;
        let mut rows = self.height(pos, &layout) - pos.offset;
        while (Pos { offset: 0, ..pos }) < target && rows <= 2 * self.height {
            if !self.next_row(&mut pos, &layout) {
                break;
            }
            rows += self.height(pos, &layout);
        }
        if rows > self.height {
            self.scroll_in(
                isize::try_from(rows - self.height).unwrap_or(isize::MAX),
                &layout,
            );
        }
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
            let shown = document.shown(file, g)?;
            let start = match selection.side {
                Side::Old => span.old,
                Side::New => span.new,
            } + shown.top;
            let (from, to) = (start.max(lo), (start + shown.hidden()).min(hi + 1));
            hidden += to.saturating_sub(from);
            if partial && to > from {
                unknown.push(from..to);
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
                let sign = match (changed.contains(&i), selection.side) {
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

    /// The widest line of code on screen, in columns, for how far the view
    /// scrolls right.
    pub(crate) fn widest(&mut self) -> usize {
        let rows: Vec<Pos> = (0..self.drawn.len())
            .filter_map(|y| self.row_at(y))
            .collect();
        let tab_width = self.layout.tab_width;
        let mut widest = 0;
        for pos in rows {
            if pos.part == 0
                && let Some((_, text)) = self.sheet_text(pos.file, pos.row)
            {
                widest = widest.max(crate::render::width(&text, tab_width));
                continue;
            }
            let lines = self.row_lines(pos);
            let Some(diff) = self.document.diff_of(pos.file).map(Arc::clone) else {
                continue;
            };
            for (line, text) in lines.into_iter().zip([&diff.old, &diff.new]) {
                if let Some(i) = line {
                    let raw = text.line(i);
                    let raw = display_text(raw.strip_suffix(b"\n").unwrap_or(raw));
                    widest = widest.max(crate::render::width(raw, tab_width));
                }
            }
        }
        widest
    }
}

/// Writes one canvas row into the grid, adding `attributes` to every
/// style. Returns the column after the text.
pub(crate) fn paint<'a>(
    grid: &mut Grid,
    mut x: usize,
    y: usize,
    end: usize,
    row: impl Iterator<Item = (Ink, &'a str)>,
    theme: &Theme,
    attributes: Attributes,
) -> usize {
    for (ink, text) in row {
        let mut style = theme.ink_style(ink);
        style.attributes |= attributes;
        x = grid.put_str(x, y, end, text, style);
    }
    x
}

#[cfg(test)]
mod tests;

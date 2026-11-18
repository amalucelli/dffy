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
//!
//! Review threads and the text box writing one are drawn as extra screen
//! rows of the row they hang under, after its code, so the core's rows and
//! positions don't know about them: a row's height counts them, and
//! scrolling, paging and revealing go through them as through a wrapped
//! line's rows.

use std::collections::HashMap;
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

use super::comments::{self, At, Look, NoteRows, Thread, Writes, Writing};
use super::markdown;
use super::piped::Body;
use crate::pager;
use crate::render::{Canvas, Layout};
use crate::term::Grid;
use crate::theme::{Ink, Role, Style, Theme};

pub(crate) mod rendered;

/// Rows kept above a file's first change when it opens there.
const OPEN_CONTEXT: isize = 3;

/// A hunk header that says nothing of its hunk, for how the layout
/// arranges rows that aren't in one.
const ANY_HUNK: HunkHeader<'static> = HunkHeader {
    old_start: 1,
    old_len: 1,
    new_start: 1,
    new_len: 1,
    heading: b"",
    hidden: 0,
};

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
    /// A comment's rendered text: thread `thread` of the file's threads,
    /// its comment `note`, and what [`markdown::SourceMap::at`] gives for
    /// the cell, in the comment's Markdown.
    Note {
        thread: usize,
        note: usize,
        bytes: Range<usize>,
    },
}

/// Text selected in a comment, held as its Markdown: from the source the
/// cell pressed stands for to the one under the pointer, as a selection by
/// characters over code runs. In source terms, it outlives the box being
/// drawn again at another width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NoteSelection {
    root: String,
    note: String,
    anchor: Range<usize>,
    cursor: Range<usize>,
    /// A double click's word, copied as it is rather than widened over
    /// the markup of an element it fills.
    word: bool,
}

impl NoteSelection {
    fn range(&self) -> Range<usize> {
        self.anchor.start.min(self.cursor.start)..self.anchor.end.max(self.cursor.end)
    }
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

/// Something drawn under a row besides its code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Block {
    /// The thread at this index of the file's threads.
    Thread(usize),
    /// The text box writing a new thread.
    New,
}

/// What [`Doc::render`] wrote for a row: whether it's a gap's separator,
/// and how many of its screen rows are code; its blocks' follow.
#[derive(Debug)]
struct Rows {
    separator: bool,
    code: usize,
    /// Its folded threads, to tint when painted.
    cards: Vec<Card>,
}

/// A folded thread's row: which of its row's screen rows it is, the
/// columns inside its sides, and whether it has the focus.
#[derive(Debug)]
struct Card {
    row: usize,
    columns: Range<usize>,
    focused: bool,
}

impl Card {
    /// Gives its cells on screen row `y`, its row drawn from column `x`
    /// of a pane ending at `end`, the background of the selection, or of
    /// the inactive one when it hasn't the focus.
    fn tint(&self, grid: &mut Grid, (x, end, y): (usize, usize, usize), theme: &Theme) {
        // The background alone: mono's selection is reverse video, which
        // would turn the card inside out.
        let bg = theme
            .style(if self.focused {
                Role::Selection
            } else {
                Role::SelectionInactive
            })
            .bg;
        if bg.is_some() {
            let end = (x + self.columns.end).min(end);
            grid.map_styles(x + self.columns.start, y, end, |s| Style { bg, ..s });
        }
    }
}

/// Which threads are folded to one row. Resolved threads start folded, and
/// outdated ones unless a comment not posted is in them: on a reviewed
/// change they outnumber the open ones, and an unsent reply shouldn't
/// hide. In a local review nothing is posted, so resolving is what folds
/// there. `Z` then sets every thread, and `z` one. Choices are
/// kept by root, as folded or not rather than as a flip of the default, so
/// they outlive reloads of the comments file and a thread resolved or
/// reopened under them. The thread the comments panel follows is peeked:
/// whole over `Z` and its default, though not over a `z` choice, until the
/// peek moves on.
#[derive(Debug, Default)]
pub(crate) struct Fold {
    /// What `Z` last set every thread to.
    all: Option<bool>,
    /// What `z` set threads to since, by root.
    chosen: HashMap<String, bool>,
    /// Whether each thread of the file shown starts folded, by root.
    defaults: HashMap<String, bool>,
    peek: Option<String>,
}

impl Fold {
    pub(crate) fn collapsed(&self, root: &str) -> bool {
        self.chosen
            .get(root)
            .copied()
            .or_else(|| (self.peek.as_deref() == Some(root)).then_some(false))
            .or(self.all)
            .unwrap_or_else(|| self.defaults.get(root).copied().unwrap_or(false))
    }

    /// Keeps thread `root` whole whatever the peek does next: one the text
    /// box writes in.
    pub(crate) fn keep_open(&mut self, root: &str) {
        if self.collapsed(root) || self.peek.as_deref() == Some(root) {
            self.chosen.insert(root.to_owned(), false);
        }
    }

    pub(crate) fn toggle(&mut self, root: &str) {
        let folded = !self.collapsed(root);
        self.chosen.insert(root.to_owned(), folded);
    }

    /// Folds every thread, or unfolds every one when those of the file
    /// shown all were. Returns whether they're folded now.
    pub(crate) fn toggle_all(&mut self) -> bool {
        self.peek = None;
        let fold = if self.defaults.is_empty() {
            self.all != Some(true)
        } else {
            self.defaults.keys().any(|root| !self.collapsed(root))
        };
        self.all = Some(fold);
        self.chosen.clear();
        fold
    }

    /// Takes the threads of the file shown, for their defaults.
    fn learn(&mut self, threads: &[Thread]) {
        self.defaults = threads
            .iter()
            .filter(|t| !matches!(t.at, At::Conversation { .. }))
            .map(|t| {
                let quiet =
                    t.resolved || (t.outdated && t.notes.iter().all(comments::Note::posted));
                (t.root.clone(), quiet)
            })
            .collect();
    }
}

/// Which rows of a side a row stands for, to hang threads under it.
enum Covers {
    /// The line it shows.
    Line(usize),
    /// A rendered view's row: the source lines it was drawn from, and the
    /// next row's. A thread hangs under the last row drawn from its line.
    Rendered(Range<usize>, Range<usize>),
    /// A separator: the lines it hides.
    Hidden(Range<usize>),
}

impl Covers {
    fn holds(&self, last: usize) -> bool {
        match self {
            Self::Line(line) => *line == last,
            Self::Rendered(here, next) => here.contains(&last) && !next.contains(&last),
            Self::Hidden(lines) => lines.contains(&last),
        }
    }
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

/// A thread's box at one width. The focus and the ages don't change it:
/// they're in a border's one row.
#[derive(Default)]
struct Boxed {
    width: usize,
    /// Its rows open and folded, once counted.
    rows: [Option<usize>; 2],
    /// Its comments' text as drawn open, once it has been.
    bodies: Option<Vec<markdown::Body>>,
    /// Where each comment is in the box as last drawn open, without the
    /// text box in it.
    notes: Option<Vec<NoteRows>>,
}

impl Boxed {
    /// Thread `i`'s box in `boxes`, emptied when it was another width.
    fn at(boxes: &mut HashMap<usize, Self>, i: usize, width: usize) -> &mut Self {
        let this = boxes.entry(i).or_default();
        if this.width != width {
            *this = Self {
                width,
                ..Self::default()
            };
        }
        this
    }
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
    pub note_selection: Option<NoteSelection>,
    /// The comment threads placed on `threads_file`, the file shown.
    threads: Arc<[Thread]>,
    threads_file: usize,
    /// Each thread on a line, by the side's index and its last line.
    by_line: HashMap<(usize, usize), Vec<usize>>,
    /// Each thread's box by its index, at the width it was last drawn at,
    /// so neither a row's height nor a frame renders its text again.
    /// Emptied when the threads are replaced.
    box_rows: HashMap<usize, Boxed>,
    /// Fenced code in comments is highlighted.
    highlight: bool,
    pub fold: Fold,
    /// The root of the thread `m`, `M` or a click focused.
    focus: Option<String>,
    /// The text box, while it's open; keys go to it.
    pub writing: Option<Writing>,
    /// Seconds since 1970, for the threads' ages.
    pub now: u64,
    /// The `now` at which rows of a thread or the text box on screen were
    /// drawn, since the last frame drawn whole. What they show changes
    /// without the rows above them moving, so the next frame copies them
    /// only at the same `now`, their ages unchanged; a change of focus or
    /// of threads draws the next frame whole.
    blocks_drawn: Option<u64>,
    /// The row a block was last found under, checked before it's used.
    found: Option<(Block, Pos)>,
    /// What the conversation page's first row says: the review's title.
    pub page_title: String,
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
            note_selection: None,
            threads: Arc::new([]),
            threads_file: 0,
            by_line: HashMap::new(),
            box_rows: HashMap::new(),
            highlight: true,
            fold: Fold::default(),
            focus: None,
            writing: None,
            now: 0,
            blocks_drawn: None,
            found: None,
            page_title: String::new(),
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

    /// Puts the conversation page of `comments` comments first, or takes
    /// it away with `None` (see [`Document::set_page`]). The selection and
    /// the threads shown move with their files, and the next frame is drawn
    /// whole. Returns how far the other entries moved.
    pub(crate) fn set_page(&mut self, comments: Option<usize>) -> isize {
        let page = comments.map(|n| {
            let change = dffy_core::git::FileChange {
                path: BString::default(),
                status: dffy_core::git::Status::Modified,
                old: None,
                new: None,
                staging: None,
            };
            (change, n + 1)
        });
        let moved = self.document.set_page(page);
        if moved != 0 {
            if let Some(s) = &mut self.selection {
                s.anchor.file = s.anchor.file.saturating_add_signed(moved);
                s.cursor.file = s.cursor.file.saturating_add_signed(moved);
            }
            self.threads_file = self.threads_file.saturating_add_signed(moved);
            self.found = None;
            self.target = None;
            self.drawn.clear();
            self.separators.clear();
        }
        self.painted = None;
        self.normalize();
        moved
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
            Source::Repo | Source::Page { .. } => return None,
        };
        Some((role, display_text(text).into()))
    }

    /// Row `row` of a sheet: a commit's lines, then how many files it has
    /// and a row per file; or a combined diff's lines; or the conversation
    /// page's title, then a blank row over each comment's box. Long lines
    /// are cut and scroll sideways like code.
    fn sheet_row(&self, file: usize, row: usize, c: &mut Canvas) {
        if let Some((role, line)) = self.sheet_text(file, row) {
            self.layout.text_row(role, &line, c);
            return;
        }
        let files = self.document.files();
        if files[file].is_page() {
            if row == 0 {
                let title = display_text(self.page_title.as_bytes());
                self.layout.text_row(Role::FileHeader, title, c);
            } else {
                c.end_line();
            }
            return;
        }
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
    /// offset), drawn with its file's `layout`: its code, then the threads
    /// and the text box under it.
    fn render(&mut self, pos: Pos, layout: &Layout, c: &mut Canvas) -> Rows {
        let start = c.len();
        let separator = self.render_code(pos, layout, c);
        let code = c.len() - start;
        let mut cards = Vec::new();
        for (i, block) in self.blocks(pos, layout).into_iter().enumerate() {
            let card = self.render_block(block, pos, layout, i > 0, c);
            cards.extend(card.map(|card| Card {
                row: card.row - start,
                ..card
            }));
        }
        Rows {
            separator,
            code,
            cards,
        }
    }

    /// [`Doc::render`]'s code rows. Returns whether it's a gap's separator.
    fn render_code(&mut self, pos: Pos, layout: &Layout, c: &mut Canvas) -> bool {
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
        // without wrapping pays a flag and a lookup here for its code.
        let one = pos.part == 0
            || !(self.layout.wrap || (self.document.rendered() && self.is_rendered(pos.file)));
        let blocks = self.blocks(pos, layout);
        let code = if one { 1 } else { self.code_rows(pos, layout) };
        let gaps = blocks.len().saturating_sub(1);
        let boxes: usize = blocks
            .into_iter()
            .map(|block| self.block_height(block, pos, layout))
            .sum();
        (code + gaps + boxes).max(1)
    }

    /// Screen rows of code the logical row at `pos` takes, the rest being
    /// its blocks'; `usize::MAX`, unmeasured, when it has no blocks.
    fn code_height(&mut self, pos: Pos, layout: &Layout) -> usize {
        if self.blocks(pos, layout).is_empty() {
            return usize::MAX;
        }
        self.code_rows(pos, layout)
    }

    /// [`Doc::render_code`]'s rows for the logical row at `pos`.
    fn code_rows(&mut self, pos: Pos, layout: &Layout) -> usize {
        let mut c = std::mem::take(&mut self.scratch);
        c.clear();
        self.render_code(pos, layout, &mut c);
        let rows = c.len();
        self.scratch = c;
        rows
    }

    /// Takes `threads`, those of `file`, the file shown. Rows they hang
    /// under change height, so the position is kept within its row. Text
    /// selected in a comment ends: its text may have changed under its id.
    pub(crate) fn set_threads(&mut self, file: usize, threads: Arc<[Thread]>) {
        let same = if threads.is_empty() {
            self.threads.is_empty()
        } else {
            Arc::ptr_eq(&threads, &self.threads)
        };
        if same && file == self.threads_file {
            return;
        }
        self.by_line.clear();
        for (i, thread) in threads.iter().enumerate() {
            if let At::Line { side, lines } = &thread.at {
                self.by_line
                    .entry((side.index(), lines.end - 1))
                    .or_default()
                    .push(i);
            }
        }
        self.box_rows.clear();
        self.note_selection = None;
        self.fold.learn(&threads);
        self.threads = threads;
        self.threads_file = file;
        self.found = None;
        self.painted = None;
        self.refit();
    }

    /// The threads of the file shown.
    pub(crate) fn threads(&self) -> &[Thread] {
        if self.threads_file == self.document.pos.file {
            &self.threads
        } else {
            &[]
        }
    }

    /// Keeps the position within its row and the screen full, after the
    /// blocks under rows changed.
    pub(crate) fn refit(&mut self) {
        if self.document.files().is_empty() {
            return;
        }
        let layout = self.file_layout(self.document.pos.file);
        let pos = self.document.pos;
        self.document.pos.offset = pos.offset.min(self.height(pos, &layout) - 1);
        self.clamp_in(&layout);
    }

    /// The blocks hung under the row at `pos`, in the order they're drawn:
    /// under the file's first row the comments on the whole file, then the
    /// text box when it writes a new one; under a line the threads on it,
    /// old side first, then the text box when it writes a new one there;
    /// under a separator the threads on the lines it hides; under the
    /// file's last row the threads whose lines are gone.
    fn blocks(&mut self, pos: Pos, layout: &Layout) -> Vec<Block> {
        let (new, on_file) = match self.writing.as_ref().map(|w| &w.at) {
            Some(Writes::New { side, lines }) => (Some((*side, lines.end - 1)), false),
            Some(Writes::File) => (None, true),
            _ => (None, false),
        };
        if pos.file != self.threads_file || (self.threads.is_empty() && new.is_none() && !on_file) {
            return Vec::new();
        }
        if self.on_page(pos.file) {
            // Comment `i` hangs under row `i + 1`, the title being row 0.
            return (pos.part == 0)
                .then(|| pos.row.checked_sub(1))
                .flatten()
                .filter(|&i| i < self.threads.len())
                .map(Block::Thread)
                .into_iter()
                .collect();
        }
        let threads = Arc::clone(&self.threads);
        let logical = Pos { offset: 0, ..pos };
        let parts = self.document.parts(pos.file);
        let mut out = Vec::new();
        if (logical.part, logical.row) == (usize::from(parts > 1), 0) {
            out.extend(
                (0..threads.len())
                    .filter(|&i| threads[i].at == At::File)
                    .map(Block::Thread),
            );
            if on_file {
                out.push(Block::New);
            }
        }
        let lines = self.document.row_lines(logical, arrange(layout));
        let rendered = self.document.file(pos.file).and_then(File::rendered);
        for side in [Side::Old, Side::New] {
            let covers = match (lines[side.index()], rendered) {
                (Some(line), None) => Covers::Line(line),
                (Some(line), Some(r)) => Covers::Rendered(
                    r.source_lines(side, line, line),
                    r.source_lines(side, line + 1, line + 1),
                ),
                (None, None) => match self.hidden(logical, side) {
                    Some(lines) => Covers::Hidden(lines),
                    None => continue,
                },
                (None, Some(_)) => continue,
            };
            if let Covers::Line(line) = covers {
                if let Some(list) = self.by_line.get(&(side.index(), line)) {
                    out.extend(list.iter().copied().map(Block::Thread));
                }
            } else {
                out.extend((0..threads.len()).filter_map(|i| match &threads[i].at {
                    At::Line { side: s, lines } if *s == side && covers.holds(lines.end - 1) => {
                        Some(Block::Thread(i))
                    }
                    _ => None,
                }));
            }
            if new.is_some_and(|(s, last)| s == side && covers.holds(last)) {
                out.push(Block::New);
            }
        }
        if logical.part + 1 == parts && threads.iter().any(|t| t.at == At::Lost) {
            let rows = self.document.rows(pos.file, logical.part, arrange(layout));
            if logical.row == rows.saturating_sub(1) {
                out.extend(
                    (0..threads.len())
                        .filter(|&i| threads[i].at == At::Lost)
                        .map(Block::Thread),
                );
            }
        }
        out
    }

    fn on_page(&self, file: usize) -> bool {
        self.document.files().get(file).is_some_and(Entry::is_page)
    }

    /// Whether `block` is one of the conversation page's comments, drawn
    /// across the pane and never folded.
    fn in_conversation(&self, block: Block) -> bool {
        matches!(block, Block::Thread(i)
            if matches!(self.threads.get(i).map(|t| &t.at), Some(At::Conversation { .. })))
    }

    /// The lines of `side` the separator at `pos` hides; `None` when it
    /// isn't one.
    fn hidden(&self, pos: Pos, side: Side) -> Option<Range<usize>> {
        let Some(Segment::Gap(g)) = self.document.segment(pos.file, pos.part) else {
            return None;
        };
        let hides = self.document.shown(pos.file, g)?.hides(pos.row)?;
        let span = *self.document.file(pos.file)?.gaps().get(g)?;
        let first = match side {
            Side::Old => span.old,
            Side::New => span.new,
        };
        Some(first + hides.start..first + hides.end)
    }

    /// Appends `block`'s rows, hung under the row at `pos`; in a split row a
    /// thread on a line takes its side's half. `gap` puts a blank row over
    /// it, to part it from the block above. Returns the row of a folded
    /// thread, counted in `c`.
    fn render_block(
        &mut self,
        block: Block,
        pos: Pos,
        layout: &Layout,
        gap: bool,
        c: &mut Canvas,
    ) -> Option<Card> {
        let (start, gutter, boxed, old_half) = self.block_columns(block, pos, layout);
        let mut rows = std::mem::take(&mut self.scratch);
        rows.clear();
        if gap {
            rows.spaces(Role::Plain, boxed);
            rows.end_line();
        }
        let left = start + gutter.saturating_sub(2) + 3;
        let card = self.block_box(block, boxed, &mut rows).map(|focused| Card {
            row: c.len() + usize::from(gap),
            columns: left + 1..left + boxed - 1,
            focused,
        });
        // The conversation page has no code, so no gutter's rule.
        let ruled = !self.in_conversation(block);
        for row in rows.rows() {
            if start > 0 {
                c.spaces(Role::Plain, start - 1);
                c.push(Role::Dim, "│");
            }
            if ruled {
                c.spaces(Role::Plain, gutter.saturating_sub(2));
                c.push(Role::Dim, "│");
                c.spaces(Role::Plain, 2);
            }
            for (ink, piece) in row {
                c.push_ink(ink, piece);
            }
            if old_half {
                c.spaces(Role::Plain, 1);
                c.push(Role::Dim, "│");
            }
            c.end_line();
        }
        self.scratch = rows;
        card
    }

    fn block_height(&mut self, block: Block, pos: Pos, layout: &Layout) -> usize {
        let (_, _, boxed, _) = self.block_columns(block, pos, layout);
        // The text box changes with every key, so a box holding it is
        // counted afresh.
        let key = match block {
            Block::Thread(i) if !self.writes_in(i) => Some((i, usize::from(self.collapsed(i)))),
            _ => None,
        };
        if let Some((i, folded)) = key
            && let Some(rows) = Boxed::at(&mut self.box_rows, i, boxed).rows[folded]
        {
            return rows;
        }
        let mut c = std::mem::take(&mut self.scratch);
        c.clear();
        self.block_box(block, boxed, &mut c);
        let rows = c.len();
        self.scratch = c;
        if let Some((i, folded)) = key {
            Boxed::at(&mut self.box_rows, i, boxed).rows[folded] = Some(rows);
        }
        rows
    }

    /// Highlights fenced code in comments, or stops; their boxes are drawn
    /// again.
    pub(crate) fn set_highlight(&mut self, on: bool) {
        if self.highlight != on {
            self.highlight = on;
            for boxed in self.box_rows.values_mut() {
                boxed.bodies = None;
            }
            self.painted = None;
        }
    }

    /// Whether thread `i` shows folded; the conversation's never do.
    fn collapsed(&self, i: usize) -> bool {
        !self.in_conversation(Block::Thread(i)) && self.fold.collapsed(&self.threads[i].root)
    }

    fn writes_in(&self, i: usize) -> bool {
        self.writing.as_ref().is_some_and(|w| match &w.at {
            Writes::Reply(root) | Writes::Edit { root, .. } => *root == self.threads[i].root,
            Writes::New { .. } | Writes::File => false,
        })
    }

    /// Where [`Doc::render_block`] puts `block`'s rows: the column they
    /// start at, the gutter's width after it, the box's width, and whether
    /// they're a split row's old half, closed by a rule.
    fn block_columns(
        &mut self,
        block: Block,
        pos: Pos,
        layout: &Layout,
    ) -> (usize, usize, usize, bool) {
        let width = layout.width.unwrap_or(80);
        if self.in_conversation(block) {
            return (0, 0, width, false);
        }
        let numbers = if layout.line_numbers {
            layout.digits
        } else {
            0
        };
        let side = match block {
            Block::Thread(i) => match &self.threads[i].at {
                At::Line { side, .. } => Some(*side),
                At::File | At::Lost | At::Conversation { .. } => None,
            },
            Block::New => match &self.writing {
                Some(Writing {
                    at: Writes::New { side, .. },
                    ..
                }) => Some(*side),
                _ => None,
            },
        };
        // A block across the pane follows the layout, not its row: the
        // file's first row is a header, never split.
        let split = !layout.one_sided
            && match side {
                Some(_) => self.split_row(pos, layout),
                None => layout.arrangement(&ANY_HUNK) == Arrangement::Split,
            };
        let half = layout.lead_width() + numbers + layout.mark_width();
        let left = width.saturating_sub(1) / 2;
        let (start, gutter, end) = match (split, side) {
            (true, Some(Side::Old)) => (0, half, left),
            (true, Some(Side::New)) => (left + 1, half, width),
            (true, None) => (0, half, width),
            (false, _) => {
                let columns = if layout.one_sided { 1 } else { 2 };
                let numbers = if numbers == 0 {
                    0
                } else {
                    columns * (numbers + 1) - 1
                };
                (
                    0,
                    layout.lead_width() + numbers + layout.mark_width(),
                    width,
                )
            }
        };
        let boxed = end.saturating_sub(start + gutter + 2);
        (start, gutter, boxed, split && side == Some(Side::Old))
    }

    /// Writes `block`'s box, `boxed` columns wide, into `c`. Returns
    /// whether it has the focus when it's a folded thread's one row.
    fn block_box(&mut self, block: Block, boxed: usize, c: &mut Canvas) -> Option<bool> {
        match block {
            Block::Thread(i) => {
                let threads = Arc::clone(&self.threads);
                let thread = &threads[i];
                let writes = self.writes_in(i);
                let collapsed = self.collapsed(i);
                let focused = writes || self.focus.as_deref() == Some(thread.root.as_str());
                let mut notes = Boxed::at(&mut self.box_rows, i, boxed)
                    .notes
                    .take()
                    .unwrap_or_default();
                let bodies: &[markdown::Body] = if collapsed && !writes {
                    &[]
                } else {
                    let highlight = self.highlight;
                    Boxed::at(&mut self.box_rows, i, boxed)
                        .bodies
                        .get_or_insert_with(|| comments::bodies(thread, boxed, highlight))
                };
                let writing = self.writing.as_mut().filter(|_| writes);
                let card = comments::thread_rows(
                    thread,
                    bodies,
                    (collapsed, focused),
                    writing,
                    (boxed, self.now),
                    (c, &mut notes),
                );
                if !collapsed && !writes {
                    Boxed::at(&mut self.box_rows, i, boxed).notes = Some(notes);
                }
                card.then_some(focused)
            }
            Block::New => {
                if let Some(writing) = &mut self.writing {
                    comments::new_rows(&mut writing.text, boxed, c);
                }
                None
            }
        }
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

    /// The separator drawn on screen row `y` of the document in the last
    /// frame.
    pub(crate) fn separator_at(&self, y: usize) -> Option<Pos> {
        self.separators
            .iter()
            .find(|(row, _)| *row == y)
            .map(|&(_, p)| p)
    }

    /// Makes `separator` the one `x` reveals, until the next frame aims
    /// at the first on screen again.
    pub(crate) fn aim(&mut self, separator: Pos) {
        self.target = Some(separator);
    }

    /// A click on screen row `y` of the document: reveals more of the gap
    /// when it's a separator.
    pub(crate) fn click(&mut self, y: usize) {
        if let Some(p) = self.separator_at(y)
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
        self.line_at(self.document.pos)
    }

    /// The new-side line number of the row at `pos`, or the first after
    /// it, for the editor.
    pub(crate) fn line_at(&mut self, pos: Pos) -> usize {
        let line = self.line_of(pos);
        let file = self.document.file(pos.file);
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
            self.blocks_drawn = None;
            (None, 0..self.height, self.document.pos)
        };
        let kept = self.separators.len();
        let mut y = rows.start;
        let mut last = pos;
        while y < rows.end {
            c.clear();
            let drawn = self.render(pos, &layout, &mut c);
            if drawn.separator && pos.offset == 0 {
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
                for card in drawn.cards.iter().filter(|card| card.row == i) {
                    card.tint(grid, (x, end, top + y), theme);
                }
                if i >= drawn.code {
                    self.blocks_drawn = Some(self.now);
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
        self.draw_note_selection(grid, area);
        self.draw_search(grid, area, theme, &layout, rows);
        self.draw_markers(grid, area, theme, &layout);
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
            self.render_code(old, layout, c);
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
            self.render_code(pos, layout, c);
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
    /// document's (the note), while lines or text are selected, or when it drew a
    /// thread or the text box at another `now`: scrolling with a selection
    /// held is rare, and a thread's ages change with no scroll.
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
            || self.note_selection.is_some()
            || self.blocks_drawn.is_some_and(|at| at != self.now)
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
        let mut code = 0;
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
                code = self.code_height(logical, layout);
                if bytes.is_some() {
                    mapped.clear();
                    self.render_code(logical, layout, &mut mapped);
                }
            }
            let Some(line) = lines[side].filter(|l| (lo..=hi).contains(l) && pos.offset < code)
            else {
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
                self.render_code(logical, layout, &mut mapped);
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

    /// Marks the rows of lines with comment threads in the gutter, in the
    /// cell after the line numbers, its background kept. A unified row with
    /// threads on both sides takes the most pressing look. `layout` is the
    /// drawn file's.
    fn draw_markers(
        &mut self,
        grid: &mut Grid,
        (x, end, top): (usize, usize, usize),
        theme: &Theme,
        layout: &Layout,
    ) {
        let file = self.document.pos.file;
        if self.threads().is_empty() {
            return;
        }
        let threads = Arc::clone(&self.threads);
        let mut mapped = std::mem::take(&mut self.mapped);
        let mut previous = None;
        for y in 0..self.drawn.len() {
            let Some(pos) = self.drawn[y] else { continue };
            let logical = Pos { offset: 0, ..pos };
            if pos.file != file || previous == Some(logical) {
                continue;
            }
            previous = Some(logical);
            let lines = self.document.row_lines(logical, arrange(layout));
            let looks = [Side::Old, Side::New].map(|side| {
                let line = lines[side.index()]?;
                let source = self.source_lines(file, side, line, line);
                threads
                    .iter()
                    .filter(|t| matches!(&t.at, At::Line { side: s, lines } if *s == side && overlap(lines, &source)))
                    .map(|t| t.look)
                    .max()
            });
            if looks == [None, None] {
                continue;
            }
            mapped.clear();
            self.render_code(logical, layout, &mut mapped);
            let split = self.split_row(logical, layout);
            let Some(map) = mapped.map() else { continue };
            let mut put = |side: Option<usize>, look: Look| {
                let Some(area) = map
                    .areas
                    .iter()
                    .find(|a| a.row == pos.offset && side.is_none_or(|s| s == a.side))
                else {
                    return;
                };
                let column = x + area.col.saturating_sub(layout.mark_width());
                let mut style = theme.style(look.role());
                style.bg = grid
                    .row(top + y)
                    .get(column)
                    .and_then(|cell| cell.style().bg)
                    .or(style.bg);
                grid.put_str(column, top + y, end, look.glyph(), style);
            };
            if split {
                for side in [Side::Old, Side::New] {
                    if let Some(look) = looks[side.index()] {
                        put(Some(side.index()), look);
                    }
                }
            } else if let Some(look) = looks.into_iter().flatten().max() {
                put(None, look);
            }
        }
        self.mapped = mapped;
    }

    /// The lines of `file`'s line diff that lines `lo..=hi` of `side` show:
    /// themselves, or in a rendered view the blocks they were drawn from.
    fn source_lines(&self, file: usize, side: Side, lo: usize, hi: usize) -> Range<usize> {
        match self.document.file(file).and_then(File::rendered) {
            Some(rendered) => rendered.source_lines(side, lo, hi),
            None => lo..hi + 1,
        }
    }

    /// The lines a new comment goes on, in the line diff of their file:
    /// the selection's, else the line `v` would start on from `at`.
    pub(crate) fn comment_target(
        &mut self,
        at: Option<(Pos, Side)>,
    ) -> Option<(usize, Side, Range<usize>)> {
        let (file, side, lo, hi) = if let Some(s) = &self.selection {
            let (lo, hi) = s.range();
            (s.anchor.file, s.side, lo, hi)
        } else {
            let (pos, side, line) = self.start(at)?;
            (pos.file, side, line, line)
        };
        let lines = self.source_lines(file, side, lo, hi);
        (!lines.is_empty()).then_some((file, side, lines))
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
            Some(Segment::Gap(_)) => layout.arrangement(&ANY_HUNK) == Arrangement::Split,
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
        if drawn.offset >= self.code_height(pos, &layout) {
            return None;
        }
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
            Hit::Note {
                thread,
                note,
                bytes,
            } => {
                self.select_note(*thread, *note, bytes.clone()..bytes.clone(), false);
                return;
            }
        };
        self.note_selection = None;
        self.selection = Some(Selection {
            side,
            anchor: pos,
            cursor: pos,
            lines: (line, line),
            bytes,
        });
    }

    /// Selects in comment `note` of thread `thread` from `range.start` to
    /// `range.end`, each what [`markdown::SourceMap::at`] gave.
    fn select_note(&mut self, thread: usize, note: usize, range: Range<Range<usize>>, word: bool) {
        let Some(t) = self.threads.get(thread) else {
            return;
        };
        let Some(n) = t.notes.get(note) else {
            return;
        };
        self.selection = None;
        self.note_selection = Some(NoteSelection {
            root: t.root.clone(),
            note: n.id.clone(),
            anchor: range.start,
            cursor: range.end,
            word,
        });
    }

    /// Moves the selection's end to `hit`, on the selection's side, or in
    /// the comment it's in.
    pub(crate) fn select_to_hit(&mut self, hit: &Hit) {
        if let Hit::Note {
            thread,
            note,
            bytes,
        } = hit
        {
            if let Some(selection) = &mut self.note_selection
                && let Some(t) = self.threads.get(*thread)
                && t.root == selection.root
                && t.notes.get(*note).is_some_and(|n| n.id == selection.note)
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
            Hit::Note { .. } => return,
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
        if let Hit::Note {
            thread,
            note,
            bytes,
        } = hit
        {
            let Some(text) = self.threads.get(*thread).and_then(|t| t.notes.get(*note)) else {
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
            self.select_note(*thread, *note, range, !whole);
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
        self.note_selection = None;
        self.selection = Some(Selection {
            side: *side,
            anchor: *pos,
            cursor: *pos,
            lines: (*line, *line),
            bytes: Some((start..start, end..end)),
        });
    }

    /// Thread `i`'s open box under the row at `pos`: the pane's column its
    /// text starts at, its width, and where each comment is in it. `None`
    /// folded, or with the text box in it.
    fn box_notes(
        &mut self,
        i: usize,
        pos: Pos,
        layout: &Layout,
    ) -> Option<(usize, usize, Vec<NoteRows>)> {
        if self.collapsed(i) || self.writes_in(i) {
            return None;
        }
        let block = Block::Thread(i);
        let (start, gutter, boxed, _) = self.block_columns(block, pos, layout);
        // The columns `render_block` draws before the box: hits and the
        // highlight miss by as many if the two drift.
        let ruled = if self.in_conversation(block) {
            0
        } else {
            gutter.saturating_sub(2) + 3
        };
        if Boxed::at(&mut self.box_rows, i, boxed).notes.is_none() {
            let mut c = std::mem::take(&mut self.scratch);
            c.clear();
            self.block_box(block, boxed, &mut c);
            self.scratch = c;
        }
        let notes = Boxed::at(&mut self.box_rows, i, boxed).notes.clone()?;
        Some((start + ruled + comments::TEXT_COLUMN, boxed, notes))
    }

    /// Comment `k` of thread `i` as its box draws it, `boxed` wide.
    fn note_body(&mut self, i: usize, k: usize, boxed: usize) -> Option<&markdown::Body> {
        let threads = Arc::clone(&self.threads);
        let highlight = self.highlight;
        Boxed::at(&mut self.box_rows, i, boxed)
            .bodies
            .get_or_insert_with(|| comments::bodies(&threads[i], boxed, highlight))
            .get(k)
    }

    /// What the cell at row `y` of the pane, `column` from its left, shows
    /// of a comment's text as of the last frame: on a row of the text, from
    /// its first column to the box's padding. `within` (a thread and its
    /// comment, as a drag stays in) takes any cell instead: one on the
    /// comment's rows as there, one before them as its text's start, and
    /// one after as its end.
    pub(crate) fn note_hit(
        &mut self,
        y: usize,
        column: usize,
        within: Option<(usize, usize)>,
    ) -> Option<Hit> {
        let drawn = self.drawn.get(y).copied().flatten();
        let (thread, pos) = if let Some((i, _)) = within {
            (i, self.find_block(Block::Thread(i))?)
        } else {
            let (Block::Thread(i), _) = self.block_row_at(y)? else {
                return None;
            };
            (
                i,
                Pos {
                    offset: 0,
                    ..drawn?
                },
            )
        };
        let layout = self.file_layout(pos.file);
        let block = Block::Thread(thread);
        let (_, rows) = self
            .block_ranges(pos, &layout)
            .into_iter()
            .find(|(b, _)| *b == block)?;
        let (left, boxed, notes) = self.box_notes(thread, pos, &layout)?;
        let text_at = |k: usize| {
            let text = &notes[k].text;
            rows.start + text.start..rows.start + text.end
        };
        let (note, row, col) = if let Some((_, k)) = within {
            notes.get(k)?;
            let text = text_at(k);
            let at = |offset| Pos { offset, ..pos };
            match drawn {
                Some(d) if d < at(text.start) => (k, None, 0),
                Some(d) if d < at(text.end) => {
                    (k, Some(d.offset - text.start), column.saturating_sub(left))
                }
                _ => (k, Some(usize::MAX), 0),
            }
        } else {
            let offset = drawn?.offset;
            let k = (0..notes.len()).find(|&k| text_at(k).contains(&offset))?;
            let col = column
                .checked_sub(left)
                .filter(|&c| c < comments::inner(boxed))?;
            (k, Some(offset - text_at(k).start), col)
        };
        let map = &self.note_body(thread, note, boxed)?.map;
        let bytes = match row {
            Some(row) => map.at(row, col),
            None => map.text().start..map.text().start,
        };
        Some(Hit::Note {
            thread,
            note,
            bytes,
        })
    }

    /// The thread and comment drawn on row `y` of the pane in the last
    /// frame, by root and id: the comment whose heading is there or above;
    /// a folded thread's first.
    pub(crate) fn note_at(&mut self, y: usize) -> Option<(String, String)> {
        let drawn = self.drawn.get(y).copied().flatten()?;
        let pos = Pos { offset: 0, ..drawn };
        let (Block::Thread(i), _) = self.block_row_at(y)? else {
            return None;
        };
        let layout = self.file_layout(pos.file);
        let (_, rows) = self
            .block_ranges(pos, &layout)
            .into_iter()
            .find(|(b, _)| *b == Block::Thread(i))?;
        let k = self.box_notes(i, pos, &layout).map_or(0, |(_, _, notes)| {
            let row = drawn.offset - rows.start;
            notes.iter().rposition(|n| n.first <= row).unwrap_or(0)
        });
        let thread = &self.threads[i];
        Some((thread.root.clone(), thread.notes.get(k)?.id.clone()))
    }

    /// The thread and comment the comment selection is in, while they're
    /// on the file shown.
    fn selected_note(&self) -> Option<(usize, usize)> {
        let selection = self.note_selection.as_ref()?;
        let i = self
            .threads()
            .iter()
            .position(|t| t.root == selection.root)?;
        let k = self.threads[i]
            .notes
            .iter()
            .position(|n| n.id == selection.note)?;
        Some((i, k))
    }

    /// Shows the text selected in a comment reversed, glyph by glyph, on
    /// the rows of it on screen.
    fn draw_note_selection(&mut self, grid: &mut Grid, (x, end, top): (usize, usize, usize)) {
        let Some(range) = self.note_selection.as_ref().map(NoteSelection::range) else {
            return;
        };
        let Some((i, k)) = self.selected_note() else {
            return;
        };
        let block = Block::Thread(i);
        let Some(pos) = self.find_block(block) else {
            return;
        };
        let layout = self.file_layout(pos.file);
        let Some((_, rows)) = self
            .block_ranges(pos, &layout)
            .into_iter()
            .find(|(b, _)| *b == block)
        else {
            return;
        };
        let Some((left, boxed, notes)) = self.box_notes(i, pos, &layout) else {
            return;
        };
        let text = rows.start + notes[k].text.start..rows.start + notes[k].text.end;
        let shown: Vec<(usize, usize)> = (0..self.drawn.len())
            .filter_map(|y| {
                let d = self.drawn[y]?;
                (Pos { offset: 0, ..d } == pos && text.contains(&d.offset))
                    .then(|| (y, d.offset - text.start))
            })
            .collect();
        let inner = comments::inner(boxed);
        let Some(body) = self.note_body(i, k, boxed) else {
            return;
        };
        for (y, row) in shown {
            // Past the box's padding, a row is cut.
            for cells in body
                .map
                .cells(row, range.clone())
                .filter(|c| c.end <= inner)
            {
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
    pub(crate) fn note_selected(&mut self) -> Option<(String, bool)> {
        let selection = self.note_selection.clone()?;
        let (i, k) = self.selected_note()?;
        let range = if selection.word {
            selection.range()
        } else {
            let pos = self.find_block(Block::Thread(i))?;
            let layout = self.file_layout(pos.file);
            let (_, boxed, _) = self.box_notes(i, pos, &layout)?;
            let map = &self.note_body(i, k, boxed)?.map;
            map.span(&selection.anchor, &selection.cursor)
        };
        let text = &self.threads[i].notes[k].text;
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
    fn start(&mut self, at: Option<(Pos, Side)>) -> Option<(Pos, Side, usize)> {
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

    /// The focused thread, while it's on the file shown.
    pub(crate) fn focused(&self) -> Option<&Thread> {
        let root = self.focus.as_deref()?;
        self.threads().iter().find(|t| t.root == root)
    }

    /// Focuses the thread with root `root`, or none. Focusing another
    /// thread than the one peeked at ends the peek.
    pub(crate) fn set_focus(&mut self, root: Option<String>) {
        if let Some(root) = &root
            && self.fold.peek.as_ref().is_some_and(|p| p != root)
        {
            self.peek(None);
        }
        if self.focus != root {
            // A box's border shows the focus: the next frame is drawn whole.
            self.painted = None;
        }
        self.focus = root;
    }

    /// Peeks at the thread with root `root` (see [`Fold`]), or ends the
    /// peek.
    pub(crate) fn peek(&mut self, root: Option<&str>) {
        if self.fold.peek.as_deref() == root {
            return;
        }
        self.fold.peek = root.map(str::to_owned);
        self.painted = None;
        self.refit();
    }

    /// The block drawn on row `y` of the pane in the last frame.
    pub(crate) fn block_at(&mut self, y: usize) -> Option<Block> {
        self.block_row_at(y).map(|(b, _)| b)
    }

    /// The block drawn on row `y` in the last frame, and whether `y` is
    /// its first row: a thread's header, or its folded row.
    pub(crate) fn block_row_at(&mut self, y: usize) -> Option<(Block, bool)> {
        let drawn = self.drawn.get(y).copied().flatten()?;
        let pos = Pos { offset: 0, ..drawn };
        let layout = self.file_layout(pos.file);
        self.block_ranges(pos, &layout)
            .into_iter()
            .find(|(_, r)| r.contains(&drawn.offset))
            .map(|(b, r)| (b, r.start == drawn.offset))
    }

    /// The screen rows (from its first) of each block under the logical
    /// row at `pos`, after its code's. The blank row between two blocks is
    /// neither's.
    fn block_ranges(&mut self, pos: Pos, layout: &Layout) -> Vec<(Block, Range<usize>)> {
        let mut first = self.code_rows(pos, layout);
        self.blocks(pos, layout)
            .into_iter()
            .map(|block| {
                let rows = first..first + self.block_height(block, pos, layout);
                first = rows.end + 1;
                (block, rows)
            })
            .collect()
    }

    /// The thread to fold with `z`: the focused one, else the first drawn
    /// in the last frame.
    pub(crate) fn nearest_thread(&mut self) -> Option<usize> {
        if let Some(f) = self.focused() {
            return self.threads.iter().position(|t| t.root == f.root);
        }
        (0..self.drawn.len()).find_map(|y| match self.block_at(y) {
            Some(Block::Thread(i)) => Some(i),
            _ => None,
        })
    }

    fn first_row(&self, file: usize) -> Pos {
        Pos {
            file,
            part: usize::from(self.document.parts(file) > 1),
            row: 0,
            offset: 0,
        }
    }

    /// Each thread of the file shown with the row it hangs under, in the
    /// order the diff shows them. Walks the file's rows, as `m` and `M`
    /// are pressed: placing them at load would cost every file the walk.
    fn placed_threads(&mut self) -> Vec<(Pos, usize)> {
        let file = self.document.pos.file;
        let mut out = Vec::new();
        if self.threads().is_empty() {
            return out;
        }
        let layout = self.file_layout(file);
        let mut pos = self.first_row(file);
        loop {
            for block in self.blocks(pos, &layout) {
                if let Block::Thread(i) = block {
                    out.push((pos, i));
                }
            }
            if out.len() == self.threads.len() || !self.next_row(&mut pos, &layout) {
                return out;
            }
        }
    }

    /// Focuses and shows the thread after the focused one, or before it
    /// without `forward`; with none focused, the first at or below the top
    /// row, or the last above it. False when there's none that way in the
    /// file shown.
    pub(crate) fn step_thread(&mut self, forward: bool) -> bool {
        let placed = self.placed_threads();
        let top = Pos {
            offset: 0,
            ..self.document.pos
        };
        let current = self.focused().and_then(|f| {
            placed
                .iter()
                .position(|&(_, i)| self.threads[i].root == f.root)
        });
        let next = match (current, forward) {
            (Some(k), true) => Some(k + 1),
            (Some(k), false) => k.checked_sub(1),
            (None, true) => placed.iter().position(|&(p, _)| p >= top),
            (None, false) => placed.iter().rposition(|&(p, _)| p < top),
        };
        let Some(&(pos, i)) = next.and_then(|k| placed.get(k)) else {
            return false;
        };
        self.show_thread(pos, i);
        true
    }

    /// Focuses and shows the first thread of the file shown, or its last.
    pub(crate) fn edge_thread(&mut self, last: bool) -> bool {
        let placed = self.placed_threads();
        let edge = if last { placed.last() } else { placed.first() };
        let Some(&(pos, i)) = edge else {
            return false;
        };
        self.show_thread(pos, i);
        true
    }

    /// Focuses and shows the thread with root `root` on the file shown.
    pub(crate) fn go_to_thread(&mut self, root: &str) -> bool {
        let placed = self.placed_threads();
        let Some(&(pos, i)) = placed.iter().find(|&&(_, i)| self.threads[i].root == root) else {
            return false;
        };
        self.show_thread(pos, i);
        true
    }

    /// Focuses the thread with root `root` on the file shown, scrolling
    /// only as far as it takes to show it: after a save, the place stays.
    pub(crate) fn focus_thread(&mut self, root: &str) -> bool {
        let Some(i) = self.threads.iter().position(|t| t.root == root) else {
            return false;
        };
        self.set_focus(Some(root.to_owned()));
        self.reveal_block(Block::Thread(i), false);
        true
    }

    /// Focuses thread `i`, hung under the row at `pos`, ending a peek at
    /// another. Unless its box shows whole already, the box lands a third
    /// of the way down the pane; one too tall for that goes as far higher
    /// as it takes to fit, though the code it hangs under stays above it.
    fn show_thread(&mut self, pos: Pos, i: usize) {
        self.set_focus(Some(self.threads[i].root.clone()));
        self.found = Some((Block::Thread(i), pos));
        let Some((pos, range)) = self.block_rows(Block::Thread(i)) else {
            return;
        };
        let first = Pos {
            offset: range.start,
            ..pos
        };
        if self
            .rows_to(first)
            .is_some_and(|y| y + range.len() <= self.height)
        {
            return;
        }
        let spare = self.height.saturating_sub(range.len());
        let above = (self.height / 3).min(spare.max(range.start));
        let layout = self.file_layout(pos.file);
        self.document.pos = first;
        self.scroll_in(-isize::try_from(above).unwrap_or(0), &layout);
    }

    /// Screen rows from the top of the pane down to `target`, a screen row
    /// of a logical row; `None` when it's above the top or below the pane.
    fn rows_to(&mut self, target: Pos) -> Option<usize> {
        let top = self.document.pos;
        if target < top {
            return None;
        }
        let layout = self.file_layout(top.file);
        let mut at = Pos { offset: 0, ..top };
        // From the top of `at`'s logical row.
        let mut rows = 0;
        while at
            != (Pos {
                offset: 0,
                ..target
            })
        {
            rows += self.height(at, &layout);
            if rows >= self.height + top.offset || !self.next_row(&mut at, &layout) {
                return None;
            }
        }
        let y = rows + target.offset - top.offset;
        (y < self.height).then_some(y)
    }

    pub(crate) fn writing_block(&self) -> Option<Block> {
        match &self.writing.as_ref()?.at {
            Writes::New { .. } | Writes::File => Some(Block::New),
            Writes::Reply(root) | Writes::Edit { root, .. } => self
                .threads()
                .iter()
                .position(|t| t.root == *root)
                .map(Block::Thread),
        }
    }

    /// The row `block` hangs under in the file shown.
    fn find_block(&mut self, block: Block) -> Option<Pos> {
        let file = self.document.pos.file;
        let layout = self.file_layout(file);
        if let Some((b, pos)) = self.found
            && b == block
            && pos.file == file
            && self.blocks(pos, &layout).contains(&block)
        {
            return Some(pos);
        }
        let mut pos = self.first_row(file);
        loop {
            if self.blocks(pos, &layout).contains(&block) {
                self.found = Some((block, pos));
                return Some(pos);
            }
            if !self.next_row(&mut pos, &layout) {
                return None;
            }
        }
    }

    /// The row `block` hangs under, and its screen rows within that row.
    fn block_rows(&mut self, block: Block) -> Option<(Pos, Range<usize>)> {
        let pos = self.find_block(block)?;
        let layout = self.file_layout(pos.file);
        let (_, range) = self
            .block_ranges(pos, &layout)
            .into_iter()
            .find(|(b, _)| *b == block)?;
        Some((pos, range))
    }

    /// Whether any row of the focused thread is on screen, counted from the
    /// position rather than the last frame: keys can come between frames.
    pub(crate) fn focused_shown(&mut self) -> bool {
        let Some(i) = self
            .focused()
            .and_then(|f| self.threads.iter().position(|t| t.root == f.root))
        else {
            return false;
        };
        let Some((pos, range)) = self.block_rows(Block::Thread(i)) else {
            return false;
        };
        let top = self.document.pos;
        if (Pos {
            offset: range.end - 1,
            ..pos
        }) < top
        {
            return false;
        }
        let layout = self.file_layout(pos.file);
        let mut at = top;
        let mut rows = 0;
        while (Pos { offset: 0, ..at }) != pos {
            rows += self.height(at, &layout) - at.offset;
            if rows >= self.height || !self.next_row(&mut at, &layout) {
                return false;
            }
        }
        rows + range.start.saturating_sub(at.offset) < self.height
    }

    /// Scrolls just enough that `block` shows: from its first row, or with
    /// `end` (the text box, its cursor last), through its last.
    pub(crate) fn reveal_block(&mut self, block: Block, end: bool) {
        let Some((pos, range)) = self.block_rows(block) else {
            return;
        };
        let layout = self.file_layout(pos.file);
        let first = Pos {
            offset: range.start,
            ..pos
        };
        if first < self.document.pos {
            self.document.pos = if end {
                Pos {
                    offset: range.end.saturating_sub(self.height).max(range.start),
                    ..pos
                }
            } else {
                first
            };
            self.clamp_in(&layout);
            return;
        }
        // Screen rows from the top of the pane through the block's last.
        let mut at = self.document.pos;
        let mut rows = 0;
        loop {
            if (Pos { offset: 0, ..at }) == pos {
                rows += range.end - at.offset;
                break;
            }
            rows += self.height(at, &layout) - at.offset;
            if !self.next_row(&mut at, &layout) {
                return;
            }
        }
        if rows > self.height {
            let mut by = rows - self.height;
            if !end {
                by = by.min(rows - range.len());
            }
            self.scroll_in(isize::try_from(by).unwrap_or(isize::MAX), &layout);
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

fn overlap(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
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

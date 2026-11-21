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
//! Review threads and the text box writing one are blocks among the core's
//! rows ([`document::Block`]): the core places them, counts their rows and
//! searches their text, and this module measures each box by width and
//! draws it a row at a time.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;

use dffy_core::annotation::Placement;
use dffy_core::bstr::BString;
use dffy_core::diff::Hunk;
use dffy_core::document::{
    self, Arrange, Attach, BlockId, BlockStep, Content, Counts, Document, File, Mark, Missing,
    NO_NEWLINE_LINE, Pos, Render, Segment, Settle, Side, Source,
};
use dffy_core::git::{FileDiff, Picked, Update};
use dffy_core::patch;
use dffy_core::view::{Arrangement, Attributes, HunkHeader, Row, display_text};

use super::comments::{self, CommentRows, Writes, Writing};
use super::markdown;
use super::piped::Body;
use crate::pager;
use crate::render::{Canvas, Layout, Mapped};
use crate::review::model::{At, Look, Thread, ThreadComment};
use crate::term::Grid;
use crate::theme::{Ink, Role, Style, Theme};

pub(crate) mod rendered;
mod select;

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

/// The block of the text box writing a new thread; threads take the rest.
const NEW: BlockId = BlockId(u32::MAX);

/// How a file's rows are laid out, for the core's row queries: `layout`'s
/// arrangement of a hunk, and the boxes' rows at its width. Every query on
/// a file with blocks passes one, so their heights are kept across calls.
#[derive(Clone, Copy)]
struct Arranger<'a> {
    layout: &'a Layout,
    boxes: &'a Boxes,
}

#[inline]
fn arrange<'a>(layout: &'a Layout, boxes: &'a Boxes) -> Arranger<'a> {
    Arranger { layout, boxes }
}

impl Arrange for Arranger<'_> {
    #[inline]
    fn arrangement(&self, header: &HunkHeader<'_>) -> Arrangement {
        self.layout.arrangement(header)
    }

    #[inline]
    fn width(&self) -> usize {
        self.layout.width.unwrap_or(80)
    }

    fn measure(&self, id: BlockId, _: usize) -> usize {
        (self.boxes.block(id)).map_or(1, |b| self.boxes.height(b, self.layout))
    }

    fn locate(&self, id: BlockId, _: usize, line: usize) -> usize {
        self.boxes.locate(id, self.layout, line)
    }

    fn parting(&self) -> usize {
        1
    }
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
    /// its comment `comment`, and what [`markdown::SourceMap::at`] gives for
    /// the cell, in the comment's Markdown.
    Comment {
        thread: usize,
        comment: usize,
        bytes: Range<usize>,
    },
}

/// Text selected in a comment, held as its Markdown: from the source the
/// cell pressed stands for to the one under the pointer, as a selection by
/// characters over code runs. In source terms, it outlives the box being
/// drawn again at another width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentSelection {
    root: String,
    comment: String,
    anchor: Range<usize>,
    cursor: Range<usize>,
    /// A double click's word, copied as it is rather than widened over
    /// the markup of an element it fills.
    word: bool,
}

impl CommentSelection {
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

/// Where [`Doc::step_thread`] went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stepped {
    /// To the thread at this index of the file's threads, now shown.
    Thread(usize),
    /// Past the file's threads: the next file that way holding some.
    File(usize),
    /// Nowhere: there are none that way.
    Nowhere,
}

/// What [`Doc::render`] wrote for a row: whether it's a gap's separator,
/// and the folded thread it is, to tint when painted.
#[derive(Debug, Default)]
struct Rows {
    separator: bool,
    card: Option<Card>,
}

/// What [`Doc::render_code`] found at a row: code, a gap's separator, a
/// row of a block, or the blank row parting two.
enum Drawn {
    Code,
    Separator,
    Block(BlockId, usize),
    Parting(BlockId),
}

/// A folded thread's one row: the columns inside its sides, and whether
/// it has the focus.
#[derive(Debug, Clone)]
struct Card {
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
                    t.resolved || (t.outdated && t.comments.iter().all(ThreadComment::posted));
                (t.root.clone(), quiet)
            })
            .collect();
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
    comment_rows: Option<Vec<CommentRows>>,
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

/// The comment threads of the file shown as boxes, each a block of the
/// core's rows: what a box looks like (folded, written in, highlighted)
/// and the rows it takes, for the [`Arranger`] to measure and the frame to
/// draw alike.
pub(crate) struct Boxes {
    /// The threads placed on `file`, the file shown.
    threads: Arc<[Thread]>,
    file: usize,
    /// The block each thread root seen was given, for the viewer's life:
    /// a position on a thread's box stays on it when threads come and go
    /// around it.
    ids: HashMap<String, BlockId>,
    /// The thread each block of `file` is.
    of: HashMap<BlockId, usize>,
    /// The line of its block's text each comment of a thread starts on.
    starts: Vec<Vec<usize>>,
    /// Each thread's box by its index, at the width it was last drawn at,
    /// so neither measuring nor a frame renders its text again. Emptied
    /// when the threads are replaced.
    cache: RefCell<HashMap<usize, Boxed>>,
    /// Fenced code in comments is highlighted.
    highlight: bool,
    /// What a private comment's heading says after its word: the key that
    /// makes it a draft, as `A shares`.
    share: Option<String>,
    fold: Fold,
    /// The text box, while it's open; keys go to it.
    writing: Option<Writing>,
    /// Where the text box for a new thread hung when the blocks were set.
    new_at: Option<Attach>,
}

impl Boxes {
    fn new() -> Self {
        Self {
            threads: Arc::new([]),
            file: 0,
            ids: HashMap::new(),
            of: HashMap::new(),
            starts: Vec::new(),
            cache: RefCell::default(),
            highlight: true,
            share: None,
            fold: Fold::default(),
            writing: None,
            new_at: None,
        }
    }

    fn block(&self, id: BlockId) -> Option<Block> {
        if id == NEW {
            return self.new_at.map(|_| Block::New);
        }
        self.of.get(&id).map(|&i| Block::Thread(i))
    }

    fn id(&self, block: Block) -> Option<BlockId> {
        match block {
            Block::New => Some(NEW),
            Block::Thread(i) => self.ids.get(&self.threads.get(i)?.root).copied(),
        }
    }

    /// Where the text box hangs when it writes a new thread.
    fn writes_new(&self) -> Option<Attach> {
        match &self.writing.as_ref()?.at {
            Writes::New { side, lines } => Some(Attach::Line {
                side: *side,
                line: lines.end.saturating_sub(1),
            }),
            Writes::File => Some(Attach::Start),
            Writes::Reply(_) | Writes::Edit { .. } => None,
        }
    }

    /// The core's blocks for the threads, then the text box for a new one.
    fn list(&self) -> Vec<document::Block> {
        let mut out = thread_blocks(&self.threads, |_, t| self.ids.get(&t.root).copied());
        out.extend(self.new_at.map(|at| document::Block {
            id: NEW,
            at,
            text: Arc::from(""),
        }));
        out
    }

    /// Whether `block` is one of the conversation page's comments, drawn
    /// across the pane and never folded.
    fn in_conversation(&self, block: Block) -> bool {
        matches!(block, Block::Thread(i)
            if matches!(self.threads.get(i).map(|t| &t.at), Some(At::Conversation { .. })))
    }

    /// Whether thread `i` shows folded; the conversation's never do.
    fn collapsed(&self, i: usize) -> bool {
        !self.in_conversation(Block::Thread(i)) && self.fold.collapsed(&self.threads[i].root)
    }

    fn writes_in(&self, i: usize) -> bool {
        self.writing
            .as_ref()
            .is_some_and(|w| writes_in(w, &self.threads[i]))
    }

    /// Where [`Doc::draw_box`] puts `block`'s rows: the column they start
    /// at, the gutter's width after it, the box's width, and whether
    /// they're a split row's old half, closed by a rule. A file's rows
    /// share one arrangement, its layout fixing the gutter, so a box's
    /// columns don't depend on its row.
    fn columns(&self, block: Block, layout: &Layout) -> (usize, usize, usize, bool) {
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
        let split = !layout.one_sided && layout.arrangement(&ANY_HUNK) == Arrangement::Split;
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

    /// Rows `block`'s box takes in `layout`.
    fn height(&self, block: Block, layout: &Layout) -> usize {
        let (_, _, boxed, _) = self.columns(block, layout);
        // The text box changes with every key, so a box holding it is
        // counted afresh.
        let key = match block {
            Block::Thread(i) if !self.writes_in(i) => Some((i, usize::from(self.collapsed(i)))),
            _ => None,
        };
        if let Some((i, folded)) = key
            && let Some(rows) = Boxed::at(&mut self.cache.borrow_mut(), i, boxed).rows[folded]
        {
            return rows;
        }
        let mut c = Canvas::default();
        let mut writing = self.writing.clone();
        self.render(block, boxed, (None, 0), writing.as_mut(), &mut c);
        let rows = c.len();
        if let Some((i, folded)) = key {
            Boxed::at(&mut self.cache.borrow_mut(), i, boxed).rows[folded] = Some(rows);
        }
        rows
    }

    /// Writes `block`'s box, `boxed` columns wide, into `c`, focused on
    /// thread `focus` and aged at `now`. `writing` is the text box drawn
    /// in it (taken out of `self` while drawing, a copy while measuring).
    /// Returns whether it has the focus when it's a folded thread's one row.
    fn render(
        &self,
        block: Block,
        boxed: usize,
        (focus, now): (Option<&str>, u64),
        writing: Option<&mut Writing>,
        c: &mut Canvas,
    ) -> Option<bool> {
        match block {
            Block::Thread(i) => {
                let thread = &self.threads[i];
                let writing = writing.filter(|w| writes_in(w, thread));
                let writes = writing.is_some();
                let collapsed = self.collapsed(i);
                let focused = writes || focus == Some(thread.root.as_str());
                let mut cache = self.cache.borrow_mut();
                let held = Boxed::at(&mut cache, i, boxed);
                let mut comment_rows = held.comment_rows.take().unwrap_or_default();
                let bodies: &[markdown::Body] = if collapsed && !writes {
                    &[]
                } else {
                    held.bodies
                        .get_or_insert_with(|| comments::bodies(thread, boxed, self.highlight))
                };
                let card = comments::thread_rows(
                    thread,
                    bodies,
                    (collapsed, focused),
                    writing,
                    (boxed, now, self.share.as_deref()),
                    (c, &mut comment_rows),
                );
                if !collapsed && !writes {
                    held.comment_rows = Some(comment_rows);
                }
                card.then_some(focused)
            }
            Block::New => {
                if let Some(writing) = writing {
                    comments::new_rows(&mut writing.text, boxed, c);
                }
                None
            }
        }
    }

    /// Thread `i`'s open box at `layout`: the column its text starts at
    /// from the pane's left, its width, and where each comment is in it.
    /// `None` folded, or with the text box in it.
    fn comment_rows(&self, i: usize, layout: &Layout) -> Option<(usize, usize, Vec<CommentRows>)> {
        if self.collapsed(i) || self.writes_in(i) {
            return None;
        }
        let block = Block::Thread(i);
        let (start, gutter, boxed, _) = self.columns(block, layout);
        // The columns `Doc::draw_box` draws before the box: hits and the
        // highlight miss by as many if the two drift.
        let ruled = if self.in_conversation(block) {
            0
        } else {
            gutter.saturating_sub(2) + 3
        };
        let drawn = Boxed::at(&mut self.cache.borrow_mut(), i, boxed)
            .comment_rows
            .is_some();
        if !drawn {
            self.render(block, boxed, (None, 0), None, &mut Canvas::default());
        }
        let comment_rows = Boxed::at(&mut self.cache.borrow_mut(), i, boxed)
            .comment_rows
            .clone()?;
        Some((start + ruled + comments::TEXT_COLUMN, boxed, comment_rows))
    }

    /// Runs `f` on comment `k` of thread `i` as its box draws it, `boxed`
    /// wide.
    fn with_body<R>(
        &self,
        i: usize,
        k: usize,
        boxed: usize,
        f: impl FnOnce(&markdown::Body) -> R,
    ) -> Option<R> {
        let thread = self.threads.get(i)?;
        let mut cache = self.cache.borrow_mut();
        let bodies = Boxed::at(&mut cache, i, boxed)
            .bodies
            .get_or_insert_with(|| comments::bodies(thread, boxed, self.highlight));
        bodies.get(k).map(f)
    }

    /// The comment of thread `i` that line `line` of its block's text is
    /// in, and where `bytes` of that line are in the comment's text.
    fn in_comment(
        &self,
        i: usize,
        line: usize,
        bytes: Range<usize>,
    ) -> Option<(usize, Range<usize>)> {
        let starts = self.starts.get(i)?;
        let k = starts.partition_point(|&s| s <= line).checked_sub(1)?;
        let text = &self.threads[i].comments.get(k)?.text;
        let from: usize = (text.split_inclusive('\n'))
            .take(line - starts[k])
            .map(str::len)
            .sum();
        Some((k, from + bytes.start..from + bytes.end))
    }

    /// The row of block `id`'s box showing line `line` of its text: the
    /// row its comment's text has it on, or the heading of a comment the
    /// text box edits; a folded thread's one row.
    fn locate(&self, id: BlockId, layout: &Layout, line: usize) -> usize {
        let Some(Block::Thread(i)) = self.block(id) else {
            return 0;
        };
        let Some((k, bytes)) = self.in_comment(i, line, 0..0) else {
            return 0;
        };
        let Some((_, boxed, comment_rows)) = self.comment_rows(i, layout) else {
            return 0;
        };
        let Some(comment) = comment_rows.get(k) else {
            return 0;
        };
        let row = self.with_body(i, k, boxed, |body| body.map.row_of(bytes.start));
        comment.text.start + row.flatten().unwrap_or(0)
    }
}

/// `threads` as the core's blocks, in their order, each with the id `id`
/// gives it (from its index) and its comments' text to search: a thread on
/// the whole file hangs under the file's first row, one on lines under the
/// row of its last, one whose lines are gone and the conversation's
/// comments under the last row.
pub(crate) fn thread_blocks(
    threads: &[Thread],
    id: impl Fn(usize, &Thread) -> Option<BlockId>,
) -> Vec<document::Block> {
    (threads.iter().enumerate())
        .filter_map(|(i, t)| {
            let at = match &t.at {
                At::File => Attach::Start,
                At::Line { side, lines } => Attach::Line {
                    side: *side,
                    line: lines.end.saturating_sub(1),
                },
                At::Lost | At::Conversation { .. } => Attach::End,
            };
            let text: Vec<&str> = t.comments.iter().map(|n| n.text.as_str()).collect();
            Some(document::Block {
                id: id(i, t)?,
                at,
                text: text.join("\n").into(),
            })
        })
        .collect()
}

/// Whether the text box writes in `thread`: a reply, or an edit of one of
/// its comments.
fn writes_in(writing: &Writing, thread: &Thread) -> bool {
    match &writing.at {
        Writes::Reply(root) | Writes::Edit { root, .. } => *root == thread.root,
        Writes::New { .. } | Writes::File => false,
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
    /// The search's matches on the row being painted, old side's first, as
    /// [`restyle_matches`] takes them; kept so painting doesn't allocate.
    search_spans: Vec<(u32, u32, Style)>,
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
    pub comment_selection: Option<CommentSelection>,
    /// The comment threads of the file shown, as boxes.
    pub boxes: Boxes,
    /// The root of the thread `m`, `M` or a click focused.
    focus: Option<String>,
    /// Seconds since 1970, for the threads' ages.
    pub now: u64,
    /// When an age on a box drawn since the last frame drawn whole next
    /// reads differently: the frame from then is drawn whole. A change of
    /// focus or of threads draws the next frame whole too.
    ages_until: u64,
    /// The box a frame last drew, for the next of its rows down the pane.
    box_drawn: Option<(BlockId, Canvas, Option<Card>)>,
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
            search_spans: Vec::new(),
            overview: false,
            separators: Vec::new(),
            target: None,
            drawn: Vec::new(),
            painted: None,
            selection: None,
            comment_selection: None,
            boxes: Boxes::new(),
            focus: None,
            now: 0,
            ages_until: u64::MAX,
            box_drawn: None,
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
        let layout = self.layout.for_file(entry.status(), lines);
        Layout {
            // Both sides are the same lines: one column says it.
            one_sided: layout.one_sided || entry.is_unchanged(),
            ..layout
        }
    }

    /// Keeps the position inside the document after rows changed under it
    /// (a diff arrived, the layout changed).
    pub(crate) fn normalize(&mut self) {
        let layout = self.file_layout(self.document.pos.file);
        if !self.document.normalize(arrange(&layout, &self.boxes)) {
            return;
        }
        let pos = self.document.pos;
        self.document.pos.offset = pos.offset.min(self.height(pos, &layout) - 1);
        self.clamp_in(&layout);
    }

    /// Lays rows out with `layout` from now on, the row at the top kept on
    /// what it showed (see [`Document::relayout`]).
    pub(crate) fn set_layout(&mut self, layout: Layout) {
        let file = self.document.pos.file;
        let before = self.file_layout(file);
        self.layout = layout;
        let after = self.file_layout(file);
        // Scrolling sideways moves no row.
        if (Layout {
            scroll: 0,
            ..before
        }) != (Layout { scroll: 0, ..after })
        {
            let (old, new) = (arrange(&before, &self.boxes), arrange(&after, &self.boxes));
            self.document.relayout(old, new, |_| {});
            if self.boxes.file != file {
                self.remeasure_boxes();
            }
        }
        self.normalize();
    }

    /// Measures every box again: the file's layout changed at the same
    /// width, or what the boxes show did.
    fn remeasure_boxes(&mut self) {
        let file = self.boxes.file;
        let layout = self.file_layout(file);
        (self.document).remeasure(file, None, arrange(&layout, &self.boxes));
        self.rehome_selection(file);
    }

    /// Puts the selection's ends back on the rows showing their lines,
    /// once boxes of `file` above them took other heights; drops it when a
    /// line has no row any more.
    fn rehome_selection(&mut self, file: usize) {
        let Some(s) = self.selection.as_ref().filter(|s| s.anchor.file == file) else {
            return;
        };
        match self.selection_rows(s.anchor.file, s.side, s.lines) {
            Some((anchor, cursor)) => {
                if let Some(s) = &mut self.selection {
                    (s.anchor, s.cursor) = (anchor, cursor);
                }
            }
            None => self.selection = None,
        }
    }

    /// The rows showing `lines` of `side` in `file`, the selection's ends.
    fn selection_rows(
        &mut self,
        file: usize,
        side: Side,
        lines: (usize, usize),
    ) -> Option<(Pos, Pos)> {
        let layout = self.file_layout(file);
        let mut row = |line: usize| {
            let m = document::Match {
                side,
                line: u32::try_from(line).ok()?,
                start: 0,
                end: 0,
            };
            (self.document).match_pos(file, &m, arrange(&layout, &self.boxes))
        };
        Some((row(lines.0)?, row(lines.1)?))
    }

    /// Shows the syntax diff where there's one, or the line diff, the row
    /// at the top kept on what it showed.
    pub(crate) fn set_structural(&mut self, on: bool) {
        let layout = self.file_layout(self.document.pos.file);
        let arrange = arrange(&layout, &self.boxes);
        self.document
            .relayout(arrange, arrange, |doc| doc.structural = on);
        self.normalize();
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
    /// anew, and positions in it are void. Its gutter may be another
    /// width, so its boxes are measured again.
    fn settle_render(&mut self, settle: Settle) {
        if let Settle::Open { .. } = settle {
            self.selection = None;
            self.remeasure_boxes();
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
    /// it away with `None` (see [`Document::set_page`]): its title, then a
    /// blank row the comments hang under, each a blank row apart. The
    /// selection and the threads shown move with their files, and the next
    /// frame is drawn whole. Returns how far the other entries moved.
    pub(crate) fn set_page(&mut self, comments: Option<usize>) -> isize {
        let page = comments.map(|n| {
            let change = dffy_core::git::FileChange {
                path: BString::default(),
                status: dffy_core::git::Status::Modified,
                old: None,
                new: None,
                staging: None,
            };
            (change, 1 + usize::from(n > 0))
        });
        let moved = self.document.set_page(page);
        if moved != 0 {
            if let Some(s) = &mut self.selection {
                s.anchor.file = s.anchor.file.saturating_add_signed(moved);
                s.cursor.file = s.cursor.file.saturating_add_signed(moved);
            }
            self.boxes.file = self.boxes.file.saturating_add_signed(moved);
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
        let change = self.document.open(file, arrange(&layout, &self.boxes));
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
            Source::Repo | Source::Page { .. } | Source::Unchanged => return None,
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
    /// offset), drawn with its file's `layout`: code, or a row of a box.
    fn render(&mut self, pos: Pos, layout: &Layout, c: &mut Canvas) -> Rows {
        match self.render_code(pos, layout, c) {
            Drawn::Code => Rows::default(),
            Drawn::Separator => Rows {
                separator: true,
                card: None,
            },
            Drawn::Block(id, line) => Rows {
                separator: false,
                card: self.block_row(id, line, layout, c),
            },
            Drawn::Parting(id) => {
                self.parting_row(id, layout, c);
                Rows::default()
            }
        }
    }

    /// [`Doc::render`]'s code rows; what it leaves to draw for blocks.
    fn render_code(&mut self, pos: Pos, layout: &Layout, c: &mut Canvas) -> Drawn {
        let target = self.target == Some(Pos { offset: 0, ..pos });
        match self.document.row(pos, arrange(layout, &self.boxes)) {
            Some(Content::Start(header)) => {
                layout.hunk_row(&header, Row::Header, |_| NO_NEWLINE_LINE, c);
            }
            Some(Content::Hunk { header, row, lines }) => {
                layout.hunk_row(&header, row, |i| lines.line(i), c);
            }
            Some(Content::Separator { hidden, next }) => {
                layout.separator(hidden, next.as_ref(), target, c);
                return Drawn::Separator;
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
            Some(Content::Block { id, line }) => return Drawn::Block(id, line),
            Some(Content::Parting { id }) => return Drawn::Parting(id),
            Some(Content::Sheet(row)) => {
                let sheet = (self.document.files().get(pos.file)).is_some_and(Entry::is_sheet);
                if sheet {
                    self.sheet_row(pos.file, row, c);
                } else {
                    // The note in the middle of the pane says why it's empty.
                    c.end_line();
                }
            }
            None => {
                if pos.part == 0 {
                    c.end_line();
                }
            }
        }
        Drawn::Code
    }

    /// Appends row `line` of block `id`'s box. A frame goes down a box's
    /// rows one at a time, so the box is drawn whole once, for the first of
    /// them. Returns its card when it's a folded thread's one row.
    fn block_row(
        &mut self,
        id: BlockId,
        line: usize,
        layout: &Layout,
        c: &mut Canvas,
    ) -> Option<Card> {
        let Some(block) = self.boxes.block(id) else {
            c.end_line();
            return None;
        };
        if self
            .box_drawn
            .as_ref()
            .is_none_or(|(drawn, ..)| *drawn != id)
        {
            let mut rows = self
                .box_drawn
                .take()
                .map(|(_, rows, _)| rows)
                .unwrap_or_default();
            rows.clear();
            let card = self.draw_box(block, layout, &mut rows);
            self.box_drawn = Some((id, rows, card));
        }
        let (_, rows, card) = self.box_drawn.as_ref()?;
        if let Some(row) = rows.rows().nth(line) {
            for (ink, piece) in row {
                c.push_ink(ink, piece);
            }
        }
        c.end_line();
        card.clone().filter(|_| line == 0)
    }

    /// Appends `block`'s box, each row after the gutter's rule, its side's
    /// half of a split row for a thread on a line. Returns the card of a
    /// folded thread.
    fn draw_box(&mut self, block: Block, layout: &Layout, c: &mut Canvas) -> Option<Card> {
        let columns = self.boxes.columns(block, layout);
        let (start, gutter, boxed, _) = columns;
        let mut rows = std::mem::take(&mut self.scratch);
        rows.clear();
        let mut writing = self.boxes.writing.take();
        let focused = (self.boxes).render(
            block,
            boxed,
            (self.focus.as_deref(), self.now),
            writing.as_mut(),
            &mut rows,
        );
        self.boxes.writing = writing;
        if let Block::Thread(i) = block {
            let next = comments::ages_change(&self.boxes.threads[i], self.now);
            self.ages_until = self.ages_until.min(next);
        }
        let left = start + gutter.saturating_sub(2) + 3;
        let card = focused.map(|focused| Card {
            columns: left + 1..left + boxed - 1,
            focused,
        });
        let ruled = !self.boxes.in_conversation(block);
        for row in rows.rows() {
            rule_row(c, columns, ruled, |c| {
                for (ink, piece) in row {
                    c.push_ink(ink, piece);
                }
            });
        }
        self.scratch = rows;
        card
    }

    /// Appends the blank row parting block `id` from the one above it, in
    /// its columns.
    fn parting_row(&mut self, id: BlockId, layout: &Layout, c: &mut Canvas) {
        let Some(block) = self.boxes.block(id) else {
            c.end_line();
            return;
        };
        let columns = self.boxes.columns(block, layout);
        let ruled = !self.boxes.in_conversation(block);
        rule_row(c, columns, ruled, |c| c.spaces(Role::Plain, columns.2));
    }

    /// Screen rows the logical row at `pos` takes, in its file's `layout`.
    fn height(&mut self, pos: Pos, layout: &Layout) -> usize {
        // Rendered views show only while the mode is on, so a line diff
        // without wrapping pays a flag and a lookup here for its code.
        let one = pos.part == 0
            || !(self.layout.wrap || (self.document.rendered() && self.is_rendered(pos.file)));
        if one {
            return 1;
        }
        self.code_rows(pos, layout).max(1)
    }

    /// [`Doc::render_code`]'s rows for the logical row at `pos`; none for
    /// a block's.
    fn code_rows(&mut self, pos: Pos, layout: &Layout) -> usize {
        let mut c = std::mem::take(&mut self.scratch);
        c.clear();
        self.render_code(pos, layout, &mut c);
        let rows = c.len();
        self.scratch = c;
        rows
    }

    /// Takes `threads`, those of `file`, the file shown, as its blocks; the
    /// file shown before keeps none. Rows change around the position, so
    /// it's kept on what it showed. Text selected in a comment ends: its
    /// text may have changed under its id.
    pub(crate) fn set_threads(&mut self, file: usize, threads: Arc<[Thread]>) {
        let same = if threads.is_empty() {
            self.boxes.threads.is_empty()
        } else {
            Arc::ptr_eq(&threads, &self.boxes.threads)
        };
        if same && file == self.boxes.file {
            return;
        }
        // Measured as the boxes it had, before they're replaced.
        if file != self.boxes.file {
            self.set_blocks(self.boxes.file, Vec::new());
        }
        // Another file, and still no boxes: no rows changed.
        if threads.is_empty()
            && self.boxes.threads.is_empty()
            && self.boxes.new_at.is_none()
            && self.boxes.writing.is_none()
        {
            self.boxes.file = file;
            return;
        }
        let boxes = &mut self.boxes;
        boxes.of.clear();
        boxes.starts.clear();
        for (i, thread) in threads.iter().enumerate() {
            let next = BlockId(u32::try_from(boxes.ids.len()).unwrap_or(NEW.0 - 1));
            let id = *boxes.ids.entry(thread.root.clone()).or_insert(next);
            boxes.of.insert(id, i);
            let mut line = 0;
            let starts = (thread.comments.iter()).map(|n| {
                let start = line;
                line += n.text.split('\n').count();
                start
            });
            boxes.starts.push(starts.collect());
        }
        boxes.cache.get_mut().clear();
        boxes.fold.learn(&threads);
        boxes.threads = threads;
        boxes.file = file;
        boxes.new_at = boxes.writes_new();
        self.comment_selection = None;
        self.painted = None;
        let list = self.boxes.list();
        self.set_blocks(file, list);
        self.fit();
    }

    /// Gives the core `blocks` as `file`'s, unless it has none and gets
    /// none.
    fn set_blocks(&mut self, file: usize, blocks: Vec<document::Block>) {
        if blocks.is_empty() && self.document.blocks(file).is_empty() {
            return;
        }
        let layout = self.file_layout(file);
        (self.document).set_blocks(file, blocks, arrange(&layout, &self.boxes));
        self.rehome_selection(file);
    }

    /// The file whose threads the boxes are.
    pub(crate) fn threads_file(&self) -> usize {
        self.boxes.file
    }

    /// The threads of the file shown.
    pub(crate) fn threads(&self) -> &[Thread] {
        if self.boxes.file == self.document.pos.file {
            &self.boxes.threads
        } else {
            &[]
        }
    }

    /// Measures the boxes again after what they show changed (folds, the
    /// text box opened or closed), keeping the position on what it showed
    /// and the screen full.
    pub(crate) fn refit(&mut self) {
        if self.document.files().is_empty() {
            return;
        }
        let new_at = self.boxes.writes_new();
        if new_at == self.boxes.new_at {
            self.remeasure_boxes();
        } else {
            self.boxes.new_at = new_at;
            let list = self.boxes.list();
            self.set_blocks(self.boxes.file, list);
        }
        self.painted = None;
        self.fit();
    }

    /// Measures `block` again, as [`Doc::refit`] does every box.
    fn remeasure(&mut self, block: Block) {
        let file = self.boxes.file;
        if let Some(id) = self.boxes.id(block) {
            let layout = self.file_layout(file);
            (self.document).remeasure(file, Some(id), arrange(&layout, &self.boxes));
            self.rehome_selection(file);
        }
        self.painted = None;
        self.fit();
    }

    /// How the threads are folded.
    pub(crate) fn fold(&self) -> &Fold {
        &self.boxes.fold
    }

    /// `z`: folds the thread with root `root`, or unfolds it.
    pub(crate) fn toggle_fold(&mut self, root: &str) {
        self.boxes.fold.toggle(root);
        if let Some(i) = self.boxes.threads.iter().position(|t| t.root == root) {
            self.remeasure(Block::Thread(i));
        }
    }

    /// `Z` (see [`Fold::toggle_all`]): returns whether every thread is
    /// folded now.
    pub(crate) fn toggle_folds(&mut self) -> bool {
        let folded = self.boxes.fold.toggle_all();
        self.refit();
        folded
    }

    /// The text box, while it's open.
    pub(crate) fn writing(&self) -> Option<&Writing> {
        self.boxes.writing.as_ref()
    }

    /// Opens the text box: in the thread it replies in or edits, kept
    /// whole, or under the lines of a new thread.
    pub(crate) fn write(&mut self, writing: Writing) {
        if let Writes::Reply(root) | Writes::Edit { root, .. } = &writing.at {
            self.boxes.fold.keep_open(root);
        }
        self.boxes.writing = Some(writing);
        self.refit();
    }

    /// Closes the text box and returns it.
    pub(crate) fn stop_writing(&mut self) -> Option<Writing> {
        let writing = self.boxes.writing.take()?;
        self.refit();
        Some(writing)
    }

    /// Changes the text box's text with `f`; its box is measured again.
    pub(crate) fn edit_writing(&mut self, f: impl FnOnce(&mut Writing)) {
        if let Some(writing) = &mut self.boxes.writing {
            f(writing);
        }
        if let Some(block) = self.writing_block() {
            self.remeasure(block);
        }
    }

    /// Keeps the position within its row and the screen full.
    fn fit(&mut self) {
        if self.document.files().is_empty() {
            return;
        }
        let layout = self.file_layout(self.document.pos.file);
        let pos = self.document.pos;
        self.document.pos.offset = pos.offset.min(self.height(pos, &layout) - 1);
        self.clamp_in(&layout);
    }

    /// What follows a private comment's word in its heading, as `A shares`;
    /// set before any box is drawn.
    pub(crate) fn set_share(&mut self, words: Option<String>) {
        self.boxes.share = words;
    }

    /// Highlights fenced code in comments, or stops; their boxes are drawn
    /// again.
    pub(crate) fn set_highlight(&mut self, on: bool) {
        if self.boxes.highlight != on {
            self.boxes.highlight = on;
            for boxed in self.boxes.cache.get_mut().values_mut() {
                boxed.bodies = None;
            }
            self.painted = None;
        }
    }

    /// Moves `pos` to the next logical row of its file; false at the end.
    #[inline]
    fn next_row(&mut self, pos: &mut Pos, layout: &Layout) -> bool {
        self.document.next_row(pos, arrange(layout, &self.boxes))
    }

    /// Moves `pos` to the last screen row of the previous logical row of
    /// its file; false at the start.
    fn prev_row(&mut self, pos: &mut Pos, layout: &Layout) -> bool {
        let mut prev = *pos;
        if !self
            .document
            .prev_row(&mut prev, arrange(layout, &self.boxes))
        {
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
        let layout = self.file_layout(self.document.pos.file);
        Some(
            match self.document.context(arrange(&layout, &self.boxes))? {
                document::Context::Heading(text) => Context {
                    numbers: None,
                    heading: crate::render::escape(display_text(text)),
                },
                document::Context::Hunk(header) => Context {
                    numbers: Some(header.numbers()),
                    heading: crate::render::escape(display_text(header.heading)),
                },
            },
        )
    }

    /// The definitions around the code at the top, outermost first (see
    /// [`File::scopes`]): around the top row's new line, or its old line
    /// on a row with only that. A box's row stands for the row it hangs
    /// under, and a row without code (a hunk's header, a separator) for the
    /// next row with some. `None` when the file has no definitions to look
    /// in.
    pub(crate) fn scope(&mut self) -> Option<Vec<String>> {
        let layout = self.file_layout(self.document.pos.file);
        let mut pos = Pos {
            offset: 0,
            ..self.document.pos
        };
        while self.in_block(pos)
            && self
                .document
                .prev_row(&mut pos, arrange(&layout, &self.boxes))
        {}
        // A separator, then a header: code is at most two own rows on.
        let mut own = 0;
        while own < 3 {
            let (side, line) = match self.document.row_lines(pos, arrange(&layout, &self.boxes)) {
                [_, Some(new)] => (Side::New, new),
                [Some(old), None] => (Side::Old, old),
                [None, None] => {
                    own += usize::from(!self.in_block(pos));
                    if !self
                        .document
                        .next_row(&mut pos, arrange(&layout, &self.boxes))
                    {
                        return None;
                    }
                    continue;
                }
            };
            let file = self.document.file(pos.file)?;
            return Some(
                file.scopes(side, line)?
                    .into_iter()
                    .map(|label| crate::render::escape(label.as_bytes()))
                    .collect(),
            );
        }
        None
    }

    pub(crate) fn bottom(&mut self) {
        if self.document.files().is_empty() {
            return;
        }
        let file = self.document.pos.file;
        let layout = self.file_layout(file);
        let part = self.document.parts(file) - 1;
        let row = self
            .document
            .rows(file, part, arrange(&layout, &self.boxes))
            - 1;
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
        if let Some(at) = self
            .document
            .edge(file, last, arrange(&layout, &self.boxes))
        {
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
        let settle = self.document.next_change(arrange(&layout, &self.boxes))?;
        self.settle_change(settle)
    }

    /// Puts the previous block of changed lines at the top. Before the
    /// file's first, opens the previous file at its last change (or its
    /// top while it isn't diffed) and returns that file.
    pub(crate) fn prev_change(&mut self) -> Option<usize> {
        let layout = self.file_layout(self.document.change_file());
        let settle = self.document.prev_change(arrange(&layout, &self.boxes))?;
        self.settle_change(settle)
    }

    /// Reveals more of gap `gap` of `file` (see [`Document::expand`]).
    /// Returns whether any line was hidden.
    pub(crate) fn expand(&mut self, file: usize, gap: usize) -> bool {
        let layout = self.file_layout(file);
        if !self
            .document
            .expand(file, gap, arrange(&layout, &self.boxes))
        {
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
        let layout = self.file_layout(self.document.pos.file);
        if self.document.toggle_full(arrange(&layout, &self.boxes)) {
            self.normalize();
        }
    }

    /// Reveals the lines of `placements` on `file` that its gaps hide (see
    /// [`Document::reveal_annotations`]). Returns whether any were hidden.
    pub(crate) fn reveal_annotations(&mut self, file: usize, placements: &[Placement]) -> bool {
        let layout = self.file_layout(file);
        (self.document).reveal_annotations(file, placements, arrange(&layout, &self.boxes))
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
        if let Some(at) = self
            .document
            .change_after(file, target, arrange(&layout, &self.boxes))
        {
            self.land(at);
        }
    }

    /// The new-side line number (1-based) that the row at `pos` shows, or
    /// the first after it.
    fn line_of(&mut self, pos: Pos) -> usize {
        let layout = self.file_layout(pos.file);
        self.document.line_of(pos, arrange(&layout, &self.boxes))
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
        let (scrolled, rows, pos) = if let Some((by, rows, from)) = reused {
            (Some(by), rows, from)
        } else {
            self.separators.clear();
            self.drawn.clear();
            self.drawn.resize(self.height, None);
            self.ages_until = u64::MAX;
            (None, 0..self.height, self.document.pos)
        };
        let kept = self.separators.len();
        self.box_drawn = None;
        let (y, last) = self.draw_rows(grid, &mut c, area, rows.clone(), pos, &layout, theme);
        self.box_drawn = None;
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
        self.draw_comment_selection(grid, area);
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
                self.document
                    .line_of(self.document.pos, arrange(&layout, &self.boxes)),
                self.document.line_of(last, arrange(&layout, &self.boxes)),
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

    /// Lays out and paints screen rows `rows` from `pos` down, as far as
    /// the file goes, noting each row in `drawn` and each separator at its
    /// top. Returns the row after the last painted and the position of the
    /// last laid out.
    // Out of line, so the code layout of every scroll's row loop doesn't
    // shift as `draw` grows.
    #[inline(never)]
    #[expect(clippy::too_many_arguments, reason = "draw's state, passed through")]
    fn draw_rows(
        &mut self,
        grid: &mut Grid,
        c: &mut Canvas,
        (x, end, top): (usize, usize, usize),
        rows: Range<usize>,
        mut pos: Pos,
        layout: &Layout,
        theme: &Theme,
    ) -> (usize, Pos) {
        let mut y = rows.start;
        let mut last = pos;
        while y < rows.end {
            c.clear();
            let drawn = self.render(pos, layout, c);
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
                if let Some(card) = &drawn.card {
                    card.tint(grid, (x, end, top + y), theme);
                }
                self.drawn[y] = Some(Pos { offset: i, ..pos });
                y += 1;
            }
            last = pos;
            if !self.next_row(&mut pos, layout) {
                break;
            }
        }
        (y, last)
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
    /// document's (the note), while lines or text are selected, or once an
    /// age on a box it copies reads differently: scrolling with a selection
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
            || self.comment_selection.is_some()
            || self.now >= self.ages_until
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

    /// Paints the search's matches on screen rows `rows`, the current one
    /// in its own color. Only those rows are looked up, however many
    /// matches the file has. `layout` is the drawn file's.
    // Out of line, so the code layout of the per-row match loop doesn't
    // shift as `draw` grows.
    #[inline(never)]
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
        let mut spans = std::mem::take(&mut self.search_spans);
        let mut previous = None;
        let blocks = !marks.matches.blocks.is_empty()
            && marks.blocks_version == self.document.blocks_version(marks.file);
        for y in rows {
            let Some(pos) = self.drawn[y] else { continue };
            let logical = Pos { offset: 0, ..pos };
            if pos.file != marks.file {
                continue;
            }
            let lines = self
                .document
                .row_lines(logical, arrange(layout, &self.boxes));
            if lines == [None, None] && blocks {
                let row = self.document.row(logical, arrange(layout, &self.boxes));
                if let Some(Content::Block { id, line }) = row {
                    self.draw_box_matches(
                        grid,
                        (x, end, top + y),
                        theme,
                        layout,
                        &marks,
                        (id, line),
                    );
                }
                continue;
            }
            let (matches, current) = (&marks.matches, marks.current);
            let on = move |side: Side| {
                (lines[side.index()].into_iter())
                    .flat_map(move |line| matches.on(side, line))
                    .map(move |i| {
                        let found = matches.list[i];
                        let role = if current == Some(matches.of_line(i)) {
                            Role::SearchCurrent
                        } else {
                            Role::SearchMatch
                        };
                        (found.start, found.end, theme.style(role))
                    })
            };
            spans.clear();
            spans.extend(on(Side::Old));
            let on_old = spans.len();
            spans.extend(on(Side::New));
            if spans.is_empty() {
                continue;
            }
            if previous != Some(logical) {
                mapped.clear();
                self.render_code(logical, layout, &mut mapped);
                previous = Some(logical);
            }
            let split = self.split_row(logical, layout);
            let Some(map) = mapped.map() else { continue };
            // Glyphs come in the order of the canvas's rows.
            let first = map.glyphs.partition_point(|g| g.row < pos.offset);
            let glyphs = &map.glyphs[first..];
            let glyphs = &glyphs[..glyphs.partition_point(|g| g.row == pos.offset)];
            let (on_old, on_new) = spans.split_at(on_old);
            restyle_matches(grid, glyphs, [on_old, on_new], split, (x, end, top + y));
        }
        self.mapped = mapped;
        self.search_spans = spans;
        self.document.search = Some(marks);
    }

    /// Paints the matches of the search in row `line` of block `id`, a
    /// thread's open box, on screen row `y`, as far as the box's padding.
    fn draw_box_matches(
        &mut self,
        grid: &mut Grid,
        (x, end, y): (usize, usize, usize),
        theme: &Theme,
        layout: &Layout,
        marks: &document::Found,
        (id, line): (BlockId, usize),
    ) {
        let Some(Block::Thread(i)) = self.boxes.block(id) else {
            return;
        };
        let Some((left, boxed, comment_rows)) = self.boxes.comment_rows(i, layout) else {
            return;
        };
        let Some(k) = comment_rows.iter().position(|n| n.text.contains(&line)) else {
            return;
        };
        let row = line - comment_rows[k].text.start;
        let inner = comments::inner(boxed);
        let found = (marks.matches.blocks.iter().enumerate()).filter(|(_, m)| m.block == id);
        for (n, m) in found {
            let bytes = m.start as usize..m.end as usize;
            let Some((comment, bytes)) = self.boxes.in_comment(i, m.line as usize, bytes) else {
                continue;
            };
            if comment != k {
                continue;
            }
            let role = if marks.current == Some(m.after as usize + n) {
                Role::SearchCurrent
            } else {
                Role::SearchMatch
            };
            let cells: Vec<Range<usize>> = (self.boxes)
                .with_body(i, k, boxed, |body| {
                    let cells = body.map.cells(row, bytes);
                    cells.filter(|c| c.end <= inner).collect()
                })
                .unwrap_or_default();
            for cells in cells {
                let from = (x + left + cells.start).min(end);
                grid.restyle(from, y, (x + left + cells.end).min(end), theme.style(role));
            }
        }
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
        let threads = Arc::clone(&self.boxes.threads);
        let mut mapped = std::mem::take(&mut self.mapped);
        let mut previous = None;
        for y in 0..self.drawn.len() {
            let Some(pos) = self.drawn[y] else { continue };
            let logical = Pos { offset: 0, ..pos };
            if pos.file != file || previous == Some(logical) {
                continue;
            }
            previous = Some(logical);
            let lines = self
                .document
                .row_lines(logical, arrange(layout, &self.boxes));
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
                let mut style = theme.style(comments::look_role(look));
                style.bg = grid
                    .row(top + y)
                    .get(column)
                    .and_then(|cell| cell.style().bg)
                    .or(style.bg);
                grid.put_str(column, top + y, end, comments::glyph(look), style);
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

    /// The lines staging, unstaging or discarding takes, and their file:
    /// every line on the rows the selection spans, both sides of a row
    /// drawn as one, the selection's side of a row drawn as two halves;
    /// else the line diff's hunk around the row `at`, or around the first
    /// row on screen in one. Why there are none otherwise.
    pub(crate) fn edit_target(
        &mut self,
        at: Option<(Pos, Side)>,
    ) -> Result<(usize, Picked), &'static str> {
        let file = match (&self.selection, at) {
            (Some(s), _) => s.anchor.file,
            (None, Some((pos, _))) => pos.file,
            (None, None) => self.document.pos.file,
        };
        if self.document.file(file).and_then(File::rendered).is_some() {
            return Err("a rendered view has no lines to stage; r shows them");
        }
        let layout = self.file_layout(file);
        let mut picked = Picked::default();
        let mut take = |lines: [Option<usize>; 2]| {
            picked.old.extend(lines[0]);
            picked.new.extend(lines[1]);
        };
        if let Some(s) = self.selection.clone() {
            let (anchor, cursor) = self
                .selection_rows(file, s.side, s.lines)
                .ok_or("the selection's lines aren't shown")?;
            let (mut pos, last) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            loop {
                let mut lines = self.document.row_lines(pos, arrange(&layout, &self.boxes));
                if self.split_row(pos, &layout) {
                    lines[1 - s.side.index()] = None;
                }
                take(lines);
                if pos >= last || !self.next_row(&mut pos, &layout) {
                    break;
                }
            }
            return Ok((file, picked));
        }
        let diff = Arc::clone(
            self.document
                .diff_of(file)
                .ok_or("this file isn't diffed yet")?,
        );
        let within = |h: &Hunk, [old, new]: [Option<usize>; 2]| {
            old.is_some_and(|l| (h.old_start..h.old_start + h.old_len).contains(&l))
                || new.is_some_and(|l| (h.new_start..h.new_start + h.new_len).contains(&l))
        };
        let rows: Vec<Pos> = match at {
            Some((pos, _)) => vec![pos],
            None => (0..self.drawn.len())
                .filter_map(|y| self.row_at(y))
                .filter(|p| p.file == file)
                .collect(),
        };
        for pos in rows {
            let lines = self.document.row_lines(pos, arrange(&layout, &self.boxes));
            if let Some(h) = diff.hunks.iter().find(|h| within(h, lines)) {
                return Ok((file, Picked::hunk(h)));
            }
        }
        Err("no change here; v selects lines")
    }

    /// The conflict regions of the file shown with a row on screen, in
    /// order, or with `at` the one that row is in.
    pub(crate) fn conflicts_at(&mut self, at: Option<Pos>) -> Vec<usize> {
        let file = at.map_or(self.document.pos.file, |p| p.file);
        let rows: Vec<Pos> = match at {
            Some(pos) => vec![pos],
            None => (0..self.drawn.len())
                .filter_map(|y| self.row_at(y))
                .filter(|p| p.file == file)
                .collect(),
        };
        let mut found = Vec::new();
        for pos in rows {
            let lines = self.row_lines(pos);
            let region = self
                .document
                .file(file)
                .and_then(File::conflicted)
                .and_then(|c| c.region_at(lines));
            if let Some(region) = region
                && found.last() != Some(&region)
            {
                found.push(region);
            }
        }
        found
    }

    /// The row a view on conflict region `region` of the file shown puts
    /// at its top (see [`dffy_core::document::Document::conflict_pos`]).
    pub(crate) fn conflict_pos(&mut self, region: usize) -> Option<Pos> {
        let file = self.document.pos.file;
        let layout = self.file_layout(file);
        self.document
            .conflict_pos(file, region, arrange(&layout, &self.boxes))
    }

    /// Shows conflict region `region` of the file shown, a few rows below
    /// the top; false when it has no row to show.
    pub(crate) fn goto_conflict(&mut self, region: usize) -> bool {
        let Some(pos) = self.conflict_pos(region) else {
            return false;
        };
        self.goto_match(pos);
        true
    }

    /// The row showing match `hit` of `file`, in its code or a box.
    pub(crate) fn hit_pos(&mut self, file: usize, hit: &document::Hit) -> Option<Pos> {
        let layout = self.file_layout(file);
        self.document
            .hit_pos(file, hit, arrange(&layout, &self.boxes))
    }

    /// Shows the row at `pos` a few rows below the top of the pane.
    pub(crate) fn goto_match(&mut self, pos: Pos) {
        self.document.pos = pos;
        self.clamp();
        let context = isize::try_from(self.height / 4).unwrap_or(0);
        self.scroll(-context);
        self.reveal(pos);
    }

    /// Shows line `line` (0-based) of `side` of `file`, a loaded file, as
    /// [`Doc::goto_match`] shows a row, revealing it with a few lines
    /// around when a gap hides it. False when no row shows that line.
    pub(crate) fn goto_line(&mut self, file: usize, side: Side, line: usize) -> bool {
        let layout = self.file_layout(file);
        let around = OPEN_CONTEXT.unsigned_abs();
        let lines = line..line + 1;
        if (self.document).reveal_lines(file, side, lines, around, arrange(&layout, &self.boxes)) {
            self.normalize();
        }
        let Ok(line) = u32::try_from(line) else {
            return false;
        };
        let m = document::Match {
            side,
            line,
            start: 0,
            end: 0,
        };
        match (self.document).match_pos(file, &m, arrange(&layout, &self.boxes)) {
            Some(pos) => {
                self.goto_match(pos);
                true
            }
            None => false,
        }
    }

    /// The old and new lines (0-based) of the first row drawn in the last
    /// frame that shows code.
    pub(crate) fn top_lines(&mut self) -> [Option<usize>; 2] {
        for y in 0..self.drawn.len() {
            if let Some(pos) = self.row_at(y) {
                let lines = self.row_lines(pos);
                if lines != [None, None] {
                    return lines;
                }
            }
        }
        [None, None]
    }

    /// The old and new lines (0-based) a logical row shows; `None` on a
    /// side without one, and on rows that aren't code.
    pub(crate) fn row_lines(&mut self, pos: Pos) -> [Option<usize>; 2] {
        let layout = self.file_layout(pos.file);
        self.document.row_lines(pos, arrange(&layout, &self.boxes))
    }

    /// Whether the logical row at `pos` is drawn as two halves in its
    /// file's `layout`.
    fn split_row(&mut self, pos: Pos, layout: &Layout) -> bool {
        match self.document.segment(pos.file, pos.part) {
            Some(Segment::Hunk(_)) => {
                self.document.arrangement(pos, arrange(layout, &self.boxes))
                    == Some(Arrangement::Split)
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
            && self.boxes.fold.peek.as_ref().is_some_and(|p| p != root)
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
    /// peek: the thread peeked at before and the one now are measured
    /// again.
    pub(crate) fn peek(&mut self, root: Option<&str>) {
        if self.boxes.fold.peek.as_deref() == root {
            return;
        }
        let before = std::mem::replace(&mut self.boxes.fold.peek, root.map(str::to_owned));
        for root in before.as_deref().into_iter().chain(root) {
            if let Some(i) = self.boxes.threads.iter().position(|t| t.root == root) {
                self.remeasure(Block::Thread(i));
            }
        }
        self.painted = None;
    }

    /// The block drawn on row `y` of the pane in the last frame.
    pub(crate) fn block_at(&mut self, y: usize) -> Option<Block> {
        self.block_row_at(y).map(|(b, _)| b)
    }

    /// The block drawn on row `y` in the last frame, and whether `y` is
    /// its first row: a thread's header, or its folded row. The blank row
    /// parting two blocks is neither's.
    pub(crate) fn block_row_at(&mut self, y: usize) -> Option<(Block, bool)> {
        let (id, line) = self.block_line_at(y)?;
        Some((self.boxes.block(id)?, line == 0))
    }

    /// The thread to fold with `z`: the focused one, else the first drawn
    /// in the last frame.
    pub(crate) fn nearest_thread(&mut self) -> Option<usize> {
        if let Some(f) = self.focused() {
            return self.boxes.threads.iter().position(|t| t.root == f.root);
        }
        (0..self.drawn.len()).find_map(|y| match self.block_at(y) {
            Some(Block::Thread(i)) => Some(i),
            _ => None,
        })
    }

    /// Whether the row at `pos` is a block's, or parts two.
    fn in_block(&mut self, pos: Pos) -> bool {
        if self.document.blocks(pos.file).is_empty() {
            return false;
        }
        let layout = self.file_layout(pos.file);
        matches!(
            self.document.row(pos, arrange(&layout, &self.boxes)),
            Some(Content::Block { .. } | Content::Parting { .. })
        )
    }

    /// The thread whose box starts after `from` (before it, without
    /// `forward`) in its file, by first row; a box `from` is in counts as
    /// before it. The text box is passed over. Past the file's last (or
    /// first), the core's next file that way holding blocks.
    fn step_from(&mut self, mut from: Pos, forward: bool) -> Stepped {
        let layout = self.file_layout(from.file);
        loop {
            let step = self
                .document
                .step_block(from, forward, arrange(&layout, &self.boxes));
            let (pos, id) = match step {
                Some(BlockStep::At { pos, id }) => (pos, id),
                Some(BlockStep::File(file)) => return Stepped::File(file),
                None => return Stepped::Nowhere,
            };
            if let Some(Block::Thread(i)) = self.boxes.block(id) {
                return Stepped::Thread(i);
            }
            from = pos;
        }
    }

    /// Focuses and shows the thread after the focused one, or before it
    /// without `forward`; with none focused, the first at or below the top
    /// row, or the last above it. Past the file's threads, the file to go
    /// on in, which this doesn't go to.
    pub(crate) fn step_thread(&mut self, forward: bool) -> Stepped {
        let top = Pos {
            offset: 0,
            ..self.document.pos
        };
        let focused = self
            .focused()
            .and_then(|f| self.boxes.ids.get(&f.root).copied());
        let from = focused.and_then(|id| {
            let layout = self.file_layout(top.file);
            let arrange = arrange(&layout, &self.boxes);
            self.document
                .block_span(top.file, id, arrange)
                .map(|(p, _)| p)
        });
        let next = match (from, forward) {
            (Some(from), _) => self.step_from(from, forward),
            (None, true) => match self.block_line_at_pos(top) {
                // A box starting on the top row is below it.
                Some((id, 0)) if matches!(self.boxes.block(id), Some(Block::Thread(_))) => self
                    .boxes
                    .of
                    .get(&id)
                    .map_or(Stepped::Nowhere, |&i| Stepped::Thread(i)),
                _ => self.step_from(top, true),
            },
            (None, false) => self.step_from(top, false),
        };
        if let Stepped::Thread(i) = next {
            self.show_thread(i);
        }
        next
    }

    /// The block whose row `pos` is, and which of its rows.
    fn block_line_at_pos(&mut self, pos: Pos) -> Option<(BlockId, usize)> {
        if self.document.blocks(pos.file).is_empty() {
            return None;
        }
        let layout = self.file_layout(pos.file);
        match self.document.row(pos, arrange(&layout, &self.boxes))? {
            Content::Block { id, line } => Some((id, line)),
            _ => None,
        }
    }

    /// Focuses and shows the first thread of the file shown, or its last.
    pub(crate) fn edge_thread(&mut self, last: bool) -> bool {
        let file = self.document.pos.file;
        if self.threads().is_empty() {
            return false;
        }
        let layout = self.file_layout(file);
        let edge = (self.document).edge_block(file, last, arrange(&layout, &self.boxes));
        let Some((pos, id)) = edge else {
            return false;
        };
        let found = match self.boxes.block(id) {
            Some(Block::Thread(i)) => Stepped::Thread(i),
            _ => self.step_from(pos, !last),
        };
        let Stepped::Thread(i) = found else {
            return false;
        };
        self.show_thread(i);
        true
    }

    /// Focuses and shows the thread with root `root` on the file shown.
    pub(crate) fn go_to_thread(&mut self, root: &str) -> bool {
        let Some(i) = self.threads().iter().position(|t| t.root == root) else {
            return false;
        };
        if self.block_rows(Block::Thread(i)).is_none() {
            return false;
        }
        self.show_thread(i);
        true
    }

    /// Focuses the thread with root `root` on the file shown, scrolling
    /// only as far as it takes to show it: after a save, the place stays.
    pub(crate) fn focus_thread(&mut self, root: &str) -> bool {
        let Some(i) = self.boxes.threads.iter().position(|t| t.root == root) else {
            return false;
        };
        self.set_focus(Some(root.to_owned()));
        self.reveal_block(Block::Thread(i), false);
        true
    }

    /// Focuses thread `i`, ending a peek at another. Unless its box shows
    /// whole already, the box's first row lands a third of the way down the
    /// pane; a box too tall for that goes as far higher as it takes to fit,
    /// though the code it hangs under stays above it.
    fn show_thread(&mut self, i: usize) {
        self.set_focus(Some(self.boxes.threads[i].root.clone()));
        let Some((first, rows)) = self.block_rows(Block::Thread(i)) else {
            return;
        };
        if self.rows_to(first).is_some_and(|y| y + rows <= self.height) {
            return;
        }
        let spare = self.height.saturating_sub(rows);
        let above = (self.height / 3).min(spare.max(self.lead(first)));
        let layout = self.file_layout(first.file);
        self.document.pos = first;
        self.scroll_in(-isize::try_from(above).unwrap_or(0), &layout);
    }

    /// Screen rows from the top of the row the block at `first` hangs
    /// under down to the block: that row's, and those of the blocks above
    /// it there.
    fn lead(&mut self, first: Pos) -> usize {
        let layout = self.file_layout(first.file);
        let mut at = first;
        let mut rows = 0;
        while self.prev_row(&mut at, &layout) {
            rows += at.offset + 1;
            if !self.in_block(at) {
                break;
            }
        }
        rows
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
        match &self.boxes.writing.as_ref()?.at {
            Writes::New { .. } | Writes::File => Some(Block::New),
            Writes::Reply(root) | Writes::Edit { root, .. } => self
                .threads()
                .iter()
                .position(|t| t.root == *root)
                .map(Block::Thread),
        }
    }

    /// The first row of `block` in the file shown, and the rows it takes.
    fn block_rows(&mut self, block: Block) -> Option<(Pos, usize)> {
        let file = self.boxes.file;
        if file != self.document.pos.file {
            return None;
        }
        let id = self.boxes.id(block)?;
        let layout = self.file_layout(file);
        (self.document).block_span(file, id, arrange(&layout, &self.boxes))
    }

    /// Whether any row of the focused thread is on screen, counted from the
    /// position rather than the last frame: keys can come between frames.
    pub(crate) fn focused_shown(&mut self) -> bool {
        let Some(i) = self
            .focused()
            .and_then(|f| self.boxes.threads.iter().position(|t| t.root == f.root))
        else {
            return false;
        };
        let Some((first, rows)) = self.block_rows(Block::Thread(i)) else {
            return false;
        };
        let top = Pos {
            offset: 0,
            ..self.document.pos
        };
        let last = Pos {
            row: first.row + rows - 1,
            ..first
        };
        last >= top && (first <= top || self.rows_to(first).is_some())
    }

    /// Scrolls just enough that `block` shows: from its first row, or with
    /// `end` (the text box, its cursor last), through its last.
    pub(crate) fn reveal_block(&mut self, block: Block, end: bool) {
        let Some((first, rows)) = self.block_rows(block) else {
            return;
        };
        let layout = self.file_layout(first.file);
        if first < self.document.pos {
            self.document.pos = if end {
                Pos {
                    row: first.row + rows.saturating_sub(self.height),
                    ..first
                }
            } else {
                first
            };
            self.clamp_in(&layout);
            return;
        }
        // Screen rows from the top of the pane through the block's last.
        let last = Pos {
            row: first.row + rows - 1,
            ..first
        };
        let mut at = self.document.pos;
        let mut down = 0;
        loop {
            down += self.height(at, &layout) - at.offset;
            if (Pos { offset: 0, ..at }) == last {
                break;
            }
            if !self.next_row(&mut at, &layout) {
                return;
            }
        }
        if down > self.height {
            let mut by = down - self.height;
            if !end {
                by = by.min(down - rows);
            }
            self.scroll_in(isize::try_from(by).unwrap_or(isize::MAX), &layout);
        }
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

/// Restyles the cells of `glyphs`, a canvas row's, that show a byte in
/// one of `matches`, each side's on that side's line as start, end and
/// style, in screen row `y` of a pane from `x` to `end`. Each side's
/// matches must be ascending and not overlapping, as a search finds them.
/// In a split row a glyph shows only its own side's matches; otherwise
/// either side's, the new side's where both cover it.
// Out of line: inlined into the frame's drawing, this loop over every glyph
// of the row competes with the caller for registers and runs longer per
// glyph.
#[inline(never)]
fn restyle_matches(
    grid: &mut Grid,
    glyphs: &[Mapped],
    matches: [&[(u32, u32, Style)]; 2],
    split: bool,
    (x, end, y): (usize, usize, usize),
) {
    debug_assert!(
        (matches.iter()).all(|m| m.windows(2).all(|w| w[0].1 <= w[1].0)),
        "a side's matches on a line overlap or are out of order"
    );
    // The match whose bytes hold `byte`: the last starting at or before it.
    let covering = |matches: &[(u32, u32, Style)], byte: usize| {
        let after = matches.partition_point(|m| m.0 as usize <= byte);
        let &(_, end, style) = matches.get(after.checked_sub(1)?)?;
        (byte < end as usize).then_some(style)
    };
    for g in glyphs {
        let byte = g.bytes.start;
        let style = if split {
            matches.get(g.side).and_then(|m| covering(m, byte))
        } else {
            covering(matches[1], byte).or_else(|| covering(matches[0], byte))
        };
        if let Some(style) = style {
            let left = (x + g.col).min(end);
            grid.restyle(left, y, (left + g.width).min(end), style);
        }
    }
}

/// Appends a row of a box in `columns` (see [`Boxes::columns`]): the rule
/// ending the old half before a new side's box, the gutter's rule when
/// `ruled`, what `body` writes, then the rule closing an old side's half.
fn rule_row(
    c: &mut Canvas,
    (start, gutter, _, old_half): (usize, usize, usize, bool),
    ruled: bool,
    body: impl FnOnce(&mut Canvas),
) {
    if start > 0 {
        c.spaces(Role::Plain, start - 1);
        c.push(Role::Dim, "│");
    }
    if ruled {
        c.spaces(Role::Plain, gutter.saturating_sub(2));
        c.push(Role::Dim, "│");
        c.spaces(Role::Plain, 2);
    }
    body(c);
    if old_half {
        c.spaces(Role::Plain, 1);
        c.push(Role::Dim, "│");
    }
    c.end_line();
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

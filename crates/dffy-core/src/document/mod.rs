//! A diff as a frontend shows it, one file at a time. A [`Document`] holds
//! every entry of the diff, the one shown and the position in it, and moves
//! between files, changes and rows. Each entry's diff is a [`File`]: its
//! parts (hunks, and the gaps of unchanged lines between them), the rows of
//! each part in the arrangement the frontend picks, the overview of its
//! changes, and search. [`File::prepare`] runs the work that builds and
//! completes a [`File`], at the granularity a host draws it: a whole file
//! for print, a hunk or a gap at a time for a viewer, as
//! [`Document::missing`] names it.
//!
//! Nothing here measures text, draws it, reads files or spawns threads. A
//! position is anchored to a row of a part, never a screen row, so a host's
//! screen logic (wrapping, scrolling, clamping to its height) sits on top
//! and costs only the rows it shows. Rows, lines and headers come out
//! borrowed from the file; queries take the part or row asked for and never
//! walk a whole file.
//!
//! A gap shows as a separator row (`⋯ 40 unchanged lines`) with whatever
//! the user revealed of it above and below. Lines the host reveals inside
//! a gap split it, with a separator for each stretch still hidden.
//! Revealing more, or switching to the full text, changes only the rows of
//! its gaps, so the hunks' rows and every position in them stay put. The
//! syntax diff's hunks cover the same lines as the line diff's, so a
//! position also survives that toggle.
//!
//! A host may insert blocks of rows among a file's (see [`Block`]): each
//! part's rows are its own (code, separators, headers) with the blocks
//! hung in it placed between them. Positions, moving and search count
//! block rows like any other.
//!
//! This is the top layer of the core: it uses [`crate::git`],
//! [`crate::highlight`], [`crate::structural`] and [`crate::cache`], and none
//! of them use it.

mod blocks;
mod conflicted;
mod entries;
mod prepare;
mod rendered;
mod search;

use std::ops::Range;
use std::sync::Arc;

use gix::bstr::{BString, ByteSlice};

pub use blocks::{Arrange, Attach, Block, BlockId};
pub use conflicted::Conflicted;
pub use entries::{
    BlockStep, Document, Entry, Found, Missing, Piped, Render, Settle, Source, State,
};
pub use prepare::{Prepare, SyntaxSlot, pair};
pub use rendered::{Origin, Rendered};
pub use search::{BlockMatch, Hit, Match, Matches, Needle, Reading, find, find_blocks, word};

use blocks::{At, Blocks, Key, Placed, order};

use crate::diff::{self, Emphasis, Hunk, LineKind, Whitespace};
use crate::git::{FileChange, FileDiff, Mode, ObjectId, Side as GitSide, Status};
use crate::highlight::{self, HunkSyntax, MAX_HIGHLIGHT_BYTES, Registry};
use crate::patch::{self, FileInfo};
use crate::structural::{Structural, Unavailable};
pub use crate::view::Side;
use crate::view::{
    Arrangement, FileHeader, FileStatus, Heading, HunkHeader, Kind, Line, LineState, Row, Spans,
    display_text, heading_text, line_bytes, no_newline, rows, side_rows,
};

/// Lines one reveal adds to a gap; a gap of up to twice this opens whole.
const REVEAL_LINES: usize = 20;

/// A file's added and removed lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Counts {
    /// Lines only on the new side.
    pub added: usize,
    /// Lines only on the old side.
    pub removed: usize,
}

/// A place in a [`Document`]: entry `file`, and a row of it. Part `n` is the
/// file's segment `n - 1` (see [`File::segment`]); part 0 holds the rows of
/// an entry shown as text ([`Entry::is_sheet`]), or the single blank row of
/// a file with no segments (binary, unchanged, too large, not diffed yet),
/// and a file with segments never takes it. Positions order as the document
/// shows them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Pos {
    /// The entry.
    pub file: usize,
    /// The part, 0 or a segment's index plus one.
    pub part: usize,
    /// The row within the part.
    pub row: usize,
    /// A sub-row inside the row, for a host that draws a row on several
    /// screen rows (a wrapped line, a soft wrap). Navigation that moves to
    /// another row sets it to 0; it's otherwise kept as the host set it, and
    /// only takes part in ordering positions.
    pub offset: usize,
}

/// A stretch of unchanged lines: where it starts on each side (0-based)
/// and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    /// First old line.
    pub old: usize,
    /// First new line.
    pub new: usize,
    /// Lines in it, the same on both sides.
    pub len: usize,
}

/// A part of a file after its header, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Segment {
    /// The first hunk's `@@` row, when no gap comes before it.
    Start,
    /// The gap before hunk `n`, or after the last hunk.
    Gap(usize),
    /// Hunk `n`.
    Hunk(usize),
}

/// How much of a gap shows: lines from its top edge, lines from its bottom
/// edge, windows of lines between, and a separator row for each stretch
/// still hidden around them. Its rows are the gap's own: blocks hung in the
/// gap add theirs between them (see [`Block`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shown<'a> {
    /// Lines shown from the gap's top.
    pub top: usize,
    /// Lines shown from the gap's bottom.
    pub bottom: usize,
    /// Lines in the gap.
    pub len: usize,
    /// Lines shown between the edges, as gap lines: in order, each with
    /// hidden lines on both sides.
    pub windows: &'a [Range<usize>],
}

impl Shown<'_> {
    /// Lines under the separators.
    #[inline]
    #[must_use]
    pub fn hidden(self) -> usize {
        let windows: usize = self.windows.iter().map(ExactSizeIterator::len).sum();
        self.len - self.top - self.bottom - windows
    }

    /// Each stretch a separator hides, paired with the lines shown below
    /// it: the next window, or the bottom edge's lines after the last
    /// stretch. Only the last stretch may be empty, when the gap hides
    /// nothing.
    fn stretches(self) -> impl Iterator<Item = (Range<usize>, Range<usize>)> {
        let end = self.len - self.bottom;
        let ends = self.windows.iter().map(|w| w.start).chain([end]);
        let starts = std::iter::once(self.top).chain(self.windows.iter().map(|w| w.end));
        let shown = self
            .windows
            .iter()
            .cloned()
            .chain(std::iter::once(end..self.len));
        starts.zip(ends).map(|(s, e)| s..e).zip(shown)
    }

    /// Rows the gap takes.
    #[inline]
    #[must_use]
    pub fn rows(self) -> usize {
        let separators = self.windows.len() + usize::from(self.hidden() > 0);
        self.len - self.hidden() + separators
    }

    /// The gap line row `row` shows; `None` for a separator.
    #[inline]
    #[must_use]
    pub fn line(self, row: usize) -> Option<usize> {
        if row < self.top {
            return Some(row);
        }
        let mut row = row - self.top;
        for (hidden, shown) in self.stretches() {
            if !hidden.is_empty() {
                row = row.checked_sub(1)?;
            }
            if row < shown.len() {
                return Some(shown.start + row);
            }
            row -= shown.len();
        }
        Some(self.len + row)
    }

    /// The gap lines the separator at `row` hides; `None` when the row
    /// shows a line.
    #[must_use]
    pub fn hides(self, row: usize) -> Option<Range<usize>> {
        let mut at = self.top;
        for (hidden, shown) in self.stretches() {
            if !hidden.is_empty() {
                if at == row {
                    return Some(hidden);
                }
                at += 1;
            }
            at += shown.len();
            if at > row {
                return None;
            }
        }
        None
    }

    /// The gap line row `row` shows, or the first its separator hides.
    fn line_at(self, row: usize) -> usize {
        self.line(row)
            .or_else(|| self.hides(row).map(|h| h.start))
            .unwrap_or(self.top)
    }

    /// The stretches of gap lines the separators hide, in order.
    pub fn hidden_lines(self) -> impl Iterator<Item = Range<usize>> {
        self.stretches()
            .map(|(hidden, _)| hidden)
            .filter(|h| !h.is_empty())
    }

    /// The row showing gap line `line`, or its separator when it's hidden.
    #[inline]
    #[must_use]
    pub fn row(self, line: usize) -> usize {
        if line < self.top {
            return line;
        }
        let mut row = self.top;
        for (hidden, shown) in self.stretches() {
            if hidden.contains(&line) {
                return row;
            }
            row += usize::from(!hidden.is_empty());
            if shown.contains(&line) {
                return row + line - shown.start;
            }
            row += shown.len();
        }
        row
    }
}

/// What's revealed of a gap: lines from each edge, and windows of lines
/// between. Kept settled for the gap's length: windows sorted, none
/// touching another window or an edge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Reveal {
    top: usize,
    bottom: usize,
    windows: Vec<Range<usize>>,
}

impl Reveal {
    /// Clamps to a gap of `len` lines and joins windows that touch each
    /// other or an edge.
    fn settle(&mut self, len: usize) {
        self.top = self.top.min(len);
        let mut windows = std::mem::take(&mut self.windows);
        windows.sort_unstable_by_key(|w| w.start);
        for w in windows {
            let w = w.start..w.end.min(len);
            match self.windows.last_mut() {
                _ if w.start <= self.top => self.top = self.top.max(w.end),
                Some(last) if w.start <= last.end => last.end = last.end.max(w.end),
                _ if w.is_empty() => {}
                _ => self.windows.push(w),
            }
        }
        self.bottom = self.bottom.min(len - self.top);
        while let Some(last) = self.windows.last()
            && last.end >= len - self.bottom
        {
            self.bottom = self.bottom.max(len - last.start);
            self.windows.pop();
        }
    }
}

/// What a hunk's rows are built for: an arrangement of both sides, one
/// side read alone, or a hunk opened in place while a side is read alone:
/// its header row, which closes it again, then its unified rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layout {
    Arranged(Arrangement),
    Read(Side),
    Opened,
}

/// Line `line`'s number (0-based) on `side`: where it would be there, for
/// a line only on the other side.
fn line_on(line: &diff::Line, side: Side) -> usize {
    match side {
        Side::Old => line.old,
        Side::New => line.new,
    }
}

/// How a file is shown, which decides its parts' rows: the syntax diff or
/// the line diff, how much of each gap is revealed, each hunk's
/// arrangement and the blocks among the rows. Queries that never build a
/// hunk's rows take any `A`.
struct Shape<'a, A> {
    /// Show the syntax diff where the file has one.
    structural: bool,
    /// Show every gap whole; a piped file's gaps still never open. Always
    /// set while a side is read alone.
    full: bool,
    /// The side read alone, if any: each hunk shows only its lines, with
    /// the other side's marked where they're hidden (see [`side_rows`]),
    /// but for the hunks opened in place, in order (see
    /// [`Document::set_opened`]).
    // One field: a shape is built for every row drawn or moved over, and
    // while both sides show this is a single word to set.
    reading: Option<(Side, &'a [usize])>,
    /// What's revealed of each gap, by gap; gaps past its end show nothing.
    reveal: &'a [Reveal],
    /// Each hunk's arrangement, from its numbers (and no heading), and the
    /// blocks' heights.
    arrange: &'a A,
    /// The entry's blocks, when it has any.
    blocks: Option<&'a Blocks>,
}

/// What a row shows, to find it again once the rows around it change.
#[derive(Debug, Clone, Copy)]
enum Anchor {
    /// A part's own row, which stays where it is: a hunk's, a header.
    Own(usize),
    /// A gap's line; a separator counts as the first line it hides.
    Line(usize),
    /// Row `line` of a block, hung under `after` of its part's own rows.
    Block {
        id: BlockId,
        line: usize,
        after: usize,
    },
}

/// The entry's blocks as one [`File`] places them.
struct FileBlocks {
    /// The [`Blocks::generation`] they're of.
    generation: u64,
    /// The blocks on lines, as indices into the list, by side: by the line
    /// of this file (a rendered view's line drawn from theirs), then index.
    lines: [Vec<(usize, u32)>; 2],
    /// By part, once placed.
    parts: Vec<Placed>,
}

/// The blocks of `lines` on lines in `range`.
fn lines_in(lines: &[(usize, u32)], range: Range<usize>) -> &[(usize, u32)] {
    let from = lines.partition_point(|&(l, _)| l < range.start);
    let to = lines.partition_point(|&(l, _)| l < range.end);
    &lines[from..to.max(from)]
}

/// A changed line in the overview, most visible last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    /// Moved code (see [`LineState::RemovedMoved`]).
    Moved,
    /// A line paired with its edit, when modified lines show as such.
    Modified,
    /// A removed line.
    Removed,
    /// An added line.
    Added,
}

/// The overview's marks, for the syntax diff or not and with modified
/// lines or not.
type OverviewMarks = (bool, bool, Vec<(usize, Mark)>);

/// A file diff prepared for display. [`Prepare::file`] builds it with every
/// hunk paired; emphasis, gap highlighting and the syntax diff arrive later
/// through its setters, for the parts a host draws.
pub struct File {
    diff: Arc<FileDiff>,
    /// `None` when highlighting is off or neither side has a known
    /// language.
    syntax: Option<SyntaxSlot>,
    hunks: Vec<HunkView>,
    structural: Option<SyntaxDiff>,
    /// The unchanged lines before each hunk, then those after the last.
    /// The syntax diff's hunks cover the same lines, so these serve both.
    gaps: Vec<Gap>,
    segments: Vec<Segment>,
    /// Each side's syntax runs over each gap, once they're set. The old
    /// side's are only wanted while it's read alone, so its list stays
    /// empty until then.
    gap_syntax: [Vec<Option<Spans>>; 2],
    /// Made on first use, for the syntax diff or not.
    marks: Option<OverviewMarks>,
    counts: Counts,
    /// The hunks with a removed line, then those with an added one: the
    /// changes on each side.
    changes: [usize; 2],
    note: Option<&'static str>,
    bytes: usize,
    /// The estimate of the highlighting of a file without hunks, which is
    /// prepared only once its lines show: it goes into `bytes` when the
    /// first gap's runs arrive, so a range of renames doesn't count it for
    /// files never looked at.
    unparsed: usize,
    /// Each hunk's heading as a piped diff wrote it. Set for a piped file,
    /// which has only its hunks: the text between them is unknown, so its
    /// gaps never open.
    headings: Option<Vec<Vec<u8>>>,
    /// One hunk holding a whole added or deleted file, whose `@@` row
    /// would say nothing a title doesn't.
    whole: bool,
    rendered: Option<Box<Rendered>>,
    /// A conflicted file's conflicts, for a diff whose new side is its
    /// working-tree file.
    conflicted: Option<Box<Conflicted>>,
    /// Set once a part of an entry with blocks is placed.
    blocks: Option<Box<FileBlocks>>,
}

/// A file's syntax diff as displayed. Its hunks cover the same lines as
/// the line diff's, so a position stays valid across the toggle.
struct SyntaxDiff {
    /// Empty when the file has no syntax diff; the line diff shows then.
    /// Shared with searches of the file read alone (see
    /// [`Document::reading_in`]).
    hunks: Arc<[Hunk]>,
    views: Vec<HunkView>,
    note: Option<String>,
}

struct HunkView {
    /// The old line its heading is on, a `u32` like the body's lines: every
    /// hunk of every file held has a view, so its size counts.
    heading: Option<u32>,
    body: Vec<BodyLine>,
    /// Rows in `layout`, computed when the hunk is first drawn in it.
    rows: Vec<Row>,
    /// The rows that start a block of changed lines, and every row marking
    /// hidden lines.
    starts: Vec<usize>,
    /// What `rows` and `starts` are for; `None` until the hunk is drawn.
    layout: Option<Layout>,
    /// The layout drawn before the one shown, with its rows and starts,
    /// once the hunk was drawn in two (a resize across the split width, a
    /// layout toggle, a side read alone).
    other: Option<Box<Held>>,
    /// Per line of the core hunk, once set.
    emphasis: Option<Vec<Emphasis>>,
    /// Set along with the emphasis, for highlighted files.
    syntax: Option<HunkSyntax>,
}

/// A layout a hunk was drawn in, with its rows and starts.
type Held = (Layout, Vec<Row>, Vec<usize>);

// Every hunk of every file held has a view: it stays within its budget.
const _: () = assert!(size_of::<HunkView>() <= 280);

/// A displayed line of a hunk: a line of the core hunk, or the
/// no-newline marker after it, with what it is.
#[derive(Debug, Clone, Copy)]
struct BodyLine {
    line: u32,
    state: LineState,
}

// A hunk's body has a line each: it stays as small as its index.
const _: () = assert!(size_of::<BodyLine>() == 8);

const NO_EMPHASIS: &[Range<usize>] = &[];

/// The no-newline marker row's line.
pub const NO_NEWLINE_LINE: Line<'static> = Line {
    state: LineState::NoNewline,
    old: None,
    new: None,
    text: b"",
    emphasis: &[],
    syntax: None,
    paint: &[],
    block_start: false,
};

/// The views of `hunks` (of `diff`), without emphasis. Their lines are
/// paired already, so each line's state is final.
fn views(diff: &FileDiff, hunks: &[Hunk]) -> Vec<HunkView> {
    let mut heading = Heading::default();
    hunks
        .iter()
        .map(|hunk| {
            let mut body = Vec::with_capacity(hunk.lines.len());
            for (i, line) in hunk.lines.iter().enumerate() {
                let line_index = u32::try_from(i).unwrap_or(u32::MAX);
                body.push(BodyLine {
                    line: line_index,
                    state: LineState::of(line),
                });
                if no_newline(&diff.old, &diff.new, line) {
                    body.push(BodyLine {
                        line: line_index,
                        state: LineState::NoNewline,
                    });
                }
            }
            HunkView {
                heading: heading
                    .find_line(&diff.old, hunk.old_start)
                    .and_then(|i| u32::try_from(i).ok()),
                body,
                rows: Vec::new(),
                starts: Vec::new(),
                layout: None,
                other: None,
                emphasis: None,
                syntax: None,
            }
        })
        .collect()
}

/// The row of `view`, a view of `core` with its rows built, showing line
/// `line` (0-based) of `side`.
fn line_row(core: &Hunk, view: &HunkView, side: Side, line: usize) -> Option<usize> {
    // Along the body, a line's number on `side`, or its insertion point
    // there when it's only on the other side, never goes down; at one
    // number, the lines only on the other side come first.
    let key = |b: &BodyLine| {
        let l = &core.lines[b.line as usize];
        match side {
            Side::Old => (l.old, l.kind != LineKind::Added),
            Side::New => (l.new, l.kind != LineKind::Removed),
        }
    };
    let body = view.body.partition_point(|b| key(b) < (line, true));
    view.body
        .get(body)
        .filter(|b| b.state != LineState::NoNewline && key(b) == (line, true))?;
    match view.layout {
        Some(Layout::Read(_)) => read_row(&view.rows, body),
        _ => row_of(&view.rows, body),
    }
}

/// The row of `rows`, built by [`side_rows`], showing body line `i`, the
/// row of the line it pairs with when it's hidden, or the row standing for
/// it unpaired. `None` for a hidden no-newline marker.
// Linear in the hunk's rows: a block's leftover hidden lines come after its
// lines read, out of body order, so the rows can't be searched by line.
fn read_row(rows: &[Row], i: usize) -> Option<usize> {
    rows.iter().position(|row| match *row {
        Row::One { line, paired } => line == i || paired == Some(i),
        Row::Hidden { from, n } => (from..from + n as usize).contains(&i),
        _ => false,
    })
}

/// The row of `rows`, built by [`rows`], showing body line `i`, which isn't
/// a no-newline marker.
///
/// Rows follow the body: each row's first line is past the one before's,
/// and a marker, which names no line, sits where its line would. A line
/// is its row's first, except an added line paired with a removed one. Its
/// block's rows pair the removed lines with the added ones in order, so it
/// is as many rows above the last pair as its line is before that pair's
/// added line.
fn row_of(rows: &[Row], i: usize) -> Option<usize> {
    let first = |mut r: usize| loop {
        match rows[r] {
            Row::Line(l)
            | Row::Pair(Some(l), _)
            | Row::Pair(None, Some(l))
            | Row::One { line: l, .. } => {
                return Some(l);
            }
            Row::Header | Row::Marker | Row::Pair(None, None) | Row::Hidden { .. } => {
                r = r.checked_sub(1)?;
            }
        }
    };
    let shows = |r: usize| match rows[r] {
        Row::Line(l) | Row::One { line: l, .. } => l == i,
        Row::Pair(old, new) => old == Some(i) || new == Some(i),
        Row::Header | Row::Marker | Row::Hidden { .. } => false,
    };
    // The rows whose first line is at or before `i`, a marker counting as
    // the row above it.
    let (mut lo, mut hi) = (0, rows.len());
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if first(mid).is_none_or(|l| l <= i) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut r = lo.checked_sub(1)?;
    while rows[r] == Row::Marker {
        r = r.checked_sub(1)?;
    }
    if shows(r) {
        return Some(r);
    }
    // `r` is the last row of `i`'s block with a removed line. Steps back
    // over the removed lines left unpaired one at a time: linear in those
    // alone.
    while let Row::Pair(Some(_), None) = rows[r] {
        r = r.checked_sub(1)?;
    }
    let Row::Pair(Some(_), Some(added)) = rows[r] else {
        return None;
    };
    let row = r.checked_sub(added.checked_sub(i)?)?;
    shows(row).then_some(row)
}

/// The gaps around `diff`'s hunks. Without hunks, its lines are one gap
/// when both sides have them line for line, as a file renamed or with its
/// mode changed does; none otherwise.
fn gaps(diff: &FileDiff) -> Vec<Gap> {
    if diff.hunks.is_empty() {
        let len = diff.new.len();
        return (len > 0 && diff.old.len() == len)
            .then_some(Gap {
                old: 0,
                new: 0,
                len,
            })
            .into_iter()
            .collect();
    }
    let (mut old, mut new) = (0, 0);
    let mut gaps = Vec::with_capacity(diff.hunks.len() + 1);
    for h in &diff.hunks {
        gaps.push(Gap {
            old,
            new,
            len: h.old_start.saturating_sub(old),
        });
        old = h.old_start + h.old_len;
        new = h.new_start + h.new_len;
    }
    gaps.push(Gap {
        old,
        new,
        len: diff.old.len().saturating_sub(old),
    });
    gaps
}

impl File {
    /// Pairs every hunk of `diff` and builds its views. `highlight` has the
    /// grammars to highlight with; `None` leaves the file plain.
    fn new(diff: FileDiff, whitespace: Whitespace, highlight: Option<&Registry>) -> Self {
        let gaps = gaps(&diff);
        // A file without hunks shows its lines only when asked to.
        let deferred = diff.hunks.is_empty();
        Self::with_gaps(diff, gaps, whitespace, (highlight, deferred))
    }

    /// `diff`, a file compared with itself (see [`FileChange::unchanged`]),
    /// as one gap of every line, as any file without hunks has. An empty,
    /// binary or too large one has none, and a note that says why.
    fn unchanged(diff: FileDiff, highlight: Option<&Registry>) -> Self {
        let gaps = gaps(&diff);
        let why = if diff.binary {
            Some("binary file, not shown")
        } else if diff.too_large {
            Some("file too large to show")
        } else {
            None
        };
        let mut file = Self::with_gaps(diff, gaps, Whitespace::Exact, (highlight, false));
        file.note = why.or(file.note);
        file
    }

    /// With `deferred`, the highlighting's estimate waits for the first
    /// gap's runs (see [`File::unparsed`]).
    fn with_gaps(
        mut diff: FileDiff,
        gaps: Vec<Gap>,
        whitespace: Whitespace,
        (highlight, deferred): (Option<&Registry>, bool),
    ) -> Self {
        // Whether a line is modified decides its color from the first frame
        // on, and the overview marks every hunk, shown or not.
        for hunk in &mut diff.hunks {
            diff::pair(&diff.old, &diff.new, hunk);
        }
        let hunks = views(&diff, &diff.hunks);
        let lines: usize = hunks.iter().map(|h| h.body.len()).sum();
        let mut counts = Counts::default();
        let mut changes = [0; 2];
        for hunk in &diff.hunks {
            let before = counts;
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Added => counts.added += 1,
                    LineKind::Removed => counts.removed += 1,
                    LineKind::Context => {}
                }
            }
            changes[0] += usize::from(counts.removed > before.removed);
            changes[1] += usize::from(counts.added > before.added);
        }
        let note = note(&diff)
            .or_else(|| whitespace_only(&diff, whitespace).then_some("only whitespace changed"));
        let highlighted = |grammars: &Registry, path: &[u8], bytes: &[u8]| {
            let head = &bytes[..bytes.len().min(256)];
            !bytes.is_empty()
                && bytes.len() <= MAX_HIGHLIGHT_BYTES
                && grammars.detect(path, head).is_some()
        };
        let sides = [
            (diff.change.old_path().as_bytes(), diff.old.bytes()),
            (diff.change.path.as_bytes(), diff.new.bytes()),
        ];
        let syntax_bytes: usize = sides
            .iter()
            .filter(|(path, bytes)| {
                highlight
                    .is_some_and(|grammars| !gaps.is_empty() && highlighted(grammars, path, bytes))
            })
            // Measured on kernel/sched/fair.c: a first parse grew the process
            // by about 19 bytes per byte of source, an upper bound.
            .map(|(_, bytes)| bytes.len() * 20)
            .sum();
        let whole = hunks.len() == 1
            && gaps.iter().all(|g| g.len == 0)
            && (diff.old.is_empty() || diff.new.is_empty());
        let mut segments = Vec::with_capacity(gaps.len() + hunks.len());
        for (i, gap) in gaps.iter().enumerate() {
            if gap.len > 0 {
                segments.push(Segment::Gap(i));
            } else if i == 0 && !whole {
                segments.push(Segment::Start);
            }
            if i < hunks.len() {
                segments.push(Segment::Hunk(i));
            }
        }
        let unparsed = if deferred { syntax_bytes } else { 0 };
        Self {
            bytes: diff.old.bytes().len() + diff.new.bytes().len() + lines * 64 + syntax_bytes
                - unparsed,
            unparsed,
            syntax: (syntax_bytes > 0).then(SyntaxSlot::default),
            diff: Arc::new(diff),
            hunks,
            structural: None,
            gap_syntax: [Vec::new(), vec![None; gaps.len()]],
            marks: None,
            gaps,
            segments,
            counts,
            changes,
            note,
            headings: None,
            whole,
            rendered: None,
            conflicted: None,
            blocks: None,
        }
    }

    /// A piped file: `change`, with the `sides` its patch shows (see
    /// [`patch::sides`]) and whether the patch called it binary. It has
    /// only its hunks, so its gaps never open and its hunk headings are the
    /// patch's. Not highlighted: hunks alone parse poorly.
    #[must_use]
    pub fn piped(change: FileChange, sides: patch::Sides, binary: bool) -> Self {
        let diff = FileDiff {
            change,
            old: sides.old,
            new: sides.new,
            binary,
            too_large: false,
            note: None,
            hunks: sides.hunks,
        };
        let headings = sides.headings;
        let mut file = Self::new(diff, Whitespace::Exact, None);
        file.bytes += headings.iter().map(|h| h.len() + 24).sum::<usize>();
        file.headings = Some(headings);
        file
    }

    /// The diff shown.
    #[inline]
    #[must_use]
    pub fn diff(&self) -> &Arc<FileDiff> {
        &self.diff
    }

    /// Where this file's highlighting is prepared; `None` when it isn't
    /// highlighted.
    #[inline]
    #[must_use]
    pub fn syntax(&self) -> Option<&SyntaxSlot> {
        self.syntax.as_ref()
    }

    /// The labels of the definitions around `line` (0-based) of `side`,
    /// outermost first, such as `["impl Error", "fn fatal"]`; empty at
    /// the top level. `None` until the file's highlighting is prepared
    /// (see [`Prepare::emphasis`]) and its definitions found after it
    /// ([`Prepare::scopes`]), and when that side has no definitions to
    /// look in: its language has no scope query, it isn't highlighted, or
    /// the file shows rendered. Never waits or parses.
    #[must_use]
    pub fn scopes(&self, side: Side, line: usize) -> Option<Vec<&str>> {
        if self.rendered.is_some() {
            return None;
        }
        let syntax = self.syntax.as_ref()?.get()?;
        Some(syntax.scopes()?[side.index()]?.at(line))
    }

    /// Whether the highlighting is prepared and the definitions it leaves
    /// to find aren't yet (see [`Prepare::scopes`]).
    #[must_use]
    pub(crate) fn wants_scopes(&self) -> bool {
        let syntax = self.syntax.as_ref().and_then(|slot| slot.get());
        self.rendered.is_none() && syntax.is_some_and(|s| s.scopes().is_none())
    }

    /// Its added and removed lines. A rendered view counts the source
    /// lines it was drawn from.
    #[inline]
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.counts
    }

    /// The changes on `side`: the hunks with a line only there.
    #[inline]
    #[must_use]
    pub fn changes(&self, side: Side) -> usize {
        self.changes[side.index()]
    }

    /// Approximate memory held, for a host's cache cap. Grows when the
    /// syntax diff is set or a hunk is drawn in a second arrangement.
    #[inline]
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Why there are no hunks to show (binary, too large, only whitespace
    /// changed), or that a rendered view renders the same.
    #[inline]
    #[must_use]
    pub fn note(&self) -> Option<&'static str> {
        self.note
    }

    /// Whether this is a piped file, which has only its hunks.
    #[inline]
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.headings.is_some()
    }

    /// The rendered view's mapping to its source, for a rendered view.
    #[inline]
    #[must_use]
    pub fn rendered(&self) -> Option<&Rendered> {
        self.rendered.as_deref()
    }

    /// The gaps, one before each hunk and one after the last. A file
    /// without hunks has one of every line when its sides have them line
    /// for line, and none otherwise.
    #[inline]
    #[must_use]
    pub fn gaps(&self) -> &[Gap] {
        &self.gaps
    }

    /// The lines of `side` in gap `gap`, for highlighting.
    #[must_use]
    pub fn gap_lines(&self, gap: usize, side: Side) -> Range<usize> {
        self.gaps.get(gap).map_or(0..0, |g| {
            let start = match side {
                Side::Old => g.old,
                Side::New => g.new,
            };
            start..start + g.len
        })
    }

    /// Whether hunk `hunk` of the line diff has no emphasis yet.
    #[inline]
    #[must_use]
    pub fn wants_emphasis(&self, hunk: usize) -> bool {
        self.hunks.get(hunk).is_some_and(|h| h.emphasis.is_none())
    }

    /// Whether gap `gap` is highlighted and has no syntax runs of `side`
    /// yet.
    #[inline]
    #[must_use]
    pub fn wants_gap_syntax(&self, gap: usize, side: Side) -> bool {
        self.syntax.is_some()
            && gap < self.gaps.len()
            && (self.gap_syntax[side.index()].get(gap)).is_none_or(Option::is_none)
    }

    /// Whether the syntax diff is worth computing: not yet set, and the
    /// file has hunks of its own text.
    #[must_use]
    pub fn wants_structural(&self) -> bool {
        self.structural.is_none()
            && !self.diff.hunks.is_empty()
            && !self.is_partial()
            && self.rendered.is_none()
            && !self.is_resolution()
    }

    /// What to say about the syntax diff of this file, if anything.
    #[must_use]
    pub fn structural_note(&self) -> Option<&str> {
        if self.is_partial() {
            return Some("no syntax diff: a piped diff has only its hunks");
        }
        if self.rendered.is_some() {
            return None;
        }
        match &self.structural {
            None if self.wants_structural() => Some("syntax diff…"),
            None => None,
            Some(s) => s.note.as_deref(),
        }
    }

    /// Stores hunk `hunk`'s emphasis and syntax spans (see
    /// [`Prepare::emphasis`]) if `diff` is this file's diff.
    pub fn set_emphasis(
        &mut self,
        diff: &Arc<FileDiff>,
        hunk: usize,
        emphasis: Vec<Emphasis>,
        syntax: Option<HunkSyntax>,
    ) {
        if !Arc::ptr_eq(&self.diff, diff) {
            return;
        }
        if let Some(view) = self.structural.as_mut().and_then(|s| s.views.get_mut(hunk)) {
            view.syntax.clone_from(&syntax);
        }
        if let Some(h) = self.hunks.get_mut(hunk) {
            h.emphasis = Some(emphasis);
            h.syntax = syntax;
        }
    }

    /// Stores the syntax runs of gap `gap`'s lines of `side` (see
    /// [`Prepare::gap_syntax`]), empty when there are none, if `diff` is
    /// this file's diff and the gap has none of that side yet.
    pub fn set_gap_syntax(
        &mut self,
        diff: &Arc<FileDiff>,
        gap: usize,
        side: Side,
        spans: Option<Spans>,
    ) {
        if !Arc::ptr_eq(&self.diff, diff) || gap >= self.gaps.len() {
            return;
        }
        let slots = &mut self.gap_syntax[side.index()];
        if slots.len() < self.gaps.len() {
            slots.resize(self.gaps.len(), None);
        }
        if let Some(slot @ None) = slots.get_mut(gap) {
            *slot = Some(spans.unwrap_or_default());
            self.bytes += std::mem::take(&mut self.unparsed);
        }
    }

    /// Stores the syntax diff of `diff` (see [`Prepare::structural`]) if
    /// that's this file's diff and it has none yet. Returns whether it was
    /// stored.
    pub fn set_structural(
        &mut self,
        diff: &Arc<FileDiff>,
        result: Option<Result<Structural, Unavailable>>,
    ) -> bool {
        if !Arc::ptr_eq(&self.diff, diff) || self.structural.is_some() {
            return false;
        }
        let note = result.as_ref().and_then(structural_note);
        let mut hunks = match result {
            Some(Ok(s)) => s.hunks,
            _ => Vec::new(),
        };
        let mut views = views(&self.diff, &hunks);
        for ((view, hunk), line_view) in views.iter_mut().zip(&mut hunks).zip(&self.hunks) {
            view.emphasis = Some(
                hunk.lines
                    .iter_mut()
                    .map(|l| std::mem::take(&mut l.emphasis))
                    .collect(),
            );
            view.syntax.clone_from(&line_view.syntax);
        }
        self.bytes += views.iter().map(|v| v.body.len() * 96).sum::<usize>();
        self.structural = Some(SyntaxDiff {
            hunks: hunks.into(),
            views,
            note,
        });
        true
    }

    /// The syntax diff's hunks when they show for `structural`, rather than
    /// the line diff's.
    #[must_use]
    pub(crate) fn shown_structural(&self, structural: bool) -> Option<&Arc<[Hunk]>> {
        let s = self.structural.as_ref()?;
        (self.uses_structural(structural)).then_some(&s.hunks)
    }

    /// Hunk `h`'s heading: the patch's, for a piped file, else found above
    /// it.
    #[inline]
    fn heading(&self, views: &[HunkView], h: usize) -> &[u8] {
        match &self.headings {
            Some(headings) => headings.get(h).map_or(&b""[..], |s| &s[..]),
            None => views[h]
                .heading
                .map_or(&b""[..], |i| heading_text(&self.diff.old, i as usize)),
        }
    }

    /// `core`'s numbers: of the source lines its blocks were drawn from,
    /// for a rendered view.
    #[inline]
    fn header<'a>(&self, core: &Hunk, heading: &'a [u8]) -> HunkHeader<'a> {
        match &self.rendered {
            Some(rendered) => rendered.header(core, heading),
            None => hunk_header(core, heading),
        }
    }

    /// How many lines the old side's `lines` count as: the source lines
    /// they stand for, for a rendered view.
    fn old_count(&self, lines: Range<usize>) -> usize {
        match &self.rendered {
            Some(rendered) => rendered.old_count(lines),
            None => lines.len(),
        }
    }

    /// The hunks to show and their views: the syntax diff's when it's
    /// asked for and there is one.
    #[inline]
    fn shown_hunks(&self, structural: bool) -> (&[Hunk], &[HunkView]) {
        match &self.structural {
            Some(s) if structural && s.hunks.len() == self.hunks.len() => (&*s.hunks, &s.views),
            _ => (&self.diff.hunks, &self.hunks),
        }
    }

    /// [`File::shown_hunks`], its views to change, with the diff.
    fn shown_hunks_mut(&mut self, structural: bool) -> (&FileDiff, &[Hunk], &mut [HunkView]) {
        let diff = &*self.diff;
        match &mut self.structural {
            Some(s) if structural && s.hunks.len() == self.hunks.len() => {
                (diff, &*s.hunks, &mut s.views)
            }
            _ => (diff, &diff.hunks, &mut self.hunks),
        }
    }

    /// Lines the overview scales to: the new side's, or the old side's for
    /// a deleted file; at least 1.
    #[must_use]
    pub fn overview_lines(&self) -> usize {
        if self.diff.new.is_empty() {
            self.diff.old.len()
        } else {
            self.diff.new.len()
        }
        .max(1)
    }

    /// The changed lines of the shown hunks by line (see
    /// [`File::overview_lines`]), in order, computed once per diff and
    /// setting; paired lines are marked modified when `modified` lines show
    /// as such.
    pub fn marks(&mut self, structural: bool, modified: bool) -> &[(usize, Mark)] {
        if self
            .marks
            .as_ref()
            .is_none_or(|m| (m.0, m.1) != (structural, modified))
        {
            let deleted = self.diff.new.is_empty();
            let (cores, views) = self.shown_hunks(structural);
            let mut marks: Vec<(usize, Mark)> = cores
                .iter()
                .zip(views)
                .flat_map(|(core, view)| view.body.iter().map(move |b| (core, b)))
                .filter_map(|(core, b)| {
                    let mark = match b.state {
                        LineState::Context
                        | LineState::NoNewline
                        | LineState::RemovedReformatted
                        | LineState::AddedReformatted => return None,
                        LineState::RemovedMoved | LineState::AddedMoved => Mark::Moved,
                        LineState::RemovedPaired | LineState::AddedPaired if modified => {
                            Mark::Modified
                        }
                        LineState::Removed | LineState::RemovedPaired => Mark::Removed,
                        LineState::Added | LineState::AddedPaired => Mark::Added,
                    };
                    let l = &core.lines[b.line as usize];
                    Some((if deleted { l.old } else { l.new }, mark))
                })
                .collect();
            marks.sort_unstable();
            self.marks = Some((structural, modified, marks));
        }
        self.marks.as_ref().map_or(&[], |m| &m.2)
    }

    /// Parts, part 0 included.
    #[inline]
    #[must_use]
    pub fn parts(&self) -> usize {
        1 + self.segments.len()
    }

    /// The first part a position may be on: 1, or 0 for a file without
    /// segments.
    #[inline]
    #[must_use]
    pub fn first_part(&self) -> usize {
        usize::from(!self.segments.is_empty())
    }

    /// What part `part` is; `None` for part 0 and past the last.
    #[inline]
    #[must_use]
    pub fn segment(&self, part: usize) -> Option<Segment> {
        self.segments.get(part.checked_sub(1)?).copied()
    }

    /// How much of gap `gap` shows.
    #[must_use]
    fn shown<'s, A>(&self, shape: &Shape<'s, A>, gap: usize) -> Shown<'s> {
        let len = self.gaps.get(gap).map_or(0, |g| g.len);
        let partial = self.is_partial();
        if shape.full && !partial {
            return Shown {
                top: len,
                bottom: 0,
                len,
                windows: &[],
            };
        }
        match shape.reveal.get(gap).filter(|_| !partial) {
            // Settled for this length when stored.
            Some(r) => Shown {
                top: r.top,
                bottom: r.bottom,
                len,
                windows: &r.windows,
            },
            None => Shown {
                top: 0,
                bottom: 0,
                len,
                windows: &[],
            },
        }
    }

    /// Builds hunk `h`'s rows and block starts in the layout `shape` picks,
    /// unless it has them; a second layout's count toward [`File::bytes`].
    fn ensure_rows(&mut self, shape: &Shape<'_, impl Arrange>, h: usize) {
        let (diff, cores, views) = self.shown_hunks_mut(shape.structural);
        let (core, view) = (&cores[h], &mut views[h]);
        let layout = match shape.reading {
            None => {
                let arrangement = shape.arrange.arrangement(&hunk_header(core, b""));
                // Asked for every row drawn or moved over: the rows built
                // while both sides show are found without comparing layouts.
                if matches!(view.layout, Some(Layout::Arranged(shown)) if shown == arrangement) {
                    return;
                }
                Layout::Arranged(arrangement)
            }
            Some((_, opened)) if opened.binary_search(&h).is_ok() => Layout::Opened,
            Some((side, _)) => Layout::Read(side),
        };
        let held = |view: &HunkView| {
            view.rows.capacity() * size_of::<Row>() + view.starts.capacity() * size_of::<usize>()
        };
        match (view.layout, &mut view.other) {
            (Some(shown), _) if shown == layout => return,
            (Some(shown), Some(other)) if other.0 == layout => {
                std::mem::swap(&mut view.rows, &mut other.1);
                std::mem::swap(&mut view.starts, &mut other.2);
                other.0 = shown;
                view.layout = Some(layout);
                return;
            }
            // The other slot goes to the layout shown until now, and its
            // vectors are reused for the new one.
            (Some(shown), Some(other)) => {
                std::mem::swap(&mut view.rows, &mut other.1);
                std::mem::swap(&mut view.starts, &mut other.2);
                other.0 = shown;
                view.rows.clear();
                view.starts.clear();
            }
            (Some(shown), other @ None) => {
                *other = Some(Box::new((
                    shown,
                    std::mem::take(&mut view.rows),
                    std::mem::take(&mut view.starts),
                )));
            }
            (None, _) => {}
        }
        let reused = held(view);
        let body = &view.body;
        let kind = |i: usize| {
            let BodyLine { line, state } = body[i];
            let block_start =
                state != LineState::NoNewline && core.lines[line as usize].block_start;
            (state.kind(), block_start)
        };
        match layout {
            Layout::Arranged(arrangement) => rows(arrangement, body.len(), kind, &mut view.rows),
            Layout::Read(side) => {
                let text = [&diff.old, &diff.new][side.index()];
                let ends = |i: usize| {
                    let l = &core.lines[body[i].line as usize];
                    l.kind == LineKind::Context && !text.line(line_on(l, side)).ends_with(b"\n")
                };
                side_rows(side, body.len(), (kind, ends), &mut view.rows);
            }
            Layout::Opened => {
                view.rows.push(Row::Header);
                rows(Arrangement::Unified, body.len(), kind, &mut view.rows);
            }
        }
        let changed = |row: &Row| match *row {
            Row::Line(i)
            | Row::Pair(Some(i), _)
            | Row::Pair(None, Some(i))
            | Row::One { line: i, .. } => {
                matches!(body[i].state.kind(), Kind::Removed | Kind::Added)
            }
            Row::Header | Row::Marker | Row::Pair(None, None) | Row::Hidden { .. } => false,
        };
        view.starts.clear();
        let mut before = false;
        for (r, row) in view.rows.iter().enumerate() {
            // Every hidden run is a stop for moving by change, even beside
            // changed lines of the side read.
            let hidden = matches!(*row, Row::Hidden { .. });
            let now = hidden || changed(row);
            if now && (!before || hidden) {
                view.starts.push(r);
            }
            before = now;
        }
        // The first layout's rows are in the estimate the file was built
        // with; a second one's are on top of it, and a layout replacing the
        // second adds only what its vectors grew by. The estimate never
        // shrinks: callers count what a call added.
        let second = view.other.is_some();
        let added = held(view).saturating_sub(reused);
        view.layout = Some(layout);
        if second {
            self.bytes += added;
        }
    }

    /// Rows in part `part`, its blocks' included.
    #[inline]
    fn part_rows(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) -> usize {
        if shape.blocks.is_some() {
            self.ensure_blocks(shape, part);
            if let Some(placed) = self.placed(shape, part) {
                return placed.rows();
            }
        }
        self.own_rows(shape, part)
    }

    /// Part `part`'s own rows, without blocks.
    #[inline]
    fn own_rows(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) -> usize {
        match self.segment(part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(shape, h);
                self.shown_hunks(shape.structural).1[h].rows.len().max(1)
            }
            Some(Segment::Gap(g)) => self.shown(shape, g).rows(),
            Some(Segment::Start) | None => 1,
        }
    }

    /// Part `part`'s blocks as last placed for `shape`; `None` while the
    /// entry has none.
    #[inline]
    fn placed<A>(&self, shape: &Shape<'_, A>, part: usize) -> Option<&Placed> {
        let generation = shape.blocks?.generation;
        let blocks = self
            .blocks
            .as_deref()
            .filter(|b| b.generation == generation)?;
        blocks.parts.get(part).filter(|p| p.key.is_some())
    }

    /// What part `part`'s row `row` shows, once its blocks are placed.
    #[inline]
    fn at<A>(&self, shape: &Shape<'_, A>, part: usize, row: usize) -> At {
        self.placed(shape, part).map_or(At::Own(row), |p| p.at(row))
    }

    /// The row of part `part` showing its own row `own`, once its blocks are
    /// placed.
    #[inline]
    fn row_at<A>(&self, shape: &Shape<'_, A>, part: usize, own: usize) -> usize {
        self.placed(shape, part).map_or(own, |p| p.row(own))
    }

    /// The line of this file that `line` of the line diff's `side` is: a
    /// rendered view's last line drawn from it.
    fn shown_line(&self, side: Side, line: usize) -> Option<usize> {
        match &self.rendered {
            Some(rendered) => rendered.shown_line(side, line),
            None => Some(line),
        }
    }

    /// The part showing line `line` of `side` of this file; `None` when no
    /// part does.
    fn line_part(&self, structural: bool, side: Side, line: usize) -> Option<usize> {
        // Gaps and hunks run down the file in order on both sides.
        let gap = |g: &Gap| match side {
            Side::Old => g.old..g.old + g.len,
            Side::New => g.new..g.new + g.len,
        };
        let g = self.gaps.partition_point(|g| gap(g).end <= line);
        if self.gaps.get(g).is_some_and(|g| gap(g).contains(&line)) {
            return self.gap_part(g);
        }
        let (cores, _) = self.shown_hunks(structural);
        let hunk = |h: &Hunk| match side {
            Side::Old => h.old_start..h.old_start + h.old_len,
            Side::New => h.new_start..h.new_start + h.new_len,
        };
        let h = cores.partition_point(|h| hunk(h).end <= line);
        cores.get(h).filter(|h| hunk(h).contains(&line))?;
        self.segment_part(Segment::Hunk(h))
    }

    /// Places the entry's blocks in part `part` as `shape` shows it, unless
    /// they are.
    #[inline]
    fn ensure_blocks(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) {
        // Inlined so an entry without blocks pays a branch: every row query
        // comes through here.
        if shape.blocks.is_some() {
            self.place_blocks(shape, part);
        }
    }

    fn place_blocks(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) {
        let Some(blocks) = shape.blocks else {
            return;
        };
        if part >= self.parts() {
            return;
        }
        if (self.blocks.as_ref()).is_none_or(|b| b.generation != blocks.generation) {
            self.blocks = Some(Box::new(self.index_blocks(blocks)));
        }
        let own = self.own_rows(shape, part);
        let key = Key {
            width: shape.arrange.width(),
            structural: self.uses_structural(shape.structural),
            layout: match self.segment(part) {
                Some(Segment::Hunk(h)) => self.shown_hunks(shape.structural).1[h].layout,
                _ => None,
            },
            gap: None,
        };
        let gap = match self.segment(part) {
            Some(Segment::Gap(g)) => Some(self.shown(shape, g)),
            _ => None,
        };
        let fits = |held: &Key| {
            (held.width, held.structural, held.layout) == (key.width, key.structural, key.layout)
                && match (&held.gap, gap) {
                    (Some((top, bottom, windows)), Some(shown)) => {
                        (*top, *bottom, &windows[..]) == (shown.top, shown.bottom, shown.windows)
                    }
                    (None, None) => true,
                    _ => false,
                }
        };
        let Some(index) = self.blocks.as_deref() else {
            return;
        };
        if (index.parts.get(part))
            .and_then(|p| p.key.as_ref())
            .is_some_and(fits)
        {
            return;
        }
        let hanging = self.hanging(shape, part, own, blocks, index);
        let mut placed = Placed::new(own, hanging, blocks, shape.arrange);
        if let Some(Segment::Hunk(h)) = self.segment(part) {
            let starts = &self.shown_hunks(shape.structural).1[h].starts;
            placed.starts = starts.iter().map(|&s| placed.row(s)).collect();
        }
        placed.key = Some(Key {
            gap: gap.map(|s| (s.top, s.bottom, s.windows.to_vec())),
            ..key
        });
        let parts = self.parts();
        let Some(index) = self.blocks.as_deref_mut() else {
            return;
        };
        if index.parts.len() <= part {
            index.parts.resize_with(parts, Placed::default);
        }
        index.parts[part] = placed;
    }

    /// `blocks` on this file's lines (see [`FileBlocks::lines`]).
    fn index_blocks(&self, blocks: &Blocks) -> FileBlocks {
        let mut lines = [Vec::new(), Vec::new()];
        for (b, i) in blocks.list.iter().zip(0u32..) {
            if let Attach::Line { side, line } = b.at
                && let Some(line) = self.shown_line(side, line)
            {
                lines[side.index()].push((line, i));
            }
        }
        for side in &mut lines {
            side.sort_unstable();
        }
        FileBlocks {
            generation: blocks.generation,
            lines,
            parts: Vec::new(),
        }
    }

    /// The blocks hung in part `part` of `own` rows, for [`Placed::new`]:
    /// under its first row when it's the file's first part, under the rows
    /// showing their lines, under its last row when it's the last part.
    fn hanging<A>(
        &self,
        shape: &Shape<'_, A>,
        part: usize,
        own: usize,
        blocks: &Blocks,
        index: &FileBlocks,
    ) -> Vec<(usize, (u8, u32), BlockId)> {
        let (first, last) = (part == self.first_part(), part + 1 == self.parts());
        let mut hanging: Vec<_> = (blocks.list.iter().zip(0u32..))
            .filter_map(|(b, i)| match b.at {
                Attach::Start if first => Some((own.min(1), order(b.at, i), b.id)),
                Attach::End if last => Some((own, order(b.at, i), b.id)),
                _ => None,
            })
            .collect();
        let mut hang = |after: usize, i: u32| {
            let block = &blocks.list[i as usize];
            hanging.push((after, order(block.at, i), block.id));
        };
        match self.segment(part) {
            Some(Segment::Gap(g)) => {
                let (span, shown) = (self.gaps[g], self.shown(shape, g));
                for (side, start) in [(Side::Old, span.old), (Side::New, span.new)] {
                    let on = &index.lines[side.index()];
                    for &(line, i) in lines_in(on, start..start + span.len) {
                        hang(shown.row(line - start) + 1, i);
                    }
                }
            }
            Some(Segment::Hunk(h)) => {
                let core = &self.shown_hunks(shape.structural).0[h];
                for side in [Side::Old, Side::New] {
                    let range = match side {
                        Side::Old => core.old_start..core.old_start + core.old_len,
                        Side::New => core.new_start..core.new_start + core.new_len,
                    };
                    let wanted = lines_in(&index.lines[side.index()], range);
                    if wanted.is_empty() {
                        continue;
                    }
                    let rows = self.line_rows(shape.structural, h, side);
                    for &(line, i) in wanted {
                        let at = rows.partition_point(|&(l, _)| l < line);
                        if let Some(&(l, r)) = rows.get(at)
                            && l == line
                        {
                            hang(r + 1, i);
                        }
                    }
                }
            }
            Some(Segment::Start) | None => {}
        }
        hanging
    }

    /// Each line of `side` hunk `h` shows, with the row showing it, by line.
    /// While a side is read alone, a hidden line's row is the one of the
    /// line it pairs with, or else the one standing for it.
    fn line_rows(&self, structural: bool, h: usize, side: Side) -> Vec<(usize, usize)> {
        let (cores, views) = self.shown_hunks(structural);
        let (core, view) = (&cores[h], &views[h]);
        let mut rows = Vec::new();
        let mut push = |i: usize, r: usize| {
            let body = view.body[i];
            if body.state == LineState::NoNewline {
                return;
            }
            let l = &core.lines[body.line as usize];
            match (side, l.kind) {
                (Side::Old, LineKind::Added) | (Side::New, LineKind::Removed) => {}
                _ => rows.push((line_on(l, side), r)),
            }
        };
        for (r, row) in view.rows.iter().enumerate() {
            let shows = match *row {
                Row::Line(i) => [Some(i), None],
                Row::Pair(old, new) => [old, new],
                Row::One { line, paired } => [Some(line), paired],
                Row::Hidden { from, n } => {
                    (from..from + n as usize).for_each(|i| push(i, r));
                    [None, None]
                }
                Row::Header | Row::Marker => [None, None],
            };
            for i in shows.into_iter().flatten() {
                push(i, r);
            }
        }
        rows.sort_unstable();
        rows
    }

    /// Whether the syntax diff's hunks show for `structural`.
    fn uses_structural(&self, structural: bool) -> bool {
        matches!(&self.structural, Some(s) if structural && s.hunks.len() == self.hunks.len())
    }

    /// The arrangement of part `part`'s rows; `None` for a part that isn't
    /// a hunk, and for one read on a side alone. A hunk opened in place is
    /// unified.
    fn arrangement(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) -> Option<Arrangement> {
        let Some(Segment::Hunk(h)) = self.segment(part) else {
            return None;
        };
        self.ensure_rows(shape, h);
        match self.shown_hunks(shape.structural).1[h].layout? {
            Layout::Arranged(arrangement) => Some(arrangement),
            Layout::Opened => Some(Arrangement::Unified),
            Layout::Read(_) => None,
        }
    }

    /// Moves `pos` to the next row, offset 0; false at the file's end.
    fn next_row(&mut self, shape: &Shape<'_, impl Arrange>, pos: &mut Pos) -> bool {
        let mut next = Pos { offset: 0, ..*pos };
        if next.row + 1 < self.part_rows(shape, next.part) {
            next.row += 1;
        } else if next.part + 1 < self.parts() {
            next.part += 1;
            next.row = 0;
        } else {
            return false;
        }
        *pos = next;
        true
    }

    /// Moves `pos` to the previous row, offset 0; false at the file's
    /// first.
    fn prev_row(&mut self, shape: &Shape<'_, impl Arrange>, pos: &mut Pos) -> bool {
        let mut prev = Pos { offset: 0, ..*pos };
        if prev.row > 0 {
            prev.row -= 1;
        } else if prev.part > self.first_part() {
            prev.part -= 1;
            prev.row = self.part_rows(shape, prev.part) - 1;
        } else {
            return false;
        }
        *pos = prev;
        true
    }

    /// The rows of part `part` that start a block of changed lines; none
    /// for a part that isn't a hunk.
    fn starts(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) -> &[usize] {
        let Some(Segment::Hunk(h)) = self.segment(part) else {
            return &[];
        };
        self.ensure_rows(shape, h);
        self.ensure_blocks(shape, part);
        if let Some(placed) = self.placed(shape, part) {
            return &placed.starts;
        }
        &self.shown_hunks(shape.structural).1[h].starts
    }

    /// The first start of a block of changes after `from`.
    fn next_change(&mut self, shape: &Shape<'_, impl Arrange>, from: Pos) -> Option<Pos> {
        for part in from.part.max(1)..self.parts() {
            let at = |row| Pos {
                file: from.file,
                part,
                row,
                offset: 0,
            };
            if let Some(&row) = self.starts(shape, part).iter().find(|&&r| at(r) > from) {
                return Some(at(row));
            }
        }
        None
    }

    /// The last start of a block of changes before `from`: with an offset
    /// past 0, its own row's start counts as before it.
    fn prev_change(&mut self, shape: &Shape<'_, impl Arrange>, from: Pos) -> Option<Pos> {
        for part in (1..=from.part).rev() {
            let at = |row| Pos {
                file: from.file,
                part,
                row,
                offset: 0,
            };
            if let Some(&row) = self
                .starts(shape, part)
                .iter()
                .rev()
                .find(|&&r| at(r) < from)
            {
                return Some(at(row));
            }
        }
        None
    }

    /// The first (or `last`) start of a block of changes.
    fn edge(&mut self, shape: &Shape<'_, impl Arrange>, file: usize, last: bool) -> Option<Pos> {
        let parts = self.parts();
        let mut edge = |p: usize| {
            let starts = self.starts(shape, p);
            let row = if last { starts.last() } else { starts.first() };
            Some(Pos {
                file,
                part: p,
                row: *row?,
                offset: 0,
            })
        };
        if last {
            (1..parts).rev().find_map(&mut edge)
        } else {
            (1..parts).find_map(&mut edge)
        }
    }

    /// The first start of a block of changes whose new-side line number
    /// (1-based, as [`File::line_of`] counts) is past `line`, or with none
    /// the last start.
    fn change_after(
        &mut self,
        shape: &Shape<'_, impl Arrange>,
        file: usize,
        line: usize,
    ) -> Option<Pos> {
        let mut best = None;
        for part in 1..self.parts() {
            let Some(Segment::Hunk(h)) = self.segment(part) else {
                continue;
            };
            self.ensure_rows(shape, h);
            self.ensure_blocks(shape, part);
            let (_, views) = self.shown_hunks(shape.structural);
            for &own in &views[h].starts {
                let pos = Pos {
                    file,
                    part,
                    row: self.row_at(shape, part, own),
                    offset: 0,
                };
                best = Some(pos);
                if self.hunk_line(shape.structural, h, own) > line {
                    return best;
                }
            }
        }
        best
    }

    /// The hunk `pos` is in and how many there are: the hunk its part is,
    /// or the last above it.
    #[must_use]
    pub fn hunk_position(&self, pos: Pos) -> Option<(usize, usize)> {
        let total = self.hunks.len();
        let at = match self.segment(pos.part) {
            Some(Segment::Hunk(h)) => h + 1,
            Some(Segment::Gap(g)) => g.max(1),
            Some(Segment::Start) | None => 1,
        };
        (total > 0).then_some((at.min(total), total))
    }

    /// The new-side line number (1-based) the row at `pos` shows, or the
    /// first after it; a block's row counts as the row it hangs under.
    fn line_of(&mut self, shape: &Shape<'_, impl Arrange>, pos: Pos) -> usize {
        let (part, from) = if pos.part == 0 {
            (1, 0)
        } else {
            self.ensure_blocks(shape, pos.part);
            (pos.part, self.at(shape, pos.part, pos.row).own())
        };
        match self.segment(part) {
            Some(Segment::Gap(g)) => self.gaps[g].new + self.shown(shape, g).line_at(from) + 1,
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(shape, h);
                self.hunk_line(shape.structural, h, from)
            }
            Some(Segment::Start) => self.hunk_line(shape.structural, 0, 0),
            None => 1,
        }
    }

    /// The new-side line number of the first line at or after row `from`
    /// of hunk `h`. A removed line's is where it would be in the new file;
    /// a row standing only for hidden lines counts the first of them.
    fn hunk_line(&self, structural: bool, h: usize, from: usize) -> usize {
        let (cores, views) = self.shown_hunks(structural);
        let (core, view) = (&cores[h], &views[h]);
        let first_body = (from..view.rows.len())
            .find_map(|r| match view.rows[r] {
                Row::Line(i)
                | Row::Pair(_, Some(i))
                | Row::Pair(Some(i), None)
                | Row::One { line: i, .. }
                | Row::Hidden { from: i, .. } => Some(i),
                Row::Header | Row::Marker | Row::Pair(None, None) => None,
            })
            .unwrap_or(0);
        view.body
            .get(first_body)
            .map_or(1, |b| core.lines[b.line as usize].new + 1)
    }

    /// What the row at `pos` shows, to find it again after rows changed
    /// around it (see [`File::unanchor`]).
    fn anchor(&mut self, shape: &Shape<'_, impl Arrange>, pos: Pos) -> Anchor {
        self.ensure_blocks(shape, pos.part);
        match self.at(shape, pos.part, pos.row) {
            At::Block { id, line, after } => Anchor::Block { id, line, after },
            At::Parting { id, after } => Anchor::Block { id, line: 0, after },
            At::Own(own) => match self.segment(pos.part) {
                Some(Segment::Gap(g)) => Anchor::Line(self.shown(shape, g).line_at(own)),
                _ => Anchor::Own(own),
            },
        }
    }

    /// `pos` moved to the row of its part showing `anchor` now; a gap's
    /// line found on another row starts at its first screen row.
    fn unanchor(&mut self, shape: &Shape<'_, impl Arrange>, pos: Pos, anchor: Anchor) -> Pos {
        self.ensure_blocks(shape, pos.part);
        let own = match anchor {
            Anchor::Own(own) => own,
            Anchor::Line(line) => {
                let Some(Segment::Gap(g)) = self.segment(pos.part) else {
                    return pos;
                };
                let own = self.shown(shape, g).row(line);
                let row = self.row_at(shape, pos.part, own);
                let offset = if row == pos.row { pos.offset } else { 0 };
                return Pos { row, offset, ..pos };
            }
            Anchor::Block { id, line, after } => {
                match self.placed(shape, pos.part).and_then(|p| p.block(id)) {
                    Some(block) => {
                        return Pos {
                            row: block.top + line.min(block.rows - 1),
                            ..pos
                        };
                    }
                    None => after.saturating_sub(1),
                }
            }
        };
        Pos {
            row: self.row_at(shape, pos.part, own),
            ..pos
        }
    }

    /// The part showing gap `gap`.
    fn gap_part(&self, gap: usize) -> Option<usize> {
        self.segment_part(Segment::Gap(gap))
    }

    /// The part showing `segment`.
    fn segment_part(&self, segment: Segment) -> Option<usize> {
        // Gap `n` (or the start, in its place) comes before hunk `n`.
        let rank = |s: &Segment| match *s {
            Segment::Start => 0,
            Segment::Gap(g) => 2 * g,
            Segment::Hunk(h) => 2 * h + 1,
        };
        let i = (self.segments).partition_point(|s| rank(s) < rank(&segment));
        (self.segments.get(i) == Some(&segment)).then_some(1 + i)
    }

    /// What gap `gap` shows after one more reveal: lines from its edges,
    /// next to the hunk below for the gap before the first hunk, next to
    /// the hunk above for the gap after the last, half of each otherwise;
    /// windows inside it stay until the edges reach them. `None` when
    /// nothing of it is hidden, and always for a piped file.
    #[must_use]
    fn reveal_more<A>(&self, shape: &Shape<'_, A>, gap: usize) -> Option<Reveal> {
        let gaps = self.gaps.len();
        let shown = self.shown(shape, gap);
        let hidden = shown.hidden();
        if self.is_partial() || gap >= gaps || hidden == 0 {
            return None;
        }
        let mut reveal = shape.reveal.get(gap).cloned().unwrap_or_default();
        let n = if hidden <= 2 * REVEAL_LINES {
            // The edges alone wouldn't reach the lines between windows.
            if !reveal.windows.is_empty() {
                reveal.top = shown.len;
            }
            hidden
        } else {
            REVEAL_LINES
        };
        if gap == 0 {
            reveal.bottom += n;
        } else if gap + 1 == gaps {
            reveal.top += n;
        } else {
            reveal.top += n - n / 2;
            reveal.bottom += n / 2;
        }
        reveal.settle(shown.len);
        Some(reveal)
    }

    /// What gap `gap` shows once the gap lines of `side` in `lines` show,
    /// with `context` lines around them; `None` when they show already, or
    /// none is in the gap, and always for a piped file.
    fn reveal_lines(
        &self,
        reveal: Option<&Reveal>,
        gap: usize,
        side: Side,
        lines: &Range<usize>,
        context: usize,
    ) -> Option<Reveal> {
        let span = self.gaps.get(gap).filter(|_| !self.is_partial())?;
        let first = match side {
            Side::Old => span.old,
            Side::New => span.new,
        };
        let (start, end) = (
            lines.start.max(first) - first,
            lines.end.min(first + span.len).checked_sub(first)?,
        );
        let (top, bottom, windows) =
            reveal.map_or((0, 0, &[][..]), |r| (r.top, r.bottom, &r.windows[..]));
        let shown = Shown {
            top,
            bottom,
            len: span.len,
            windows,
        };
        if start >= end || !shown.hidden_lines().any(|h| h.start < end && start < h.end) {
            return None;
        }
        let mut reveal = reveal.cloned().unwrap_or_default();
        reveal
            .windows
            .push(start.saturating_sub(context)..end + context);
        reveal.settle(span.len);
        Some(reveal)
    }

    /// The new lines the gaps hide under their separators, in order, for
    /// [`find`].
    #[must_use]
    fn hidden_lines<A>(&self, shape: &Shape<'_, A>) -> Vec<Range<usize>> {
        (0..self.gaps.len())
            .flat_map(|g| {
                let start = self.gaps[g].new;
                self.shown(shape, g)
                    .hidden_lines()
                    .map(move |h| start + h.start..start + h.end)
            })
            .collect()
    }

    /// The row showing match `m`.
    fn match_pos(
        &mut self,
        shape: &Shape<'_, impl Arrange>,
        file: usize,
        m: &Match,
    ) -> Option<Pos> {
        let line = m.line as usize;
        let start = |g: &Gap| match m.side {
            Side::Old => g.old,
            Side::New => g.new,
        };
        if let Some(g) =
            (self.gaps.iter()).position(|g| (start(g)..start(g) + g.len).contains(&line))
        {
            let part = self.gap_part(g)?;
            let own = self.shown(shape, g).row(line - start(&self.gaps[g]));
            self.ensure_blocks(shape, part);
            return Some(Pos {
                file,
                part,
                row: self.row_at(shape, part, own),
                offset: 0,
            });
        }
        let (cores, _) = self.shown_hunks(shape.structural);
        let h = cores.iter().position(|h| match m.side {
            Side::Old => (h.old_start..h.old_start + h.old_len).contains(&line),
            Side::New => (h.new_start..h.new_start + h.new_len).contains(&line),
        })?;
        let part = 1 + self.segments.iter().position(|s| *s == Segment::Hunk(h))?;
        self.ensure_rows(shape, h);
        let (cores, views) = self.shown_hunks(shape.structural);
        let own = line_row(&cores[h], &views[h], m.side, line)?;
        self.ensure_blocks(shape, part);
        Some(Pos {
            file,
            part,
            row: self.row_at(shape, part, own),
            offset: 0,
        })
    }

    /// The old and new lines (0-based) the row at `pos` shows; `None` on a
    /// side without one, and on rows that aren't code, a block's included.
    fn row_lines(&mut self, shape: &Shape<'_, impl Arrange>, pos: Pos) -> [Option<usize>; 2] {
        self.ensure_blocks(shape, pos.part);
        let At::Own(row) = self.at(shape, pos.part, pos.row) else {
            return [None, None];
        };
        let pos = Pos { row, ..pos };
        match self.segment(pos.part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(shape, h);
                let (cores, views) = self.shown_hunks(shape.structural);
                let (core, view) = (&cores[h], &views[h]);
                let line = |i: usize| {
                    let body = view.body[i];
                    if body.state == LineState::NoNewline {
                        return [None, None];
                    }
                    let l = &core.lines[body.line as usize];
                    match l.kind {
                        LineKind::Context => [Some(l.old), Some(l.new)],
                        LineKind::Removed => [Some(l.old), None],
                        LineKind::Added => [None, Some(l.new)],
                    }
                };
                match view.rows.get(pos.row) {
                    Some(&(Row::Line(i) | Row::One { line: i, .. })) => line(i),
                    Some(&Row::Pair(old, new)) => {
                        [old.and_then(|i| line(i)[0]), new.and_then(|i| line(i)[1])]
                    }
                    _ => [None, None],
                }
            }
            Some(Segment::Gap(g)) => match self.shown(shape, g).line(pos.row) {
                Some(offset) => {
                    let span = self.gaps[g];
                    [Some(span.old + offset), Some(span.new + offset)]
                }
                None => [None, None],
            },
            Some(Segment::Start) | None => [None, None],
        }
    }

    /// The lines of the other side that the row at `pos` marks as hidden,
    /// while a side is read alone: that side and the lines, in order. `None`
    /// on any other row.
    fn mark(&mut self, shape: &Shape<'_, impl Arrange>, pos: Pos) -> Option<(Side, Range<usize>)> {
        let (side, _) = shape.reading?;
        let Some(Segment::Hunk(h)) = self.segment(pos.part) else {
            return None;
        };
        self.ensure_rows(shape, h);
        self.ensure_blocks(shape, pos.part);
        let At::Own(row) = self.at(shape, pos.part, pos.row) else {
            return None;
        };
        let (cores, views) = self.shown_hunks(shape.structural);
        let (core, view) = (&cores[h], &views[h]);
        let Row::Hidden { from, n } = *view.rows.get(row)? else {
            return None;
        };
        let start = line_on(&core.lines[view.body[from].line as usize], side.other());
        Some((side.other(), start..start + n as usize))
    }

    /// Hunk `h`'s lines as shown (the syntax diff's with `structural`, when
    /// there is one), by line of its body.
    #[inline]
    #[must_use]
    pub fn lines(&self, structural: bool, h: usize) -> HunkLines<'_> {
        self.lines_read(structural, h, Side::New)
    }

    /// Hunk `h`'s lines as [`File::lines`] gives them, unchanged lines
    /// taking `side`'s text.
    fn lines_read(&self, structural: bool, h: usize, side: Side) -> HunkLines<'_> {
        let (cores, views) = self.shown_hunks(structural);
        HunkLines {
            file: self,
            core: &cores[h],
            view: &views[h],
            side,
        }
    }

    /// Builds part `part`'s rows in the arrangement `shape` picks, when
    /// it's a hunk without them, and places its blocks (see [`File::row`]).
    #[inline]
    fn ensure_part(&mut self, shape: &Shape<'_, impl Arrange>, part: usize) {
        if let Some(Segment::Hunk(h)) = self.segment(part) {
            self.ensure_rows(shape, h);
        }
        self.ensure_blocks(shape, part);
    }

    /// What the row at `pos` shows, once [`File::ensure_part`] built its
    /// part's rows; `None` past the last part. Its offset is ignored.
    fn row<A>(&self, shape: &Shape<'_, A>, pos: Pos) -> Option<Content<'_>> {
        let pos = match self.at(shape, pos.part, pos.row) {
            At::Block { id, line, .. } => return Some(Content::Block { id, line }),
            At::Parting { id, .. } => return Some(Content::Parting { id }),
            At::Own(row) if pos.part == 0 => return Some(Content::Sheet(row)),
            At::Own(row) => Pos { row, ..pos },
        };
        let segment = self.segment(pos.part)?;
        let (cores, views) = self.shown_hunks(shape.structural);
        Some(match segment {
            Segment::Start => Content::Start(self.header(&cores[0], self.heading(views, 0))),
            Segment::Hunk(h) => {
                let side = shape.reading.map_or(Side::New, |(side, _)| side);
                let lines = self.lines_read(shape.structural, h, side);
                Content::Hunk {
                    header: self.header(lines.core, self.heading(views, h)),
                    row: lines.view.rows[pos.row],
                    lines,
                }
            }
            Segment::Gap(g) => {
                let shown = self.shown(shape, g);
                let span = self.gaps[g];
                match shown.line(pos.row) {
                    None => {
                        let hides = shown.hides(pos.row).unwrap_or_default();
                        let last = hides.end == shown.len - shown.bottom;
                        Content::Separator {
                            hidden: self.old_count(span.old + hides.start..span.old + hides.end),
                            next: cores
                                .get(g)
                                .filter(|_| last)
                                .map(|core| self.header(core, self.heading(views, g))),
                        }
                    }
                    Some(offset) => {
                        let (old, new) = (span.old + offset, span.new + offset);
                        // The sides' unchanged lines may differ where
                        // whitespace is ignored.
                        let (side, line) = match shape.reading {
                            Some((Side::Old, _)) => (Side::Old, old),
                            Some((Side::New, _)) | None => (Side::New, new),
                        };
                        let raw = match side {
                            Side::Old => self.diff.old.line(old),
                            Side::New => self.diff.new.line(new),
                        };
                        let spans = self.gap_syntax[side.index()]
                            .get(g)
                            .and_then(Option::as_ref);
                        let line = Line {
                            state: LineState::Context,
                            old: Some(old + 1),
                            new: Some(new + 1),
                            text: display_text(raw.strip_suffix(b"\n").unwrap_or(raw)),
                            emphasis: &[],
                            syntax: spans.map(|s| s.line(line)),
                            paint: &[],
                            block_start: false,
                        };
                        Content::Gap {
                            old,
                            new,
                            line: rendered::restyle(self.rendered.as_deref(), line),
                        }
                    }
                }
            }
        })
    }

    /// Where `pos` is, for a title: the hunk it's in, or in revealed
    /// unchanged lines the heading above them, else the hunk above. `None`
    /// before the first hunk and for a whole added or deleted file.
    /// `headings` keeps the last heading search going while positions move
    /// down. A block's row is where the row it hangs under is; its blocks
    /// are placed first ([`File::ensure_part`]).
    fn context<A>(
        &self,
        shape: &Shape<'_, A>,
        pos: Pos,
        headings: &mut Headings,
    ) -> Option<Context<'_>> {
        if self.whole {
            return None;
        }
        let hunk = match self.segment(pos.part)? {
            Segment::Hunk(h) => h,
            Segment::Start => 0,
            Segment::Gap(g) => {
                let shown = self.shown(shape, g);
                let line = shown
                    .line(self.at(shape, pos.part, pos.row).own())
                    .and_then(|offset| headings.above(&self.diff, self.gaps[g].old + offset));
                if let Some(line) = line {
                    return Some(Context::Heading(heading_text(&self.diff.old, line)));
                }
                g.checked_sub(1)?
            }
        };
        let (cores, views) = self.shown_hunks(shape.structural);
        Some(Context::Hunk(
            self.header(&cores[hunk], self.heading(views, hunk)),
        ))
    }
}

/// What a row of a file shows (see [`Document::row`]).
#[derive(Debug, Clone, Copy)]
pub enum Content<'a> {
    /// The first hunk's header row, before which no gap comes.
    Start(HunkHeader<'a>),
    /// A row of a hunk: its arranged lines, by line of the body.
    Hunk {
        /// The hunk's header, its heading included.
        header: HunkHeader<'a>,
        /// The row.
        row: Row,
        /// The lines its indices refer to.
        lines: HunkLines<'a>,
    },
    /// A gap's separator row.
    Separator {
        /// Old lines it hides, counted as source lines for a rendered view.
        hidden: usize,
        /// The header of the hunk after it; `None` after the last, and on a
        /// separator with revealed lines below it in the same gap.
        next: Option<HunkHeader<'a>>,
    },
    /// An unchanged line of a gap.
    Gap {
        /// Its old line, 0-based.
        old: usize,
        /// Its new line, 0-based.
        new: usize,
        /// The line, its numbers 1-based (a rendered view's are its
        /// source's).
        line: Line<'a>,
    },
    /// Row `line` of a block the host inserted (see [`Block`]).
    Block {
        /// The block.
        id: BlockId,
        /// Its row, from 0.
        line: usize,
    },
    /// A blank row parting block `id` from the block above it, both hung
    /// at one place (see [`super::Arrange::parting`]). It's neither's.
    Parting {
        /// The block below it.
        id: BlockId,
    },
    /// Row `n` of part 0: a sheet's row (see [`Entry::is_sheet`]), or the
    /// blank row of a file without segments. The host draws it.
    Sheet(usize),
}

/// The lines of a hunk as shown, each with its state, emphasis (once set)
/// and syntax runs, borrowed from the [`File`].
#[derive(Clone, Copy)]
pub struct HunkLines<'a> {
    file: &'a File,
    core: &'a Hunk,
    view: &'a HunkView,
    /// The side unchanged lines take their text and syntax runs from.
    side: Side,
}

impl std::fmt::Debug for HunkLines<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HunkLines")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl<'a> HunkLines<'a> {
    /// Lines in the body, no-newline markers included.
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        self.view.body.len()
    }

    /// Whether the body has no lines.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.view.body.is_empty()
    }

    /// The hunk's lines only on `side`: removed lines on the old side,
    /// added ones on the new.
    #[must_use]
    pub fn only_on(&self, side: Side) -> usize {
        let kind = match side {
            Side::Old => LineKind::Removed,
            Side::New => LineKind::Added,
        };
        self.core.lines.iter().filter(|l| l.kind == kind).count()
    }

    /// Line `i` of the body.
    #[inline]
    #[must_use]
    pub fn line(&self, i: usize) -> Line<'a> {
        let BodyLine { line, state } = self.view.body[i];
        if state == LineState::NoNewline {
            return NO_NEWLINE_LINE;
        }
        let line = line as usize;
        let emphasis = self
            .view
            .emphasis
            .as_deref()
            .map_or(NO_EMPHASIS, |e| &e[line][..]);
        let core_line = &self.core.lines[line];
        if self.side == Side::Old && core_line.kind == LineKind::Context {
            return self.old_context(core_line, state, emphasis);
        }
        let shown = diff_line(
            &self.file.diff,
            core_line,
            state,
            emphasis,
            self.view.syntax.as_ref(),
        );
        rendered::restyle(self.file.rendered.as_deref(), shown)
    }

    /// [`HunkLines::line`] for an unchanged line read on the old side: the
    /// sides' unchanged lines may differ where whitespace is ignored.
    // Out of line, so the path every drawn line takes stays as small.
    #[inline(never)]
    fn old_context(
        &self,
        core_line: &diff::Line,
        state: LineState,
        emphasis: &'a [Range<usize>],
    ) -> Line<'a> {
        let syntax = self.view.syntax.as_ref();
        let raw = self.file.diff.old.line(core_line.old);
        let shown = Line {
            text: display_text(raw.strip_suffix(b"\n").unwrap_or(raw)),
            syntax: (syntax.and_then(|s| s.old.as_ref())).map(|o| o.line(core_line.old)),
            ..diff_line(&self.file.diff, core_line, state, emphasis, syntax)
        };
        rendered::restyle(self.file.rendered.as_deref(), shown)
    }
}

/// Where a position is, for a title: the hunk it's in, or in revealed
/// unchanged lines the heading above them, else the hunk above (see
/// [`Document::context`]).
#[derive(Debug, Clone, Copy)]
pub enum Context<'a> {
    /// In a hunk, or at a gap's separator under the hunk above.
    Hunk(HunkHeader<'a>),
    /// In revealed unchanged lines, under this heading line.
    Heading(&'a [u8]),
}

/// The last search for a heading line: the diff, the line it searched up
/// from, and the search, which continues forward from there for a line
/// further down.
#[derive(Debug, Default)]
struct Headings {
    last: Option<(Arc<FileDiff>, usize, Heading)>,
}

impl Headings {
    /// The heading line (as git finds one) above old line `line` of `diff`.
    fn above(&mut self, diff: &Arc<FileDiff>, line: usize) -> Option<usize> {
        let heading = match &mut self.last {
            Some((d, from, heading)) if Arc::ptr_eq(d, diff) && *from <= line => {
                *from = line;
                heading
            }
            slot => &mut slot.insert((Arc::clone(diff), line, Heading::default())).2,
        };
        heading.find_line(&diff.old, line)
    }
}

/// A hunk's numbers as git prints them, 1-based.
#[inline]
#[must_use]
pub fn hunk_header<'a>(hunk: &Hunk, heading: &'a [u8]) -> HunkHeader<'a> {
    let one_based = |start: usize, len: usize| if len == 0 { start } else { start + 1 };
    HunkHeader {
        old_start: one_based(hunk.old_start, hunk.old_len),
        old_len: hunk.old_len,
        new_start: one_based(hunk.new_start, hunk.new_len),
        new_len: hunk.new_len,
        heading,
        hidden: 0,
    }
}

/// `line`, a line of a hunk of `fd`, as drawn: its numbers 1-based, its
/// text without terminator, and its syntax runs from `syntax` unless
/// `state` is moved or reformatted code, whose own color they'd hide.
/// `state` is the line's [`LineState::of`], which a caller may have stored.
#[inline]
#[must_use]
pub fn diff_line<'a>(
    fd: &'a FileDiff,
    line: &diff::Line,
    state: LineState,
    emphasis: &'a [Range<usize>],
    syntax: Option<&'a HunkSyntax>,
) -> Line<'a> {
    let raw = line_bytes(&fd.old, &fd.new, line);
    let runs = syntax
        .filter(|_| !state.is_unchanged())
        .and_then(|s| match line.kind {
            LineKind::Removed => s.old.as_ref().map(|o| o.line(line.old)),
            LineKind::Added | LineKind::Context => s.new.as_ref().map(|n| n.line(line.new)),
        });
    Line {
        state,
        old: (line.kind != LineKind::Added).then_some(line.old + 1),
        new: (line.kind != LineKind::Removed).then_some(line.new + 1),
        text: display_text(raw.strip_suffix(b"\n").unwrap_or(raw)),
        emphasis,
        syntax: runs,
        paint: &[],
        block_start: line.block_start,
    }
}

/// With `-w`/`-b`, a file whose changes are all whitespace has no hunks and
/// is left out, as git does. One with a note (a read or a filter failed)
/// stays, to show it.
#[must_use]
pub fn whitespace_only(fd: &FileDiff, whitespace: Whitespace) -> bool {
    whitespace != Whitespace::Exact
        && fd.hunks.is_empty()
        && !fd.binary
        && !fd.too_large
        && fd.note.is_none()
        && fd.change.status == Status::Modified
        && fd.change.old.map(|s| s.mode()) == fd.change.new.map(|s| s.mode())
}

/// How a header names `status`.
#[must_use]
pub fn file_status(status: &Status) -> FileStatus {
    match status {
        Status::Added => FileStatus::Added,
        Status::Deleted => FileStatus::Deleted,
        Status::Modified => FileStatus::Modified,
        Status::TypeChange => FileStatus::TypeChange,
        Status::Untracked => FileStatus::Untracked,
        Status::Unmerged => FileStatus::Unmerged,
        Status::Renamed { .. } => FileStatus::Renamed,
    }
}

fn mode_str(mode: Mode) -> &'static str {
    match mode {
        Mode::File => "100644",
        Mode::Executable => "100755",
        Mode::Symlink => "120000",
        Mode::Gitlink => "160000",
    }
}

/// The header of `change`'s diff, as printed: without staging. `note`
/// replaces its hunks.
#[must_use]
pub fn file_header<'a>(change: &'a FileChange, note: Option<&'a str>) -> FileHeader<'a> {
    FileHeader {
        status: file_status(&change.status),
        old_path: change.old_path().as_bytes(),
        new_path: change.path.as_bytes(),
        old_mode: change.old.map(|s| mode_str(s.mode())),
        new_mode: change.new.map(|s| mode_str(s.mode())),
        similarity: match change.status {
            Status::Renamed { similarity, .. } => Some(similarity),
            _ => None,
        },
        note,
        staging: None,
    }
}

/// Why `fd` has no hunks to show, if it's for a reason worth a note.
#[must_use]
pub fn note(fd: &FileDiff) -> Option<&'static str> {
    if fd.binary {
        Some("binary files differ")
    } else if fd.too_large {
        Some("file too large to diff")
    } else {
        None
    }
}

/// The note a syntax diff adds under the file header, if any.
#[must_use]
pub fn structural_note(result: &Result<Structural, Unavailable>) -> Option<String> {
    match result {
        Ok(s) => s.note(),
        Err(e) => Some(e.note().to_owned()),
    }
}

/// The id a side's cached results are stored under. A working-tree side's id
/// is the one the scan hashed, and the file may have changed since, so only
/// a side read from the object database has one.
fn cache_id(side: Option<GitSide>) -> Option<ObjectId> {
    match side? {
        GitSide::Object { id, .. } => Some(id),
        GitSide::Worktree { .. } | GitSide::Path { .. } => None,
    }
}

/// The old and new sides of `change`, whose texts are `old` and `new`, as
/// highlighting takes them. Borrows only those, so a caller may pair the
/// diff's hunks while the sides parse.
#[must_use]
pub fn side_sources<'a>(
    change: &'a FileChange,
    old: &'a diff::Text,
    new: &'a diff::Text,
) -> [highlight::Source<'a>; 2] {
    [
        highlight::Source {
            path: change.old_path().as_bytes(),
            text: old,
            id: cache_id(change.old),
        },
        highlight::Source {
            path: change.path.as_bytes(),
            text: new,
            id: cache_id(change.new),
        },
    ]
}

/// `fd`'s old and new sides, as highlighting takes them.
#[must_use]
pub fn sources(fd: &FileDiff) -> [highlight::Source<'_>; 2] {
    side_sources(&fd.change, &fd.old, &fd.new)
}

/// The file a patch header describes, as the rest of the core knows files.
/// The sides have no object ids, so nothing is cached under them.
#[must_use]
pub fn file_change(info: FileInfo) -> FileChange {
    let mode = |mode: Option<&str>| match mode {
        Some("100755") => Mode::Executable,
        Some("120000") => Mode::Symlink,
        Some("160000") => Mode::Gitlink,
        _ => Mode::File,
    };
    let side = |m: Option<&String>| {
        Some(GitSide::Worktree {
            mode: mode(m.map(String::as_str)),
            id: None,
        })
    };
    let (old, new) = (side(info.old_mode.as_ref()), side(info.new_mode.as_ref()));
    let (old_path, new_path) = (BString::from(info.old_path), BString::from(info.new_path));
    let (path, status, old, new) = match info.status {
        FileStatus::Added | FileStatus::Untracked => (new_path, Status::Added, None, new),
        FileStatus::Deleted => (old_path, Status::Deleted, old, None),
        FileStatus::Renamed | FileStatus::Copied => (
            new_path,
            Status::Renamed {
                from: old_path,
                similarity: info.similarity.unwrap_or(0),
            },
            old,
            new,
        ),
        FileStatus::TypeChange => (new_path, Status::TypeChange, old, new),
        FileStatus::Unmerged => (new_path, Status::Unmerged, old, new),
        FileStatus::Modified | FileStatus::Combined => (new_path, Status::Modified, old, new),
    };
    FileChange {
        path,
        status,
        old,
        new,
        staging: None,
    }
}

#[cfg(test)]
mod tests;

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
//! This is the top layer of the core: it uses [`crate::git`],
//! [`crate::highlight`], [`crate::structural`] and [`crate::cache`], and none
//! of them use it.

mod entries;
mod prepare;
mod rendered;
mod search;

use std::ops::Range;
use std::sync::Arc;

use gix::bstr::{BString, ByteSlice};

pub use entries::{Document, Entry, Found, Missing, Piped, Render, Settle, Source, State};
pub use prepare::{Prepare, SyntaxSlot, pair};
pub use rendered::{Origin, Rendered};
pub use search::{Match, Matches, Needle, find, word};

use crate::diff::{self, Emphasis, Hunk, LineKind, Whitespace};
use crate::git::{FileChange, FileDiff, Mode, ObjectId, Side as GitSide, Status};
use crate::highlight::{self, HunkSyntax, MAX_HIGHLIGHT_BYTES, Registry};
use crate::patch::{self, FileInfo};
use crate::structural::{Structural, Unavailable};
use crate::view::{
    Arrangement, FileHeader, FileStatus, Heading, HunkHeader, Kind, Line, LineState, Row, Spans,
    display_text, heading_text, line_bytes, no_newline, rows,
};

/// Lines one reveal adds to a gap; a gap of up to twice this opens whole.
const REVEAL_LINES: usize = 20;

/// A side of a diff: the old text or the new.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The text before the change.
    Old,
    /// The text after it.
    New,
}

impl Side {
    /// 0 for the old side, 1 for the new, to index per-side pairs.
    #[inline]
    #[must_use]
    pub fn index(self) -> usize {
        match self {
            Self::Old => 0,
            Self::New => 1,
        }
    }

    /// The opposite side.
    #[inline]
    #[must_use]
    pub fn other(self) -> Self {
        match self {
            Self::Old => Self::New,
            Self::New => Self::Old,
        }
    }
}

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
/// still hidden around them.
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

/// What the user revealed of a gap: lines from each edge, and windows of
/// lines between. Kept settled for the gap's length: windows sorted, none
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

/// How a file is shown, which decides its parts' rows: the syntax diff or
/// the line diff, how much of each gap is revealed, and each hunk's
/// arrangement. Queries that never build a hunk's rows take any `A`.
struct Shape<'a, A> {
    /// Show the syntax diff where the file has one.
    structural: bool,
    /// Show every gap whole; a piped file's gaps still never open.
    full: bool,
    /// What's revealed of each gap, by gap; gaps past its end show nothing.
    reveal: &'a [Reveal],
    /// Each hunk's arrangement, from its numbers (and no heading).
    arrange: A,
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
    /// The new side's syntax runs over each gap, once they're set.
    gap_syntax: Vec<Option<Spans>>,
    /// Made on first use, for the syntax diff or not.
    marks: Option<OverviewMarks>,
    counts: Counts,
    note: Option<&'static str>,
    bytes: usize,
    /// Each hunk's heading as a piped diff wrote it. Set for a piped file,
    /// which has only its hunks: the text between them is unknown, so its
    /// gaps never open.
    headings: Option<Vec<Vec<u8>>>,
    /// One hunk holding a whole added or deleted file, whose `@@` row
    /// would say nothing a title doesn't.
    whole: bool,
    rendered: Option<Box<Rendered>>,
}

/// A file's syntax diff as displayed. Its hunks cover the same lines as
/// the line diff's, so a position stays valid across the toggle.
struct SyntaxDiff {
    /// Empty when the file has no syntax diff; the line diff shows then.
    hunks: Vec<Hunk>,
    views: Vec<HunkView>,
    note: Option<String>,
}

struct HunkView {
    /// The old line its heading is on, a `u32` like the body's lines: every
    /// hunk of every file held has a view, so its size counts.
    heading: Option<u32>,
    body: Vec<BodyLine>,
    /// Rows in `arrangement`, computed when the hunk is first drawn in it.
    rows: Vec<Row>,
    /// The rows that start a block of changed lines.
    starts: Vec<usize>,
    /// What `rows` and `starts` are for; `None` until the hunk is drawn.
    arrangement: Option<Arrangement>,
    /// The other arrangement's rows and starts, once the hunk was drawn in
    /// both (a resize across the split width, a layout toggle).
    other: Option<Box<(Vec<Row>, Vec<usize>)>>,
    /// Per line of the core hunk, once set.
    emphasis: Option<Vec<Emphasis>>,
    /// Set along with the emphasis, for highlighted files.
    syntax: Option<HunkSyntax>,
}

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
                arrangement: None,
                other: None,
                emphasis: None,
                syntax: None,
            }
        })
        .collect()
}

/// The gaps around `diff`'s hunks; none when it has no hunks.
fn gaps(diff: &FileDiff) -> Vec<Gap> {
    if diff.hunks.is_empty() {
        return Vec::new();
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
    fn new(mut diff: FileDiff, whitespace: Whitespace, highlight: Option<&Registry>) -> Self {
        // Whether a line is modified decides its color from the first frame
        // on, and the overview marks every hunk, shown or not.
        for hunk in &mut diff.hunks {
            diff::pair(&diff.old, &diff.new, hunk);
        }
        let hunks = views(&diff, &diff.hunks);
        let lines: usize = hunks.iter().map(|h| h.body.len()).sum();
        let mut counts = Counts::default();
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Added => counts.added += 1,
                LineKind::Removed => counts.removed += 1,
                LineKind::Context => {}
            }
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
                highlight.is_some_and(|grammars| {
                    !diff.hunks.is_empty() && highlighted(grammars, path, bytes)
                })
            })
            // Measured on kernel/sched/fair.c: a first parse grew the process
            // by about 19 bytes per byte of source, an upper bound.
            .map(|(_, bytes)| bytes.len() * 20)
            .sum();
        let gaps = gaps(&diff);
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
        Self {
            bytes: diff.old.bytes().len() + diff.new.bytes().len() + lines * 64 + syntax_bytes,
            syntax: (syntax_bytes > 0).then(SyntaxSlot::default),
            diff: Arc::new(diff),
            hunks,
            structural: None,
            gap_syntax: vec![None; gaps.len()],
            marks: None,
            gaps,
            segments,
            counts,
            note,
            headings: None,
            whole,
            rendered: None,
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

    /// Its added and removed lines. A rendered view counts the source
    /// lines it was drawn from.
    #[inline]
    #[must_use]
    pub fn counts(&self) -> Counts {
        self.counts
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

    /// The gaps, one before each hunk and one after the last; none for a
    /// file without hunks.
    #[inline]
    #[must_use]
    pub fn gaps(&self) -> &[Gap] {
        &self.gaps
    }

    /// The new-side lines of gap `gap`, for highlighting.
    #[must_use]
    pub fn gap_lines(&self, gap: usize) -> Range<usize> {
        self.gaps.get(gap).map_or(0..0, |g| g.new..g.new + g.len)
    }

    /// Whether hunk `hunk` of the line diff has no emphasis yet.
    #[inline]
    #[must_use]
    pub fn wants_emphasis(&self, hunk: usize) -> bool {
        self.hunks.get(hunk).is_some_and(|h| h.emphasis.is_none())
    }

    /// Whether gap `gap` is highlighted and has no syntax runs yet.
    #[inline]
    #[must_use]
    pub fn wants_gap_syntax(&self, gap: usize) -> bool {
        self.syntax.is_some() && self.gap_syntax.get(gap).is_some_and(Option::is_none)
    }

    /// Whether the syntax diff is worth computing: not yet set, and the
    /// file has hunks of its own text.
    #[must_use]
    pub fn wants_structural(&self) -> bool {
        self.structural.is_none()
            && !self.diff.hunks.is_empty()
            && !self.is_partial()
            && self.rendered.is_none()
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

    /// Stores gap `gap`'s syntax runs (see [`Prepare::gap_syntax`]), empty
    /// when there are none, if `diff` is this file's diff and the gap has
    /// none yet.
    pub fn set_gap_syntax(&mut self, diff: &Arc<FileDiff>, gap: usize, spans: Option<Spans>) {
        if !Arc::ptr_eq(&self.diff, diff) {
            return;
        }
        if let Some(slot @ None) = self.gap_syntax.get_mut(gap) {
            *slot = Some(spans.unwrap_or_default());
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
        self.structural = Some(SyntaxDiff { hunks, views, note });
        true
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
            Some(s) if structural && s.hunks.len() == self.hunks.len() => (&s.hunks, &s.views),
            _ => (&self.diff.hunks, &self.hunks),
        }
    }

    fn shown_hunks_mut(&mut self, structural: bool) -> (&[Hunk], &mut [HunkView]) {
        match &mut self.structural {
            Some(s) if structural && s.hunks.len() == self.hunks.len() => (&s.hunks, &mut s.views),
            _ => (&self.diff.hunks, &mut self.hunks),
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

    /// Builds hunk `h`'s rows and block starts in the arrangement `shape`
    /// picks, unless it has them; a second arrangement's count toward
    /// [`File::bytes`].
    fn ensure_rows(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        h: usize,
    ) {
        let (cores, views) = self.shown_hunks_mut(shape.structural);
        let (core, view) = (&cores[h], &mut views[h]);
        let arrangement = (shape.arrange)(&hunk_header(core, b""));
        match (view.arrangement, &mut view.other) {
            (Some(shown), _) if shown == arrangement => return,
            (Some(_), Some(other)) => {
                std::mem::swap(&mut view.rows, &mut other.0);
                std::mem::swap(&mut view.starts, &mut other.1);
                view.arrangement = Some(arrangement);
                return;
            }
            (Some(_), other @ None) => {
                *other = Some(Box::new((
                    std::mem::take(&mut view.rows),
                    std::mem::take(&mut view.starts),
                )));
            }
            (None, _) => {}
        }
        let body = &view.body;
        let kind = |i: usize| {
            let BodyLine { line, state } = body[i];
            let block_start =
                state != LineState::NoNewline && core.lines[line as usize].block_start;
            (state.kind(), block_start)
        };
        rows(arrangement, body.len(), kind, &mut view.rows);
        let changed = |row: &Row| match *row {
            Row::Line(i) | Row::Pair(Some(i), _) | Row::Pair(None, Some(i)) => {
                matches!(body[i].state.kind(), Kind::Removed | Kind::Added)
            }
            Row::Header | Row::Marker | Row::Pair(None, None) => false,
        };
        view.starts.clear();
        let mut before = false;
        for (r, row) in view.rows.iter().enumerate() {
            let now = changed(row);
            if now && !before {
                view.starts.push(r);
            }
            before = now;
        }
        // The first arrangement's rows are in the estimate the file was
        // built with; a second one's are on top of it.
        let second = view.other.is_some();
        let added =
            view.rows.capacity() * size_of::<Row>() + view.starts.capacity() * size_of::<usize>();
        view.arrangement = Some(arrangement);
        if second {
            self.bytes += added;
        }
    }

    /// Rows in part `part`.
    fn part_rows(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        part: usize,
    ) -> usize {
        match self.segment(part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(shape, h);
                self.shown_hunks(shape.structural).1[h].rows.len().max(1)
            }
            Some(Segment::Gap(g)) => self.shown(shape, g).rows(),
            Some(Segment::Start) | None => 1,
        }
    }

    /// The arrangement of part `part`'s rows; `None` for a part that isn't
    /// a hunk.
    fn arrangement(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        part: usize,
    ) -> Option<Arrangement> {
        let Some(Segment::Hunk(h)) = self.segment(part) else {
            return None;
        };
        self.ensure_rows(shape, h);
        self.shown_hunks(shape.structural).1[h].arrangement
    }

    /// Moves `pos` to the next row, offset 0; false at the file's end.
    fn next_row(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        pos: &mut Pos,
    ) -> bool {
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
    fn prev_row(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        pos: &mut Pos,
    ) -> bool {
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
    fn starts(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        part: usize,
    ) -> &[usize] {
        let Some(Segment::Hunk(h)) = self.segment(part) else {
            return &[];
        };
        self.ensure_rows(shape, h);
        &self.shown_hunks(shape.structural).1[h].starts
    }

    /// The first start of a block of changes after `from`.
    fn next_change(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        from: Pos,
    ) -> Option<Pos> {
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
    fn prev_change(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        from: Pos,
    ) -> Option<Pos> {
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
    fn edge(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        file: usize,
        last: bool,
    ) -> Option<Pos> {
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
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        file: usize,
        line: usize,
    ) -> Option<Pos> {
        let mut best = None;
        for part in 1..self.parts() {
            let Some(Segment::Hunk(h)) = self.segment(part) else {
                continue;
            };
            self.ensure_rows(shape, h);
            let (_, views) = self.shown_hunks(shape.structural);
            for &row in &views[h].starts {
                let pos = Pos {
                    file,
                    part,
                    row,
                    offset: 0,
                };
                best = Some(pos);
                if self.hunk_line(shape.structural, h, row) > line {
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
    /// first after it.
    fn line_of(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        pos: Pos,
    ) -> usize {
        let (part, from) = if pos.part == 0 {
            (1, 0)
        } else {
            (pos.part, pos.row)
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
    /// of hunk `h`. A removed line's is where it would be in the new file.
    fn hunk_line(&self, structural: bool, h: usize, from: usize) -> usize {
        let (cores, views) = self.shown_hunks(structural);
        let (core, view) = (&cores[h], &views[h]);
        let first_body = (from..view.rows.len())
            .find_map(|r| match view.rows[r] {
                Row::Line(i) | Row::Pair(_, Some(i)) | Row::Pair(Some(i), None) => Some(i),
                Row::Header | Row::Marker | Row::Pair(None, None) => None,
            })
            .unwrap_or(0);
        view.body
            .get(first_body)
            .map_or(1, |b| core.lines[b.line as usize].new + 1)
    }

    /// The gap `pos` is in and the gap line its row shows; a separator
    /// counts as the first line it hides.
    #[must_use]
    fn gap_anchor<A>(&self, shape: &Shape<'_, A>, pos: Pos) -> Option<(usize, usize)> {
        let Some(Segment::Gap(gap)) = self.segment(pos.part) else {
            return None;
        };
        Some((gap, self.shown(shape, gap).line_at(pos.row)))
    }

    /// The part showing gap `gap`.
    fn gap_part(&self, gap: usize) -> Option<usize> {
        Some(1 + self.segments.iter().position(|s| *s == Segment::Gap(gap))?)
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
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        file: usize,
        m: &Match,
    ) -> Option<Pos> {
        let line = m.line as usize;
        if m.side == Side::New
            && let Some(g) = self
                .gaps
                .iter()
                .position(|g| (g.new..g.new + g.len).contains(&line))
        {
            return Some(Pos {
                file,
                part: self.gap_part(g)?,
                row: self.shown(shape, g).row(line - self.gaps[g].new),
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
        let (core, view) = (&cores[h], &views[h]);
        let body = view.body.iter().position(|b| {
            let l = &core.lines[b.line as usize];
            b.state != LineState::NoNewline
                && match m.side {
                    Side::Old => l.kind == LineKind::Removed && l.old == line,
                    Side::New => l.kind != LineKind::Removed && l.new == line,
                }
        })?;
        let row = view.rows.iter().position(|r| match *r {
            Row::Line(i) => i == body,
            Row::Pair(old, new) => old == Some(body) || new == Some(body),
            Row::Header | Row::Marker => false,
        })?;
        Some(Pos {
            file,
            part,
            row,
            offset: 0,
        })
    }

    /// The old and new lines (0-based) the row at `pos` shows; `None` on a
    /// side without one, and on rows that aren't code.
    fn row_lines(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        pos: Pos,
    ) -> [Option<usize>; 2] {
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
                    Some(&Row::Line(i)) => line(i),
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

    /// Hunk `h`'s lines as shown (the syntax diff's with `structural`, when
    /// there is one), by line of its body.
    #[inline]
    #[must_use]
    pub fn lines(&self, structural: bool, h: usize) -> HunkLines<'_> {
        let (cores, views) = self.shown_hunks(structural);
        HunkLines {
            file: self,
            core: &cores[h],
            view: &views[h],
        }
    }

    /// Builds part `part`'s rows in the arrangement `shape` picks, when
    /// it's a hunk without them (see [`File::row`]).
    #[inline]
    fn ensure_part(
        &mut self,
        shape: &Shape<'_, impl Fn(&HunkHeader<'_>) -> Arrangement>,
        part: usize,
    ) {
        if let Some(Segment::Hunk(h)) = self.segment(part) {
            self.ensure_rows(shape, h);
        }
    }

    /// What the row at `pos` shows, once [`File::ensure_part`] built its
    /// part's rows; `None` for part 0 and past the last part. Its offset is
    /// ignored.
    fn row<A>(&self, shape: &Shape<'_, A>, pos: Pos) -> Option<Content<'_>> {
        let segment = self.segment(pos.part)?;
        let (cores, views) = self.shown_hunks(shape.structural);
        Some(match segment {
            Segment::Start => Content::Start(self.header(&cores[0], self.heading(views, 0))),
            Segment::Hunk(h) => {
                let lines = self.lines(shape.structural, h);
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
                        let raw = self.diff.new.line(new);
                        let line = Line {
                            state: LineState::Context,
                            old: Some(old + 1),
                            new: Some(new + 1),
                            text: display_text(raw.strip_suffix(b"\n").unwrap_or(raw)),
                            emphasis: &[],
                            syntax: self.gap_syntax[g].as_ref().map(|s| s.line(new)),
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
    /// down.
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
                    .line(pos.row)
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
}

/// The lines of a hunk as shown, each with its state, emphasis (once set)
/// and syntax runs, borrowed from the [`File`].
#[derive(Clone, Copy)]
pub struct HunkLines<'a> {
    file: &'a File,
    core: &'a Hunk,
    view: &'a HunkView,
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
        let shown = diff_line(
            &self.file.diff,
            core_line,
            state,
            emphasis,
            self.view.syntax.as_ref(),
        );
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

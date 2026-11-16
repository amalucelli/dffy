//! The diff, one file at a time: its parts, hunks and the gaps of unchanged
//! lines between them. The file's name, and the hunk or function the top
//! row is in, are the viewer's to show in its frame; nothing here takes a
//! row for them. Scrolling stops at the file's edges; each file keeps its
//! own place while others are shown. Files arrive from the enumeration
//! with no parts and get them when a worker has diffed them. The position
//! is anchored to a row of a part, not a screen row, so nothing ever
//! measures a whole file: a diff loading, a resize or a layout toggle only
//! touch the rows drawn.
//!
//! A gap shows as a separator row (`⋯ 40 unchanged lines`) with whatever
//! the user revealed of it above and below. Revealing more, or switching a
//! file to its full text, changes only the rows of its gaps, so the hunks'
//! rows and every position in them stay put.
//!
//! Rows come from the same [`Layout`] code print mode uses, so both
//! frontends draw identical lines, less print's file headers.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::diff::{Emphasis, Hunk, LineKind, Whitespace};
use dffy_core::git::{FileChange, FileDiff, Status, Update};
use dffy_core::highlight::{FileSyntax, HunkSyntax, MAX_HIGHLIGHT_BYTES, Registry};
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::{
    Arrangement, Attributes, FileStatus, Heading, HunkHeader, Kind, Line, LineState, Row, Spans,
    display_text, heading_text, no_newline, rows,
};

use dffy_core::patch::{self, Commit};

use super::piped::PipedFile;
use crate::pager;
use crate::print;
use crate::render::{Canvas, Layout};
use crate::term::Grid;
use crate::theme::{Ink, Role, Style, Theme};

pub(crate) mod rendered;

/// A file's highlighting, prepared by whichever worker first needs it;
/// workers asking at the same time wait for that one.
pub(crate) type SyntaxSlot = Arc<OnceLock<FileSyntax>>;

/// Lines one reveal adds to a gap; a gap of up to twice this opens whole.
const REVEAL_LINES: usize = 20;

/// Rows kept above a file's first change when it opens there.
const OPEN_CONTEXT: isize = 3;

/// A file diff prepared for display. Built on a worker.
pub(crate) struct Loaded {
    pub diff: Arc<FileDiff>,
    /// `None` when highlighting is off or neither side has a known
    /// language.
    pub syntax: Option<SyntaxSlot>,
    hunks: Vec<HunkView>,
    /// The syntax diff, once a worker has computed it.
    structural: Option<SyntaxDiff>,
    /// The unchanged lines before each hunk, then those after the last.
    /// The syntax diff's hunks cover the same lines, so these serve both.
    gaps: Vec<GapSpan>,
    /// The file's parts after its header, in order.
    segments: Vec<Segment>,
    /// The new side's syntax runs over each gap, once a worker has them.
    gap_syntax: Vec<Option<Spans>>,
    /// Changed lines for the overview column, made on first use, for the
    /// syntax diff or not.
    marks: Option<OverviewMarks>,
    pub added: usize,
    pub removed: usize,
    note: Option<&'static str>,
    /// Approximate memory held, for the cache cap.
    pub bytes: usize,
    /// Each hunk's heading as a piped diff wrote it. Set for a piped file,
    /// which has only its hunks: the text between them is unknown, so its
    /// gaps never open and nothing searches it.
    headings: Option<Vec<Vec<u8>>>,
    /// One hunk holding a whole added or deleted file, whose `@@` row
    /// would say nothing the title doesn't.
    whole: bool,
    /// Set for a rendered view (see [`rendered`]), whose diff is of
    /// rendered text.
    rendered: Option<Box<rendered::Rendered>>,
}

/// The overview's marks, for the syntax diff or not and with modified
/// lines or not.
type OverviewMarks = (bool, bool, Vec<(usize, Mark)>);

/// A stretch of unchanged lines: where it starts on each side (0-based)
/// and how long it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GapSpan {
    old: usize,
    new: usize,
    len: usize,
}

/// A changed line in the overview column, most visible last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mark {
    Moved,
    Modified,
    Removed,
    Added,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Segment {
    /// The first hunk's `@@` line, when no gap comes before it.
    Start,
    Gap(usize),
    Hunk(usize),
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
    /// hunk of every file loaded has a view, so its size counts.
    heading: Option<u32>,
    body: Vec<BodyLine>,
    /// Rows in `arrangement`, computed when the hunk is first drawn in it.
    rows: Vec<Row>,
    /// The rows that start a block of changed lines, for `n` and `N`.
    starts: Vec<usize>,
    /// What `rows` and `starts` are for; `None` until the hunk is drawn.
    arrangement: Option<Arrangement>,
    /// The other arrangement's rows and starts, once the hunk was drawn in
    /// both (a resize across the split width, a layout toggle).
    other: Option<Box<(Vec<Row>, Vec<usize>)>>,
    /// Per line of the core hunk, once a worker has computed it.
    emphasis: Option<Vec<Emphasis>>,
    /// Computed along with the emphasis, for highlighted files.
    syntax: Option<HunkSyntax>,
}

/// A displayed line of a hunk: a line of the core hunk, or the
/// no-newline marker after it, with what it is.
#[derive(Debug, Clone, Copy)]
struct BodyLine {
    line: u32,
    state: LineState,
}

// A hunk's body has a line each: it stays as small as its index.
const _: () = assert!(size_of::<BodyLine>() == 8);

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
fn gaps(diff: &FileDiff) -> Vec<GapSpan> {
    if diff.hunks.is_empty() {
        return Vec::new();
    }
    let (mut old, mut new) = (0, 0);
    let mut gaps = Vec::with_capacity(diff.hunks.len() + 1);
    for h in &diff.hunks {
        gaps.push(GapSpan {
            old,
            new,
            len: h.old_start.saturating_sub(old),
        });
        old = h.old_start + h.old_len;
        new = h.new_start + h.new_len;
    }
    gaps.push(GapSpan {
        old,
        new,
        len: diff.old.len().saturating_sub(old),
    });
    gaps
}

impl Loaded {
    /// `highlight` has the grammars to highlight with; `None` leaves the
    /// file plain.
    pub(crate) fn new(
        mut diff: FileDiff,
        whitespace: Whitespace,
        highlight: Option<&Registry>,
    ) -> Self {
        // Here, on the worker, rather than with the emphasis: whether a
        // line is modified decides its color from the first frame on, and
        // the overview marks every hunk, shown or not.
        for hunk in &mut diff.hunks {
            dffy_core::diff::pair(&diff.old, &diff.new, hunk);
        }
        let hunks = views(&diff, &diff.hunks);
        let lines: usize = hunks.iter().map(|h| h.body.len()).sum();
        let (mut added, mut removed) = (0, 0);
        for line in diff.hunks.iter().flat_map(|h| &h.lines) {
            match line.kind {
                LineKind::Added => added += 1,
                LineKind::Removed => removed += 1,
                LineKind::Context => {}
            }
        }
        let note = print::note(&diff).or_else(|| {
            print::whitespace_only(&diff, whitespace).then_some("only whitespace changed")
        });
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
            added,
            removed,
            note,
            headings: None,
            whole,
            rendered: None,
        }
    }

    /// A piped file's diff (see [`super::piped::PipedFile::diff`]), with
    /// its hunks' headings. Not highlighted: hunks alone parse poorly.
    pub(crate) fn piped(diff: FileDiff, headings: Vec<Vec<u8>>) -> Self {
        let mut loaded = Self::new(diff, Whitespace::Exact, None);
        loaded.bytes += headings.iter().map(|h| h.len() + 24).sum::<usize>();
        loaded.headings = Some(headings);
        loaded
    }

    /// Whether this is a piped file, which has only its hunks.
    pub(crate) fn is_partial(&self) -> bool {
        self.headings.is_some()
    }

    /// Hunk `h`'s heading: git's, for a piped file, else found above it.
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
    fn header<'a>(&self, core: &Hunk, heading: &'a [u8]) -> HunkHeader<'a> {
        match &self.rendered {
            Some(rendered) => rendered.header(core, heading),
            None => print::hunk_header(core, heading),
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

    /// Whether a worker should syntax-diff this file.
    pub(crate) fn wants_structural(&self) -> bool {
        self.structural.is_none()
            && !self.diff.hunks.is_empty()
            && !self.is_partial()
            && self.rendered.is_none()
    }

    /// The width a rendered view was drawn for; `None` for a line diff.
    pub(crate) fn rendered_width(&self) -> Option<usize> {
        self.rendered.as_ref().map(|r| r.width)
    }

    /// The hunks to show and their views: the syntax diff's when it's
    /// asked for and there is one.
    fn shown(&self, structural: bool) -> (&[Hunk], &[HunkView]) {
        match &self.structural {
            Some(s) if structural && s.hunks.len() == self.hunks.len() => (&s.hunks, &s.views),
            _ => (&self.diff.hunks, &self.hunks),
        }
    }

    fn shown_mut(&mut self, structural: bool) -> (&[Hunk], &mut [HunkView]) {
        match &mut self.structural {
            Some(s) if structural && s.hunks.len() == self.hunks.len() => (&s.hunks, &mut s.views),
            _ => (&self.diff.hunks, &mut self.hunks),
        }
    }

    /// What the status bar says about the syntax diff of this file.
    pub(crate) fn structural_note(&self) -> Option<&str> {
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

    /// Lines the overview column scales to: the new side's, or the old
    /// side's for a deleted file.
    fn overview_lines(&self) -> usize {
        if self.diff.new.is_empty() {
            self.diff.old.len()
        } else {
            self.diff.new.len()
        }
        .max(1)
    }

    /// The changed lines of the shown hunks by position (see
    /// [`Loaded::overview_lines`]), computed once per diff and view;
    /// paired lines are marked modified when `modified` lines show so.
    fn marks(&mut self, structural: bool, modified: bool) -> &[(usize, Mark)] {
        if self
            .marks
            .as_ref()
            .is_none_or(|m| (m.0, m.1) != (structural, modified))
        {
            let deleted = self.diff.new.is_empty();
            let (cores, views) = self.shown(structural);
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

    /// The new-side lines of gap `gap`, for highlighting.
    pub(crate) fn gap_lines(&self, gap: usize) -> Range<usize> {
        self.gaps.get(gap).map_or(0..0, |g| g.new..g.new + g.len)
    }
}

pub(crate) enum State {
    Pending,
    Loaded(Box<Loaded>),
    Failed(String),
}

/// A file's rendered view beside its line diff. Whichever of the two shows
/// is the entry's [`State::Loaded`]; this holds the other. Entries hold it
/// boxed, and only once a rendering was asked for: the tree walks every
/// entry each frame, and a file that never renders costs it a pointer.
pub(crate) enum Render {
    /// The rendered view shows; this is the line diff.
    Shown(Box<Loaded>),
    /// The line diff shows; this is the rendered view, kept for `r`.
    Hidden(Box<Loaded>),
    /// The renderer failed on this diff, for this reason. It isn't asked
    /// again until the diff changes or rendering is turned on again.
    Failed(String),
}

impl Render {
    fn bytes(&self) -> usize {
        match self {
            Self::Shown(loaded) | Self::Hidden(loaded) => loaded.bytes,
            Self::Failed(_) => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Counts {
    pub added: usize,
    pub removed: usize,
}

/// Where an entry of the document comes from.
pub(crate) enum Source {
    /// A file a worker diffs from the repository.
    Repo,
    /// A file of a piped diff.
    Piped(Arc<PipedFile>),
    /// A commit's header, or text from a piped diff: no diff of its own. A
    /// commit's files are the entries after it, up to the next commit.
    Commit(Arc<Commit>),
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one file, not a state machine"
)]
pub(crate) struct Entry {
    pub change: FileChange,
    pub source: Source,
    pub state: State,
    /// `None` until a rendering is asked for, and again once the diff
    /// it was of is replaced.
    pub render: Option<Box<Render>>,
    /// The file changed since its diff was loaded. The old diff stays on
    /// screen, keeping the position in it, until the new one arrives.
    pub stale: bool,
    /// Kept apart from the diff, which the cache cap may drop, so the file
    /// tree never loses a count it has shown.
    pub counts: Option<Counts>,
    /// The file changed since `counts` were taken; they show until new
    /// ones arrive.
    pub recount: bool,
    /// Marked viewed in the file tree.
    pub viewed: bool,
    /// Lines revealed from the top and bottom edge of each gap. Kept apart
    /// from the diff so they outlive the cache cap.
    reveal: Vec<(usize, usize)>,
    /// Where the file was left when another was shown.
    saved: Option<Pos>,
    /// Shown with a diff this session: opened at its first change, or left
    /// where it was since. Diffs reloaded later (a refresh, the cache cap)
    /// never move the view again.
    placed: bool,
    /// The frame that last drew this file, for evicting the least recent.
    last_drawn: u64,
    /// Unique to this entry and its content: new whenever the file may
    /// have changed. A worker's result is taken only for the version it
    /// was asked for, wherever the entry has moved in the list since.
    pub version: u64,
}

/// A version no entry has had (see [`Entry::version`]).
fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Entry {
    fn new(change: FileChange, source: Source) -> Self {
        let counts = match &source {
            Source::Repo => None,
            Source::Piped(file) => Some(file.counts),
            Source::Commit(_) => Some(Counts::default()),
        };
        let mut entry = Self {
            change,
            source,
            state: State::Pending,
            render: None,
            stale: false,
            counts,
            recount: false,
            viewed: false,
            reveal: Vec::new(),
            saved: None,
            placed: false,
            last_drawn: 0,
            version: next_version(),
        };
        // A sheet is never diffed, so nothing would place it.
        entry.placed = entry.is_sheet();
        entry
    }

    /// Whether a worker should diff this file.
    pub(crate) fn wants_diff(&self) -> bool {
        !self.is_sheet() && (self.stale || matches!(self.state, State::Pending))
    }

    /// A commit's header or a piped text block, not a file.
    pub(crate) fn is_commit(&self) -> bool {
        matches!(self.source, Source::Commit(_))
    }

    /// Shown as rows of text rather than hunks: a commit, or a piped
    /// combined diff, which is shown as it came.
    fn is_sheet(&self) -> bool {
        match &self.source {
            Source::Commit(_) => true,
            Source::Piped(file) => file.combined.is_some(),
            Source::Repo => false,
        }
    }

    /// The status to show: a piped file's is its patch header's.
    pub(crate) fn status(&self) -> FileStatus {
        match &self.source {
            Source::Piped(file) => file.status,
            Source::Repo | Source::Commit(_) => print::file_status(&self.change.status),
        }
    }

    /// Whether the background pass should count this file's lines.
    pub(crate) fn wants_counts(&self) -> bool {
        self.counts.is_none() || self.recount
    }

    /// The line diff to render, when this file has one and no rendering
    /// or failure for it yet. A piped file has only its hunks: nothing to
    /// render.
    pub(crate) fn wants_render(&self) -> Option<&Arc<FileDiff>> {
        match (&self.state, &self.render, &self.source) {
            (State::Loaded(loaded), None, Source::Repo) if !self.stale => Some(&loaded.diff),
            _ => None,
        }
    }

    fn render_bytes(&self) -> usize {
        self.render.as_deref().map_or(0, Render::bytes)
    }

    /// Swaps the rendered view and the line diff, when it has both.
    fn swap_render(&mut self) {
        let Some(render) = self.render.take() else {
            return;
        };
        self.render = Some(Box::new(match (*render, &mut self.state) {
            (Render::Shown(mut other), State::Loaded(shown)) => {
                std::mem::swap(&mut other, shown);
                Render::Hidden(other)
            }
            (Render::Hidden(mut other), State::Loaded(shown)) => {
                std::mem::swap(&mut other, shown);
                Render::Shown(other)
            }
            (render, _) => render,
        }));
        if let State::Loaded(loaded) = &self.state {
            self.reveal = vec![(0, 0); loaded.gaps.len()];
        }
        self.saved = None;
        self.placed = false;
    }

    /// Puts the line diff back in [`Entry::state`] and drops the rendered
    /// view; returns the bytes freed.
    fn drop_render(&mut self) -> usize {
        if matches!(self.render.as_deref(), Some(Render::Shown(_))) {
            self.swap_render();
        }
        let freed = self.render_bytes();
        self.render = None;
        freed
    }
}

/// How much of a gap shows: lines from its top edge, lines from its bottom
/// edge, and a separator row for the rest when any is hidden.
#[derive(Debug, Clone, Copy)]
struct Shown {
    top: usize,
    bottom: usize,
    len: usize,
}

impl Shown {
    fn hidden(self) -> usize {
        self.len - self.top - self.bottom
    }

    fn separator(self) -> usize {
        usize::from(self.hidden() > 0)
    }

    fn rows(self) -> usize {
        self.top + self.separator() + self.bottom
    }

    /// The gap line row `row` shows; `None` for the separator.
    fn line(self, row: usize) -> Option<usize> {
        if row < self.top {
            Some(row)
        } else if self.hidden() > 0 && row == self.top {
            None
        } else {
            Some(self.len - self.bottom + row - self.top - self.separator())
        }
    }

    /// The row showing gap line `line`, or the separator when it's hidden.
    fn row(self, line: usize) -> usize {
        if line < self.top {
            line
        } else if line >= self.len - self.bottom {
            self.top + self.separator() + line - (self.len - self.bottom)
        } else {
            self.top
        }
    }
}

/// A place in a file: part `n` is its segment `n - 1`, and part 0 holds a
/// sheet's rows ([`Entry::is_sheet`]), or the blank row of a file that has
/// no segments (not diffed yet, binary, unchanged); a file with segments is
/// never at part 0. Ordered as in the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) struct Pos {
    pub file: usize,
    pub part: usize,
    pub row: usize,
    /// A sub-row inside the row, for a row drawn on several screen rows (a
    /// wrapped line). Going to a row by what it shows (a change, a match, a
    /// revealed line, the next row) sets it to 0, and a saved place keeps
    /// it. Only the screen code, which knows how many screen rows a row
    /// takes, gives it any other value or reads it: scrolling up into a
    /// row lands on its last one, and a row whose height shrank clamps it.
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Side {
    Old,
    New,
}

impl Side {
    fn index(self) -> usize {
        match self {
            Self::Old => 0,
            Self::New => 1,
        }
    }

    pub(crate) fn other(self) -> Self {
        match self {
            Self::Old => Self::New,
            Self::New => Self::Old,
        }
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

/// What `/` in the diff looks for: a literal string, case ignored unless
/// it has a capital letter (smart case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Needle {
    pub text: String,
    fold: bool,
}

impl Needle {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            fold: !text.chars().any(char::is_uppercase),
        }
    }

    /// Where it occurs in `line`, not overlapping, as byte ranges.
    pub(crate) fn find_all(&self, line: &[u8]) -> Vec<Range<usize>> {
        let needle = self.text.as_bytes();
        let mut out = Vec::new();
        if needle.is_empty() || needle.len() > line.len() {
            return out;
        }
        let mut i = 0;
        while i + needle.len() <= line.len() {
            let window = &line[i..i + needle.len()];
            let hit = if self.fold {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if hit {
                out.push(i..i + needle.len());
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

/// A match of the search: the side and line (0-based) it's on, and its
/// bytes within the line. Kept small: a one-letter search of a large file
/// has a match on most lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Match {
    pub side: Side,
    pub line: u32,
    pub start: u32,
    pub end: u32,
}

/// A file's matches in the order the diff shows them (unified), and for
/// each side, the matches on it by line, to find a row's matches while
/// drawing.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Matches {
    pub list: Vec<Match>,
    by_side: [Vec<u32>; 2],
}

impl Matches {
    /// The matches on `line` of `side`, as indices into `list`.
    fn on(&self, side: Side, line: usize) -> impl Iterator<Item = usize> + '_ {
        let ids = &self.by_side[side.index()];
        let line_of = |i: &u32| self.list[*i as usize].line as usize;
        let from = ids.partition_point(|i| line_of(i) < line);
        ids[from..]
            .iter()
            .take_while(move |i| line_of(i) == line)
            .map(|&i| i as usize)
    }
}

/// Every match of `needle` on the lines a file's diff shows: removed
/// lines' old text, added and unchanged lines' new text (an unchanged
/// line counts once), in the order the diff shows them. `hidden` has the
/// new lines each gap of unchanged lines hides under its separator, by
/// gap; without it, only the hunks' lines count. `None` once `cancelled`
/// says so: it's asked every few thousand lines, so a search that's been
/// typed past stops.
pub(crate) fn find(
    diff: &FileDiff,
    hidden: Option<&[Range<usize>]>,
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    let mut matches = Matches::default();
    let mut lines = 0usize;
    let mut scan = |side: Side, line: usize| {
        if lines.is_multiple_of(4096) && cancelled() {
            return false;
        }
        lines += 1;
        let text = match side {
            Side::Old => diff.old.line(line),
            Side::New => diff.new.line(line),
        };
        let text = display_text(text.strip_suffix(b"\n").unwrap_or(text));
        let line = u32::try_from(line).unwrap_or(u32::MAX);
        for bytes in needle.find_all(text) {
            let id = u32::try_from(matches.list.len()).unwrap_or(u32::MAX);
            matches.by_side[side.index()].push(id);
            matches.list.push(Match {
                side,
                line,
                start: u32::try_from(bytes.start).unwrap_or(u32::MAX),
                end: u32::try_from(bytes.end).unwrap_or(u32::MAX),
            });
        }
        true
    };
    for (g, gap) in gaps(diff).iter().enumerate() {
        if let Some(hidden) = hidden {
            let skip = hidden.get(g).cloned().unwrap_or(gap.new..gap.new + gap.len);
            for line in (gap.new..skip.start).chain(skip.end..gap.new + gap.len) {
                if !scan(Side::New, line) {
                    return None;
                }
            }
        }
        let Some(hunk) = diff.hunks.get(g) else {
            continue;
        };
        for l in &hunk.lines {
            let more = match l.kind {
                LineKind::Removed => scan(Side::Old, l.old),
                LineKind::Added | LineKind::Context => scan(Side::New, l.new),
            };
            if !more {
                return None;
            }
        }
    }
    Some(matches)
}

/// The search's matches in one file, for drawing them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Marks {
    pub file: usize,
    /// The lines that were hidden when they were found (see
    /// [`Doc::hidden_lines`]): revealing more means searching again.
    pub hidden: Vec<Range<usize>>,
    pub matches: Arc<Matches>,
    /// The one `n`/`N` went to.
    pub current: Option<usize>,
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

/// What a double click groups characters by.
#[derive(PartialEq)]
enum Class {
    Word,
    Blank,
    Other,
}

/// The word around byte `at` of `text`: letters, digits and `_`, or a run
/// of blanks, or the one other character there.
fn word(text: &[u8], at: usize) -> (usize, usize) {
    let s = String::from_utf8_lossy(text);
    // Lossy text has the same offsets when the line is valid UTF-8; a line
    // that isn't selects by bytes.
    if s.len() != text.len() {
        return (at, (at + 1).min(text.len()));
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c == ' ' || c == '\t' {
            Class::Blank
        } else {
            Class::Other
        }
    };
    let Some(here) = s.get(at..).and_then(|rest| rest.chars().next()) else {
        return (at, at);
    };
    let kind = class(here);
    if kind == Class::Other {
        return (at, at + here.len_utf8());
    }
    let start = s[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind)
        .last()
        .map_or(at, |(i, _)| i);
    let end = s[at..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind)
        .map_or(s.len(), |(i, _)| at + i);
    (start, end)
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

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent view modes, not a state machine"
)]
pub(crate) struct Doc {
    pub files: Vec<Entry>,
    /// The entries that are commits, in order.
    pub commits: Vec<usize>,
    /// The file shown and its row at the top of the screen.
    pub pos: Pos,
    pub layout: Layout,
    /// Screen rows available for the document.
    pub height: usize,
    loaded_bytes: usize,
    scratch: Canvas,
    /// For rows rendered again to find which byte a cell shows.
    mapped: Canvas,
    /// The diff search's matches in the file they're drawn for.
    pub search: Option<Marks>,
    /// Hunks drawn in the last frame that still lack emphasis.
    pub missing_emphasis: Vec<(usize, usize)>,
    /// Gaps with lines drawn in the last frame that still lack syntax runs.
    pub missing_gap_syntax: Vec<(usize, usize)>,
    /// Show syntax diffs where they're ready.
    pub structural: bool,
    /// Show rendered views where they're ready (see [`Render`]).
    pub rendered: bool,
    /// Files show their full text (`X`), all but piped ones.
    pub full: bool,
    /// The last column of the drawing area is kept for the overview of the
    /// file at the top.
    pub overview: bool,
    /// Separator rows drawn in the last frame, by screen row from the top
    /// of the document.
    separators: Vec<(usize, Pos)>,
    /// The separator `x` reveals: the first one on screen.
    target: Option<Pos>,
    /// The change `n` or `N` last went to, and the position the view took.
    landed: Option<(Pos, Pos)>,
    /// `n` or `N` opened a file not diffed yet, at this position; once its
    /// diff arrives, and if the view is still there, the view goes to its
    /// first change (false) or its last (true). Any input clears it.
    pub pending_land: Option<(Pos, bool)>,
    /// The logical row drawn on each screen row of the pane in the last
    /// frame; `None` for empty rows.
    drawn: Vec<Option<Pos>>,
    pub selection: Option<Selection>,
    /// The last heading search for the title's context: the diff, the line it
    /// searched up from, and the search, which continues forward from
    /// there for the next line down.
    heading: Option<(Arc<FileDiff>, usize, Heading)>,
}

const NO_EMPHASIS: &[Range<usize>] = &[];

impl Doc {
    /// An empty document. Only watch mode shows one; everything that moves
    /// the position does nothing until a file arrives.
    pub(crate) fn new(layout: Layout, height: usize) -> Self {
        Self {
            files: Vec::new(),
            commits: Vec::new(),
            pos: Pos::default(),
            layout,
            height,
            loaded_bytes: 0,
            scratch: Canvas::default(),
            mapped: Canvas::mapped(),
            search: None,
            missing_emphasis: Vec::new(),
            missing_gap_syntax: Vec::new(),
            structural: false,
            rendered: false,
            full: false,
            overview: false,
            separators: Vec::new(),
            target: None,
            landed: None,
            pending_land: None,
            drawn: Vec::new(),
            selection: None,
            heading: None,
        }
    }

    /// Keeps the position inside the document after rows changed under it
    /// (a diff arrived, the layout changed).
    pub(crate) fn normalize(&mut self) {
        if self.files.is_empty() {
            self.pos = Pos::default();
            return;
        }
        let mut pos = self.pos;
        let first = self.first_part(pos.file);
        if pos.part < first {
            pos = Pos {
                file: pos.file,
                part: first,
                ..Pos::default()
            };
        }
        pos.part = pos.part.min(self.parts(pos.file) - 1);
        pos.row = pos.row.min(self.rows(pos.file, pos.part) - 1);
        pos.offset = pos.offset.min(self.height(pos) - 1);
        self.pos = pos;
        self.clamp();
    }

    /// Marks every file to be diffed and counted again, the settings having
    /// changed; what's shown stays until the new diffs arrive.
    pub(crate) fn restale(&mut self) {
        for entry in &mut self.files {
            if !matches!(entry.state, State::Pending) {
                entry.stale = true;
            }
            entry.recount = true;
            entry.version = next_version();
        }
    }

    pub(crate) fn push(&mut self, change: FileChange, source: Source) {
        if matches!(source, Source::Commit(_)) {
            self.commits.push(self.files.len());
        }
        self.files.push(Entry::new(change, source));
    }

    /// The commit entry `file` is under, as an index into `commits`.
    pub(crate) fn commit_of(&self, file: usize) -> Option<usize> {
        self.commits.partition_point(|&c| c <= file).checked_sub(1)
    }

    /// The files of the commit at entry `commit`.
    pub(crate) fn commit_files(&self, commit: usize) -> Range<usize> {
        let next = self.commits.partition_point(|&c| c <= commit);
        commit + 1..self.commits.get(next).copied().unwrap_or(self.files.len())
    }

    /// Commits at the head of the list (`dffy show`'s), which stay first
    /// when the files after them are sorted or replaced.
    fn head(&self) -> usize {
        self.files.iter().take_while(|e| e.is_commit()).count()
    }

    /// Puts the files in path order, the file tree's order, keeping the
    /// position on the same file. The enumeration lists untracked files
    /// after tracked ones, so they need moving into place. Returns whether
    /// any file moved.
    pub(crate) fn sort_by_path(&mut self) -> bool {
        let head = self.head();
        let files = &mut self.files[head..];
        if files.is_sorted_by(|a, b| a.change.path <= b.change.path) {
            return false;
        }
        let anchor = (self.pos.file >= head)
            .then(|| files.get(self.pos.file - head))
            .flatten()
            .map(|e| e.change.path.clone());
        files.sort_by(|a, b| a.change.path.cmp(&b.change.path));
        if let Some(file) = anchor.and_then(|path| files.iter().position(|e| e.change.path == path))
        {
            self.pos.file = head + file;
        }
        self.forget_positions();
        true
    }

    /// Drops what holds positions in files that moved or changed.
    fn forget_positions(&mut self) {
        self.pending_land = None;
        self.selection = None;
        self.landed = None;
    }

    /// Takes the refreshed file list, in path order. Files still listed keep
    /// their diffs, counts, viewed marks and revealed lines; diffs and
    /// counts are marked for redoing when they may have changed. The
    /// position stays on the same file, or moves to the one in its place
    /// when it's gone.
    pub(crate) fn replace(&mut self, update: &Update) {
        let head: Vec<Entry> = self.files.drain(..self.head()).collect();
        let on_head = self.pos.file < head.len();
        let kept = self.pos;
        self.pos.file = self.pos.file.saturating_sub(head.len());
        if let Some(s) = &mut self.selection {
            s.anchor.file -= head.len();
            s.cursor.file -= head.len();
        }
        self.replace_files(update);
        let n = head.len();
        self.files.splice(0..0, head);
        if on_head {
            self.pos = kept;
        } else {
            self.pos.file += n;
        }
        if let Some(s) = &mut self.selection {
            s.anchor.file += n;
            s.cursor.file += n;
        }
        self.normalize();
    }

    fn replace_files(&mut self, update: &Update) {
        let anchor = self.files.get(self.pos.file).map(|e| e.change.path.clone());
        let selected = self.selection.take().map(|s| {
            let path = self.files[s.anchor.file].change.path.clone();
            (s, path)
        });
        self.forget_positions();
        // The rename pass turns an addition into a rename and drops the
        // deletion it pairs with; a position on either stays on the pair.
        let renamed_from = |c: &FileChange, path: &BString| matches!(&c.status, Status::Renamed { from, .. } if from == path);
        let mut old: HashMap<BString, Entry> = std::mem::take(&mut self.files)
            .into_iter()
            .map(|e| (e.change.path.clone(), e))
            .collect();
        self.files = update
            .files
            .iter()
            .map(|change| match old.remove(&change.path) {
                Some(mut entry) => {
                    if entry.change != *change || update.is_dirty(change) {
                        entry.stale = true;
                        entry.recount = true;
                        entry.version = next_version();
                    }
                    entry.change = change.clone();
                    entry
                }
                None => Entry::new(change.clone(), Source::Repo),
            })
            .collect();
        self.files.sort_by(|a, b| a.change.path.cmp(&b.change.path));
        for gone in old.into_values() {
            self.loaded_bytes -= gone.render_bytes();
            if let State::Loaded(loaded) = gone.state {
                self.loaded_bytes -= loaded.bytes;
            }
        }
        if let Some(file) = anchor.and_then(|path| {
            let files = &self.files;
            files
                .iter()
                .position(|e| e.change.path == path)
                .or_else(|| files.iter().position(|e| renamed_from(&e.change, &path)))
        }) {
            self.pos.file = file;
        } else {
            let file = self.pos.file.min(self.files.len().saturating_sub(1));
            self.pos = Pos {
                file,
                ..self
                    .files
                    .get(file)
                    .and_then(|e| e.saved)
                    .unwrap_or_default()
            };
            // Shown in its place: opened now if it can be, so a later
            // reload of it doesn't move the view.
            if let Some(entry) = self.files.get(file)
                && !entry.placed
                && !matches!(entry.state, State::Pending)
            {
                self.normalize();
                self.place(file, false);
            }
        }
        // A selection outlives other files changing, not its own.
        if let Some((mut selection, path)) = selected
            && let Some(file) = self.files.iter().position(|e| e.change.path == path)
            && !self.files[file].stale
        {
            selection.anchor.file = file;
            selection.cursor.file = file;
            self.selection = Some(selection);
        }
        self.normalize();
    }

    /// Stores a worker's diff for a file that is pending or stale. A file
    /// whose hunks moved loses its revealed lines; one reloaded after the
    /// cache cap dropped it keeps them.
    pub(crate) fn load(&mut self, file: usize, result: Result<Box<Loaded>, String>) -> bool {
        let Some(entry) = self.files.get_mut(file) else {
            return false;
        };
        if !entry.wants_diff() {
            return false;
        }
        if self
            .selection
            .as_ref()
            .is_some_and(|s| s.anchor.file == file)
        {
            self.selection = None;
        }
        // A rendering is of the diff being replaced.
        let rendered = matches!(entry.render.as_deref(), Some(Render::Shown(_)));
        self.loaded_bytes -= entry.drop_render();
        let mut keep_reveal = !entry.stale && !rendered;
        if let State::Loaded(old) = &entry.state {
            self.loaded_bytes -= old.bytes;
            if let Ok(new) = &result {
                keep_reveal |= old.gaps == new.gaps;
            }
        }
        if !keep_reveal {
            entry.reveal.clear();
        }
        entry.stale = false;
        entry.recount = false;
        entry.state = match result {
            Ok(loaded) => {
                self.loaded_bytes += loaded.bytes;
                entry.counts = Some(Counts {
                    added: loaded.added,
                    removed: loaded.removed,
                });
                entry.reveal.resize(loaded.gaps.len(), (0, 0));
                State::Loaded(loaded)
            }
            Err(e) => {
                entry.counts.get_or_insert_default();
                State::Failed(e)
            }
        };
        let pending = self.pending_land.filter(|(at, _)| at.file == file);
        if pending.is_some() {
            self.pending_land = None;
        }
        if self.pos.file == file && !self.files[file].placed {
            let landing = pending.is_some_and(|(at, _)| at == self.pos);
            self.place(file, landing);
        } else if let Some((at, last)) = pending
            && self.pos == at
        {
            self.land_edge(file, last);
        }
        // Off the blank row it had without segments.
        if self.pos.file == file {
            self.normalize();
        }
        true
    }

    /// Opens `file`, shown and diffed for the first time, at its first
    /// change with [`OPEN_CONTEXT`] rows above it; at its top when the
    /// change shows on the first screen from there anyway. With `landing`
    /// (`n` opened it) the next `n` goes on past that change either way.
    fn place(&mut self, file: usize, landing: bool) {
        self.files[file].placed = true;
        let top = Pos {
            file,
            part: self.first_part(file),
            ..Pos::default()
        };
        self.pos = top;
        let Some((part, row)) = self.edge(file, false) else {
            return;
        };
        let change = Pos {
            file,
            part,
            row,
            offset: 0,
        };
        let context = OPEN_CONTEXT.unsigned_abs();
        let limit = self.height.saturating_sub(context).max(context + 1);
        let (mut pos, mut rows) = (top, 0);
        while rows < limit {
            if (pos.part, pos.row) == (part, row) {
                if landing {
                    self.landed = Some((change, top));
                }
                return;
            }
            rows += self.height(pos);
            if !self.next_row(&mut pos) {
                return;
            }
        }
        self.pos = change;
        self.scroll(-OPEN_CONTEXT);
        self.landed = Some((change, self.pos));
    }

    /// Whether `file` is still at `version`: work asked for it then is
    /// current.
    pub(crate) fn is_current(&self, file: usize, version: u64) -> bool {
        self.files.get(file).is_some_and(|e| e.version == version)
    }

    /// Stores counts from the background pass unless the file already has
    /// current ones. Returns whether they were stored.
    pub(crate) fn set_counts(&mut self, file: usize, counts: Counts) -> bool {
        match self.files.get_mut(file) {
            Some(entry) if entry.wants_counts() => {
                entry.counts = Some(counts);
                entry.recount = false;
                true
            }
            _ => false,
        }
    }

    /// Stores a worker's emphasis of hunk `hunk` of `diff` if that's still
    /// the diff loaded for `file`.
    pub(crate) fn set_emphasis(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        hunk: usize,
        emphasis: Vec<Emphasis>,
        syntax: Option<HunkSyntax>,
    ) {
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return;
        };
        if !Arc::ptr_eq(&loaded.diff, diff) {
            return;
        }
        if let Some(view) = loaded
            .structural
            .as_mut()
            .and_then(|s| s.views.get_mut(hunk))
        {
            view.syntax.clone_from(&syntax);
        }
        if let Some(h) = loaded.hunks.get_mut(hunk) {
            h.emphasis = Some(emphasis);
            h.syntax = syntax;
        }
    }

    /// Stores the syntax runs of a gap's lines, empty when there are none,
    /// if `diff` is still the diff loaded for `file`.
    pub(crate) fn set_gap_syntax(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        gap: usize,
        spans: Option<Spans>,
    ) {
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return;
        };
        if !Arc::ptr_eq(&loaded.diff, diff) {
            return;
        }
        if let Some(slot @ None) = loaded.gap_syntax.get_mut(gap) {
            *slot = Some(spans.unwrap_or_default());
        }
    }

    /// Stores a worker's syntax diff of `diff` if that's still the diff
    /// loaded for `file`. Returns whether it was stored.
    pub(crate) fn set_structural(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Option<Result<Structural, Unavailable>>,
    ) -> bool {
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return false;
        };
        if !Arc::ptr_eq(&loaded.diff, diff) || loaded.structural.is_some() {
            return false;
        }
        let note = result.as_ref().and_then(print::structural_note);
        let mut hunks = match result {
            Some(Ok(s)) => s.hunks,
            _ => Vec::new(),
        };
        let mut views = views(&loaded.diff, &hunks);
        for ((view, hunk), line_view) in views.iter_mut().zip(&mut hunks).zip(&loaded.hunks) {
            view.emphasis = Some(
                hunk.lines
                    .iter_mut()
                    .map(|l| std::mem::take(&mut l.emphasis))
                    .collect(),
            );
            view.syntax.clone_from(&line_view.syntax);
        }
        let bytes: usize = views.iter().map(|v| v.body.len() * 96).sum();
        loaded.bytes += bytes;
        self.loaded_bytes += bytes;
        loaded.structural = Some(SyntaxDiff { hunks, views, note });
        true
    }

    /// Stores a worker's rendering of `diff` (or why there's none) if
    /// that's still the line diff loaded for `file` and it has none yet.
    /// It shows at once when rendered views do, from the file's first
    /// change. Returns whether it was stored.
    pub(crate) fn set_rendered(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Result<Box<Loaded>, String>,
    ) -> bool {
        let Some(entry) = self.files.get_mut(file) else {
            return false;
        };
        if entry.wants_render().is_none_or(|d| !Arc::ptr_eq(d, diff)) {
            return false;
        }
        match result {
            Ok(loaded) => {
                self.loaded_bytes += loaded.bytes;
                entry.render = Some(Box::new(Render::Hidden(loaded)));
                if self.rendered {
                    self.show_render(file);
                }
            }
            Err(reason) => entry.render = Some(Box::new(Render::Failed(reason))),
        }
        true
    }

    /// Swaps `file` between its line diff and its rendered view, keeping
    /// the position valid: a file shown now opens at its first change.
    fn show_render(&mut self, file: usize) {
        if self
            .selection
            .as_ref()
            .is_some_and(|s| s.anchor.file == file)
        {
            self.selection = None;
        }
        if self.search.as_ref().is_some_and(|m| m.file == file) {
            self.search = None;
        }
        self.files[file].swap_render();
        if self.pos.file == file {
            self.forget_positions();
            self.place(file, false);
            self.normalize();
        }
    }

    /// Turns rendered views on or off for every file that has one. Turning
    /// them on gives files the renderer failed on another try.
    pub(crate) fn set_rendered_mode(&mut self, on: bool) {
        self.rendered = on;
        for file in 0..self.files.len() {
            match (self.files[file].render.as_deref(), on) {
                (Some(Render::Hidden(_)), true) | (Some(Render::Shown(_)), false) => {
                    self.show_render(file);
                }
                (Some(Render::Failed(_)), true) => self.files[file].render = None,
                _ => {}
            }
        }
        self.normalize();
    }

    /// Drops rendered views drawn for another width than `width`, putting
    /// the line diff back until a new rendering arrives.
    pub(crate) fn forget_renders(&mut self, width: usize) {
        for file in 0..self.files.len() {
            let entry = &self.files[file];
            let shown = matches!(entry.render.as_deref(), Some(Render::Shown(_)));
            let stale = match entry.render.as_deref() {
                Some(Render::Shown(_)) => match &entry.state {
                    State::Loaded(shown) => shown.rendered_width() != Some(width),
                    State::Pending | State::Failed(_) => true,
                },
                Some(Render::Hidden(r)) => r.rendered_width() != Some(width),
                Some(Render::Failed(_)) | None => false,
            };
            if stale {
                self.loaded_bytes -= self.files[file].drop_render();
                if shown && self.pos.file == file {
                    self.forget_positions();
                    self.place(file, false);
                }
            }
        }
        self.normalize();
    }

    /// Drops the least recently drawn diffs outside `keep` until the cache
    /// is under `cap` bytes; they're diffed again if they come back.
    pub(crate) fn evict(&mut self, cap: usize, keep: &Range<usize>) {
        while self.loaded_bytes > cap {
            let Some(i) = self
                .files
                .iter()
                .enumerate()
                .filter(|(i, e)| !keep.contains(i) && matches!(e.state, State::Loaded(_)))
                .min_by_key(|(_, e)| e.last_drawn)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.loaded_bytes -= self.files[i].drop_render();
            if let State::Loaded(loaded) =
                std::mem::replace(&mut self.files[i].state, State::Pending)
            {
                self.loaded_bytes -= loaded.bytes;
            }
            self.files[i].stale = false;
        }
    }

    pub(crate) fn diff_of(&self, file: usize) -> Option<&Arc<FileDiff>> {
        self.loaded(file).map(|l| &l.diff)
    }

    fn loaded(&self, file: usize) -> Option<&Loaded> {
        match &self.files.get(file)?.state {
            State::Loaded(l) => Some(l),
            _ => None,
        }
    }

    /// Whether `entry` shows its full text. A piped file never does.
    pub(crate) fn is_full(&self, entry: &Entry) -> bool {
        !matches!(entry.source, Source::Piped(_)) && self.full
    }

    /// Whether the file shown is a piped one, which has only its hunks.
    pub(crate) fn is_partial(&self) -> bool {
        self.files
            .get(self.pos.file)
            .is_some_and(|e| matches!(e.source, Source::Piped(_)))
    }

    fn segment(&self, file: usize, part: usize) -> Option<Segment> {
        self.loaded(file)?
            .segments
            .get(part.checked_sub(1)?)
            .copied()
    }

    fn gap_shown(&self, file: usize, gap: usize) -> Shown {
        let entry = &self.files[file];
        let len = self
            .loaded(file)
            .and_then(|l| l.gaps.get(gap))
            .map_or(0, |g| g.len);
        if self.is_full(entry) {
            return Shown {
                top: len,
                bottom: 0,
                len,
            };
        }
        let (top, bottom) = match entry.source {
            Source::Piped(_) => (0, 0),
            _ => entry.reveal.get(gap).copied().unwrap_or_default(),
        };
        let top = top.min(len);
        Shown {
            top,
            bottom: bottom.min(len - top),
            len,
        }
    }

    /// The layout of a file's hunks. A rendered view always wraps, since
    /// renderers leave paragraphs as single lines, and numbers its source.
    fn file_layout(&self, file: usize) -> Layout {
        let Some(entry) = self.files.get(file) else {
            return self.layout;
        };
        let loaded = self.loaded(file);
        if let Some(rendered) = loaded.and_then(|l| l.rendered.as_deref()) {
            let layout = Layout {
                wrap: true,
                words: true,
                scroll: 0,
                ..self.layout
            };
            return layout.for_file(entry.status(), rendered.source_len());
        }
        let lines = loaded.map_or(0, |l| l.diff.old.len().max(l.diff.new.len()));
        self.layout.for_file(entry.status(), lines)
    }

    fn is_rendered(&self, file: usize) -> bool {
        self.loaded(file).is_some_and(|l| l.rendered.is_some())
    }

    fn parts(&self, file: usize) -> usize {
        1 + self.loaded(file).map_or(0, |l| l.segments.len())
    }

    /// The first part the position may be on: part 0 holds rows only for
    /// a sheet or a file without segments.
    fn first_part(&self, file: usize) -> usize {
        let sheet = self.files.get(file).is_some_and(Entry::is_sheet);
        usize::from(!sheet && self.parts(file) > 1)
    }

    /// The rows of an entry shown as text (see [`Entry::is_sheet`]); `None`
    /// for one with hunks.
    fn sheet_rows(&self, file: usize) -> Option<usize> {
        let entry = self.files.get(file).filter(|e| e.is_sheet())?;
        match &entry.source {
            Source::Commit(commit) => Some(match commit.id {
                Some(_) => commit.lines.len() + 2 + self.commit_files(file).len(),
                None => commit.lines.len().max(1),
            }),
            Source::Piped(piped) => Some(piped.combined.as_ref().map_or(0, Vec::len).max(1)),
            Source::Repo => None,
        }
    }

    /// The line of the input a sheet shows at `row`: a commit's header and
    /// message, or a combined diff's line; `None` for the rows made here.
    fn sheet_text(&self, file: usize, row: usize) -> Option<(Role, BString)> {
        let (role, text) = match &self.files.get(file)?.source {
            Source::Commit(commit) => {
                let (kind, text) = commit.lines.get(row)?;
                (pager::text_role(*kind), text)
            }
            Source::Piped(piped) => {
                let (parents, line) = piped.combined.as_ref()?.get(row)?;
                (
                    pager::combined_role(patch::combined_kind(*parents, line)),
                    line,
                )
            }
            Source::Repo => return None,
        };
        Some((role, display_text(text).into()))
    }

    /// Row `row` of a sheet: a commit's lines, then how many files it has
    /// and a row per file; or a combined diff's lines. Long lines are cut
    /// and scroll sideways like code.
    fn sheet_row(&mut self, file: usize, row: usize, c: &mut Canvas) {
        if let Some((role, line)) = self.sheet_text(file, row) {
            self.layout.text_row(role, &line, c);
            return;
        }
        let Source::Commit(commit) = &self.files[file].source else {
            c.end_line();
            return;
        };
        let files = self.commit_files(file);
        match row - commit.lines.len() {
            0 => c.end_line(),
            1 => {
                let totals =
                    self.files[files.clone()]
                        .iter()
                        .try_fold(Counts::default(), |t, e| {
                            let n = e.counts?;
                            Some(Counts {
                                added: t.added + n.added,
                                removed: t.removed + n.removed,
                            })
                        });
                let s = if files.len() == 1 { "" } else { "s" };
                c.push(Role::Dim, &format!("{} file{s} ", files.len()));
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
                let Some(entry) = self.files.get(files.start + n - 2) else {
                    c.end_line();
                    return;
                };
                let (letter, role) = crate::render::stat::status_letter(entry.status());
                c.push(Role::Plain, "  ");
                c.push(role, letter);
                c.push(Role::Plain, " ");
                c.push(Role::Plain, &crate::render::escape(&entry.change.path));
                if let Some(n) = entry.counts {
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
        let entry = self.files.get(file)?;
        if entry.is_sheet() {
            return None;
        }
        match &entry.state {
            State::Pending => Some("loading…"),
            State::Loaded(l) => l.note,
            State::Failed(e) => Some(e.as_str()),
        }
    }

    fn ensure_rows(&mut self, file: usize, hunk: usize) {
        let (layout, structural) = (self.file_layout(file), self.structural);
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return;
        };
        let (cores, views) = loaded.shown_mut(structural);
        let (core, view) = (&cores[hunk], &mut views[hunk]);
        let arrangement = layout.arrangement(&print::hunk_header(core, b""));
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
        // The first arrangement's rows are in the estimate the view was
        // loaded with; a second one's are on top of it.
        let second = view.other.is_some();
        let added =
            view.rows.capacity() * size_of::<Row>() + view.starts.capacity() * size_of::<usize>();
        view.arrangement = Some(arrangement);
        if second {
            loaded.bytes += added;
            self.loaded_bytes += added;
        }
    }

    /// Logical rows in a part.
    fn rows(&mut self, file: usize, part: usize) -> usize {
        if part == 0 {
            return self.sheet_rows(file).unwrap_or(1);
        }
        match self.segment(file, part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(file, h);
                let structural = self.structural;
                self.loaded(file)
                    .map_or(1, |l| l.shown(structural).1[h].rows.len().max(1))
            }
            Some(Segment::Gap(g)) => self.gap_shown(file, g).rows(),
            Some(Segment::Start) | None => 1,
        }
    }

    /// Appends the screen rows of the logical row at `pos` (ignoring its
    /// offset).
    fn render(&mut self, pos: Pos, c: &mut Canvas) {
        if pos.part == 0 && self.files.get(pos.file).is_some_and(Entry::is_sheet) {
            self.sheet_row(pos.file, pos.row, c);
            return;
        }
        if pos.part == 0 {
            // The note in the middle of the pane says why it's empty.
            c.end_line();
            return;
        }
        let Some(segment) = self.segment(pos.file, pos.part) else {
            return;
        };
        if let Segment::Hunk(h) = segment {
            self.ensure_rows(pos.file, h);
        }
        let layout = self.file_layout(pos.file);
        let shown = match segment {
            Segment::Gap(g) => Some(self.gap_shown(pos.file, g)),
            _ => None,
        };
        let target = self.target == Some(Pos { offset: 0, ..pos });
        let Some(loaded) = self.loaded(pos.file) else {
            return;
        };
        let (cores, views) = loaded.shown(self.structural);
        let heading = |h: usize| loaded.heading(views, h);
        let rendered = loaded.rendered.as_deref();
        match (segment, shown) {
            (Segment::Start, _) => {
                let header = loaded.header(&cores[0], heading(0));
                layout.hunk_row(&header, Row::Header, |_| print::NO_NEWLINE_LINE, c);
            }
            (Segment::Hunk(h), _) => {
                let (core, view) = (&cores[h], &views[h]);
                let header = loaded.header(core, heading(h));
                let emphasis = view.emphasis.as_deref();
                let syntax = view.syntax.as_ref();
                layout.hunk_row(
                    &header,
                    view.rows[pos.row],
                    |i| {
                        let BodyLine { line, state } = view.body[i];
                        if state == LineState::NoNewline {
                            return print::NO_NEWLINE_LINE;
                        }
                        let line = line as usize;
                        let emph = emphasis.map_or(NO_EMPHASIS, |e| &e[line][..]);
                        let core_line = &core.lines[line];
                        let line = print::diff_line(&loaded.diff, core_line, state, emph, syntax);
                        rendered::restyle(rendered, line)
                    },
                    c,
                );
            }
            (Segment::Gap(g), Some(shown)) => match shown.line(pos.row) {
                None => {
                    let next = cores.get(g).map(|core| loaded.header(core, heading(g)));
                    let from = loaded.gaps[g].old + shown.top;
                    let hidden = loaded.old_count(from..from + shown.hidden());
                    layout.separator(hidden, next.as_ref(), target, c);
                }
                Some(offset) => {
                    let span = loaded.gaps[g];
                    let (old, new) = (span.old + offset, span.new + offset);
                    let raw = loaded.diff.new.line(new);
                    let line = Line {
                        state: LineState::Context,
                        old: Some(old + 1),
                        new: Some(new + 1),
                        text: display_text(raw.strip_suffix(b"\n").unwrap_or(raw)),
                        emphasis: &[],
                        syntax: loaded.gap_syntax[g].as_ref().map(|s| s.line(new)),
                        paint: &[],
                        block_start: false,
                    };
                    let line = rendered::restyle(rendered, line);
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
            },
            (Segment::Gap(_), None) => {}
        }
    }

    /// Screen rows the logical row at `pos` takes.
    fn height(&mut self, pos: Pos) -> usize {
        // Rendered views show only while the mode is on, so a line diff
        // without wrapping pays one flag here.
        if pos.part == 0 || !(self.layout.wrap || (self.rendered && self.is_rendered(pos.file))) {
            return 1;
        }
        let mut c = std::mem::take(&mut self.scratch);
        c.clear();
        self.render(pos, &mut c);
        let n = c.rows().count().max(1);
        self.scratch = c;
        n
    }

    /// Moves `pos` to the next logical row of its file; false at the end.
    fn next_row(&mut self, pos: &mut Pos) -> bool {
        let mut next = Pos { offset: 0, ..*pos };
        if next.row + 1 < self.rows(next.file, next.part) {
            next.row += 1;
        } else if next.part + 1 < self.parts(next.file) {
            next.part += 1;
            next.row = 0;
        } else {
            return false;
        }
        *pos = next;
        true
    }

    /// Moves `pos` to the last screen row of the previous logical row of
    /// its file; false at the start.
    fn prev_row(&mut self, pos: &mut Pos) -> bool {
        let mut prev = *pos;
        if prev.row > 0 {
            prev.row -= 1;
        } else if prev.part > self.first_part(prev.file) {
            prev.part -= 1;
            prev.row = self.rows(prev.file, prev.part) - 1;
        } else {
            return false;
        }
        prev.offset = self.height(prev) - 1;
        *pos = prev;
        true
    }

    /// Scrolls the code by `delta` screen rows, then keeps the screen full.
    pub(crate) fn scroll(&mut self, delta: isize) {
        if self.files.is_empty() {
            return;
        }
        let mut pos = self.pos;
        for _ in 0..delta.unsigned_abs() {
            let moved = if delta > 0 {
                if pos.offset + 1 < self.height(pos) {
                    pos.offset += 1;
                    true
                } else {
                    self.next_row(&mut pos)
                }
            } else if pos.offset > 0 {
                pos.offset -= 1;
                true
            } else {
                self.prev_row(&mut pos)
            };
            if !moved {
                break;
            }
        }
        self.pos = pos;
        self.clamp();
    }

    /// Pulls the position up when the rows below it don't fill the screen.
    pub(crate) fn clamp(&mut self) {
        if self.files.is_empty() {
            return;
        }
        let mut pos = self.pos;
        let first = self.height(pos);
        let mut rows = first - pos.offset.min(first - 1);
        while rows < self.height {
            if !self.next_row(&mut pos) {
                break;
            }
            rows += self.height(pos);
        }
        while rows < self.height {
            let mut pos = self.pos;
            let moved = if pos.offset > 0 {
                pos.offset -= 1;
                true
            } else {
                self.prev_row(&mut pos)
            };
            if !moved {
                break;
            }
            self.pos = pos;
            rows += 1;
        }
    }

    /// The hunk or function the top row is in: its hunk, or in revealed
    /// unchanged lines the heading above them, else the hunk above. `None`
    /// before the first hunk and for a whole added or deleted file.
    pub(crate) fn context(&mut self) -> Option<Context> {
        let pos = self.pos;
        let loaded = self.loaded(pos.file)?;
        if loaded.whole {
            return None;
        }
        let hunk = match self.segment(pos.file, pos.part)? {
            Segment::Hunk(h) => h,
            Segment::Start => 0,
            Segment::Gap(g) => {
                let shown = self.gap_shown(pos.file, g);
                let line = shown.line(pos.row).and_then(|offset| {
                    let old = self.loaded(pos.file)?.gaps[g].old + offset;
                    self.heading_above(pos.file, old)
                });
                if let Some(line) = line {
                    let diff = &self.loaded(pos.file)?.diff;
                    let text = display_text(heading_text(&diff.old, line));
                    return Some(Context {
                        numbers: None,
                        heading: crate::render::escape(text),
                    });
                }
                g.checked_sub(1)?
            }
        };
        let loaded = self.loaded(pos.file)?;
        let (cores, views) = loaded.shown(self.structural);
        let header = loaded.header(&cores[hunk], loaded.heading(views, hunk));
        Some(Context {
            numbers: Some(header.numbers()),
            heading: crate::render::escape(display_text(header.heading)),
        })
    }

    /// The hunk at the top of the file shown and how many it has: the one
    /// the top row is in, or the last above it.
    pub(crate) fn hunk_position(&self) -> Option<(usize, usize)> {
        let loaded = self.loaded(self.pos.file)?;
        let total = loaded.hunks.len();
        let at = match self.segment(self.pos.file, self.pos.part) {
            Some(Segment::Hunk(h)) => h + 1,
            Some(Segment::Gap(g)) => g.max(1),
            Some(Segment::Start) | None => 1,
        };
        (total > 0).then_some((at.min(total), total))
    }

    /// The heading line (as git finds one) above old line `line` of `file`.
    /// Searches continue from the last one while the view moves down, so
    /// scrolling costs a line or so each.
    fn heading_above(&mut self, file: usize, line: usize) -> Option<usize> {
        let diff = Arc::clone(&self.loaded(file)?.diff);
        let heading = match &mut self.heading {
            Some((d, from, heading)) if Arc::ptr_eq(d, &diff) && *from <= line => {
                *from = line;
                heading
            }
            slot => &mut slot.insert((Arc::clone(&diff), line, Heading::default())).2,
        };
        heading.find_line(&diff.old, line)
    }

    pub(crate) fn top(&mut self) {
        self.pos = Pos {
            file: self.pos.file,
            part: self.first_part(self.pos.file),
            ..Pos::default()
        };
    }

    pub(crate) fn bottom(&mut self) {
        if self.files.is_empty() {
            return;
        }
        let file = self.pos.file;
        let part = self.parts(file) - 1;
        let row = self.rows(file, part) - 1;
        let mut pos = Pos {
            file,
            part,
            row,
            offset: 0,
        };
        pos.offset = self.height(pos) - 1;
        self.pos = pos;
        self.clamp();
    }

    /// Shows `file` where it was left, or the first time at its first
    /// change (see [`Doc::place`]), once it's diffed. The file shown until
    /// now keeps its place for when it comes back, if it was diffed by then.
    pub(crate) fn goto_file(&mut self, file: usize) {
        if file >= self.files.len() || file == self.pos.file {
            return;
        }
        if let Some(entry) = self.files.get_mut(self.pos.file)
            && entry.placed
        {
            entry.saved = Some(self.pos);
        }
        self.pos = Pos {
            file,
            ..self.files[file].saved.unwrap_or_default()
        };
        self.normalize();
        let entry = &self.files[file];
        if !entry.placed && (entry.is_sheet() || !matches!(entry.state, State::Pending)) {
            self.place(file, false);
        }
    }

    /// Shows `file` from its top, keeping the current file's place.
    fn open_at_top(&mut self, file: usize) {
        if let Some(entry) = self.files.get_mut(file) {
            entry.placed = true;
        }
        self.goto_file(file);
        self.top();
    }

    pub(crate) fn next_file(&mut self) {
        self.goto_file(self.pos.file + 1);
    }

    pub(crate) fn prev_file(&mut self) {
        if let Some(file) = self.pos.file.checked_sub(1) {
            self.goto_file(file);
        }
    }

    /// The rows of `file`'s part `part` that start a block of changes.
    fn starts(&mut self, file: usize, part: usize) -> Vec<usize> {
        let Some(Segment::Hunk(h)) = self.segment(file, part) else {
            return Vec::new();
        };
        self.ensure_rows(file, h);
        let structural = self.structural;
        self.loaded(file)
            .map(|l| l.shown(structural).1[h].starts.clone())
            .unwrap_or_default()
    }

    fn land(&mut self, file: usize, part: usize, row: usize) {
        let pos = Pos {
            file,
            part,
            row,
            offset: 0,
        };
        self.pos = pos;
        self.clamp();
        self.landed = Some((pos, self.pos));
    }

    /// Where `n` and `N` count from: the change they last landed on while
    /// the view hasn't moved since, since a change near the end of a file
    /// can't be put at the top.
    fn change_cursor(&self) -> Pos {
        match self.landed {
            Some((change, view)) if view == self.pos => change,
            _ => self.pos,
        }
    }

    /// Puts the next block of changed lines at the top. Past the file's
    /// last, opens the next file at its first change (or its top while it
    /// isn't diffed) and returns that file.
    pub(crate) fn next_change(&mut self) -> Option<usize> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        for part in start.part.max(1)..self.parts(start.file) {
            let here = part == start.part;
            if let Some(row) = self
                .starts(start.file, part)
                .into_iter()
                .find(|&r| !here || r > start.row)
            {
                self.land(start.file, part, row);
                return None;
            }
        }
        // Commits have no changes of their own; `n` goes on to their files.
        let next = (start.file + 1..self.files.len()).find(|&f| !self.files[f].is_commit())?;
        if self.files[next].placed {
            self.open_at_top(next);
            self.land_edge(next, false);
        } else if matches!(self.files[next].state, State::Pending) {
            // Opened as any file is the first time, once diffed.
            self.goto_file(next);
            self.pending_land = Some((self.pos, false));
        } else {
            self.files[next].placed = true;
            self.goto_file(next);
            self.place(next, true);
        }
        Some(next)
    }

    /// Puts the first (or `last`) block of changes of `file` at the top.
    /// A file not diffed yet stays at its top until its diff arrives.
    fn land_edge(&mut self, file: usize, last: bool) {
        if matches!(self.files[file].state, State::Pending) {
            self.pending_land = Some((self.pos, last));
            return;
        }
        if let Some((part, row)) = self.edge(file, last) {
            self.land(file, part, row);
        }
    }

    /// The part and row of the first (or `last`) block of changes of
    /// `file`.
    fn edge(&mut self, file: usize, last: bool) -> Option<(usize, usize)> {
        let edge = |doc: &mut Self, p: usize| {
            let starts = doc.starts(file, p);
            let row = if last { starts.last() } else { starts.first() };
            Some((p, *row?))
        };
        if last {
            (1..self.parts(file)).rev().find_map(|p| edge(self, p))
        } else {
            (1..self.parts(file)).find_map(|p| edge(self, p))
        }
    }

    /// Puts the previous block of changed lines at the top. Before the
    /// file's first, opens the previous file at its last change (or its
    /// top while it isn't diffed) and returns that file.
    pub(crate) fn prev_change(&mut self) -> Option<usize> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        for part in (1..=start.part).rev() {
            let here = part == start.part;
            if let Some(row) = self
                .starts(start.file, part)
                .into_iter()
                .rev()
                .find(|&r| !here || r < start.row || (r == start.row && start.offset > 0))
            {
                self.land(start.file, part, row);
                return None;
            }
        }
        let Some(prev) = (0..start.file).rev().find(|&f| !self.files[f].is_commit()) else {
            self.top();
            return None;
        };
        self.open_at_top(prev);
        self.land_edge(prev, true);
        Some(prev)
    }

    /// The line of gap `gap` the position is on, when it's in that gap (the
    /// separator counts as its first hidden line).
    fn gap_anchor(&self, file: usize, gap: usize) -> Option<usize> {
        let pos = self.pos;
        if pos.file != file || self.segment(file, pos.part) != Some(Segment::Gap(gap)) {
            return None;
        }
        let shown = self.gap_shown(file, gap);
        Some(shown.line(pos.row).unwrap_or(shown.top))
    }

    /// Puts the position back on gap line `line` after the gap changed.
    fn reanchor(&mut self, file: usize, gap: usize, line: Option<usize>) {
        if let Some(line) = line {
            self.pos.row = self.gap_shown(file, gap).row(line);
            self.pos.offset = 0;
        }
        self.normalize();
    }

    /// Reveals more of gap `gap` of `file`: lines next to the hunk below
    /// for the gap before the first hunk, next to the hunk above for the
    /// gap after the last, half of each otherwise. Returns whether any
    /// line was hidden.
    pub(crate) fn expand(&mut self, file: usize, gap: usize) -> bool {
        let Some(gaps) = self
            .loaded(file)
            .filter(|l| !l.is_partial())
            .map(|l| l.gaps.len())
        else {
            return false;
        };
        let shown = self.gap_shown(file, gap);
        let hidden = shown.hidden();
        if gap >= gaps || hidden == 0 {
            return false;
        }
        let n = if hidden <= 2 * REVEAL_LINES {
            hidden
        } else {
            REVEAL_LINES
        };
        let (top, bottom) = if gap == 0 {
            (shown.top, shown.bottom + n)
        } else if gap + 1 == gaps {
            (shown.top + n, shown.bottom)
        } else {
            (shown.top + n - n / 2, shown.bottom + n / 2)
        };
        let anchor = self.gap_anchor(file, gap);
        let entry = &mut self.files[file];
        entry.reveal.resize(gaps, (0, 0));
        entry.reveal[gap] = (top, bottom);
        self.reanchor(file, gap, anchor);
        true
    }

    /// `x`: reveals the highlighted separator's gap, or with none on
    /// screen the gaps around the hunk at the top. Returns whether there
    /// was anything to reveal.
    pub(crate) fn expand_target(&mut self) -> bool {
        if let Some(t) = self.target
            && let Some(Segment::Gap(g)) = self.segment(t.file, t.part)
        {
            return self.expand(t.file, g);
        }
        let Pos { file, part, .. } = self.pos;
        match self.segment(file, part) {
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
            && let Some(Segment::Gap(g)) = self.segment(p.file, p.part)
        {
            self.expand(p.file, g);
        }
    }

    /// `X`: switches every file between its changes and its full text,
    /// keeping the line at the top where it is.
    pub(crate) fn toggle_full(&mut self) {
        let file = self.pos.file;
        if file >= self.files.len() {
            return;
        }
        let gap = match self.segment(file, self.pos.part) {
            Some(Segment::Gap(g)) => Some((g, self.gap_anchor(file, g))),
            _ => None,
        };
        self.full = !self.full;
        match gap {
            Some((g, line)) => self.reanchor(file, g, line),
            None => self.normalize(),
        }
    }

    /// The new-side line number at the top of the screen, for the editor.
    pub(crate) fn line_at_top(&mut self) -> usize {
        let line = self.line_of(self.pos);
        match self
            .loaded(self.pos.file)
            .and_then(|l| l.rendered.as_deref())
        {
            Some(rendered) => rendered.source_lines(Side::New, line - 1, line - 1).start + 1,
            None => line,
        }
    }

    /// Whether the overview column shows for `file`: in its full text, or
    /// when its text is taller than the screen.
    fn has_overview(&self, file: usize) -> bool {
        self.loaded(file).is_some_and(|l| {
            !l.segments.is_empty()
                && (self.is_full(&self.files[file]) || l.overview_lines() > self.height)
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
        let (file, height, structural) = (self.pos.file, self.height, self.structural);
        let modified = self.layout.modified_lines;
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return;
        };
        let total = loaded.overview_lines();
        let marks = loaded.marks(structural, modified);
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
        let file = self.pos.file;
        if !self.has_overview(file) {
            return;
        }
        let Some(total) = self.loaded(file).map(Loaded::overview_lines) else {
            return;
        };
        let target = y * total / self.height.max(1);
        let mut best = None;
        for part in 1..self.parts(file) {
            let Some(Segment::Hunk(h)) = self.segment(file, part) else {
                continue;
            };
            for row in self.starts(file, part) {
                best = Some((part, row));
                if self.hunk_line(file, h, row) > target {
                    self.land(file, part, row);
                    return;
                }
            }
        }
        if let Some((part, row)) = best {
            self.land(file, part, row);
        }
    }

    /// The new-side line number (1-based) that the row at `pos` shows, or
    /// the first after it.
    fn line_of(&mut self, pos: Pos) -> usize {
        let (part, from) = if pos.part == 0 {
            (1, 0)
        } else {
            (pos.part, pos.row)
        };
        match self.segment(pos.file, part) {
            Some(Segment::Gap(g)) => {
                let shown = self.gap_shown(pos.file, g);
                let line = shown.line(from).unwrap_or(shown.top);
                self.loaded(pos.file)
                    .map_or(1, |l| l.gaps[g].new + line + 1)
            }
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(pos.file, h);
                self.hunk_line(pos.file, h, from)
            }
            Some(Segment::Start) => self.hunk_line(pos.file, 0, 0),
            None => 1,
        }
    }

    /// The new-side line number of the first line at or after row `from`
    /// of hunk `h`. A removed line's is where it would be in the new file.
    fn hunk_line(&self, file: usize, h: usize, from: usize) -> usize {
        let Some(loaded) = self.loaded(file) else {
            return 1;
        };
        let (cores, views) = loaded.shown(self.structural);
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

    /// Notes what the logical row at `pos`, drawn at screen row `y`, still
    /// needs from the workers, and whether it's a separator.
    fn note_drawn(&mut self, pos: Pos, y: usize) {
        let Some(loaded) = self.loaded(pos.file) else {
            return;
        };
        match self.segment(pos.file, pos.part) {
            Some(Segment::Hunk(h)) => {
                if loaded.hunks[h].emphasis.is_none()
                    && self.missing_emphasis.last() != Some(&(pos.file, h))
                {
                    self.missing_emphasis.push((pos.file, h));
                }
            }
            Some(Segment::Gap(g)) => {
                let wants_syntax = loaded.syntax.is_some() && loaded.gap_syntax[g].is_none();
                let shown = self.gap_shown(pos.file, g);
                match shown.line(pos.row) {
                    None if pos.offset == 0 => self.separators.push((y, pos)),
                    Some(_)
                        if wants_syntax
                            && self.missing_gap_syntax.last() != Some(&(pos.file, g)) =>
                    {
                        self.missing_gap_syntax.push((pos.file, g));
                    }
                    _ => {}
                }
            }
            Some(Segment::Start) | None => {}
        }
    }

    /// Draws from the position down into rows `top..top + self.height` of
    /// `grid`, columns `x..end`. Returns the files drawn. The first
    /// separator on screen is drawn highlighted, as what `x` reveals.
    pub(crate) fn draw(
        &mut self,
        grid: &mut Grid,
        x: usize,
        end: usize,
        top: usize,
        theme: &Theme,
        frame: u64,
    ) -> Range<usize> {
        self.missing_emphasis.clear();
        self.missing_gap_syntax.clear();
        self.separators.clear();
        self.target = None;
        if self.files.is_empty() {
            return 0..0;
        }
        let overview = (self.overview && end > x + 1).then_some(end - 1);
        let end = overview.unwrap_or(end);
        let mut c = Canvas::default();
        let mut pos = self.pos;
        let file = pos.file;
        self.drawn.clear();
        self.drawn.resize(self.height, None);
        let mut y = 0;
        let mut last = pos;
        while y < self.height {
            self.note_drawn(pos, y);
            c.clear();
            self.render(pos, &mut c);
            for (i, row) in c.rows().enumerate().skip(pos.offset) {
                if y == self.height {
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
            if !self.next_row(&mut pos) {
                break;
            }
        }
        self.draw_selection(grid, x, end, top);
        self.draw_search(grid, x, end, top, theme);
        self.files[file].last_drawn = frame;
        if let Some(note) = self.note(file) {
            let text_width = unicode_width::UnicodeWidthStr::width(note);
            let left = x + end.saturating_sub(x + text_width) / 2;
            let line = top + y.max(self.height / 2).min(self.height.saturating_sub(1));
            grid.put_str(left, line, end, note, theme.style(Role::Dim));
        }
        if let Some(column) = overview
            && self.has_overview(file)
        {
            let lines = (self.line_of(self.pos), self.line_of(last));
            self.draw_overview(grid, column, top, lines, theme);
        }
        if let Some(&(y, pos)) = self.separators.first() {
            self.target = Some(pos);
            c.clear();
            self.render(pos, &mut c);
            if let Some(row) = c.rows().next() {
                grid.fill(x, top + y, end, Style::PLAIN);
                paint(grid, x, top + y, end, row, theme, Attributes::NONE);
            }
        }
        file..file + 1
    }

    /// Shows the selected lines in reverse video, on their side's half of
    /// a split row.
    fn draw_selection(&mut self, grid: &mut Grid, x: usize, end: usize, top: usize) {
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
                lines = self.row_lines(logical);
                previous = Some(logical);
                if bytes.is_some() {
                    mapped.clear();
                    self.render(logical, &mut mapped);
                }
            }
            let Some(line) = lines[side].filter(|l| (lo..=hi).contains(l)) else {
                continue;
            };
            if let (Some((from, to)), Some(map)) = (bytes, mapped.map()) {
                let split = self.split_row(logical);
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
            let columns = if self.split_row(logical) {
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

    /// Paints the search's matches on the rows drawn, the current one in
    /// its own color. Only the rows on screen are looked up, however many
    /// matches the file has.
    fn draw_search(&mut self, grid: &mut Grid, x: usize, end: usize, top: usize, theme: &Theme) {
        let Some(marks) = self.search.take() else {
            return;
        };
        let mut mapped = std::mem::take(&mut self.mapped);
        let mut previous = None;
        for y in 0..self.drawn.len() {
            let Some(pos) = self.drawn[y] else { continue };
            let logical = Pos { offset: 0, ..pos };
            if pos.file != marks.file {
                continue;
            }
            let lines = self.row_lines(logical);
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
                self.render(logical, &mut mapped);
                previous = Some(logical);
            }
            let split = self.split_row(logical);
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
        self.search = Some(marks);
    }

    /// The new lines each gap of `file` hides under its separator, for
    /// [`find`]; `None` until it's diffed.
    pub(crate) fn hidden_lines(&self, file: usize) -> Option<Vec<Range<usize>>> {
        let loaded = self.loaded(file)?;
        Some(
            (0..loaded.gaps.len())
                .map(|g| {
                    let (span, shown) = (loaded.gaps[g], self.gap_shown(file, g));
                    span.new + shown.top..span.new + shown.len - shown.bottom
                })
                .collect(),
        )
    }

    /// The row showing match `m` of `file`.
    pub(crate) fn match_pos(&mut self, file: usize, m: &Match) -> Option<Pos> {
        let line = m.line as usize;
        let loaded = self.loaded(file)?;
        if m.side == Side::New
            && let Some(g) = loaded
                .gaps
                .iter()
                .position(|g| (g.new..g.new + g.len).contains(&line))
        {
            let part = 1 + loaded.segments.iter().position(|s| *s == Segment::Gap(g))?;
            let row = self.gap_shown(file, g).row(line - loaded.gaps[g].new);
            return Some(Pos {
                file,
                part,
                row,
                offset: 0,
            });
        }
        let (cores, _) = loaded.shown(self.structural);
        let h = cores.iter().position(|h| match m.side {
            Side::Old => (h.old_start..h.old_start + h.old_len).contains(&line),
            Side::New => (h.new_start..h.new_start + h.new_len).contains(&line),
        })?;
        let part = 1 + loaded
            .segments
            .iter()
            .position(|s| *s == Segment::Hunk(h))?;
        self.ensure_rows(file, h);
        let loaded = self.loaded(file)?;
        let (cores, views) = loaded.shown(self.structural);
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

    /// Shows the row at `pos` a few rows below the top of the pane.
    pub(crate) fn goto_match(&mut self, pos: Pos) {
        self.pos = pos;
        self.clamp();
        let context = isize::try_from(self.height / 4).unwrap_or(0);
        self.scroll(-context);
        self.reveal(pos);
    }

    /// The old and new lines (0-based) a logical row shows; `None` on a
    /// side without one, and on rows that aren't code.
    fn row_lines(&mut self, pos: Pos) -> [Option<usize>; 2] {
        match self.segment(pos.file, pos.part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(pos.file, h);
                let Some(loaded) = self.loaded(pos.file) else {
                    return [None, None];
                };
                let (cores, views) = loaded.shown(self.structural);
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
            Some(Segment::Gap(g)) => {
                let shown = self.gap_shown(pos.file, g);
                match (shown.line(pos.row), self.loaded(pos.file)) {
                    (Some(offset), Some(loaded)) => {
                        let span = loaded.gaps[g];
                        [Some(span.old + offset), Some(span.new + offset)]
                    }
                    _ => [None, None],
                }
            }
            Some(Segment::Start) | None => [None, None],
        }
    }

    /// Whether the logical row at `pos` is drawn as two halves.
    fn split_row(&mut self, pos: Pos) -> bool {
        match self.segment(pos.file, pos.part) {
            Some(Segment::Hunk(h)) => {
                self.ensure_rows(pos.file, h);
                let structural = self.structural;
                self.loaded(pos.file).is_some_and(|l| {
                    l.shown(structural).1[h].arrangement == Some(Arrangement::Split)
                })
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
                self.file_layout(pos.file).arrangement(&header) == Arrangement::Split
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
        let lines = self.row_lines(pos);
        if self.split_row(pos) {
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
        let lines = self.row_lines(pos);
        let confined = side.is_some();
        let side = match side {
            Some(side) => side,
            None => self.side_at(pos, column)?,
        };
        let line = lines[side.index()]?;
        let split = self.split_row(pos);
        let mut c = std::mem::take(&mut self.mapped);
        c.clear();
        self.render(pos, &mut c);
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
        let diff = &self.loaded(file)?.diff;
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
            word(text, bytes.start)
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
        let mut pos = from;
        loop {
            let moved = if back {
                self.prev_row(&mut pos)
            } else {
                self.next_row(&mut pos)
            };
            if !moved {
                return None;
            }
            pos.offset = 0;
            if let Some(line) = self.row_lines(pos)[side.index()] {
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
                ..self.pos
            })
        {
            self.pos = target;
            self.clamp();
            return;
        }
        // Screen rows from the top of the pane to the end of `target`.
        let mut pos = self.pos;
        let mut rows = self.height(pos) - pos.offset;
        while (Pos { offset: 0, ..pos }) < target && rows <= 2 * self.height {
            if !self.next_row(&mut pos) {
                break;
            }
            rows += self.height(pos);
        }
        if rows > self.height {
            self.scroll(isize::try_from(rows - self.height).unwrap_or(isize::MAX));
        }
    }

    /// The selected lines of their side, as the file has them, or the
    /// selected part of them. With `decorations`, each line starts with its
    /// number and its sign as a unified diff has it (`12 +code`).
    pub(crate) fn selected(&mut self, decorations: bool) -> Option<Copied> {
        let selection = self.selection.clone()?;
        let file = selection.anchor.file;
        let (lo, hi) = selection.range();
        if let Some(rendered) = self.loaded(file)?.rendered.as_deref() {
            return Some(rendered.copy(selection.side, lo, hi));
        }
        let gaps = self.loaded(file)?.gaps.len();
        let partial = self.loaded(file)?.is_partial();
        let mut hidden = 0;
        // A piped file's hidden lines aren't known, so they're left out.
        let mut unknown = Vec::new();
        for g in 0..gaps {
            let shown = self.gap_shown(file, g);
            let span = self.loaded(file)?.gaps[g];
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
        let diff = &self.loaded(file)?.diff;
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
            let Some(diff) = self.loaded(pos.file).map(|l| Arc::clone(&l.diff)) else {
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

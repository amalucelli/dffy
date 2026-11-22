//! The entries of a diff and the one shown: each a file (or a commit, or a
//! piped diff's text) with its [`File`] once the host prepared it, and the
//! state that outlives the file being dropped and prepared again: revealed
//! lines, where it was left, counts. [`Document`] moves between entries and
//! rows; the host fetches content, runs the work [`Document::missing`] names
//! and keeps the screen.

use std::cell::Ref;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gix::bstr::BString;

use super::blocks::{Arrange, At, Attach, Block, BlockId, Blocks, Placed};
use super::{
    Anchor, Content, Context, Counts, File, Headings, Hit, Mark, Match, Matches, Pos, Reveal,
    Segment, Shape, Shown, Side, file_status,
};
use crate::annotation::{Placement, REVEAL_CONTEXT};
use crate::conflict::{Choice, Choices};
use crate::diff::Emphasis;
use crate::git::{FileChange, FileDiff, Status, Update};
use crate::highlight::HunkSyntax;
use crate::patch::Commit;
use crate::structural::{Structural, Unavailable};
use crate::view::{Arrangement, FileStatus, Spans};

/// What a row shows, to find it again once rows are laid out anew (see
/// [`Document::relayout`]).
#[derive(Clone, Copy)]
enum Spot {
    /// Row `line` of a block.
    Block { id: BlockId, line: usize },
    /// A line of `side`, `down` rows below the row.
    Line {
        side: Side,
        line: usize,
        down: usize,
    },
}

/// A file of a piped diff as its patch described it. The patch's hunks are
/// the host's to keep and read back (`patch`); [`File::piped`] prepares them.
#[derive(Debug)]
pub struct Piped<P> {
    /// The status the patch header gave, finer than the change's.
    pub status: FileStatus,
    /// The patch called the file binary.
    pub binary: bool,
    /// Its added and removed lines.
    pub counts: Counts,
    /// A combined diff's lines, each with its parent count, shown as they
    /// came instead of hunks (see [`crate::patch::combined_kind`]).
    pub combined: Option<Vec<(usize, Vec<u8>)>>,
    /// The host's handle to the file's hunks.
    pub patch: P,
}

/// Where an entry comes from. `P` is the host's handle to a piped file's
/// hunks (see [`Piped`]).
#[derive(Debug)]
pub enum Source<P> {
    /// A file the host diffs from a repository.
    Repo,
    /// A file of a piped diff, which has only its hunks.
    Piped(Arc<Piped<P>>),
    /// A commit's header, or text from a piped diff: no diff of its own. A
    /// commit's files are the entries after it, up to the next commit.
    Commit(Arc<Commit>),
    /// A page the host draws, such as a review's conversation: `rows` rows
    /// in part 0, no diff and no files of its own. It's always the first
    /// entry (see [`Document::set_page`]).
    Page {
        /// At least 1.
        rows: usize,
    },
    /// A file the diff doesn't list, shown whole for what's on it, such as
    /// comments: its change is [`FileChange::unchanged`], which the host
    /// diffs from the repository and prepares with
    /// [`super::Prepare::unchanged`], so every line is unchanged. These come
    /// after the diff's files (see [`Document::show_unchanged`]).
    Unchanged,
}

/// An entry's diff.
pub enum State {
    /// Not prepared yet, or dropped by [`Document::evict`].
    Pending,
    /// Prepared.
    Loaded(Box<File>),
    /// Preparing it failed, for this reason.
    Failed(String),
}

/// A file's rendered view beside its line diff. Whichever of the two shows
/// is the entry's [`State::Loaded`]; this holds the other. Entries hold it
/// boxed, and only once a rendering was stored: a host walking every entry
/// pays a pointer for a file that never renders.
pub enum Render {
    /// The rendered view shows; this is the line diff.
    Shown(Box<File>),
    /// The line diff shows; this is the rendered view.
    Hidden(Box<File>),
    /// The renderer failed on this diff, for this reason. It isn't wanted
    /// again until the diff changes or rendered views are turned on again.
    Failed(String),
}

impl Render {
    fn bytes(&self) -> usize {
        match self {
            Self::Shown(file) | Self::Hidden(file) => file.bytes(),
            Self::Failed(_) => 0,
        }
    }
}

/// An entry of a [`Document`].
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent flags of one file, not a state machine"
)]
pub struct Entry<P> {
    change: FileChange,
    source: Source<P>,
    state: State,
    /// `None` until a rendering is stored, and again once the diff it was
    /// of is replaced.
    render: Option<Box<Render>>,
    /// The file changed since its diff was loaded.
    stale: bool,
    /// Kept apart from the diff, which eviction may drop.
    counts: Option<Counts>,
    /// The file changed since `counts` were taken.
    recount: bool,
    viewed: bool,
    /// What's revealed of each gap, by gap. Gaps past its end show nothing,
    /// so a file nothing was revealed in holds none.
    reveal: Vec<Reveal>,
    /// What [`Document::reveal_lines`] revealed of each of the line diff's
    /// gaps, kept apart from `reveal`: a swap back to the line diff shows
    /// these again without what the user revealed.
    host_reveal: Vec<Reveal>,
    /// Where the file was left when another was shown.
    saved: Option<Pos>,
    /// Shown with a diff since it arrived: opened at its first change, or
    /// left where it was since. Diffs reloaded later never move the view.
    placed: bool,
    /// The frame that last showed it (see [`Document::touch`]).
    last_shown: u64,
    version: u64,
    /// The choices made on a conflicted file's regions, which its diff
    /// previews once loaded (see [`Document::choose`]). Kept apart from the
    /// diff, which eviction may drop.
    choices: Option<Arc<Choices>>,
    /// The host's blocks among its rows (see [`Document::set_blocks`]);
    /// kept apart from the diff, which eviction may drop.
    blocks: Option<Box<Blocks>>,
    /// The host holds blocks for it, given or not (see
    /// [`Document::set_held_blocks`]).
    held_blocks: bool,
}

/// A version no entry has had (see [`Entry::version`]).
fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl<P> Entry<P> {
    fn new(change: FileChange, source: Source<P>) -> Self {
        let counts = match &source {
            Source::Repo => None,
            Source::Piped(file) => Some(file.counts),
            Source::Commit(_) | Source::Page { .. } | Source::Unchanged => Some(Counts::default()),
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
            host_reveal: Vec::new(),
            saved: None,
            placed: false,
            last_shown: 0,
            version: next_version(),
            choices: None,
            blocks: None,
            held_blocks: false,
        };
        // A sheet is never diffed, so nothing would place it.
        entry.placed = entry.is_sheet();
        entry
    }

    /// The file, as listed; a commit's path is its label.
    #[inline]
    #[must_use]
    pub fn change(&self) -> &FileChange {
        &self.change
    }

    /// Where the entry comes from.
    #[inline]
    #[must_use]
    pub fn source(&self) -> &Source<P> {
        &self.source
    }

    /// Its diff, or why there's none.
    #[inline]
    #[must_use]
    pub fn state(&self) -> &State {
        &self.state
    }

    /// The diff shown, once prepared.
    #[inline]
    #[must_use]
    pub fn file(&self) -> Option<&File> {
        match &self.state {
            State::Loaded(file) => Some(file),
            State::Pending | State::Failed(_) => None,
        }
    }

    /// The rendered view or line diff beside the one shown, or why the
    /// renderer failed; `None` until one is stored.
    #[inline]
    #[must_use]
    pub fn render(&self) -> Option<&Render> {
        self.render.as_deref()
    }

    /// The file changed since its diff was loaded: the old diff shows,
    /// keeping the position in it, until the new one is loaded.
    #[inline]
    #[must_use]
    pub fn is_stale(&self) -> bool {
        self.stale
    }

    /// Its added and removed lines, once known. They outlive the diff, and
    /// a file that changed keeps its old ones until new ones are set.
    #[inline]
    #[must_use]
    pub fn counts(&self) -> Option<Counts> {
        self.counts
    }

    /// Marked viewed (see [`Document::set_viewed`]).
    #[inline]
    #[must_use]
    pub fn viewed(&self) -> bool {
        self.viewed
    }

    /// Unique to this entry and its content: new whenever the file may have
    /// changed. Work asked for an entry is current while the entry has the
    /// version it was asked for, wherever the entry moved in the list since
    /// (see [`Document::is_current`]).
    #[inline]
    #[must_use]
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Whether the host should prepare this file's diff and
    /// [`Document::load`] it.
    #[inline]
    #[must_use]
    pub fn wants_diff(&self) -> bool {
        !self.is_sheet() && (self.stale || matches!(self.state, State::Pending))
    }

    /// Not a file: a commit's header, a piped text block, or a page.
    #[inline]
    #[must_use]
    pub fn is_commit(&self) -> bool {
        matches!(self.source, Source::Commit(_) | Source::Page { .. })
    }

    /// The page the host draws (see [`Source::Page`]).
    #[inline]
    #[must_use]
    pub fn is_page(&self) -> bool {
        matches!(self.source, Source::Page { .. })
    }

    /// Shown as rows in part 0 rather than hunks: a commit, a page, or a
    /// piped combined diff, which is shown as it came.
    #[inline]
    #[must_use]
    pub fn is_sheet(&self) -> bool {
        match &self.source {
            Source::Commit(_) | Source::Page { .. } => true,
            Source::Piped(file) => file.combined.is_some(),
            Source::Repo | Source::Unchanged => false,
        }
    }

    /// A file the diff doesn't list, shown whole (see [`Source::Unchanged`]).
    #[inline]
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self.source, Source::Unchanged)
    }

    /// The status to show: a piped file's is its patch header's.
    #[must_use]
    pub fn status(&self) -> FileStatus {
        match &self.source {
            Source::Piped(file) => file.status,
            Source::Repo | Source::Commit(_) | Source::Page { .. } | Source::Unchanged => {
                file_status(&self.change.status)
            }
        }
    }

    /// Whether the host should count this file's lines and
    /// [`Document::set_counts`] them.
    #[inline]
    #[must_use]
    pub fn wants_counts(&self) -> bool {
        self.counts.is_none() || self.recount
    }

    /// The line diff to render, when this file has one and no rendering or
    /// failure for it yet. A piped file has only its hunks: nothing to
    /// render.
    #[must_use]
    pub fn wants_render(&self) -> Option<&Arc<FileDiff>> {
        match (&self.state, &self.render, &self.source) {
            // A conflicted file shows its conflicts, not a rendering.
            (State::Loaded(file), None, Source::Repo)
                if !self.stale && file.conflicted().is_none() =>
            {
                Some(file.diff())
            }
            _ => None,
        }
    }

    /// The choices made on its conflicts, which the host's diff of it
    /// previews: see [`super::Prepare::conflicted`].
    #[inline]
    #[must_use]
    pub fn choices(&self) -> Option<&Arc<Choices>> {
        self.choices.as_ref()
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
        if matches!(self.state, State::Loaded(_)) {
            match self.render.as_deref() {
                Some(Render::Hidden(_)) => self.reveal.clone_from(&self.host_reveal),
                _ => self.reveal.clear(),
            }
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

/// The search's matches in one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The entry.
    pub file: usize,
    /// The lines that were hidden when they were found (see
    /// [`Document::hidden_lines`]): revealing more means searching again.
    pub hidden: Vec<Range<usize>>,
    /// The version of the file's blocks they were found in (see
    /// [`Document::blocks_version`]): other blocks mean searching again.
    pub blocks_version: u64,
    /// The matches.
    pub matches: Arc<Matches>,
    /// The one the view last went to.
    pub current: Option<usize>,
}

/// Where the view goes after a call that moved the position or changed the
/// rows under it. The core settles what it can; the rest needs the host's
/// screen (heights, clamping to the bottom), so the host finishes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Settle {
    /// Nothing more than keeping the position on rows that exist.
    Keep,
    /// Put this row of the file shown, which starts a block of changes, at
    /// the top, then call [`Document::landed`] with it.
    Land(Pos),
    /// `file`, now shown, shows a diff for the first time: open it at its
    /// first change ([`Document::open`]). With `landing` the next change
    /// counts from that one even when the view stays at the file's top.
    Open {
        /// The entry.
        file: usize,
        /// The view moved there by change.
        landing: bool,
    },
    /// Put the first (or `last`) block of changes of `file`, now shown and
    /// diffed, at the top ([`Document::edge`]).
    Edge {
        /// The entry.
        file: usize,
        /// The last block rather than the first.
        last: bool,
    },
    /// `file`, now shown, isn't diffed yet. Once the host has put the view
    /// in place, it sets [`Document::pending_land`] to that position: when
    /// the diff is loaded with the view still there, the view goes to the
    /// file's first (or `last`) change.
    Wait {
        /// The entry.
        file: usize,
        /// The last change rather than the first.
        last: bool,
    },
}

/// Where [`Document::step_block`] goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockStep {
    /// To block `id`, whose first row is at `pos`.
    At {
        /// Its first row.
        pos: Pos,
        /// The block.
        id: BlockId,
    },
    /// To the entry `file`, the nearest that way with blocks, which may not
    /// be diffed yet: the host shows it, and once it is, goes to its first
    /// block (or last, going back) with [`Document::edge_block`].
    File(usize),
}

/// Work a host does for the rows it shows (see [`Document::missing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// Hunk `n` of the line diff has no emphasis:
    /// [`super::Prepare::emphasis`], then [`Document::set_emphasis`].
    Emphasis(usize),
    /// Gap `n` shows lines and has no syntax runs:
    /// [`super::Prepare::gap_syntax`], then [`Document::set_gap_syntax`].
    GapSyntax(usize),
    /// The file's highlighting is prepared and the definitions it leaves
    /// to find, for the scope line, aren't yet: [`super::Prepare::scopes`],
    /// which stores them in the file's syntax slot.
    Scopes,
    /// The syntax diff shows and the file has none yet:
    /// [`super::Prepare::structural`], then [`Document::set_structural`].
    Structural,
    /// Rendered views show and the file has none (see
    /// [`Entry::wants_render`]); stored with [`Document::set_rendered`].
    Render,
}

/// Every entry of a diff, one shown at a time, and the position in it. Each
/// file keeps its own place while others are shown; moving stops at a
/// file's edges.
///
/// Calls that build rows take the file's arrangement: a function from a
/// hunk's numbers to how its lines are arranged (a host decides split or
/// unified by its width). Rows are cached per arrangement, so a host passes
/// the same decision for a file until its layout changes. Such a call only
/// builds rows of the one file it's about.
pub struct Document<P> {
    files: Vec<Entry<P>>,
    /// The entries that are commits, in order.
    commits: Vec<usize>,
    /// The file shown and its row at the top. A host that moves it keeps it
    /// on a row that exists ([`Document::normalize`]).
    pub pos: Pos,
    loaded_bytes: usize,
    /// Show syntax diffs where they're ready.
    pub structural: bool,
    rendered: bool,
    /// Files show their full text, all but piped ones.
    pub full: bool,
    /// The change the view last landed on, and the position it took.
    landed: Option<(Pos, Pos)>,
    /// A file not diffed yet was opened by change at this position (see
    /// [`Settle::Wait`]): once its diff is loaded, and if the view is still
    /// there, the view goes to its first change (false) or its last (true).
    /// A host clears it on any input that moves the view.
    pub pending_land: Option<(Pos, bool)>,
    /// The search's matches in the file they're shown for.
    pub search: Option<Found>,
    /// The last heading search for [`Document::context`].
    headings: Headings,
}

impl<P> Default for Document<P> {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            commits: Vec::new(),
            pos: Pos::default(),
            loaded_bytes: 0,
            structural: false,
            rendered: false,
            full: false,
            landed: None,
            pending_land: None,
            search: None,
            headings: Headings::default(),
        }
    }
}

impl<P> Document<P> {
    /// The entries, in order.
    #[inline]
    #[must_use]
    pub fn files(&self) -> &[Entry<P>] {
        &self.files
    }

    /// The entries that are commits, in order.
    #[inline]
    #[must_use]
    pub fn commits(&self) -> &[usize] {
        &self.commits
    }

    /// Whether rendered views show where they're ready.
    #[inline]
    #[must_use]
    pub fn rendered(&self) -> bool {
        self.rendered
    }

    /// Adds an entry at the end, not diffed yet. A host lists the diff's
    /// files before it adds any [`Source::Unchanged`] entry, which come last.
    pub fn push(&mut self, change: FileChange, source: Source<P>) {
        if matches!(source, Source::Commit(_)) {
            self.commits.push(self.files.len());
        }
        self.files.push(Entry::new(change, source));
    }

    /// The entry of `change`'s path, adding `change`, a file the diff
    /// doesn't list (see [`FileChange::unchanged`]), as a
    /// [`Source::Unchanged`] entry after the others when there's none. It
    /// stays through [`Document::replace`], until a refreshed list has its
    /// path: then it's that file's entry.
    pub fn show_unchanged(&mut self, change: FileChange) -> usize {
        if let Some(file) = self
            .files
            .iter()
            .position(|e| !e.is_commit() && e.change.path == change.path)
        {
            return file;
        }
        self.files.push(Entry::new(change, Source::Unchanged));
        self.files.len() - 1
    }

    /// Where the [`Source::Unchanged`] entries after the diff's start.
    fn listed_end(&self) -> usize {
        self.files.len()
            - self
                .files
                .iter()
                .rev()
                .take_while(|e| e.is_unchanged())
                .count()
    }

    /// Marks entry `file` viewed, or not.
    pub fn set_viewed(&mut self, file: usize, viewed: bool) {
        if let Some(entry) = self.files.get_mut(file) {
            entry.viewed = viewed;
        }
    }

    /// The commit entry `file` is under, as an index into
    /// [`Document::commits`].
    #[must_use]
    pub fn commit_of(&self, file: usize) -> Option<usize> {
        if file >= self.listed_end() {
            return None;
        }
        self.commits.partition_point(|&c| c <= file).checked_sub(1)
    }

    /// The files of the commit at entry `commit`.
    #[must_use]
    pub fn commit_files(&self, commit: usize) -> Range<usize> {
        let next = self.commits.partition_point(|&c| c <= commit);
        commit + 1
            ..self
                .commits
                .get(next)
                .copied()
                .unwrap_or_else(|| self.listed_end())
    }

    /// Puts a page of `rows` rows (see [`Source::Page`]) first in the list,
    /// or with `None` takes away the one there; a page there already takes
    /// the new count of rows. Returns how far every other entry moved: 1
    /// when the page was put first, -1 when it was taken away, else 0. The
    /// position moves with its file, or from the page taken away to the
    /// first file; the search's file moves too, and other positions held in
    /// files are dropped ([`Document::forget_positions`]). The host
    /// normalizes the position.
    pub fn set_page(&mut self, page: Option<(FileChange, usize)>) -> isize {
        let present = self.files.first().is_some_and(Entry::is_page);
        let moved: isize = match (page, present) {
            (Some((_, rows)), true) => {
                self.files[0].source = Source::Page { rows };
                return 0;
            }
            (None, false) => return 0,
            (Some((change, rows)), false) => {
                self.files
                    .insert(0, Entry::new(change, Source::Page { rows }));
                // An empty list's position becomes the page's.
                if self.files.len() > 1 {
                    self.pos.file += 1;
                }
                1
            }
            (None, true) => {
                self.files.remove(0);
                if self.pos.file == 0 {
                    self.pos = Pos::default();
                } else {
                    self.pos.file -= 1;
                }
                -1
            }
        };
        for c in &mut self.commits {
            *c = c.saturating_add_signed(moved);
        }
        if let Some(found) = &mut self.search {
            found.file = found.file.saturating_add_signed(moved);
        }
        self.forget_positions();
        moved
    }

    /// Commits and the page at the head of the list (`dffy show`'s, or a
    /// review's), which stay first when the files after them are sorted or
    /// replaced.
    fn head(&self) -> usize {
        self.files.iter().take_while(|e| e.is_commit()).count()
    }

    /// Puts the files in path order, keeping the position on the same file;
    /// [`Source::Unchanged`] entries stay last, as they were added.
    /// Returns whether any file moved; positions held in other files are
    /// dropped then ([`Document::forget_positions`]).
    pub fn sort_by_path(&mut self) -> bool {
        let head = self.head();
        let end = self.listed_end().max(head);
        let files = &mut self.files[head..end];
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

    /// Drops the change landed on and [`Document::pending_land`], which
    /// hold positions in files that moved or changed.
    pub fn forget_positions(&mut self) {
        self.pending_land = None;
        self.landed = None;
    }

    /// Takes the refreshed file list, in path order. Files still listed
    /// keep their diffs, counts, viewed marks and revealed lines; diffs and
    /// counts are marked for redoing when they may have changed.
    /// [`Source::Unchanged`] entries stay after them, their diffs marked for
    /// redoing when the refresh may have touched them, but for one the list
    /// now has, which becomes its entry. The
    /// position stays on the same file, or moves to the one in its place
    /// when it's gone. Returns that file when it has a diff it never showed:
    /// the host opens it ([`Settle::Open`]). Positions held in files are
    /// dropped ([`Document::forget_positions`]).
    pub fn replace(&mut self, update: &Update) -> Option<usize> {
        let head: Vec<Entry<P>> = self.files.drain(..self.head()).collect();
        let n = head.len();
        let on_head = self.pos.file < n;
        let kept = self.pos;
        self.pos.file = self.pos.file.saturating_sub(n);
        let open = self.replace_files(update);
        self.files.splice(0..0, head);
        if on_head {
            self.pos = kept;
            // Opened under the commit shown: it opens at its top later.
            if let Some(file) = open {
                self.files[file + n].placed = true;
            }
            return None;
        }
        self.pos.file += n;
        open.map(|file| file + n)
    }

    fn replace_files(&mut self, update: &Update) -> Option<usize> {
        let anchor = self.files.get(self.pos.file).map(|e| e.change.path.clone());
        self.forget_positions();
        // The rename pass turns an addition into a rename and drops the
        // deletion it pairs with; a position on either stays on the pair.
        let renamed_from = |c: &FileChange, path: &BString| matches!(&c.status, Status::Renamed { from, .. } if from == path);
        let unchanged: Vec<BString> = (self.files.iter())
            .filter(|e| e.is_unchanged())
            .map(|e| e.change.path.clone())
            .collect();
        let mut old: HashMap<BString, Entry<P>> = std::mem::take(&mut self.files)
            .into_iter()
            .map(|e| (e.change.path.clone(), e))
            .collect();
        self.files = update
            .files
            .iter()
            .map(|change| match old.remove(&change.path) {
                Some(mut entry) => {
                    if entry.is_unchanged() {
                        entry.source = Source::Repo;
                    }
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
        for path in unchanged {
            if let Some(mut entry) = old.remove(&path) {
                if update.is_dirty(&entry.change) {
                    entry.stale = !matches!(entry.state, State::Pending);
                    entry.version = next_version();
                }
                self.files.push(entry);
            }
        }
        for gone in old.into_values() {
            self.loaded_bytes -= gone.render_bytes();
            if let State::Loaded(file) = gone.state {
                self.loaded_bytes -= file.bytes();
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
            return None;
        }
        let file = self.pos.file.min(self.files.len().saturating_sub(1));
        self.pos = Pos {
            file,
            ..self
                .files
                .get(file)
                .and_then(|e| e.saved)
                .unwrap_or_default()
        };
        // Shown in its place: opened now if it can be, so a later reload of
        // it doesn't move the view.
        self.files
            .get(file)
            .is_some_and(|e| !e.placed && !matches!(e.state, State::Pending))
            .then_some(file)
    }

    /// Marks every file to be diffed and counted again, the settings having
    /// changed; what's shown stays until the new diffs are loaded.
    pub fn restale(&mut self) {
        for entry in &mut self.files {
            if !matches!(entry.state, State::Pending) {
                entry.stale = true;
            }
            entry.recount = true;
            entry.version = next_version();
        }
    }

    /// Stores the host's diff for a file that wants one (see
    /// [`Entry::wants_diff`]); `None` when it doesn't. A file whose hunks
    /// moved loses its revealed lines; one reloaded after eviction keeps
    /// them. Its rendering, of the diff replaced, is dropped.
    pub fn load(&mut self, file: usize, result: Result<Box<File>, String>) -> Option<Settle> {
        let entry = self.files.get_mut(file)?;
        if !entry.wants_diff() {
            return None;
        }
        let rendered = matches!(entry.render.as_deref(), Some(Render::Shown(_)));
        self.loaded_bytes -= entry.drop_render();
        let mut keep_reveal = !entry.stale && !rendered;
        if let State::Loaded(old) = &entry.state {
            self.loaded_bytes -= old.bytes();
            if let Ok(new) = &result {
                keep_reveal |= old.gaps() == new.gaps();
            }
        }
        if !keep_reveal {
            entry.reveal.clear();
            entry.host_reveal.clear();
        }
        entry.stale = false;
        entry.recount = false;
        entry.state = match result {
            Ok(loaded) => {
                self.loaded_bytes += loaded.bytes();
                entry.counts = Some(loaded.counts());
                // Choices the new diff doesn't preview no longer fit the
                // file: its conflicts changed.
                let previewed = loaded.conflicted().and_then(super::Conflicted::choices);
                if entry
                    .choices
                    .as_ref()
                    .is_some_and(|c| previewed.is_none_or(|p| p != c))
                {
                    entry.choices = None;
                }
                // A diff of the same file may cut its gaps differently.
                for reveal in [&mut entry.reveal, &mut entry.host_reveal] {
                    reveal.truncate(loaded.gaps().len());
                    for (reveal, gap) in reveal.iter_mut().zip(loaded.gaps()) {
                        reveal.settle(gap.len);
                    }
                }
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
        Some(if self.pos.file == file && !self.files[file].placed {
            Settle::Open {
                file,
                landing: pending.is_some_and(|(at, _)| at == self.pos),
            }
        } else if let Some((at, last)) = pending
            && self.pos == at
        {
            Settle::Edge { file, last }
        } else {
            Settle::Keep
        })
    }

    /// Makes `choice` on region `region` of `file`, a conflicted file whose
    /// diff is loaded, or with `None` takes its choice back. The file then
    /// wants its diff again ([`Entry::wants_diff`]): the preview of its
    /// choices, or its line diff once none is left. The diff shown stays,
    /// with the position, until that's loaded. Returns the choices now.
    ///
    /// # Errors
    ///
    /// Why the choice can't be made, in words for the user: the file has
    /// no such region or can't be resolved by region, or the choice is the
    /// base of a region without one.
    pub fn choose(
        &mut self,
        file: usize,
        region: usize,
        choice: Option<Choice>,
    ) -> Result<Option<Arc<Choices>>, String> {
        let entry = self
            .files
            .get_mut(file)
            .ok_or_else(|| "no such file".to_owned())?;
        let conflicted = entry
            .file()
            .and_then(File::conflicted)
            .ok_or_else(|| "this file has no conflicts".to_owned())?;
        if let Some(why) = conflicted.refused() {
            return Err(why.to_owned());
        }
        let regions = conflicted.regions();
        let Some(at) = regions.get(region) else {
            return Err("no conflict here".to_owned());
        };
        if choice.is_some_and(|c| at.kept(c).is_none()) {
            return Err(
                "no base in this conflict: merge.conflictStyle diff3 writes one".to_owned(),
            );
        }
        let mut choices = match &entry.choices {
            Some(c) => Choices::clone(c),
            None => Choices::new(regions.to_vec()),
        };
        if choices.picks[region] == choice {
            return Ok(entry.choices.clone());
        }
        choices.picks[region] = choice;
        entry.choices = (choices.chosen() > 0).then(|| Arc::new(choices));
        entry.stale = true;
        entry.version = next_version();
        Ok(entry.choices.clone())
    }

    /// Drops `file`'s choices, as once they were written: it wants its
    /// diff again.
    pub fn forget_choices(&mut self, file: usize) {
        if let Some(entry) = self.files.get_mut(file)
            && entry.choices.take().is_some()
        {
            entry.stale = true;
            entry.version = next_version();
        }
    }

    /// The row a view on conflict region `region` of `file` puts at its
    /// top: the region's first line, or in a preview the first line it
    /// became, or its `<<<<<<<` line when it became nothing.
    pub fn conflict_pos(
        &mut self,
        file: usize,
        region: usize,
        arrange: impl Arrange,
    ) -> Option<Pos> {
        let m = self.file(file)?.conflict_match(region)?;
        self.match_pos(file, &m, arrange)
    }

    /// Whether `file` is still at `version`: work asked for it then is
    /// current.
    #[must_use]
    pub fn is_current(&self, file: usize, version: u64) -> bool {
        self.files.get(file).is_some_and(|e| e.version == version)
    }

    /// Stores counts unless the file already has current ones. Returns
    /// whether they were stored.
    pub fn set_counts(&mut self, file: usize, counts: Counts) -> bool {
        match self.files.get_mut(file) {
            Some(entry) if entry.wants_counts() => {
                entry.counts = Some(counts);
                entry.recount = false;
                true
            }
            _ => false,
        }
    }

    /// Stores hunk `hunk`'s emphasis (see [`File::set_emphasis`]) if `diff`
    /// is still the diff loaded for `file`.
    pub fn set_emphasis(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        hunk: usize,
        emphasis: Vec<Emphasis>,
        syntax: Option<HunkSyntax>,
    ) {
        if let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) {
            loaded.set_emphasis(diff, hunk, emphasis, syntax);
        }
    }

    /// Stores gap `gap`'s syntax runs (see [`File::set_gap_syntax`]) if
    /// `diff` is still the diff loaded for `file`.
    pub fn set_gap_syntax(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        gap: usize,
        spans: Option<Spans>,
    ) {
        if let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) {
            loaded.set_gap_syntax(diff, gap, spans);
        }
    }

    /// Stores the syntax diff of `diff` (see [`File::set_structural`]) if
    /// that's still the diff loaded for `file`. Returns whether it was
    /// stored; positions stay valid either way.
    pub fn set_structural(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Option<Result<Structural, Unavailable>>,
    ) -> bool {
        let Some(State::Loaded(loaded)) = self.files.get_mut(file).map(|e| &mut e.state) else {
            return false;
        };
        let before = loaded.bytes();
        let stored = loaded.set_structural(diff, result);
        self.loaded_bytes += loaded.bytes() - before;
        stored
    }

    /// Stores a rendering of `diff` (see [`File::rendered`]), or why there's
    /// none, if that's still the line diff loaded for `file` and it has none
    /// yet; `None` when it isn't. While rendered views show, it shows at
    /// once; the view then opens it when it's the file shown.
    pub fn set_rendered(
        &mut self,
        file: usize,
        diff: &Arc<FileDiff>,
        result: Result<Box<File>, String>,
    ) -> Option<Settle> {
        let entry = self.files.get_mut(file)?;
        if entry.wants_render().is_none_or(|d| !Arc::ptr_eq(d, diff)) {
            return None;
        }
        match result {
            Ok(loaded) => {
                self.loaded_bytes += loaded.bytes();
                entry.render = Some(Box::new(Render::Hidden(loaded)));
                if self.rendered {
                    return Some(self.show_render(file));
                }
            }
            Err(reason) => entry.render = Some(Box::new(Render::Failed(reason))),
        }
        Some(Settle::Keep)
    }

    /// Swaps `file` between its line diff and its rendered view. The file
    /// shown opens anew.
    fn show_render(&mut self, file: usize) -> Settle {
        if self.search.as_ref().is_some_and(|m| m.file == file) {
            self.search = None;
        }
        self.files[file].swap_render();
        if self.pos.file != file {
            return Settle::Keep;
        }
        self.forget_positions();
        Settle::Open {
            file,
            landing: false,
        }
    }

    /// Turns rendered views on or off for every file that has one. Turning
    /// them on gives files the renderer failed on another try. Returns how
    /// the file shown settles when it swapped.
    pub fn set_rendered_mode(&mut self, on: bool) -> Settle {
        self.rendered = on;
        let mut settle = Settle::Keep;
        for file in 0..self.files.len() {
            match (self.files[file].render.as_deref(), on) {
                (Some(Render::Hidden(_)), true) | (Some(Render::Shown(_)), false) => {
                    if let open @ Settle::Open { .. } = self.show_render(file) {
                        settle = open;
                    }
                }
                (Some(Render::Failed(_)), true) => self.files[file].render = None,
                _ => {}
            }
        }
        settle
    }

    /// Drops rendered views drawn for another width than `width`, putting
    /// the line diff back until a new rendering is stored. When the file
    /// shown loses the one it showed, positions are dropped
    /// ([`Document::forget_positions`]) and it opens anew.
    pub fn forget_renders(&mut self, width: usize) -> Settle {
        let mut settle = Settle::Keep;
        for file in 0..self.files.len() {
            let entry = &self.files[file];
            let shown = matches!(entry.render.as_deref(), Some(Render::Shown(_)));
            let stale = match entry.render.as_deref() {
                Some(Render::Shown(_)) => match &entry.state {
                    State::Loaded(shown) => {
                        shown.rendered().map(super::Rendered::width) != Some(width)
                    }
                    State::Pending | State::Failed(_) => true,
                },
                Some(Render::Hidden(r)) => r.rendered().map(super::Rendered::width) != Some(width),
                Some(Render::Failed(_)) | None => false,
            };
            if stale {
                self.loaded_bytes -= self.files[file].drop_render();
                if shown && self.pos.file == file {
                    self.forget_positions();
                    settle = Settle::Open {
                        file,
                        landing: false,
                    };
                }
            }
        }
        settle
    }

    /// Notes that `file` was shown in frame `frame`, for
    /// [`Document::evict`].
    #[inline]
    pub fn touch(&mut self, file: usize, frame: u64) {
        if let Some(entry) = self.files.get_mut(file) {
            entry.last_shown = frame;
        }
    }

    /// Drops the least recently shown diffs outside `keep` until the loaded
    /// ones hold at most `budget` bytes; they're wanted again
    /// ([`Entry::wants_diff`]) if they come back. Entries in `keep` stay
    /// whatever their size, so the files on screen are never dropped and
    /// prepared again in a loop.
    pub fn evict(&mut self, budget: usize, keep: &Range<usize>) {
        while self.loaded_bytes > budget {
            let Some(i) = self
                .files
                .iter()
                .enumerate()
                .filter(|(i, e)| !keep.contains(i) && matches!(e.state, State::Loaded(_)))
                .min_by_key(|(_, e)| e.last_shown)
                .map(|(i, _)| i)
            else {
                break;
            };
            self.loaded_bytes -= self.files[i].drop_render();
            if let State::Loaded(loaded) =
                std::mem::replace(&mut self.files[i].state, State::Pending)
            {
                self.loaded_bytes -= loaded.bytes();
            }
            self.files[i].stale = false;
        }
    }

    /// `file`'s diff, once loaded.
    #[inline]
    #[must_use]
    pub fn file(&self, file: usize) -> Option<&File> {
        self.files.get(file)?.file()
    }

    /// `file`'s line diff, or the one its rendered view was made from.
    #[inline]
    #[must_use]
    pub fn diff_of(&self, file: usize) -> Option<&Arc<FileDiff>> {
        self.file(file).map(File::diff)
    }

    /// Whether `entry` shows its full text. A piped file never does, and a
    /// [`Source::Unchanged`] one always does.
    #[inline]
    #[must_use]
    pub fn is_full(&self, entry: &Entry<P>) -> bool {
        match entry.source {
            Source::Piped(_) => false,
            Source::Unchanged => true,
            Source::Repo | Source::Commit(_) | Source::Page { .. } => self.full,
        }
    }

    /// Whether the file shown is a piped one, which has only its hunks.
    #[must_use]
    pub fn is_partial(&self) -> bool {
        self.files
            .get(self.pos.file)
            .is_some_and(|e| matches!(e.source, Source::Piped(_)))
    }

    /// `file`'s diff and how it's shown, for queries that build no rows.
    fn view(&self, file: usize) -> Option<(&File, Shape<'_, ()>)> {
        let entry = self.files.get(file)?;
        let State::Loaded(loaded) = &entry.state else {
            return None;
        };
        let shape = Shape {
            structural: self.structural,
            full: self.full || matches!(entry.source, Source::Unchanged),
            reveal: &entry.reveal,
            arrange: &(),
            blocks: entry.blocks.as_deref(),
        };
        Some((loaded, shape))
    }

    /// Runs `f` on `file`'s diff and how it's shown with `arrange`; `None`
    /// while it has no diff. What `f` adds to the diff's memory (rows for a
    /// second arrangement) counts toward [`Document::loaded_bytes`].
    #[inline]
    fn with_file<A: Arrange, R>(
        &mut self,
        file: usize,
        arrange: &A,
        f: impl FnOnce(&mut File, &Shape<'_, A>) -> R,
    ) -> Option<R> {
        let entry = self.files.get_mut(file)?;
        let State::Loaded(loaded) = &mut entry.state else {
            return None;
        };
        let shape = Shape {
            structural: self.structural,
            full: self.full || matches!(entry.source, Source::Unchanged),
            reveal: &entry.reveal,
            arrange,
            blocks: entry.blocks.as_deref(),
        };
        let before = loaded.bytes();
        let result = f(loaded, &shape);
        self.loaded_bytes += loaded.bytes() - before;
        Some(result)
    }

    /// The blocks of an entry without a diff to place them in, a sheet or a
    /// file not diffed yet or whose diff failed: those under its first row
    /// and under its last, in part 0. `None` for any other entry, and one
    /// without blocks.
    #[inline]
    fn edge_layout<A: Arrange>(&self, file: usize, arrange: &A) -> Option<Ref<'_, Placed>> {
        let entry = self.files.get(file)?;
        let blocks = entry.blocks.as_deref()?;
        if !entry.is_sheet() && matches!(entry.state, State::Loaded(_)) {
            return None;
        }
        blocks.edges(self.sheet_rows(file).unwrap_or(1), arrange)
    }

    /// Runs `f` on part `part` of `file`'s blocks, placed; `None` while it
    /// has none there.
    fn with_layout<A: Arrange, R>(
        &mut self,
        file: usize,
        part: usize,
        arrange: &A,
        f: impl FnOnce(&Placed) -> R,
    ) -> Option<R> {
        if let Some(edges) = self.edge_layout(file, arrange) {
            return (part == 0).then(|| f(&edges));
        }
        self.with_file(file, arrange, |loaded, shape| {
            loaded.ensure_part(shape, part);
            loaded.placed(shape, part).map(f)
        })?
    }

    /// Parts of `file`, part 0 included.
    #[inline]
    #[must_use]
    pub fn parts(&self, file: usize) -> usize {
        self.file(file).map_or(1, File::parts)
    }

    /// The first part the position may be on in `file`: part 0 holds rows
    /// only for a sheet or a file without segments.
    #[inline]
    #[must_use]
    pub fn first_part(&self, file: usize) -> usize {
        let sheet = self.files.get(file).is_some_and(Entry::is_sheet);
        usize::from(!sheet && self.parts(file) > 1)
    }

    /// What part `part` of `file` is; `None` for part 0, past the last and
    /// while it has no diff.
    #[inline]
    #[must_use]
    pub fn segment(&self, file: usize, part: usize) -> Option<Segment> {
        self.file(file)?.segment(part)
    }

    /// The rows of a sheet (see [`Entry::is_sheet`]): a commit's lines, then
    /// a blank row, how many files it has and a row per file; or a combined
    /// diff's lines. `None` for an entry with hunks.
    fn sheet_rows(&self, file: usize) -> Option<usize> {
        let entry = self.files.get(file).filter(|e| e.is_sheet())?;
        match &entry.source {
            Source::Commit(commit) => Some(match commit.id {
                Some(_) => commit.lines.len() + 2 + self.commit_files(file).len(),
                None => commit.lines.len().max(1),
            }),
            Source::Piped(piped) => Some(piped.combined.as_ref().map_or(0, Vec::len).max(1)),
            Source::Page { rows } => Some((*rows).max(1)),
            Source::Repo | Source::Unchanged => None,
        }
    }

    /// Rows in part `part` of `file`, its blocks' included; at least 1.
    #[inline]
    pub fn rows(&mut self, file: usize, part: usize, arrange: impl Arrange) -> usize {
        self.rows_in(file, part, &arrange)
    }

    fn rows_in<A: Arrange>(&mut self, file: usize, part: usize, arrange: &A) -> usize {
        if let Some(edges) = self.edge_layout(file, arrange) {
            return if part == 0 { edges.rows() } else { 1 };
        }
        if part == 0 && self.file(file).is_none() {
            return self.sheet_rows(file).unwrap_or(1);
        }
        self.with_file(file, arrange, |f, shape| f.part_rows(shape, part))
            .unwrap_or(1)
    }

    /// What the row at `pos` shows; `None` past its file's last part and
    /// while it has no diff, except in part 0. Its offset is ignored.
    #[inline]
    pub fn row(&mut self, pos: Pos, arrange: impl Arrange) -> Option<Content<'_>> {
        if pos.part == 0 && self.file(pos.file).is_none() {
            let at = self.edge_layout(pos.file, &arrange);
            return Some(match at.map(|p| p.at(pos.row)) {
                Some(At::Block { id, line, .. }) => Content::Block { id, line },
                Some(At::Parting { id, .. }) => Content::Parting { id },
                Some(At::Own(row)) => Content::Sheet(row),
                None => Content::Sheet(pos.row),
            });
        }
        let (structural, full) = (self.structural, self.full);
        let entry = self.files.get_mut(pos.file)?;
        let State::Loaded(file) = &mut entry.state else {
            return None;
        };
        let shape = Shape {
            structural,
            full: full || matches!(entry.source, Source::Unchanged),
            reveal: &entry.reveal,
            arrange: &arrange,
            blocks: entry.blocks.as_deref(),
        };
        let before = file.bytes();
        file.ensure_part(&shape, pos.part);
        self.loaded_bytes += file.bytes() - before;
        file.row(&shape, pos)
    }

    /// Moves `pos` to the next row of its file, offset 0; false at the end.
    #[inline]
    pub fn next_row(&mut self, pos: &mut Pos, arrange: impl Arrange) -> bool {
        let at = *pos;
        let next = self
            .with_file(at.file, &arrange, |f, shape| {
                let mut at = at;
                f.next_row(shape, &mut at).then_some(at)
            })
            .unwrap_or_else(|| {
                // A sheet's rows, or the blank row of a file without a diff.
                let rows = if at.part == 0 {
                    self.rows_in(at.file, 0, &arrange)
                } else {
                    1
                };
                (at.row + 1 < rows).then_some(Pos {
                    row: at.row + 1,
                    offset: 0,
                    ..at
                })
            });
        let Some(next) = next else {
            return false;
        };
        *pos = next;
        true
    }

    /// Moves `pos` to the previous row of its file, offset 0; false at the
    /// start.
    #[inline]
    pub fn prev_row(&mut self, pos: &mut Pos, arrange: impl Arrange) -> bool {
        let at = *pos;
        let prev = match self.with_file(at.file, &arrange, |f, shape| {
            let mut at = at;
            f.prev_row(shape, &mut at).then_some(at)
        }) {
            Some(prev) => prev,
            None => (at.row > 0).then_some(Pos {
                row: at.row.saturating_sub(1),
                offset: 0,
                ..at
            }),
        };
        let Some(prev) = prev else {
            return false;
        };
        *pos = prev;
        true
    }

    /// Keeps the position's part and row inside its file after rows changed
    /// under it (a diff loaded, the arrangement changed); its offset is the
    /// host's to keep. Returns false for an empty document, whose position
    /// is the default.
    pub fn normalize(&mut self, arrange: impl Arrange) -> bool {
        if self.files.is_empty() {
            self.pos = Pos::default();
            return false;
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
        pos.row = pos.row.min(self.rows(pos.file, pos.part, arrange) - 1);
        self.pos = pos;
        true
    }

    /// Puts the position at the top of the file shown.
    pub fn top(&mut self) {
        self.pos = Pos {
            file: self.pos.file,
            part: self.first_part(self.pos.file),
            ..Pos::default()
        };
    }

    /// Shows `file`; returns whether it moved there. The file shown until
    /// now keeps its place for when it comes back, if it was opened.
    fn show(&mut self, file: usize) -> bool {
        if file >= self.files.len() || file == self.pos.file {
            return false;
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
        true
    }

    /// Shows `file` where it was left, or the first time it has a diff at
    /// its first change ([`Settle::Open`]); `None` when it's the file shown
    /// or there's no such entry. The host normalizes the position, then
    /// settles.
    pub fn goto_file(&mut self, file: usize) -> Option<Settle> {
        if !self.show(file) {
            return None;
        }
        let entry = &self.files[file];
        Some(
            if !entry.placed && (entry.is_sheet() || !matches!(entry.state, State::Pending)) {
                Settle::Open {
                    file,
                    landing: false,
                }
            } else {
                Settle::Keep
            },
        )
    }

    /// Shows `file` from its top, keeping the current file's place.
    fn open_at_top(&mut self, file: usize) {
        if let Some(entry) = self.files.get_mut(file) {
            entry.placed = true;
        }
        self.show(file);
        self.top();
    }

    /// Opens `file`, the file shown, for the first time: the position goes
    /// to its top. Returns its first block of changes, where the host puts
    /// the view unless the change shows from the top anyway; it then calls
    /// [`Document::landed`] with it if the view moved, or if it opened with
    /// `landing`.
    pub fn open(&mut self, file: usize, arrange: impl Arrange) -> Option<Pos> {
        if let Some(entry) = self.files.get_mut(file) {
            entry.placed = true;
        }
        self.pos = Pos {
            file,
            part: self.first_part(file),
            ..Pos::default()
        };
        self.edge(file, false, arrange)
    }

    /// Notes that the view was put at change `change` and is now at the
    /// position: the next and previous change count from `change` while
    /// the view stays there, since a change near the end of a file can't be
    /// put at the top.
    pub fn landed(&mut self, change: Pos) {
        self.landed = Some((change, self.pos));
    }

    /// Where the next and previous change count from (see
    /// [`Document::landed`]).
    fn change_cursor(&self) -> Pos {
        match self.landed {
            Some((change, view)) if view == self.pos => change,
            _ => self.pos,
        }
    }

    /// The file the next and previous change count from: the arrangement
    /// [`Document::next_change`] and [`Document::prev_change`] take is this
    /// file's.
    #[must_use]
    pub fn change_file(&self) -> usize {
        self.change_cursor().file
    }

    /// The next block of changed lines: in the file shown, or past its last
    /// the next file's first, which this shows (a commit has none; `n`
    /// goes on to its files). `arrange` is the arrangement of
    /// [`Document::change_file`]. `None` at the last change of the last
    /// file.
    pub fn next_change(&mut self, arrange: impl Arrange) -> Option<Settle> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        if let Some(Some(at)) =
            self.with_file(start.file, &arrange, |f, shape| f.next_change(shape, start))
        {
            return Some(Settle::Land(at));
        }
        let next = (start.file + 1..self.files.len()).find(|&f| !self.files[f].is_commit())?;
        let pending = matches!(self.files[next].state, State::Pending);
        if self.files[next].placed {
            self.open_at_top(next);
            return Some(if pending {
                Settle::Wait {
                    file: next,
                    last: false,
                }
            } else {
                Settle::Edge {
                    file: next,
                    last: false,
                }
            });
        }
        if pending {
            // Opened as any file is the first time, once diffed.
            self.show(next);
            return Some(Settle::Wait {
                file: next,
                last: false,
            });
        }
        self.files[next].placed = true;
        self.show(next);
        Some(Settle::Open {
            file: next,
            landing: true,
        })
    }

    /// The previous block of changed lines: in the file shown, or before
    /// its first the previous file's last, which this shows. Before the
    /// first file's first change, the position goes to its top and this
    /// returns `None`. `arrange` is as for [`Document::next_change`].
    pub fn prev_change(&mut self, arrange: impl Arrange) -> Option<Settle> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        // Within the row the view is in, below its first screen row, the
        // row's own change counts as before the view.
        if let Some(Some(at)) =
            self.with_file(start.file, &arrange, |f, shape| f.prev_change(shape, start))
        {
            return Some(Settle::Land(at));
        }
        let Some(prev) = (0..start.file).rev().find(|&f| !self.files[f].is_commit()) else {
            self.top();
            return None;
        };
        self.open_at_top(prev);
        Some(if matches!(self.files[prev].state, State::Pending) {
            Settle::Wait {
                file: prev,
                last: true,
            }
        } else {
            Settle::Edge {
                file: prev,
                last: true,
            }
        })
    }

    /// The first (or `last`) block of changes of `file`.
    pub fn edge(&mut self, file: usize, last: bool, arrange: impl Arrange) -> Option<Pos> {
        self.with_file(file, &arrange, |f, shape| f.edge(shape, file, last))?
    }

    /// The first block of changes of `file` whose new-side line number
    /// (1-based) is past `line`, or with none the last block.
    pub fn change_after(&mut self, file: usize, line: usize, arrange: impl Arrange) -> Option<Pos> {
        self.with_file(file, &arrange, |f, shape| f.change_after(shape, file, line))?
    }

    /// The hunk at the position and how many the file has: the one its row
    /// is in, or the last above it.
    #[must_use]
    pub fn hunk_position(&self) -> Option<(usize, usize)> {
        self.file(self.pos.file)?.hunk_position(self.pos)
    }

    /// The new-side line number (1-based) the row at `pos` shows, or the
    /// first after it; 1 while its file has no diff.
    pub fn line_of(&mut self, pos: Pos, arrange: impl Arrange) -> usize {
        self.with_file(pos.file, &arrange, |f, shape| f.line_of(shape, pos))
            .unwrap_or(1)
    }

    /// The old and new lines (0-based) the row at `pos` shows; `None` on a
    /// side without one, and on rows that aren't code.
    #[inline]
    pub fn row_lines(&mut self, pos: Pos, arrange: impl Arrange) -> [Option<usize>; 2] {
        self.with_file(pos.file, &arrange, |f, shape| f.row_lines(shape, pos))
            .unwrap_or([None, None])
    }

    /// The arrangement of the rows of the hunk at `pos`; `None` for a part
    /// that isn't a hunk.
    #[inline]
    pub fn arrangement(&mut self, pos: Pos, arrange: impl Arrange) -> Option<Arrangement> {
        self.with_file(pos.file, &arrange, |f, shape| {
            f.arrangement(shape, pos.part)
        })?
    }

    /// How much of gap `gap` of `file` shows.
    #[must_use]
    pub fn shown(&self, file: usize, gap: usize) -> Option<Shown<'_>> {
        let (loaded, shape) = self.view(file)?;
        Some(loaded.shown(&shape, gap))
    }

    /// The new lines the gaps of `file` hide under their separators, in
    /// order, for [`super::find`]; `None` until it's diffed.
    #[must_use]
    pub fn hidden_lines(&self, file: usize) -> Option<Vec<Range<usize>>> {
        let (loaded, shape) = self.view(file)?;
        Some(loaded.hidden_lines(&shape))
    }

    /// The row of `file` showing match `m`.
    pub fn match_pos(&mut self, file: usize, m: &Match, arrange: impl Arrange) -> Option<Pos> {
        self.with_file(file, &arrange, |f, shape| f.match_pos(shape, file, m))?
    }

    /// What the row at `pos` shows, to find it again once rows change
    /// around it.
    fn anchor<A: Arrange>(&mut self, pos: Pos, arrange: &A) -> Anchor {
        if let Some(anchor) = self.with_file(pos.file, arrange, |f, shape| f.anchor(shape, pos)) {
            return anchor;
        }
        match self.with_layout(pos.file, pos.part, arrange, |p| p.at(pos.row)) {
            Some(At::Block { id, line, after }) => Anchor::Block { id, line, after },
            Some(At::Parting { id, after }) => Anchor::Block { id, line: 0, after },
            Some(At::Own(own)) => Anchor::Own(own),
            None => Anchor::Own(pos.row),
        }
    }

    /// `pos` moved to the row showing `anchor` now.
    fn unanchor<A: Arrange>(&mut self, pos: Pos, anchor: Anchor, arrange: &A) -> Pos {
        if let Some(pos) =
            self.with_file(pos.file, arrange, |f, shape| f.unanchor(shape, pos, anchor))
        {
            return pos;
        }
        let row = self.with_layout(pos.file, pos.part, arrange, |placed| match anchor {
            Anchor::Own(own) | Anchor::Line(own) => placed.row(own),
            Anchor::Block { id, line, after } => match placed.block(id) {
                Some(block) => block.top + line.min(block.rows - 1),
                None => placed.row(after.saturating_sub(1)),
            },
        });
        row.map_or(pos, |row| Pos { row, ..pos })
    }

    /// Runs `change` on the document, then puts the position, `file`'s
    /// saved place and the change the view landed on in it back on the
    /// rows they showed: a gap's line, a block's row, else the same row of
    /// their part.
    fn keep_place<A: Arrange>(&mut self, file: usize, arrange: &A, change: impl FnOnce(&mut Self)) {
        // The saved place's index is stale once the list moved under it
        // (a page put first, a re-sort).
        let saved = self.files[file].saved.map(|p| Pos { file, ..p });
        let here = (self.pos.file == file).then(|| self.anchor(self.pos, arrange));
        let there = saved.map(|p| (p, self.anchor(p, arrange)));
        let landed = (self.landed)
            .filter(|(change, _)| change.file == file)
            .map(|(c, v)| ((c, self.anchor(c, arrange)), (v, self.anchor(v, arrange))));
        change(self);
        if let Some(anchor) = here {
            self.pos = self.unanchor(self.pos, anchor, arrange);
        }
        if let Some((saved, anchor)) = there {
            self.files[file].saved = Some(self.unanchor(saved, anchor, arrange));
        }
        if let Some(((change, c), (view, v))) = landed {
            let change = self.unanchor(change, c, arrange);
            self.landed = Some((change, self.unanchor(view, v, arrange)));
        }
    }

    /// Runs `change` on the document, as when the host lays rows out anew
    /// (another width or arrangement) or the syntax diff turns on or off,
    /// then puts the position back on what it showed as `before` laid rows
    /// out, now that `after` does: the line it showed (the new side's on a
    /// row with both), a block's row, or a header or separator as far
    /// above the line under it; the change the view landed on goes with it.
    /// The file's blocks are measured anew. A position none of those holds
    /// is left for the host to normalize.
    pub fn relayout(
        &mut self,
        before: impl Arrange,
        after: impl Arrange,
        change: impl FnOnce(&mut Self),
    ) {
        let pos = self.pos;
        let spot = (pos.file < self.files.len()).then(|| self.spot(pos, &before));
        let landed = (self.landed.take())
            .filter(|(change, view)| change.file == pos.file && *view == pos)
            .map(|(change, _)| self.spot(change, &before));
        change(self);
        let Some(entry) = self.files.get_mut(pos.file) else {
            return;
        };
        if let Some(blocks) = &mut entry.blocks {
            blocks.remeasure(None);
        }
        let Some(at) = spot
            .flatten()
            .and_then(|s| self.find_spot(pos.file, s, after))
        else {
            return;
        };
        self.pos = Pos {
            offset: pos.offset,
            ..at
        };
        let landed = landed
            .flatten()
            .and_then(|s| self.find_spot(pos.file, s, after));
        self.landed = landed.map(|change| (change, self.pos));
    }

    /// What the row at `pos` shows, for [`Document::relayout`].
    fn spot<A: Arrange>(&mut self, pos: Pos, arrange: &A) -> Option<Spot> {
        match self.row(pos, *arrange)? {
            Content::Block { id, line } => return Some(Spot::Block { id, line }),
            Content::Parting { id } => return Some(Spot::Block { id, line: 0 }),
            _ => {}
        }
        // A separator, then a header: a line is at most two own rows on.
        let mut at = pos;
        let mut down = 0;
        while down < 3 {
            let (side, line) = match self.row_lines(at, *arrange) {
                [_, Some(new)] => (Side::New, new),
                [Some(old), None] => (Side::Old, old),
                [None, None] => {
                    down += usize::from(!self.in_block(at, *arrange));
                    if !self.next_row(&mut at, *arrange) {
                        return None;
                    }
                    continue;
                }
            };
            return Some(Spot::Line { side, line, down });
        }
        None
    }

    /// The row of `file` showing `spot` as `arrange` lays rows out.
    fn find_spot(&mut self, file: usize, spot: Spot, arrange: impl Arrange) -> Option<Pos> {
        match spot {
            Spot::Block { id, line } => self.block_row(file, id, &arrange).map(|(at, rows)| Pos {
                row: at.row + line.min(rows - 1),
                ..at
            }),
            Spot::Line { side, line, down } => {
                let m = Match {
                    side,
                    line: u32::try_from(line).unwrap_or(u32::MAX),
                    start: 0,
                    end: 0,
                };
                let mut at = self.match_pos(file, &m, arrange)?;
                let mut up = 0;
                while up < down && self.prev_row(&mut at, arrange) {
                    up += usize::from(!self.in_block(at, arrange));
                }
                Some(at)
            }
        }
    }

    /// Whether the row at `pos` is a block's or parts two.
    fn in_block(&mut self, pos: Pos, arrange: impl Arrange) -> bool {
        matches!(
            self.row(pos, arrange),
            Some(Content::Block { .. } | Content::Parting { .. })
        )
    }

    /// Stores `reveal` for gap `gap` of `file`, keeping the position and
    /// the file's saved place on their rows.
    fn set_reveal<A: Arrange>(
        &mut self,
        file: usize,
        gap: usize,
        gaps: usize,
        reveal: Reveal,
        arrange: &A,
    ) {
        self.keep_place(file, arrange, |doc| {
            let entry = &mut doc.files[file];
            entry.reveal.resize(gaps, Reveal::default());
            entry.reveal[gap] = reveal;
        });
    }

    /// Reveals more of gap `gap` of `file`, keeping a position in the gap
    /// on its line: 20 lines, next to the hunk below for the gap before the
    /// first hunk, next to the hunk above for the gap after the last, half
    /// of each otherwise; up to 40 hidden lines open whole. A piped file's
    /// gaps never open. Returns whether any line was hidden; the host then
    /// normalizes the position.
    pub fn expand(&mut self, file: usize, gap: usize, arrange: impl Arrange) -> bool {
        let Some((gaps, reveal)) = self.view(file).and_then(|(loaded, shape)| {
            Some((loaded.gaps().len(), loaded.reveal_more(&shape, gap)?))
        }) else {
            return false;
        };
        self.set_reveal(file, gap, gaps, reveal, &arrange);
        true
    }

    /// Reveals the lines of `side` in `lines` (0-based) of `file` that its
    /// gaps hide, with `context` lines around them, leaving the rest of
    /// each gap hidden. Positions in the gaps stay on their lines, as with
    /// [`Document::expand`]. Hunks and piped files are left as they are.
    /// While the rendered view shows, the lines are kept for the line diff
    /// behind it and the view is left as it is. They show whenever the line
    /// diff comes back from the rendered view, which drops what
    /// [`Document::expand`] revealed. Returns whether any line was hidden
    /// in the view shown; the host then normalizes the position.
    pub fn reveal_lines(
        &mut self,
        file: usize,
        side: Side,
        lines: Range<usize>,
        context: usize,
        arrange: impl Arrange,
    ) -> bool {
        self.reveal_lines_in(file, side, &lines, context, &arrange)
    }

    fn reveal_lines_in<A: Arrange>(
        &mut self,
        file: usize,
        side: Side,
        lines: &Range<usize>,
        context: usize,
        arrange: &A,
    ) -> bool {
        let Some(entry) = self.files.get(file) else {
            return false;
        };
        let (loaded, on_lines) = match (&entry.state, entry.render.as_deref()) {
            (State::Loaded(_), Some(Render::Shown(line))) => (line, false),
            (State::Loaded(loaded), _) => (loaded, true),
            (State::Pending | State::Failed(_), _) => return false,
        };
        let gaps = loaded.gaps().len();
        let revealed: Vec<_> = (0..gaps)
            .filter_map(|g| {
                let host = loaded.reveal_lines(entry.host_reveal.get(g), g, side, lines, context);
                let shown = on_lines
                    .then(|| loaded.reveal_lines(entry.reveal.get(g), g, side, lines, context))
                    .flatten();
                (host.is_some() || shown.is_some()).then_some((g, host, shown))
            })
            .collect();
        let mut any = false;
        for (gap, host, shown) in revealed {
            if let Some(host) = host {
                let entry = &mut self.files[file];
                entry.host_reveal.resize(gaps, Reveal::default());
                entry.host_reveal[gap] = host;
            }
            if let Some(shown) = shown {
                self.set_reveal(file, gap, gaps, shown, arrange);
                any = true;
            }
        }
        any
    }

    /// Reveals the lines of each annotation `placements` places on `file`
    /// that its gaps hide, with two lines around them, as
    /// [`Document::reveal_lines`] does, so the annotation shows under its
    /// own line. Returns whether any line was hidden in the view shown.
    pub fn reveal_annotations(
        &mut self,
        file: usize,
        placements: &[Placement],
        arrange: impl Arrange,
    ) -> bool {
        let mut any = false;
        for placement in placements {
            if let Placement::Placed { side, lines, .. } = placement {
                any |= self.reveal_lines_in(file, *side, lines, REVEAL_CONTEXT, &arrange);
            }
        }
        any
    }

    /// Switches every file between its changes and its full text, keeping
    /// the position on its row: a gap's line, a block's row. Returns false
    /// for an empty document; otherwise the host then normalizes the
    /// position.
    pub fn toggle_full(&mut self, arrange: impl Arrange) -> bool {
        let file = self.pos.file;
        if file >= self.files.len() {
            return false;
        }
        let anchor = self.anchor(self.pos, &arrange);
        self.full = !self.full;
        self.pos = self.unanchor(self.pos, anchor, &arrange);
        true
    }

    /// Where the position is, for a title (see [`Context`]); a block's row
    /// is where the row it hangs under is. `None` before the first hunk and
    /// for a whole added or deleted file.
    pub fn context(&mut self, arrange: impl Arrange) -> Option<Context<'_>> {
        let pos = self.pos;
        let entry = self.files.get_mut(pos.file)?;
        let State::Loaded(loaded) = &mut entry.state else {
            return None;
        };
        let shape = Shape {
            structural: self.structural,
            full: self.full || matches!(entry.source, Source::Unchanged),
            reveal: &entry.reveal,
            arrange: &arrange,
            blocks: entry.blocks.as_deref(),
        };
        let before = loaded.bytes();
        loaded.ensure_part(&shape, pos.part);
        self.loaded_bytes += loaded.bytes() - before;
        loaded.context(&shape, pos, &mut self.headings)
    }

    /// Puts `blocks` among `file`'s rows in place of those it had, keeping
    /// the position, the file's saved place and the change the view landed
    /// on there on the rows they showed. Blocks on lines show once the
    /// file's diff is loaded; until then, and on a sheet or a failed diff,
    /// only those under its first row and under its last do. Heights are
    /// measured anew.
    pub fn set_blocks(&mut self, file: usize, blocks: Vec<Block>, arrange: impl Arrange) {
        if file >= self.files.len() {
            return;
        }
        self.keep_place(file, &arrange, |doc| {
            let entry = &mut doc.files[file];
            match (&mut entry.blocks, blocks.is_empty()) {
                (slot, true) => *slot = None,
                (Some(held), false) => held.replace(blocks),
                (slot @ None, false) => *slot = Some(Box::new(Blocks::new(blocks))),
            }
        });
    }

    /// Measures block `id` of `file` again, or with `None` every block of
    /// it, as when it looks different (folded, edited, another theme)
    /// without its width changing. Positions stay on their rows, as with
    /// [`Document::set_blocks`].
    pub fn remeasure(&mut self, file: usize, id: Option<BlockId>, arrange: impl Arrange) {
        if self.files.get(file).is_none_or(|e| e.blocks.is_none()) {
            return;
        }
        self.keep_place(file, &arrange, |doc| {
            if let Some(blocks) = &mut doc.files[file].blocks {
                blocks.remeasure(id);
            }
        });
    }

    /// `file`'s blocks, as last set.
    #[must_use]
    pub fn blocks(&self, file: usize) -> &[Block] {
        (self.files.get(file))
            .and_then(|e| e.blocks.as_deref())
            .map_or(&[], |b| &b.list)
    }

    /// Moves on whenever `file`'s blocks are set to a list unlike the one it
    /// had, though not when they're measured again; 0 while it has none. Matches found in blocks of
    /// another version are stale.
    #[must_use]
    pub fn blocks_version(&self, file: usize) -> u64 {
        (self.files.get(file))
            .and_then(|e| e.blocks.as_deref())
            .map_or(0, |b| b.version)
    }

    /// `file`'s blocks, on the lines of the diff it shows, for
    /// [`super::find`] to search with that diff: a rendered view's blocks
    /// are on its own lines. Blocks on lines it doesn't show are left out.
    #[must_use]
    pub fn search_blocks(&self, file: usize) -> Vec<Block> {
        let blocks = self.blocks(file);
        let Some(loaded) = self.file(file).filter(|f| f.rendered().is_some()) else {
            return blocks.to_vec();
        };
        (blocks.iter())
            .filter_map(|b| match b.at {
                Attach::Line { side, line } => Some(Block {
                    at: Attach::Line {
                        side,
                        line: loaded.shown_line(side, line)?,
                    },
                    ..b.clone()
                }),
                Attach::Start | Attach::End => Some(b.clone()),
            })
            .collect()
    }

    /// The part of `file` `block` hangs in; `None` when it has no rows.
    fn block_part(&self, file: usize, block: &Block) -> Option<usize> {
        let entry = self.files.get(file)?;
        let Some(loaded) = entry.file() else {
            let edges = entry.is_sheet() || !matches!(entry.state, State::Loaded(_));
            return (edges && matches!(block.at, Attach::Start | Attach::End)).then_some(0);
        };
        match block.at {
            Attach::Start => Some(loaded.first_part()),
            Attach::End => Some(loaded.parts() - 1),
            Attach::Line { side, line } => {
                let line = loaded.shown_line(side, line)?;
                loaded.line_part(self.structural, side, line)
            }
        }
    }

    /// The first row of block `id` of `file`; `None` when it has no rows.
    pub fn block_pos(&mut self, file: usize, id: BlockId, arrange: impl Arrange) -> Option<Pos> {
        self.block_row(file, id, &arrange).map(|(pos, _)| pos)
    }

    /// The first row of block `id` of `file` and how many rows it takes;
    /// `None` when it has no rows.
    pub fn block_span(
        &mut self,
        file: usize,
        id: BlockId,
        arrange: impl Arrange,
    ) -> Option<(Pos, usize)> {
        self.block_row(file, id, &arrange)
    }

    /// Block `id`'s first row and its rows.
    fn block_row<A: Arrange>(
        &mut self,
        file: usize,
        id: BlockId,
        arrange: &A,
    ) -> Option<(Pos, usize)> {
        let block = self.files.get(file)?.blocks.as_deref()?.get(id)?;
        let part = self.block_part(file, block)?;
        let block = self.with_layout(file, part, arrange, |p| p.block(id))??;
        let pos = Pos {
            file,
            part,
            row: block.top,
            offset: 0,
        };
        Some((pos, block.rows))
    }

    /// The row of `file` showing `hit`: a line's match as
    /// [`Document::match_pos`] finds it, a block's on the row of the block
    /// the host locates its line on ([`Arrange::locate`]).
    pub fn hit_pos(&mut self, file: usize, hit: &Hit, arrange: impl Arrange) -> Option<Pos> {
        match hit {
            Hit::Line(m) => self.match_pos(file, m, arrange),
            Hit::Block(m) => {
                let (pos, rows) = self.block_row(file, m.block, &arrange)?;
                let line = arrange.locate(m.block, arrange.width(), m.line as usize);
                Some(Pos {
                    row: pos.row + line.min(rows - 1),
                    ..pos
                })
            }
        }
    }

    /// The blocks of `file` with their first rows, in the order they show.
    /// Only the parts holding blocks are laid out.
    fn placed_blocks<A: Arrange>(&mut self, file: usize, arrange: &A) -> Vec<(Pos, BlockId)> {
        let blocks = self.blocks(file);
        let mut parts: Vec<usize> = (blocks.iter())
            .filter_map(|b| self.block_part(file, b))
            .collect();
        parts.sort_unstable();
        parts.dedup();
        let mut out = Vec::new();
        for part in parts {
            self.with_layout(file, part, arrange, |placed| {
                out.extend(placed.inserts.iter().map(|i| {
                    let pos = Pos {
                        file,
                        part,
                        row: i.top,
                        offset: 0,
                    };
                    (pos, i.id)
                }));
            });
        }
        out
    }

    /// The first (or `last`) block of `file`, by its first row.
    pub fn edge_block(
        &mut self,
        file: usize,
        last: bool,
        arrange: impl Arrange,
    ) -> Option<(Pos, BlockId)> {
        let placed = self.placed_blocks(file, &arrange);
        if last { placed.last() } else { placed.first() }.copied()
    }

    /// Says which entries the host holds blocks for, by index, given with
    /// [`Document::set_blocks`] or not: a host that gives an entry its
    /// blocks only when it shows, as they're measured to show, still has
    /// [`Document::step_block`] step into the others.
    pub fn set_held_blocks(&mut self, files: &[usize]) {
        for entry in &mut self.files {
            entry.held_blocks = false;
        }
        for &f in files {
            if let Some(entry) = self.files.get_mut(f) {
                entry.held_blocks = true;
            }
        }
    }

    /// The block after `from` (or before it, without `forward`) in its
    /// file, by first row: a block whose rows `from` is in counts as before
    /// it. Past the file's last (or first), the nearest entry that way with
    /// blocks, given or held ([`Document::set_held_blocks`]). `None` when
    /// there's none that way.
    pub fn step_block(
        &mut self,
        from: Pos,
        forward: bool,
        arrange: impl Arrange,
    ) -> Option<BlockStep> {
        let placed = self.placed_blocks(from.file, &arrange);
        let here = if forward {
            placed.iter().find(|(pos, _)| *pos > from)
        } else {
            placed.iter().rev().find(|(pos, _)| *pos < from)
        };
        if let Some(&(pos, id)) = here {
            return Some(BlockStep::At { pos, id });
        }
        let has = |f: &usize| self.files[*f].blocks.is_some() || self.files[*f].held_blocks;
        let file = if forward {
            (from.file + 1..self.files.len()).find(has)
        } else {
            (0..from.file).rev().find(has)
        };
        file.map(BlockStep::File)
    }

    /// The overview's marks of `file` (see [`File::marks`]); none while it
    /// has no diff.
    pub fn marks(&mut self, file: usize, modified: bool) -> &[(usize, Mark)] {
        let structural = self.structural;
        match self.files.get_mut(file).map(|e| &mut e.state) {
            Some(State::Loaded(loaded)) => loaded.marks(structural, modified),
            _ => &[],
        }
    }

    /// The work missing for the rows from `first` to `last` of one file, as
    /// they show now: emphasis of the hunks among them, syntax runs of the
    /// gaps whose lines are among them (a gap's separator alone shows no
    /// lines), the file's definitions once its highlighting is ready, then
    /// its syntax diff and rendered view when those show. Builds no rows;
    /// nothing for rows of different files.
    pub fn missing(&self, first: Pos, last: Pos) -> impl Iterator<Item = Missing> + '_ {
        let entry = self
            .files
            .get(first.file)
            .filter(|_| first.file == last.file && first <= last);
        let view = self.view(first.file).filter(|_| entry.is_some());
        let loaded = view.as_ref().map(|(file, _)| *file);
        let parts = match loaded {
            Some(_) => first.part..last.part + 1,
            None => 0..0,
        };
        let emphasis = parts
            .clone()
            .filter_map(move |part| match loaded?.segment(part)? {
                Segment::Hunk(h) if loaded?.wants_emphasis(h) => Some(Missing::Emphasis(h)),
                _ => None,
            });
        let gaps = parts.filter_map(move |part| {
            let (file, shape) = view.as_ref()?;
            let Segment::Gap(g) = file.segment(part)? else {
                return None;
            };
            if !file.wants_gap_syntax(g) {
                return None;
            }
            let shown = file.shown(shape, g);
            // The gap's own rows, as the rows drawn placed its blocks.
            let own = |row: usize| file.at(shape, part, row).own();
            let from = if part == first.part {
                own(first.row)
            } else {
                0
            };
            let to = if part == last.part {
                own(last.row)
            } else {
                shown.rows().saturating_sub(1)
            };
            let separator = from == to && shown.line(from).is_none();
            (from <= to && !separator).then_some(Missing::GapSyntax(g))
        });
        let scopes = loaded
            .is_some_and(File::wants_scopes)
            .then_some(Missing::Scopes);
        let structural = (self.structural && loaded.is_some_and(File::wants_structural))
            .then_some(Missing::Structural);
        let render = (self.rendered && entry.is_some_and(|e| e.wants_render().is_some()))
            .then_some(Missing::Render);
        emphasis
            .chain(gaps)
            .chain(scopes)
            .chain(structural)
            .chain(render)
    }
}

//! The entries of a diff and the one shown: each a file (or a commit, or a
//! piped diff's text) with its [`File`] once the host prepared it, and the
//! state that outlives the file being dropped and prepared again: revealed
//! lines, where it was left, counts. [`Document`] moves between entries and
//! rows; the host fetches content, runs the work [`Document::missing`] names
//! and keeps the screen.

use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gix::bstr::BString;

use super::{
    Content, Context, Counts, File, Headings, Mark, Match, Matches, Pos, Reveal, Segment, Shape,
    Shown, Side, file_status,
};
use crate::diff::Emphasis;
use crate::git::{FileChange, FileDiff, Status, Update};
use crate::highlight::HunkSyntax;
use crate::patch::Commit;
use crate::structural::{Structural, Unavailable};
use crate::view::{Arrangement, FileStatus, HunkHeader, Spans};

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
    /// Where the file was left when another was shown.
    saved: Option<Pos>,
    /// Shown with a diff since it arrived: opened at its first change, or
    /// left where it was since. Diffs reloaded later never move the view.
    placed: bool,
    /// The frame that last showed it (see [`Document::touch`]).
    last_shown: u64,
    version: u64,
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
            Source::Commit(_) | Source::Page { .. } => Some(Counts::default()),
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
            last_shown: 0,
            version: next_version(),
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
            Source::Repo => false,
        }
    }

    /// The status to show: a piped file's is its patch header's.
    #[must_use]
    pub fn status(&self) -> FileStatus {
        match &self.source {
            Source::Piped(file) => file.status,
            Source::Repo | Source::Commit(_) | Source::Page { .. } => {
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
            (State::Loaded(file), None, Source::Repo) if !self.stale => Some(file.diff()),
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
        if matches!(self.state, State::Loaded(_)) {
            self.reveal.clear();
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

/// Work a host does for the rows it shows (see [`Document::missing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    /// Hunk `n` of the line diff has no emphasis:
    /// [`super::Prepare::emphasis`], then [`Document::set_emphasis`].
    Emphasis(usize),
    /// Gap `n` shows lines and has no syntax runs:
    /// [`super::Prepare::gap_syntax`], then [`Document::set_gap_syntax`].
    GapSyntax(usize),
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

    /// Adds an entry at the end, not diffed yet.
    pub fn push(&mut self, change: FileChange, source: Source<P>) {
        if matches!(source, Source::Commit(_)) {
            self.commits.push(self.files.len());
        }
        self.files.push(Entry::new(change, source));
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
        self.commits.partition_point(|&c| c <= file).checked_sub(1)
    }

    /// The files of the commit at entry `commit`.
    #[must_use]
    pub fn commit_files(&self, commit: usize) -> Range<usize> {
        let next = self.commits.partition_point(|&c| c <= commit);
        commit + 1..self.commits.get(next).copied().unwrap_or(self.files.len())
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

    /// Puts the files in path order, keeping the position on the same file.
    /// Returns whether any file moved; positions held in other files are
    /// dropped then ([`Document::forget_positions`]).
    pub fn sort_by_path(&mut self) -> bool {
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

    /// Drops the change landed on and [`Document::pending_land`], which
    /// hold positions in files that moved or changed.
    pub fn forget_positions(&mut self) {
        self.pending_land = None;
        self.landed = None;
    }

    /// Takes the refreshed file list, in path order. Files still listed
    /// keep their diffs, counts, viewed marks and revealed lines; diffs and
    /// counts are marked for redoing when they may have changed. The
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
        let mut old: HashMap<BString, Entry<P>> = std::mem::take(&mut self.files)
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
        }
        entry.stale = false;
        entry.recount = false;
        entry.state = match result {
            Ok(loaded) => {
                self.loaded_bytes += loaded.bytes();
                entry.counts = Some(loaded.counts());
                entry.reveal.truncate(loaded.gaps().len());
                // A diff of the same file may cut its gaps differently.
                for (reveal, gap) in entry.reveal.iter_mut().zip(loaded.gaps()) {
                    reveal.settle(gap.len);
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

    /// Whether `entry` shows its full text. A piped file never does.
    #[inline]
    #[must_use]
    pub fn is_full(&self, entry: &Entry<P>) -> bool {
        !matches!(entry.source, Source::Piped(_)) && self.full
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
            full: self.full,
            reveal: &entry.reveal,
            arrange: (),
        };
        Some((loaded, shape))
    }

    /// Runs `f` on `file`'s diff and how it's shown with `arrange`; `None`
    /// while it has no diff. What `f` adds to the diff's memory (rows for a
    /// second arrangement) counts toward [`Document::loaded_bytes`].
    #[inline]
    fn with_file<A: Fn(&HunkHeader<'_>) -> Arrangement, R>(
        &mut self,
        file: usize,
        arrange: A,
        f: impl FnOnce(&mut File, &Shape<'_, A>) -> R,
    ) -> Option<R> {
        let entry = self.files.get_mut(file)?;
        let State::Loaded(loaded) = &mut entry.state else {
            return None;
        };
        let shape = Shape {
            structural: self.structural,
            full: self.full,
            reveal: &entry.reveal,
            arrange,
        };
        let before = loaded.bytes();
        let result = f(loaded, &shape);
        self.loaded_bytes += loaded.bytes() - before;
        Some(result)
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
            Source::Repo => None,
        }
    }

    /// Rows in part `part` of `file`, at least 1.
    #[inline]
    pub fn rows(
        &mut self,
        file: usize,
        part: usize,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> usize {
        if part == 0 {
            return self.sheet_rows(file).unwrap_or(1);
        }
        self.with_file(file, arrange, |f, shape| f.part_rows(shape, part))
            .unwrap_or(1)
    }

    /// What the row at `pos` shows; `None` for part 0 (a sheet's rows and
    /// the blank row are the host's to draw) and while the file has no
    /// diff. Its offset is ignored.
    #[inline]
    pub fn row(
        &mut self,
        pos: Pos,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Content<'_>> {
        let (structural, full) = (self.structural, self.full);
        let entry = self.files.get_mut(pos.file)?;
        let State::Loaded(file) = &mut entry.state else {
            return None;
        };
        let shape = Shape {
            structural,
            full,
            reveal: &entry.reveal,
            arrange,
        };
        let before = file.bytes();
        file.ensure_part(&shape, pos.part);
        self.loaded_bytes += file.bytes() - before;
        file.row(&shape, pos)
    }

    /// Moves `pos` to the next row of its file, offset 0; false at the end.
    #[inline]
    pub fn next_row(
        &mut self,
        pos: &mut Pos,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> bool {
        let at = *pos;
        let next = self
            .with_file(at.file, arrange, |f, shape| {
                let mut at = at;
                f.next_row(shape, &mut at).then_some(at)
            })
            .unwrap_or_else(|| {
                // A sheet's rows, or the blank row of a file without a diff.
                let rows = if at.part == 0 {
                    self.sheet_rows(at.file).unwrap_or(1)
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
    pub fn prev_row(
        &mut self,
        pos: &mut Pos,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> bool {
        let at = *pos;
        let prev = match self.with_file(at.file, arrange, |f, shape| {
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
    pub fn normalize(&mut self, arrange: impl Fn(&HunkHeader<'_>) -> Arrangement) -> bool {
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
    pub fn open(
        &mut self,
        file: usize,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Pos> {
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
    pub fn next_change(
        &mut self,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Settle> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        if let Some(Some(at)) =
            self.with_file(start.file, arrange, |f, shape| f.next_change(shape, start))
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
    pub fn prev_change(
        &mut self,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Settle> {
        let start = self.change_cursor();
        if start.file >= self.files.len() {
            return None;
        }
        // Within the row the view is in, below its first screen row, the
        // row's own change counts as before the view.
        if let Some(Some(at)) =
            self.with_file(start.file, arrange, |f, shape| f.prev_change(shape, start))
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
    pub fn edge(
        &mut self,
        file: usize,
        last: bool,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Pos> {
        self.with_file(file, arrange, |f, shape| f.edge(shape, file, last))?
    }

    /// The first block of changes of `file` whose new-side line number
    /// (1-based) is past `line`, or with none the last block.
    pub fn change_after(
        &mut self,
        file: usize,
        line: usize,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Pos> {
        self.with_file(file, arrange, |f, shape| f.change_after(shape, file, line))?
    }

    /// The hunk at the position and how many the file has: the one its row
    /// is in, or the last above it.
    #[must_use]
    pub fn hunk_position(&self) -> Option<(usize, usize)> {
        self.file(self.pos.file)?.hunk_position(self.pos)
    }

    /// The new-side line number (1-based) the row at `pos` shows, or the
    /// first after it; 1 while its file has no diff.
    pub fn line_of(&mut self, pos: Pos, arrange: impl Fn(&HunkHeader<'_>) -> Arrangement) -> usize {
        self.with_file(pos.file, arrange, |f, shape| f.line_of(shape, pos))
            .unwrap_or(1)
    }

    /// The old and new lines (0-based) the row at `pos` shows; `None` on a
    /// side without one, and on rows that aren't code.
    #[inline]
    pub fn row_lines(
        &mut self,
        pos: Pos,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> [Option<usize>; 2] {
        self.with_file(pos.file, arrange, |f, shape| f.row_lines(shape, pos))
            .unwrap_or([None, None])
    }

    /// The arrangement of the rows of the hunk at `pos`; `None` for a part
    /// that isn't a hunk.
    #[inline]
    pub fn arrangement(
        &mut self,
        pos: Pos,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Arrangement> {
        self.with_file(pos.file, arrange, |f, shape| f.arrangement(shape, pos.part))?
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
    pub fn match_pos(
        &mut self,
        file: usize,
        m: &Match,
        arrange: impl Fn(&HunkHeader<'_>) -> Arrangement,
    ) -> Option<Pos> {
        self.with_file(file, arrange, |f, shape| f.match_pos(shape, file, m))?
    }

    /// The line of gap `gap` of `file` that `pos` is on, when it's in that
    /// gap (a separator counts as the first line it hides).
    fn gap_anchor(&self, file: usize, gap: usize, pos: Pos) -> Option<usize> {
        if pos.file != file {
            return None;
        }
        let (loaded, shape) = self.view(file)?;
        let (at, line) = loaded.gap_anchor(&shape, pos)?;
        (at == gap).then_some(line)
    }

    /// Puts `pos` back on gap line `line` after the gap changed.
    fn reanchor(&self, gap: usize, line: Option<usize>, mut pos: Pos) -> Pos {
        if let Some(line) = line
            && let Some((loaded, shape)) = self.view(pos.file)
        {
            pos.row = loaded.shown(&shape, gap).row(line);
            pos.offset = 0;
        }
        pos
    }

    /// Stores `reveal` for gap `gap` of `file`, keeping the position and
    /// the file's saved place on their lines of the gap.
    fn set_reveal(&mut self, file: usize, gap: usize, gaps: usize, reveal: Reveal) {
        let saved = self.files[file].saved;
        let anchors = (
            self.gap_anchor(file, gap, self.pos),
            saved.and_then(|p| self.gap_anchor(file, gap, p)),
        );
        let entry = &mut self.files[file];
        entry.reveal.resize(gaps, Reveal::default());
        entry.reveal[gap] = reveal;
        self.pos = self.reanchor(gap, anchors.0, self.pos);
        if let Some(saved) = saved {
            self.files[file].saved = Some(self.reanchor(gap, anchors.1, saved));
        }
    }

    /// Reveals more of gap `gap` of `file`, keeping a position in the gap
    /// on its line: 20 lines, next to the hunk below for the gap before the
    /// first hunk, next to the hunk above for the gap after the last, half
    /// of each otherwise; up to 40 hidden lines open whole. A piped file's
    /// gaps never open. Returns whether any line was hidden; the host then
    /// normalizes the position.
    pub fn expand(&mut self, file: usize, gap: usize) -> bool {
        let Some((gaps, reveal)) = self.view(file).and_then(|(loaded, shape)| {
            Some((loaded.gaps().len(), loaded.reveal_more(&shape, gap)?))
        }) else {
            return false;
        };
        self.set_reveal(file, gap, gaps, reveal);
        true
    }

    /// Reveals the lines of `side` in `lines` (0-based) of `file` that its
    /// gaps hide, with `context` lines around them, leaving the rest of
    /// each gap hidden. Positions in the gaps stay on their lines, as with
    /// [`Document::expand`]. Hunks, piped files and rendered views are left
    /// as they are. Returns whether any line was hidden; the host then
    /// normalizes the position.
    pub fn reveal_lines(
        &mut self,
        file: usize,
        side: Side,
        lines: Range<usize>,
        context: usize,
    ) -> bool {
        let Some(entry) = self
            .files
            .get(file)
            .filter(|e| !matches!(e.render.as_deref(), Some(Render::Shown(_))))
        else {
            return false;
        };
        let State::Loaded(loaded) = &entry.state else {
            return false;
        };
        let gaps = loaded.gaps().len();
        let revealed: Vec<_> = (0..gaps)
            .filter_map(|g| {
                loaded
                    .reveal_lines(entry.reveal.get(g), g, side, &lines, context)
                    .map(|reveal| (g, reveal))
            })
            .collect();
        let any = !revealed.is_empty();
        for (gap, reveal) in revealed {
            self.set_reveal(file, gap, gaps, reveal);
        }
        any
    }

    /// Switches every file between its changes and its full text, keeping a
    /// position in a gap on its line. Returns false for an empty document;
    /// otherwise the host then normalizes the position.
    pub fn toggle_full(&mut self) -> bool {
        let file = self.pos.file;
        if file >= self.files.len() {
            return false;
        }
        let gap = match self.segment(file, self.pos.part) {
            Some(Segment::Gap(g)) => Some((g, self.gap_anchor(file, g, self.pos))),
            _ => None,
        };
        self.full = !self.full;
        if let Some((g, line)) = gap {
            self.pos = self.reanchor(g, line, self.pos);
        }
        true
    }

    /// Where the position is, for a title (see [`Context`]). `None` before
    /// the first hunk and for a whole added or deleted file.
    pub fn context(&mut self) -> Option<Context<'_>> {
        let pos = self.pos;
        let entry = self.files.get(pos.file)?;
        let State::Loaded(loaded) = &entry.state else {
            return None;
        };
        let shape = Shape {
            structural: self.structural,
            full: self.full,
            reveal: &entry.reveal,
            arrange: (),
        };
        loaded.context(&shape, pos, &mut self.headings)
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
    /// lines), then the file's syntax diff and rendered view when those
    /// show. Builds no rows; nothing for rows of different files.
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
            let from = if part == first.part { first.row } else { 0 };
            let to = if part == last.part {
                last.row
            } else {
                shown.rows().saturating_sub(1)
            };
            let separator = from == to && shown.line(from).is_none();
            (from <= to && !separator).then_some(Missing::GapSyntax(g))
        });
        let structural = (self.structural && loaded.is_some_and(File::wants_structural))
            .then_some(Missing::Structural);
        let render = (self.rendered && entry.is_some_and(|e| e.wants_render().is_some()))
            .then_some(Missing::Render);
        emphasis.chain(gaps).chain(structural).chain(render)
    }
}

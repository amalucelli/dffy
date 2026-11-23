//! Work off the UI thread: the change set, which streams the changed files
//! as the core finds them, pairs renames once it has them all, and then
//! keeps them current; and a small pool that diffs files and places their
//! annotations on them, computes emphasis and syntax spans for the hunks on
//! screen and then the shown file's definitions for the scope line, and
//! syntax-diffs the files on screen when that view is on. It also makes go
//! to definition's requests to the navigator helper, the process that holds
//! the index and builds it. When it has nothing else to do,
//! the pool compiles the highlight queries of the languages in the list,
//! so a language's first file doesn't wait for its query, then counts
//! every file's lines for the file tree. Results come back as [`Event`]s.

use std::collections::VecDeque;
use std::io;
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use dffy_core::annotation::{Annotated, AnnotationSet};
use dffy_core::bstr::BString;
use dffy_core::cache::Cache;
use dffy_core::conflict::Choices;
use dffy_core::diff::{Emphasis, Options, Text};
use dffy_core::document::{self, BlockId, Counts, File, Matches, Needle, Prepare, SyntaxSlot};
use dffy_core::git::{
    ChangeSet, FileChange, FileDiff, Repo, Revisions, Side, Snapshot, StatStore, Status,
    WhichCheckpoint,
};
use dffy_core::highlight::{self, HunkSyntax, Language, Registry};
use dffy_core::navigate;
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::Spans;
use dffy_core::watch::MAX_BATCH;

use super::Event;
use super::picker::Paths;
use super::piped::PipedFile;
use super::view;
use crate::navigator::client::{self, Cancel, Helper, Helpers, Opened, Pushed, Refused};
use crate::plugin::{self, Program};
use crate::print;
use crate::review::model::{self, Thread};

/// What to bring the file list up to date with.
pub(crate) enum Request {
    /// These repository-relative paths changed in the working tree.
    Refresh(Vec<BString>),
    /// Anything may have changed, git state included.
    Rescan,
    /// List these revisions instead, from now on.
    Retarget(Revisions),
}

/// What the change set lists, to count what it leaves out when it lists
/// nothing (see [`crate::print::others`]).
pub(crate) struct Selected {
    pub repo: Repo,
    pub revisions: Revisions,
    pub paths: Vec<BString>,
}

/// Streams the changes as [`Event::Enumerated`], then
/// [`Event::EnumerationDone`], then the list with renames paired as
/// [`Event::Refreshed`] if there are any, then serves [`Request`]s one at a
/// time with [`Event::Refreshed`]. Serving them in order on one thread means each
/// result is at least as new as the one before. Requests that queue up
/// while one runs are merged. Whenever the list is empty, [`Event::Others`]
/// follows with what `selected` leaves out. Stops once nobody listens or
/// asks.
pub(crate) fn changes(
    mut set: ChangeSet,
    mut selected: Selected,
    tx: Sender<Event>,
) -> io::Result<Sender<Request>> {
    let (requests, rx) = mpsc::channel();
    thread::Builder::new()
        .name("dffy-changes".into())
        .spawn(move || {
            let others = |selected: &Selected, empty: bool| {
                let others = empty
                    .then(|| print::others(&selected.repo, &selected.revisions, &selected.paths))
                    .flatten();
                tx.send(Event::Others(others)).is_ok()
            };
            let mut empty = true;
            let result = set.enumerate(|change| {
                empty = false;
                match tx.send(Event::Enumerated(change)) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(_) => ControlFlow::Break(()),
                }
            });
            if tx
                .send(Event::EnumerationDone(result.err().map(|e| e.to_string())))
                .is_err()
                || !others(&selected, empty)
            {
                return;
            }
            let renamed = match set.renamed() {
                Ok(Some(update)) => Some(Event::Refreshed(update)),
                Ok(None) => None,
                Err(e) => Some(Event::RefreshFailed(format!("finding renames: {e}"))),
            };
            if renamed.is_some_and(|event| tx.send(event).is_err()) {
                return;
            }
            // Read again on each rescan, which git's state changing asks for,
            // so a checkout moves the comments shown to its branch.
            let mut branch = crate::review::current_branch(&selected.repo);
            let mut head = crate::review::head_commit(&selected.repo);
            let latest = |repo: &Repo| repo.checkpoint(WhichCheckpoint::Latest).ok();
            let mut checkpoint = latest(&selected.repo);
            if checkpoint.is_some() && tx.send(Event::Checkpoint(checkpoint)).is_err() {
                return;
            }
            while let Ok(first) = rx.recv() {
                let mut paths = Vec::new();
                let mut rescan = false;
                for request in std::iter::once(first).chain(rx.try_iter()) {
                    match request {
                        Request::Refresh(p) => paths.extend(p),
                        Request::Rescan => rescan = true,
                        Request::Retarget(revisions) => {
                            set.set_revisions(revisions.clone());
                            selected.revisions = revisions;
                            rescan = true;
                        }
                    }
                }
                paths.sort_unstable();
                paths.dedup();
                if rescan {
                    let now = crate::review::current_branch(&selected.repo);
                    if now != branch {
                        branch.clone_from(&now);
                        if tx.send(Event::Branch(now)).is_err() {
                            return;
                        }
                    }
                    let now = crate::review::head_commit(&selected.repo);
                    if now != head {
                        head.clone_from(&now);
                        if tx.send(Event::Head(now)).is_err() {
                            return;
                        }
                    }
                    let now = latest(&selected.repo);
                    if now != checkpoint {
                        checkpoint = now;
                        if tx.send(Event::Checkpoint(now)).is_err() {
                            return;
                        }
                    }
                }
                let result = if rescan || paths.len() > MAX_BATCH {
                    set.rescan()
                } else {
                    set.refresh(&paths)
                };
                let event = match result {
                    Ok(Some(update)) => {
                        empty = update.files.is_empty();
                        Some(Event::Refreshed(update))
                    }
                    // The list stayed empty, but what it leaves out may not have.
                    Ok(None) => None,
                    Err(e) => Some(Event::RefreshFailed(e.to_string())),
                };
                if event.is_some_and(|event| tx.send(event).is_err())
                    || (empty && !others(&selected, true))
                {
                    return;
                }
            }
        })?;
    Ok(requests)
}

/// Work for the pool. Each job carries the version its file's entry had
/// when it was asked for (see [`view::Entry::version`]); its result is
/// taken only while the entry still has it, so a refresh voids the work
/// of the files it touched and no other.
pub(crate) enum Job {
    /// The file's diff, with its annotations placed on it as the diff
    /// comes (see [`Pool::annotate`]), so they show with its first frame.
    Diff {
        file: usize,
        version: u64,
        change: FileChange,
        /// A piped file's patch to read back, instead of the repository.
        piped: Option<Arc<PipedFile>>,
        /// The choices made on a conflicted file, to preview.
        choices: Option<Arc<Choices>>,
        /// A file the diff doesn't list, shown whole (see
        /// [`Source::Unchanged`](dffy_core::document::Source::Unchanged)).
        unchanged: bool,
        /// The code of the annotations lost is parsed with the placement
        /// (see [`model::land`]); else a [`Job::Place`] does it after.
        paint: bool,
    },
    /// The file's annotations placed on its line diff `diff`, after they
    /// changed since it was loaded.
    Place(PlaceJob),
    /// Emphasis, and syntax spans when the file has a slot for them.
    Emphasis {
        file: usize,
        version: u64,
        hunk: usize,
        diff: Arc<FileDiff>,
        syntax: Option<SyntaxSlot>,
    },
    Structural {
        file: usize,
        version: u64,
        diff: Arc<FileDiff>,
    },
    /// How many times the diff search's `needle` (search `id`) occurs in
    /// the hunks of a file not on screen and in the text of its `threads`;
    /// without a `change`, a sheet's, in its threads alone. With `reading`,
    /// how the file is read alone, in every line of that side instead of
    /// its hunks. Stops once `live` holds another search's id.
    Search {
        file: usize,
        version: u64,
        change: Option<FileChange>,
        piped: Option<Arc<PipedFile>>,
        reading: Option<document::Reading>,
        threads: Arc<[Thread]>,
        needle: Arc<Needle>,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// The diff search's matches in the shown file's visible lines (its
    /// hunks and the unchanged lines not `hidden`) and in the text of its
    /// `blocks`, the comments' (version `blocks_version`), as `reading` says
    /// when the file is read on one side alone; without a `diff`, a
    /// sheet's, in its blocks alone.
    Find {
        file: usize,
        version: u64,
        diff: Option<Arc<FileDiff>>,
        hidden: Vec<Range<usize>>,
        reading: Option<document::Reading>,
        blocks: Vec<document::Block>,
        blocks_version: u64,
        needle: Arc<Needle>,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// Syntax runs for the `lines` of `side` of a gap someone revealed.
    GapSyntax {
        file: usize,
        version: u64,
        gap: usize,
        side: document::Side,
        lines: Range<usize>,
        diff: Arc<FileDiff>,
        syntax: SyntaxSlot,
    },
    /// The definitions the file's highlighting, ready in `syntax`, left to
    /// find; they land in the slot itself.
    Scopes {
        file: usize,
        version: u64,
        diff: Arc<FileDiff>,
        syntax: SyntaxSlot,
    },
    Render(RenderJob),
    /// Every file at `snapshot`, for the picker. Stops once `live` holds
    /// another id.
    ListPaths {
        snapshot: Snapshot,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// The picker's `query` scored against `candidates`, positions in
    /// `paths`, for query `id`. Stops once `live` holds another id.
    Score {
        paths: Arc<Paths>,
        candidates: Arc<[usize]>,
        query: String,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// A go-to-definition navigator for one side of the diff, held by the
    /// navigator helper (docs/navigate.md), which the first such job
    /// starts: it lists everything that differs between the diff's
    /// snapshots, whatever paths the view lists, then that side's files.
    /// The helper builds its index and pushes how far it is.
    Navigator(NavigatorJob),
    /// Where a name is defined, or with `text` the lines using it.
    Lookup(LookupJob),
    /// A file's lines, highlighted, for the peek.
    PeekCode(PeekCodeJob),
    /// The next files of a search for a name's uses, for about 50ms, so
    /// what it finds streams in and other work comes between.
    References(ReferencesJob),
}

/// The viewer's navigator helper across its restarts.
pub(crate) type HelperSlot = Arc<Mutex<Helpers>>;

/// Why a go-to-definition job has no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unanswered {
    Cancelled,
    /// The navigator asked of, or the helper holding it, is gone: it's
    /// made again.
    Gone,
    /// The helper can't start, as said.
    Unavailable(String),
    Failed(String),
}

impl From<Refused> for Unanswered {
    fn from(r: Refused) -> Self {
        match r.code {
            client::CANCELLED => Self::Cancelled,
            client::GONE => Self::Gone,
            _ if r.helper_gone() => Self::Gone,
            _ => Self::Failed(r.message),
        }
    }
}

pub(crate) struct ReferencesJob {
    pub helper: Helper,
    pub navigator: u64,
    /// The helper's search, started by the first of its jobs.
    pub search: Arc<Mutex<Option<u64>>>,
    pub name: String,
    pub origin: navigate::Origin,
    pub id: u64,
    pub cancel: Arc<Cancel>,
}

/// What a step of a search for a name's uses found.
pub(crate) struct Referenced {
    pub hits: Vec<navigate::Reference>,
    /// Files with a use found so far.
    pub files: usize,
    pub searched: navigate::Progress,
    pub done: bool,
    /// The navigator's index was complete when the step started: its
    /// files' definitions were told apart from uses.
    pub indexed: bool,
}

/// How long a step of a search for a name's uses runs before it hands
/// what it found back.
const REFERENCES_STEP: std::time::Duration = std::time::Duration::from_millis(50);

/// How long a step of the text search runs before the lookup checks
/// whether it was cancelled meanwhile; the helper stops it sooner.
const TEXT_STEP: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) struct NavigatorJob {
    pub side: document::Side,
    pub snapshots: [Snapshot; 2],
    pub helper: HelperSlot,
    pub id: u64,
}

pub(crate) struct LookupJob {
    pub helper: Helper,
    pub navigator: u64,
    pub name: String,
    pub origin: navigate::Origin,
    /// The whole-word text search, for a name with no definition found.
    pub text: bool,
    pub id: u64,
    pub cancel: Arc<Cancel>,
}

#[derive(Clone)]
pub(crate) struct PeekCodeJob {
    pub path: BString,
    pub side: Side,
    pub language: Option<Language>,
    pub id: u64,
}

/// A lookup's answer, as the peek shows it.
pub(crate) struct Looked {
    pub candidates: Vec<navigate::Candidate>,
    /// Found in all, beyond those listed.
    pub total: usize,
    pub progress: navigate::Progress,
    /// Text matches: lines using the name, not definitions.
    pub text: bool,
    /// The text search went through every file.
    pub complete: bool,
    /// The first line of each of the first [`LISTED_LINES`] candidates,
    /// trimmed; `None` where the file couldn't be read.
    pub lines: Vec<Option<String>>,
}

/// A file's lines for the peek, read and highlighted once for every
/// definition shown in it.
pub(crate) struct Code {
    pub lines: Vec<String>,
    /// Syntax runs, by line; `None` unhighlighted.
    pub spans: Option<Spans>,
}

/// Candidates whose line the lookup reads for the list.
const LISTED_LINES: usize = 50;

/// Lines the text search stops at.
pub(crate) const TEXT_MATCHES: usize = 200;

/// The rendered view of a file's line diff `diff`, by `program`, for a
/// pane `width` columns wide. Stops once `live` holds another id.
pub(crate) struct RenderJob {
    pub file: usize,
    pub version: u64,
    pub diff: Arc<FileDiff>,
    pub program: Program,
    pub width: usize,
    pub light: bool,
    pub id: u64,
    pub live: Arc<AtomicU64>,
}

pub(crate) struct PlaceJob {
    pub file: usize,
    pub version: u64,
    pub annotated: Annotated,
    pub diff: Arc<FileDiff>,
    /// The code of those lost is parsed (see [`model::land`]).
    pub paint: bool,
}

impl PlaceJob {
    pub(crate) fn run(self) -> Done {
        Done::Placed {
            file: self.file,
            version: self.version,
            placed: model::land(&self.annotated, &self.diff, self.paint),
            diff: self.diff,
        }
    }
}

/// What a job is for, to tell whether it's already running: the file's
/// position and version, and what of it. A job for an older version of a
/// file never holds back the one for its current version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Key {
    file: usize,
    version: u64,
    of: Of,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Of {
    Diff,
    /// With the generation of the annotations placed.
    Place(u64),
    Emphasis(usize),
    Structural,
    GapSyntax(usize, document::Side),
    Scopes,
    /// With the search's id: a running scan for an older search doesn't
    /// hold back one for the new.
    Search(u64),
    Find(u64),
    Render(u64),
    /// Not a file's: its key's file and version are [`NO_FILE`].
    ListPaths(u64),
    Score(u64),
    Navigator(u64),
    Lookup(u64),
    PeekCode(u64),
    References(u64),
}

/// The file and version of a key for work no file has.
const NO_FILE: usize = usize::MAX;

impl Job {
    fn key(&self) -> Key {
        let (file, version, of) = match self {
            Self::Diff { file, version, .. } => (file, version, Of::Diff),
            Self::Place(job) => (&job.file, &job.version, Of::Place(job.annotated.generation)),
            Self::Emphasis {
                file,
                version,
                hunk,
                ..
            } => (file, version, Of::Emphasis(*hunk)),
            Self::Structural { file, version, .. } => (file, version, Of::Structural),
            Self::GapSyntax {
                file,
                version,
                gap,
                side,
                ..
            } => (file, version, Of::GapSyntax(*gap, *side)),
            Self::Scopes { file, version, .. } => (file, version, Of::Scopes),
            Self::Search {
                file, version, id, ..
            } => (file, version, Of::Search(*id)),
            Self::Find {
                file, version, id, ..
            } => (file, version, Of::Find(*id)),
            Self::Render(job) => (&job.file, &job.version, Of::Render(job.id)),
            Self::ListPaths { id, .. } => (&NO_FILE, &0, Of::ListPaths(*id)),
            Self::Score { id, .. } => (&NO_FILE, &0, Of::Score(*id)),
            Self::Navigator(job) => (&NO_FILE, &0, Of::Navigator(job.id)),
            Self::Lookup(job) => (&NO_FILE, &0, Of::Lookup(job.id)),
            Self::PeekCode(job) => (&NO_FILE, &0, Of::PeekCode(job.id)),
            Self::References(job) => (&NO_FILE, &0, Of::References(job.id)),
        };
        Key {
            file: *file,
            version: *version,
            of,
        }
    }
}

/// A finished job, with the position and version of the file it was asked
/// for (see [`Job`]).
pub(crate) enum Done {
    Diff {
        file: usize,
        version: u64,
        result: Result<Box<File>, String>,
        /// Where the job's annotations landed on the diff.
        placed: Option<model::Landed>,
    },
    /// Where the file's annotations landed on its line diff `diff`, as for
    /// [`Done::Diff`]: the file may have been diffed again since, within
    /// the version.
    Placed {
        file: usize,
        version: u64,
        diff: Arc<FileDiff>,
        placed: model::Landed,
    },
    Emphasis {
        file: usize,
        version: u64,
        /// The diff `hunk` is of: the file may have been diffed again since,
        /// within the version.
        diff: Arc<FileDiff>,
        hunk: usize,
        emphasis: Vec<Emphasis>,
        syntax: Option<HunkSyntax>,
    },
    Structural {
        file: usize,
        version: u64,
        /// The diff it was computed from: the file may have been diffed
        /// again since.
        diff: Arc<FileDiff>,
        result: Option<Result<Structural, Unavailable>>,
    },
    GapSyntax {
        file: usize,
        version: u64,
        /// The diff `gap` is of, as for [`Done::Emphasis`].
        diff: Arc<FileDiff>,
        gap: usize,
        side: document::Side,
        spans: Option<Spans>,
    },
    /// The file's definitions are found, in its syntax slot.
    Scopes { file: usize, version: u64 },
    /// A file's matches of search `id`; `None` when it can't be read.
    Search {
        file: usize,
        version: u64,
        id: u64,
        count: Option<usize>,
    },
    /// The matches of search `id` in the file shown, as `diff` with
    /// `hidden` lines, read on `reading`, and its blocks of
    /// `blocks_version` have them; `None` when the scan was stopped.
    Found {
        file: usize,
        version: u64,
        id: u64,
        diff: Option<Arc<FileDiff>>,
        hidden: Vec<Range<usize>>,
        reading: Option<document::Reading>,
        blocks_version: u64,
        matches: Option<Matches>,
    },
    /// The rendered view of `diff` for render `id`: why there's none as
    /// `Err(Some(..))`, or `Err(None)` when it was cancelled.
    Rendered {
        file: usize,
        version: u64,
        id: u64,
        diff: Arc<FileDiff>,
        result: Result<Box<File>, Option<String>>,
    },
    /// Line counts from the background pass, a chunk at a time, each with
    /// its file's position and version. A file that can't be read counts
    /// as nothing changed.
    Counts { counts: Vec<(usize, u64, Counts)> },
    /// The files at `snapshot` for listing `id`, or why they can't be
    /// listed: `Err(None)` when it was stopped.
    Listed {
        snapshot: Snapshot,
        id: u64,
        result: Result<Arc<Paths>, Option<String>>,
    },
    /// The matches of query `id` among the candidates it was asked for,
    /// best first; `None` when it was stopped.
    Scored {
        id: u64,
        matches: Option<Vec<(i64, usize)>>,
    },
    /// The navigator of `side` asked for as `id`, with the generation of
    /// the helper holding it, or why there's none.
    Navigator {
        side: document::Side,
        id: u64,
        result: Result<(u64, Opened), Unanswered>,
    },
    /// Lookup `id`'s answer.
    Looked {
        id: u64,
        result: Result<Looked, Unanswered>,
    },
    /// The peek's code for request `id`, or why there's none.
    PeekCode {
        id: u64,
        result: Result<Code, String>,
    },
    /// A step of reference search `id`.
    Referenced {
        id: u64,
        result: Result<Referenced, Unanswered>,
    },
}

impl Done {
    /// The job this answers, as [`Job::key`] names it; a chunk of the count
    /// pass answers none. A key that differs from the job's leaves the job
    /// counted as running, never to be asked again.
    fn job(&self) -> Option<Key> {
        let (file, version, of) = match *self {
            Self::Diff { file, version, .. } => (file, version, Of::Diff),
            Self::Placed {
                file,
                version,
                placed: model::Landed { generation, .. },
                ..
            } => (file, version, Of::Place(generation)),
            Self::Emphasis {
                file,
                version,
                hunk,
                ..
            } => (file, version, Of::Emphasis(hunk)),
            Self::Structural { file, version, .. } => (file, version, Of::Structural),
            Self::GapSyntax {
                file,
                version,
                gap,
                side,
                ..
            } => (file, version, Of::GapSyntax(gap, side)),
            Self::Scopes { file, version } => (file, version, Of::Scopes),
            Self::Search {
                file, version, id, ..
            } => (file, version, Of::Search(id)),
            Self::Found {
                file, version, id, ..
            } => (file, version, Of::Find(id)),
            Self::Rendered {
                file, version, id, ..
            } => (file, version, Of::Render(id)),
            Self::Counts { .. } => return None,
            Self::Listed { id, .. } => (NO_FILE, 0, Of::ListPaths(id)),
            Self::Scored { id, .. } => (NO_FILE, 0, Of::Score(id)),
            Self::Navigator { id, .. } => (NO_FILE, 0, Of::Navigator(id)),
            Self::Looked { id, .. } => (NO_FILE, 0, Of::Lookup(id)),
            Self::PeekCode { id, .. } => (NO_FILE, 0, Of::PeekCode(id)),
            Self::Referenced { id, .. } => (NO_FILE, 0, Of::References(id)),
        };
        Some(Key { file, version, of })
    }

    /// The position and version of the file this is for; `None` for a
    /// chunk of the count pass, whose files carry their own, and for work
    /// no file has.
    pub(crate) fn file(&self) -> Option<(usize, u64)> {
        self.job()
            .filter(|k| k.file != NO_FILE)
            .map(|k| (k.file, k.version))
    }
}

/// Files counted per [`Done::Counts`]: enough to keep events few, few
/// enough that counts appear steadily.
const COUNT_CHUNK: usize = 32;

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
    /// How many jobs wait, read between the files of a chunk of the count
    /// pass without taking the lock. Changed only under it.
    waiting: AtomicUsize,
    /// How many jobs are being run, likewise.
    running: AtomicUsize,
    /// Where the count pass finds files counted before and keeps the rest;
    /// `None` without a disk cache.
    stats: Option<StatStore>,
}

impl Shared {
    fn new(queue: Queue) -> Self {
        let stats = queue.settings.cache.clone().map(StatStore::new);
        Self {
            queue: Mutex::new(queue),
            ready: Condvar::new(),
            waiting: AtomicUsize::new(0),
            running: AtomicUsize::new(0),
            stats,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct Queue {
    waiting: VecDeque<Job>,
    /// Jobs taken by a worker whose result the viewer hasn't taken yet
    /// ([`Pool::finish`]). Until then the result may still make the job
    /// unwanted, so asking for it again would run it twice; from then on
    /// the viewer's next ask is the one that counts.
    running: Vec<Key>,
    /// Languages whose highlight queries to compile ahead of their first
    /// file being highlighted, taken only when nothing is waiting, one at a
    /// time: a compile can take 140ms (Swift), and three at once would
    /// leave no worker for what comes on screen meanwhile.
    preparing: VecDeque<Language>,
    /// A worker is compiling one of `preparing`.
    compiling: bool,
    /// The count pass, taken only when nothing is waiting or to compile.
    /// It's set once per list rather than with every frame's jobs, since it
    /// covers every file.
    counting: VecDeque<Counted>,
    /// Which count pass `counting` is, so a chunk of a replaced one doesn't
    /// hand its uncounted files back to the new one.
    pass: u64,
    /// Chunks of the count pass being counted.
    chunks: usize,
    /// What jobs are done with; set before any job is.
    settings: Settings,
    /// The viewer's annotations as last published, read as each diff
    /// comes so one in flight when they change still gets them.
    annotations: Arc<AnnotationSet>,
}

impl Queue {
    fn new(settings: Settings) -> Self {
        Self {
            annotations: Arc::default(),
            waiting: VecDeque::new(),
            running: Vec::new(),
            preparing: VecDeque::new(),
            compiling: false,
            counting: VecDeque::new(),
            pass: 0,
            chunks: 0,
            settings,
        }
    }
}

/// A file for the count pass: its position, version and change.
pub(crate) type Counted = (usize, u64, FileChange);

pub(crate) struct Pool {
    shared: Arc<Shared>,
    /// The threads that only count, started with the first count pass so
    /// they don't hold up the first paint: how many, and what with.
    counters: Mutex<Option<(usize, Repo, Sender<Event>)>>,
}

#[derive(Clone)]
pub(crate) struct Settings {
    pub options: Options,
    /// Files get syntax slots only with this set.
    pub highlight: bool,
    /// What highlighting and the syntax diff detect languages with.
    pub grammars: &'static Registry,
    pub cache: Option<Cache>,
}

impl Settings {
    /// The core's per-file work, with these grammars and cache.
    fn prepare(&self) -> Prepare<'_> {
        File::prepare(self.grammars, self.cache.as_ref())
    }
}

impl Pool {
    /// Without a repository (a piped diff), only piped files are diffed.
    pub(crate) fn new(
        repo: Option<&Repo>,
        settings: &Settings,
        tx: &Sender<Event>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared::new(Queue::new(settings.clone())));
        let cores = thread::available_parallelism().map_or(2, std::num::NonZero::get);
        // One core stays free for the UI and the enumeration.
        let threads = cores.saturating_sub(1).clamp(1, 3);
        for _ in 0..threads {
            let (shared, repo, tx) = (Arc::clone(&shared), repo.cloned(), tx.clone());
            thread::Builder::new()
                .name("dffy-diff".into())
                .spawn(move || work(&shared, repo.as_ref(), &tx, false))?;
        }
        // The count pass also takes the cores left over, the UI's included:
        // it starts once the list is complete, and these threads stop
        // between files whenever a job waits or runs.
        let counters = repo
            .filter(|_| cores > threads)
            .map(|r| (cores - threads, r.clone(), tx.clone()));
        Ok(Self {
            shared,
            counters: Mutex::new(counters),
        })
    }

    /// Diffs and highlights with these from the next job on.
    pub(crate) fn set(&self, highlight: bool, whitespace: dffy_core::diff::Whitespace) {
        let mut queue = self.shared.lock();
        queue.settings.highlight = highlight;
        queue.settings.options.whitespace = whitespace;
    }

    /// Replaces the waiting jobs with `jobs`, most wanted first. Jobs that
    /// are running, or whose result the viewer hasn't taken yet, aren't
    /// queued again.
    pub(crate) fn replace(&self, jobs: Vec<Job>) {
        let mut queue = self.shared.lock();
        let Queue {
            waiting, running, ..
        } = &mut *queue;
        waiting.clear();
        waiting.extend(jobs.into_iter().filter(|j| !running.contains(&j.key())));
        self.shared.waiting.store(waiting.len(), Ordering::Relaxed);
        self.shared.ready.notify_all();
    }

    /// Places `annotations` on the diffs that come from now on.
    pub(crate) fn annotate(&self, annotations: AnnotationSet) {
        self.shared.lock().annotations = Arc::new(annotations);
    }

    /// The viewer took `done`: its job may be asked for again.
    pub(crate) fn finish(&self, done: &Done) {
        if let Some(job) = done.job() {
            self.shared.lock().running.retain(|k| *k != job);
        }
    }

    /// Compiles the highlight queries of `languages` once nothing else is
    /// waiting, in order.
    pub(crate) fn prepare(&self, languages: Vec<Language>) {
        self.shared.lock().preparing.extend(languages);
        self.shared.ready.notify_all();
    }

    /// Writes the counts kept for the disk cache now, or waits for the
    /// write under way. The pool writes them itself after each pass.
    pub(crate) fn flush(&self) {
        if let Some(stats) = &self.shared.stats {
            stats.flush();
        }
    }

    /// Replaces the count pass with `files`, in the order to count them;
    /// an empty list cancels it.
    pub(crate) fn count(&self, files: Vec<Counted>) {
        let counters = self
            .counters
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some((n, repo, tx)) = counters {
            // Counting reads each blob once: a cache of them would only
            // hold copies.
            for _ in 0..n {
                let (shared, repo, tx) = (
                    Arc::clone(&self.shared),
                    repo.without_object_cache(),
                    tx.clone(),
                );
                // One that can't start leaves the count pass to the others.
                drop(
                    thread::Builder::new()
                        .name("dffy-count".into())
                        .spawn(move || work(&shared, Some(&repo), &tx, true)),
                );
            }
        }
        let mut queue = self.shared.lock();
        queue.counting = files.into();
        queue.pass += 1;
        self.shared.ready.notify_all();
    }
}

/// What a worker takes next, a job, a query to compile or a chunk of the
/// count pass (of pass `u64`), with the settings to do it with.
enum Work {
    Job(Job, Settings),
    Prepare(Language),
    Count(Vec<Counted>, u64, Settings),
}

/// Waits for the next job, or with none waiting a query to compile, then a
/// chunk of the count pass. A `counter` takes only chunks, and only while
/// no job waits or runs.
fn next(shared: &Shared, counter: bool) -> Work {
    let mut q = shared.lock();
    loop {
        if !counter {
            if let Some(job) = q.waiting.pop_front() {
                shared.waiting.store(q.waiting.len(), Ordering::Relaxed);
                shared.running.fetch_add(1, Ordering::Relaxed);
                q.running.push(job.key());
                return Work::Job(job, q.settings.clone());
            }
            if !q.compiling
                && let Some(language) = q.preparing.pop_front()
            {
                q.compiling = true;
                return Work::Prepare(language);
            }
        }
        let idle = !counter || q.waiting.is_empty() && shared.running.load(Ordering::Relaxed) == 0;
        if idle && !q.counting.is_empty() {
            let n = q.counting.len().min(COUNT_CHUNK);
            let chunk = q.counting.drain(..n).collect();
            q.chunks += 1;
            return Work::Count(chunk, q.pass, q.settings.clone());
        }
        q = shared.ready.wait(q).unwrap_or_else(PoisonError::into_inner);
    }
}

/// Whether `change` is a conflicted file shown as its working-tree file,
/// or as gone from it: one whose conflicts can be resolved there.
fn in_worktree_conflict(change: &FileChange) -> bool {
    change.status == Status::Unmerged
        && matches!(change.new, Some(Side::Worktree { .. }) | None)
        && !matches!(change.old, Some(Side::Worktree { .. }))
}

/// A file's diff: a piped file's read back, or diffed from `repo`; for a
/// conflicted file, with its conflicts, or the preview of `choices`; for an
/// `unchanged` one, its every line.
fn diff(
    repo: Option<&Repo>,
    settings: &Settings,
    change: FileChange,
    piped: Option<Arc<PipedFile>>,
    choices: Option<Arc<Choices>>,
    unchanged: bool,
) -> Result<Box<File>, String> {
    let options = &settings.options;
    match (piped, repo) {
        (Some(piped), _) => super::piped::read(&piped, change)
            .map(Box::new)
            .map_err(|e| format!("reading the diff back failed: {e}")),
        (None, Some(repo)) => repo
            .file_diff(change, options)
            .map(|fd| {
                let prepare = settings.prepare();
                let (whitespace, highlight) = (options.whitespace, settings.highlight);
                Box::new(if unchanged {
                    prepare.unchanged(fd, highlight)
                } else if in_worktree_conflict(&fd.change) {
                    let conflicts = repo.conflicts(&fd);
                    prepare.conflicted(fd, conflicts, choices, whitespace, highlight)
                } else {
                    prepare.file(fd, whitespace, highlight)
                })
            })
            .map_err(|e| e.to_string()),
        (None, None) => Err("there's no repository to diff this file in".to_owned()),
    }
}

/// Each file's added and removed lines, for the tree, until a job waits
/// (or for a `counter`, runs); a file that can't be read counts as nothing
/// changed. Returns the counts and the files left uncounted.
fn count(
    shared: &Shared,
    repo: Option<&Repo>,
    settings: &Settings,
    files: Vec<Counted>,
    counter: bool,
) -> (Vec<(usize, u64, Counts)>, Vec<Counted>) {
    let whitespace = settings.options.whitespace;
    let mut out = Vec::with_capacity(files.len());
    let mut files = files.into_iter();
    for (file, version, change) in files.by_ref() {
        let stat = repo
            .and_then(|r| match &shared.stats {
                Some(stats) => stats.stat(r, &change, whitespace).ok(),
                None => r.stat(&change, whitespace).ok(),
            })
            .unwrap_or_default();
        out.push((
            file,
            version,
            Counts {
                added: stat.added,
                removed: stat.removed,
            },
        ));
        if shared.waiting.load(Ordering::Relaxed) > 0
            || counter && shared.running.load(Ordering::Relaxed) > 0
        {
            break;
        }
    }
    (out, files.collect())
}

/// Takes work until nobody listens. A job stays in the running list after
/// it's done, until the viewer takes its result ([`Pool::finish`]).
fn work(shared: &Shared, repo: Option<&Repo>, tx: &Sender<Event>, counter: bool) {
    loop {
        let (done, pass_done) = match next(shared, counter) {
            Work::Job(job, settings) => {
                let paint = matches!(job, Job::Diff { paint: true, .. });
                let mut done = run_pushing(job, repo, &settings, Some(tx));
                if let Done::Diff {
                    result: Ok(loaded),
                    placed,
                    ..
                } = &mut done
                {
                    let annotations = Arc::clone(&shared.lock().annotations);
                    *placed = place_on(&annotations, loaded, paint);
                }
                let q = shared.lock();
                shared.running.fetch_sub(1, Ordering::Relaxed);
                if !q.counting.is_empty() {
                    shared.ready.notify_all();
                }
                (done, false)
            }
            Work::Prepare(language) => {
                language.prepare();
                language.prepare_scopes();
                shared.lock().compiling = false;
                shared.ready.notify_all();
                continue;
            }
            Work::Count(files, pass, settings) => {
                let (counts, left) = count(shared, repo, &settings, files, counter);
                let mut q = shared.lock();
                if pass == q.pass {
                    for file in left.into_iter().rev() {
                        q.counting.push_front(file);
                    }
                }
                q.chunks -= 1;
                (
                    Done::Counts { counts },
                    q.chunks == 0 && q.counting.is_empty(),
                )
            }
        };
        if tx.send(Event::Diffed(done)).is_err() {
            return;
        }
        if pass_done && let Some(stats) = &shared.stats {
            stats.flush();
        }
    }
}

pub(crate) fn run(job: Job, repo: Option<&Repo>, settings: &Settings) -> Done {
    run_pushing(job, repo, settings, None)
}

/// [`run`], with what a navigator helper this job starts pushes of its
/// index going to `pushes` as [`Event::IndexBuilt`].
#[expect(clippy::too_many_lines, reason = "an arm per kind of job")]
pub(crate) fn run_pushing(
    job: Job,
    repo: Option<&Repo>,
    settings: &Settings,
    pushes: Option<&Sender<Event>>,
) -> Done {
    match job {
        Job::Diff {
            file,
            version,
            change,
            piped,
            choices,
            unchanged,
            ..
        } => Done::Diff {
            file,
            version,
            result: diff(repo, settings, change, piped, choices, unchanged),
            placed: None,
        },
        Job::Place(job) => job.run(),
        Job::Emphasis {
            file,
            version,
            hunk,
            diff,
            syntax,
        } => {
            let (emphasis, syntax) = settings.prepare().emphasis(&diff, hunk, syntax.as_ref());
            Done::Emphasis {
                file,
                version,
                hunk,
                emphasis,
                syntax,
                diff,
            }
        }
        Job::Search {
            file,
            version,
            change,
            piped,
            reading,
            threads,
            needle,
            id,
            live,
        } => {
            // Only their text counts here.
            let blocks = view::thread_blocks(&threads, |i, _| u32::try_from(i).ok().map(BlockId));
            let stop = || live.load(Ordering::Relaxed) != id;
            // Read alone, a file shows whole: nothing is hidden.
            let hidden = reading.as_ref().map(|_| &[][..]);
            let found = match change {
                Some(change) => diff(repo, settings, change, piped, None, false)
                    .ok()
                    .and_then(|l| {
                        let reading = reading.as_ref();
                        document::find(l.diff(), hidden, reading, &blocks, &needle, stop)
                    }),
                None => document::find_blocks(&blocks, &needle, stop),
            };
            Done::Search {
                file,
                version,
                id,
                count: found.map(|m| m.len()),
            }
        }
        Job::Find {
            file,
            version,
            diff,
            hidden,
            reading,
            blocks,
            blocks_version,
            needle,
            id,
            live,
        } => Done::Found {
            file,
            version,
            id,
            matches: find(
                diff.as_deref(),
                (&hidden, reading.as_ref()),
                &blocks,
                &needle,
                || live.load(Ordering::Relaxed) != id,
            ),
            diff,
            hidden,
            reading,
            blocks_version,
        },
        Job::Structural {
            file,
            version,
            diff,
        } => Done::Structural {
            file,
            version,
            result: settings.prepare().structural(&diff),
            diff,
        },
        Job::GapSyntax {
            file,
            version,
            gap,
            side,
            lines,
            diff,
            syntax,
        } => Done::GapSyntax {
            file,
            version,
            gap,
            side,
            spans: settings.prepare().gap_syntax(&diff, &syntax, side, lines),
            diff,
        },
        Job::Scopes {
            file,
            version,
            diff,
            syntax,
        } => {
            settings.prepare().scopes(&diff, &syntax);
            Done::Scopes { file, version }
        }
        Job::Render(job) => Done::Rendered {
            file: job.file,
            version: job.version,
            id: job.id,
            result: render(&job, settings),
            diff: job.diff,
        },
        Job::ListPaths { snapshot, id, live } => Done::Listed {
            snapshot,
            id,
            result: match repo {
                Some(repo) => {
                    Paths::list(repo, snapshot, || live.load(Ordering::Relaxed) != id).map(Arc::new)
                }
                None => Err(Some("there's no repository to list".to_owned())),
            },
        },
        Job::Score {
            paths,
            candidates,
            query,
            id,
            live,
        } => Done::Scored {
            id,
            matches: paths.score(&query, &candidates, || live.load(Ordering::Relaxed) != id),
        },
        Job::Navigator(job) => Done::Navigator {
            side: job.side,
            id: job.id,
            result: navigator(&job, repo, settings, pushes),
        },
        Job::Lookup(job) => Done::Looked {
            id: job.id,
            result: lookup(&job, repo),
        },
        Job::PeekCode(job) => Done::PeekCode {
            id: job.id,
            result: peek_code(&job, repo),
        },
        Job::References(job) => Done::Referenced {
            id: job.id,
            result: references(&job),
        },
    }
}

/// A step of a [`Job::References`]: its search started by the first, then
/// the files the helper gets through in [`REFERENCES_STEP`], at least one.
/// The search's id is never held across a request to the helper.
fn references(job: &ReferencesJob) -> Result<Referenced, Unanswered> {
    let kept = *job.search.lock().unwrap_or_else(PoisonError::into_inner);
    let id = if let Some(id) = kept {
        id
    } else {
        let origin = (job.origin.path.as_slice(), job.origin.language.name());
        let id = (job.helper).start_search(job.navigator, "references", &job.name, origin, 0)?;
        let mut search = job.search.lock().unwrap_or_else(PoisonError::into_inner);
        match *search {
            // Another step started one meanwhile: that one goes on.
            Some(other) => {
                drop(search);
                job.helper.close_search(id);
                other
            }
            None => *search.insert(id),
        }
    };
    let step =
        (job.helper).step_references(id, (REFERENCES_STEP, None, true), Some(&job.cancel))?;
    if job.cancel.is_set() {
        return Err(Unanswered::Cancelled);
    }
    Ok(Referenced {
        hits: step.hits,
        files: step.files,
        searched: step.searched,
        done: step.done,
        indexed: step.indexed,
    })
}

/// The helper to ask, with its generation: the one in `slot`, or one
/// started now in the repository's working tree when there's none or the
/// one of generation `gone` failed and nothing replaced it yet. What it
/// pushes goes to `pushes`, tagged with its generation.
fn helper(
    slot: &HelperSlot,
    (repo, settings): (&Repo, &Settings),
    gone: Option<(u64, &str)>,
    pushes: Option<&Sender<Event>>,
) -> Result<(u64, Helper), Unanswered> {
    let unavailable =
        |why: String| Unanswered::Unavailable(format!("definitions unavailable: {why}"));
    let root = repo
        .workdir()
        .or_else(|| repo.git_dir())
        .ok_or_else(|| unavailable("there's no repository to look names up in".to_owned()))?;
    let cache = (settings.cache.as_ref()).ok_or_else(|| unavailable(NO_CACHE.to_owned()))?;
    let mut helpers = slot.lock().unwrap_or_else(PoisonError::into_inner);
    helpers
        .get(gone, |generation| {
            let forward = pushes.cloned().map(|tx| {
                Box::new(move |pushed: Pushed| {
                    tx.send(Event::IndexBuilt { generation, pushed }).ok();
                }) as Box<dyn Fn(Pushed) + Send>
            });
            Helper::start(root, cache, 1, forward)
        })
        .map_err(Unanswered::Unavailable)
}

/// A [`Job::Navigator`]'s navigator, held by the helper, and the helper's
/// generation: what differs between the diff's snapshots listed whole,
/// past any path the view filters by. A helper gone is started again,
/// once, unless the helpers keep exiting.
fn navigator(
    job: &NavigatorJob,
    repo: Option<&Repo>,
    settings: &Settings,
    pushes: Option<&Sender<Event>>,
) -> Result<(u64, Opened), Unanswered> {
    let repo = repo.ok_or_else(|| {
        Unanswered::Unavailable("there's no repository to look names up in".to_owned())
    })?;
    let open = |helper: &Helper| helper.open(job.snapshots, job.side, settings.options.whitespace);
    let (generation, first) = helper(&job.helper, (repo, settings), None, pushes)?;
    match open(&first) {
        Err(refused) if refused.helper_gone() => {
            let gone = Some((generation, refused.message.as_str()));
            let (generation, again) = helper(&job.helper, (repo, settings), gone, pushes)?;
            match open(&again) {
                Ok(opened) => Ok((generation, opened)),
                Err(refused) if refused.helper_gone() => Err(Unanswered::Unavailable(format!(
                    "definitions unavailable: {}",
                    refused.message
                ))),
                Err(refused) => Err(Unanswered::Failed(refused.message)),
            }
        }
        Err(refused) => Err(Unanswered::Failed(refused.message)),
        Ok(opened) => Ok((generation, opened)),
    }
}

/// A [`Job::Lookup`]'s answer: the definitions found, or the text
/// search's lines, with the first line of each of the first.
fn lookup(job: &LookupJob, repo: Option<&Repo>) -> Result<Looked, Unanswered> {
    let repo = repo.ok_or_else(|| {
        Unanswered::Unavailable("there's no repository to look names up in".to_owned())
    })?;
    let cancel = &*job.cancel;
    let origin = (job.origin.path.as_slice(), job.origin.language.name());
    let mut looked = if job.text {
        let search =
            (job.helper).start_search(job.navigator, "text", &job.name, origin, TEXT_MATCHES)?;
        let mut candidates = Vec::new();
        let done = loop {
            let step = (job.helper).step_text(search, TEXT_STEP, Some(cancel));
            let step = match step {
                Ok(step) => step,
                Err(e) => {
                    job.helper.close_search(search);
                    return Err(e.into());
                }
            };
            candidates.extend(step.hits);
            if step.done || cancel.is_set() {
                break step.done;
            }
        };
        job.helper.close_search(search);
        let progress = job.helper.progress(job.navigator)?.progress;
        Looked {
            total: candidates.len(),
            candidates,
            progress,
            text: true,
            complete: done,
            lines: Vec::new(),
        }
    } else {
        let found = (job.helper).lookup(job.navigator, &job.name, origin, Some(cancel))?;
        Looked {
            candidates: found.candidates,
            total: found.total,
            progress: found.progress,
            text: false,
            complete: true,
            lines: Vec::new(),
        }
    };
    if cancel.is_set() {
        return Err(Unanswered::Cancelled);
    }
    let listed = &looked.candidates[..looked.candidates.len().min(LISTED_LINES)];
    let mut read: Option<(&BString, Option<Text>)> = None;
    looked.lines = listed
        .iter()
        .map(|c| {
            if read.as_ref().is_none_or(|(path, _)| **path != c.path) {
                let text = repo.load(c.path.as_ref(), &c.side).ok().map(Text::new);
                read = Some((&c.path, text));
            }
            let text = read.as_ref()?.1.as_ref()?;
            (c.start < text.len()).then(|| {
                let line = String::from_utf8_lossy(text.line(c.start));
                line.trim().chars().take(200).collect()
            })
        })
        .collect();
    Ok(looked)
}

/// Why go-to-definition is off: its index lives in the disk cache.
pub(crate) const NO_CACHE: &str =
    "go to definition keeps its index in the disk cache, which is off here";

/// A [`Job::PeekCode`]'s lines.
fn peek_code(job: &PeekCodeJob, repo: Option<&Repo>) -> Result<Code, String> {
    let repo = repo.ok_or("there's no repository to read the file from")?;
    let bytes = (repo.load(job.path.as_ref(), &job.side)).map_err(|e| e.to_string())?;
    if dffy_core::git::is_binary(&bytes) {
        return Err("it's binary".to_owned());
    }
    let text = Text::new(bytes);
    let spans = (job.language.as_ref())
        .and_then(|lang| highlight::parse(lang, text.bytes()))
        .map(|parsed| parsed.spans(&text, 0..text.len()));
    let lines = (0..text.len())
        .map(|i| {
            let line = text.line(i);
            let line = line.strip_suffix(b"\n").unwrap_or(line);
            String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(line)).into_owned()
        })
        .collect();
    Ok(Code { lines, spans })
}

/// The matches of a [`Job::Find`]: in `diff` with `hidden` lines, read on
/// `reading`, and in `blocks`, or without a diff in a sheet's `blocks`
/// alone.
pub(crate) fn find(
    diff: Option<&FileDiff>,
    (hidden, reading): (&[Range<usize>], Option<&document::Reading>),
    blocks: &[document::Block],
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    match diff {
        Some(diff) => document::find(diff, Some(hidden), reading, blocks, needle, cancelled),
        None => document::find_blocks(blocks, needle, cancelled),
    }
}

/// Where `annotations` on `loaded`'s path land on its line diff, with
/// `paint` the code of those lost parsed (see [`model::land`]); `None`
/// when the path has none.
pub(crate) fn place_on(
    annotations: &AnnotationSet,
    loaded: &File,
    paint: bool,
) -> Option<model::Landed> {
    let diff = model::line_diff(loaded);
    let annotated = annotations.on(&diff.change.path)?;
    Some(model::land(annotated, diff, paint))
}

/// A side's text for a renderer: `None` for a side the file doesn't have.
fn side_text(present: bool, side: &Text) -> Result<Option<&str>, Option<String>> {
    present
        .then(|| std::str::from_utf8(side.bytes()))
        .transpose()
        .map_err(|_| Some("not UTF-8 text, so not rendered".to_owned()))
}

/// The job's diff rendered and diffed block by block. A side that isn't
/// whole UTF-8 text is never sent: the line diff stays.
fn render(job: &RenderJob, settings: &Settings) -> Result<Box<File>, Option<String>> {
    let RenderJob {
        diff,
        program,
        width,
        light,
        id,
        live,
        ..
    } = job;
    if diff.binary || diff.too_large {
        return Err(Some("binary or too large to render".to_owned()));
    }
    let change = &diff.change;
    let request = plugin::Request {
        path: &String::from_utf8_lossy(&change.path),
        old: side_text(change.old.is_some(), &diff.old)?,
        new: side_text(change.new.is_some(), &diff.new)?,
        width: *width,
        light: *light,
    };
    let lines = [diff.old.len(), diff.new.len()];
    let cancelled = || live.load(Ordering::Relaxed) != *id;
    match plugin::render(program, &request, lines, settings.cache.as_ref(), cancelled) {
        Ok(sides) => Ok(Box::new(view::rendered::build(
            Arc::clone(diff),
            sides,
            settings.grammars,
            settings.options.context,
            *width,
        ))),
        Err(plugin::Failure::Cancelled) => Err(None),
        Err(failure) => Err(Some(format!(
            "{}{} {failure}",
            plugin::PREFIX,
            program.name
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use dffy_core::diff::{Options, Text, hunks};
    use dffy_core::git::{Mode, Side, Status};

    use super::*;

    fn modified() -> Arc<FileDiff> {
        let (old, new) = (Text::new(b"old\n".to_vec()), Text::new(b"new\n".to_vec()));
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        Arc::new(FileDiff {
            change: FileChange {
                path: "notes.txt".into(),
                status: Status::Modified,
                old: side,
                new: side,
                staging: None,
            },
            hunks: hunks(&old, &new, &Options::default()),
            old,
            new,
            binary: false,
            too_large: false,
            note: None,
        })
    }

    /// Compiling queries ahead waits for every job asked for, takes one
    /// worker at a time, and comes before the count pass.
    #[test]
    fn queries_compile_after_the_jobs_one_at_a_time_before_the_count_pass() {
        let diff = modified();
        let grammars = crate::grammars();
        let lang = |name| grammars.named(name).expect("bundled").clone();
        let queue = Queue {
            waiting: VecDeque::from([Job::Emphasis {
                file: 0,
                version: 0,
                hunk: 0,
                diff: Arc::clone(&diff),
                syntax: None,
            }]),
            preparing: VecDeque::from([lang("swift"), lang("kotlin")]),
            counting: VecDeque::from([(0, 0, diff.change.clone())]),
            ..Queue::new(Settings {
                options: Options::default(),
                highlight: true,
                grammars,
                cache: None,
            })
        };
        let shared = Shared::new(queue);
        let taken = |work: Work| match work {
            Work::Job(..) => "job".to_owned(),
            Work::Prepare(language) => language.name().to_owned(),
            Work::Count(..) => "count".to_owned(),
        };
        assert_eq!(taken(next(&shared, false)), "job");
        assert_eq!(taken(next(&shared, false)), "swift");
        assert_eq!(
            taken(next(&shared, false)),
            "count",
            "a second compile at once"
        );
        shared.lock().compiling = false;
        assert_eq!(taken(next(&shared, false)), "kotlin");
    }

    /// The threads that only count stay off the count pass while a job
    /// waits or runs, and a worker counting a chunk hands the rest back
    /// once a job waits.
    #[test]
    fn counting_yields_to_jobs() {
        let diff = modified();
        let files = (0..4).map(|i| (i, 0, diff.change.clone()));
        let shared = Shared::new(Queue {
            counting: files.collect(),
            ..Queue::new(Settings {
                options: Options::default(),
                highlight: false,
                grammars: crate::grammars(),
                cache: None,
            })
        });
        shared.running.store(1, Ordering::Relaxed);
        let shared = Arc::new(shared);
        let waiter = Arc::clone(&shared);
        let counter = thread::spawn(move || matches!(next(&waiter, true), Work::Count(..)));
        thread::sleep(Duration::from_millis(100));
        assert!(!counter.is_finished(), "counted while a job ran");
        {
            let _q = shared.lock();
            shared.running.store(0, Ordering::Relaxed);
            shared.ready.notify_all();
        }
        assert!(counter.join().expect("counter"), "counts once idle");

        shared.waiting.store(1, Ordering::Relaxed);
        let chunk: Vec<Counted> = (10..13).map(|i| (i, 0, diff.change.clone())).collect();
        let settings = shared.lock().settings.clone();
        let (done, left) = count(&shared, None, &settings, chunk, false);
        assert_eq!(done.len(), 1, "stops after the file it's on");
        assert_eq!(left.iter().map(|f| f.0).collect::<Vec<_>>(), [11, 12]);
    }

    /// Go to definition's jobs ask the navigator helper, which the first
    /// navigator job starts: one without the disk cache says it's
    /// unavailable, the helper pushes how far its build is up to its end,
    /// and a search's steps go on until it's done.
    #[test]
    fn navigator_jobs_go_through_the_helper() {
        client::use_program(crate::navigator::built_program());
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .expect("git runs")
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        for name in ["a.c", "b.c", "c.c"] {
            std::fs::write(
                d.join(name),
                "int f(void)\n{\n\treturn 0;\n}\nint g = f();\n",
            )
            .expect("write");
        }
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        let repo = Repo::discover(d).expect("repo");
        let spec = repo.resolve(&[], false, false).expect("resolves");
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let settings = Settings {
            options: Options::default(),
            highlight: false,
            grammars: crate::grammars(),
            cache: Some(Cache::new(cache_dir.path().to_owned(), 1 << 20)),
        };
        let make = |settings: &Settings, helper: &HelperSlot| {
            let job = Job::Navigator(NavigatorJob {
                side: document::Side::New,
                snapshots: spec.snapshots(false).expect("snapshots"),
                helper: Arc::clone(helper),
                id: 0,
            });
            let (tx, rx) = mpsc::channel();
            let Done::Navigator { result, .. } = run_pushing(job, Some(&repo), settings, Some(&tx))
            else {
                panic!("a navigator job answers as one")
            };
            (result, rx)
        };
        let none = Settings {
            cache: None,
            ..settings
        };
        let (unavailable, _) = make(&none, &Arc::default());
        assert!(
            matches!(&unavailable, Err(Unanswered::Unavailable(why)) if why.starts_with("definitions unavailable")),
            "{unavailable:?}"
        );

        let slot: HelperSlot = Arc::default();
        let (opened, pushes) = make(&settings, &slot);
        let (generation, opened) = opened.expect("opens");
        let helper = slot.lock().expect("lock").current().expect("started").1;
        // The helper pushes how far the build is, its end last.
        loop {
            match pushes.recv_timeout(Duration::from_secs(30)) {
                Ok(Event::IndexBuilt {
                    generation: g,
                    pushed,
                }) if g == generation && pushed.navigator == opened.navigator => {
                    if !pushed.built.building {
                        assert!(pushed.built.progress.complete());
                        break;
                    }
                }
                Ok(_) => {}
                Err(e) => panic!("no push: {e}"),
            }
        }

        let job = ReferencesJob {
            helper: helper.clone(),
            navigator: opened.navigator,
            search: Arc::new(Mutex::new(None)),
            name: "f".to_owned(),
            origin: navigate::Origin {
                path: "a.c".into(),
                language: crate::grammars().named("c").expect("bundled").clone(),
            },
            id: 3,
            cancel: Arc::new(Cancel::default()),
        };
        let mut hits = 0;
        loop {
            let step = references(&job).expect("steps");
            hits += step.hits.len();
            if step.done {
                break;
            }
        }
        assert!(hits >= 3, "{hits} uses");
        helper.close();
    }

    /// A job's result either makes the job unwanted (it's stored) or wanted
    /// again (it's for a diff since replaced, and the draw that follows asks
    /// for the current diff's job under the same key). So a finished job
    /// holds back the same job until the viewer takes its result: an ask in
    /// between would run it twice (a renderer started twice), and one after
    /// must go through (or the file stays without highlighting).
    #[test]
    fn a_done_job_is_held_back_until_the_viewer_takes_its_result() {
        let (tx, rx) = mpsc::channel();
        let settings = Settings {
            options: Options::default(),
            highlight: false,
            grammars: crate::grammars(),
            cache: None,
        };
        let pool = Pool::new(None, &settings, &tx).expect("pool");
        let diff = modified();
        let ask = || {
            pool.replace(vec![Job::Emphasis {
                file: 0,
                version: 0,
                hunk: 0,
                diff: Arc::clone(&diff),
                syntax: None,
            }]);
        };
        let result = || match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Diffed(done @ Done::Emphasis { .. })) => Some(done),
            _ => None,
        };
        ask();
        let done = result().expect("the job's result");
        ask();
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "asked again before the viewer took its result, it ran again"
        );
        pool.finish(&done);
        ask();
        assert!(result().is_some(), "asked again after, it runs");
    }

    /// A placement answers its job, generation included: the same job is
    /// held back until the viewer takes it, while one for annotations that
    /// changed since runs at once.
    #[test]
    fn a_placement_is_held_back_by_its_generation() {
        let (tx, rx) = mpsc::channel();
        let settings = Settings {
            options: Options::default(),
            highlight: false,
            grammars: crate::grammars(),
            cache: None,
        };
        let pool = Pool::new(None, &settings, &tx).expect("pool");
        let diff = modified();
        let ask = |generation| {
            let annotation = dffy_core::annotation::Annotation {
                id: "a".to_owned(),
                source: "test",
                path: "notes.txt".into(),
                old_path: None,
                side: document::Side::New,
                lines: 0..1,
                anchor: None,
            };
            pool.replace(vec![Job::Place(PlaceJob {
                file: 0,
                version: 0,
                annotated: Annotated {
                    generation,
                    annotations: Arc::new([annotation]),
                },
                diff: Arc::clone(&diff),
                paint: false,
            })]);
        };
        let result = || match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Event::Diffed(done @ Done::Placed { .. })) => Some(done),
            _ => None,
        };
        ask(1);
        let done = result().expect("placed");
        assert!(
            matches!(&done, Done::Placed { placed, .. } if placed.generation == 1 && placed.placements.len() == 1)
        );
        ask(1);
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "asked again before the viewer took it, it ran again"
        );
        ask(2);
        assert!(result().is_some(), "the annotations changed: it runs");
        pool.finish(&done);
        ask(1);
        assert!(result().is_some(), "taken, it may run again");
    }
}

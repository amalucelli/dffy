//! Work off the UI thread: the change set, which streams the changed files
//! as the core finds them, pairs renames once it has them all, and then
//! keeps them current; and a small pool that diffs files and places their
//! annotations on them, computes emphasis and syntax spans for the hunks on
//! screen and then the shown file's definitions for the scope line, and
//! syntax-diffs the files on screen when that view is on. When it has nothing else to do,
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
use dffy_core::diff::{Emphasis, Options};
use dffy_core::document::{self, BlockId, Counts, File, Matches, Needle, Prepare, SyntaxSlot};
use dffy_core::git::{ChangeSet, FileChange, FileDiff, Repo, Revisions, Side, StatStore, Status};
use dffy_core::highlight::{HunkSyntax, Language, Registry};
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::Spans;
use dffy_core::watch::MAX_BATCH;

use super::Event;
use super::piped::PipedFile;
use super::view;
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
    /// without a `change`, a sheet's, in its threads alone. Stops once
    /// `live` holds another search's id.
    Search {
        file: usize,
        version: u64,
        change: Option<FileChange>,
        piped: Option<Arc<PipedFile>>,
        threads: Arc<[Thread]>,
        needle: Arc<Needle>,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// The diff search's matches in the shown file's visible lines (its
    /// hunks and the unchanged lines not `hidden`) and in the text of its
    /// `blocks`, the comments' (version `blocks_version`); without a
    /// `diff`, a sheet's, in its blocks alone.
    Find {
        file: usize,
        version: u64,
        diff: Option<Arc<FileDiff>>,
        hidden: Vec<Range<usize>>,
        blocks: Vec<document::Block>,
        blocks_version: u64,
        needle: Arc<Needle>,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// Syntax runs for the new-side `lines` of a gap someone revealed.
    GapSyntax {
        file: usize,
        version: u64,
        gap: usize,
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
}

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
    GapSyntax(usize),
    Scopes,
    /// With the search's id: a running scan for an older search doesn't
    /// hold back one for the new.
    Search(u64),
    Find(u64),
    Render(u64),
}

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
                file, version, gap, ..
            } => (file, version, Of::GapSyntax(*gap)),
            Self::Scopes { file, version, .. } => (file, version, Of::Scopes),
            Self::Search {
                file, version, id, ..
            } => (file, version, Of::Search(*id)),
            Self::Find {
                file, version, id, ..
            } => (file, version, Of::Find(*id)),
            Self::Render(job) => (&job.file, &job.version, Of::Render(job.id)),
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
    /// `hidden` lines and its blocks of `blocks_version` have them; `None`
    /// when the scan was stopped.
    Found {
        file: usize,
        version: u64,
        id: u64,
        diff: Option<Arc<FileDiff>>,
        hidden: Vec<Range<usize>>,
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
                file, version, gap, ..
            } => (file, version, Of::GapSyntax(gap)),
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
        };
        Some(Key { file, version, of })
    }

    /// The position and version of the file this is for; `None` for a
    /// chunk of the count pass, whose files carry their own.
    pub(crate) fn file(&self) -> Option<(usize, u64)> {
        self.job().map(|k| (k.file, k.version))
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
/// conflicted file, with its conflicts, or the preview of `choices`.
fn diff(
    repo: Option<&Repo>,
    settings: &Settings,
    change: FileChange,
    piped: Option<Arc<PipedFile>>,
    choices: Option<Arc<Choices>>,
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
                Box::new(if in_worktree_conflict(&fd.change) {
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
                let mut done = run(job, repo, &settings);
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

#[expect(clippy::too_many_lines, reason = "an arm per kind of job")]
fn run(job: Job, repo: Option<&Repo>, settings: &Settings) -> Done {
    match job {
        Job::Diff {
            file,
            version,
            change,
            piped,
            choices,
            ..
        } => Done::Diff {
            file,
            version,
            result: diff(repo, settings, change, piped, choices),
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
            threads,
            needle,
            id,
            live,
        } => {
            // Only their text counts here.
            let blocks = view::thread_blocks(&threads, |i, _| u32::try_from(i).ok().map(BlockId));
            let stop = || live.load(Ordering::Relaxed) != id;
            let found = match change {
                Some(change) => diff(repo, settings, change, piped, None)
                    .ok()
                    .and_then(|l| document::find(l.diff(), None, &blocks, &needle, stop)),
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
            blocks,
            blocks_version,
            needle,
            id,
            live,
        } => Done::Found {
            file,
            version,
            id,
            matches: find(diff.as_deref(), &hidden, &blocks, &needle, || {
                live.load(Ordering::Relaxed) != id
            }),
            diff,
            hidden,
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
            lines,
            diff,
            syntax,
        } => Done::GapSyntax {
            file,
            version,
            gap,
            spans: settings.prepare().gap_syntax(&diff, &syntax, lines),
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
    }
}

/// The matches of a [`Job::Find`]: in `diff` with `hidden` lines and in
/// `blocks`, or without a diff in a sheet's `blocks` alone.
pub(crate) fn find(
    diff: Option<&FileDiff>,
    hidden: &[Range<usize>],
    blocks: &[document::Block],
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    match diff {
        Some(diff) => document::find(diff, Some(hidden), blocks, needle, cancelled),
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
fn side_text(present: bool, side: &dffy_core::diff::Text) -> Result<Option<&str>, Option<String>> {
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

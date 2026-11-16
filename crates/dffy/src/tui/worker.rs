//! Work off the UI thread: the change set, which streams the changed files
//! as the core finds them, pairs renames once it has them all, and then
//! keeps them current; and a small pool that diffs files, computes
//! emphasis and syntax spans for the hunks on screen, and syntax-diffs the
//! files on screen when that view is on. When it has nothing else to do,
//! the pool counts every file's lines for the file tree. Results come back
//! as [`Event`]s.

use std::collections::VecDeque;
use std::io;
use std::ops::{ControlFlow, Range};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;

use dffy_core::bstr::BString;
use dffy_core::cache::Cache;
use dffy_core::diff::{Emphasis, Options, emphasize};
use dffy_core::git::{ChangeSet, FileChange, FileDiff, Repo, Revisions};
use dffy_core::highlight::{FileSyntax, HunkSyntax, Registry};
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::Spans;
use dffy_core::watch::MAX_BATCH;

use super::Event;
use super::piped::PipedFile;
use super::view::{self, Counts, Loaded, Matches, Needle, SyntaxSlot};
use crate::plugin::{self, Program};
use crate::print;

/// What to bring the file list up to date with.
pub(crate) enum Request {
    /// These repository-relative paths changed in the working tree.
    Refresh(Vec<BString>),
    /// Anything may have changed, git state included.
    Rescan,
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
    selected: Selected,
    tx: Sender<Event>,
) -> io::Result<Sender<Request>> {
    let (requests, rx) = mpsc::channel();
    thread::Builder::new()
        .name("dffy-changes".into())
        .spawn(move || {
            let others = |empty: bool| {
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
                || !others(empty)
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
            while let Ok(first) = rx.recv() {
                let mut paths = Vec::new();
                let mut rescan = false;
                for request in std::iter::once(first).chain(rx.try_iter()) {
                    match request {
                        Request::Refresh(p) => paths.extend(p),
                        Request::Rescan => rescan = true,
                    }
                }
                paths.sort_unstable();
                paths.dedup();
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
                if event.is_some_and(|event| tx.send(event).is_err()) || (empty && !others(true)) {
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
    Diff {
        file: usize,
        version: u64,
        change: FileChange,
        /// A piped file's patch to read back, instead of the repository.
        piped: Option<Arc<PipedFile>>,
    },
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
    /// the hunks of a file not on screen. Stops once `live` holds another
    /// search's id.
    Search {
        file: usize,
        version: u64,
        change: FileChange,
        piped: Option<Arc<PipedFile>>,
        needle: Arc<Needle>,
        id: u64,
        live: Arc<AtomicU64>,
    },
    /// The diff search's matches in the shown file's visible lines: its
    /// hunks and the unchanged lines not `hidden`.
    Find {
        file: usize,
        version: u64,
        diff: Arc<FileDiff>,
        hidden: Vec<Range<usize>>,
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
    Emphasis(usize),
    Structural,
    GapSyntax(usize),
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
        result: Result<Box<Loaded>, String>,
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
    /// A file's matches of search `id`; `None` when it can't be read.
    Search {
        file: usize,
        version: u64,
        id: u64,
        count: Option<usize>,
    },
    /// The matches of search `id` in the file shown, as `diff` with
    /// `hidden` lines has them; `None` when the scan was stopped.
    Found {
        file: usize,
        version: u64,
        id: u64,
        diff: Arc<FileDiff>,
        hidden: Vec<Range<usize>>,
        matches: Option<Matches>,
    },
    /// The rendered view of `diff` for render `id`: why there's none as
    /// `Err(Some(..))`, or `Err(None)` when it was cancelled.
    Rendered {
        file: usize,
        version: u64,
        id: u64,
        diff: Arc<FileDiff>,
        result: Result<Box<Loaded>, Option<String>>,
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

struct Queue {
    waiting: VecDeque<Job>,
    /// Jobs taken by a worker whose result the viewer hasn't taken yet
    /// ([`Pool::finish`]). Until then the result may still make the job
    /// unwanted, so asking for it again would run it twice; from then on
    /// the viewer's next ask is the one that counts.
    running: Vec<Key>,
    /// The count pass, taken only when nothing is waiting. It's set once per
    /// list rather than with every frame's jobs, since it covers every file.
    counting: VecDeque<Counted>,
    /// What jobs are done with; set before any job is.
    settings: Settings,
}

/// A file for the count pass: its position, version and change.
pub(crate) type Counted = (usize, u64, FileChange);

pub(crate) struct Pool {
    shared: Arc<(Mutex<Queue>, Condvar)>,
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

impl Pool {
    /// Without a repository (a piped diff), only piped files are diffed.
    pub(crate) fn new(
        repo: Option<&Repo>,
        settings: &Settings,
        tx: &Sender<Event>,
    ) -> io::Result<Self> {
        let queue = Queue {
            waiting: VecDeque::new(),
            running: Vec::new(),
            counting: VecDeque::new(),
            settings: settings.clone(),
        };
        let shared = Arc::new((Mutex::new(queue), Condvar::new()));
        // One core stays free for the UI and the enumeration.
        let threads = thread::available_parallelism()
            .map_or(2, std::num::NonZero::get)
            .saturating_sub(1)
            .clamp(1, 3);
        for _ in 0..threads {
            let (shared, repo, tx) = (Arc::clone(&shared), repo.cloned(), tx.clone());
            thread::Builder::new()
                .name("dffy-diff".into())
                .spawn(move || work(&shared, repo.as_ref(), &tx))?;
        }
        Ok(Self { shared })
    }

    /// Diffs and highlights with these from the next job on.
    pub(crate) fn set(&self, highlight: bool, whitespace: dffy_core::diff::Whitespace) {
        let mut queue = self.shared.0.lock().unwrap_or_else(PoisonError::into_inner);
        queue.settings.highlight = highlight;
        queue.settings.options.whitespace = whitespace;
    }

    /// Replaces the waiting jobs with `jobs`, most wanted first. Jobs that
    /// are running, or whose result the viewer hasn't taken yet, aren't
    /// queued again.
    pub(crate) fn replace(&self, jobs: Vec<Job>) {
        let (queue, ready) = &*self.shared;
        let mut queue = queue.lock().unwrap_or_else(PoisonError::into_inner);
        let Queue {
            waiting, running, ..
        } = &mut *queue;
        waiting.clear();
        waiting.extend(jobs.into_iter().filter(|j| !running.contains(&j.key())));
        ready.notify_all();
    }

    /// The viewer took `done`: its job may be asked for again.
    pub(crate) fn finish(&self, done: &Done) {
        if let Some(job) = done.job() {
            self.shared
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .running
                .retain(|k| *k != job);
        }
    }

    /// Replaces the count pass with `files`, in the order to count them;
    /// an empty list cancels it.
    pub(crate) fn count(&self, files: Vec<Counted>) {
        let (queue, ready) = &*self.shared;
        let mut queue = queue.lock().unwrap_or_else(PoisonError::into_inner);
        queue.counting = files.into();
        ready.notify_all();
    }
}

/// What a worker takes next, a job or a chunk of the count pass, with the
/// settings to do it with.
enum Work {
    Job(Job, Settings),
    Count(Vec<Counted>, Settings),
}

/// Waits for the next job, or with none waiting a chunk of the count pass.
fn next(shared: &(Mutex<Queue>, Condvar)) -> Work {
    let (queue, ready) = shared;
    let mut q = queue.lock().unwrap_or_else(PoisonError::into_inner);
    loop {
        if let Some(job) = q.waiting.pop_front() {
            q.running.push(job.key());
            return Work::Job(job, q.settings.clone());
        }
        if !q.counting.is_empty() {
            let n = q.counting.len().min(COUNT_CHUNK);
            let chunk = q.counting.drain(..n).collect();
            return Work::Count(chunk, q.settings.clone());
        }
        q = ready.wait(q).unwrap_or_else(PoisonError::into_inner);
    }
}

/// A file's diff: a piped file's read back, or diffed from `repo`.
fn diff(
    repo: Option<&Repo>,
    settings: &Settings,
    change: FileChange,
    piped: Option<Arc<PipedFile>>,
) -> Result<Box<Loaded>, String> {
    let options = &settings.options;
    match (piped, repo) {
        (Some(piped), _) => piped
            .diff(change)
            .map(|(fd, headings)| Box::new(Loaded::piped(fd, headings)))
            .map_err(|e| format!("reading the diff back failed: {e}")),
        (None, Some(repo)) => repo
            .file_diff(change, options)
            .map(|fd| {
                let highlight = settings.highlight.then_some(settings.grammars);
                Box::new(Loaded::new(fd, options.whitespace, highlight))
            })
            .map_err(|e| e.to_string()),
        (None, None) => Err("there's no repository to diff this file in".to_owned()),
    }
}

/// Each file's added and removed lines, for the tree; a file that can't
/// be read counts as nothing changed.
fn count(
    repo: Option<&Repo>,
    settings: &Settings,
    files: Vec<Counted>,
) -> Vec<(usize, u64, Counts)> {
    files
        .into_iter()
        .map(|(file, version, change)| {
            let stat = repo
                .and_then(|r| r.stat(&change, settings.options.whitespace).ok())
                .unwrap_or_default();
            (
                file,
                version,
                Counts {
                    added: stat.added,
                    removed: stat.removed,
                },
            )
        })
        .collect()
}

/// Takes work until nobody listens. A job stays in the running list after
/// it's done, until the viewer takes its result ([`Pool::finish`]).
fn work(shared: &(Mutex<Queue>, Condvar), repo: Option<&Repo>, tx: &Sender<Event>) {
    loop {
        let done = match next(shared) {
            Work::Job(job, settings) => run(job, repo, &settings),
            Work::Count(files, settings) => Done::Counts {
                counts: count(repo, &settings, files),
            },
        };
        if tx.send(Event::Diffed(done)).is_err() {
            return;
        }
    }
}

/// The emphasis of hunk `hunk`, and its syntax spans when the file has a
/// slot for them. The first hunk of a file to get here parses it; the parse
/// gives the emphasis its tokens too.
fn hunk_emphasis(
    diff: &FileDiff,
    hunk: usize,
    slot: Option<&SyntaxSlot>,
    settings: &Settings,
) -> (Vec<Emphasis>, Option<HunkSyntax>) {
    let core = &diff.hunks[hunk];
    let syntax = slot
        .map(|slot| slot.get_or_init(|| file_syntax(diff, settings)))
        .filter(|s| !s.is_empty())
        .map(|s| s.hunk(&diff.old, &diff.new, core));
    let emphasis = match &syntax {
        Some(spans) => spans.emphasis(&diff.old, &diff.new, core),
        None => emphasize(&diff.old, &diff.new, core),
    };
    (emphasis, syntax)
}

/// `diff`'s sides prepared for highlighting, their large sides' spans
/// stored in the disk cache.
fn file_syntax(diff: &FileDiff, settings: &Settings) -> FileSyntax {
    let sources = print::sources(diff);
    FileSyntax::new(settings.grammars, sources, settings.cache.as_ref(), true)
}

fn run(job: Job, repo: Option<&Repo>, settings: &Settings) -> Done {
    match job {
        Job::Diff {
            file,
            version,
            change,
            piped,
        } => Done::Diff {
            file,
            version,
            result: diff(repo, settings, change, piped),
        },
        Job::Emphasis {
            file,
            version,
            hunk,
            diff,
            syntax,
        } => {
            let (emphasis, syntax) = hunk_emphasis(&diff, hunk, syntax.as_ref(), settings);
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
            needle,
            id,
            live,
        } => Done::Search {
            file,
            version,
            id,
            count: diff(repo, settings, change, piped).ok().and_then(|loaded| {
                view::find(&loaded.diff, None, &needle, || {
                    live.load(Ordering::Relaxed) != id
                })
                .map(|m| m.list.len())
            }),
        },
        Job::Find {
            file,
            version,
            diff,
            hidden,
            needle,
            id,
            live,
        } => Done::Found {
            file,
            version,
            id,
            matches: view::find(&diff, Some(&hidden), &needle, || {
                live.load(Ordering::Relaxed) != id
            }),
            diff,
            hidden,
        },
        Job::Structural {
            file,
            version,
            diff,
        } => Done::Structural {
            file,
            version,
            result: print::structural_diff(settings.grammars, &diff, settings.cache.as_ref()),
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
            spans: syntax
                .get_or_init(|| file_syntax(&diff, settings))
                .new_spans(&diff.new, lines),
            diff,
        },
        Job::Render(job) => Done::Rendered {
            file: job.file,
            version: job.version,
            id: job.id,
            result: render(&job, settings),
            diff: job.diff,
        },
    }
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
fn render(job: &RenderJob, settings: &Settings) -> Result<Box<Loaded>, Option<String>> {
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
}

//! Print mode: diffs from the repository, streamed file by file in path
//! order. Resolution happens in [`prepare`], before any output, so errors
//! never land inside a pager.
//!
//! With renames on, files stream until the first addition or deletion;
//! from there the rest of the list is held until the rename pass has paired
//! them, since any later file may pair with it.

use std::ops::{ControlFlow, Range};
use std::path::{Component, Path, PathBuf};

use dffy_core::bstr::{BStr, BString, ByteSlice};
use dffy_core::cache::Cache;
use dffy_core::diff::{Hunk, Line, LineKind, Options, Text, Unchanged, Whitespace, emphasize_with};
use dffy_core::git::{
    CommitInfo, DiffSpec, FileChange, FileDiff, Mode, Repo, Revisions, Side, Staging, Status,
};
use dffy_core::highlight::{Highlighter, Language, Spans};
use dffy_core::structural::{self, Structural, Unavailable};

use crate::config::DefaultDiff;
use crate::output::Output;
use crate::render::stat::{self, StatEntry};
use crate::render::{
    Canvas, DiffLine, FileHeader, FileStatus, HunkHeader, HunkRow, Kind, display_text, escape,
};
use crate::theme::Role;
use crate::{Ctx, Error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Summary {
    Diff,
    Stat,
    NameOnly,
    NameStatus,
}

pub(crate) enum Target<'a> {
    Diff(&'a Revisions),
    Show(&'a str),
}

pub(crate) struct Plan {
    pub repo: Repo,
    pub spec: DiffSpec,
    /// For `show`: the commit, whose header comes first.
    pub commit: Option<CommitInfo>,
    pub paths: Vec<BString>,
    pub summary: Summary,
    pub reverse: bool,
    /// The directory pathspecs are relative to.
    base: PathBuf,
}

/// Opens the repository and resolves what to diff.
pub(crate) fn prepare(
    dir: Option<&Path>,
    target: &Target,
    paths: &[PathBuf],
    summary: Summary,
    reverse: bool,
) -> Result<Plan, Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = match dir {
        Some(dir) => cwd.join(dir),
        None => cwd,
    };
    let repo = Repo::discover(&base)?;
    let (spec, commit) = match target {
        Target::Diff(revisions) => (repo.resolve_revisions(revisions)?, None),
        Target::Show(rev) => {
            let (id, spec) = repo.resolve_show(rev)?;
            (spec, Some(repo.commit_info(id)?))
        }
    };
    let paths = paths
        .iter()
        .map(|p| repo_relative(repo.workdir(), &base, p))
        .collect::<Result<_, _>>()?;
    Ok(Plan {
        repo,
        spec,
        commit,
        paths,
        summary,
        reverse,
        base,
    })
}

impl Plan {
    /// Two files or folders compared outside git; `base` is where `--focus`
    /// paths start.
    pub(crate) fn compare(repo: Repo, base: &Path, summary: Summary, reverse: bool) -> Self {
        Self {
            repo,
            spec: DiffSpec::Paths,
            commit: None,
            paths: Vec::new(),
            summary,
            reverse,
            base: base.to_owned(),
        }
    }
}

/// What a terminal shows for an empty diff. `--staged` and `--unstaged` see
/// part of what's uncommitted, so for them the rest is counted, with the
/// command that shows it: otherwise staged work would look lost.
pub(crate) fn empty_message(
    repo: &Repo,
    revisions: &Revisions,
    paths: &[BString],
    default: DefaultDiff,
) -> String {
    empty_text(revisions, default, others(repo, revisions, paths))
}

/// The uncommitted changes a `--staged` or `--unstaged` view leaves out,
/// by kind, for its empty text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Others {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
}

/// Counts every uncommitted change by kind when `revisions` shows part of
/// them; `None` for other views, or when the scan fails. Scans the whole
/// working tree.
pub(crate) fn others(repo: &Repo, revisions: &Revisions, paths: &[BString]) -> Option<Others> {
    let part = revisions.staged || revisions.unstaged;
    if !part || !revisions.revs.is_empty() || revisions.merge_base {
        return None;
    }
    let spec = repo.resolve(&[], false, false).ok()?;
    let mut others = Others::default();
    repo.for_each_change(&spec, paths, |change| {
        match (&change.status, change.staging) {
            (Status::Untracked, _) => others.untracked += 1,
            (_, Some(Staging::Staged)) => others.staged += 1,
            _ => others.unstaged += 1,
        }
        ControlFlow::Continue(())
    })
    .ok()?;
    Some(others)
}

/// [`empty_message`]'s words, from the counts [`others`] took.
pub(crate) fn empty_text(
    revisions: &Revisions,
    default: DefaultDiff,
    others: Option<Others>,
) -> String {
    let nothing = "no changes".to_owned();
    if !revisions.revs.is_empty() || revisions.merge_base {
        return nothing;
    }
    let (side, command) = match (revisions.staged, revisions.unstaged, default) {
        (true, _, _) => ("staged", "dffy"),
        (_, true, DefaultDiff::All) => ("unstaged", "dffy"),
        (_, true, DefaultDiff::Unstaged) => ("unstaged", "dffy --staged"),
        (false, false, _) => return nothing,
    };
    let Some(Others {
        staged,
        unstaged,
        untracked,
    }) = others
    else {
        return nothing;
    };
    let counts = [
        (staged, "staged"),
        (unstaged, "unstaged"),
        (untracked, "untracked"),
    ];
    let others: Vec<String> = counts
        .iter()
        .filter(|&&(n, name)| n > 0 && name != side)
        .map(|(n, name)| format!("{n} {name}"))
        .collect();
    if others.is_empty() {
        return nothing;
    }
    // With `default-diff = unstaged` no command shows untracked files.
    let tracked = if side == "staged" { unstaged } else { staged };
    let hint = if default == DefaultDiff::All || tracked > 0 {
        format!("; run {command} to see them")
    } else {
        String::new()
    };
    format!("no {side} changes ({}{hint})", others.join(", "))
}

/// Turns a pathspec given relative to `base` into a repository-relative one,
/// as git does.
fn repo_relative(workdir: Option<&Path>, base: &Path, path: &Path) -> Result<BString, Error> {
    let Some(workdir) = workdir else {
        return Ok(path.as_os_str().as_encoded_bytes().into());
    };
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_owned());
    let root = canonical(workdir);
    let mut full = canonical(base);
    for component in path.components() {
        match component {
            Component::RootDir => full = PathBuf::from("/"),
            Component::ParentDir => {
                full.pop();
            }
            Component::Normal(part) => full.push(part),
            Component::CurDir | Component::Prefix(_) => {}
        }
    }
    match full.strip_prefix(&root) {
        Ok(rel) => Ok(rel.as_os_str().as_encoded_bytes().into()),
        Err(_) => Err(Error::fatal(format!(
            "'{}' is outside the repository at {}",
            path.display(),
            root.display()
        ))),
    }
}

impl Plan {
    /// `path`, given relative to where dffy started, as a repository path.
    pub(crate) fn relative(&self, path: &Path) -> Result<BString, Error> {
        repo_relative(self.repo.workdir(), &self.base, path)
    }

    /// Writes the diff (or summary) and returns whether anything changed.
    pub(crate) fn run(&self, ctx: &Ctx, out: &mut Output) -> Result<bool, Error> {
        let theme = ctx.out_theme();
        let options = Options {
            context: ctx.config.context.hunk_lines(false),
            whitespace: ctx.config.whitespace,
        };
        let mut canvas = Canvas::default();
        if let Some(commit) = &self.commit {
            for (role, line) in commit_lines(commit) {
                canvas.line(role, &escape(&line));
            }
            canvas.end_line();
            out.write(&canvas, theme)?;
            canvas.clear();
        }
        if self.summary == Summary::Diff {
            return self.write_diffs(ctx, out, options);
        }
        let mut stats = Vec::new();
        let mut any = false;
        let mut first = true;
        let mut emit = |change: Option<FileChange>| -> Result<(), Error> {
            let Some(change) = change else {
                return Ok(());
            };
            match self.summary {
                Summary::NameOnly => {
                    canvas.line(Role::Plain, &escape(&change.path));
                    any = true;
                }
                Summary::NameStatus => {
                    name_status(&change, &mut canvas);
                    any = true;
                }
                Summary::Stat => {
                    let s = self.repo.stat(&change, options.whitespace)?;
                    let path = match &change.status {
                        Status::Renamed { from, .. } => stat::rename_name(from, &change.path),
                        _ => escape(&change.path),
                    };
                    stats.push(StatEntry {
                        path,
                        added: s.added,
                        removed: s.removed,
                        binary: s.binary,
                    });
                    any = true;
                }
                Summary::Diff => unreachable!("write_diffs writes diffs"),
            }
            if !canvas.is_empty() {
                out.write(&canvas, theme)?;
                canvas.clear();
                if first {
                    out.flush()?;
                }
                first = false;
            }
            Ok(())
        };
        self.for_each_change(ctx, &mut emit)?;
        if self.summary == Summary::Stat {
            if self.commit.is_some() {
                canvas.end_line();
            }
            let width = if ctx.term.stdout_tty {
                ctx.term.width.unwrap_or(80)
            } else {
                80
            };
            stat::stat(&stats, width, &mut canvas);
            out.write(&canvas, theme)?;
        }
        Ok(any)
    }
}

/// Bytes of file contents and rendered output held for files not yet
/// written: enough to keep every core busy on typical source files, little
/// enough that a run of generated multi-megabyte files, or a pager that
/// stops reading, doesn't grow dffy's memory with them. A file larger than
/// this goes to a thread of its own, one such file at a time, and streams
/// to the output as it's laid out once every file before it is out, while
/// the workers carry on with the files after it. On the kernel's
/// v6.0..v6.1 without highlighting, no limit at all is about 7% faster,
/// half of that from diffing its three large headers at once, and peaks
/// about 14MB higher (64MB against 50MB); 6MB to 12MB gain nothing
/// measurable.
const BUDGET: usize = 4 << 20;

/// What a file holds in the print writer's budget while it's diffed: both
/// sides, or nothing for a side too large to load. Syntax trees aren't
/// counted: weighing them made highlighted runs up to 3× slower for a peak
/// 20% lower.
fn held_bytes(sizes: [u64; 2]) -> usize {
    if sizes.iter().any(|&s| s >= dffy_core::git::MAX_DIFF_BYTES) {
        return 0;
    }
    usize::try_from(sizes[0].saturating_add(sizes[1])).unwrap_or(usize::MAX)
}

/// The print writer's accounting of the bytes it holds for files not yet
/// written: each file's two sides while a worker diffs it, then its
/// rendered output until it's written.
#[derive(Debug)]
struct Budget {
    limit: usize,
    held: usize,
}

impl Budget {
    /// Whether a file holding `bytes`, at most the limit, may start now.
    fn fits(&self, bytes: usize) -> bool {
        self.held.saturating_add(bytes) <= self.limit
    }

    fn take(&mut self, bytes: usize) {
        self.held = self.held.saturating_add(bytes);
    }

    fn give(&mut self, bytes: usize) {
        self.held = self.held.saturating_sub(bytes);
    }
}

/// A file's encoded output, in parts so a long one isn't copied whole as it
/// grows; `None` when it's left out.
type Rendered = Result<Option<Vec<Vec<u8>>>, Error>;

/// A file for a worker: its position, the change, the sizes of its sides
/// (`None` when they couldn't be read, so the diff reports why) and what
/// it holds in the budget.
type Job = (usize, FileChange, Option<[u64; 2]>, usize);

/// What the threads diffing files send the writer.
enum Done {
    /// A whole file: its position, what it held in the budget while
    /// diffed, and its output.
    File(usize, usize, Rendered),
    /// The next part of the output of the large file whose turn it is.
    Part(Vec<u8>),
    /// That large file is laid out: whether it was shown.
    End(Result<bool, Error>),
}

/// How print mode diffs, lays out and encodes each file.
struct Renderer<'a> {
    layout: crate::render::Layout,
    theme: Option<&'a crate::theme::Theme>,
    options: Options,
    highlight: bool,
    syntax_diff: bool,
    cache: Option<&'a Cache>,
    /// Threads the machine runs at once.
    cores: usize,
    /// Files whose sides are being parsed for highlighting now.
    parsing: std::sync::atomic::AtomicUsize,
}

impl Renderer<'_> {
    /// `change`'s diff. A file that can't be read has none, and a note
    /// naming the path that failed, so the files after it still print.
    fn diff(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
    ) -> Result<FileDiff, Error> {
        let diffed = match sizes {
            Some(sizes) => repo.file_diff_sized(change.clone(), sizes, &self.options),
            None => repo.file_diff(change.clone(), &self.options),
        };
        match diffed {
            Err(e @ dffy_core::git::Error::Io { .. }) => Ok(FileDiff {
                change,
                old: Text::default(),
                new: Text::default(),
                binary: false,
                too_large: false,
                note: Some(escape(e.to_string().as_bytes())),
                hunks: Vec::new(),
            }),
            other => other.map_err(Error::from),
        }
    }

    /// Diffs and lays out a file, handing its encoded output to `emit` in
    /// parts. `false` when the file is left out, before anything is emitted.
    fn file(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        let mut fd = self.diff(repo, change, sizes)?;
        if whitespace_only(&fd, self.options.whitespace) {
            return Ok(false);
        }
        std::thread::scope(|s| {
            let pending = self.highlight.then(|| {
                let change = &fd.change;
                let shown = |range: fn(&Hunk) -> Range<usize>| -> Vec<_> {
                    fd.hunks.iter().map(range).collect()
                };
                let both = [
                    (
                        change.old_path(),
                        &fd.old,
                        change.old,
                        shown(|h| h.old_start..h.old_start + h.old_len),
                    ),
                    (
                        change.path.as_bstr(),
                        &fd.new,
                        change.new,
                        shown(|h| h.new_start..h.new_start + h.new_len),
                    ),
                ];
                // Two threads for one file only while cores are free: when
                // every core has a file, splitting each one only delays the
                // file due out next.
                let others = self
                    .parsing
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let parallel = (others + 1) * 2 <= self.cores
                    && fd.old.bytes().len().min(fd.new.bytes().len()) >= PARALLEL_PARSE_BYTES;
                both.map(|(path, text, side, ranges)| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    // Each side's spans go out as soon as they're computed,
                    // so freeing its syntax tree overlaps the layout.
                    let work = move || {
                        let h = prepare_side(path, text, side, self.cache, false);
                        let spans = h.as_ref().map(|h| {
                            ranges
                                .into_iter()
                                .map(|r| h.spans(text, r))
                                .collect::<Vec<_>>()
                        });
                        tx.send(spans).ok();
                    };
                    if parallel {
                        s.spawn(work);
                    } else {
                        work();
                    }
                    rx
                })
            });
            let structural = self
                .syntax_diff
                .then(|| structural_diff(&fd, self.cache))
                .flatten();
            // A syntax diff pairs its lines itself. Pairing finds the
            // emphasis on the way, kept when no syntax tokens will replace
            // it.
            let mut plain = Vec::new();
            if !matches!(structural, Some(Ok(_))) {
                for hunk in &mut fd.hunks {
                    let emphasis = dffy_core::diff::pair(&fd.old, &fd.new, hunk);
                    if pending.is_none() {
                        plain.push(emphasis);
                    }
                }
            }
            let syntax = pending.map(|sides| {
                let [old, new] = sides.map(|rx| rx.recv().ok().flatten().unwrap_or_default());
                self.parsing
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                let (mut old, mut new) = (old.into_iter(), new.into_iter());
                fd.hunks
                    .iter()
                    .map(|_| HunkSyntax {
                        old: old.next(),
                        new: new.next(),
                    })
                    .collect::<Vec<_>>()
            });
            self.lay_out(&fd, syntax, structural, &mut plain, emit)
                .map(|()| true)
        })
    }

    /// Lays out and encodes `fd`, handing the output to `emit` in parts.
    fn lay_out(
        &self,
        fd: &FileDiff,
        syntax: Option<Vec<HunkSyntax>>,
        structural: Option<Result<Structural, Unavailable>>,
        plain: &mut [Vec<dffy_core::diff::Emphasis>],
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (mut c, mut emitted) = (Canvas::default(), Ok(()));
        let mut flush = |c: &mut Canvas| {
            if emitted.is_ok() {
                let mut part = Vec::new();
                emitted = match c.write_to(&mut part, self.theme) {
                    Ok(()) => {
                        part.shrink_to_fit();
                        emit(part)
                    }
                    Err(e) => Err(e.into()),
                };
            }
            c.clear();
        };
        file_parts(
            &self.layout,
            fd,
            syntax.as_deref(),
            structural.as_ref(),
            plain,
            &mut c,
            &mut flush,
        );
        drop((syntax, structural));
        flush(&mut c);
        emitted
    }

    /// A worker: diffs, lays out and encodes files from `queue` until it
    /// closes, sending each whole to the writer.
    fn files(
        &self,
        repo: &Repo,
        queue: &std::sync::Mutex<std::sync::mpsc::Receiver<Job>>,
        done: &std::sync::mpsc::SyncSender<Done>,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        loop {
            let job = queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .recv();
            let Ok((i, change, sizes, held)) = job else {
                break;
            };
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let mut parts = Vec::new();
            let shown = self.file(repo, change, sizes, &mut |part| {
                parts.push(part);
                Ok(())
            });
            let rendered = shown.map(|shown| shown.then_some(parts));
            if done.send(Done::File(i, held, rendered)).is_err() {
                break;
            }
        }
    }

    /// Lays out the files larger than the whole budget from `queue`, one at
    /// a time, while the workers carry on with the files after it. Each is
    /// diffed right away, but its parts go to the writer only once a token
    /// on `turns` says every file before it is out, so none are held.
    fn large_files(
        &self,
        repo: &Repo,
        queue: std::sync::mpsc::Receiver<(FileChange, Option<[u64; 2]>)>,
        turns: &std::sync::mpsc::Receiver<()>,
        done: &std::sync::mpsc::SyncSender<Done>,
        stop: &std::sync::atomic::AtomicBool,
    ) {
        let stopped = || Error::fatal("the print writer stopped");
        for (change, sizes) in queue {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                break;
            }
            let mut turn = false;
            let shown = self.file(repo, change, sizes, &mut |part| {
                if !turn {
                    turns.recv().map_err(|_| stopped())?;
                    turn = true;
                }
                done.send(Done::Part(part)).map_err(|_| stopped())
            });
            // A file left out, or failing, still waits for its turn.
            let shown = if turn {
                shown
            } else {
                turns.recv().map_err(|_| stopped()).and(shown)
            };
            if done.send(Done::End(shown)).is_err() {
                break;
            }
        }
    }
}

impl Plan {
    /// Writes each file's diff in order while worker threads diff,
    /// highlight, lay out and encode the files after it, as many as
    /// [`BUDGET`] allows. Returns whether any file was written.
    fn write_diffs(&self, ctx: &Ctx, out: &mut Output, options: Options) -> Result<bool, Error> {
        let render = Renderer {
            layout: ctx.layout(),
            theme: ctx.out_theme(),
            options,
            highlight: ctx.highlight(),
            syntax_diff: ctx.config.syntax_diff,
            cache: ctx.cache.as_ref(),
            cores: std::thread::available_parallelism().map_or(1, std::num::NonZero::get),
            parsing: std::sync::atomic::AtomicUsize::new(0),
        };
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let queue = std::sync::Mutex::new(queue);
        // Bounded, so a large file's parts stop being laid out while the
        // output is stalled, as when a pager stops reading.
        let (done, results) = std::sync::mpsc::sync_channel::<Done>(render.cores * 2);
        let (large, large_queue) = std::sync::mpsc::channel();
        let (turns, turn) = std::sync::mpsc::channel();
        // Set when output fails (the reader went away), so workers drop the
        // files still queued instead of rendering them.
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (repo, lane, render_ref, stop_ref) = (
                self.repo.without_object_cache(),
                done.clone(),
                &render,
                &stop,
            );
            scope.spawn(move || {
                render_ref.large_files(&repo, large_queue, &turn, &lane, stop_ref);
            });
            for _ in 0..render.cores {
                let (repo, done, queue, render, stop) = (
                    self.repo.without_object_cache(),
                    done.clone(),
                    &queue,
                    &render,
                    &stop,
                );
                scope.spawn(move || render.files(&repo, queue, &done, stop));
            }
            drop(done);
            let mut writer = InOrder {
                results,
                pending: std::collections::BTreeMap::new(),
                next: 0,
                any: false,
                budget: Budget {
                    limit: BUDGET,
                    held: 0,
                },
                started: false,
                turns,
                large: std::collections::VecDeque::new(),
            };
            let mut sent = 0;
            let mut dispatch = |change: Option<FileChange>| -> Result<(), Error> {
                let Some(change) = change else {
                    return writer.wait_for(sent, out);
                };
                let sizes = self.repo.sizes(&change).ok();
                let held = sizes.map_or(0, held_bytes);
                // A thread hung up only if it panicked; waiting for its
                // result reports that.
                if held > writer.budget.limit {
                    large.send((change, sizes)).ok();
                    writer.large.push_back(sent);
                    writer.give_turn();
                } else {
                    // Anything held belongs to a file not yet written, and
                    // the first of those is being diffed, so a result is on
                    // its way.
                    while !writer.budget.fits(held) {
                        writer.wait_one(out)?;
                    }
                    writer.budget.take(held);
                    jobs.send((sent, change, sizes, held)).ok();
                }
                sent += 1;
                // The first file goes out as soon as it's ready: the rest
                // of the scan may take a while.
                if sent == 1 {
                    writer.wait_for(1, out)
                } else {
                    writer.write_ready(out)
                }
            };
            let mut result = self.for_each_change(ctx, &mut dispatch);
            if result.is_ok() {
                result = dispatch(None);
            }
            drop(dispatch);
            drop((jobs, large));
            if result.is_err() {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            result.map(|()| writer.any)
        })
    }

    /// Hands each change to `emit`, oriented, with renames paired when
    /// they're on: changes stream until the first addition or deletion,
    /// and the rest wait for the rename pass. `emit(None)` comes when that
    /// wait starts and before the pass runs, so whatever is pending can go
    /// out first.
    fn for_each_change(
        &self,
        ctx: &Ctx,
        emit: &mut dyn FnMut(Option<FileChange>) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut held: Option<Vec<FileChange>> = None;
        let mut failure = None;
        self.repo
            .for_each_change(&self.spec, &self.paths, |change| {
                let change = if self.reverse {
                    change.reversed()
                } else {
                    change
                };
                let pairs = matches!(change.status, Status::Added | Status::Deleted);
                if let Some(held) = &mut held {
                    held.push(change);
                    return ControlFlow::Continue(());
                }
                let step = if ctx.config.renames && pairs {
                    held = Some(vec![change]);
                    None
                } else {
                    Some(change)
                };
                match emit(step) {
                    Ok(()) => ControlFlow::Continue(()),
                    Err(e) => {
                        failure = Some(e);
                        ControlFlow::Break(())
                    }
                }
            })?;
        if let Some(e) = failure {
            return Err(e);
        }
        if let Some(held) = held {
            emit(None)?;
            for change in self.repo.detect_renames(held, ctx.cache.as_ref())? {
                emit(Some(change))?;
            }
        }
        Ok(())
    }
}

/// Rendered files arriving in any order, written in enumeration order.
struct InOrder {
    results: std::sync::mpsc::Receiver<Done>,
    pending: std::collections::BTreeMap<usize, Rendered>,
    /// The index of the next file to write.
    next: usize,
    any: bool,
    budget: Budget,
    /// Part of the next file is out.
    started: bool,
    /// Tells the large-file thread that its next file is next to write.
    turns: std::sync::mpsc::Sender<()>,
    /// The positions of the large files not yet given their turn.
    large: std::collections::VecDeque<usize>,
}

impl InOrder {
    /// Writes files in order until the first `upto` are out, waiting for
    /// workers as needed.
    fn wait_for(&mut self, upto: usize, out: &mut Output) -> Result<(), Error> {
        while self.next < upto {
            self.wait_one(out)?;
        }
        Ok(())
    }

    /// Waits for one more file, or part of one, then writes whatever is
    /// next in order.
    fn wait_one(&mut self, out: &mut Output) -> Result<(), Error> {
        let done = self
            .results
            .recv()
            .map_err(|_| Error::fatal("a diff worker stopped unexpectedly"))?;
        self.take(done, out)
    }

    /// Writes whatever finished files are next in order, without waiting.
    fn write_ready(&mut self, out: &mut Output) -> Result<(), Error> {
        while let Ok(done) = self.results.try_recv() {
            self.take(done, out)?;
        }
        Ok(())
    }

    fn take(&mut self, done: Done, out: &mut Output) -> Result<(), Error> {
        match done {
            Done::File(i, held, rendered) => {
                self.budget.give(held);
                self.budget.take(output_len(&rendered));
                self.pending.insert(i, rendered);
            }
            Done::Part(part) => self.write_part(&part, out)?,
            Done::End(shown) => {
                self.next += 1;
                self.started = false;
                if shown? {
                    self.wrote(out)?;
                }
            }
        }
        while let Some(rendered) = self.pending.remove(&self.next) {
            self.next += 1;
            self.budget.give(output_len(&rendered));
            let Some(parts) = rendered? else { continue };
            for part in parts {
                self.write_part(&part, out)?;
            }
            self.started = false;
            self.wrote(out)?;
        }
        self.give_turn();
        Ok(())
    }

    /// Lets the large-file thread send its file once that file is next.
    fn give_turn(&mut self) {
        if self.large.front() == Some(&self.next) {
            self.large.pop_front();
            // The thread hung up only if it panicked.
            self.turns.send(()).ok();
        }
    }

    /// Writes part of the next file, after the blank line between files
    /// when it's the first part.
    fn write_part(&mut self, part: &[u8], out: &mut Output) -> Result<(), Error> {
        if !self.started && self.any {
            out.write_raw(b"\n")?;
        }
        self.started = true;
        Ok(out.write_raw(part)?)
    }

    fn wrote(&mut self, out: &mut Output) -> Result<(), Error> {
        if !self.any {
            // The first screen shouldn't wait for the buffer to fill.
            out.flush()?;
        }
        self.any = true;
        Ok(())
    }
}

fn output_len(rendered: &Rendered) -> usize {
    rendered
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map_or(0, |parts| parts.iter().map(Vec::len).sum())
}

/// A `--name-status` line: the letter (with the score for a rename), then
/// the path or paths.
fn name_status(change: &FileChange, c: &mut Canvas) {
    let (letter, role) = stat::status_letter(file_status(&change.status));
    c.push(role, letter);
    if let Status::Renamed { from, similarity } = &change.status {
        c.push(role, &format!("{similarity:03}"));
        c.push(Role::Plain, "\t");
        c.push(Role::Plain, &escape(from));
    }
    c.push(Role::Plain, "\t");
    c.line(Role::Plain, &escape(&change.path));
}

/// With `-w`/`-b`, a file whose changes are all whitespace has no hunks and
/// is left out, as git does. One with a note (a read or a filter failed)
/// stays, to show it.
pub(crate) fn whitespace_only(fd: &FileDiff, whitespace: Whitespace) -> bool {
    whitespace != Whitespace::Exact
        && fd.hunks.is_empty()
        && !fd.binary
        && !fd.too_large
        && fd.note.is_none()
        && fd.change.status == Status::Modified
        && fd.change.old.map(|s| s.mode()) == fd.change.new.map(|s| s.mode())
}

pub(crate) fn file_status(status: &Status) -> FileStatus {
    match status {
        Status::Added => FileStatus::Added,
        Status::Deleted => FileStatus::Deleted,
        Status::Modified => FileStatus::Modified,
        Status::TypeChange => FileStatus::TypeChange,
        Status::Untracked => FileStatus::Untracked,
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

/// The header of a file diff; `note` replaces its hunks.
pub(crate) fn file_header<'a>(change: &'a FileChange, note: Option<&'a str>) -> FileHeader<'a> {
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
        // The viewer words staging in its header; printed output doesn't.
        staging: None,
    }
}

/// Why a file diff has no hunks to show, if it's for a reason worth a note.
pub(crate) fn note(fd: &FileDiff) -> Option<&'static str> {
    if fd.binary {
        Some("binary files differ")
    } else if fd.too_large {
        Some("file too large to diff")
    } else {
        None
    }
}

/// A hunk's numbers as git prints them, 1-based.
pub(crate) fn hunk_header<'a>(hunk: &Hunk, heading: &'a [u8]) -> HunkHeader<'a> {
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

/// A hunk line as the renderer takes it, and whether its side of the file
/// ends without a newline right after it.
pub(crate) fn diff_line<'a>(
    fd: &'a FileDiff,
    line: &Line,
    emphasis: &'a [Range<usize>],
    syntax: Option<&'a HunkSyntax>,
) -> (DiffLine<'a>, bool) {
    let (kind, raw) = match line.kind {
        LineKind::Context => (Kind::Context, fd.new.line(line.new)),
        LineKind::Removed => (Kind::Removed, fd.old.line(line.old)),
        LineKind::Added => (Kind::Added, fd.new.line(line.new)),
    };
    let content = raw.strip_suffix(b"\n");
    // A dimmed or moved line drops its syntax colors, or they'd hide its
    // color.
    let runs = syntax
        .filter(|_| line.unchanged == Unchanged::No)
        .and_then(|s| match line.kind {
            LineKind::Removed => s.old.as_ref().map(|o| o.line(line.old)),
            LineKind::Added | LineKind::Context => s.new.as_ref().map(|n| n.line(line.new)),
        });
    let diff_line = DiffLine {
        kind,
        old: (kind != Kind::Added).then_some(line.old + 1),
        new: (kind != Kind::Removed).then_some(line.new + 1),
        text: display_text(content.unwrap_or(raw)),
        emphasis,
        syntax: runs,
        paint: &[],
        unchanged: line.unchanged,
        modified: line.paired,
        block_start: line.block_start,
    };
    (diff_line, content.is_none())
}

pub(crate) const NO_NEWLINE_LINE: DiffLine<'static> = DiffLine {
    kind: Kind::NoNewline,
    old: None,
    new: None,
    text: b"",
    emphasis: &[],
    syntax: None,
    paint: &[],
    unchanged: Unchanged::No,
    modified: false,
    block_start: false,
};

/// A file's syntax diff, through `cache` for large files: `None` for files
/// with nothing to diff (binary, too large, only a mode change).
pub(crate) fn structural_diff(
    fd: &FileDiff,
    cache: Option<&Cache>,
) -> Option<Result<Structural, Unavailable>> {
    let ids = [cache_id(fd.change.old), cache_id(fd.change.new)];
    (!fd.binary && !fd.too_large && !fd.hunks.is_empty()).then(|| {
        let path = fd.change.path.as_bytes();
        structural::diff_cached(path, &fd.old, &fd.new, &fd.hunks, ids, cache)
    })
}

/// The note a syntax diff adds under the file header, if any.
pub(crate) fn structural_note(result: &Result<Structural, Unavailable>) -> Option<String> {
    match result {
        Ok(s) => s.note(),
        Err(e) => Some(e.note().to_owned()),
    }
}

/// Sides at least this large are parsed on two threads at once. Smaller
/// ones parse in a few milliseconds, where a second thread gains little.
const PARALLEL_PARSE_BYTES: usize = 32 << 10;

/// The two sides of a file, prepared for highlighting where its language
/// is known and each side is small enough.
pub(crate) struct FileSyntax {
    old: Option<Highlighter>,
    new: Option<Highlighter>,
}

impl FileSyntax {
    /// Prepares `fd`'s sides; with `store`, large sides' spans go to
    /// `cache` (see [`Highlighter::prepare`]).
    pub(crate) fn new(fd: &FileDiff, cache: Option<&Cache>, store: bool) -> Self {
        let change = &fd.change;
        let old = || prepare_side(change.old_path(), &fd.old, change.old, cache, store);
        let new = || prepare_side(change.path.as_bstr(), &fd.new, change.new, cache, store);
        let parallel = fd.old.bytes().len().min(fd.new.bytes().len()) >= PARALLEL_PARSE_BYTES;
        let (old, new) = if parallel {
            std::thread::scope(|s| {
                let spawned = std::thread::Builder::new().spawn_scoped(s, old);
                let new = new();
                let old = match spawned {
                    Ok(spawned) => spawned
                        .join()
                        .unwrap_or_else(|e| std::panic::resume_unwind(e)),
                    // Out of threads: parse here, as `Scope::spawn` would panic.
                    Err(_) => old(),
                };
                (old, new)
            })
        } else {
            (old(), new())
        };
        Self { old, new }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.old.is_none() && self.new.is_none()
    }

    /// The spans of `lines` of the new side, if it's highlighted.
    pub(crate) fn new_spans(&self, fd: &FileDiff, lines: Range<usize>) -> Option<Spans> {
        self.new.as_ref().map(|h| h.spans(&fd.new, lines))
    }

    /// The spans of the lines `hunk` shows on each side.
    pub(crate) fn hunk(&self, fd: &FileDiff, hunk: &Hunk) -> HunkSyntax {
        HunkSyntax {
            old: self
                .old
                .as_ref()
                .map(|h| h.spans(&fd.old, hunk.old_start..hunk.old_start + hunk.old_len)),
            new: self
                .new
                .as_ref()
                .map(|h| h.spans(&fd.new, hunk.new_start..hunk.new_start + hunk.new_len)),
        }
    }
}

/// One side of a file at `path`, prepared for highlighting where its
/// language is known and it's small enough; see [`Highlighter::prepare`]
/// for `store`.
fn prepare_side(
    path: &BStr,
    text: &Text,
    side: Option<Side>,
    cache: Option<&Cache>,
    store: bool,
) -> Option<Highlighter> {
    let head = &text.bytes()[..text.bytes().len().min(256)];
    let lang = Language::detect(path, head)?;
    if text.is_empty() {
        return None;
    }
    Highlighter::prepare(lang, text, cache_id(side), cache, store)
}

/// The id a side's cached results are stored under. A working-tree side's id
/// is the one the scan hashed, and the file may have changed since, so only
/// a side read from the object database has one.
fn cache_id(side: Option<Side>) -> Option<dffy_core::git::ObjectId> {
    match side? {
        Side::Object { id, .. } => Some(id),
        Side::Worktree { .. } | Side::Path { .. } => None,
    }
}

/// Syntax spans of the lines one hunk shows, per side.
#[derive(Debug, Default, Clone)]
pub(crate) struct HunkSyntax {
    pub old: Option<Spans>,
    pub new: Option<Spans>,
}

impl HunkSyntax {
    /// The hunk's intra-line emphasis, with tokens from the spans.
    pub(crate) fn emphasis(&self, fd: &FileDiff, hunk: &Hunk) -> Vec<dffy_core::diff::Emphasis> {
        emphasize_with(&fd.old, &fd.new, hunk, self.old.as_ref(), self.new.as_ref())
    }
}

/// [`file_parts`] into one canvas.
#[cfg(test)]
pub(crate) fn file(
    layout: &crate::render::Layout,
    fd: &FileDiff,
    syntax: Option<&[HunkSyntax]>,
    structural: Option<&Result<Structural, Unavailable>>,
    c: &mut Canvas,
) {
    file_parts(layout, fd, syntax, structural, &mut [], c, &mut |_| {});
}

/// Rows laid out before [`file_parts`] hands the canvas over.
const PART_ROWS: usize = 1024;

/// Lays out a file diff: its line hunks, or with `structural` (asked for
/// with `--syntax`) the syntax diff's, falling back to the line hunks with
/// a note when there is none. `syntax` has the spans of each of `fd`'s
/// hunks, which the syntax diff's match one for one. `plain` may have each
/// of `fd`'s hunks' emphasis from [`dffy_core::diff::pair`], taken for the
/// hunks without syntax spans. Every [`PART_ROWS`] rows or so, always
/// between rows, `flush` gets the canvas, so a long file needn't be laid out
/// whole; it may clear the canvas, and whatever it leaves is drawn on.
fn file_parts(
    layout: &crate::render::Layout,
    fd: &FileDiff,
    syntax: Option<&[HunkSyntax]>,
    structural: Option<&Result<Structural, Unavailable>>,
    plain: &mut [Vec<dffy_core::diff::Emphasis>],
    c: &mut Canvas,
    flush: &mut dyn FnMut(&mut Canvas),
) {
    let structural_note = structural.and_then(structural_note);
    let header_note = note(fd)
        .or(fd.note.as_deref())
        .or(structural_note.as_deref());
    let header = file_header(&fd.change, header_note);
    layout.file_header(&header, c);
    let layout = layout.for_file(header.status, fd.old.len().max(fd.new.len()));
    let (hunks, precomputed) = match structural {
        Some(Ok(s)) => (&s.hunks, true),
        _ => (&fd.hunks, false),
    };
    let mut heading = Heading::default();
    let mut shown_to = 0;
    for (i, hunk) in hunks.iter().enumerate() {
        let mut header = hunk_header(hunk, heading.find(&fd.old, hunk.old_start));
        header.hidden = hunk.old_start.saturating_sub(shown_to);
        shown_to = hunk.old_start + hunk.old_len;
        let spans = syntax.and_then(|s| s.get(i));
        let emphasis = if precomputed {
            hunk.lines.iter().map(|l| l.emphasis.clone()).collect()
        } else {
            let old = spans.and_then(|s| s.old.as_ref());
            let new = spans.and_then(|s| s.new.as_ref());
            match plain.get_mut(i) {
                Some(emphasis) if old.is_none() && new.is_none() => std::mem::take(emphasis),
                _ => emphasize_with(&fd.old, &fd.new, hunk, old, new),
            }
        };
        // Rows are drawn from the hunk's lines as they're needed rather than
        // collected first, which for a generated file of 100,000 lines was
        // megabytes held per file. The only rows without a hunk line are
        // the no-newline markers, after a side's last line.
        let markers: Vec<usize> = hunk
            .lines
            .iter()
            .enumerate()
            .filter(|(_, line)| diff_line(fd, line, &[], None).1)
            .map(|(i, _)| i)
            .collect();
        // The hunk line shown at row `i`, or `None` for a marker.
        let line_at = |i: usize| {
            let mut line = i;
            for (k, &m) in markers.iter().enumerate() {
                match i.cmp(&(m + k + 1)) {
                    std::cmp::Ordering::Equal => return None,
                    std::cmp::Ordering::Greater => line -= 1,
                    std::cmp::Ordering::Less => {}
                }
            }
            Some(line)
        };
        let row_line = |i: usize| match line_at(i) {
            Some(l) => diff_line(fd, &hunk.lines[l], &emphasis[l], spans).0,
            None => NO_NEWLINE_LINE,
        };
        let len = hunk.lines.len() + markers.len();
        let mut draw = |row| {
            layout.hunk_row(&header, row, row_line, c);
            if c.len() >= PART_ROWS {
                flush(c);
            }
        };
        draw(HunkRow::Header);
        // Unified rows are the lines in order, with nothing to work out
        // ahead of drawing them.
        if layout.splits(&header) {
            let mut rows = Vec::new();
            layout.hunk_rows(
                &header,
                len,
                |i| {
                    let line = row_line(i);
                    (line.kind, line.block_start)
                },
                &mut rows,
            );
            rows.into_iter().for_each(draw);
        } else {
            (0..len).map(HunkRow::Line).for_each(draw);
        }
    }
    if !hunks.is_empty() && shown_to < fd.old.len() {
        layout.separator(fd.old.len() - shown_to, None, false, c);
    }
}

/// git's default hunk heading: the nearest line above the hunk that starts
/// with a letter, `_` or `$`. Each search stops where the previous one
/// started, so a file costs one backward pass in total.
#[derive(Default)]
pub(crate) struct Heading {
    searched_to: usize,
    found: Option<usize>,
}

impl Heading {
    fn find<'t>(&mut self, text: &'t Text, start: usize) -> &'t [u8] {
        self.find_line(text, start)
            .map_or(b"", |i| heading_text(text, i))
    }

    /// The index of the heading line for a hunk starting at old line `start`.
    pub(crate) fn find_line(&mut self, text: &Text, start: usize) -> Option<usize> {
        let start = start.min(text.len());
        if let Some(i) = (self.searched_to..start).rev().find(|&i| {
            text.line(i)
                .first()
                .is_some_and(|&b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
        }) {
            self.found = Some(i);
        }
        self.searched_to = self.searched_to.max(start);
        self.found
    }
}

/// Line `i` of `text` as a hunk heading.
pub(crate) fn heading_text(text: &Text, i: usize) -> &[u8] {
    let line = text.line(i).trim_end();
    &line[..line.len().min(80)]
}

/// A commit's header as `git show` prints it, a line at a time.
pub(crate) fn commit_lines(commit: &CommitInfo) -> Vec<(Role, BString)> {
    let mut lines = vec![(Role::CommitHeader, format!("commit {}", commit.id).into())];
    if commit.parents.len() > 1 {
        let short: Vec<String> = commit
            .parents
            .iter()
            .map(|p| p.to_hex_with_len(7).to_string())
            .collect();
        lines.push((Role::Dim, format!("Merge: {}", short.join(" ")).into()));
    }
    let mut author = BString::from("Author: ");
    author.extend_from_slice(&commit.author);
    lines.push((Role::Dim, author));
    lines.push((Role::Dim, format!("Date:   {}", commit.date).into()));
    lines.push((Role::Plain, BString::default()));
    for line in commit.message.trim_end().lines() {
        let mut indented = BString::from("    ");
        indented.extend_from_slice(line);
        lines.push((Role::Plain, indented));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writer's dispatch loop over a run of file sizes: a file larger
    /// than the whole budget goes to its own thread without waiting or
    /// holding anything, every other starts only when it fits, and each
    /// finished file's sides are swapped for its output until it's written.
    #[test]
    fn look_ahead_is_bounded_by_bytes_held() {
        let mut budget = Budget { limit: 10, held: 0 };
        let mut in_flight = std::collections::VecDeque::new();
        let (mut peak, mut beside_large) = (0, 0);
        for size in [3, 4, 2, 1, 25, 6, 9, 0, 0, 10, 11, 5] {
            if size > budget.limit {
                beside_large += budget.held;
                continue;
            }
            while !budget.fits(size) {
                let (sides, output) = in_flight.pop_front().expect("only held bytes block");
                budget.give(sides);
                budget.take(output);
                budget.give(output);
            }
            budget.take(size);
            in_flight.push_back((size, size * 2));
            peak = peak.max(budget.held);
        }
        assert_eq!(
            peak, 10,
            "files share the budget up to its limit, never past it"
        );
        assert!(beside_large > 0, "files keep starting beside a large one");
    }

    #[test]
    fn a_file_holds_both_sides_unless_one_is_never_loaded() {
        assert_eq!(held_bytes([3, 4]), 7);
        let large = dffy_core::git::MAX_DIFF_BYTES;
        assert_eq!(held_bytes([large, 1]), 0);
    }

    #[test]
    fn heading_is_the_nearest_unindented_line_above() {
        let text = Text::new(b"int a;\nstatic void f(void)\n{\n\tx();\n\ty();\n}\n".to_vec());
        let mut h = Heading::default();
        assert_eq!(h.find(&text, 0), b"");
        assert_eq!(h.find(&text, 3), b"static void f(void)");
        assert_eq!(h.find(&text, 5), b"static void f(void)");
    }

    /// With `default-diff = unstaged`, staged files point at `--staged`, and
    /// untracked ones, which nothing shows then, are counted without a hint.
    #[test]
    fn an_empty_side_counts_the_other_and_names_the_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(d)
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .expect("git runs");
            assert!(out.status.success(), "git {args:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(d.join("staged.txt"), "s\n").expect("write");
        git(&["add", "staged.txt"]);
        std::fs::write(d.join("untracked.txt"), "u\n").expect("write");
        let repo = Repo::discover(d).expect("repo");
        let unstaged = Revisions {
            unstaged: true,
            ..Revisions::default()
        };
        let message = |default| empty_message(&repo, &unstaged, &[], default);
        assert_eq!(
            message(DefaultDiff::All),
            "no unstaged changes (1 staged, 1 untracked; run dffy to see them)"
        );
        assert_eq!(
            message(DefaultDiff::Unstaged),
            "no unstaged changes (1 staged, 1 untracked; run dffy --staged to see them)"
        );
        git(&["commit", "-q", "-m", "staged"]);
        assert_eq!(
            message(DefaultDiff::Unstaged),
            "no unstaged changes (1 untracked)"
        );
        let all = empty_message(&repo, &Revisions::default(), &[], DefaultDiff::All);
        assert_eq!(all, "no changes", "only --staged and --unstaged count");
    }

    #[test]
    fn pathspecs_are_relative_to_the_starting_directory() {
        let root = tempfile::tempdir().expect("tempdir");
        let sub = root.path().join("src");
        std::fs::create_dir(&sub).expect("mkdir");
        let rel = |p: &str| repo_relative(Some(root.path()), &sub, Path::new(p));
        assert_eq!(rel("main.rs").expect("inside"), "src/main.rs");
        assert_eq!(rel("../README").expect("inside"), "README");
        assert_eq!(rel(".").expect("inside"), "src");
        assert_eq!(rel("..").expect("inside"), "");
        assert!(rel("../..").is_err());
    }
}

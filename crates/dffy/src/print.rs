//! Print mode: diffs from the repository, streamed file by file in path
//! order. Resolution happens in [`prepare`], before any output, so errors
//! never land inside a pager.
//!
//! With renames on, files stream until the first addition or deletion;
//! from there the rest of the list is held until the rename pass has paired
//! them, since any later file may pair with it.

use std::collections::{HashMap, HashSet};
use std::ops::{ControlFlow, Range};
use std::path::{Component, Path, PathBuf};

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::cache::Cache;
use dffy_core::diff::{self, Hunk, LineKind, Options, Text, emphasize_with};
use dffy_core::document::{
    self, File, NO_NEWLINE_LINE, Side, diff_line, file_header, hunk_header, structural_note,
};
use dffy_core::git::{CommitInfo, DiffSpec, FileChange, FileDiff, Repo, Revisions, Status};
use dffy_core::highlight::{Highlighter, HunkSyntax, Language, PARALLEL_PARSE_BYTES, Source};
use dffy_core::patch::TextKind;
use dffy_core::structural::{Structural, Unavailable};
use dffy_core::view::{
    self, Arrangement, Attributes, Heading, LineState, Row, Staging, no_newline,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config::DefaultDiff;
use crate::output::Output;
use crate::pager::text_role;
use crate::render::stat::{self, StatEntry};
use crate::render::{Canvas, escape, plain};
use crate::review::model::snapshot::{Hung, Snapshot};
use crate::review::model::text::age;
use crate::review::model::{self, At};
use crate::review::{self, prompt, who};
use crate::theme::{Ink, Role};
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
    /// The threads `--show-comments` shows on each file's diff.
    pub comments: Option<Snapshot>,
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
    Plan::resolve(repo, base, target, paths, summary, reverse)
}

impl Plan {
    /// Resolves what to diff in an open repository; `paths` are relative to
    /// `base`.
    pub(crate) fn resolve(
        repo: Repo,
        base: PathBuf,
        target: &Target,
        paths: &[PathBuf],
        summary: Summary,
        reverse: bool,
    ) -> Result<Self, Error> {
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
        Ok(Self {
            repo,
            spec,
            commit,
            paths,
            summary,
            reverse,
            comments: None,
            base,
        })
    }

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
            comments: None,
            base: base.to_owned(),
        }
    }
}

/// The prompt of the thread comment `id` is in, for `dffy comments prompt`
/// and MCP's `thread_prompt`: placed on its file's diff in what plain
/// `dffy` shows in `repo`, else its lines as written. `review_head` is the
/// head of the review session `review` is the file of; the prompt says
/// when HEAD isn't it. `None` when no comment has that id.
pub(crate) fn thread_prompt(
    repo: Option<Repo>,
    (review, review_head): (&review::Review, Option<&str>),
    id: &str,
    ctx: &Ctx,
) -> Option<prompt::Prompt> {
    let thread = review.thread(id)?;
    let elsewhere = elsewhere(repo.as_ref(), review_head);
    let (diffs, placed) = place_threads(repo, review, &[thread[0]], ctx);
    let (diff, at) = match placed.into_values().next() {
        Some((i, at)) => (Some(&diffs[i]), at),
        None => (None, unplaced(thread[0])),
    };
    Some(prompt::thread(
        &thread,
        &at,
        diff,
        prompt::Definitions::Parse,
        elsewhere,
    ))
}

/// The open threads of yours that `branches` takes, for `dffy comments
/// prompt --open`: their first comments' ids, how many of yours on other
/// branches are left out, and the prompt holding them, each placed as
/// [`thread_prompt`] places one.
pub(crate) fn open_prompt(
    repo: Option<Repo>,
    (review, review_head): (&review::Review, Option<&str>),
    branches: review::Branches,
    ctx: &Ctx,
) -> (Vec<String>, usize, prompt::Prompt) {
    let threads = review.open_threads_of_yours(branches);
    let others = match branches {
        review::Branches::All => 0,
        review::Branches::On(_) => {
            let all = review.open_threads_of_yours(review::Branches::All);
            all.len() - threads.len()
        }
    };
    let elsewhere = elsewhere(repo.as_ref(), review_head);
    let roots: Vec<&review::Comment> = threads.iter().map(|t| t[0]).collect();
    let (diffs, placed) = place_threads(repo, review, &roots, ctx);
    let ats: Vec<(At, Option<&FileDiff>)> = (roots.iter())
        .map(|root| match placed.get(&root.id) {
            Some((i, at)) => (at.clone(), Some(&diffs[*i])),
            None => (unplaced(root), None),
        })
        .collect();
    let open: Vec<prompt::Open> = (threads.iter().zip(&ats))
        .map(|(thread, (at, diff))| (thread.as_slice(), at, *diff))
        .collect();
    let ids = roots.iter().map(|r| r.id.clone()).collect();
    (ids, others, prompt::open_threads(&open, others, elsewhere))
}

/// Whether HEAD in `repo` isn't `review_head`, the head of the review
/// session whose threads a prompt is of.
fn elsewhere(repo: Option<&Repo>, review_head: Option<&str>) -> bool {
    (repo.and_then(review::head_commit))
        .zip(review_head)
        .is_some_and(|(head, review_head)| head != review_head)
}

/// Where a thread with first comment `root` is shown without a diff of
/// its file.
fn unplaced(root: &review::Comment) -> At {
    if root.path.is_empty() {
        At::Conversation {
            kind: root.kind.clone(),
            label: root.label.clone(),
        }
    } else if root.range().is_some() {
        At::Lost
    } else {
        At::File
    }
}

/// The threads of `roots`, their first comments, placed on their files'
/// diffs in what plain `dffy` shows in `repo`: those diffs, and by first
/// comment's id, the diff each thread is on and where. A thread whose
/// file that doesn't change has no entry, and neither has any when the
/// diff can't be made, as without a worktree.
fn place_threads(
    repo: Option<Repo>,
    review: &review::Review,
    roots: &[&review::Comment],
    ctx: &Ctx,
) -> (Vec<FileDiff>, HashMap<String, (usize, At)>) {
    let mut diffs = Vec::new();
    let mut placed = HashMap::new();
    let paths: Vec<PathBuf> = (roots.iter())
        .filter(|r| !r.path.is_empty())
        .flat_map(|r| std::iter::once(&r.path).chain(&r.old_path))
        .map(PathBuf::from)
        .collect();
    let Some(repo) = repo.filter(|_| !paths.is_empty()) else {
        return (diffs, placed);
    };
    let Some(workdir) = repo.workdir().map(Path::to_owned) else {
        return (diffs, placed);
    };
    let wanted: HashSet<&str> = roots.iter().map(|r| r.id.as_str()).collect();
    let place = || -> Result<(), Error> {
        let revisions = Revisions {
            unstaged: ctx.config.default_diff == DefaultDiff::Unstaged,
            ..Revisions::default()
        };
        let target = Target::Diff(&revisions);
        let plan = Plan::resolve(repo, workdir, &target, &paths, Summary::Diff, false)?;
        let mut changes = Vec::new();
        plan.for_each_change(ctx, &mut |change| {
            changes.extend(change);
            Ok(())
        })?;
        let snapshot = Snapshot::new(review, review::Branches::All);
        let options = Options {
            context: 3,
            whitespace: ctx.config.whitespace,
        };
        for change in changes.into_iter().filter(|c| snapshot.has(c, false)) {
            let diff = plan.repo.file_diff(change.clone(), &options)?;
            let mut on_it = false;
            for hung in snapshot.on(&change, &diff, false) {
                let id = &hung.thread[0].id;
                if wanted.contains(id.as_str()) && !placed.contains_key(id) {
                    placed.insert(id.clone(), (diffs.len(), hung.at));
                    on_it = true;
                }
            }
            if on_it {
                diffs.push(diff);
            }
        }
        Ok(())
    };
    if place().is_err() {
        placed.clear();
    }
    (diffs, placed)
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
    if revisions.since_checkpoint.is_some() {
        return "no changes since the checkpoint".to_owned();
    }
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
            for (kind, line) in commit_lines(commit) {
                canvas.line(text_role(kind), &escape(&line));
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
        // Written when dropped, at the end of the run.
        let stored = (self.summary == Summary::Stat)
            .then(|| ctx.cache.clone().map(dffy_core::git::StatStore::new))
            .flatten();
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
                    let s = match &stored {
                        Some(stored) => stored.stat(&self.repo, &change, options.whitespace)?,
                        None => self.repo.stat(&change, options.whitespace)?,
                    };
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

/// The per-file step of the print writer ([`Plan::stream`]): diffs a file
/// and encodes its output. The writer runs it on its threads, writes the
/// outputs in order and bounds what they hold.
pub(crate) trait FileOutput: Sync {
    /// Written between the outputs of two files.
    const SEPARATOR: &'static [u8];

    /// Diffs `change` (with its sides' sizes, when they could be read) and
    /// hands its encoded output to `emit` in parts. `false` when the file is
    /// left out, before anything is emitted.
    fn file(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<bool, Error>;

    /// `change` is queued for [`FileOutput::file`], which a worker may not
    /// take up for a while. Called on the writer's thread, in file order.
    fn queued(&self, _change: &FileChange) {}
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
    /// Languages whose query compile has been started ahead of their first
    /// file.
    prepared: std::sync::Mutex<Vec<Language>>,
    /// The threads drawn under their lines, and whether the sides are
    /// swapped (`-R`).
    comments: Option<(&'a Snapshot, bool)>,
    /// Seconds since 1970, for the threads' ages.
    now: u64,
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
}

impl FileOutput for Renderer<'_> {
    const SEPARATOR: &'static [u8] = b"\n";

    fn file(
        &self,
        repo: &Repo,
        change: FileChange,
        sizes: Option<[u64; 2]>,
        emit: &mut dyn FnMut(Vec<u8>) -> Result<(), Error>,
    ) -> Result<bool, Error> {
        // Without highlighting or the syntax diff the registry is never built.
        let prepare = || File::prepare(crate::grammars(), self.cache);
        let mut fd = self.diff(repo, change, sizes)?;
        if document::whitespace_only(&fd, self.options.whitespace) {
            return Ok(false);
        }
        let threads = match self.comments {
            Some((comments, reverse)) if comments.has(&fd.change, reverse) => {
                comments.on(&fd.change, &fd, reverse)
            }
            _ => Vec::new(),
        };
        std::thread::scope(|s| {
            let pending = self.highlight.then(|| {
                let shown = |range: fn(&Hunk) -> Range<usize>| -> Vec<_> {
                    fd.hunks.iter().map(range).collect()
                };
                // Of the sides' fields alone, as the hunks are paired while
                // the sides parse.
                let [old, new] = document::side_sources(&fd.change, &fd.old, &fd.new);
                let both = [
                    (old, shown(|h| h.old_start..h.old_start + h.old_len)),
                    (new, shown(|h| h.new_start..h.new_start + h.new_len)),
                ];
                // Two threads for one file only while cores are free: when
                // every core has a file, splitting each one only delays the
                // file due out next.
                let others = self
                    .parsing
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let parallel = (others + 1) * 2 <= self.cores
                    && fd.old.bytes().len().min(fd.new.bytes().len()) >= PARALLEL_PARSE_BYTES;
                let spans = |h: Option<&Highlighter>, side: Source, ranges: Vec<Range<usize>>| {
                    h.map(|h| {
                        ranges
                            .into_iter()
                            .map(|r| h.spans(side.text, r))
                            .collect::<Vec<_>>()
                    })
                };
                if parallel {
                    return both.map(|(side, ranges)| {
                        let (tx, rx) = std::sync::mpsc::sync_channel(1);
                        // Each side's spans go out as soon as they're
                        // computed, so freeing its syntax tree overlaps the
                        // layout.
                        s.spawn(move || {
                            let h = prepare().highlighter(side);
                            tx.send(spans(h.as_ref(), side, ranges)).ok();
                        });
                        rx
                    });
                }
                // One after the other, the new side parsed from the old
                // side's tree. A tree it can't be parsed from is freed
                // before that parse, so two whole trees are never held.
                let [(old, old_ranges), (new, new_ranges)] = both;
                let old_h = prepare().highlighter(old);
                let old_spans = spans(old_h.as_ref(), old, old_ranges);
                let seed = old_h.filter(Highlighter::can_seed);
                let from = seed.as_ref().map(|h| (h, old.text, fd.hunks.as_slice()));
                let new_h = prepare().highlighter_after(new, from);
                drop(seed);
                [old_spans, spans(new_h.as_ref(), new, new_ranges)].map(|spans| {
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    tx.send(spans).ok();
                    rx
                })
            });
            let structural = self
                .syntax_diff
                .then(|| prepare().structural(&fd))
                .flatten();
            // A syntax diff pairs its lines itself. Pairing finds the
            // emphasis on the way, kept when no syntax tokens will replace
            // it.
            let mut plain = if matches!(structural, Some(Ok(_))) {
                Vec::new()
            } else {
                document::pair(&fd.old, &fd.new, &mut fd.hunks, pending.is_none())
            };
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
            self.lay_out(&fd, syntax, structural, &mut plain, &threads, emit)
                .map(|()| true)
        })
    }

    /// Starts compiling the query of a language not seen yet, so it runs
    /// while the files queued ahead of this one are diffed instead of
    /// beside its first parse. Only while a core isn't parsing; otherwise a
    /// later file of the language tries again, and its first parse compiles
    /// it at the latest. By path only: a file known by its `#!` line alone
    /// compiles beside its parse.
    fn queued(&self, change: &FileChange) {
        if !self.highlight || self.parsing.load(std::sync::atomic::Ordering::Relaxed) >= self.cores
        {
            return;
        }
        let Some(lang) = crate::grammars().detect(&change.path, &[]) else {
            return;
        };
        let mut prepared = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if prepared.contains(lang) {
            return;
        }
        let lang = lang.clone();
        prepared.push(lang.clone());
        // Detached, as nothing joins it: the language's first parse waits on
        // the same compile. Out of threads, that parse compiles it instead.
        std::thread::Builder::new()
            .name("dffy-query".into())
            .spawn(move || lang.prepare())
            .ok();
    }
}

impl Renderer<'_> {
    /// Lays out and encodes `fd`, handing the output to `emit` in parts.
    fn lay_out(
        &self,
        fd: &FileDiff,
        syntax: Option<Vec<HunkSyntax>>,
        structural: Option<Result<Structural, Unavailable>>,
        plain: &mut [Vec<diff::Emphasis>],
        threads: &[Hung],
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
            (threads, self.now),
            (&mut c, &mut flush),
        );
        drop((syntax, structural));
        flush(&mut c);
        emitted
    }
}

/// A worker: diffs and encodes files from `queue` until it closes, sending
/// each whole to the writer.
fn files(
    output: &impl FileOutput,
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
        let shown = output.file(repo, change, sizes, &mut |part| {
            parts.push(part);
            Ok(())
        });
        let rendered = shown.map(|shown| shown.then_some(parts));
        if done.send(Done::File(i, held, rendered)).is_err() {
            break;
        }
    }
}

/// Threads the machine runs at once.
fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Diffs and encodes the files larger than the whole budget from `queue`,
/// one at a time, while the workers carry on with the files after it. Each
/// is diffed right away, but its parts go to the writer only once a token
/// on `turns` says every file before it is out, so none are held.
fn large_files(
    output: &impl FileOutput,
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
        let shown = output.file(repo, change, sizes, &mut |part| {
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

impl Plan {
    /// Writes each file's diff in order while worker threads diff,
    /// highlight, lay out and encode the files after it.
    fn write_diffs(&self, ctx: &Ctx, out: &mut Output, options: Options) -> Result<bool, Error> {
        let render = Renderer {
            layout: ctx.layout(),
            theme: ctx.out_theme(),
            options,
            highlight: ctx.highlight(),
            syntax_diff: ctx.config.syntax_diff,
            cache: ctx.cache.as_ref(),
            cores: cores(),
            parsing: std::sync::atomic::AtomicUsize::new(0),
            prepared: std::sync::Mutex::new(Vec::new()),
            comments: self.comments.as_ref().map(|c| (c, self.reverse)),
            now: model::text::now(),
        };
        self.stream(ctx, out, &render)
    }

    /// Writes each file's output from `output` in order, separated by its
    /// [`FileOutput::SEPARATOR`], while worker threads diff and encode the
    /// files after it, as many as [`BUDGET`] allows. Returns whether any
    /// file was written.
    pub(crate) fn stream<O: FileOutput>(
        &self,
        ctx: &Ctx,
        out: &mut Output,
        output: &O,
    ) -> Result<bool, Error> {
        let cores = cores();
        let (jobs, queue) = std::sync::mpsc::channel::<Job>();
        let queue = std::sync::Mutex::new(queue);
        // Bounded, so a large file's parts stop being laid out while the
        // output is stalled, as when a pager stops reading.
        let (done, results) = std::sync::mpsc::sync_channel::<Done>(cores * 2);
        let (large, large_queue) = std::sync::mpsc::channel();
        let (turns, turn) = std::sync::mpsc::channel();
        // Set when output fails (the reader went away), so workers drop the
        // files still queued instead of rendering them.
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            let (repo, lane, stop_ref) = (self.repo.without_object_cache(), done.clone(), &stop);
            scope.spawn(move || {
                large_files(output, &repo, large_queue, &turn, &lane, stop_ref);
            });
            for _ in 0..cores {
                let (repo, done, queue, stop) = (
                    self.repo.without_object_cache(),
                    done.clone(),
                    &queue,
                    &stop,
                );
                scope.spawn(move || files(output, &repo, queue, &done, stop));
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
                separator: O::SEPARATOR,
                started: false,
                turns,
                large: std::collections::VecDeque::new(),
            };
            let mut sent = 0;
            // The dispatcher borrows the writer until every change is sent.
            let result = {
                let mut dispatch = |change: Option<FileChange>| -> Result<(), Error> {
                    let Some(change) = change else {
                        return writer.wait_for(sent, out);
                    };
                    output.queued(&change);
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
                let result = self.for_each_change(ctx, &mut dispatch);
                if result.is_ok() {
                    dispatch(None)
                } else {
                    result
                }
            };
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
    pub(crate) fn for_each_change(
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
    /// Written between two files.
    separator: &'static [u8],
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

    /// Writes part of the next file, after the separator between files when
    /// it's the first part.
    fn write_part(&mut self, part: &[u8], out: &mut Output) -> Result<(), Error> {
        if !self.started && self.any && !self.separator.is_empty() {
            out.write_raw(self.separator)?;
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
    let (letter, role) = stat::status_letter(document::file_status(&change.status));
    c.push(role, letter);
    if let Status::Renamed { from, similarity } = &change.status {
        c.push(role, &format!("{similarity:03}"));
        c.push(Role::Plain, "\t");
        c.push(Role::Plain, &escape(from));
    }
    c.push(Role::Plain, "\t");
    c.line(Role::Plain, &escape(&change.path));
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
    file_parts(
        layout,
        fd,
        syntax,
        structural,
        &mut [],
        (&[], 0),
        (c, &mut |_| {}),
    );
}

/// Rows laid out before [`file_parts`] hands the canvas over.
const PART_ROWS: usize = 1024;

/// Lays out a file diff: its line hunks, or with `structural` (asked for
/// with `--syntax`) the syntax diff's, falling back to the line hunks with
/// a note when there is none. `syntax` has the spans of each of `fd`'s
/// hunks, which the syntax diff's match one for one. `plain` may have each
/// of `fd`'s hunks' emphasis from [`diff::pair`], taken for the
/// hunks without syntax spans. Every [`PART_ROWS`] rows or so, always
/// between rows, `flush` gets the canvas, so a long file needn't be laid out
/// whole; it may clear the canvas, and whatever it leaves is drawn on.
/// `threads` (placed on `fd`, with the time `now` their ages are from) are
/// drawn as boxes where [`hang`] puts them.
#[expect(
    clippy::too_many_lines,
    reason = "a file's rows in order, with its threads' boxes among them"
)]
fn file_parts(
    layout: &crate::render::Layout,
    fd: &FileDiff,
    syntax: Option<&[HunkSyntax]>,
    structural: Option<&Result<Structural, Unavailable>>,
    plain: &mut [Vec<diff::Emphasis>],
    (threads, now): (&[Hung], u64),
    (c, flush): (&mut Canvas, &mut dyn FnMut(&mut Canvas)),
) {
    let structural_note = structural.and_then(structural_note);
    let header_note = document::note(fd)
        .or(fd.note.as_deref())
        .or(structural_note.as_deref());
    let header = file_header(&fd.change, header_note);
    layout.file_header(&header, c);
    let width = layout.width.unwrap_or(BOX_WIDTH);
    let layout = layout.for_file(header.status, fd.old.len().max(fd.new.len()));
    let (hunks, precomputed) = match structural {
        Some(Ok(s)) => (&s.hunks, true),
        _ => (&fd.hunks, false),
    };
    let hangs: Vec<Hang> = threads.iter().map(|t| hang(&t.at, hunks)).collect();
    // The threads not under a row of their lines, old side's first, as the
    // viewer hangs them, each side in thread order; their lines aren't on
    // screen, so their boxes say which.
    let boxes = |at: Hang, c: &mut Canvas| {
        for side in [Side::Old, Side::New] {
            for (t, _) in (threads.iter().zip(&hangs))
                .filter(|(t, h)| **h == at && matches!(t.at, At::Line { side: s, .. } if s == side))
            {
                thread_box(t, (width, true), now, c);
            }
        }
        for (t, _) in (threads.iter().zip(&hangs))
            .filter(|(t, h)| **h == at && !matches!(t.at, At::Line { .. }))
        {
            thread_box(t, (width, true), now, c);
        }
    };
    boxes(Hang::Top, c);
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
            .filter(|(_, line)| no_newline(&fd.old, &fd.new, line))
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
            Some(l) => {
                let line = &hunk.lines[l];
                diff_line(fd, line, LineState::of(line), &emphasis[l], spans)
            }
            None => NO_NEWLINE_LINE,
        };
        let len = hunk.lines.len() + markers.len();
        // This hunk's threads on its lines, old side's first.
        let mut here: Vec<(Side, usize, &Hung)> = (threads.iter().zip(&hangs))
            .filter(|(_, h)| **h == Hang::Hunk(i))
            .filter_map(|(t, _)| match &t.at {
                At::Line { side, lines } => Some((*side, lines.end - 1, t)),
                _ => None,
            })
            .collect();
        here.sort_by_key(|&(side, ..)| side == Side::New);
        // Whether row index `i` shows line `last` of `side`.
        let shows = |i: Option<usize>, side: Side, last: usize| {
            i.and_then(line_at).is_some_and(|l| {
                let line = &hunk.lines[l];
                match side {
                    Side::Old => line.kind != LineKind::Added && line.old == last,
                    Side::New => line.kind != LineKind::Removed && line.new == last,
                }
            })
        };
        let mut draw = |row| {
            layout.hunk_row(&header, row, row_line, c);
            match row {
                Row::Header => boxes(Hang::Gap(i), c),
                Row::Line(a) | Row::Pair(Some(a), None) | Row::Pair(None, Some(a)) => {
                    for &(side, last, t) in &here {
                        if shows(Some(a), side, last) {
                            thread_box(t, (width, false), now, c);
                        }
                    }
                }
                Row::Pair(a, b) => {
                    for &(side, last, t) in &here {
                        if shows(a, side, last) || shows(b, side, last) {
                            thread_box(t, (width, false), now, c);
                        }
                    }
                }
                Row::Marker => {}
            }
            if c.len() >= PART_ROWS {
                flush(c);
            }
        };
        draw(Row::Header);
        // Unified rows are the lines in order, with nothing to work out
        // ahead of drawing them.
        if layout.arrangement(&header) == Arrangement::Split {
            let mut rows = Vec::new();
            view::rows(
                Arrangement::Split,
                len,
                |i| {
                    let line = row_line(i);
                    (line.state.kind(), line.block_start)
                },
                &mut rows,
            );
            rows.into_iter().for_each(draw);
        } else {
            (0..len).map(Row::Line).for_each(draw);
        }
    }
    if !hunks.is_empty() && shown_to < fd.old.len() {
        layout.separator(fd.old.len() - shown_to, None, false, c);
    }
    boxes(Hang::End, c);
    boxes(Hang::Lost, c);
}

/// Where printed output draws a thread placed on a file's diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hang {
    /// Under the file's header: a comment on the whole file.
    Top,
    /// Under the row of hunk `k` that shows its last line.
    Hunk(usize),
    /// Under hunk `k`'s header: its last line is in the gap above it.
    Gap(usize),
    /// After the file's last row: its last line is below the last hunk,
    /// or the file has no hunks.
    End,
    /// After those: its lines weren't found.
    Lost,
}

/// Where a thread `at` its place goes among the rows of `hunks`. A line
/// that no hunk shows hangs under the separator of the gap that hides it,
/// as a block does in the viewer.
fn hang(at: &At, hunks: &[Hunk]) -> Hang {
    let At::Line { side, lines } = at else {
        return if *at == At::File {
            Hang::Top
        } else {
            Hang::Lost
        };
    };
    let last = lines.end - 1;
    let range = |h: &Hunk| match side {
        Side::Old => h.old_start..h.old_start + h.old_len,
        Side::New => h.new_start..h.new_start + h.new_len,
    };
    match hunks.iter().position(|h| range(h).end > last) {
        Some(k) if range(&hunks[k]).start <= last => Hang::Hunk(k),
        Some(k) => Hang::Gap(k),
        None => Hang::End,
    }
}

/// Columns a thread's box takes in output without a width.
const BOX_WIDTH: usize = 80;

const AUTHOR: Ink = Ink {
    role: Role::Plain,
    syntax: None,
    attributes: Attributes::BOLD,
};

/// Writes `comments`, in [`crate::review::threads`] order, for reading
/// (`dffy comments`, and MCP's text): a heading with each thread's place,
/// and its branch when it isn't `here`, then each comment's id, author and
/// time over its text. A reply not under its thread's first comment, as on
/// a page that starts mid-thread, is headed by its thread's id. A thread
/// `placed` has, by its first comment's id, says where its lines are now.
pub(crate) fn comments(
    comments: &[&review::Comment],
    here: Option<&str>,
    placed: &[(String, At)],
    c: &mut Canvas,
) {
    let place = |side: Side, lines: &Range<usize>| {
        let (first, last) = (lines.start + 1, lines.end);
        let mut place = if first == last {
            format!(":{first}")
        } else {
            format!(":{first}-{last}")
        };
        if side == Side::Old {
            place.push_str(" (old side)");
        }
        place
    };
    let text = |c: &mut Canvas, indent: usize, text: &str| {
        for line in text.lines() {
            c.spaces(Role::Plain, indent);
            c.line(Role::Plain, &escape(line.as_bytes()));
        }
    };
    let placed: HashMap<&str, &At> = placed.iter().map(|(id, at)| (id.as_str(), at)).collect();
    let mut thread = None;
    for (i, comment) in comments.iter().enumerate() {
        if comment.in_reply_to.is_none() || comment.in_reply_to.as_deref() != thread {
            thread = Some(comment.in_reply_to.as_deref().unwrap_or(&comment.id));
            if i > 0 {
                c.end_line();
            }
            if comment.path.is_empty() {
                c.push(Role::FileHeader, "conversation");
                for word in [&comment.kind, &comment.label].into_iter().flatten() {
                    c.push(Role::Dim, &format!(", {}", escape(word.as_bytes())));
                }
            } else {
                c.push(Role::FileHeader, &escape(comment.path.as_bytes()));
            }
            match (&comment.in_reply_to, comment.range()) {
                (Some(root), _) => c.push(
                    Role::Dim,
                    &format!(", reply in thread {}", escape(root.as_bytes())),
                ),
                (None, Some((side, lines))) => {
                    let side = match side {
                        review::Side::Old => Side::Old,
                        review::Side::New => Side::New,
                    };
                    c.push(Role::FileHeader, &place(side, &lines));
                }
                (None, None) => {}
            }
            match placed.get(comment.id.as_str()) {
                Some(At::Line { side, lines }) => {
                    c.push(Role::Dim, &format!(", now {}", place(*side, lines)));
                }
                Some(At::Lost) => c.push(Role::Warn, ", lines not found"),
                _ => {}
            }
            if let Some(branch) = comment.branch.as_deref().filter(|b| Some(*b) != here) {
                c.push(
                    Role::Dim,
                    &format!(", branch {}", escape(branch.as_bytes())),
                );
            }
            if comment.resolved {
                c.push(Role::Dim, ", resolved");
            }
            c.end_line();
        }
        c.spaces(Role::Plain, 2);
        c.push(Role::Dim, &escape(comment.id.as_bytes()));
        c.push(Role::Plain, " ");
        c.push(Role::Plain, &escape(who(&comment.author).as_bytes()));
        c.push(
            Role::Dim,
            &format!(", {}", escape(comment.created.as_bytes())),
        );
        if comment.reference.is_some() {
            c.push(Role::Dim, ", posted");
        }
        if comment.pending {
            c.push(Role::Dim, ", pending");
        }
        c.end_line();
        text(c, 4, &comment.text);
        if let Some(suggestion) = &comment.suggestion {
            c.spaces(Role::Plain, 4);
            c.line(Role::Dim, "suggestion:");
            text(c, 6, suggestion);
        }
        if let Some(error) = &comment.post_error {
            c.spaces(Role::Plain, 4);
            c.line(
                Role::Warn,
                &format!("not posted: {}", escape(error.as_bytes())),
            );
        }
    }
}

/// Writes `hung`'s box, `width` columns wide: the first comment's author,
/// age and the thread's state in its top border; each comment as plain
/// text wrapped to fit, a reply under a blank row with a heading of its
/// own; a suggested change after its comment; and for a thread whose lines
/// are gone, the lines it was on, numbered as they were. With `apart`, the
/// box isn't under its lines, so its border names them.
fn thread_box(hung: &Hung, (width, apart): (usize, bool), now: u64, c: &mut Canvas) {
    let Some(root) = hung.thread.first() else {
        return;
    };
    let width = width.max(24);
    let inner = width - 4;
    let snippet = (hung.at == At::Lost)
        .then(|| model::snippet::snippet(root, None))
        .flatten();
    let mut tags: Vec<(Role, String)> = Vec::new();
    if root.resolved {
        tags.push((Role::Dim, "resolved".to_owned()));
    }
    if root.outdated {
        tags.push((Role::Warn, "outdated".to_owned()));
    }
    match (&hung.at, &snippet) {
        (At::File, _) => tags.push((Role::Dim, "on the file".to_owned())),
        (At::Lost, Some(s)) => tags.push((Role::Warn, s.heading.clone())),
        (At::Lost, None) => tags.push((Role::Warn, "lines not found".to_owned())),
        (At::Line { side, lines }, _) if apart => {
            let old = if *side == Side::Old { "old " } else { "" };
            let (first, last) = (lines.start + 1, lines.end);
            tags.push((
                Role::Dim,
                if first == last {
                    format!("{old}line {first}")
                } else {
                    format!("{old}lines {first}–{last}")
                },
            ));
        }
        _ => {}
    }
    let body = |c: &mut Canvas, spans: &[(Ink, &str)]| {
        c.push(Role::Dim, "│ ");
        let used = cut(c, spans, inner);
        c.spaces(Role::Plain, inner - used + 1);
        c.line(Role::Dim, "│");
    };
    for (i, comment) in hung.thread.iter().enumerate() {
        let author = escape(who(&comment.author).as_bytes());
        let when = format!(" · {}", escape(age(&comment.created, now).as_bytes()));
        let mut spans: Vec<(Ink, &str)> = vec![(AUTHOR, &author), (Role::Dim.into(), &when)];
        let own = comment.pending.then_some((Role::Hint, "pending"));
        let words = (i == 0).then_some(&tags).into_iter().flatten();
        for (role, word) in own.into_iter().chain(words.map(|(r, w)| (*r, w.as_str()))) {
            spans.extend([(Role::Dim.into(), " · "), (role.into(), word)]);
        }
        if i == 0 {
            c.push(Role::Dim, "┌ ");
            let used = cut(c, &spans, width - 4);
            c.push(Role::Plain, " ");
            c.push(Role::Dim, &"─".repeat(width - 4 - used));
            c.line(Role::Dim, "┐");
            if let Some(snippet) = &snippet {
                let digits = (snippet.rows.iter().map(|r| r.number).max())
                    .map_or(1, |n| n.to_string().len());
                for row in &snippet.rows {
                    let number = format!("{:>digits$} {} ", row.number, row.sign);
                    let role = if row.context { Role::Dim } else { Role::Plain };
                    let text: String = row.pieces.iter().map(|(_, text)| text.as_str()).collect();
                    let text = plain(&text);
                    body(c, &[(Role::Dim.into(), &number), (role.into(), &text)]);
                }
                body(c, &[]);
            }
        } else {
            body(c, &[]);
            body(c, &spans);
        }
        for line in model::text::plain_text(&comment.text) {
            for part in wrap(&line, inner) {
                body(c, &[(Role::Plain.into(), part)]);
            }
        }
        if let Some(suggestion) = &comment.suggestion {
            body(c, &[(Role::Dim.into(), "suggested change:")]);
            let old = (root.anchor.iter().flat_map(|a| &a.lines))
                .map(|l| (Role::Removed, "- ", plain(l)));
            let new = (suggestion.lines()).map(|l| (Role::Added, "+ ", plain(l)));
            for (role, sign, text) in old.chain(new) {
                body(c, &[(role.into(), sign), (role.into(), &text)]);
            }
        }
    }
    c.push(Role::Dim, "└");
    c.push(Role::Dim, &"─".repeat(width - 2));
    c.line(Role::Dim, "┘");
}

/// Pushes `spans`, cut to `width` columns with `…` when they're longer.
/// Returns the columns they took.
fn cut(c: &mut Canvas, spans: &[(Ink, &str)], width: usize) -> usize {
    let mut used = 0;
    let total: usize = spans.iter().map(|(_, t)| UnicodeWidthStr::width(*t)).sum();
    let room = if total > width {
        width.saturating_sub(1)
    } else {
        width
    };
    for &(ink, text) in spans {
        let mut piece = String::new();
        for ch in text.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > room {
                break;
            }
            used += w;
            piece.push(ch);
        }
        c.push_ink(ink, &piece);
    }
    if total > width && width > 0 {
        c.push(Role::Dim, "…");
        used += 1;
    }
    used
}

/// `line` in pieces of at most `width` columns, broken after a space when
/// there's one to break at.
fn wrap(line: &str, width: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while UnicodeWidthStr::width(rest) > width {
        let (mut at, mut space, mut used) = (0, None, 0);
        for (i, ch) in rest.char_indices() {
            let w = ch.width().unwrap_or(0);
            if used + w > width {
                break;
            }
            used += w;
            at = i + ch.len_utf8();
            if ch == ' ' {
                space = Some(at);
            }
        }
        let first = rest.chars().next().map_or(1, char::len_utf8);
        let at = space.unwrap_or(at).max(first);
        let (piece, tail) = rest.split_at(at);
        out.push(piece.trim_end());
        rest = tail;
    }
    out.push(rest);
    out
}

/// A commit's header as `git show` prints it, a line at a time.
pub(crate) fn commit_lines(commit: &CommitInfo) -> Vec<(TextKind, Vec<u8>)> {
    let mut lines = vec![(
        TextKind::CommitHeader,
        format!("commit {}", commit.id).into(),
    )];
    if commit.parents.len() > 1 {
        let short: Vec<String> = commit
            .parents
            .iter()
            .map(|p| p.to_hex_with_len(7).to_string())
            .collect();
        lines.push((
            TextKind::CommitMeta,
            format!("Merge: {}", short.join(" ")).into(),
        ));
    }
    let mut author = b"Author: ".to_vec();
    author.extend_from_slice(&commit.author);
    lines.push((TextKind::CommitMeta, author));
    lines.push((
        TextKind::CommitMeta,
        format!("Date:   {}", commit.date).into(),
    ));
    lines.push((TextKind::Plain, Vec::new()));
    for line in commit.message.trim_end().lines() {
        let mut indented = b"    ".to_vec();
        indented.extend_from_slice(line);
        lines.push((TextKind::CommitMessage, indented));
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

    /// A thread hangs under the row of its last line when a hunk shows
    /// it, under the header of the gap that hides it, or at the file's end
    /// below the last hunk or without hunks; a side a hunk has no lines of,
    /// as an insertion's old side, doesn't hold it.
    #[test]
    fn a_thread_hangs_where_its_last_line_shows() {
        let hunk = |old: (usize, usize), new: (usize, usize)| Hunk {
            old_start: old.0,
            old_len: old.1,
            new_start: new.0,
            new_len: new.1,
            lines: Vec::new(),
        };
        // Lines 2–4 and 20–22 of the new side shown; an insertion at old 20.
        let hunks = [hunk((2, 3), (2, 3)), hunk((20, 0), (20, 3))];
        let on = |side, lines: Range<usize>| hang(&At::Line { side, lines }, &hunks);
        assert_eq!(on(Side::New, 3..5), Hang::Hunk(0));
        assert_eq!(on(Side::New, 0..1), Hang::Gap(0));
        assert_eq!(on(Side::New, 10..11), Hang::Gap(1));
        assert_eq!(on(Side::New, 21..22), Hang::Hunk(1));
        assert_eq!(on(Side::New, 30..31), Hang::End);
        assert_eq!(on(Side::Old, 19..20), Hang::Gap(1));
        assert_eq!(
            on(Side::Old, 20..21),
            Hang::End,
            "the insertion has no old lines"
        );
        assert_eq!(
            hang(
                &At::Line {
                    side: Side::New,
                    lines: 0..1
                },
                &[]
            ),
            Hang::End
        );
        assert_eq!(hang(&At::File, &hunks), Hang::Top);
        assert_eq!(hang(&At::Lost, &hunks), Hang::Lost);
    }

    #[test]
    fn a_file_holds_both_sides_unless_one_is_never_loaded() {
        assert_eq!(held_bytes([3, 4]), 7);
        let large = dffy_core::git::MAX_DIFF_BYTES;
        assert_eq!(held_bytes([large, 1]), 0);
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
        let since = Revisions {
            since_checkpoint: Some(dffy_core::git::WhichCheckpoint::Latest),
            ..Revisions::default()
        };
        assert_eq!(
            empty_text(&since, DefaultDiff::Unstaged, None),
            "no changes since the checkpoint"
        );
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

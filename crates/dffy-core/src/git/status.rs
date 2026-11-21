//! The working-tree side: a parallel stat scan of the index that streams in
//! index order, an untracked-file walk, and the merge of both with a tree for
//! `git diff <rev>`.
//!
//! The scan compares what git's `ie_match_stat` compares with default config
//! (mtime and ctime seconds, inode, owner, size, file type and executable
//! bit). Entries whose stat data differs, and racily clean entries (mtime not
//! older than the index file), are confirmed by hashing their content as git
//! would store it (see [`super::worktree`]). The index is never written back,
//! so files touched without being changed are re-hashed on every run until
//! git refreshes the index.

use std::fs;
use std::io::Read;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::entry::Flags;

use super::worktree::{Conversion, Leading, ToGit};
use super::{Error, FileChange, Filter, Mode, ObjectId, Repo, Result, Side, Staging, Status, tree};

/// Index entries per unit of work. Workers claim chunks front to back, so the
/// start of the index finishes first and can be emitted while the rest is
/// still being scanned.
const CHUNK: usize = 512;

pub(super) fn index_worktree(
    repo: &Repo,
    index: &gix::index::State,
    filter: &Filter,
    untracked: bool,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let scan = Scan::new(repo, index)?;
    let flow = scan.run(filter, &mut |m| {
        let change = if m.unmerged {
            FileChange::unmerged
        } else {
            FileChange::new
        };
        Ok(match (m.old, m.new) {
            // An intent-to-add entry that's gone from the worktree.
            (None, None) => ControlFlow::Continue(()),
            (old, new) => emit(change(m.path.to_owned(), old, new)),
        })
    })?;
    if flow.is_break() || !untracked {
        return Ok(flow);
    }
    for (path, mode) in untracked_files(repo, index, scan.root, filter) {
        let change = FileChange {
            path,
            status: Status::Untracked,
            old: None,
            new: Some(Side::Worktree { mode, id: None }),
            staging: None,
        };
        if emit(change).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// `git diff <tree>`: the tree against the index, overlaid with the index
/// against the worktree. Staged changes are usually few, so they are collected
/// first and merged into the worktree scan's stream by path.
///
/// With `uncommitted` ([`super::DiffSpec::Uncommitted`]), untracked files
/// follow, and each change gets its [`Staging`] from which of the two diffs
/// it came out of.
pub(super) fn tree_worktree(
    repo: &Repo,
    tree: ObjectId,
    index: &gix::index::State,
    filter: &Filter,
    uncommitted: bool,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let staging = |change: FileChange, staging| FileChange {
        staging: uncommitted.then_some(staging),
        ..change
    };
    let mut staged = Vec::new();
    let _: ControlFlow<()> = tree::diff_tree_index(repo, tree, index, filter, &mut |c| {
        staged.push(staging(c, Staging::Staged));
        ControlFlow::Continue(())
    })?;
    // `git rm --cached` leaves the file behind, untracked. The staged
    // deletion already lists the path; a second row for it would read as
    // two files.
    let removed: Vec<BString> = staged
        .iter()
        .filter(|c| uncommitted && c.status == Status::Deleted)
        .map(|c| c.path.clone())
        .collect();
    let mut staged = staged.into_iter().peekable();
    let scan = Scan::new(repo, index)?;
    let mut to_git = Conversion::default();
    let flow = scan.run(filter, &mut |m| {
        while let Some(s) = staged.next_if(|s| s.path.as_bstr() < m.path) {
            if emit(s).is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        let change = match staged.next_if(|s| s.path.as_bstr() == m.path) {
            // Listed even when the file matches the tree again: the conflict
            // is still in the index. `git status` marks both columns.
            Some(s) if m.unmerged => Some(staging(
                FileChange::unmerged(m.path.to_owned(), s.old, m.new),
                Staging::Partly,
            )),
            // The worktree side wins; the index side in between is irrelevant.
            Some(s) => scan
                .overlay(m.path, s.old, m.new, &mut to_git)?
                .map(|c| staging(c, Staging::Partly)),
            None => match (m.old, m.new) {
                (None, None) => None,
                (old, new) => Some(staging(
                    FileChange::new(m.path.to_owned(), old, new),
                    Staging::Unstaged,
                )),
            },
        };
        Ok(match change {
            Some(change) => emit(change),
            None => ControlFlow::Continue(()),
        })
    })?;
    if flow.is_break() {
        return Ok(flow);
    }
    for s in staged {
        if emit(s).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    if !uncommitted {
        return Ok(ControlFlow::Continue(()));
    }
    for (path, mode) in untracked_files(repo, index, scan.root, filter) {
        if removed.binary_search(&path).is_ok() {
            continue;
        }
        let change = FileChange {
            path,
            status: Status::Untracked,
            old: None,
            new: Some(Side::Worktree { mode, id: None }),
            staging: Some(Staging::Unstaged),
        };
        if emit(change).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// [`super::DiffSpec::Checkpoint`]: `tree` against the index overlaid with
/// the worktree, as [`tree_worktree`] gives it, and the untracked files.
/// The tree holds files that were untracked when it was taken, which the
/// index doesn't: the tree-to-index diff reads one as deleted, so where the
/// file is still on disk, untracked, the two sides are compared instead and
/// its untracked row goes.
pub(super) fn checkpoint_worktree(
    repo: &Repo,
    tree: ObjectId,
    index: &gix::index::State,
    filter: &Filter,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let scan = Scan::new(repo, index)?;
    let mut untracked = untracked_files(repo, index, scan.root, filter);
    untracked.retain(|(_, mode)| *mode != Mode::Gitlink);
    let mut paired = vec![false; untracked.len()];
    let mut to_git = Conversion::default();
    let mut failed = None;
    let flow = tree_worktree(repo, tree, index, filter, false, &mut |change| {
        let on_disk = (change.status == Status::Deleted)
            .then(|| untracked.binary_search_by(|(path, _)| path.cmp(&change.path)))
            .and_then(std::result::Result::ok);
        let Some(i) = on_disk else {
            return emit(change);
        };
        paired[i] = true;
        let new = Side::Worktree {
            mode: untracked[i].1,
            id: None,
        };
        match scan.overlay(change.path.as_bstr(), change.old, Some(new), &mut to_git) {
            Ok(Some(change)) => emit(change),
            Ok(None) => ControlFlow::Continue(()),
            Err(e) => {
                failed = Some(e);
                ControlFlow::Break(())
            }
        }
    })?;
    if let Some(e) = failed {
        return Err(e);
    }
    if flow.is_break() {
        return Ok(flow);
    }
    for ((path, mode), paired) in untracked.into_iter().zip(paired) {
        if paired {
            continue;
        }
        let change = FileChange {
            path,
            status: Status::Untracked,
            old: None,
            new: Some(Side::Worktree { mode, id: None }),
            staging: None,
        };
        if emit(change).is_break() {
            return Ok(ControlFlow::Break(()));
        }
    }
    Ok(ControlFlow::Continue(()))
}

/// A tracked path whose worktree file differs from the index.
struct Modified<'a> {
    path: &'a BStr,
    /// The index side; `None` for an intent-to-add entry.
    old: Option<Side>,
    /// `None` when the file is gone from the worktree.
    new: Option<Side>,
    /// The index has conflict stages for it; `old` is one of them.
    unmerged: bool,
}

#[derive(Clone, Copy)]
enum Outcome {
    Clean,
    Deleted,
    Changed(Side),
}

/// The modified entries of one chunk, by index position.
type ChunkResult = Result<Vec<(usize, Outcome)>>;

/// What a scanning thread keeps from one entry to the next.
#[derive(Default)]
struct Scratch {
    path: Vec<u8>,
    content: Vec<u8>,
    leading: Leading,
    to_git: Conversion,
}

struct Scan<'a> {
    index: &'a gix::index::State,
    root: &'a Path,
    /// For each thread's own handle, which conversions need.
    repo: gix::ThreadSafeRepository,
    hash_kind: gix::hash::Kind,
    filemode: bool,
    trust_ctime: bool,
    /// Seconds part of the index file's mtime; entries modified in or after
    /// that second may have changed without their stat data showing it.
    racy_after: u32,
}

impl<'a> Scan<'a> {
    fn new(repo: &'a Repo, index: &'a gix::index::State) -> Result<Self> {
        let git = repo.git()?;
        let config = git.config_snapshot();
        Ok(Self {
            index,
            root: repo.workdir().ok_or(Error::NoWorktree)?,
            repo: git.clone().into_sync(),
            hash_kind: git.object_hash(),
            filemode: config.boolean("core.fileMode").unwrap_or(true),
            trust_ctime: config.boolean("core.trustCtime").unwrap_or(true),
            racy_after: u32::try_from(index.timestamp().unix_seconds()).unwrap_or(0),
        })
    }

    /// Checks every index entry matching `filter` and hands the modified ones
    /// to `emit` in index order, on the calling thread.
    fn run(
        &self,
        filter: &Filter,
        emit: &mut dyn FnMut(Modified<'a>) -> Result<ControlFlow<()>>,
    ) -> Result<ControlFlow<()>> {
        let entries = self.index.entries();
        let selected = (!filter.is_empty()).then(|| filter.select(self.index));
        let total = selected.as_ref().map_or(entries.len(), Vec::len);
        let position = |k: usize| selected.as_ref().map_or(k, |s| s[k]);
        let chunks = total.div_ceil(CHUNK);
        let threads = std::thread::available_parallelism()
            .map_or(1, std::num::NonZero::get)
            .min(chunks.max(1));

        let next_chunk = AtomicUsize::new(0);
        let stop = AtomicBool::new(false);
        let (tx, rx) = mpsc::channel::<(usize, ChunkResult)>();
        std::thread::scope(|s| {
            for _ in 0..threads {
                let tx = tx.clone();
                let (next_chunk, stop, position) = (&next_chunk, &stop, &position);
                s.spawn(move || {
                    let mut scratch = Scratch::default();
                    while !stop.load(Ordering::Relaxed) {
                        let c = next_chunk.fetch_add(1, Ordering::Relaxed);
                        if c >= chunks {
                            break;
                        }
                        let found = (c * CHUNK..total.min((c + 1) * CHUNK))
                            .map(position)
                            .filter_map(|i| match self.check(i, &mut scratch) {
                                Ok(Outcome::Clean) => None,
                                Ok(outcome) => Some(Ok((i, outcome))),
                                Err(e) => Some(Err(e)),
                            })
                            .collect();
                        if tx.send((c, found)).is_err() {
                            break;
                        }
                    }
                });
            }
            drop(tx);

            let result = self.reorder(chunks, &rx, emit);
            // Workers finish their current chunk and see the flag; dropping the
            // receiver makes their next send fail too.
            stop.store(true, Ordering::Relaxed);
            drop(rx);
            result
        })
    }

    /// Emits chunk results in chunk order as they arrive out of order.
    fn reorder(
        &self,
        chunks: usize,
        rx: &mpsc::Receiver<(usize, ChunkResult)>,
        emit: &mut dyn FnMut(Modified<'a>) -> Result<ControlFlow<()>>,
    ) -> Result<ControlFlow<()>> {
        let mut pending: Vec<Option<ChunkResult>> = (0..chunks).map(|_| None).collect();
        let mut want = 0;
        while want < chunks {
            let Ok((c, found)) = rx.recv() else {
                break;
            };
            pending[c] = Some(found);
            while let Some(found) = pending.get_mut(want).and_then(Option::take) {
                for (i, outcome) in found? {
                    if emit(self.modified(i, outcome))?.is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                want += 1;
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn modified(&self, i: usize, outcome: Outcome) -> Modified<'a> {
        let entries = self.index.entries();
        let e = &entries[i];
        let unmerged = e.stage_raw() != 0;
        let old = if unmerged {
            conflict_side(entries, i, self.index)
        } else if e.flags.contains(Flags::INTENT_TO_ADD) {
            None
        } else {
            Mode::from_index(e.mode).map(|mode| Side::Object { id: e.id, mode })
        };
        let new = match outcome {
            Outcome::Changed(side) => Some(side),
            Outcome::Clean | Outcome::Deleted => None,
        };
        Modified {
            path: e.path(self.index),
            old,
            new,
            unmerged,
        }
    }

    fn check(&self, i: usize, scratch: &mut Scratch) -> Result<Outcome> {
        let Scratch {
            path: path_buf,
            content,
            leading,
            to_git,
        } = scratch;
        let entries = self.index.entries();
        let e = &entries[i];
        let Some(index_mode) = Mode::from_index(e.mode) else {
            // A sparse-index directory entry: nothing checked out to compare.
            return Ok(Outcome::Clean);
        };
        let conflicted = e.stage_raw() != 0;
        if conflicted && i > 0 && entries[i - 1].path(self.index) == e.path(self.index) {
            return Ok(Outcome::Clean);
        }
        // git takes skip-worktree and assume-unchanged entries as unchanged.
        if index_mode == Mode::Gitlink
            || e.flags
                .intersects(Flags::SKIP_WORKTREE | Flags::ASSUME_VALID)
        {
            return Ok(Outcome::Clean);
        }

        path_buf.clear();
        path_buf.extend_from_slice(self.root.as_os_str().as_bytes());
        path_buf.push(b'/');
        path_buf.extend_from_slice(e.path(self.index));
        let full = Path::new(std::ffi::OsStr::from_bytes(path_buf));
        let meta = match fs::symlink_metadata(full) {
            Ok(meta) => meta,
            Err(err) if is_gone(&err) => return Ok(Outcome::Deleted),
            Err(source) => return Err(io_error(full, source)),
        };
        // What was found went through a symbolic link to a directory, out of
        // the working tree; git reports the path as deleted.
        if !leading.real(self.root, e.path(self.index)) {
            return Ok(Outcome::Deleted);
        }
        let file_type = meta.file_type();
        let mode = if file_type.is_symlink() {
            Mode::Symlink
        } else if !file_type.is_file() {
            // A directory or special file where a file was tracked.
            return Ok(Outcome::Deleted);
        } else if !self.filemode {
            if index_mode == Mode::Executable {
                Mode::Executable
            } else {
                Mode::File
            }
        } else if meta.mode() & 0o100 != 0 {
            Mode::Executable
        } else {
            Mode::File
        };
        if conflicted || mode != index_mode {
            return Ok(Outcome::Changed(Side::Worktree { mode, id: None }));
        }

        let s = &e.stat;
        let size = low32(meta.size());
        let stat_same = low32(meta.mtime().cast_unsigned()) == s.mtime.secs
            && (!self.trust_ctime || low32(meta.ctime().cast_unsigned()) == s.ctime.secs)
            && low32(meta.ino()) == s.ino
            && meta.uid() == s.uid
            && meta.gid() == s.gid
            && size == s.size;
        let racy = self.racy_after != 0 && s.mtime.secs >= self.racy_after;
        if stat_same && !racy {
            return Ok(Outcome::Clean);
        }
        // Git zeroes the size of racily clean entries when it writes the
        // index, so only a non-zero size proves a change without hashing.
        if s.size != 0 && size != s.size {
            return Ok(Outcome::Changed(Side::Worktree { mode, id: None }));
        }
        match self.hash(full, e.path(self.index), mode, content, to_git)? {
            None => Ok(Outcome::Deleted),
            Some(id) if id == e.id => Ok(Outcome::Clean),
            Some(id) => Ok(Outcome::Changed(Side::Worktree { mode, id: Some(id) })),
        }
    }

    /// The blob id of a worktree file, as git would store it, or symlink;
    /// `None` if it vanished.
    fn hash(
        &self,
        full: &Path,
        path: &BStr,
        mode: Mode,
        content: &mut Vec<u8>,
        to_git: &mut Conversion,
    ) -> Result<Option<ObjectId>> {
        content.clear();
        let read = if mode == Mode::Symlink {
            fs::read_link(full)
                .map(|target| content.extend_from_slice(target.as_os_str().as_bytes()))
        } else {
            fs::File::open(full)
                .and_then(|mut f| f.read_to_end(content))
                .map(drop)
        };
        match read {
            Ok(()) => {
                let build = || ToGit::new(self.repo.to_thread_local(), self.index);
                if mode != Mode::Symlink
                    && let Some(to_git) = to_git.get(build)
                {
                    // A filter that can't run leaves the bytes as read; the
                    // file's diff says so.
                    to_git.convert(path, content, Some(self.index));
                }
                Ok(Some(gix::objs::compute_hash(
                    self.hash_kind,
                    gix::objs::Kind::Blob,
                    content,
                )?))
            }
            Err(err) if is_gone(&err) => Ok(None),
            Err(source) => Err(io_error(full, source)),
        }
    }

    /// Combines the tree side of a staged change with the worktree side of the
    /// same path, dropping it when the worktree matches the tree again.
    fn overlay(
        &self,
        path: &BStr,
        old: Option<Side>,
        new: Option<Side>,
        to_git: &mut Conversion,
    ) -> Result<Option<FileChange>> {
        let (old, new) = match (old, new) {
            (None, None) => return Ok(None),
            (Some(o), Some(Side::Worktree { mode, id })) if o.mode() == mode => {
                let full = self.root.join(std::ffi::OsStr::from_bytes(path));
                let id = match id {
                    Some(id) => Some(id),
                    None => self.hash(&full, path, mode, &mut Vec::new(), to_git)?,
                };
                if id.is_some() && id == o.id() {
                    return Ok(None);
                }
                (Some(o), Some(Side::Worktree { mode, id }))
            }
            pair => pair,
        };
        Ok(Some(FileChange::new(path.to_owned(), old, new)))
    }
}

/// The stage that best represents a conflicted path's index side: ours, then
/// the base, then theirs.
fn conflict_side(
    entries: &[gix::index::Entry],
    i: usize,
    index: &gix::index::State,
) -> Option<Side> {
    let path = entries[i].path(index);
    let stages: Vec<_> = entries[i..]
        .iter()
        .take_while(|e| e.path(index) == path)
        .collect();
    [2, 1, 3]
        .into_iter()
        .find_map(|stage| stages.iter().find(|e| e.stage_raw() == stage))
        .and_then(|e| Mode::from_index(e.mode).map(|mode| Side::Object { id: e.id, mode }))
}

fn is_gone(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn io_error(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_owned(),
        source,
    }
}

/// The index stores stat fields truncated to 32 bits.
fn low32(v: u64) -> u32 {
    u32::try_from(v & u64::from(u32::MAX)).unwrap_or_default()
}

/// Files under the worktree that aren't in the index and aren't ignored by
/// `.gitignore`, `.git/info/exclude` or `core.excludesFile`, sorted by path.
/// A directory holding its own `.git` is reported once, as a nested
/// repository, rather than walked.
pub(super) fn untracked_files(
    repo: &Repo,
    index: &gix::index::State,
    root: &Path,
    filter: &Filter,
) -> Vec<(BString, Mode)> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .follow_links(false)
        .threads(std::thread::available_parallelism().map_or(1, std::num::NonZero::get));
    if let Some(excludes) = excludes_file(repo).filter(|p| p.is_file()) {
        builder.current_dir(root);
        // A malformed global excludes file degrades to fewer ignores, as in git.
        let _partial = builder.add_ignore(excludes);
    }
    let root_len = root.as_os_str().len() + 1;
    let filter = filter.clone();
    builder.filter_entry(move |e| {
        if e.file_name() == ".git" {
            return false;
        }
        if filter.is_empty() {
            return true;
        }
        let rel = e
            .path()
            .as_os_str()
            .as_bytes()
            .get(root_len..)
            .unwrap_or_default();
        let is_dir = e.file_type().is_some_and(|t| t.is_dir());
        rel.is_empty()
            || if is_dir {
                filter.may_contain(rel)
            } else {
                filter.matches(rel)
            }
    });

    let found = Mutex::new(Vec::new());
    builder.build_parallel().run(|| {
        let found = &found;
        Box::new(move |entry| {
            let Ok(entry) = entry else {
                return ignore::WalkState::Continue;
            };
            let Some(rel) = entry.path().as_os_str().as_bytes().get(root_len..) else {
                return ignore::WalkState::Continue;
            };
            let Some(file_type) = entry.file_type() else {
                return ignore::WalkState::Continue;
            };
            // Any stage: `entry_by_path` misses a conflict without ours.
            let in_index = index.entry_index_by_path(rel.as_bstr()).is_ok();
            let push = |mode| {
                if let Ok(mut found) = found.lock() {
                    found.push((BString::from(rel), mode));
                }
            };
            if file_type.is_dir() {
                if entry.depth() > 0 && entry.path().join(".git").symlink_metadata().is_ok() {
                    if !in_index {
                        push(Mode::Gitlink);
                    }
                    return ignore::WalkState::Skip;
                }
            } else if !in_index {
                if file_type.is_symlink() {
                    push(Mode::Symlink);
                } else if file_type.is_file() {
                    let executable = entry.metadata().is_ok_and(|m| m.mode() & 0o100 != 0);
                    push(if executable {
                        Mode::Executable
                    } else {
                        Mode::File
                    });
                }
            }
            ignore::WalkState::Continue
        })
    });
    let mut found = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    found
}

/// `core.excludesFile`, defaulting to git's `$XDG_CONFIG_HOME/git/ignore`.
pub(crate) fn excludes_file(repo: &Repo) -> Option<PathBuf> {
    if let Ok(Some(path)) = repo
        .git()
        .ok()?
        .config_snapshot()
        .trusted_path("core.excludesFile")
    {
        return Some(path);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".config")))
        .map(|config| config.join("git").join("ignore"))
}

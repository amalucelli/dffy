//! Git access: revision resolution, streaming change enumeration, and content
//! loading. gix provides the object database, packs, refs and rev-parse;
//! tree diffs, the index/worktree scan and the untracked walk are our own so
//! they can stream in path order and stop early.

mod changes;
mod paths;
mod rename;
mod spec;
mod stats;
mod status;
mod tree;
mod worktree;

use std::cell::RefCell;
use std::ops::ControlFlow;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use gix::bstr::{BStr, BString, ByteSlice};

pub use changes::{ChangeSet, Update};
pub use gix::ObjectId;
pub use paths::{Compare, Endpoint};
pub use spec::{DiffSpec, Revisions};
pub use stats::StatStore;
pub(crate) use status::excludes_file;

use crate::diff::{self, Hunk, Options, Text};
use crate::view::Staging;

/// Files at or above this size are not diffed and get a one-line summary,
/// like binaries. The largest file in the Linux tree is 24 MB.
pub const MAX_DIFF_BYTES: u64 = 64 << 20;

/// How much of a file is scanned for a NUL byte to call it binary (git's
/// `FIRST_FEW_BYTES`).
pub const BINARY_SNIFF_BYTES: usize = 8000;

/// Why a repository operation failed. Every message names what went wrong
/// in the words git would use, so frontends can print it as is.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// No repository at or above `path`.
    #[error("not a git repository: {}", path.display())]
    NotARepository {
        /// Where the search started.
        path: PathBuf,
        /// What gix reported.
        #[source]
        source: gix::Error,
    },
    /// A revision that doesn't resolve to an object.
    #[error("unknown revision '{rev}'")]
    UnknownRevision {
        /// As given, range syntax included.
        rev: String,
        /// The local branch, tag or remote branch whose name is closest to the
        /// unresolved part of `rev`, for a "did you mean" hint.
        suggestion: Option<String>,
        /// What gix reported.
        #[source]
        source: gix::Error,
    },
    /// Revision arguments in a shape git would reject, that would need a
    /// combined diff, or that don't go with the flags given.
    #[error("unsupported revision '{rev}': {reason}")]
    UnsupportedRevision {
        /// As given.
        rev: String,
        /// Why, in a few words.
        reason: &'static str,
    },
    /// `a...b` or `--merge-base` on commits with no common ancestor.
    #[error("no merge base between {0} and {1}")]
    NoMergeBase(ObjectId, ObjectId),
    /// A comparison with the working tree or index in a bare repository.
    #[error("this operation needs a working tree, and the repository is bare")]
    NoWorktree,
    /// A git operation on a handle from [`Repo::compare`], which has no
    /// repository behind it.
    #[error("this operation needs a git repository, and two paths are being compared")]
    NoRepository,
    /// Two paths that can't be compared as given, such as stdin against a
    /// directory; the message says why.
    #[error("{0}")]
    Uncomparable(String),
    /// A filesystem call failed on a path in the working tree, the git
    /// directory or a comparison outside git.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path the call was made on.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// Any other failure inside gix: a corrupt object, an unreadable index.
    #[error(transparent)]
    Git(#[from] gix::Error),
}

/// A result whose error defaults to [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A tree entry mode, without the tree kind (changes are only ever files).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// A regular file, `100644`.
    File,
    /// A regular file with the executable bit, `100755`.
    Executable,
    /// `120000`: the blob holds the link's target.
    Symlink,
    /// A submodule commit.
    Gitlink,
}

impl Mode {
    /// Whether `self` and `other` are the same kind of thing, so that a change
    /// between them is a modification rather than a type change. The
    /// executable bit doesn't change the kind.
    #[must_use]
    pub fn same_kind(self, other: Self) -> bool {
        let regular = |m| matches!(m, Self::File | Self::Executable);
        self == other || (regular(self) && regular(other))
    }

    fn from_tree(mode: gix::objs::tree::EntryMode) -> Option<Self> {
        use gix::objs::tree::EntryKind;
        match mode.kind() {
            EntryKind::Blob => Some(Self::File),
            EntryKind::BlobExecutable => Some(Self::Executable),
            EntryKind::Link => Some(Self::Symlink),
            EntryKind::Commit => Some(Self::Gitlink),
            EntryKind::Tree => None,
        }
    }

    fn from_index(mode: gix::index::entry::Mode) -> Option<Self> {
        use gix::index::entry::Mode as M;
        match mode {
            M::FILE => Some(Self::File),
            M::FILE_EXECUTABLE => Some(Self::Executable),
            M::SYMLINK => Some(Self::Symlink),
            M::COMMIT => Some(Self::Gitlink),
            _ => None,
        }
    }
}

/// Where one side of a change lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// A blob in the object database (or a submodule commit for gitlinks).
    Object {
        /// The blob, or the commit for a gitlink.
        id: ObjectId,
        /// Its mode in the tree or index.
        mode: Mode,
    },
    /// The file in the working tree.
    Worktree {
        /// As found on disk.
        mode: Mode,
        /// Its blob hash when the scan had to hash it; files whose size
        /// changed are reported without hashing.
        id: Option<ObjectId>,
    },
    /// A file under one of the two paths a [`DiffSpec::Paths`] compares,
    /// read from disk (or from stdin's bytes) whenever it's loaded.
    Path {
        /// As found on disk, symlinks followed; [`Mode::Symlink`] only for
        /// a link whose target is missing.
        mode: Mode,
        /// Which of the two compared paths it's under. It stays with the
        /// side when [`FileChange::reversed`] swaps them.
        root: PathRoot,
    },
}

/// One of the two paths [`Repo::compare`] was given.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PathRoot {
    /// The first path.
    Old,
    /// The second path.
    New,
}

impl Side {
    /// The mode, whichever kind of side.
    #[must_use]
    pub fn mode(&self) -> Mode {
        match *self {
            Self::Object { mode, .. } | Self::Worktree { mode, .. } | Self::Path { mode, .. } => {
                mode
            }
        }
    }

    /// The blob id, if known without reading the working tree.
    #[must_use]
    pub fn id(&self) -> Option<ObjectId> {
        match *self {
            Self::Object { id, .. } => Some(id),
            Self::Worktree { id, .. } => id,
            Self::Path { .. } => None,
        }
    }
}

/// What happened to a path, as `git diff --name-status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Status {
    /// Only on the new side.
    Added,
    /// Only on the old side.
    Deleted,
    /// On both sides as the same kind of thing; the content or the
    /// executable bit changed.
    Modified,
    /// The file changed between regular file, symlink and submodule.
    TypeChange,
    /// Not in the index and not ignored. Only reported for
    /// [`DiffSpec::Uncommitted`] and for [`DiffSpec::IndexToWorktree`] with
    /// `untracked` set.
    Untracked,
    /// Has conflict stages in the index, from a merge, rebase or
    /// cherry-pick that stopped. Reported once in every diff of the index
    /// or the working tree, whatever the sides. The new side is the
    /// working-tree file, `None` when it's gone; the old side is what the
    /// diff compares against: the tree, or for
    /// [`DiffSpec::IndexToWorktree`] the stage that stands for the index
    /// (ours, then the base, then theirs).
    Unmerged,
    /// A deletion at `from` and an addition at the change's path, paired by
    /// [`Repo::detect_renames`]. `similarity` is git's percentage: 100 for
    /// identical content.
    Renamed {
        /// The old path, repository-relative.
        from: BString,
        /// 0 to 100.
        similarity: u8,
    },
}

/// One changed path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repository-relative, `/`-separated. An untracked nested repository is
    /// reported as its directory with a [`Mode::Gitlink`] side.
    pub path: BString,
    /// Derived from the sides, except for unmerged paths and the renames
    /// rename detection pairs.
    pub status: Status,
    /// Where the old content is; `None` for additions and untracked files.
    pub old: Option<Side>,
    /// Where the new content is; `None` for deletions.
    pub new: Option<Side>,
    /// Set for [`DiffSpec::Uncommitted`] only.
    pub staging: Option<Staging>,
}

impl FileChange {
    fn new(path: BString, old: Option<Side>, new: Option<Side>) -> Self {
        let status = match (&old, &new) {
            (None, _) => Status::Added,
            (Some(_), None) => Status::Deleted,
            (Some(o), Some(n)) if o.mode().same_kind(n.mode()) => Status::Modified,
            (Some(_), Some(_)) => Status::TypeChange,
        };
        Self {
            path,
            status,
            old,
            new,
            staging: None,
        }
    }

    fn unmerged(path: BString, old: Option<Side>, new: Option<Side>) -> Self {
        Self {
            status: Status::Unmerged,
            ..Self::new(path, old, new)
        }
    }

    /// The same change seen backwards (`git diff -R`): sides swapped, so
    /// additions become deletions and renames go the other way. Untracked
    /// files read as deletions; unmerged paths stay unmerged.
    #[must_use]
    pub fn reversed(self) -> Self {
        let staging = self.staging;
        match self.status {
            Status::Renamed { from, similarity } => Self {
                path: from,
                status: Status::Renamed {
                    from: self.path,
                    similarity,
                },
                old: self.new,
                new: self.old,
                staging,
            },
            Status::Unmerged => Self {
                old: self.new,
                new: self.old,
                ..self
            },
            _ => Self {
                staging,
                ..Self::new(self.path, self.new, self.old)
            },
        }
    }

    /// Where the old side lives: the path a rename came from, otherwise the
    /// change's path.
    #[must_use]
    pub fn old_path(&self) -> &BStr {
        match &self.status {
            Status::Renamed { from, .. } => from.as_bstr(),
            _ => self.path.as_bstr(),
        }
    }
}

/// Line counts of a file diff, for `--stat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stat {
    /// Lines only in the new side.
    pub added: usize,
    /// Lines only in the old side.
    pub removed: usize,
    /// Byte sizes of both sides when either is binary or too large to diff;
    /// the line counts are zero then.
    pub binary: Option<(u64, u64)>,
}

/// A commit's header fields, as `git show` prints them.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    /// The commit itself.
    pub id: ObjectId,
    /// In the commit's order, first parent first; empty for a root commit.
    pub parents: Vec<ObjectId>,
    /// `Name <email>`.
    pub author: BString,
    /// The author date in git's default format, e.g.
    /// `Thu Sep 4 10:45:06 2022 -0400`.
    pub date: String,
    /// The raw message as stored: subject, blank line, body.
    pub message: BString,
}

/// What `HEAD` points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// A branch with commits, by its short name.
    Branch(String),
    /// A branch with no commits yet, as in a new repository.
    Unborn(String),
    /// A commit, not a branch.
    Detached(ObjectId),
}

/// A file's diff, ready to render.
#[derive(Debug, Clone)]
pub struct FileDiff {
    /// The change this diff is of.
    pub change: FileChange,
    /// Empty when either side is binary or too large.
    pub old: Text,
    /// Empty when either side is binary or too large.
    pub new: Text,
    /// A NUL byte in the first [`BINARY_SNIFF_BYTES`] of either side.
    pub binary: bool,
    /// Either side is at least [`MAX_DIFF_BYTES`].
    pub too_large: bool,
    /// Why a working-tree side is shown as read from disk rather than as
    /// git converts it: its clean filter couldn't run. The diff may then
    /// show changes that git doesn't see.
    pub note: Option<String>,
    /// Empty for binary, too-large, and mode-only changes. Lines carry no
    /// intra-line emphasis; [`diff::emphasize`] computes it per hunk.
    pub hunks: Vec<Hunk>,
}

/// A repository handle, or the two paths of a comparison outside git
/// ([`Repo::compare`]). Either way it lists changes, loads their sides and
/// pairs renames; git-only operations on paths fail with
/// [`Error::NoRepository`]. Cheap to clone; each clone can move to its own
/// thread.
#[derive(Clone)]
pub struct Repo {
    backend: Backend,
    to_git: LoadConversion,
}

/// [`Repo::load`]'s to-git conversion, built on first use and kept with the
/// handle, so a process filter such as git-lfs starts once rather than per
/// file. A clone starts without one: it's for one thread. Attributes
/// outside the working tree and the filter config are read once per
/// handle; a change to them shows in new handles.
#[derive(Default)]
struct LoadConversion(RefCell<worktree::Conversion>);

impl Clone for LoadConversion {
    fn clone(&self) -> Self {
        Self::default()
    }
}

#[derive(Clone)]
#[expect(
    clippy::large_enum_variant,
    reason = "the handle was this size before paths; boxing would add a hop to every object lookup"
)]
enum Backend {
    Git(gix::Repository),
    Paths(Arc<paths::Pair>),
}

impl Repo {
    fn git(&self) -> Result<&gix::Repository> {
        match &self.backend {
            Backend::Git(repo) => Ok(repo),
            Backend::Paths(_) => Err(Error::NoRepository),
        }
    }

    /// A clone that keeps no copy of the objects it reads. The handle from
    /// [`Repo::discover`] caches objects for tree diffs, which revisit the
    /// same trees; a thread loading many blobs once each would only fill
    /// that cache with copies of them.
    #[must_use]
    pub fn without_object_cache(&self) -> Self {
        let mut repo = self.clone();
        if let Backend::Git(git) = &mut repo.backend {
            git.object_cache_size(None);
        }
        repo
    }

    /// Paths compared outside git hash like a SHA-1 repository's blobs.
    fn object_hash(&self) -> gix::hash::Kind {
        match &self.backend {
            Backend::Git(repo) => repo.object_hash(),
            Backend::Paths(_) => gix::hash::Kind::Sha1,
        }
    }

    /// Where the file `path` of a change is on disk to open it in an
    /// editor: in the working tree, or under the second compared path (the
    /// first when the second isn't on disk). `None` for a bare repository,
    /// stdin, and a path behind a symbolic link to a directory, which
    /// leads out of the working tree.
    #[must_use]
    pub fn on_disk(&self, path: &BStr) -> Option<PathBuf> {
        match &self.backend {
            Backend::Git(_) => self.worktree_path(path).ok(),
            Backend::Paths(pair) => pair.on_disk(path),
        }
    }

    /// What to watch to keep a comparison of paths current: each compared
    /// path that's on disk. Empty for a repository; see
    /// [`Repo::watch_root`].
    #[must_use]
    pub fn compared_roots(&self) -> Vec<crate::watch::Root> {
        match &self.backend {
            Backend::Git(_) => Vec::new(),
            Backend::Paths(pair) => pair.watch_roots(),
        }
    }
    /// Opens the repository containing `path`, searching upwards like git.
    ///
    /// # Errors
    ///
    /// [`Error::NotARepository`] when no repository encloses `path`.
    pub fn discover(path: &Path) -> Result<Self> {
        let mut repo = gix::discover(path).map_err(|source| Error::NotARepository {
            path: path.to_owned(),
            source,
        })?;
        // Tree diffs revisit the same trees across specs; a small cache keeps
        // repeated lookups out of the pack.
        repo.object_cache_size_if_unset(4 << 20);
        Ok(Self {
            backend: Backend::Git(repo),
            to_git: LoadConversion::default(),
        })
    }

    /// A handle comparing two files or directories outside git, as
    /// `git diff --no-index` and `diff -r` do; diff it with
    /// [`DiffSpec::Paths`]. See [`Compare`] for how the two are paired.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when a path can't be read, or a file compared with a
    /// directory has no counterpart there; [`Error::Uncomparable`] for
    /// stdin or `/dev/null` against a directory, a path that's neither a
    /// file nor a directory, or a malformed exclude pattern.
    pub fn compare(compare: Compare) -> Result<Self> {
        Ok(Self {
            backend: Backend::Paths(Arc::new(paths::Pair::new(compare)?)),
            to_git: LoadConversion::default(),
        })
    }

    /// The working tree root, `None` for a bare repository and for paths
    /// compared outside git.
    #[must_use]
    pub fn workdir(&self) -> Option<&Path> {
        match &self.backend {
            Backend::Git(repo) => repo.workdir(),
            Backend::Paths(_) => None,
        }
    }

    /// In a linked worktree, its own git directory (under the shared one's
    /// `worktrees/`), not the shared one. `None` for paths compared outside
    /// git.
    #[must_use]
    pub fn git_dir(&self) -> Option<&Path> {
        self.git().ok().map(gix::Repository::git_dir)
    }

    /// The git directory every worktree of the repository shares; the same
    /// as [`Repo::git_dir`] outside a linked worktree. `None` for paths
    /// compared outside git.
    #[must_use]
    pub fn common_dir(&self) -> Option<&Path> {
        self.git().ok().map(gix::Repository::common_dir)
    }

    /// Streams the changed files of `spec` to `emit`, which can stop the
    /// enumeration early by returning [`ControlFlow::Break`].
    ///
    /// `paths` restricts the result to repository-relative path prefixes,
    /// matched by whole components (`fs` matches `fs/namei.c`, not
    /// `fsverity/`). Globs and other pathspec magic are not supported.
    ///
    /// Order: path order (bytewise, as git sorts). For
    /// [`DiffSpec::Uncommitted`] and [`DiffSpec::IndexToWorktree`], tracked
    /// changes come first in path order, then untracked files in path order
    /// once the walk finishes.
    ///
    /// # Errors
    ///
    /// Object database, index, and working-tree read failures.
    pub fn for_each_change(
        &self,
        spec: &DiffSpec,
        paths: &[BString],
        mut emit: impl FnMut(FileChange) -> ControlFlow<()>,
    ) -> Result<()> {
        self.changes_in(spec, &mut None, &Filter::new(paths), &mut emit)
    }

    /// [`Repo::for_each_change`] with the index kept in `index` across calls:
    /// loaded on first use and reused until the caller clears it.
    fn changes_in(
        &self,
        spec: &DiffSpec,
        index: &mut Option<gix::index::File>,
        filter: &Filter,
        emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
    ) -> Result<()> {
        if let DiffSpec::Paths = spec {
            let Backend::Paths(pair) = &self.backend else {
                return Err(Error::Uncomparable(
                    "this repository handle compares no paths".to_owned(),
                ));
            };
            let _: ControlFlow<()> = pair.changes(filter, emit)?;
            return Ok(());
        }
        if let DiffSpec::TreeToTree { old, new } = *spec {
            // Whether `emit` stopped early is the caller's own knowledge.
            let _: ControlFlow<()> = tree::diff_trees(self, old, new, filter, emit)?;
            return Ok(());
        }
        let index = match index {
            Some(index) => index,
            None => index.insert(self.load_index()?),
        };
        let _: ControlFlow<()> = match *spec {
            DiffSpec::TreeToIndex { tree } => {
                tree::diff_tree_index(self, tree, index, filter, emit)?
            }
            DiffSpec::IndexToWorktree { untracked } => {
                status::index_worktree(self, index, filter, untracked, emit)?
            }
            DiffSpec::TreeToWorktree { tree } => {
                status::tree_worktree(self, tree, index, filter, false, emit)?
            }
            DiffSpec::Uncommitted { head } => {
                status::tree_worktree(self, head, index, filter, true, emit)?
            }
            DiffSpec::TreeToTree { .. } | DiffSpec::Paths => ControlFlow::Continue(()),
        };
        Ok(())
    }

    /// What `HEAD` points at.
    ///
    /// # Errors
    ///
    /// An unreadable or malformed `HEAD`.
    pub fn head(&self) -> Result<Head> {
        let head = self.git()?.head()?;
        let name = |name: &gix::refs::FullNameRef| name.shorten().to_str_lossy().into_owned();
        Ok(match head.kind {
            gix::head::Kind::Symbolic(reference) => Head::Branch(name(reference.name.as_ref())),
            gix::head::Kind::Unborn(full) => Head::Unborn(name(full.as_ref())),
            gix::head::Kind::Detached { target, .. } => Head::Detached(target),
        })
    }

    /// The untracked, unignored files [`DiffSpec::Uncommitted`] lists, in
    /// path order: its walk of the working tree alone.
    ///
    /// # Errors
    ///
    /// [`Error::NoWorktree`] for a bare repository; an unreadable index.
    pub fn untracked(&self) -> Result<Vec<BString>> {
        let root = self.workdir().ok_or(Error::NoWorktree)?;
        let index = self.load_index()?;
        Ok(
            status::untracked_files(self, &index, root, &Filter::new(&[]))
                .into_iter()
                .map(|(path, _)| path)
                .collect(),
        )
    }

    /// What to watch to keep this repository's diffs current: the working
    /// tree and the git directories, as canonical paths so they compare
    /// equal to the paths watchers report.
    ///
    /// # Errors
    ///
    /// [`Error::NoWorktree`] for a bare repository; unreadable paths.
    pub fn watch_root(&self) -> Result<crate::watch::Root> {
        let canonical = |path: &Path| {
            path.canonicalize().map_err(|source| Error::Io {
                path: path.to_owned(),
                source,
            })
        };
        let repo = self.git()?;
        Ok(crate::watch::Root {
            worktree: canonical(self.workdir().ok_or(Error::NoWorktree)?)?,
            git_dir: canonical(repo.git_dir())?,
            common_dir: canonical(repo.common_dir())?,
            plain: None,
        })
    }

    /// Reads one side of a change: blob bytes, the worktree file's bytes as
    /// git's to-git conversion leaves them (see [`FileDiff::note`] for when
    /// it can't), or a symlink's target. Gitlinks read as git renders them,
    /// `Subproject commit <id>`.
    ///
    /// # Errors
    ///
    /// Missing objects, unreadable worktree files, and worktree files
    /// behind a symbolic link to a directory.
    pub fn load(&self, path: &BStr, side: &Side) -> Result<Vec<u8>> {
        self.load_noted(path, side).map(|(bytes, _)| bytes)
    }

    /// [`Repo::load`], with why a worktree file is as read from disk when
    /// its conversion failed.
    fn load_noted(&self, path: &BStr, side: &Side) -> Result<(Vec<u8>, Option<String>)> {
        match *side {
            Side::Object {
                id,
                mode: Mode::Gitlink,
            }
            | Side::Worktree {
                mode: Mode::Gitlink,
                id: Some(id),
            } => Ok((format!("Subproject commit {id}\n").into_bytes(), None)),
            Side::Object { id, .. } => {
                let mut data = self.git()?.find_object(id)?.detach().data;
                // Resolving a delta chain leaves room for the base and the
                // delta in the same buffer, often twice the blob's size.
                data.shrink_to_fit();
                Ok((data, None))
            }
            Side::Path { mode, root } => Ok((self.pair()?.load(path, root, mode)?, None)),
            Side::Worktree { mode, .. } => {
                let full = self.worktree_path(path)?;
                let io = |source| Error::Io {
                    path: full.clone(),
                    source,
                };
                match mode {
                    Mode::Symlink => Ok((
                        std::fs::read_link(&full)
                            .map_err(io)?
                            .into_os_string()
                            .into_vec(),
                        None,
                    )),
                    Mode::Gitlink => Ok((Vec::new(), None)),
                    Mode::File | Mode::Executable => {
                        let mut bytes = std::fs::read(&full).map_err(io)?;
                        let note = self.to_git(path, &mut bytes);
                        Ok((bytes, note))
                    }
                }
            }
        }
    }

    /// Converts a worktree file's `bytes` as git would store them, through
    /// the handle's [`LoadConversion`]; the reason when the conversion or a
    /// filter failed.
    fn to_git(&self, path: &BStr, bytes: &mut Vec<u8>) -> Option<String> {
        let mut conversion = self.to_git.0.borrow_mut();
        let Some(to_git) = conversion.get(|| {
            let repo = self.git()?.clone();
            // The index is read only if a conversion needs one of its blobs.
            let empty = gix::index::State::new(repo.object_hash());
            worktree::ToGit::new(repo, &empty)
        }) else {
            return Some("git's conversion couldn't be set up, shown as on disk".to_owned());
        };
        let reason = to_git.convert(path, bytes, None)?;
        Some(format!("filter failed, shown as on disk: {reason}"))
    }

    /// Loads both sides of `change` and diffs them.
    ///
    /// # Errors
    ///
    /// See [`Repo::load`].
    pub fn file_diff(&self, change: FileChange, options: &Options) -> Result<FileDiff> {
        let sizes = self.sizes(&change)?;
        self.file_diff_sized(change, sizes, options)
    }

    /// The byte sizes of `change`'s old and new sides, read from object
    /// headers and file metadata without loading the contents. A missing
    /// side, a gitlink, or a symlink in the working tree counts as 0.
    ///
    /// # Errors
    ///
    /// Missing objects and unreadable worktree files.
    pub fn sizes(&self, change: &FileChange) -> Result<[u64; 2]> {
        let size = |path, side: &Option<Side>| side.as_ref().map_or(Ok(0), |s| self.size(path, s));
        Ok([
            size(change.old_path(), &change.old)?,
            size(change.path.as_bstr(), &change.new)?,
        ])
    }

    /// [`Repo::file_diff`] with the sizes [`Repo::sizes`] returned for
    /// `change`, so a caller that budgets by size doesn't look them up twice.
    ///
    /// # Errors
    ///
    /// See [`Repo::load`].
    pub fn file_diff_sized(
        &self,
        change: FileChange,
        sizes: [u64; 2],
        options: &Options,
    ) -> Result<FileDiff> {
        let mut out = FileDiff {
            change,
            old: Text::default(),
            new: Text::default(),
            binary: false,
            too_large: false,
            note: None,
            hunks: Vec::new(),
        };
        let (contents, note) = self.contents(&out.change, sizes)?;
        out.note = note;
        match contents {
            Contents::TooLarge(..) => out.too_large = true,
            Contents::Binary(..) => out.binary = true,
            Contents::Text(old, new) => {
                out.hunks = diff::hunks_without_emphasis(&old, &new, options);
                out.old = old;
                out.new = new;
            }
        }
        Ok(out)
    }

    /// Counts the added and removed lines of `change` without building hunks
    /// or intra-line emphasis.
    ///
    /// # Errors
    ///
    /// See [`Repo::load`].
    pub fn stat(&self, change: &FileChange, whitespace: diff::Whitespace) -> Result<Stat> {
        Ok(match self.contents(change, self.sizes(change)?)?.0 {
            Contents::TooLarge(old, new) | Contents::Binary(old, new) => Stat {
                binary: Some((old, new)),
                ..Stat::default()
            },
            Contents::Text(old, new) => {
                let changes = diff::changes(&old, &new, whitespace);
                Stat {
                    added: changes.added.iter().filter(|&&c| c).count(),
                    removed: changes.removed.iter().filter(|&&c| c).count(),
                    binary: None,
                }
            }
        })
    }

    /// The header fields of commit `id`.
    ///
    /// # Errors
    ///
    /// When `id` is missing or not a commit.
    pub fn commit_info(&self, id: ObjectId) -> Result<CommitInfo> {
        let commit = self.git()?.find_object(id)?.peel_to_commit()?;
        let parents = commit.parent_ids().map(gix::Id::detach).collect();
        let decoded = commit.decode()?;
        let author = decoded.author()?;
        let mut name = BString::from(author.name);
        name.extend_from_slice(b" <");
        name.extend_from_slice(author.email);
        name.push(b'>');
        let date = author
            .time()
            .map(|t| t.format_or_unix(gix::date::time::format::DEFAULT))
            .unwrap_or_default();
        Ok(CommitInfo {
            id: commit.id,
            parents,
            author: name,
            date,
            message: decoded.message.to_owned(),
        })
    }

    /// Both sides of `change`, and why one is as read from disk when its
    /// filter failed.
    fn contents(
        &self,
        change: &FileChange,
        [old_size, new_size]: [u64; 2],
    ) -> Result<(Contents, Option<String>)> {
        if old_size >= MAX_DIFF_BYTES || new_size >= MAX_DIFF_BYTES {
            return Ok((Contents::TooLarge(old_size, new_size), None));
        }
        let load = |path, side: &Option<Side>| {
            side.as_ref()
                .map_or(Ok((Vec::new(), None)), |s| self.load_noted(path, s))
        };
        let ((old, old_note), (new, new_note)) = (
            load(change.old_path(), &change.old)?,
            load(change.path.as_bstr(), &change.new)?,
        );
        let note = new_note.or(old_note);
        if is_binary(&old) || is_binary(&new) {
            return Ok((Contents::Binary(old.len() as u64, new.len() as u64), note));
        }
        Ok((Contents::Text(Text::new(old), Text::new(new)), note))
    }

    fn size(&self, path: &BStr, side: &Side) -> Result<u64> {
        match *side {
            Side::Object {
                mode: Mode::Gitlink,
                ..
            }
            | Side::Worktree {
                mode: Mode::Gitlink | Mode::Symlink,
                ..
            } => Ok(0),
            Side::Object { id, .. } => Ok(self.git()?.find_header(id)?.size()),
            Side::Worktree { .. } => {
                let full = self.worktree_path(path)?;
                std::fs::symlink_metadata(&full)
                    .map(|m| m.len())
                    .map_err(|source| Error::Io { path: full, source })
            }
            Side::Path { mode, root } => self.pair()?.size(path, root, mode),
        }
    }

    fn pair(&self) -> Result<&paths::Pair> {
        match &self.backend {
            Backend::Paths(pair) => Ok(pair),
            Backend::Git(_) => Err(Error::Uncomparable(
                "a compared path's file was asked of a repository".to_owned(),
            )),
        }
    }

    /// Where the tracked `path` is in the working tree. A path behind a
    /// symbolic link to a directory isn't there, as git sees it: reading it
    /// would read a file outside the working tree.
    fn worktree_path(&self, path: &BStr) -> Result<PathBuf> {
        let root = self.workdir().ok_or(Error::NoWorktree)?;
        let full = root.join(std::ffi::OsStr::from_bytes(path));
        if !worktree::Leading::default().real(root, path) {
            return Err(Error::Io {
                path: full,
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "a directory leading to it is a symbolic link",
                ),
            });
        }
        Ok(full)
    }

    pub(crate) fn load_index(&self) -> Result<gix::index::File> {
        // The trailing checksum is skipped: verifying it costs as much as the
        // parse, and a torn index fails to decode anyway.
        let repo = self.git()?;
        Ok(gix::index::File::at_or_default(
            repo.index_path(),
            repo.object_hash(),
            true,
            gix::index::decode::Options::default(),
        )?)
    }
}

enum Contents {
    TooLarge(u64, u64),
    Binary(u64, u64),
    Text(Text, Text),
}

/// Git's binary heuristic: a NUL byte near the start.
#[must_use]
pub fn is_binary(bytes: &[u8]) -> bool {
    memchr::memchr(0, &bytes[..bytes.len().min(BINARY_SNIFF_BYTES)]).is_some()
}

/// Path-prefix filter matching whole components. Empty matches everything.
#[derive(Debug, Clone)]
struct Filter {
    prefixes: Vec<Vec<u8>>,
}

impl Filter {
    fn new(paths: &[BString]) -> Self {
        let prefixes: Vec<Vec<u8>> = paths
            .iter()
            .map(|p| p.trim_end_with(|c| c == '/').to_vec())
            .collect();
        // `.` or an empty prefix selects the whole tree.
        let everything = prefixes.iter().any(|p| p.is_empty() || p == b".");
        Self {
            prefixes: if everything { Vec::new() } else { prefixes },
        }
    }

    fn is_empty(&self) -> bool {
        self.prefixes.is_empty()
    }

    fn matches(&self, path: &[u8]) -> bool {
        self.is_empty()
            || self
                .prefixes
                .iter()
                .any(|p| path.starts_with(p) && (path.len() == p.len() || path[p.len()] == b'/'))
    }

    /// The positions of the index entries this filter matches, in index
    /// order. One binary search per prefix, so a few paths in a large index
    /// cost next to nothing; the index sorts bytewise by path.
    fn select(&self, index: &gix::index::State) -> Vec<usize> {
        let entries = index.entries();
        let path = |i: usize| entries[i].path(index).as_bytes();
        let lower = |key: &[u8]| entries.partition_point(|e| e.path(index).as_bytes() < key);
        let mut selected = Vec::new();
        let mut dir = Vec::new();
        for prefix in &self.prefixes {
            let exact = lower(prefix);
            selected.extend((exact..entries.len()).take_while(|&i| path(i) == prefix.as_slice()));
            // Entries under `prefix/` sort after siblings like `prefix.c`.
            dir.clear();
            dir.extend_from_slice(prefix);
            dir.push(b'/');
            let under = lower(&dir);
            selected.extend((under..entries.len()).take_while(|&i| path(i).starts_with(&dir)));
        }
        selected.sort_unstable();
        selected.dedup();
        selected
    }

    /// Whether anything under directory `dir` (no trailing slash) can match.
    fn may_contain(&self, dir: &[u8]) -> bool {
        self.matches(dir)
            || self
                .prefixes
                .iter()
                .any(|p| p.starts_with(dir) && p.get(dir.len()) == Some(&b'/'))
    }
}

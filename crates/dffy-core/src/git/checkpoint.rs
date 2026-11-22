//! Checkpoints: the working tree as plain `dffy` lists it (staged and
//! unstaged changes, untracked files git doesn't ignore), kept as a commit
//! so a later diff can show only what changed since. Taking one writes
//! objects and a ref and nothing else: the index, `HEAD` and the working
//! tree stay as they were.
//!
//! The refs are `refs/worktree/dffy/checkpoints/<n>`, numbered from 1 per
//! worktree. Git keeps `refs/worktree/` apart for each worktree and no
//! default refspec fetches or pushes it. Each commit's parent is `HEAD`
//! when it was taken, so `git diff <ref>^ <ref>` shows what was uncommitted
//! then.

use std::ops::ControlFlow;

use gix::bstr::{BString, ByteSlice};
use gix::objs::tree::EntryKind;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};

use super::{
    Error, FileChange, Filter, MAX_DIFF_BYTES, Mode, ObjectId, Repo, Result, Side, status,
};

const PREFIX: &str = "refs/worktree/dffy/checkpoints/";

/// How many checkpoints a worktree keeps: taking one removes those older
/// than the last this many.
pub const KEPT_CHECKPOINTS: usize = 5;

/// A checkpoint taken by [`Repo::take_checkpoint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Checkpoint {
    /// Its number in the worktree, one more than the last one's.
    pub number: u64,
    /// The commit its ref points at.
    pub commit: ObjectId,
    /// The working tree's files as they were.
    pub tree: ObjectId,
    /// When it was taken, in seconds since 1970.
    pub time: i64,
}

/// Which checkpoint a diff starts from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhichCheckpoint {
    /// The last one taken, whichever that is when the diff is listed.
    Latest,
    /// The one with this [`Checkpoint::number`].
    Number(u64),
}

impl Repo {
    /// Writes the working tree as [`super::DiffSpec::Uncommitted`] lists it
    /// into the object database and points a new checkpoint ref at it, then
    /// removes all but the last [`KEPT_CHECKPOINTS`]. Untracked nested
    /// repositories are left out, and so are working-tree files of
    /// [`MAX_DIFF_BYTES`] or more: a tracked one keeps `HEAD`'s entry, so it
    /// lists as changed since while it differs from `HEAD`. Reads the whole
    /// working tree's status, so
    /// a frontend calls it off its main thread.
    ///
    /// # Errors
    ///
    /// [`Error::NoWorktree`] for a bare repository; unreadable files, the
    /// index or objects; a failed object or ref write, as when another
    /// process took a checkpoint with the same number at the same moment.
    pub fn take_checkpoint(&self) -> Result<Checkpoint> {
        let git = self.git()?;
        let root = self.workdir().ok_or(Error::NoWorktree)?;
        let head = self.head_tree()?;
        let index = self.load_index()?;
        let everything = Filter::new(&[]);
        let mut changes: Vec<FileChange> = Vec::new();
        let _: ControlFlow<()> =
            status::tree_worktree(self, head, &index, &everything, false, &mut |c| {
                changes.push(c);
                ControlFlow::Continue(())
            })?;
        // After the tracked changes: a file `git rm --cached` left behind is
        // a staged deletion there and untracked here, and it's on disk.
        let untracked = status::untracked_files(self, &index, root, &everything);
        let untracked = untracked.into_iter().map(|(path, mode)| {
            let side = Side::Worktree { mode, id: None };
            (path, Some(side))
        });
        let mut editor = git.edit_tree(head)?;
        for (path, side) in changes
            .into_iter()
            .map(|c| (c.path, c.new))
            .chain(untracked)
        {
            // Not copied into the object database on every checkpoint: the
            // tree keeps HEAD's entry, or none for an untracked file.
            // A file it can't size, as one removed meanwhile, is `stored`'s.
            if let Some(side @ Side::Worktree { .. }) = &side
                && self
                    .size(path.as_bstr(), side)
                    .is_ok_and(|size| size >= MAX_DIFF_BYTES)
            {
                continue;
            }
            let Some((kind, id)) = self.stored(&path, side)? else {
                editor.remove(&path)?;
                continue;
            };
            editor.upsert(&path, kind, id)?;
        }
        let tree = editor.write()?.detach();

        let number = self.checkpoints()?.last().map_or(1, |c| c.number + 1);
        let time = gix::date::Time::now_local_or_utc();
        let signature = gix::actor::Signature {
            name: "dffy".into(),
            email: "dffy".into(),
            time,
        };
        let commit = gix::objs::Commit {
            tree,
            parents: git
                .head_id()
                .ok()
                .map(gix::Id::detach)
                .into_iter()
                .collect(),
            author: signature.clone(),
            committer: signature.clone(),
            encoding: None,
            message: format!("dffy checkpoint {number}\n").into(),
            extra_headers: Vec::new(),
        };
        let commit = git.write_object(&commit)?.detach();
        let name = format!("{PREFIX}{number}")
            .try_into()
            .map_err(gix::Error::from_error)?;
        let create = RefEdit::update(name, commit, PreviousValue::MustNotExist, "dffy checkpoint");
        // As dffy, so a repository with no identity configured takes one too.
        let mut buf = gix::date::parse::TimeBuf::default();
        let committer = signature.to_ref(&mut buf);
        git.edit_references_as(Some(create), Some(committer))?;
        let checkpoints = self.checkpoints()?;
        let old = checkpoints.len().saturating_sub(KEPT_CHECKPOINTS);
        let prune = checkpoints[..old].iter().map(|c| {
            Ok(RefEdit {
                change: Change::Delete {
                    expected: PreviousValue::Any,
                    log: RefLog::AndReference,
                },
                name: format!("{PREFIX}{}", c.number)
                    .try_into()
                    .map_err(gix::Error::from_error)?,
                deref: false,
            })
        });
        git.edit_references_as(prune.collect::<Result<Vec<_>>>()?, Some(committer))?;
        Ok(Checkpoint {
            number,
            commit,
            tree,
            time: time.seconds,
        })
    }

    /// The entry a working-tree file gets in a checkpoint's tree, its blob
    /// written; `None` when there's none: it's gone, or it's a nested
    /// repository, which has no commit to record.
    fn stored(&self, path: &BString, side: Option<Side>) -> Result<Option<(EntryKind, ObjectId)>> {
        let Some(side) = side else {
            return Ok(None);
        };
        let kind = match side.mode() {
            Mode::File => EntryKind::Blob,
            Mode::Executable => EntryKind::BlobExecutable,
            Mode::Symlink => EntryKind::Link,
            Mode::Gitlink => EntryKind::Commit,
        };
        let id = match side {
            Side::Object { id, .. } => id,
            Side::Worktree {
                mode: Mode::Gitlink,
                id: None,
            } => return Ok(None),
            _ => match self.load(path.as_bstr(), &side) {
                Ok(bytes) => self.git()?.write_blob(bytes)?.detach(),
                // Removed while the checkpoint was being taken.
                Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(e) => return Err(e),
            },
        };
        Ok(Some((kind, id)))
    }

    /// This worktree's checkpoints, oldest first.
    ///
    /// # Errors
    ///
    /// Unreadable refs, or a checkpoint ref that doesn't point at a commit.
    pub fn checkpoints(&self) -> Result<Vec<Checkpoint>> {
        let git = self.git()?;
        let mut found: Vec<(u64, ObjectId)> = Vec::new();
        for reference in git.references()?.prefixed(PREFIX)? {
            let reference = reference?;
            let number = (reference.name().as_bstr().strip_prefix(PREFIX.as_bytes()))
                .and_then(|n| n.to_str().ok()?.parse().ok());
            if let (Some(number), Some(id)) = (number, reference.target().try_id()) {
                found.push((number, id.to_owned()));
            }
        }
        found.sort_unstable();
        found
            .into_iter()
            .map(|(number, commit)| {
                let commit = git.find_object(commit)?.peel_to_commit()?;
                Ok(Checkpoint {
                    number,
                    commit: commit.id,
                    tree: commit.tree_id()?.detach(),
                    time: commit.time()?.seconds,
                })
            })
            .collect()
    }

    /// The checkpoint `which` names.
    ///
    /// # Errors
    ///
    /// [`Error::NoCheckpoint`] when there's no such checkpoint; as
    /// [`Repo::checkpoints`].
    pub fn checkpoint(&self, which: WhichCheckpoint) -> Result<Checkpoint> {
        let checkpoints = self.checkpoints()?;
        let found = match which {
            WhichCheckpoint::Latest => checkpoints.last(),
            WhichCheckpoint::Number(n) => checkpoints.iter().find(|c| c.number == n),
        };
        found.copied().ok_or(Error::NoCheckpoint(match which {
            WhichCheckpoint::Latest => None,
            WhichCheckpoint::Number(n) => Some(n),
        }))
    }
}

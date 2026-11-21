//! Turning `git diff`-style arguments into the two sides to compare.

use gix::bstr::{BStr, ByteSlice};
use gix::revision::plumbing::Spec;

use super::{Error, ObjectId, Repo, Result, WhichCheckpoint};

/// The two sides of a diff, with revisions resolved to trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiffSpec {
    /// Everything not committed: `head` (`HEAD`'s tree, or the empty tree
    /// before the first commit) against the working tree, as `git diff HEAD`,
    /// plus untracked files. Each change carries its
    /// [`Staging`](super::Staging).
    Uncommitted {
        /// A tree id, not a commit's.
        head: ObjectId,
    },
    /// `git diff`: the index against the working tree, plus untracked files
    /// when `untracked` is set (git itself never shows those in a diff).
    IndexToWorktree {
        /// Report untracked, unignored files as [`Status::Untracked`](super::Status::Untracked).
        untracked: bool,
    },
    /// `git diff --cached [<rev>]`.
    TreeToIndex {
        /// The old side's tree.
        tree: ObjectId,
    },
    /// `git diff <rev>`.
    TreeToWorktree {
        /// The old side's tree.
        tree: ObjectId,
    },
    /// `git diff <a> <b>`, `a..b`, `a...b`, and `show`.
    TreeToTree {
        /// The old side's tree; for `a...b`, the merge base's.
        old: ObjectId,
        /// The new side's tree.
        new: ObjectId,
    },
    /// Two files or directories outside git, as the handle from
    /// [`Repo::compare`] holds them; no other handle lists anything for it.
    Paths,
    /// What changed since a checkpoint ([`Repo::take_checkpoint`]): its
    /// tree against the working tree as [`DiffSpec::Uncommitted`] sees it,
    /// untracked files included, staged or not alike. A file untracked
    /// both then and now is compared with its checkpointed content; one
    /// untracked only now is [`Status::Untracked`](super::Status::Untracked).
    /// Nested repositories aren't in a checkpoint and aren't listed.
    Checkpoint {
        /// The checkpoint's tree.
        tree: ObjectId,
    },
}

/// `git diff` arguments as typed, kept to resolve them again once refs move.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Revisions {
    /// Revisions or ranges (`a..b`, `a...b`) as typed; [`Repo::resolve`]
    /// says which shapes it takes.
    pub revs: Vec<String>,
    /// `--staged`/`--cached`: the new side is the index.
    pub staged: bool,
    /// The index against the working tree, exactly `git diff` without
    /// arguments; takes no revisions.
    pub unstaged: bool,
    /// `--merge-base`: the old side is the merge base of the revision and
    /// `HEAD` (or of the two revisions).
    pub merge_base: bool,
    /// `--since-checkpoint`: the checkpoint's tree against the working
    /// tree ([`DiffSpec::Checkpoint`]); takes nothing else. Resolved again
    /// on each rescan, so [`WhichCheckpoint::Latest`] follows new ones.
    pub since_checkpoint: Option<WhichCheckpoint>,
}

impl Repo {
    /// [`Repo::resolve`] for arguments kept as [`Revisions`].
    ///
    /// # Errors
    ///
    /// As [`Repo::resolve`]; [`Error::UnsupportedRevision`] for `unstaged`
    /// with revisions, `staged` or `merge_base`.
    pub fn resolve_revisions(&self, r: &Revisions) -> Result<DiffSpec> {
        if let Some(which) = r.since_checkpoint {
            if !r.revs.is_empty() || r.staged || r.unstaged || r.merge_base {
                return Err(Error::UnsupportedRevision {
                    rev: r.revs.first().cloned().unwrap_or_default(),
                    reason: "--since-checkpoint compares a checkpoint with the working tree; \
                             it takes no revisions, --staged, --unstaged or --merge-base",
                });
            }
            return Ok(DiffSpec::Checkpoint {
                tree: self.checkpoint(which)?.tree,
            });
        }
        if r.unstaged {
            return match r.revs.first() {
                None if !r.staged && !r.merge_base => {
                    Ok(DiffSpec::IndexToWorktree { untracked: false })
                }
                rev => Err(Error::UnsupportedRevision {
                    rev: rev.cloned().unwrap_or_default(),
                    reason: "--unstaged compares the index with the working tree; \
                             it takes no revisions, --staged or --merge-base",
                }),
            };
        }
        let revs: Vec<&str> = r.revs.iter().map(String::as_str).collect();
        self.resolve(&revs, r.staged, r.merge_base)
    }

    /// Resolves `git diff` arguments with git's semantics, except that no
    /// arguments mean everything uncommitted rather than the index against
    /// the working tree:
    ///
    /// | revs | `staged` | result |
    /// |---|---|---|
    /// | none | no | [`DiffSpec::Uncommitted`] |
    /// | none | yes | `HEAD` vs index |
    /// | `X` | no | `X` vs worktree |
    /// | `X` | yes | `X` vs index |
    /// | `X Y`, `X..Y` | no | `X` vs `Y` |
    /// | `X...Y` | no | merge-base of `X` and `Y` vs `Y` |
    /// | `X^!` | no | first parent of `X` vs `X` |
    ///
    /// An empty side of a range means `HEAD`. `merge_base` is `--merge-base`:
    /// with one rev the old side becomes the merge base of it and `HEAD`, with
    /// two the merge base of both. An unborn `HEAD` reads as the empty tree.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownRevision`] with a suggestion for names that don't
    /// resolve, [`Error::UnsupportedRevision`] for argument shapes git would
    /// reject or that need a combined diff, [`Error::NoMergeBase`].
    pub fn resolve(&self, revs: &[&str], staged: bool, merge_base: bool) -> Result<DiffSpec> {
        let unsupported = |rev: &str, reason| {
            Err(Error::UnsupportedRevision {
                rev: rev.to_owned(),
                reason,
            })
        };
        match revs {
            [] if merge_base => unsupported("", "--merge-base needs a revision"),
            [] if staged => Ok(DiffSpec::TreeToIndex {
                tree: self.head_tree()?,
            }),
            [] => Ok(DiffSpec::Uncommitted {
                head: self.head_tree()?,
            }),
            [rev] => match self.rev(rev)? {
                Spec::Include(id) => {
                    let base = if merge_base {
                        self.merge_base(id, self.head_commit()?)?
                    } else {
                        id
                    };
                    let tree = self.tree_of(base)?;
                    Ok(if staged {
                        DiffSpec::TreeToIndex { tree }
                    } else {
                        DiffSpec::TreeToWorktree { tree }
                    })
                }
                _ if staged || merge_base => unsupported(
                    rev,
                    "ranges can't be combined with --staged or --merge-base",
                ),
                Spec::Range { from, to } => Ok(DiffSpec::TreeToTree {
                    old: self.tree_of(from)?,
                    new: self.tree_of(to)?,
                }),
                Spec::Merge { theirs, ours } => Ok(DiffSpec::TreeToTree {
                    old: self.tree_of(self.merge_base(theirs, ours)?)?,
                    new: self.tree_of(ours)?,
                }),
                Spec::ExcludeParents(id) => self.against_parent(id),
                Spec::Exclude(_) | Spec::IncludeOnlyParents(_) => {
                    unsupported(rev, "not a single revision or range")
                }
            },
            [a, b] if !staged => {
                let (Spec::Include(x), Spec::Include(y)) = (self.rev(a)?, self.rev(b)?) else {
                    return unsupported(b, "two revisions must each name a single commit or tree");
                };
                let old = if merge_base {
                    self.merge_base(x, y)?
                } else {
                    x
                };
                Ok(DiffSpec::TreeToTree {
                    old: self.tree_of(old)?,
                    new: self.tree_of(y)?,
                })
            }
            [_, b] => unsupported(b, "--staged takes at most one revision"),
            [.., last] => unsupported(last, "combined diffs of more than two revisions"),
        }
    }

    /// Resolves `dffy show <rev>`: the commit against its first parent, or
    /// against the empty tree for a root commit. Returns the commit id too, for
    /// the header.
    ///
    /// # Errors
    ///
    /// As [`Repo::resolve`]; also when `rev` isn't a commit.
    pub fn resolve_show(&self, rev: &str) -> Result<(ObjectId, DiffSpec)> {
        match self.rev(rev)? {
            Spec::Include(id) => {
                let commit = self.git()?.find_object(id)?.peel_to_commit()?.id;
                Ok((commit, self.against_parent(commit)?))
            }
            _ => Err(Error::UnsupportedRevision {
                rev: rev.to_owned(),
                reason: "show takes a single revision",
            }),
        }
    }

    /// Whether `rev` names a single object, as `git rev-parse --verify`
    /// would accept it. Always false for paths compared outside git.
    #[must_use]
    pub fn is_revision(&self, rev: &str) -> bool {
        self.git()
            .is_ok_and(|repo| repo.rev_parse_single(BStr::new(rev)).is_ok())
    }

    fn against_parent(&self, id: ObjectId) -> Result<DiffSpec> {
        let commit = self.git()?.find_object(id)?.peel_to_commit()?;
        let old = match commit.parent_ids().next() {
            Some(parent) => self.tree_of(parent.detach())?,
            None => ObjectId::empty_tree(self.git()?.object_hash()),
        };
        Ok(DiffSpec::TreeToTree {
            old,
            new: commit.tree_id()?.detach(),
        })
    }

    fn rev(&self, rev: &str) -> Result<Spec> {
        match self.git()?.rev_parse(rev) {
            Ok(spec) => Ok(spec.detach()),
            Err(source) => Err(Error::UnknownRevision {
                rev: rev.to_owned(),
                suggestion: self.suggest(rev),
                source,
            }),
        }
    }

    fn tree_of(&self, id: ObjectId) -> Result<ObjectId> {
        Ok(self.git()?.find_object(id)?.peel_to_tree()?.id)
    }

    pub(super) fn head_tree(&self) -> Result<ObjectId> {
        Ok(self.git()?.head_tree_id_or_empty()?.detach())
    }

    fn head_commit(&self) -> Result<ObjectId> {
        Ok(self.git()?.head_id()?.detach())
    }

    fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<ObjectId> {
        let a = self.git()?.find_object(a)?.peel_to_commit()?.id;
        let b = self.git()?.find_object(b)?.peel_to_commit()?.id;
        match self.git()?.merge_base(a, b)? {
            Some(id) => Ok(id.detach()),
            None => Err(Error::NoMergeBase(a, b)),
        }
    }

    /// The ref name closest to the part of `rev` that failed to resolve. Only
    /// names that don't resolve get a suggestion: `HEAD~99` in a short history
    /// fails for another reason.
    fn suggest(&self, rev: &str) -> Option<String> {
        let repo = self.git().ok()?;
        let failing = rev
            .split("...")
            .flat_map(|s| s.split(".."))
            .find(|part| !part.is_empty() && repo.rev_parse_single(BStr::new(part)).is_err())?;
        let name = failing.split(['^', '~', ':']).next()?;
        let name = name.split("@{").next()?;
        if name.is_empty() || repo.rev_parse_single(BStr::new(name)).is_ok() {
            return None;
        }
        let refs = repo.references().ok()?;
        let names: Vec<String> = [refs.local_branches(), refs.tags(), refs.remote_branches()]
            .into_iter()
            .flatten()
            .flatten()
            .flatten()
            .map(|r| r.name().shorten().to_str_lossy().into_owned())
            .collect();
        crate::suggest::closest(name, names.iter().map(String::as_str)).map(str::to_owned)
    }
}

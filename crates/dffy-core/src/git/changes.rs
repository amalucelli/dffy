//! A diff's file list kept current as files change, for watch mode. The
//! parsed index stays loaded between refreshes: on a kernel-size repository
//! loading it costs about 10ms, most of a refresh's budget, while checking a
//! few paths against it costs microseconds.

use std::ops::ControlFlow;

use gix::bstr::{BStr, BString, ByteSlice};

use super::{DiffSpec, FileChange, Filter, Repo, Result, Revisions, Side, Status};
use crate::cache::Cache;

/// The changed files of one diff, re-evaluated per path as the working tree
/// changes and in full when git state (index, `HEAD`, refs) changes.
pub struct ChangeSet {
    repo: Repo,
    revisions: Revisions,
    spec: DiffSpec,
    /// The user's pathspecs.
    filter: Filter,
    reverse: bool,
    index: Option<gix::index::File>,
    /// In enumeration order, not reversed.
    files: Vec<FileChange>,
    /// `None` when renames aren't detected.
    renames: Option<Renames>,
}

/// The last rename pass, reused while the additions and deletions it saw
/// stay the same.
struct Renames {
    cache: Option<Cache>,
    /// The additions and deletions of the list it ran on.
    inputs: Option<Vec<FileChange>>,
    /// Each pair as (source path, renamed change).
    pairs: Vec<(BString, FileChange)>,
}

/// The file list after a refresh, with what its diffs may need redone.
#[derive(Debug, Clone)]
pub struct Update {
    /// The whole list, in enumeration order.
    pub files: Vec<FileChange>,
    dirty: Dirty,
}

#[derive(Debug, Clone)]
enum Dirty {
    /// Files under these path prefixes may have changed on disk.
    Paths(Filter),
    /// Any working-tree file may have changed.
    Worktree,
    /// Only the pairing of files changed, not their content.
    Nothing,
}

impl Update {
    /// Whether the content behind `change` may differ from when it was last
    /// diffed even though the change itself compares equal: a working-tree
    /// side without a hash, or either side of compared paths, looks the
    /// same before and after an edit.
    #[must_use]
    pub fn is_dirty(&self, change: &FileChange) -> bool {
        let worktree = [change.old, change.new]
            .iter()
            .any(|side| matches!(side, Some(Side::Worktree { .. } | Side::Path { .. })));
        worktree
            && match &self.dirty {
                // A rename between compared folders reads its old side from
                // disk too, at the path it came from.
                Dirty::Paths(filter) => {
                    filter.matches(&change.path) || filter.matches(change.old_path())
                }
                Dirty::Worktree => true,
                Dirty::Nothing => false,
            }
    }
}

impl ChangeSet {
    /// A change set for `spec`, resolved from `revisions`, limited to the
    /// path prefixes `paths`. Empty until [`ChangeSet::enumerate`].
    #[must_use]
    pub fn new(
        repo: Repo,
        spec: DiffSpec,
        revisions: Revisions,
        paths: &[BString],
        reverse: bool,
    ) -> Self {
        Self {
            repo,
            revisions,
            spec,
            filter: Filter::new(paths),
            reverse,
            index: None,
            files: Vec::new(),
            renames: None,
        }
    }

    /// Pairs deletions with additions into renames in every list it hands
    /// out after the initial enumeration, which streams them unpaired (see
    /// [`ChangeSet::renamed`]). `cache` keeps rename results across runs.
    #[must_use]
    pub fn with_renames(mut self, cache: Option<Cache>) -> Self {
        self.renames = Some(Renames {
            cache,
            inputs: None,
            pairs: Vec::new(),
        });
        self
    }

    /// Lists `revisions` from the next [`ChangeSet::rescan`] on.
    pub fn set_revisions(&mut self, revisions: Revisions) {
        self.revisions = revisions;
    }

    /// The current list, in enumeration order, without renames.
    #[must_use]
    pub fn files(&self) -> Vec<FileChange> {
        self.files
            .iter()
            .map(|c| self.oriented(c.clone()))
            .collect()
    }

    fn oriented(&self, change: FileChange) -> FileChange {
        if self.reverse {
            change.reversed()
        } else {
            change
        }
    }

    /// Lists every change, handing each to `emit` as it's found (the first
    /// files paint before the scan ends), and keeps the list. `emit` can
    /// stop early, which leaves the list partial.
    ///
    /// # Errors
    ///
    /// As [`Repo::for_each_change`].
    pub fn enumerate(&mut self, mut emit: impl FnMut(FileChange) -> ControlFlow<()>) -> Result<()> {
        self.files.clear();
        let (repo, reverse, files) = (&self.repo, self.reverse, &mut self.files);
        repo.changes_in(&self.spec, &mut self.index, &self.filter, &mut |change| {
            files.push(change.clone());
            emit(if reverse { change.reversed() } else { change })
        })
    }

    /// After [`ChangeSet::enumerate`], the list with renames paired, when
    /// renames are on and any were found.
    ///
    /// # Errors
    ///
    /// As [`Repo::detect_renames`].
    pub fn renamed(&mut self) -> Result<Option<Update>> {
        let files = self.listed()?;
        let paired = self.renames.as_ref().is_some_and(|r| !r.pairs.is_empty());
        Ok(paired.then_some(Update {
            files,
            dirty: Dirty::Nothing,
        }))
    }

    /// The current list, oriented, with renames paired when they're on.
    /// The pass reruns only when the additions or deletions changed.
    fn listed(&mut self) -> Result<Vec<FileChange>> {
        let files = self.files();
        let Some(renames) = &mut self.renames else {
            return Ok(files);
        };
        let inputs: Vec<FileChange> = files
            .iter()
            .filter(|c| matches!(c.status, Status::Added | Status::Deleted))
            .cloned()
            .collect();
        if renames.inputs.as_ref() != Some(&inputs) {
            renames.pairs = self
                .repo
                .detect_renames(files.clone(), renames.cache.as_ref())?
                .into_iter()
                .filter_map(|c| match &c.status {
                    Status::Renamed { from, .. } => Some((from.clone(), c)),
                    _ => None,
                })
                .collect();
            renames.inputs = Some(inputs);
        }
        if renames.pairs.is_empty() {
            return Ok(files);
        }
        let sources: rustc_hash::FxHashSet<&BString> = renames.pairs.iter().map(|p| &p.0).collect();
        let by_path: rustc_hash::FxHashMap<&BString, &FileChange> =
            renames.pairs.iter().map(|p| (&p.1.path, &p.1)).collect();
        Ok(files
            .into_iter()
            .filter_map(|c| match c.status {
                Status::Deleted if sources.contains(&c.path) => None,
                Status::Added => Some(by_path.get(&c.path).map_or(c, |&r| r.clone())),
                _ => Some(c),
            })
            .collect())
    }

    /// Re-evaluates the files under the repository-relative path prefixes
    /// `paths` (a path that was a directory covers what was in it, and a
    /// `.gitignore` covers its directory). `None` when the diff can't have
    /// changed: it doesn't involve the working tree, or nothing listed or
    /// found lives under `paths`.
    ///
    /// # Errors
    ///
    /// As [`Repo::for_each_change`].
    pub fn refresh(&mut self, paths: &[BString]) -> Result<Option<Update>> {
        if matches!(
            self.spec,
            DiffSpec::TreeToTree { .. } | DiffSpec::TreeToIndex { .. }
        ) {
            return Ok(None);
        }
        let paths: Vec<BString> = paths
            .iter()
            .map(|path| match path.strip_suffix(b".gitignore") {
                Some(dir) if dir.is_empty() || dir.ends_with(b"/") => dir.into(),
                _ => path.clone(),
            })
            .collect();
        let changed = Filter::new(&paths);
        if changed.is_empty() {
            // The working tree's root itself changed.
            return self.rescan();
        }
        let mut found = Vec::new();
        self.repo
            .changes_in(&self.spec, &mut self.index, &changed, &mut |change| {
                if self.filter.matches(&change.path) {
                    found.push(change);
                }
                ControlFlow::Continue(())
            })?;
        let before = self.files.len();
        self.files.retain(|c| !changed.matches(&c.path));
        if found.is_empty() && self.files.len() == before {
            return Ok(None);
        }
        self.files.extend(found);
        self.files.sort_by(|a, b| order(a).cmp(&order(b)));
        Ok(Some(Update {
            files: self.listed()?,
            dirty: Dirty::Paths(changed),
        }))
    }

    /// Resolves the revisions again, reloads the index and lists every
    /// change. `None` when only revisions are compared and they still
    /// resolve to the same trees.
    ///
    /// # Errors
    ///
    /// As [`Repo::resolve`] and [`Repo::for_each_change`]; the list is kept
    /// as it was.
    pub fn rescan(&mut self) -> Result<Option<Update>> {
        let spec = match self.spec {
            DiffSpec::Paths => DiffSpec::Paths,
            _ => self.repo.resolve_revisions(&self.revisions)?,
        };
        if spec == self.spec && matches!(spec, DiffSpec::TreeToTree { .. }) {
            return Ok(None);
        }
        let mut index = None;
        let mut files = Vec::new();
        self.repo
            .changes_in(&spec, &mut index, &self.filter, &mut |change| {
                files.push(change);
                ControlFlow::Continue(())
            })?;
        (self.spec, self.index, self.files) = (spec, index, files);
        Ok(Some(Update {
            files: self.listed()?,
            dirty: Dirty::Worktree,
        }))
    }
}

/// Enumeration order: tracked changes by path, then untracked files by path.
fn order(change: &FileChange) -> (bool, &BStr) {
    (change.status == Status::Untracked, change.path.as_bstr())
}

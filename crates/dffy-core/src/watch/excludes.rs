//! Which working-tree paths a watcher can leave alone: those git ignores and
//! doesn't track. The rules are the untracked walk's (`.gitignore` files, the
//! repository's `info/exclude`, `core.excludesFile`), so a path the diff
//! never lists is never watched, and changes to it never cause a refresh.
//! On Linux that keeps build output from spending the per-user watch limit.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::Root;
use crate::git::Repo;

pub(super) struct Excludes {
    worktree: PathBuf,
    /// Each directory's `.gitignore`, read on first use; empty when it has
    /// none.
    per_dir: HashMap<PathBuf, Gitignore>,
    exclude_path: PathBuf,
    exclude: Gitignore,
    global_path: Option<PathBuf>,
    global: Gitignore,
    global_stamp: Option<Stamp>,
    /// Whether each directory looked at so far is ignored, itself or through
    /// a parent. Only valid until the rules change.
    ignored_dirs: HashMap<PathBuf, bool>,
    tracked: Tracked,
    /// Without a repository no rule file is read or noticed.
    has_repo: bool,
}

type Stamp = (i64, i64, u64, u64);

impl Excludes {
    /// Without a repository nothing is excluded.
    pub(super) fn load(root: &Root, repo: Option<&Repo>) -> Self {
        let global_path = repo.and_then(crate::git::excludes_file);
        let exclude_path = root.common_dir.join("info").join("exclude");
        let mut excludes = Self {
            has_repo: repo.is_some(),
            worktree: root.worktree.clone(),
            per_dir: HashMap::new(),
            exclude: Gitignore::empty(),
            exclude_path,
            global: Gitignore::empty(),
            global_stamp: None,
            global_path,
            ignored_dirs: HashMap::new(),
            tracked: repo.and_then(Tracked::load).unwrap_or_default(),
        };
        if repo.is_some() {
            excludes.reload_all();
        }
        excludes
    }

    /// Whether events for `path` can be dropped: git ignores it and tracks
    /// nothing at or below it.
    pub(super) fn skip(&mut self, path: &Path, is_dir: bool) -> bool {
        self.rel(path).is_some() && !self.tracked(path) && self.ignored(path, is_dir)
    }

    /// Whether `path` is a tracked file, or a directory holding one.
    pub(super) fn tracked(&self, path: &Path) -> bool {
        self.rel(path)
            .is_some_and(|rel| self.tracked.holds(rel.as_os_str().as_bytes()))
    }

    /// Whether git ignores directory `dir`, given whether its parent is
    /// ignored; the answer is remembered for [`Excludes::skip`].
    #[cfg_attr(
        target_os = "macos",
        expect(dead_code, reason = "FSEvents watches every directory")
    )]
    pub(super) fn child_dir_ignored(&mut self, dir: &Path, parent_ignored: bool) -> bool {
        if self.rel(dir).is_none() {
            return false;
        }
        let ignored = parent_ignored || self.matched(dir, true);
        self.ignored_dirs.insert(dir.to_owned(), ignored);
        ignored
    }

    /// Whether git ignores `path`, itself or through a parent directory: git
    /// never looks inside an ignored directory, so nothing re-includes what's
    /// below it.
    fn ignored(&mut self, path: &Path, is_dir: bool) -> bool {
        path.parent().is_some_and(|parent| self.dir_ignored(parent)) || self.matched(path, is_dir)
    }

    /// Whether git ignores directory `dir`, itself or through a parent.
    pub(super) fn dir_ignored(&mut self, dir: &Path) -> bool {
        if self.rel(dir).is_none() {
            return false;
        }
        if let Some(&ignored) = self.ignored_dirs.get(dir) {
            return ignored;
        }
        let ignored = self.ignored(dir, true);
        self.ignored_dirs.insert(dir.to_owned(), ignored);
        ignored
    }

    /// The rule for `path` alone, with the walk's precedence: the nearest
    /// `.gitignore` first, then `info/exclude`, then the global file.
    fn matched(&mut self, path: &Path, is_dir: bool) -> bool {
        if !self.has_repo || self.rel(path).is_none() {
            return false;
        }
        let mut dir = path.parent();
        while let Some(d) = dir {
            if !self.per_dir.contains_key(d) {
                self.per_dir
                    .insert(d.to_owned(), Gitignore::new(d.join(".gitignore")).0);
            }
            match self.per_dir[d].matched(path, is_dir) {
                Match::Ignore(_) => return true,
                Match::Whitelist(_) => return false,
                Match::None => {}
            }
            if d == self.worktree {
                break;
            }
            dir = d.parent();
        }
        self.exclude
            .matched(path, is_dir)
            .or(self.global.matched(path, is_dir))
            .is_ignore()
    }

    /// Whether `path` holds rules the walk reads: `info/exclude`, or a
    /// `.gitignore` in a directory the walk enters.
    pub(super) fn is_rule_file(&mut self, path: &Path) -> bool {
        if !self.has_repo {
            return false;
        }
        if path == self.exclude_path {
            return true;
        }
        path.file_name() == Some(OsStr::new(".gitignore"))
            && self.rel(path).is_some()
            && !path.parent().is_some_and(|dir| self.dir_ignored(dir))
    }

    /// Takes in changes to the rule files in `changed`, and to the global
    /// excludes file, which isn't watched and is checked here instead.
    /// Returns whether any rule changed.
    pub(super) fn reload(&mut self, changed: &[PathBuf]) -> bool {
        let global_changed = self.global_stamp() != self.global_stamp;
        if global_changed {
            self.load_global();
        }
        for path in changed {
            if *path == self.exclude_path {
                self.exclude = self.read_rules(&self.exclude_path);
            } else if let Some(dir) = path.parent() {
                self.per_dir.remove(dir);
            }
        }
        let any = global_changed || !changed.is_empty();
        if any {
            self.ignored_dirs.clear();
        }
        any
    }

    /// Rereads every rule file, for when changes to them may have been lost.
    pub(super) fn reload_all(&mut self) {
        self.per_dir.clear();
        self.ignored_dirs.clear();
        self.exclude = self.read_rules(&self.exclude_path);
        self.load_global();
    }

    pub(super) fn set_tracked(&mut self, tracked: Tracked) {
        self.tracked = tracked;
    }

    fn load_global(&mut self) {
        self.global_stamp = self.global_stamp();
        self.global = match &self.global_path {
            Some(path) if self.global_stamp.is_some() => self.read_rules(path),
            _ => Gitignore::empty(),
        };
    }

    fn global_stamp(&self) -> Option<Stamp> {
        let meta = fs::metadata(self.global_path.as_ref()?).ok()?;
        meta.is_file()
            .then(|| (meta.mtime(), meta.mtime_nsec(), meta.size(), meta.ino()))
    }

    /// A rule file whose patterns apply from the top of the working tree.
    fn read_rules(&self, path: &Path) -> Gitignore {
        let mut builder = GitignoreBuilder::new(&self.worktree);
        // A missing or partly malformed file degrades to fewer rules, as in
        // git and the untracked walk.
        builder.add(path);
        builder.build().unwrap_or_else(|_| Gitignore::empty())
    }

    /// `path` relative to the working tree, when it's below it and outside
    /// any `.git`.
    fn rel<'a>(&self, path: &'a Path) -> Option<&'a Path> {
        let rel = path.strip_prefix(&self.worktree).ok()?;
        let inside = !rel.as_os_str().is_empty()
            && !rel
                .components()
                .any(|c| c == Component::Normal(OsStr::new(".git")));
        inside.then_some(rel)
    }
}

/// The index's paths, sorted as git sorts them, in one buffer.
#[derive(Default)]
pub(super) struct Tracked {
    bytes: Vec<u8>,
    paths: Vec<Range<usize>>,
}

impl Tracked {
    /// `None` when the index can't be read.
    pub(super) fn load(repo: &Repo) -> Option<Self> {
        let index = repo.load_index().ok()?;
        let mut tracked = Self::default();
        for entry in index.entries() {
            let path: &[u8] = entry.path(&index);
            // Conflicted paths have an entry per stage.
            if tracked
                .paths
                .last()
                .is_some_and(|last| &tracked.bytes[last.clone()] == path)
            {
                continue;
            }
            let start = tracked.bytes.len();
            tracked.bytes.extend_from_slice(path);
            tracked.paths.push(start..tracked.bytes.len());
        }
        Some(tracked)
    }

    /// Whether `rel` is in the index, or a directory with entries below it.
    fn holds(&self, rel: &[u8]) -> bool {
        let at = |key: &[u8]| {
            let i = self.paths.partition_point(|p| &self.bytes[p.clone()] < key);
            self.paths.get(i).map(|p| &self.bytes[p.clone()])
        };
        if at(rel) == Some(rel) {
            return true;
        }
        let mut dir = Vec::with_capacity(rel.len() + 1);
        dir.extend_from_slice(rel);
        dir.push(b'/');
        at(&dir).is_some_and(|p| p.starts_with(&dir))
    }
}

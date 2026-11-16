//! Tree-to-tree and tree-to-index diffs as ordered merge-joins, emitting in
//! path order and skipping equal subtrees by id. Renames are a separate pass.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use gix::bstr::{BStr, BString, ByteSlice};
use gix::index::extension::Tree as CacheTree;
use gix::objs::FindExt;
use gix::objs::tree::EntryRef;

use super::{FileChange, Filter, Mode, ObjectId, Repo, Result, Side};

pub(super) fn diff_trees(
    repo: &Repo,
    old: ObjectId,
    new: ObjectId,
    filter: &Filter,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let mut diff = TreeDiff { repo, filter, emit };
    diff.trees(Some(&old), Some(&new), &mut BString::default())
}

struct TreeDiff<'a> {
    repo: &'a Repo,
    filter: &'a Filter,
    emit: &'a mut dyn FnMut(FileChange) -> ControlFlow<()>,
}

impl TreeDiff<'_> {
    /// Diffs the trees at `prefix`, which is empty or ends in `/`.
    fn trees(
        &mut self,
        old: Option<&gix::oid>,
        new: Option<&gix::oid>,
        prefix: &mut BString,
    ) -> Result<ControlFlow<()>> {
        if old == new {
            return Ok(ControlFlow::Continue(()));
        }
        let objects = &self.repo.git()?.objects;
        let (mut old_buf, mut new_buf) = (Vec::new(), Vec::new());
        let old = match old {
            Some(id) => objects.find_tree(id, &mut old_buf)?.entries,
            None => Vec::new(),
        };
        let new = match new {
            Some(id) => objects.find_tree(id, &mut new_buf)?.entries,
            None => Vec::new(),
        };
        let (mut i, mut j) = (0, 0);
        while i < old.len() || j < new.len() {
            let order = match (old.get(i), new.get(j)) {
                (Some(a), Some(b)) => entry_order(a, b),
                (Some(_), None) => Ordering::Less,
                _ => Ordering::Greater,
            };
            let flow = match order {
                Ordering::Less => {
                    i += 1;
                    self.entry(Some(&old[i - 1]), None, prefix)?
                }
                Ordering::Greater => {
                    j += 1;
                    self.entry(None, Some(&new[j - 1]), prefix)?
                }
                Ordering::Equal => {
                    let (a, b) = (&old[i], &new[j]);
                    i += 1;
                    j += 1;
                    if a.oid == b.oid && a.mode == b.mode {
                        continue;
                    }
                    self.entry(Some(a), Some(b), prefix)?
                }
            };
            if flow.is_break() {
                return Ok(flow);
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Handles one name present on either side. Both sides, when present,
    /// are either trees or non-trees: [`entry_order`] tells them apart.
    fn entry(
        &mut self,
        old: Option<&EntryRef<'_>>,
        new: Option<&EntryRef<'_>>,
        prefix: &mut BString,
    ) -> Result<ControlFlow<()>> {
        let Some(any) = old.or(new) else {
            return Ok(ControlFlow::Continue(()));
        };
        let len = prefix.len();
        prefix.extend_from_slice(any.filename);
        let flow = if any.mode.is_tree() {
            if self.filter.may_contain(prefix) {
                prefix.push(b'/');
                self.trees(old.map(|e| e.oid), new.map(|e| e.oid), prefix)
            } else {
                Ok(ControlFlow::Continue(()))
            }
        } else if self.filter.matches(prefix) {
            Ok((self.emit)(FileChange::new(
                prefix.clone(),
                old.and_then(object_side),
                new.and_then(object_side),
            )))
        } else {
            Ok(ControlFlow::Continue(()))
        };
        prefix.truncate(len);
        flow
    }
}

fn object_side(e: &EntryRef<'_>) -> Option<Side> {
    Mode::from_tree(e.mode).map(|mode| Side::Object {
        id: e.oid.to_owned(),
        mode,
    })
}

/// Git's tree order: names compare bytewise as if trees had a trailing `/`,
/// which makes a depth-first walk come out in full-path byte order.
fn entry_order(a: &EntryRef<'_>, b: &EntryRef<'_>) -> Ordering {
    let (x, y) = (a.filename.as_bytes(), b.filename.as_bytes());
    let common = x.len().min(y.len());
    x[..common].cmp(&y[..common]).then_with(|| {
        let end = |name: &[u8], tree: bool| {
            name.get(common)
                .copied()
                .unwrap_or(if tree { b'/' } else { 0 })
        };
        end(x, a.mode.is_tree()).cmp(&end(y, b.mode.is_tree()))
    })
}

pub(super) fn diff_tree_index(
    repo: &Repo,
    tree: ObjectId,
    index: &gix::index::State,
    filter: &Filter,
    emit: &mut dyn FnMut(FileChange) -> ControlFlow<()>,
) -> Result<ControlFlow<()>> {
    let mut walk = TreeIndex {
        repo,
        index,
        cursor: 0,
        filter,
        emit,
    };
    if walk.root(&tree, &mut BString::default())?.is_break() {
        return Ok(ControlFlow::Break(()));
    }
    Ok(walk.drain_before(None))
}

/// Walks a tree depth-first while advancing a cursor through the index, which
/// is sorted the same way. Subtrees whose id matches the index's cache-tree
/// node are skipped along with the index entries they cover.
struct TreeIndex<'a> {
    repo: &'a Repo,
    index: &'a gix::index::State,
    cursor: usize,
    filter: &'a Filter,
    emit: &'a mut dyn FnMut(FileChange) -> ControlFlow<()>,
}

impl<'a> TreeIndex<'a> {
    fn root(&mut self, tree: &gix::oid, prefix: &mut BString) -> Result<ControlFlow<()>> {
        let cache = self.index.tree();
        if let Some(n) = cache.filter(|c| c.id == tree).and_then(|c| c.num_entries)
            && self.covers(b"", n as usize)
        {
            self.cursor += n as usize;
            return Ok(ControlFlow::Continue(()));
        }
        self.walk(tree, prefix, cache)
    }

    /// Walks the tree at `prefix`, which is empty or ends in `/`.
    fn walk(
        &mut self,
        tree: &gix::oid,
        prefix: &mut BString,
        cache: Option<&CacheTree>,
    ) -> Result<ControlFlow<()>> {
        let mut buf = Vec::new();
        let entries = self.repo.git()?.objects.find_tree(tree, &mut buf)?.entries;
        for e in &entries {
            let len = prefix.len();
            prefix.extend_from_slice(e.filename);
            let flow = if e.mode.is_tree() {
                prefix.push(b'/');
                self.subtree(e, prefix, cache)
            } else {
                Ok(self.blob(e, prefix))
            };
            prefix.truncate(len);
            if flow?.is_break() {
                return Ok(ControlFlow::Break(()));
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn subtree(
        &mut self,
        e: &EntryRef<'_>,
        dir: &mut BString,
        cache: Option<&CacheTree>,
    ) -> Result<ControlFlow<()>> {
        if self.drain_before(Some(dir)).is_break() {
            return Ok(ControlFlow::Break(()));
        }
        if !self.filter.may_contain(&dir[..dir.len() - 1]) {
            while self.cursor < self.len() && self.path(self.cursor).starts_with(dir) {
                self.cursor += 1;
            }
            return Ok(ControlFlow::Continue(()));
        }
        let child = cache.and_then(|c| {
            c.children
                .iter()
                .find(|c| c.name.as_slice() == e.filename.as_bytes())
        });
        if let Some(n) = child.filter(|c| c.id == e.oid).and_then(|c| c.num_entries)
            && self.covers(dir, n as usize)
        {
            self.cursor += n as usize;
            return Ok(ControlFlow::Continue(()));
        }
        self.walk(e.oid, dir, child)
    }

    fn blob(&mut self, e: &EntryRef<'_>, path: &BString) -> ControlFlow<()> {
        if self.drain_before(Some(path)).is_break() {
            return ControlFlow::Break(());
        }
        let old = object_side(e);
        let (new, unmerged) =
            if self.cursor < self.len() && self.path(self.cursor) == path.as_bstr() {
                self.take_index_side()
            } else {
                (None, false)
            };
        if old == new && !unmerged || !self.filter.matches(path) {
            return ControlFlow::Continue(());
        }
        let change = if unmerged {
            FileChange::unmerged
        } else {
            FileChange::new
        };
        (self.emit)(change(path.clone(), old, new))
    }

    /// Emits index entries sorting before `bound` (all remaining for `None`)
    /// as additions: the tree has nothing at those paths.
    fn drain_before(&mut self, bound: Option<&BString>) -> ControlFlow<()> {
        while self.cursor < self.len() {
            let path = self.path(self.cursor);
            if bound.is_some_and(|b| path >= b.as_bstr()) {
                break;
            }
            let (new, unmerged) = self.take_index_side();
            if (new.is_some() || unmerged) && self.filter.matches(path) {
                let change = if unmerged {
                    FileChange::unmerged
                } else {
                    FileChange::new
                };
                (self.emit)(change(path.to_owned(), None, new))?;
            }
        }
        ControlFlow::Continue(())
    }

    /// Consumes every entry for the path at the cursor and returns what the
    /// index holds there, and whether it's unmerged: nothing for an
    /// intent-to-add entry, and for a conflict the working-tree file, if
    /// there is one, since no single stage stands for it.
    fn take_index_side(&mut self) -> (Option<Side>, bool) {
        let entries = self.index.entries();
        let first = &entries[self.cursor];
        let path = self.path(self.cursor);
        self.cursor += 1;
        while self.cursor < entries.len() && self.path(self.cursor) == path {
            self.cursor += 1;
        }
        let unmerged = first.stage_raw() != 0;
        let side = Mode::from_index(first.mode).and_then(|mode| {
            if unmerged {
                let on_disk = self
                    .repo
                    .worktree_path(path)
                    .is_ok_and(|full| full.symlink_metadata().is_ok_and(|m| !m.is_dir()));
                on_disk.then_some(Side::Worktree { mode, id: None })
            } else if first
                .flags
                .contains(gix::index::entry::Flags::INTENT_TO_ADD)
            {
                None
            } else {
                Some(Side::Object { id: first.id, mode })
            }
        });
        (side, unmerged)
    }

    /// Whether the `n` entries at the cursor are exactly those under `dir`.
    /// The cache-tree's counts can't be trusted blindly.
    fn covers(&self, dir: &[u8], n: usize) -> bool {
        let end = self.cursor + n;
        end <= self.len()
            && (n == 0
                || self.path(self.cursor).starts_with(dir) && self.path(end - 1).starts_with(dir))
            && (end == self.len() || !self.path(end).starts_with(dir))
    }

    fn len(&self) -> usize {
        self.index.entries().len()
    }

    fn path(&self, i: usize) -> &'a BStr {
        self.index.entries()[i].path(self.index)
    }
}

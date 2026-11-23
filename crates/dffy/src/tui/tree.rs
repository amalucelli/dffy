//! The file tree beside the diff: directories that collapse, chains of
//! single-child directories shown as one row, a status letter and line
//! counts per file, in a bordered panel titled with the totals.
//!
//! It grows as files stream in and is rebuilt when the list is replaced.
//! Rows are flattened once per change to the tree, not per frame, and only
//! the rows on screen are drawn. Children keep git's path order, the
//! document's order, so moving down the tree moves down the diff.
//!
//! Nothing here reflows as counts arrive: the counts column has one width
//! for every row, and names are cut to fit it whether or not their own
//! counts are known. The column changes width once, when every file is
//! counted.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::document::{Counts, Side};
use dffy_core::view::Attributes;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::comments::look_role;
use super::fuzzy;
use super::picker::Paths;
use super::view::Entry;
use crate::render::{escape, stat};
use crate::review::model::Comments;
use crate::term::Grid;
use crate::theme::{Role, Style, Theme};

const ROOT: usize = 0;
/// Rows kept between the selection and the panel's top or bottom edge.
const SCROLLOFF: usize = 3;
/// Digits each count reserves until every file is counted, so the column
/// doesn't widen as counts stream in.
const PLACEHOLDER_DIGITS: usize = 3;
/// Larger counts are shown in thousands.
const MAX_DIGITS: usize = 5;

/// For the benchmarks: [`Tree::bench`].
pub(crate) fn bench_all_files(paths: &Arc<Paths>, files: &[Entry], open: Option<&[u8]>) -> usize {
    Tree::bench(paths, files, open)
}

/// Columns for the panel, borders included: just over a quarter of the
/// screen.
pub(super) fn width(total: usize) -> usize {
    (total * 28 / 100).clamp(24, 60)
}

struct Node {
    name: Box<[u8]>,
    parent: usize,
    kind: Kind,
}

enum Kind {
    Dir {
        children: Vec<usize>,
        open: bool,
    },
    File(usize),
    /// In the all-files mode, a file the diff doesn't list: its place in
    /// the version's list of files ([`AllFiles::paths`]).
    Path(usize),
    /// A commit of a piped log, at the top level in input order, holding
    /// its files' tree; `entry` is its own entry in the document.
    Commit {
        entry: usize,
        children: Vec<usize>,
        open: bool,
    },
    /// The review's conversation, the document's page: the first row,
    /// outside the root's children, which keep git's order.
    Page(usize),
    /// A top-level group of files of one kind, holding their tree, its row
    /// counting them; shown only while there are files outside the diff
    /// (see [`Section`]).
    Section {
        section: Section,
        children: Vec<usize>,
        open: bool,
        files: usize,
    },
}

/// The kinds of file the tree keeps apart, in the document's order: the
/// diff's files, untracked ones among them, then those opened outside it.
/// The diff's files are at the top level until a file outside it is
/// opened; then each kind has its group. The files of a commit's tree go
/// under the commit instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Section {
    Changed,
    /// Files the diff doesn't list, shown whole
    /// ([`Source::Unchanged`](dffy_core::document::Source::Unchanged)).
    Unlisted,
}

impl Section {
    pub(super) fn of(entry: &Entry) -> Self {
        if entry.is_unchanged() {
            Self::Unlisted
        } else {
            Self::Changed
        }
    }

    /// Its row's words, and its name among collapsed paths.
    fn label(self) -> &'static str {
        match self {
            Self::Changed => "Changed in this diff",
            Self::Unlisted => "Not in this diff",
        }
    }
}

/// The status letter of `entry`'s row; a file with no changes has a dot.
pub(super) fn letter(entry: &Entry) -> (&'static str, Role) {
    if entry.is_unchanged() {
        ("·", Role::Dim)
    } else {
        stat::status_letter(entry.status())
    }
}

impl Node {
    fn children(&self) -> &[usize] {
        match &self.kind {
            Kind::Dir { children, .. }
            | Kind::Commit { children, .. }
            | Kind::Section { children, .. } => children,
            Kind::File(_) | Kind::Path(_) | Kind::Page(_) => &[],
        }
    }

    fn children_mut(&mut self) -> Option<&mut Vec<usize>> {
        match &mut self.kind {
            Kind::Dir { children, .. }
            | Kind::Commit { children, .. }
            | Kind::Section { children, .. } => Some(children),
            Kind::File(_) | Kind::Path(_) | Kind::Page(_) => None,
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir { .. })
    }

    /// git's order: a directory sorts as its name followed by `/`.
    fn cmp_key(&self, name: &[u8], dir: bool) -> Ordering {
        let slash = |d: bool| d.then_some(&b'/');
        self.name
            .iter()
            .chain(slash(self.is_dir()))
            .cmp(name.iter().chain(slash(dir)))
    }
}

#[derive(Debug, Clone, Copy)]
struct Row {
    /// The file, or the deepest directory of a chain.
    node: usize,
    /// The first directory of a chain; `node` otherwise.
    top: usize,
    depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Item {
    File(usize),
    /// A file the diff doesn't list, in the all-files mode (see
    /// [`Tree::all_path`]).
    Path(usize),
    Dir {
        open: bool,
    },
    /// A commit's row; `file` is its entry.
    Commit {
        file: usize,
        open: bool,
    },
}

/// Totals over every file, for the title and the counts column.
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct Totals {
    pub files: usize,
    pub added: usize,
    pub removed: usize,
    max_added: usize,
    max_removed: usize,
    /// Every file has counts.
    complete: bool,
}

impl Totals {
    pub(super) fn of(files: &[Entry], listing: bool) -> Self {
        let mut t = Self {
            complete: !listing,
            ..Self::default()
        };
        for e in files.iter().filter(|e| !e.is_commit()) {
            t.files += 1;
            match e.counts() {
                Some(c) => {
                    t.added += c.added;
                    t.removed += c.removed;
                    t.max_added = t.max_added.max(c.added);
                    t.max_removed = t.max_removed.max(c.removed);
                }
                None => t.complete = false,
            }
        }
        t
    }

    /// The digits each side of the counts column holds: a fixed placeholder
    /// until every file is counted, whatever has arrived, then what the
    /// largest count needs. The column changes width at most once.
    fn digits(&self) -> (usize, usize) {
        if !self.complete {
            return (PLACEHOLDER_DIGITS, PLACEHOLDER_DIGITS);
        }
        let digits = |n: usize| (n.max(1).ilog10() as usize + 1).min(MAX_DIGITS);
        (digits(self.max_added), digits(self.max_removed))
    }
}

/// Where a file row's counts go, between the name starting at column `x`
/// and the panel's edge `end`: the column they start at, and each count's
/// text, role and column. Every row puts them in the same two columns, as
/// `git --stat` does: additions right-aligned in the first, deletions in
/// the second, each as wide as the [`Totals::digits`] it reserves; a
/// number wider than that is shortened (12345 to 12k) in place. Names end
/// before the column; they never push it. A zero count is left out, its
/// cells kept and in the row's style, so a selected row shows no colored
/// gap.
fn place_counts(
    counts: Option<Counts>,
    (x, end): (usize, usize),
    totals: &Totals,
) -> (usize, Vec<(String, Role, usize)>) {
    let (a, r) = totals.digits();
    let stats_x = end.saturating_sub(a + r + 4).max(x);
    let Some(Counts { added, removed }) = counts else {
        return (stats_x, Vec::new());
    };
    let placed = [
        ('+', added, a, Role::Added, stats_x),
        ('-', removed, r, Role::Removed, stats_x + a + 2),
    ]
    .into_iter()
    .filter(|&(_, n, ..)| n > 0)
    .map(|(sign, n, digits, role, at)| {
        let text = format!("{sign}{}", short(n, digits));
        let pad = (digits + 1).saturating_sub(text.chars().count());
        (text, role, at + pad)
    })
    .collect();
    (stats_x, placed)
}

/// `n` within `digits` columns: in thousands or millions when it's longer.
fn short(n: usize, digits: usize) -> String {
    [(1, ""), (1000, "k"), (1_000_000, "M")]
        .iter()
        .map(|&(unit, suffix)| format!("{}{suffix}", n / unit))
        .find(|text| text.len() <= digits)
        .unwrap_or_else(|| "∞".to_owned())
}

pub(super) struct Tree {
    nodes: Vec<Node>,
    /// Where paths are inserted: the root, or the last commit's node.
    base: usize,
    /// The node of each file, by its index in the document.
    leaves: Vec<usize>,
    /// Directories the user collapsed, by path, so a rebuilt tree keeps
    /// them collapsed.
    collapsed: HashSet<BString>,
    rows: Vec<Row>,
    /// The row showing each file, or the collapsed directory hiding it.
    file_rows: Vec<Option<usize>>,
    /// The rows no longer match the nodes.
    stale: bool,
    selected: usize,
    /// The node of the selected row, to find it again after a reflatten.
    selected_node: Option<usize>,
    /// The first row shown.
    scroll: usize,
    /// Rows the panel showed last, for paging and keeping the selection in
    /// view.
    height: usize,
    /// The fuzzy filter, lowercase; empty shows every file. Files it
    /// keeps show with their folders, and moving between files keeps to
    /// them.
    filter: String,
    /// `/` was pressed in the tree and keys go to the filter.
    pub editing: bool,
    /// For each file, the matched characters of its name when the filter
    /// keeps it; `None` when it hides it. Empty without a filter.
    hits: Vec<Option<Vec<usize>>>,
    /// The paths with comments, while the tree keeps only those, along
    /// with what the filter keeps.
    commented: Option<HashSet<BString>>,
    /// The page's node, shown first.
    page: Option<usize>,
    /// Each [`Section`]'s node, once it has a file.
    sections: [Option<usize>; 2],
    /// There are files outside the diff: the diff's go in a group too.
    grouped: bool,
    /// The files opened at the diff's old side, which a tag names by the
    /// version it is.
    at_old: (HashSet<usize>, String),
    /// In the all-files mode, what it lists.
    all: Option<Box<AllFiles>>,
}

/// The all-files mode's source (see [`Tree::all_files`]): every file of a
/// version, the diff's among them, made into nodes a folder at a time as
/// folders open, so no frame or build walks the whole list.
struct AllFiles {
    /// The side of the diff the version is.
    side: Side,
    /// The version's files, in path order; empty while being listed.
    paths: Arc<Paths>,
    /// The version's files are being listed: the diff's alone show.
    listing: bool,
    /// The diff's files by their path in the version, sorted, with their
    /// entry and their change's rank (see [`change_rank`]) and role.
    listed: Vec<(BString, usize, u8, Role)>,
    /// Folders whose children are made.
    loaded: HashSet<usize>,
    /// Folders with changes inside, colored as the strongest of them.
    marks: HashMap<usize, (u8, Role)>,
    /// Folders opened, and folders folded that would open alone (a lone
    /// folder's), by path: the mode's fold state, kept across builds.
    opened: HashSet<BString>,
    closed: HashSet<BString>,
    /// The version's name, for the title.
    version: String,
    /// The filter's matches, the best ones by path, and how many there
    /// are in all; `None` until the first are scored.
    matched: Option<(Vec<BString>, usize)>,
    /// A worker scores the filter's matches: those shown are an older
    /// query's.
    scoring: bool,
}

/// The most files of a filter's matches the all-files tree shows.
pub(super) const FILTER_SHOWN: usize = 1000;

/// How strongly a change marks the folders above it: a conflict, then a
/// deletion, an edit, an addition, an untracked file.
fn change_rank(status: &dffy_core::git::Status) -> u8 {
    use dffy_core::git::Status;
    match status {
        Status::Unmerged => 5,
        Status::Deleted => 4,
        Status::Modified | Status::Renamed { .. } | Status::TypeChange => 3,
        Status::Added => 2,
        Status::Untracked => 1,
    }
}

/// The first of items `lo..len` `before` holds no longer for.
fn lower_bound(lo: usize, len: usize, before: impl Fn(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (lo, len);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if before(mid) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo
}

impl Default for Tree {
    fn default() -> Self {
        Self {
            nodes: vec![Node {
                name: Box::default(),
                parent: ROOT,
                kind: Kind::Dir {
                    children: Vec::new(),
                    open: true,
                },
            }],
            base: ROOT,
            leaves: Vec::new(),
            collapsed: HashSet::new(),
            rows: Vec::new(),
            file_rows: Vec::new(),
            stale: false,
            selected: 0,
            selected_node: None,
            scroll: 0,
            height: 1,
            filter: String::new(),
            editing: false,
            hits: Vec::new(),
            commented: None,
            page: None,
            sections: [None; 2],
            grouped: false,
            at_old: (HashSet::new(), String::new()),
            all: None,
        }
    }
}

impl Tree {
    /// Adds file `file` (the next index) at `path`, in `section`: a file
    /// the diff lists goes under the last commit when there is one, else
    /// at the top level unless files outside the diff are listed too.
    pub(super) fn insert(&mut self, file: usize, path: &[u8], section: Section) {
        let top = match section {
            Section::Changed if self.base != ROOT || !self.grouped => self.base,
            _ => self.section(section),
        };
        let mut dir = top;
        // Collapsed folders are kept by their path (see `Tree::path`).
        let in_section = matches!(self.nodes[top].kind, Kind::Section { .. });
        let prefix = if top == ROOT || in_section || self.collapsed.is_empty() {
            Vec::new()
        } else {
            let mut p = self.path(top).to_vec();
            p.push(b'/');
            p
        };
        let mut parts = path.split_str("/").peekable();
        let mut end = 0;
        while let Some(name) = parts.next() {
            end += name.len();
            let last = parts.peek().is_none();
            let children = self.nodes[dir].children();
            // Paths mostly arrive in order: the place is usually the end.
            let found = match children.last() {
                Some(&c) if self.nodes[c].cmp_key(name, !last) == Ordering::Less => {
                    Err(children.len())
                }
                _ => children.binary_search_by(|&c| self.nodes[c].cmp_key(name, !last)),
            };
            let child = match found {
                Ok(i) if !last => self.nodes[dir].children()[i],
                Ok(i) | Err(i) => {
                    let id = self.nodes.len();
                    let kind = if last {
                        Kind::File(file)
                    } else {
                        Kind::Dir {
                            children: Vec::new(),
                            open: !self
                                .collapsed
                                .contains(&[&prefix[..], &path[..end]].concat()[..]),
                        }
                    };
                    self.nodes.push(Node {
                        name: name.into(),
                        parent: dir,
                        kind,
                    });
                    if let Some(children) = self.nodes[dir].children_mut() {
                        children.insert(i, id);
                    }
                    id
                }
            };
            if last {
                if self.leaves.len() <= file {
                    self.leaves.resize(file + 1, ROOT);
                }
                self.leaves[file] = child;
            }
            dir = child;
            end += 1;
        }
        if let Kind::Section { files, .. } = &mut self.nodes[top].kind {
            *files += 1;
        }
        self.stale = true;
    }

    /// The node of `section`, added among the root's children in its
    /// order the first time.
    fn section(&mut self, section: Section) -> usize {
        if let Some(node) = self.sections[section as usize] {
            return node;
        }
        let id = self.nodes.len();
        self.nodes.push(Node {
            name: section.label().as_bytes().into(),
            parent: ROOT,
            kind: Kind::Section {
                section,
                children: Vec::new(),
                open: !self.collapsed.contains(section.label().as_bytes()),
                files: 0,
            },
        });
        let nodes = &self.nodes;
        let at = nodes[ROOT].children().iter().position(
            |&c| matches!(nodes[c].kind, Kind::Section { section: s, .. } if s > section),
        );
        if let Some(children) = self.nodes[ROOT].children_mut() {
            children.insert(at.unwrap_or(children.len()), id);
        }
        self.sections[section as usize] = Some(id);
        id
    }

    /// Tags the files opened at the diff's old side, `files`, with `name`,
    /// the version it is.
    pub(super) fn set_at_old(&mut self, files: HashSet<usize>, name: String) {
        self.at_old = (files, name);
    }

    /// Adds commit entry `file`, named `label`, after the last top-level
    /// row; files inserted from now on go under it.
    pub(super) fn insert_commit(&mut self, file: usize, label: &[u8]) {
        let id = self.nodes.len();
        self.nodes.push(Node {
            name: label.into(),
            parent: ROOT,
            kind: Kind::Commit {
                entry: file,
                children: Vec::new(),
                open: !self.collapsed.contains(label),
            },
        });
        if let Some(children) = self.nodes[ROOT].children_mut() {
            children.push(id);
        }
        if self.leaves.len() <= file {
            self.leaves.resize(file + 1, ROOT);
        }
        self.leaves[file] = id;
        self.base = id;
        self.stale = true;
    }

    /// Builds the tree anew for `files`, keeping what's collapsed. The
    /// selection goes to the caller's current file.
    pub(super) fn rebuild(&mut self, files: &[Entry]) {
        *self = Self {
            collapsed: std::mem::take(&mut self.collapsed),
            filter: std::mem::take(&mut self.filter),
            commented: self.commented.take(),
            at_old: std::mem::take(&mut self.at_old),
            editing: self.editing,
            scroll: self.scroll,
            height: self.height,
            grouped: files.last().is_some_and(Entry::is_unchanged),
            ..Self::default()
        };
        for (i, e) in files.iter().enumerate() {
            if e.is_page() {
                self.page = Some(self.nodes.len());
                self.nodes.push(Node {
                    name: Box::default(),
                    parent: ROOT,
                    kind: Kind::Page(i),
                });
                if self.leaves.len() <= i {
                    self.leaves.resize(i + 1, ROOT);
                }
                self.leaves[i] = self.nodes.len() - 1;
            } else if e.is_commit() {
                self.insert_commit(i, &e.change().path);
            } else {
                self.insert(i, &e.change().path, Section::of(e));
            }
        }
        self.stale = true;
    }

    /// The all-files tree: every file of the version at `side` named
    /// `version`, `paths`, or with `listing` none yet, with the diff's
    /// `files` among them under their path in that version, keeping their
    /// letters and counts and colored by their change. A renamed file is
    /// at its old path in the old version. It takes the fold state,
    /// scroll, filter and selected file of `before`, the all-files tree it
    /// replaces, if any, and its filter's matches while it's of the same
    /// version.
    pub(super) fn all_files(
        paths: Arc<Paths>,
        files: &[Entry],
        (side, version, listing): (Side, String, bool),
        before: Option<Self>,
    ) -> Self {
        let mut listed: Vec<(BString, usize, u8, Role)> = (files.iter().enumerate())
            .filter(|(_, e)| !e.is_commit() && !e.is_unchanged())
            .map(|(i, e)| {
                let change = e.change();
                let path = match side {
                    Side::Old => change.old_path().to_owned(),
                    Side::New => change.path.clone(),
                };
                (path, i, change_rank(&change.status), letter(e).1)
            })
            .collect();
        listed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let mut before = before.filter(|t| t.all.is_some()).unwrap_or_default();
        let selected = before.selected_path();
        let old = before.all.take();
        let same = old.as_ref().is_some_and(|a| a.side == side);
        let (opened, closed, matched) = old.map_or_else(Default::default, |a| {
            (a.opened, a.closed, a.matched.filter(|_| same))
        });
        let mut tree = Self {
            filter: before.filter,
            editing: before.editing,
            scroll: before.scroll,
            height: before.height,
            all: Some(Box::new(AllFiles {
                side,
                paths,
                listing,
                listed,
                loaded: HashSet::new(),
                marks: HashMap::new(),
                opened,
                closed,
                version,
                matched,
                scoring: false,
            })),
            ..Self::default()
        };
        // The review's conversation comes first, as in the diff's tree.
        if files.first().is_some_and(Entry::is_page) {
            tree.page = Some(tree.nodes.len());
            tree.nodes.push(Node {
                name: Box::default(),
                parent: ROOT,
                kind: Kind::Page(0),
            });
            tree.leaves = vec![tree.nodes.len() - 1];
        }
        tree.load(ROOT);
        tree.stale = true;
        if let Some((path, dir)) = selected {
            tree.refresh();
            if let Some(node) = tree.reveal(&path, dir)
                && let Some(row) = tree.rows.iter().position(|r| r.node == node)
            {
                tree.select(row);
            }
        }
        tree
    }

    /// The selected row's path, and whether it's a folder's.
    fn selected_path(&mut self) -> Option<(BString, bool)> {
        self.refresh();
        let node = self.rows.get(self.selected)?.node;
        matches!(
            self.nodes[node].kind,
            Kind::Dir { .. } | Kind::File(_) | Kind::Path(_)
        )
        .then(|| (self.path(node), self.nodes[node].is_dir()))
    }

    /// For the benchmarks: the all-files tree of `paths` with the diff's
    /// `files`, its rows flattened; then with `open` (a folder's path) set,
    /// that folder opened. Returns how many rows it has.
    fn bench(paths: &Arc<Paths>, files: &[Entry], open: Option<&[u8]>) -> usize {
        let mut tree = Self::all_files(
            Arc::clone(paths),
            files,
            (Side::New, "bench".to_owned(), false),
            None,
        );
        tree.refresh();
        if let Some(path) = open
            && let Some(node) = tree.reveal(path, true)
        {
            if let Some(row) = tree.rows.iter().position(|r| r.node == node) {
                tree.select(row);
            }
            tree.set_open(true);
        }
        tree.rows.len()
    }

    /// The path of the file the diff doesn't list at `i` in the
    /// all-files tree's version (see [`Item::Path`]).
    pub(super) fn all_path(&self, i: usize) -> Option<BString> {
        let all = self.all.as_ref()?;
        (i < all.paths.len()).then(|| all.paths.path(i).to_owned())
    }

    /// The all-files tree's version of files, for its filter to score,
    /// and the side it's at; `None` while being listed.
    pub(super) fn all_paths(&self) -> Option<(Arc<Paths>, Side)> {
        let all = self.all.as_ref().filter(|a| !a.listing)?;
        Some((Arc::clone(&all.paths), all.side))
    }

    /// Lets the all-files tree's list of files go, for a tree put aside:
    /// its folds and place stay for when it comes back, listed again.
    pub(super) fn drop_list(&mut self) {
        if let Some(all) = self.all.as_mut() {
            all.paths = Arc::new(Paths::default());
            all.listing = true;
        }
    }

    /// The diff's files the version's list doesn't have, as an untracked
    /// file or one deleted in it: the rest of what the filter scores.
    pub(super) fn all_extras(&self) -> Vec<BString> {
        let Some(all) = self.all.as_ref() else {
            return Vec::new();
        };
        let paths = &all.paths;
        (all.listed.iter())
            .filter(|(path, ..)| {
                let at = lower_bound(0, paths.len(), |i| {
                    paths.path(i).as_bytes() < path.as_slice()
                });
                at == paths.len() || paths.path(at).as_bytes() != path.as_slice()
            })
            .map(|(path, ..)| path.clone())
            .collect()
    }

    /// Makes the children of folder `dir` in the all-files tree, if not
    /// made yet, from the version's files and the diff's under its path;
    /// then those of the folders under it that are open, or the only
    /// folder in it, which shows as one row with it.
    fn load(&mut self, dir: usize) {
        let Some(all) = self.all.as_mut() else {
            return;
        };
        if !all.loaded.insert(dir) {
            return;
        }
        let paths = Arc::clone(&all.paths);
        let listed = std::mem::take(&mut all.listed);
        let mut prefix = self.path(dir).to_vec();
        if dir != ROOT {
            prefix.push(b'/');
        }
        let under = |path: &[u8]| path.starts_with(&prefix);
        let mut p = lower_bound(0, paths.len(), |i| {
            paths.path(i).as_bytes() < prefix.as_slice()
        });
        let mut l = listed.partition_point(|e| e.0.as_slice() < prefix.as_slice());
        let mut folder: Option<usize> = None;
        let mut open = Vec::new();
        loop {
            let from_paths = (p < paths.len())
                .then(|| paths.path(p).as_bytes())
                .filter(|x| under(x));
            let from_diff = listed.get(l).filter(|e| under(&e.0));
            // A file both lists is the diff's.
            let (full, change, at): (&[u8], _, _) = match (from_paths, from_diff) {
                (Some(a), Some(b)) if a == b.0.as_slice() => {
                    (p, l) = (p + 1, l + 1);
                    (b.0.as_slice(), Some(b), None)
                }
                (Some(a), b) if b.is_none_or(|b| a < b.0.as_slice()) => {
                    p += 1;
                    (a, None, Some(p - 1))
                }
                (_, Some(b)) => {
                    l += 1;
                    (b.0.as_slice(), Some(b), None)
                }
                (_, None) => break,
            };
            let rest = &full[prefix.len()..];
            if let Some(slash) = rest.find_byte(b'/') {
                let name = &rest[..slash];
                if at.is_some() {
                    // The rest of the folder's files are its own to make:
                    // past them, `0` being the byte after `/`.
                    let past = [&full[..prefix.len() + slash], b"0"].concat();
                    p = lower_bound(p, paths.len(), |i| {
                        paths.path(i).as_bytes() < past.as_slice()
                    });
                }
                let node = match folder {
                    Some(f) if &self.nodes[f].name[..] == name && self.nodes[f].is_dir() => f,
                    _ => {
                        let path: BString = full[..prefix.len() + slash].into();
                        let opened = self.all.as_ref().is_some_and(|a| a.opened.contains(&path));
                        let f = self.add(
                            dir,
                            name,
                            Kind::Dir {
                                children: Vec::new(),
                                open: opened,
                            },
                        );
                        if opened {
                            open.push(f);
                        }
                        folder = Some(f);
                        f
                    }
                };
                if let (Some(&(_, _, rank, role)), Some(all)) = (change, self.all.as_mut()) {
                    let mark = all.marks.entry(node).or_insert((rank, role));
                    *mark = if rank > mark.0 { (rank, role) } else { *mark };
                }
            } else {
                match (change, at) {
                    (Some(&(_, entry, ..)), _) => self.add(dir, rest, Kind::File(entry)),
                    (None, Some(i)) => self.add(dir, rest, Kind::Path(i)),
                    (None, None) => continue,
                };
            }
        }
        if let Some(all) = self.all.as_mut() {
            all.listed = listed;
        }
        // A lone folder shows as one row with this one, its children too,
        // unless it was folded.
        if let [only] = *self.nodes[dir].children()
            && dir != ROOT
            && self.nodes[only].is_dir()
            && !(self.all.as_ref()).is_some_and(|a| a.closed.contains(&self.path(only)))
            && let Kind::Dir { open: o, .. } = &mut self.nodes[only].kind
        {
            *o = true;
            open.push(only);
        }
        for f in open {
            self.load(f);
        }
    }

    /// Adds a child named `name` to `dir`, after its others; a file of the
    /// diff's is its entry's node.
    fn add(&mut self, dir: usize, name: &[u8], kind: Kind) -> usize {
        let id = self.nodes.len();
        if let Kind::File(entry) = kind {
            if self.leaves.len() <= entry {
                self.leaves.resize(entry + 1, ROOT);
            }
            self.leaves[entry] = id;
        }
        self.nodes.push(Node {
            name: name.into(),
            parent: dir,
            kind,
        });
        if let Some(children) = self.nodes[dir].children_mut() {
            children.push(id);
        }
        id
    }

    /// The node of `path` in the all-files tree, a folder's with `dir`,
    /// making the folders it's in; `None` when nothing is there. A file
    /// and a folder of one name are told apart.
    fn reveal(&mut self, path: &[u8], dir: bool) -> Option<usize> {
        let mut node = ROOT;
        let mut names = path.split_str("/").peekable();
        while let Some(name) = names.next() {
            self.load(node);
            let folder = dir || names.peek().is_some();
            let children = self.nodes[node].children();
            // Children are in git's order, as `cmp_key` compares them.
            let i = children
                .binary_search_by(|&c| self.nodes[c].cmp_key(name, folder))
                .ok()?;
            node = children[i];
        }
        Some(node)
    }

    /// Opens the folders down to the file at `path` in the all-files tree
    /// and selects it, as Zed reveals a file in its project panel.
    /// Returns false when the tree has no file there yet.
    pub(super) fn show_path(&mut self, path: &[u8]) -> bool {
        let Some(node) = self.reveal(path, false) else {
            return false;
        };
        let mut dir = self.nodes[node].parent;
        while dir != ROOT {
            let at = self.path(dir);
            if let Kind::Dir { open, .. } = &mut self.nodes[dir].kind {
                *open = true;
            }
            if let Some(all) = self.all.as_mut() {
                all.closed.remove(&at);
                all.opened.insert(at);
            }
            dir = self.nodes[dir].parent;
        }
        self.stale = true;
        self.refresh();
        if let Some(row) = self.rows.iter().position(|r| r.node == node) {
            self.select(row);
        }
        true
    }

    /// The all-files filter's matches, the best first by path, and how
    /// many there are in all; with `None`, a worker scores them and those
    /// shown stay meanwhile. With `jump`, for a query that changed, the
    /// first match is selected.
    pub(super) fn set_matched(&mut self, matched: Option<(Vec<BString>, usize)>, jump: bool) {
        let Some(all) = self.all.as_mut() else {
            return;
        };
        match matched {
            Some((mut shown, total)) => {
                shown.truncate(FILTER_SHOWN);
                all.matched = Some((shown, total));
                all.scoring = false;
            }
            None => all.scoring = true,
        }
        self.stale = true;
        self.refresh();
        if jump
            && let Some(row) = (self.rows.iter())
                .position(|r| matches!(self.nodes[r.node].kind, Kind::File(_) | Kind::Path(_)))
        {
            self.select(row);
        }
    }

    /// Which nodes the all-files filter keeps: its matches, made with the
    /// folders they're in, and those folders.
    fn all_matches(&mut self) -> Vec<bool> {
        let matched = (self.all.as_ref())
            .and_then(|a| a.matched.as_ref())
            .map(|m| m.0.clone())
            .unwrap_or_default();
        let nodes: Vec<usize> = (matched.iter())
            .filter_map(|path| self.reveal(path, false))
            .collect();
        let mut keep = vec![false; self.nodes.len()];
        for mut node in nodes {
            while !keep[node] {
                keep[node] = true;
                if node == ROOT {
                    break;
                }
                node = self.nodes[node].parent;
            }
        }
        keep
    }

    /// Flattens the visible rows if the tree changed since.
    fn refresh(&mut self) {
        if !self.stale {
            return;
        }
        self.stale = false;
        self.rows.clear();
        self.file_rows.clear();
        self.file_rows.resize(self.leaves.len(), None);
        // A filter typed is for paths, which the page hasn't.
        if let Some(node) = self.page.filter(|_| self.filter.is_empty())
            && let Kind::Page(entry) = self.nodes[node].kind
        {
            self.file_rows[entry] = Some(0);
            self.rows.push(Row {
                node,
                top: node,
                depth: 0,
            });
        }
        let keep = self.filtered().then(|| self.matches());
        // The all-files filter's matches may have made files.
        self.file_rows.resize(self.leaves.len(), None);
        self.flatten(ROOT, 0, keep.as_deref());
        if let Some(node) = self.selected_node {
            self.selected = self.rows.iter().position(|r| r.node == node).unwrap_or(0);
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        self.selected_node = self.rows.get(self.selected).map(|r| r.node);
        self.clamp_scroll();
    }

    /// Which nodes the filter keeps: the files it matches, by their path
    /// below any commit, and the folders and commits above them. Notes
    /// each file's matched characters within its name.
    fn matches(&mut self) -> Vec<bool> {
        if self.all.is_some() {
            return self.all_matches();
        }
        let mut keep = vec![false; self.nodes.len()];
        self.hits.clear();
        self.hits.resize(self.leaves.len(), None);
        for (file, &leaf) in self.leaves.iter().enumerate() {
            if !matches!(self.nodes[leaf].kind, Kind::File(_)) {
                continue;
            }
            let raw = self.file_path(leaf);
            if self.commented.as_ref().is_some_and(|c| !c.contains(&raw)) {
                continue;
            }
            let path = escape(&raw);
            let Some((_, at)) = crate::fuzzy::score(&self.filter, &path) else {
                continue;
            };
            let name_start = path.len() - escape(&self.nodes[leaf].name).len();
            self.hits[file] = Some(
                at.iter()
                    .filter_map(|&i| i.checked_sub(name_start))
                    .collect(),
            );
            let mut node = leaf;
            while !keep[node] {
                keep[node] = true;
                if node == ROOT {
                    break;
                }
                node = self.nodes[node].parent;
            }
        }
        keep
    }

    /// A file node's path, from below its commit or section.
    fn file_path(&self, mut node: usize) -> BString {
        let mut parts = Vec::new();
        while node != ROOT
            && !matches!(
                self.nodes[node].kind,
                Kind::Commit { .. } | Kind::Section { .. }
            )
        {
            parts.push(&self.nodes[node].name[..]);
            node = self.nodes[node].parent;
        }
        parts.reverse();
        parts.join(&b'/').into()
    }

    pub(super) fn filter(&self) -> &str {
        &self.filter
    }

    fn filtered(&self) -> bool {
        // The all-files tree keeps to commented files in no way of its own.
        !self.filter.is_empty() || (self.commented.is_some() && self.all.is_none())
    }

    pub(super) fn commented(&self) -> bool {
        self.commented.is_some()
    }

    /// Keeps only the files at `paths`, those with comments, as well as
    /// the filter keeps them; every file again with `None`. Selects the
    /// first file kept when the selection's is hidden.
    pub(super) fn set_commented(&mut self, paths: Option<HashSet<BString>>) {
        self.commented = paths;
        self.set_filter(&self.filter.clone());
    }

    /// Sets the filter. When it hides the selection, the first file it
    /// keeps is selected.
    pub(super) fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_lowercase();
        if !self.filtered() {
            self.hits.clear();
        }
        self.stale = true;
        let before = self.selected_node;
        self.refresh();
        if self.selected_node != before
            && let Some(row) = self
                .rows
                .iter()
                .position(|r| matches!(self.nodes[r.node].kind, Kind::File(_)))
        {
            self.select(row);
        }
    }

    /// Whether `file` is one the filter keeps (every file without one).
    pub(super) fn in_scope(&mut self, file: usize) -> bool {
        // The all-files filter is for finding a file there, not a scope.
        if !self.filtered() || self.all.is_some() {
            return true;
        }
        self.refresh();
        self.hits.get(file).is_some_and(Option::is_some)
    }

    fn flatten(&mut self, dir: usize, depth: usize, keep: Option<&[bool]>) {
        for i in 0..self.nodes[dir].children().len() {
            let child = self.nodes[dir].children()[i];
            if keep.is_some_and(|k| !k[child]) {
                continue;
            }
            match self.nodes[child].kind {
                Kind::File(file) => {
                    self.file_rows[file] = Some(self.rows.len());
                    self.rows.push(Row {
                        node: child,
                        top: child,
                        depth,
                    });
                }
                Kind::Path(_) => self.rows.push(Row {
                    node: child,
                    top: child,
                    depth,
                }),
                Kind::Commit { entry, open, .. } => {
                    let row = self.rows.len();
                    self.file_rows[entry] = Some(row);
                    self.rows.push(Row {
                        node: child,
                        top: child,
                        depth,
                    });
                    // A filter shows what it keeps, folded or not.
                    if open || keep.is_some() {
                        self.flatten(child, depth + 1, keep);
                    } else {
                        self.hide(child, row);
                    }
                }
                Kind::Section { open, .. } => {
                    let row = self.rows.len();
                    self.rows.push(Row {
                        node: child,
                        top: child,
                        depth,
                    });
                    // A heading over its files rather than a folder: they
                    // keep the tree's width.
                    if open || keep.is_some() {
                        self.flatten(child, depth, keep);
                    } else {
                        self.hide(child, row);
                    }
                }
                Kind::Dir { .. } => {
                    let mut bottom = child;
                    while let [only] = *self.nodes[bottom].children()
                        && self.nodes[only].is_dir()
                        && keep.is_none_or(|k| k[only])
                    {
                        bottom = only;
                    }
                    let row = self.rows.len();
                    self.rows.push(Row {
                        node: bottom,
                        top: child,
                        depth,
                    });
                    let open = matches!(self.nodes[bottom].kind, Kind::Dir { open: true, .. });
                    if open || keep.is_some() {
                        self.flatten(bottom, depth + 1, keep);
                    } else {
                        self.hide(bottom, row);
                    }
                }
                Kind::Page(_) => {}
            }
        }
    }

    /// Points the files under a collapsed `dir` at its row.
    fn hide(&mut self, dir: usize, row: usize) {
        let mut stack = vec![dir];
        while let Some(node) = stack.pop() {
            match &self.nodes[node].kind {
                Kind::File(file) => self.file_rows[*file] = Some(row),
                Kind::Dir { children, .. }
                | Kind::Commit { children, .. }
                | Kind::Section { children, .. } => {
                    stack.extend(children);
                }
                Kind::Path(_) | Kind::Page(_) => {}
            }
        }
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.min(self.rows.len().saturating_sub(self.height));
    }

    /// Selects `row`, scrolling only as far as keeps it `SCROLLOFF` rows
    /// from the edges.
    fn select(&mut self, row: usize) {
        let Some(r) = self.rows.get(row) else {
            return;
        };
        self.selected = row;
        self.selected_node = Some(r.node);
        let margin = SCROLLOFF.min(self.height.saturating_sub(1) / 2);
        if row < self.scroll + margin {
            self.scroll = row.saturating_sub(margin);
        } else if row + margin >= self.scroll + self.height {
            self.scroll = row + margin + 1 - self.height;
        }
        self.clamp_scroll();
    }

    /// Moves the selection to the row of `file` if it isn't there already.
    pub(super) fn follow(&mut self, file: usize) {
        self.refresh();
        if let Some(Some(row)) = self.file_rows.get(file).copied()
            && row != self.selected
        {
            self.select(row);
        }
    }

    /// The files of rows `from..to`, as far as there are rows.
    fn files_in(&self, from: usize, to: usize) -> impl Iterator<Item = usize> + '_ {
        let to = to.min(self.rows.len());
        self.rows[from.min(to)..to]
            .iter()
            .filter_map(|r| match self.nodes[r.node].kind {
                Kind::File(file) => Some(file),
                Kind::Dir { .. }
                | Kind::Path(_)
                | Kind::Commit { .. }
                | Kind::Page(_)
                | Kind::Section { .. } => None,
            })
    }

    /// The first row the panel showed last.
    pub(super) fn scroll(&self) -> usize {
        self.scroll
    }

    /// The files on the rows the panel showed last.
    pub(super) fn files_on_screen(&self) -> impl Iterator<Item = usize> + '_ {
        self.files_in(self.scroll, self.scroll + self.height)
    }

    /// [`Tree::files_on_screen`], then the files of a panel's height of
    /// rows below them and above them, where a scroll goes next.
    pub(super) fn files_near_screen(&self) -> Vec<usize> {
        let (top, height) = (self.scroll, self.height);
        self.files_on_screen()
            .chain(self.files_in(top + height, top + 2 * height))
            .chain(self.files_in(top.saturating_sub(height), top))
            .collect()
    }

    pub(super) fn item(&self) -> Option<Item> {
        let row = self.rows.get(self.selected)?;
        Some(match self.nodes[row.node].kind {
            Kind::File(file) | Kind::Page(file) => Item::File(file),
            Kind::Path(i) => Item::Path(i),
            Kind::Dir { open, .. } | Kind::Section { open, .. } => Item::Dir { open },
            Kind::Commit { entry, open, .. } => Item::Commit { file: entry, open },
        })
    }

    /// Moves the selection by `delta` rows and returns what it lands on.
    pub(super) fn step(&mut self, delta: isize) -> Option<Item> {
        self.refresh();
        let last = self.rows.len().checked_sub(1)?;
        let row = self.selected.saturating_add_signed(delta).min(last);
        self.select(row);
        self.item()
    }

    /// Moves by a panel's height.
    pub(super) fn page(&mut self, down: bool) -> Option<Item> {
        let delta = isize::try_from(self.height).unwrap_or(isize::MAX);
        self.step(if down { delta } else { -delta })
    }

    pub(super) fn set_open(&mut self, open: bool) {
        self.refresh();
        let Some(row) = self.rows.get(self.selected).copied() else {
            return;
        };
        let path = self.path(row.node);
        if let Kind::Dir { open: o, .. }
        | Kind::Commit { open: o, .. }
        | Kind::Section { open: o, .. } = &mut self.nodes[row.node].kind
        {
            *o = open;
            if let Some(all) = self.all.as_mut() {
                // Folders start folded here: the opened ones are kept.
                if open {
                    all.closed.remove(&path);
                    all.opened.insert(path);
                    self.load(row.node);
                } else {
                    all.opened.remove(&path);
                    all.closed.insert(path);
                }
            } else if open {
                self.collapsed.remove(&path);
            } else {
                self.collapsed.insert(path);
            }
            self.stale = true;
            self.refresh();
        }
    }

    /// Selects the directory holding the selected row.
    pub(super) fn select_parent(&mut self) {
        self.refresh();
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let parent = self.nodes[row.top].parent;
        if let Some(r) = self.rows[..self.selected]
            .iter()
            .rposition(|r| r.node == parent)
        {
            self.select(r);
        }
    }

    /// A node's path, its name among collapsed ones: from below its
    /// section, so a folder folds the same with or without the section's
    /// heading; a section's own is its heading.
    fn path(&self, mut node: usize) -> BString {
        let mut parts = Vec::new();
        let start = node;
        while node != ROOT {
            if node == start || !matches!(self.nodes[node].kind, Kind::Section { .. }) {
                parts.push(&self.nodes[node].name[..]);
            }
            node = self.nodes[node].parent;
        }
        parts.reverse();
        parts.join(&b'/').into()
    }

    /// The panel's height, borders included, for scrolling before it's
    /// first drawn.
    pub(super) fn set_height(&mut self, height: usize) {
        self.height = height.saturating_sub(2).max(1);
    }

    /// Scrolls the panel without moving the selection.
    pub(super) fn scroll_by(&mut self, delta: isize) {
        self.scroll = self.scroll.saturating_add_signed(delta);
        self.clamp_scroll();
    }

    /// The row at panel line `y` (0 is the first row below the border).
    pub(super) fn click(&mut self, y: usize) -> Option<Item> {
        self.refresh();
        let row = self.scroll + y;
        if row >= self.rows.len() || y >= self.height {
            return None;
        }
        self.select(row);
        self.item()
    }

    /// Draws the panel into columns `0..width` of rows `top..top + height`.
    /// With `joined`, its right edge is the divider of a frame going on
    /// to the right, and its corners there are `┬` and `┴`.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site, each a distinct input"
    )]
    pub(super) fn draw(
        &mut self,
        grid: &mut Grid,
        (top, height, width): (usize, usize, usize),
        files: &[Entry],
        totals: &Totals,
        comments: Option<&Comments>,
        listing: bool,
        focused: bool,
        joined: bool,
        theme: &Theme,
    ) {
        if height < 3 || width < 4 {
            return;
        }
        self.height = height - 2;
        self.refresh();
        self.clamp_scroll();
        self.draw_frame(
            grid,
            (top, height, width),
            totals,
            listing,
            focused,
            joined,
            theme,
        );
        for (y, row) in self
            .rows
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(self.height)
        {
            let line = top + 1 + y - self.scroll;
            let selected = (y == self.selected).then_some(focused);
            self.draw_row(
                grid,
                line,
                *row,
                selected,
                (files, totals, comments),
                width - 1,
                theme,
            );
        }
    }

    /// The border, with the title and totals in its top edge.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site, each a distinct input"
    )]
    fn draw_frame(
        &self,
        grid: &mut Grid,
        place: (usize, usize, usize),
        totals: &Totals,
        listing: bool,
        focused: bool,
        joined: bool,
        theme: &Theme,
    ) {
        let growing = if listing { "+" } else { "" };
        let parts = if let Some(all) = &self.all {
            let mut parts = vec![
                (" All files".to_owned(), Role::Plain),
                (format!(" · {}", all.version), Role::Dim),
            ];
            if all.listing {
                parts.push((" · listing…".to_owned(), Role::Dim));
            }
            if !self.filter.is_empty() {
                let count = match &all.matched {
                    _ if all.scoring => " · scoring…".to_owned(),
                    Some((shown, total)) if shown.len() < *total => {
                        format!(" · {}/{}", shown.len(), total)
                    }
                    Some((_, total)) => format!(" · {total}"),
                    None => " · scoring…".to_owned(),
                };
                parts.push((count, Role::Dim));
                parts.push((format!(" · {}", self.filter), Role::Hint));
            }
            parts
        } else if self.filtered() {
            let kept = self.hits.iter().filter(|h| h.is_some()).count();
            let mut parts = vec![
                (" Files".to_owned(), Role::Plain),
                (format!(" · {kept}/{}{growing}", totals.files), Role::Dim),
            ];
            if self.commented.is_some() {
                parts.push((" · with comments".to_owned(), Role::Hint));
            }
            if !self.filter.is_empty() {
                parts.push((format!(" · {}", self.filter), Role::Hint));
            }
            parts
        } else {
            vec![
                (" Files".to_owned(), Role::Plain),
                (format!(" · {}{growing}", totals.files), Role::Dim),
                (format!(" +{}", totals.added), Role::Added),
                (format!(" -{}", totals.removed), Role::Removed),
            ]
        };
        let typing = self
            .editing
            .then_some((self.filter.as_str(), "type to filter · esc clear "));
        frame(grid, place, &parts, typing, (focused, joined), theme);
    }

    /// `row` at screen line `line`, its counts in the column `totals`
    /// sizes, ending at column `end`; a file with comments has their count
    /// just left of that column. A selected row (`Some`, whether the tree
    /// has the keys) takes the theme's selection background, its name in
    /// bold; letters and counts keep their colors.
    #[expect(
        clippy::too_many_arguments,
        reason = "one call site, each a distinct input"
    )]
    fn draw_row(
        &self,
        grid: &mut Grid,
        line: usize,
        row: Row,
        selected: Option<bool>,
        (files, totals, comments): (&[Entry], &Totals, Option<&Comments>),
        end: usize,
        theme: &Theme,
    ) {
        let inner = end - 1;
        if let Some(focused) = selected {
            grid.fill(1, line, end, theme.selected(Style::PLAIN, focused));
        }
        let style = |role, dim: bool| {
            let s = theme.style(if dim { Role::Dim } else { role });
            selected.map_or(s, |focused| theme.selected(s, focused))
        };
        let named = |role, dim: bool| {
            let mut s = style(role, dim);
            if selected.is_some() {
                s.attributes |= Attributes::BOLD;
            }
            s
        };
        let indent = (row.depth * 2).min(inner / 2);
        let x = 2 + indent;
        match self.nodes[row.node].kind {
            Kind::Page(_) => {
                let x = grid.put_str(x, line, end, "≡ ", style(Role::Dim, false));
                let count = comments.map_or(0, Comments::conversation_len).to_string();
                let at = end.saturating_sub(count.width() + 1);
                grid.put_str(x, line, at, "Conversation", named(Role::Plain, false));
                if at > x {
                    grid.put_str(at, line, end, &count, style(Role::Hint, false));
                }
            }
            Kind::Dir { open, .. } | Kind::Section { open, .. } | Kind::Commit { open, .. } => {
                let glyph = if open { "▼ " } else { "▶ " };
                let x = grid.put_str(x, line, end, glyph, style(Role::Dim, false));
                if let Kind::Commit { .. } = self.nodes[row.node].kind {
                    let label = escape(&self.nodes[row.node].name);
                    let (id, subject) = label.split_once(' ').unwrap_or((&label, ""));
                    let x = grid.put_str(x, line, end, id, style(Role::CommitHeader, false));
                    let subject = fit_right(subject, end.saturating_sub(x + 2));
                    grid.put_str(x + 1, line, end, &subject, named(Role::Plain, false));
                } else {
                    let (label, role) = self.group_label(row, end.saturating_sub(x + 1));
                    grid.put_str(x, line, end, &label, named(role, false));
                }
            }
            Kind::File(file) => {
                let entry = &files[file];
                let dim = entry.viewed();
                let (letter, role) = letter(entry);
                let x = grid.put_str(x, line, end, letter, style(role, dim)) + 1;
                let name = escape(&self.nodes[row.node].name);
                let (mut stats_x, counts) = place_counts(entry.counts(), (x, end), totals);
                if let Some((n, look)) = comments.and_then(|c| c.tally(&entry.change().path)) {
                    let tag = n.to_string();
                    let at = stats_x.saturating_sub(tag.width() + 1);
                    if at > x {
                        grid.put_str(at, line, stats_x, &tag, style(look_role(look), false));
                        stats_x = at;
                    }
                }
                let (at_old, old) = &self.at_old;
                if at_old.contains(&file) {
                    let at = stats_x.saturating_sub(old.width() + 1);
                    if at > x {
                        grid.put_str(at, line, stats_x, old, style(Role::Dim, false));
                        stats_x = at;
                    }
                }
                let room = stats_x.saturating_sub(x + 1);
                let shown = fit_left(&name, room);
                // The filter's matches, when the name shows whole.
                let hits = self
                    .hits
                    .get(file)
                    .and_then(Option::as_ref)
                    .filter(|_| shown == name);
                // The all-files tree tells changed files apart by color.
                let name_role = self.all.as_ref().map_or(Role::Plain, |_| role);
                fuzzy::put_marked(
                    grid,
                    (x, line, stats_x),
                    &shown,
                    hits.map_or(&[][..], Vec::as_slice),
                    (named(name_role, dim), named(Role::Hint, dim)),
                );
                for (text, role, at) in counts {
                    grid.put_str(at, line, end, &text, style(role, dim));
                }
            }
            Kind::Path(_) => {
                let name = escape(&self.nodes[row.node].name);
                let x = x + 2;
                let shown = fit_left(&name, end.saturating_sub(x + 1));
                grid.put_str(x, line, end, &shown, named(Role::Plain, false));
            }
        }
    }

    /// A folder's or a section's row label within `room` columns: a
    /// folder's path, cut from the left, or a section's heading and count.
    fn group_label(&self, row: Row, room: usize) -> (String, Role) {
        if let Kind::Section { section, files, .. } = self.nodes[row.node].kind {
            // Short of room the words are cut, never the count.
            let count = format!(" ({files})");
            let words = fit_right(section.label(), room.saturating_sub(count.len()));
            return (words + &count, Role::Dim);
        }
        let mut label = escape(&self.path_between(row.top, row.node));
        label.push('/');
        // In the all-files tree, the strongest change inside colors it.
        let mark = (self.all.as_ref())
            .and_then(|a| a.marks.get(&row.node).or_else(|| a.marks.get(&row.top)));
        (
            fit_left(&label, room),
            mark.map_or(Role::Plain, |&(_, role)| role),
        )
    }

    /// The names from directory `top` down to `bottom`, joined by `/`.
    fn path_between(&self, top: usize, bottom: usize) -> BString {
        let mut parts = Vec::new();
        let mut node = bottom;
        loop {
            parts.push(&self.nodes[node].name[..]);
            if node == top {
                break;
            }
            node = self.nodes[node].parent;
        }
        parts.reverse();
        parts.join(&b'/').into()
    }
}

/// Draws the left panel's border into columns `0..width` of rows
/// `top..top + height`: `parts` of its title in the top edge, whole ones
/// only, the last dropped first, since a number cut short would read as a
/// smaller one; `typing`, what's typed and a hint shown while it's empty,
/// in the bottom edge. With `joined`, its right edge is the divider of a
/// frame going on to the right, and its corners there are `┬` and `┴`.
pub(super) fn frame(
    grid: &mut Grid,
    (top, height, width): (usize, usize, usize),
    parts: &[(String, Role)],
    typing: Option<(&str, &str)>,
    (focused, joined): (bool, bool),
    theme: &Theme,
) {
    let border = theme.style(if focused { Role::Hint } else { Role::Dim });
    let bottom = top + height - 1;
    for y in top + 1..bottom {
        grid.put_str(0, y, width, "│", border);
        grid.put_str(width - 1, y, width, "│", border);
    }
    let (top_right, bottom_right) = if joined {
        ("┬", "┴")
    } else {
        ("┐", "┘")
    };
    for (y, left, right) in [(top, "┌", top_right), (bottom, "└", bottom_right)] {
        grid.put_str(0, y, width, left, border);
        for x in 1..width - 1 {
            grid.put_str(x, y, width, "─", border);
        }
        grid.put_str(width - 1, y, width, right, border);
    }
    let end = width - 1;
    let room = end.saturating_sub(2);
    let mut shown = parts.len();
    while shown > 1 && parts[..shown].iter().map(|(t, _)| t.width()).sum::<usize>() > room {
        shown -= 1;
    }
    let mut x = 1;
    for (text, role) in &parts[..shown] {
        x = grid.put_str(x, top, end, text, theme.style(*role));
    }
    grid.put_str(x, top, end, " ", theme.style(Role::Plain));
    if let Some((typed, hint)) = typing {
        // Right of the `«` the viewer puts in the corner.
        let text = format!(" /{typed}▏ ");
        let x = grid.put_str(2, bottom, end, &text, theme.style(Role::Hint));
        if typed.is_empty() {
            grid.put_str(x, bottom, end, hint, theme.style(Role::Dim));
        }
    }
}

/// `text` within `max` columns, cut on the right.
pub(super) fn fit_right(text: &str, max: usize) -> String {
    if text.width() <= max {
        return text.to_owned();
    }
    let mut used = 1;
    let mut out: String = text
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= max
        })
        .collect();
    if max > 0 {
        out.push('…');
    }
    out
}

/// `path` within `max` columns, cut from the left so the file name stays.
pub(super) fn fit_left(path: &str, max: usize) -> String {
    if path.width() <= max {
        return path.to_owned();
    }
    let mut kept = Vec::new();
    let mut used = 1;
    for c in path.chars().rev() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        used += w;
        kept.push(c);
    }
    std::iter::once('…').chain(kept.into_iter().rev()).collect()
}

#[cfg(test)]
mod tests {
    use dffy_core::document::{Document, Source};
    use dffy_core::git::{FileChange, Mode, Side, Status};

    use super::*;
    use crate::tui::piped::Body;

    fn change(path: &str, status: Status) -> FileChange {
        let side = Some(Side::Worktree {
            mode: Mode::File,
            id: None,
        });
        let (old, new) = match status {
            Status::Added | Status::Untracked => (None, side),
            Status::Deleted => (side, None),
            _ => (side, side),
        };
        FileChange {
            path: path.into(),
            status,
            old,
            new,
            staging: None,
        }
    }

    /// A path, its status and its counts when known.
    type Listed<'a> = (&'a str, Status, Option<(usize, usize)>);

    fn files(list: &[Listed]) -> Document<Body> {
        let mut doc = Document::default();
        for (i, (path, status, counts)) in list.iter().enumerate() {
            doc.push(change(path, status.clone()), Source::Repo);
            if let Some((added, removed)) = *counts {
                doc.set_counts(i, Counts { added, removed });
            }
        }
        doc
    }

    fn draw(tree: &mut Tree, files: &[Entry], height: usize) -> String {
        let mut grid = Grid::new(34, height);
        let totals = Totals::of(files, false);
        tree.draw(
            &mut grid,
            (0, height, 34),
            files,
            &totals,
            None,
            false,
            true,
            false,
            &Theme::default_theme(),
        );
        (0..height)
            .map(|y| grid.row_text(y).trim_end().to_owned())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn sample(counted: bool) -> Document<Body> {
        let n = |a, r| counted.then_some((a, r));
        files(&[
            ("README.md", Status::Modified, n(3, 1)),
            ("crates/dffy/src/tui/app.rs", Status::Modified, n(120, 45)),
            ("crates/dffy/src/tui/tree.rs", Status::Added, n(480, 0)),
            ("crates/dffy/src/tui/sidebar.rs", Status::Deleted, n(0, 119)),
            ("docs/guide.md", Status::Untracked, n(12, 0)),
        ])
    }

    fn tree_of(files: &[Entry]) -> Tree {
        let mut tree = Tree::default();
        for (i, e) in files.iter().enumerate() {
            tree.insert(i, &e.change().path, Section::of(e));
        }
        tree
    }

    #[test]
    fn single_child_folders_compact_and_directories_collapse() {
        let files_doc = sample(true);
        let files = files_doc.files();
        let mut tree = tree_of(files);
        insta::assert_snapshot!(draw(&mut tree, files, 10), @r"
        ┌ Files · 5 +615 -165 ───────────┐
        │ M README.md            +3   -1 │
        │ ▼ crates/dffy/src/tui/         │
        │   M app.rs           +120  -45 │
        │   D sidebar.rs            -119 │
        │   A tree.rs          +480      │
        │ ▼ docs/                        │
        │   ? guide.md          +12      │
        │                                │
        └────────────────────────────────┘
        ");
        // Down to the chain's row, then collapse it.
        tree.step(1);
        tree.set_open(false);
        insta::assert_snapshot!(draw(&mut tree, files, 10), @r"
        ┌ Files · 5 +615 -165 ───────────┐
        │ M README.md            +3   -1 │
        │ ▶ crates/dffy/src/tui/         │
        │ ▼ docs/                        │
        │   ? guide.md          +12      │
        │                                │
        │                                │
        │                                │
        │                                │
        └────────────────────────────────┘
        ");
        tree.set_open(true);
        assert_eq!(tree.step(1), Some(Item::File(1)));
        tree.select_parent();
        assert_eq!(tree.item(), Some(Item::Dir { open: true }));
        assert_eq!(tree.selected, 1);
    }

    #[test]
    fn a_large_count_arriving_mid_pass_moves_nothing() {
        let mut files = sample(false);
        let mut tree = tree_of(files.files());
        let before = draw(&mut tree, files.files(), 10);
        // app.rs, counted while the others aren't yet: shown in thousands
        // within the placeholder.
        files.set_counts(
            1,
            Counts {
                added: 12345,
                removed: 0,
            },
        );
        let after = draw(&mut tree, files.files(), 10);
        for (a, b) in before.lines().zip(after.lines()).skip(1) {
            if b.contains("app.rs") {
                assert_eq!(b, "│   M app.rs           +12k      │");
                assert_eq!(a, "│   M app.rs                     │");
            } else {
                assert_eq!(a, b);
            }
        }
    }

    /// A folded folder stays folded once a file opened outside the diff
    /// puts the diff's files under a heading, and once it's gone again.
    #[test]
    fn a_folded_folder_stays_folded_with_or_without_the_heading() {
        let mut doc = sample(true);
        let mut tree = tree_of(doc.files());
        // The chain's row, folded.
        tree.step(1);
        tree.set_open(false);
        let side = Side::Worktree {
            mode: Mode::File,
            id: None,
        };
        let opened = doc.show_unchanged(FileChange::unchanged("notes.txt".into(), side));
        tree.rebuild(doc.files());
        let shown = draw(&mut tree, doc.files(), 12);
        assert!(shown.contains("Changed in this diff (5)"), "{shown}");
        assert!(shown.contains("▶ crates/dffy/src/tui/"), "{shown}");
        assert!(doc.close_unchanged(opened));
        tree.rebuild(doc.files());
        let shown = draw(&mut tree, doc.files(), 12);
        assert!(!shown.contains("Changed in this diff"), "{shown}");
        assert!(shown.contains("▶ crates/dffy/src/tui/"), "{shown}");
    }

    fn version(paths: &[&str]) -> Arc<Paths> {
        let mut list = Paths::default();
        for path in paths {
            list.push(path.as_bytes());
        }
        Arc::new(list)
    }

    fn all_tree(paths: &[&str], files: &[Entry], old: bool) -> Tree {
        let side = if old {
            dffy_core::document::Side::Old
        } else {
            dffy_core::document::Side::New
        };
        Tree::all_files(version(paths), files, (side, "v".to_owned(), false), None)
    }

    /// The tree's rows as drawn, the border's left and right edges cut.
    fn rows(tree: &mut Tree, files: &[Entry], height: usize) -> Vec<String> {
        (draw(tree, files, height).lines())
            .skip(1)
            .map(|l| {
                l.trim_start_matches('│')
                    .trim_end_matches('│')
                    .trim()
                    .to_owned()
            })
            .filter(|l| !l.is_empty() && !l.starts_with('└'))
            .collect()
    }

    /// A renamed file is at its new path in the new version and at its old
    /// one in the old version, once each.
    #[test]
    fn all_files_put_a_renamed_file_at_its_path_in_each_version() {
        let mut renamed = change("b.txt", Status::Modified);
        renamed.status = Status::Renamed {
            from: "a.txt".into(),
            similarity: 90,
        };
        let mut doc = Document::<Body>::default();
        doc.push(renamed, Source::Repo);
        let files = doc.files();
        let new = rows(
            &mut all_tree(&["b.txt", "keep.txt"], files, false),
            files,
            8,
        );
        assert_eq!(new, ["R b.txt", "keep.txt"]);
        let old = rows(&mut all_tree(&["a.txt", "keep.txt"], files, true), files, 8);
        assert_eq!(old, ["R a.txt", "keep.txt"]);
    }

    /// A file deleted in the diff, gone from the version's list, still
    /// shows; so does an untracked one, in a folder only it makes.
    #[test]
    fn all_files_show_the_diffs_files_the_version_lacks() {
        let files_doc = files(&[
            ("gone.txt", Status::Deleted, None),
            ("new/x.rs", Status::Untracked, None),
        ]);
        let files = files_doc.files();
        let mut tree = all_tree(&["keep.txt"], files, false);
        assert_eq!(
            rows(&mut tree, files, 8),
            ["D gone.txt", "keep.txt", "▶ new/"]
        );
        assert!(tree.show_path(b"new/x.rs"));
        assert_eq!(
            rows(&mut tree, files, 8),
            ["D gone.txt", "keep.txt", "▼ new/", "? x.rs"]
        );
    }

    /// A file deleted where a folder of the same name now is: both show,
    /// each found by what it is.
    #[test]
    fn all_files_tell_a_file_from_a_folder_of_its_name() {
        let files_doc = files(&[
            ("foo", Status::Deleted, None),
            ("foo/bar", Status::Added, None),
        ]);
        let files = files_doc.files();
        let mut tree = all_tree(&["foo/bar"], files, false);
        assert_eq!(rows(&mut tree, files, 8), ["D foo", "▶ foo/"]);
        assert!(tree.show_path(b"foo/bar"));
        assert_eq!(rows(&mut tree, files, 8), ["D foo", "▼ foo/", "A bar"]);
    }

    /// The filter shows its best 1,000 matches and counts them all.
    #[test]
    fn all_files_show_a_thousand_matches_and_count_the_rest() {
        let names: Vec<String> = (0..1001).map(|i| format!("f{i:04}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let doc = Document::<Body>::default();
        let mut tree = all_tree(&refs, doc.files(), false);
        tree.set_filter("f");
        let matched = names.iter().map(|n| BString::from(n.as_str())).collect();
        tree.set_matched(Some((matched, 1001)), true);
        tree.refresh();
        assert_eq!(tree.rows.len(), 1000);
        let shown = draw(&mut tree, doc.files(), 6);
        assert!(shown.contains("1000/1001"), "{shown}");
    }

    /// A lone folder opens with its parent as one row; folded, it stays
    /// folded once the tree is built again.
    #[test]
    fn all_files_keep_a_folded_chain_folded_across_a_build() {
        let doc = Document::<Body>::default();
        let files = doc.files();
        let mut tree = all_tree(&["a/b/c.txt", "z.txt"], files, false);
        assert_eq!(rows(&mut tree, files, 8), ["▶ a/", "z.txt"]);
        tree.set_open(true);
        assert_eq!(rows(&mut tree, files, 8), ["▼ a/b/", "c.txt", "z.txt"]);
        tree.set_open(false);
        let before = Some(tree);
        let mut tree = Tree::all_files(
            version(&["a/b/c.txt", "z.txt"]),
            files,
            (dffy_core::document::Side::New, "v".to_owned(), false),
            before,
        );
        assert_eq!(rows(&mut tree, files, 8), ["▶ a/b/", "z.txt"]);
    }

    #[test]
    fn counts_arriving_never_move_names() {
        let unknown_doc = sample(false);
        let unknown = unknown_doc.files();
        let known_doc = sample(true);
        let known = known_doc.files();
        let mut tree = tree_of(known);
        let (before, after) = (draw(&mut tree, unknown, 10), draw(&mut tree, known, 10));
        for (a, b) in before.lines().zip(after.lines()).skip(1) {
            // Everything left of the counts column is the same.
            let cut = |s: &str| s.chars().take(20).collect::<String>();
            assert_eq!(cut(a), cut(b));
        }
        insta::assert_snapshot!(before, @r"
        ┌ Files · 5 +0 -0 ───────────────┐
        │ M README.md                    │
        │ ▼ crates/dffy/src/tui/         │
        │   M app.rs                     │
        │   D sidebar.rs                 │
        │   A tree.rs                    │
        │ ▼ docs/                        │
        │   ? guide.md                   │
        │                                │
        └────────────────────────────────┘
        ");
    }

    #[test]
    fn a_selected_row_leaves_a_zero_count_blank_in_the_row_style() {
        let files_doc = sample(true);
        let files = files_doc.files();
        let mut tree = tree_of(files);
        // tree.rs: +480, no deletions.
        tree.step(4);
        let mut grid = Grid::new(34, 10);
        let theme = Theme::default_theme();
        let totals = Totals::of(files, false);
        tree.draw(
            &mut grid,
            (0, 10, 34),
            files,
            &totals,
            None,
            false,
            true,
            false,
            &theme,
        );
        let row = grid.row_text(5);
        assert_eq!(row.trim_end(), "│   A tree.rs          +480      │");
        let selected = theme.selected(Style::PLAIN, true);
        for x in 27..33 {
            assert_eq!(grid.row(5)[x].style(), selected, "column {x} of {row:?}");
        }
    }

    /// The selected row's cells as runs of one style: the text, then its
    /// foreground, background and attributes.
    fn style_runs(grid: &Grid, y: usize) -> String {
        let color = |c: Option<crate::theme::Color>| match c {
            None => "-".to_owned(),
            Some(crate::theme::Color::Ansi(n)) => format!("ansi{n}"),
            Some(crate::theme::Color::Rgb(r, g, b)) => format!("#{r:02x}{g:02x}{b:02x}"),
        };
        let mut runs: Vec<(String, Style)> = Vec::new();
        for cell in grid.row(y) {
            match runs.last_mut() {
                Some((text, style)) if *style == cell.style() => text.push_str(cell.text()),
                _ => runs.push((cell.text().to_owned(), cell.style())),
            }
        }
        runs.iter()
            .map(|(text, s)| {
                format!(
                    "{text:?} fg {} bg {} attributes {}",
                    color(s.fg),
                    color(s.bg),
                    s.attributes.0
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Focused or not, the selected row is one background across, the
    /// name bold, and the letter and counts in their own colors, never
    /// blocks of color.
    #[test]
    fn the_selected_row_lifts_its_background_and_keeps_letter_and_count_colors() {
        let files_doc = sample(true);
        let files = files_doc.files();
        let mut tree = tree_of(files);
        tree.step(4);
        let source = crate::theme::dump("dracula").expect("built-in");
        let theme = Theme::parse("dracula", &source, &mut Vec::new());
        let totals = Totals::of(files, false);
        let rows: Vec<String> = [true, false]
            .into_iter()
            .map(|focused| {
                let mut grid = Grid::new(34, 10);
                tree.draw(
                    &mut grid,
                    (0, 10, 34),
                    files,
                    &totals,
                    None,
                    false,
                    focused,
                    false,
                    &theme,
                );
                style_runs(&grid, 5)
            })
            .collect();
        insta::assert_snapshot!(rows.join("\n\n"), @r#"
        "│" fg #8be9fd bg - attributes 0
        "   " fg - bg #363843 attributes 0
        "A" fg #50fa7b bg #363843 attributes 0
        " " fg - bg #363843 attributes 0
        "tree.rs" fg - bg #363843 attributes 1
        "          " fg - bg #363843 attributes 0
        "+480" fg #50fa7b bg #363843 attributes 0
        "      " fg - bg #363843 attributes 0
        "│" fg #8be9fd bg - attributes 0

        "│" fg #6272a4 bg - attributes 0
        "   " fg - bg #2f313d attributes 0
        "A" fg #50fa7b bg #2f313d attributes 0
        " " fg - bg #2f313d attributes 0
        "tree.rs" fg - bg #2f313d attributes 1
        "          " fg - bg #2f313d attributes 0
        "+480" fg #50fa7b bg #2f313d attributes 0
        "      " fg - bg #2f313d attributes 0
        "│" fg #6272a4 bg - attributes 0
        "#);
    }

    #[test]
    fn the_filter_keeps_matching_files_and_their_folders() {
        let files_doc = sample(true);
        let files = files_doc.files();
        let mut tree = tree_of(files);
        tree.set_filter("TREE");
        insta::assert_snapshot!(draw(&mut tree, files, 6), @r"
        ┌ Files · 1/5 · tree ────────────┐
        │ ▼ crates/dffy/src/tui/         │
        │   A tree.rs          +480      │
        │                                │
        │                                │
        └────────────────────────────────┘
        ");
    }

    #[test]
    fn a_title_too_long_drops_whole_totals_deletions_first() {
        let files_doc = files(&[("a.rs", Status::Modified, Some((1_234_567, 7_654_321)))]);
        let files = files_doc.files();
        let mut tree = tree_of(files);
        let titles: Vec<String> = [34, 24, 20, 12]
            .into_iter()
            .map(|width| {
                let mut grid = Grid::new(width, 4);
                let totals = Totals::of(files, false);
                let theme = Theme::default_theme();
                tree.draw(
                    &mut grid,
                    (0, 4, width),
                    files,
                    &totals,
                    None,
                    false,
                    true,
                    false,
                    &theme,
                );
                grid.row_text(0)
            })
            .collect();
        insta::assert_snapshot!(titles.join("\n"), @r"
        ┌ Files · 1 +1234567 -7654321 ───┐
        ┌ Files · 1 +1234567 ──┐
        ┌ Files · 1 ───────┐
        ┌ Files ───┐
        ");
    }

    #[test]
    fn long_paths_keep_their_file_name() {
        assert_eq!(fit_left("src/lib.rs", 20), "src/lib.rs");
        assert_eq!(
            fit_left("drivers/net/wireless/rtw89/table.c", 15),
            "…/rtw89/table.c"
        );
    }
}

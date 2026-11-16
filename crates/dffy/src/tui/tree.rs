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
use std::collections::HashSet;

use dffy_core::bstr::{BString, ByteSlice};
use dffy_core::document::Counts;
use dffy_core::view::Attributes;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::fuzzy;
use super::view::Entry;
use crate::render::{escape, stat};
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
    /// A commit of a piped log, at the top level in input order, holding
    /// its files' tree; `entry` is its own entry in the document.
    Commit {
        entry: usize,
        children: Vec<usize>,
        open: bool,
    },
}

impl Node {
    fn children(&self) -> &[usize] {
        match &self.kind {
            Kind::Dir { children, .. } | Kind::Commit { children, .. } => children,
            Kind::File(_) => &[],
        }
    }

    fn children_mut(&mut self) -> Option<&mut Vec<usize>> {
        match &mut self.kind {
            Kind::Dir { children, .. } | Kind::Commit { children, .. } => Some(children),
            Kind::File(_) => None,
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
        }
    }
}

impl Tree {
    /// Adds file `file` (the next index) at `path`.
    pub(super) fn insert(&mut self, file: usize, path: &[u8]) {
        let mut dir = self.base;
        // Collapsed folders are kept by their path from the root.
        let prefix = if self.base == ROOT || self.collapsed.is_empty() {
            Vec::new()
        } else {
            let mut p = self.path(self.base).to_vec();
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
        self.stale = true;
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
            editing: self.editing,
            scroll: self.scroll,
            height: self.height,
            ..Self::default()
        };
        for (i, e) in files.iter().enumerate() {
            if e.is_commit() {
                self.insert_commit(i, &e.change().path);
            } else {
                self.insert(i, &e.change().path);
            }
        }
        self.stale = true;
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
        let keep = (!self.filter.is_empty()).then(|| self.matches());
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
        let mut keep = vec![false; self.nodes.len()];
        self.hits.clear();
        self.hits.resize(self.leaves.len(), None);
        for (file, &leaf) in self.leaves.iter().enumerate() {
            if !matches!(self.nodes[leaf].kind, Kind::File(_)) {
                continue;
            }
            let path = escape(&self.file_path(leaf));
            let Some((_, at)) = fuzzy::score(&self.filter, &path) else {
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

    /// A file node's path, from below its commit when it's under one.
    fn file_path(&self, mut node: usize) -> BString {
        let mut parts = Vec::new();
        while node != ROOT && !matches!(self.nodes[node].kind, Kind::Commit { .. }) {
            parts.push(&self.nodes[node].name[..]);
            node = self.nodes[node].parent;
        }
        parts.reverse();
        parts.join(&b'/').into()
    }

    pub(super) fn filter(&self) -> &str {
        &self.filter
    }

    /// Sets the filter. When it hides the selection, the first file it
    /// keeps is selected.
    pub(super) fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_lowercase();
        if self.filter.is_empty() {
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
        if self.filter.is_empty() {
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
            }
        }
    }

    /// Points the files under a collapsed `dir` at its row.
    fn hide(&mut self, dir: usize, row: usize) {
        let mut stack = vec![dir];
        while let Some(node) = stack.pop() {
            match &self.nodes[node].kind {
                Kind::File(file) => self.file_rows[*file] = Some(row),
                Kind::Dir { children, .. } | Kind::Commit { children, .. } => {
                    stack.extend(children);
                }
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

    /// The files on the rows the panel showed last.
    pub(super) fn files_on_screen(&self) -> Vec<usize> {
        self.rows
            .iter()
            .skip(self.scroll)
            .take(self.height)
            .filter_map(|r| match self.nodes[r.node].kind {
                Kind::File(file) => Some(file),
                Kind::Dir { .. } | Kind::Commit { .. } => None,
            })
            .collect()
    }

    pub(super) fn item(&self) -> Option<Item> {
        let row = self.rows.get(self.selected)?;
        Some(match self.nodes[row.node].kind {
            Kind::File(file) => Item::File(file),
            Kind::Dir { open, .. } => Item::Dir { open },
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
        if let Kind::Dir { open: o, .. } | Kind::Commit { open: o, .. } =
            &mut self.nodes[row.node].kind
        {
            *o = open;
            if open {
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

    fn path(&self, mut node: usize) -> BString {
        let mut parts = Vec::new();
        while node != ROOT {
            parts.push(&self.nodes[node].name[..]);
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
            self.draw_row(grid, line, *row, selected, files, totals, width - 1, theme);
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
        (top, height, width): (usize, usize, usize),
        totals: &Totals,
        listing: bool,
        focused: bool,
        joined: bool,
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
        let growing = if listing { "+" } else { "" };
        let parts = if self.filter.is_empty() {
            vec![
                (" Files".to_owned(), Role::Plain),
                (format!(" · {}{growing}", totals.files), Role::Dim),
                (format!(" +{}", totals.added), Role::Added),
                (format!(" -{}", totals.removed), Role::Removed),
            ]
        } else {
            let kept = self.hits.iter().filter(|h| h.is_some()).count();
            vec![
                (" Files".to_owned(), Role::Plain),
                (format!(" · {kept}/{}{growing}", totals.files), Role::Dim),
                (format!(" · {}", self.filter), Role::Hint),
            ]
        };
        // Whole parts only, the deletions dropped first: a number cut
        // short would read as a smaller one.
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
        if self.editing {
            // Right of the `«` the viewer puts in the corner.
            let text = format!(" /{}▏ ", self.filter);
            let x = grid.put_str(2, bottom, end, &text, theme.style(Role::Hint));
            if self.filter.is_empty() {
                grid.put_str(
                    x,
                    bottom,
                    end,
                    "type to filter · esc clear ",
                    theme.style(Role::Dim),
                );
            }
        }
    }

    /// `row` at screen line `line`, its counts in the column `totals`
    /// sizes, ending at column `end`. A selected row (`Some`, whether the
    /// tree has the keys) takes the theme's selection background, its name
    /// in bold; letters and counts keep their colors.
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
        files: &[Entry],
        totals: &Totals,
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
            Kind::Dir { open, .. } => {
                let x = grid.put_str(
                    x,
                    line,
                    end,
                    if open { "▼ " } else { "▶ " },
                    style(Role::Dim, false),
                );
                let mut label = escape(&self.path_between(row.top, row.node));
                label.push('/');
                let room = end.saturating_sub(x + 1);
                grid.put_str(
                    x,
                    line,
                    end,
                    &fit_left(&label, room),
                    named(Role::Plain, false),
                );
            }
            Kind::Commit { open, .. } => {
                let x = grid.put_str(
                    x,
                    line,
                    end,
                    if open { "▼ " } else { "▶ " },
                    style(Role::Dim, false),
                );
                let label = escape(&self.nodes[row.node].name);
                let (id, subject) = label.split_once(' ').unwrap_or((&label, ""));
                let x = grid.put_str(x, line, end, id, style(Role::CommitHeader, false));
                let room = end.saturating_sub(x + 2);
                grid.put_str(
                    x + 1,
                    line,
                    end,
                    &fit_right(subject, room),
                    named(Role::Plain, false),
                );
            }
            Kind::File(file) => {
                let entry = &files[file];
                let dim = entry.viewed();
                let (letter, role) = stat::status_letter(entry.status());
                let x = grid.put_str(x, line, end, letter, style(role, dim)) + 1;
                let name = escape(&self.nodes[row.node].name);
                let (stats_x, counts) = place_counts(entry.counts(), (x, end), totals);
                let room = stats_x.saturating_sub(x + 1);
                let shown = fit_left(&name, room);
                // The filter's matches, when the name shows whole.
                let hits = self
                    .hits
                    .get(file)
                    .and_then(Option::as_ref)
                    .filter(|_| shown == name);
                fuzzy::put_marked(
                    grid,
                    (x, line, stats_x),
                    &shown,
                    hits.map_or(&[][..], Vec::as_slice),
                    (named(Role::Plain, dim), named(Role::Hint, dim)),
                );
                for (text, role, at) in counts {
                    grid.put_str(at, line, end, &text, style(role, dim));
                }
            }
        }
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
            tree.insert(i, &e.change().path);
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

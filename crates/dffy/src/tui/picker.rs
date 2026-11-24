//! The file picker (`o`): every file in a full-screen list,
//! narrowed as the query is typed, for jumping to a file without browsing
//! the tree. The diff's files come first; then, under a divider, the
//! repository's other files at one side of the diff, which open whole.
//!
//! Matching is [`crate::fuzzy`]'s; the best files come first. Labels are
//! escaped and lowercased once, when a file is listed. A longer query
//! only narrows the last result. The diff's files are scored as each key
//! comes; the other files, as many as a kernel's 78k, are scored by a
//! worker unless the query narrows a result small enough to score at once
//! ([`SCORE_NOW`]). Only the rows on screen are drawn.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use dffy_core::bstr::BStr;
use dffy_core::document::{Side, Source};
use dffy_core::git::{Repo, Snapshot};

use super::view::Entry;
use crate::columns::Columns as _;
use crate::fuzzy::{rank, score};
use crate::render::{Canvas, escape, thousands};
use crate::theme::Role;

/// Most other files a narrowing query scores on the keystroke's own
/// thread; more go to a worker. Sized so that keystroke stays within the
/// frame budget (`picker_narrow_16k` measures it).
pub(super) const SCORE_NOW: usize = 16_384;

/// Files listed or scored between checks of whether to stop.
const STOP_EVERY: usize = 4096;

/// The label an entry is matched on, lowercased: its path, or `None` for
/// a commit and a file shown outside the diff, which the other files list.
fn lowered(entry: &Entry) -> Option<Box<str>> {
    (!entry.is_commit() && !entry.is_unchanged())
        .then(|| escape(&entry.change().path).to_ascii_lowercase().into())
}

/// An id no query had, the picker's or the all-files tree filter's: a
/// worker's score answers its job by it, so one for a closed picker or
/// another list never passes for one asked now.
pub(super) fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Best first, ties in list order.
fn sort(scored: &mut [(i64, usize)]) {
    scored.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
}

/// Every file of a snapshot, for the picker: each path, and its label
/// escaped and lowercased once, as every keystroke scores it.
#[derive(Default)]
pub(crate) struct Paths {
    raw: Vec<u8>,
    raw_ends: Vec<usize>,
    lower: String,
    lower_ends: Vec<usize>,
}

impl Paths {
    /// The files at `at`, as [`Repo::list_paths`] lists them; `Err(None)`
    /// once `stop` says to.
    pub(crate) fn list(
        repo: &Repo,
        at: Snapshot,
        stop: impl Fn() -> bool,
    ) -> Result<Self, Option<String>> {
        let mut paths = Self::default();
        let mut stopped = false;
        repo.list_paths(at, &[], |path, _| {
            if paths.len() % STOP_EVERY == 0 && stop() {
                stopped = true;
                return ControlFlow::Break(());
            }
            paths.push(path);
            ControlFlow::Continue(())
        })
        .map_err(|e| Some(e.to_string()))?;
        if stopped { Err(None) } else { Ok(paths) }
    }

    pub(crate) fn push(&mut self, path: &[u8]) {
        self.raw.extend_from_slice(path);
        self.raw_ends.push(self.raw.len());
        let mut label = escape(path);
        label.make_ascii_lowercase();
        self.lower.push_str(&label);
        self.lower_ends.push(self.lower.len());
    }

    pub(crate) fn len(&self) -> usize {
        self.raw_ends.len()
    }

    pub(crate) fn path(&self, i: usize) -> &BStr {
        let start = i.checked_sub(1).map_or(0, |p| self.raw_ends[p]);
        self.raw[start..self.raw_ends[i]].into()
    }

    fn lower(&self, i: usize) -> &str {
        let start = i.checked_sub(1).map_or(0, |p| self.lower_ends[p]);
        &self.lower[start..self.lower_ends[i]]
    }

    /// The files among `candidates` that `query` matches, best first;
    /// `None` once `stop` says to.
    pub(crate) fn score(
        &self,
        query: &str,
        candidates: &[usize],
        stop: impl Fn() -> bool,
    ) -> Option<Vec<(i64, usize)>> {
        let mut scored = Vec::new();
        for (n, &i) in candidates.iter().enumerate() {
            if n % STOP_EVERY == 0 && n > 0 && stop() {
                return None;
            }
            if let Some(s) = rank(query, self.lower(i)) {
                scored.push((s, i));
            }
        }
        if !query.is_empty() {
            sort(&mut scored);
        }
        Some(scored)
    }

    /// The positions of the files `files` doesn't list, in order.
    fn outside(&self, files: &[Entry]) -> Arc<[usize]> {
        let listed: HashSet<&[u8]> = files
            .iter()
            .filter(|e| !e.is_commit() && !e.is_unchanged())
            .flat_map(|e| [&e.change().path[..], &**e.change().old_path()])
            .collect();
        (0..self.len())
            .filter(|&i| {
                let path: &[u8] = self.path(i);
                !listed.contains(path)
            })
            .collect()
    }
}

/// Whether the list is still growing, for the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Progress {
    Reading,
    Done,
    /// A piped diff hit [`super::piped::MAX_ENTRIES`] and isn't read on.
    Capped,
}

/// The files the diff doesn't list, under the divider.
pub(super) enum Others {
    /// None are offered, for this reason.
    Unavailable(String),
    /// Being listed at this side.
    Listing(Side),
    Ready(Listed),
}

pub(super) struct Listed {
    side: Side,
    paths: Arc<Paths>,
    /// The positions in `paths` of the files the diff doesn't list.
    outside: Arc<[usize]>,
    /// The ones the query matches, best first, once scored.
    matches: Vec<(i64, usize)>,
    /// What a worker is to score for the query, while it does.
    pending: Option<Arc<[usize]>>,
}

impl Others {
    /// The files at `side`, `paths`, without those `files` lists.
    pub(super) fn ready(side: Side, paths: Arc<Paths>, files: &[Entry]) -> Self {
        let outside = paths.outside(files);
        Self::Ready(Listed {
            side,
            paths,
            outside,
            matches: Vec::new(),
            pending: None,
        })
    }
}

/// What the picker picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Pick {
    /// An entry of the document.
    Entry(usize),
    /// A file the diff doesn't list, at this side.
    Other(dffy_core::bstr::BString, Side),
}

/// A score of the other files for a worker, for query `id`.
pub(super) struct ScoreWork {
    pub paths: Arc<Paths>,
    pub candidates: Arc<[usize]>,
    pub query: String,
    pub id: u64,
}

pub(super) struct Picker {
    query: String,
    /// Bumped as the query changes; a worker's score for another is late.
    query_id: u64,
    /// Each entry's label, lowercased (see [`lowered`]).
    lower: Vec<Option<Box<str>>>,
    /// The diff's files the query matches with their scores, best first,
    /// ties in list order.
    matches: Vec<(i64, usize)>,
    /// How many entries of the list were matched; later ones arrived since.
    seen: usize,
    /// How many of those are the diff's files.
    total: usize,
    others: Others,
    /// The row selected: rows are the diff's matches, the divider, then
    /// the other files' matches.
    selected: usize,
    scroll: usize,
    /// Rows of files the last frame showed.
    height: usize,
}

impl Picker {
    pub(super) fn new(files: &[Entry], others: Others) -> Self {
        let mut picker = Self {
            query: String::new(),
            query_id: 0,
            lower: Vec::new(),
            matches: Vec::new(),
            seen: 0,
            total: 0,
            others,
            selected: 0,
            scroll: 0,
            height: 1,
        };
        picker.relabel(files);
        picker.refilter(false);
        picker
    }

    /// The side the other files are at, while they're offered.
    pub(super) fn side(&self) -> Option<Side> {
        match &self.others {
            Others::Unavailable(_) => None,
            Others::Listing(side) | Others::Ready(Listed { side, .. }) => Some(*side),
        }
    }

    /// Whether the other files are being listed.
    pub(super) fn listing(&self) -> bool {
        matches!(self.others, Others::Listing(_))
    }

    /// Lists `others` under the divider in place of the files there.
    pub(super) fn set_others(&mut self, others: Others) {
        self.others = others;
        // A score of the files there before is for another list.
        self.query_id = next_id();
        self.rescore_others();
        self.selected = self.selected.min(self.matches.len());
        self.select_file(1);
    }

    pub(super) fn query_id(&self) -> u64 {
        self.query_id
    }

    /// The score a worker is to do for the query, while one is wanted.
    pub(super) fn score_work(&self) -> Option<ScoreWork> {
        let Others::Ready(listed) = &self.others else {
            return None;
        };
        Some(ScoreWork {
            paths: Arc::clone(&listed.paths),
            candidates: Arc::clone(listed.pending.as_ref()?),
            query: self.query.clone(),
            id: self.query_id,
        })
    }

    /// Takes a worker's score of the other files for query `id`.
    pub(super) fn take_score(&mut self, id: u64, matches: Vec<(i64, usize)>) {
        if id != self.query_id {
            return;
        }
        if let Others::Ready(listed) = &mut self.others
            && listed.pending.take().is_some()
        {
            listed.matches = matches;
            if self.selected >= self.matches.len() {
                self.selected = self.matches.len();
                self.select_file(1);
            }
        }
    }

    pub(super) fn file(&self) -> Option<Pick> {
        self.pick(self.selected)
    }

    fn pick(&self, row: usize) -> Option<Pick> {
        if let Some(&(_, f)) = self.matches.get(row) {
            return Some(Pick::Entry(f));
        }
        let Others::Ready(listed) = &self.others else {
            return None;
        };
        let &(_, i) = listed
            .matches
            .get(row.checked_sub(self.matches.len() + 1)?)?;
        Some(Pick::Other(listed.paths.path(i).to_owned(), listed.side))
    }

    /// The other files' matches shown: none while a worker scores them.
    fn other_matches(&self) -> &[(i64, usize)] {
        match &self.others {
            Others::Ready(listed) if listed.pending.is_none() => &listed.matches,
            _ => &[],
        }
    }

    /// Rows in all: the diff's matches, the divider and the others'.
    fn rows_len(&self) -> usize {
        self.matches.len() + 1 + self.other_matches().len()
    }

    /// Moves the selection off the divider, in the direction of `delta`,
    /// or the other way when there's nothing that way.
    fn select_file(&mut self, delta: isize) {
        let divider = self.matches.len();
        if self.selected != divider {
            return;
        }
        let below = !self.other_matches().is_empty();
        self.selected = match (delta > 0, below, divider > 0) {
            (true, true, _) | (false, true, false) => divider + 1,
            (_, _, true) => divider - 1,
            _ => divider,
        };
    }

    /// Matches the entries that arrived since, merging them in. The
    /// selection stays on its file, or on the best match while it's on
    /// the first row.
    pub(super) fn catch_up(&mut self, files: &[Entry]) {
        if files.len() <= self.seen {
            return;
        }
        self.lower.extend(files[self.seen..].iter().map(lowered));
        let new = self.scored(self.seen..files.len());
        self.total += self.lower[self.seen..].iter().flatten().count();
        self.seen = files.len();
        if new.is_empty() {
            return;
        }
        let before = self.matches.len();
        let on_diff = self.selected < before;
        let selected = (self.selected > 0 && on_diff).then(|| self.matches[self.selected]);
        let old = std::mem::take(&mut self.matches);
        let key = |&(s, f): &(i64, usize)| (std::cmp::Reverse(s), f);
        let mut merged = Vec::with_capacity(old.len() + new.len());
        let (mut a, mut b) = (old.into_iter().peekable(), new.into_iter().peekable());
        loop {
            let next = match (a.peek(), b.peek()) {
                (Some(x), Some(y)) if key(x) <= key(y) => a.next(),
                (_, Some(_)) => b.next(),
                (Some(_), None) => a.next(),
                (None, None) => break,
            };
            merged.extend(next);
        }
        self.matches = merged;
        let moved = self.matches.len() - before;
        if let Some(selected) = selected {
            let row = self.matches.partition_point(|m| key(m) < key(&selected));
            self.scroll += row - self.selected;
            self.selected = row;
        } else if !on_diff {
            self.selected += moved;
            self.scroll += moved;
        }
    }

    /// Lists the files again from the start: the list was rebuilt.
    pub(super) fn rebuild(&mut self, files: &[Entry]) {
        if let Others::Ready(listed) = &mut self.others {
            listed.outside = listed.paths.outside(files);
        }
        self.relabel(files);
        self.refilter(false);
    }

    /// Lowers every entry's label, once per list.
    fn relabel(&mut self, files: &[Entry]) {
        self.lower = files.iter().map(lowered).collect();
        self.seen = files.len();
        self.total = self.lower.iter().flatten().count();
    }

    pub(super) fn push(&mut self, c: char) {
        self.query.push(c.to_ascii_lowercase());
        self.refilter(true);
    }

    pub(super) fn pop(&mut self) {
        if self.query.pop().is_some() {
            self.refilter(false);
        }
    }

    /// Matches the query against every file, or with `narrow` only against
    /// the last matches: a longer query can only match fewer. The diff's
    /// files already seen only: [`Picker::catch_up`] adds the rest.
    fn refilter(&mut self, narrow: bool) {
        self.query_id = next_id();
        self.matches = if narrow {
            let candidates = std::mem::take(&mut self.matches);
            self.scored(candidates.into_iter().map(|(_, f)| f))
        } else {
            self.scored(0..self.seen)
        };
        if narrow {
            self.narrow_others();
        } else {
            self.rescore_others();
        }
        self.selected = 0;
        self.scroll = 0;
        self.select_file(1);
    }

    /// The other files for the query anew: all of them in order for no
    /// query, else scored by a worker.
    fn rescore_others(&mut self) {
        let Others::Ready(listed) = &mut self.others else {
            return;
        };
        if self.query.is_empty() {
            listed.matches = listed.outside.iter().map(|&i| (0, i)).collect();
            listed.pending = None;
        } else {
            listed.matches.clear();
            listed.pending = Some(Arc::clone(&listed.outside));
        }
    }

    /// The other files for a longer query: scored now when the last
    /// matches are few, else by a worker over them.
    fn narrow_others(&mut self) {
        let Others::Ready(listed) = &mut self.others else {
            return;
        };
        let candidates: Arc<[usize]> = match &listed.pending {
            Some(pending) => Arc::clone(pending),
            None => listed.matches.iter().map(|&(_, i)| i).collect(),
        };
        if candidates.len() <= SCORE_NOW {
            listed.matches =
                (listed.paths.score(&self.query, &candidates, || false)).unwrap_or_default();
            listed.pending = None;
        } else {
            listed.matches.clear();
            listed.pending = Some(candidates);
        }
    }

    /// The diff's files among `candidates` the query matches, best first.
    fn scored(&self, candidates: impl IntoIterator<Item = usize>) -> Vec<(i64, usize)> {
        let mut scored: Vec<(i64, usize)> = candidates
            .into_iter()
            .filter_map(|f| {
                let label = self.lower.get(f)?.as_deref()?;
                rank(&self.query, label).map(|s| (s, f))
            })
            .collect();
        if !self.query.is_empty() {
            sort(&mut scored);
        }
        scored
    }

    pub(super) fn step(&mut self, delta: isize) {
        let last = self.rows_len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
        self.select_file(delta);
        self.keep_in_view();
    }

    pub(super) fn page(&self) -> isize {
        isize::try_from(self.height.max(1)).unwrap_or(isize::MAX)
    }

    fn keep_in_view(&mut self) {
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + self.height {
            self.scroll = self.selected + 1 - self.height;
        }
    }

    /// The file on list row `y` (0 is the first file row), selecting it.
    pub(super) fn click(&mut self, y: usize) -> Option<Pick> {
        if y >= self.height {
            return None;
        }
        let row = self.scroll + y;
        let pick = self.pick(row)?;
        self.selected = row;
        Some(pick)
    }

    /// Lays out the picker for `width` columns and `height` rows: the
    /// query line, then a row per file on screen. Each row comes with
    /// whether it's the selected one. The header counts the diff's files,
    /// and the matches once there's a query: `reading… 3/3,450 files`
    /// while the list grows, `3/13,003 files` once it's whole; the divider
    /// counts the other files likewise, named by the version they're in.
    /// `versions` has the old and new ones' names and what each means, if
    /// anything, which `tab`'s hint says of the one it goes to.
    pub(super) fn rows(
        &mut self,
        files: &[Entry],
        commits: &[usize],
        (width, height): (usize, usize),
        progress: Progress,
        versions: &[(String, Option<&str>); 2],
    ) -> Vec<(Canvas, bool)> {
        self.height = height.saturating_sub(1).max(1);
        self.keep_in_view();
        let mut out = Vec::with_capacity(height);
        let mut c = Canvas::default();
        let matched = |n: usize| {
            if self.query.is_empty() {
                String::new()
            } else {
                format!("{}/", thousands(n))
            }
        };
        let total = thousands(self.total);
        let count = match progress {
            Progress::Reading => format!("reading… {}{total} files", matched(self.matches.len())),
            Progress::Done => format!("{}{total} files", matched(self.matches.len())),
            Progress::Capped => format!(
                "{}{total} files, reading stopped",
                matched(self.matches.len())
            ),
        };
        c.push(Role::Hint, " > ");
        c.push(Role::Plain, &self.query);
        c.push(Role::Hint, "▏");
        let mut used = 3 + self.query.width() + 1;
        let hint = match self.side().map(|side| &versions[side.other().index()]) {
            Some((name, meaning)) => {
                let meaning = meaning.map(|m| format!(" ({m})")).unwrap_or_default();
                format!(" type to filter · enter open · tab {name}{meaning} · esc close")
            }
            None => " type to filter · enter open · esc close".to_owned(),
        };
        if self.query.is_empty() && used + hint.width() + count.width() + 3 <= width {
            c.push(Role::Dim, &hint);
            used += hint.width();
        }
        c.spaces(Role::Plain, width.saturating_sub(used + count.width() + 1));
        c.push(Role::Dim, &count);
        c.end_line();
        out.push((c, false));
        let divider = self.matches.len();
        // The counts' columns, as wide as the widest count shown needs:
        // `+N` right-aligned in one, `-N` in the next, zeros left blank.
        let width_of = |n: usize| if n > 0 { n.to_string().len() + 1 } else { 0 };
        let shown = self.scroll..(self.scroll + self.height).min(self.rows_len());
        let columns = self.matches[shown.start.min(divider)..shown.end.min(divider)]
            .iter()
            .fold((0, 0), |(a, r), &(_, f)| {
                let counts = files[f].counts().unwrap_or_default();
                (
                    a.max(width_of(counts.added)),
                    r.max(width_of(counts.removed)),
                )
            });
        for row in shown {
            let mut c = Canvas::default();
            if let Some(&(_, file)) = self.matches.get(row) {
                self.row(files, commits, file, (width, columns), &mut c);
            } else if row == divider {
                self.divider(width, versions, &mut c);
            } else if let Others::Ready(listed) = &self.others {
                let (_, i) = listed.matches[row - divider - 1];
                self.other_row(&escape(listed.paths.path(i)), width, &mut c);
            }
            out.push((c, row == self.selected && row != divider));
        }
        let waiting = matches!(self.others, Others::Listing(_))
            || matches!(&self.others, Others::Ready(l) if l.pending.is_some());
        if self.rows_len() == 1 && !self.query.is_empty() && !waiting && out.len() < height {
            let mut c = Canvas::default();
            c.line(Role::Dim, "   no file matches");
            out.push((c, false));
        }
        out
    }

    /// The divider between the diff's files and the others: the version
    /// those are in and how many match, or why there are none.
    fn divider(&self, width: usize, versions: &[(String, Option<&str>); 2], c: &mut Canvas) {
        let side = |side: Side| &versions[side.index()].0;
        let (title, note) = match &self.others {
            Others::Unavailable(why) => (format!("no other files: {why}"), String::new()),
            Others::Listing(at) => (
                format!("not in the diff · {}", side(*at)),
                " · listing…".to_owned(),
            ),
            Others::Ready(listed) => {
                let note = if listed.pending.is_some() {
                    " · scoring…".to_owned()
                } else {
                    let n = if self.query.is_empty() {
                        String::new()
                    } else {
                        format!("{}/", thousands(listed.matches.len()))
                    };
                    format!(" · {n}{} files", thousands(listed.outside.len()))
                };
                (format!("not in the diff · {}", side(listed.side)), note)
            }
        };
        c.push(Role::Dim, " ── ");
        c.push(Role::Dim, &title);
        c.push(Role::Dim, &note);
        let used = 4 + title.width() + note.width();
        if used + 2 < width {
            c.push(Role::Dim, " ");
            c.push(Role::Dim, &"─".repeat(width - used - 2));
        }
        c.end_line();
    }

    /// A file the diff doesn't list: a dot for its letter, and its path
    /// with the matched characters marked, cut from the left to fit.
    fn other_row(&self, path: &str, width: usize, c: &mut Canvas) {
        c.push(Role::Plain, " ");
        c.push(Role::Dim, "·");
        c.push(Role::Plain, " ");
        self.path(path, width.saturating_sub(4), c);
        c.end_line();
    }

    /// `path` with the query's matched characters marked, cut from the
    /// left to fit `room` columns so the file name stays. Returns the
    /// columns it took.
    fn path(&self, path: &str, room: usize, c: &mut Canvas) -> usize {
        let positions = score(&self.query, path).map(|(_, p)| p).unwrap_or_default();
        let mut start = 0;
        let cut = path.width() > room;
        if cut {
            let keep = room.saturating_sub(1);
            while start < path.len() && path[start..].width() > keep {
                start += path[start..].chars().next().map_or(1, char::len_utf8);
            }
            c.push(Role::Dim, "…");
        }
        for (i, ch) in path.char_indices().skip_while(|(i, _)| *i < start) {
            let role = if positions.contains(&i) {
                Role::Hint
            } else {
                Role::Plain
            };
            c.push_char(role, ch);
        }
        path[start..].width() + usize::from(cut)
    }

    /// One file's row: its status letter, its commit when it has one, its
    /// path with the matched characters marked, cut from the left to fit,
    /// and its counts in `columns` at the right edge.
    fn row(
        &self,
        files: &[Entry],
        commits: &[usize],
        file: usize,
        (width, columns): (usize, (usize, usize)),
        c: &mut Canvas,
    ) {
        let entry = &files[file];
        let (letter, role) = super::tree::letter(entry);
        c.push(Role::Plain, " ");
        c.push(role, letter);
        c.push(Role::Plain, " ");
        let mut used = 3;
        let commit = commits.partition_point(|&e| e <= file).checked_sub(1);
        if let Some(Source::Commit(commit)) = commit.map(|i| files[commits[i]].source())
            && let Some(id) = &commit.id
        {
            let short = &id[..id.len().min(7)];
            c.push(Role::CommitHeader, short);
            c.push(Role::Plain, " ");
            used += short.len() + 1;
        }
        let counts = entry.counts().unwrap_or_default();
        let field = |w: usize| if w > 0 { w + 1 } else { 0 };
        let right = field(columns.0) + field(columns.1);
        let room = width.saturating_sub(used + right + 1);
        let shown = self.path(&escape(&entry.change().path), room, c);
        c.spaces(Role::Plain, room.saturating_sub(shown) + 1);
        for (sign, n, w, role) in [
            ('+', counts.added, columns.0, Role::Added),
            ('-', counts.removed, columns.1, Role::Removed),
        ] {
            if w > 0 {
                let text = if n > 0 {
                    format!("{sign}{n}")
                } else {
                    String::new()
                };
                c.spaces(Role::Plain, 1 + w - text.len());
                c.push(role, &text);
            }
        }
        c.end_line();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rank(query: &str, paths: &[&'static str]) -> Vec<&'static str> {
        let mut scored: Vec<(i64, &str)> = paths
            .iter()
            .filter_map(|p| score(query, p).map(|(s, _)| (s, *p)))
            .collect();
        scored.sort_by_key(|a| std::cmp::Reverse(a.0));
        scored.into_iter().map(|(_, p)| p).collect()
    }

    #[test]
    fn a_match_in_the_file_name_beats_one_in_the_directories() {
        assert_eq!(
            rank("fair", &["fs/afs/inode/ring.c", "kernel/sched/fair.c"]),
            ["kernel/sched/fair.c", "fs/afs/inode/ring.c"]
        );
        assert_eq!(
            rank("main", &["src/main/lib.rs", "src/lib/main.rs"]),
            ["src/lib/main.rs", "src/main/lib.rs"]
        );
    }

    #[test]
    fn contiguous_runs_beat_scattered_letters() {
        assert_eq!(
            rank("tree", &["the_rule_engine.rs", "tree.rs"]),
            ["tree.rs", "the_rule_engine.rs"]
        );
        // The best start is found, not the first.
        let (_, positions) = score("fair", "f_a_fair.c").expect("matches");
        assert_eq!(positions, [4, 5, 6, 7]);
    }

    /// The score alone, from a label lowercased once, is the one the
    /// picker draws its marks with.
    #[test]
    fn a_label_lowered_once_scores_as_the_label() {
        for (query, path) in [
            ("fair", "kernel/sched/FAIR.c"),
            ("ma", "src/Main/lib.rs"),
            ("x", "a/b"),
        ] {
            let lower = path.to_ascii_lowercase();
            assert_eq!(
                crate::fuzzy::rank(query, &lower),
                score(query, path).map(|(s, _)| s)
            );
        }
    }

    fn doc(paths: &[&str]) -> super::super::view::Doc {
        let layout = crate::render::Layout {
            split: false,
            width: Some(80),
            wrap: false,
            words: false,
            tab_width: 4,
            indicators: crate::config::Indicators::Classic,
            one_sided: None,
            unmerged: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: true,
            hunk_headers: true,
            modified_lines: true,
            read_alone: false,
        };
        let mut doc = super::super::view::Doc::new(layout, 40);
        for path in paths {
            push(&mut doc, path);
        }
        doc
    }

    fn push(doc: &mut super::super::view::Doc, path: &str) {
        let change = dffy_core::git::FileChange {
            path: path.into(),
            status: dffy_core::git::Status::Modified,
            old: None,
            new: None,
            staging: None,
        };
        doc.document.push(change, Source::Repo);
    }

    fn text(row: &Canvas) -> String {
        row.rows().next().expect("a row").map(|(_, t)| t).collect()
    }

    /// Files listed while the picker is open are matched as they come and
    /// merged in by score; the selection stays on its file.
    #[test]
    fn files_that_arrive_later_are_merged_in() {
        let mut doc = doc(&["src/one.rs", "src/two.rs"]);
        let none = || Others::Unavailable("a piped diff has only its files".to_owned());
        let mut picker = Picker::new(doc.document.files(), none());
        picker.push('r');
        picker.push('s');
        picker.step(1);
        assert_eq!(picker.file(), Some(Pick::Entry(1)));
        // Shorter, so it scores higher.
        push(&mut doc, "rs");
        push(&mut doc, "docs/readme.md");
        picker.catch_up(doc.document.files());
        let order: Vec<usize> = picker.matches.iter().map(|&(_, f)| f).collect();
        assert_eq!(order, [2, 0, 1]);
        assert_eq!(picker.file(), Some(Pick::Entry(1)), "still on src/two.rs");
        assert_eq!(picker.total, 4);
        let versions = [("HEAD".to_owned(), None), ("working tree".to_owned(), None)];
        let rows = picker.rows(
            doc.document.files(),
            &[],
            (60, 10),
            Progress::Reading,
            &versions,
        );
        assert!(
            text(&rows[0].0).ends_with("reading… 3/4 files"),
            "{}",
            text(&rows[0].0)
        );
        // The selection kept its place on screen, a row down the list.
        let last = text(&rows.last().expect("rows").0);
        assert!(
            last.contains("no other files: a piped diff has only its files"),
            "{last}"
        );
    }

    /// The other files come after the diff's, under a divider, without
    /// those the diff lists. A query over many of them waits for a
    /// worker's score; the selection skips the divider.
    #[test]
    fn other_files_follow_the_divider() {
        let doc = doc(&["src/main.rs"]);
        let files = doc.document.files();
        let mut paths = Paths::default();
        for path in ["README.md", "src/lib.rs", "src/main.rs"] {
            paths.push(path.as_bytes());
        }
        let others = Others::ready(Side::New, Arc::new(paths), files);
        let mut picker = Picker::new(files, others);
        let versions = [
            ("HEAD".to_owned(), Some("last commit")),
            ("working tree".to_owned(), Some("your files now")),
        ];
        let rows = picker.rows(files, &[], (100, 10), Progress::Done, &versions);
        let shown: Vec<String> = rows.iter().map(|(c, _)| text(c)).collect();
        // `tab` goes to the other version, said with what it means.
        assert!(shown[0].contains(" tab HEAD (last commit) · "), "{shown:?}");
        assert!(shown[1].contains("src/main.rs"), "{shown:?}");
        assert!(
            shown[2].contains("not in the diff · working tree · 2 files"),
            "{shown:?}"
        );
        assert!(
            shown[3].contains("README.md") && shown[4].contains("src/lib.rs"),
            "{shown:?}"
        );
        assert_eq!(rows.len(), 5);
        picker.step(1);
        assert_eq!(
            picker.file(),
            Some(Pick::Other("README.md".into(), Side::New)),
            "past the divider"
        );
        picker.push('l');
        // Narrowed to few: scored at once.
        assert!(picker.score_work().is_none());
        assert_eq!(
            picker.file(),
            Some(Pick::Other("src/lib.rs".into(), Side::New))
        );
        picker.push('z');
        assert_eq!(picker.file(), None, "no match");
        // From scratch: a worker scores them.
        picker.pop();
        assert_eq!(picker.file(), None, "while it does");
        let work = picker.score_work().expect("a score for a worker");
        let matches = work
            .paths
            .score(&work.query, &work.candidates, || false)
            .expect("not stopped");
        picker.take_score(work.id, matches);
        assert_eq!(
            picker.file(),
            Some(Pick::Other("src/lib.rs".into(), Side::New))
        );
    }

    #[test]
    fn thousands_are_grouped() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(13_003), "13,003");
        assert_eq!(thousands(1_000_000), "1,000,000");
    }

    #[test]
    fn letters_must_come_in_order() {
        assert!(score("rf", "fair.c").is_none());
        assert!(
            score("FAIR", "kernel/sched/FAIR.c").is_none(),
            "queries are lowercased first"
        );
        assert!(score("fair", "kernel/sched/FAIR.c").is_some());
    }
}

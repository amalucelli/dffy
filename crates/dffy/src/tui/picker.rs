//! The file picker (`o`): every file in a full-screen list,
//! narrowed as the query is typed, for jumping to a file without browsing
//! the tree.
//!
//! Matching is [`super::fuzzy`]'s; the best files come first. A longer
//! query only narrows the last result, so typing stays fast on large
//! lists; only the rows on screen are drawn.

use dffy_core::document::Source;
use unicode_width::UnicodeWidthStr;

use super::fuzzy::score;
use super::view::Entry;
use crate::render::{Canvas, escape, stat, thousands};
use crate::theme::Role;

/// What the picker lists for an entry: its path, or `None` for a commit.
fn label(entry: &Entry) -> Option<String> {
    (!entry.is_commit()).then(|| escape(&entry.change().path))
}

/// Whether the list is still growing, for the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Progress {
    Reading,
    Done,
    /// A piped diff hit [`super::piped::MAX_ENTRIES`] and isn't read on.
    Capped,
}

pub(super) struct Picker {
    query: String,
    /// The files the query matches with their scores, best first, ties
    /// in list order.
    matches: Vec<(i64, usize)>,
    /// How many entries of the list were matched; later ones arrived since.
    seen: usize,
    /// How many of those are files.
    total: usize,
    pub selected: usize,
    scroll: usize,
    /// Rows of files the last frame showed.
    height: usize,
}

impl Picker {
    pub(super) fn new(files: &[Entry]) -> Self {
        let mut picker = Self {
            query: String::new(),
            matches: Vec::new(),
            seen: 0,
            total: 0,
            selected: 0,
            scroll: 0,
            height: 1,
        };
        picker.refilter(files, false);
        picker
    }

    pub(super) fn file(&self) -> Option<usize> {
        self.matches.get(self.selected).map(|&(_, f)| f)
    }

    /// Matches the entries that arrived since, merging them in. The
    /// selection stays on its file, or on the best match while it's on
    /// the first row.
    pub(super) fn catch_up(&mut self, files: &[Entry]) {
        if files.len() <= self.seen {
            return;
        }
        let new = self.scored(files, self.seen..files.len());
        self.total += files[self.seen..].iter().filter(|e| !e.is_commit()).count();
        self.seen = files.len();
        if new.is_empty() {
            return;
        }
        let selected = (self.selected > 0).then(|| self.matches[self.selected]);
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
        if let Some(selected) = selected {
            let row = self.matches.partition_point(|m| key(m) < key(&selected));
            self.scroll += row - self.selected;
            self.selected = row;
        }
    }

    /// Lists the files again from the start: the list was rebuilt.
    pub(super) fn rebuild(&mut self, files: &[Entry]) {
        self.refilter(files, false);
    }

    pub(super) fn push(&mut self, files: &[Entry], c: char) {
        self.query.push(c.to_ascii_lowercase());
        self.refilter(files, true);
    }

    pub(super) fn pop(&mut self, files: &[Entry]) {
        if self.query.pop().is_some() {
            self.refilter(files, false);
        }
    }

    /// Matches the query against every file, or with `narrow` only against
    /// the last matches: a longer query can only match fewer.
    /// Entries already seen only: [`Picker::catch_up`] adds the rest.
    fn refilter(&mut self, files: &[Entry], narrow: bool) {
        self.matches = if narrow {
            let candidates = std::mem::take(&mut self.matches);
            self.scored(files, candidates.into_iter().map(|(_, f)| f))
        } else {
            self.seen = files.len();
            self.total = files.iter().filter(|e| !e.is_commit()).count();
            self.scored(files, 0..files.len())
        };
        self.selected = 0;
        self.scroll = 0;
    }

    /// The files among `candidates` the query matches, best first.
    fn scored(
        &self,
        files: &[Entry],
        candidates: impl IntoIterator<Item = usize>,
    ) -> Vec<(i64, usize)> {
        let mut scored: Vec<(i64, usize)> = candidates
            .into_iter()
            .filter_map(|f| {
                let label = label(files.get(f)?)?;
                score(&self.query, &label).map(|(s, _)| (s, f))
            })
            .collect();
        if !self.query.is_empty() {
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        }
        scored
    }

    pub(super) fn step(&mut self, delta: isize) {
        let last = self.matches.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
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
    pub(super) fn click(&mut self, y: usize) -> Option<usize> {
        if y >= self.height {
            return None;
        }
        let row = self.scroll + y;
        let &(_, file) = self.matches.get(row)?;
        self.selected = row;
        Some(file)
    }

    /// Lays out the picker for `width` columns and `height` rows: the
    /// query line, then a row per file on screen. Each row comes with
    /// whether it's the selected one. The header counts the files, and
    /// the matches once there's a query: `reading… 3/3,450 files` while
    /// the list grows, `3/13,003 files` once it's whole.
    pub(super) fn rows(
        &mut self,
        files: &[Entry],
        commits: &[usize],
        (width, height): (usize, usize),
        progress: Progress,
    ) -> Vec<(Canvas, bool)> {
        self.height = height.saturating_sub(1).max(1);
        self.keep_in_view();
        let mut out = Vec::with_capacity(height);
        let mut c = Canvas::default();
        let matched = if self.query.is_empty() {
            String::new()
        } else {
            format!("{}/", thousands(self.matches.len()))
        };
        let total = thousands(self.total);
        let count = match progress {
            Progress::Reading => format!("reading… {matched}{total} files"),
            Progress::Done => format!("{matched}{total} files"),
            Progress::Capped => format!("{matched}{total} files, reading stopped"),
        };
        c.push(Role::Hint, " > ");
        c.push(Role::Plain, &self.query);
        c.push(Role::Hint, "▏");
        let mut used = 3 + self.query.width() + 1;
        let hint = " type to filter · enter open · esc close";
        if self.query.is_empty() && used + hint.width() + count.width() + 3 <= width {
            c.push(Role::Dim, hint);
            used += hint.width();
        }
        c.spaces(Role::Plain, width.saturating_sub(used + count.width() + 1));
        c.push(Role::Dim, &count);
        c.end_line();
        out.push((c, false));
        if self.matches.is_empty() {
            let mut c = Canvas::default();
            c.line(Role::Dim, "   no file matches");
            out.push((c, false));
            return out;
        }
        let shown = || {
            self.matches
                .iter()
                .enumerate()
                .skip(self.scroll)
                .take(self.height)
        };
        // The counts' columns, as wide as the widest count shown needs:
        // `+N` right-aligned in one, `-N` in the next, zeros left blank.
        let width_of = |n: usize| if n > 0 { n.to_string().len() + 1 } else { 0 };
        let columns = shown().fold((0, 0), |(a, r), (_, &(_, f))| {
            let counts = files[f].counts().unwrap_or_default();
            (
                a.max(width_of(counts.added)),
                r.max(width_of(counts.removed)),
            )
        });
        for (row, &(_, file)) in shown() {
            let mut c = Canvas::default();
            self.row(files, commits, file, (width, columns), &mut c);
            out.push((c, row == self.selected));
        }
        out
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
        let (letter, role) = stat::status_letter(entry.status());
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
        let path = label(entry).unwrap_or_default();
        let positions = score(&self.query, &path)
            .map(|(_, p)| p)
            .unwrap_or_default();
        let room = width.saturating_sub(used + right + 1);
        // Cut from the left so the file name stays.
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
        let shown = path[start..].width() + usize::from(cut);
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

    /// Timing, not a check: run with `--ignored --nocapture` in release.
    #[test]
    #[ignore = "a measurement"]
    fn typing_into_13k_paths() {
        use dffy_core::git::{FileChange, Status};
        let layout = crate::render::Layout {
            split: false,
            width: Some(80),
            wrap: false,
            words: false,
            tab_width: 4,
            indicators: crate::config::Indicators::Classic,
            one_sided: false,
            unmerged: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: true,
            hunk_headers: true,
            modified_lines: true,
        };
        let mut doc = super::super::view::Doc::new(layout, 40);
        for i in 0..13_000 {
            let change = FileChange {
                path: format!(
                    "drivers/subsystem{}/vendor{}/{}_{i}.c",
                    i % 40,
                    i % 300,
                    ["fair", "core", "main", "inode", "sched_fair_rt"][i % 5]
                )
                .into(),
                status: Status::Modified,
                old: None,
                new: None,
                staging: None,
            };
            doc.document.push(change, Source::Repo);
        }
        let files = doc.document.files();
        let started = std::time::Instant::now();
        let mut picker = Picker::new(files);
        eprintln!("open: {:?}", started.elapsed());
        for c in "fair".chars() {
            let at = std::time::Instant::now();
            picker.push(files, c);
            eprintln!("+{c}: {:?}, {} matches", at.elapsed(), picker.matches.len());
        }
        let at = std::time::Instant::now();
        picker.pop(files);
        eprintln!("backspace: {:?}", at.elapsed());
        let at = std::time::Instant::now();
        let rows = picker.rows(files, &[], (120, 60), Progress::Done);
        eprintln!("rows: {:?} for {}", at.elapsed(), rows.len());
    }

    fn doc(paths: &[&str]) -> super::super::view::Doc {
        let layout = crate::render::Layout {
            split: false,
            width: Some(80),
            wrap: false,
            words: false,
            tab_width: 4,
            indicators: crate::config::Indicators::Classic,
            one_sided: false,
            unmerged: false,
            digits: 0,
            inline_signs: false,
            scroll: 0,
            line_numbers: true,
            hunk_headers: true,
            modified_lines: true,
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

    /// Files listed while the picker is open are matched as they come and
    /// merged in by score; the selection stays on its file.
    #[test]
    fn files_that_arrive_later_are_merged_in() {
        let mut doc = doc(&["src/one.rs", "src/two.rs"]);
        let mut picker = Picker::new(doc.document.files());
        picker.push(doc.document.files(), 'r');
        picker.push(doc.document.files(), 's');
        picker.step(1);
        assert_eq!(picker.file(), Some(1));
        // Shorter, so it scores higher.
        push(&mut doc, "rs");
        push(&mut doc, "docs/readme.md");
        picker.catch_up(doc.document.files());
        let order: Vec<usize> = picker.matches.iter().map(|&(_, f)| f).collect();
        assert_eq!(order, [2, 0, 1]);
        assert_eq!(picker.file(), Some(1), "still on src/two.rs");
        assert_eq!(picker.total, 4);
        let header = picker.rows(doc.document.files(), &[], (60, 10), Progress::Reading);
        let text: String = header[0]
            .0
            .rows()
            .next()
            .expect("a row")
            .map(|(_, t)| t)
            .collect();
        assert!(text.ends_with("reading… 3/4 files"), "{text}");
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
            score("FAIR", "kernel/sched/fair.c").is_none(),
            "queries are lowercased first"
        );
        assert!(score("fair", "kernel/sched/FAIR.c").is_some());
    }
}

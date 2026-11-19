//! Searching the diff (`/`): the jobs that find matches off the main
//! thread, landing on them, and their counts.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dffy_core::document::{Found, Matches, Needle, Pos, Source};
use dffy_core::git::FileDiff;

use super::{App, Land, SEARCH_AHEAD, Search};
use crate::term::{Key, Mods};
use crate::tui::view;
use crate::tui::worker::Job;

impl App {
    /// `/` in the diff: a search line at the bottom, matched as it's typed.
    pub(super) fn start_search(&mut self) {
        self.doc.selection = None;
        self.search_id += 1;
        let id = self.search_id;
        self.search = Some(Search {
            needle: Arc::new(Needle::new("")),
            editing: true,
            id,
            counts: vec![None; self.doc.document.files().len()],
            pending: None,
        });
        self.live_search.store(id, Ordering::Relaxed);
        self.doc.document.search = None;
    }

    /// Typing into the search: the workers look for the new pattern and
    /// matches show as they arrive; `enter` goes to the first at or after
    /// the view, `esc` gives up.
    pub(super) fn search_key(&mut self, key: Key, mods: Mods) {
        let Some(search) = &mut self.search else {
            return;
        };
        let mut text = search.needle.text.clone();
        match key {
            Key::Esc => return self.clear_search(),
            Key::Enter => {
                search.editing = false;
                if text.is_empty() {
                    return self.clear_search();
                }
                self.search_step(true, true);
                return;
            }
            Key::Backspace => {
                text.pop();
            }
            Key::Char(c) if !mods.contains(Mods::CTRL) => text.push(c),
            _ => return,
        }
        search.needle = Arc::new(Needle::new(&text));
        self.search_id += 1;
        search.id = self.search_id;
        search.counts = vec![None; self.doc.document.files().len()];
        search.pending = None;
        self.live_search.store(search.id, Ordering::Relaxed);
        self.doc.document.search = None;
    }

    pub(super) fn clear_search(&mut self) {
        self.search = None;
        self.live_search.store(u64::MAX, Ordering::Relaxed);
        self.doc.document.search = None;
        self.message("search cleared; n/N go between changes again");
    }

    /// A file's matches from a worker. They show if they're of the search,
    /// the diff, the revealed lines and the comments as they are now, and
    /// an `n` or `N` that came before them lands in them.
    pub(super) fn found(
        &mut self,
        file: usize,
        id: u64,
        (diff, hidden, blocks_version): (Option<&Arc<FileDiff>>, &[Range<usize>], u64),
        matches: Option<Matches>,
    ) {
        if self.search.as_ref().is_none_or(|s| s.id != id) {
            return;
        }
        // A scan stopped for a newer search.
        let Some(matches) = matches else { return };
        self.dirty = true;
        let document = &self.doc.document;
        let current = document.diff_of(file).map(Arc::as_ptr) == diff.map(Arc::as_ptr)
            && document.hidden_lines(file).unwrap_or_default() == hidden
            && document.blocks_version(file) == blocks_version;
        let Some(search) = self.search.as_mut().filter(|_| current) else {
            return;
        };
        if let Some(count) = search.counts.get_mut(file) {
            *count = Some(matches.len());
        }
        let land = search.pending.take_if(|(f, _)| *f == file);
        self.doc.document.search = Some(Found {
            file,
            hidden: hidden.to_vec(),
            blocks_version,
            matches: Arc::new(matches),
            current: None,
        });
        match land.map(|(_, land)| land) {
            Some(Land::First) => self.land_search_edge(false),
            Some(Land::Last) => self.land_search_edge(true),
            Some(Land::Step { forward, here }) => self.search_step(forward, here),
            None => {}
        }
    }

    /// Goes to the file shown's first match, or its `last`, as its rows
    /// show them.
    fn land_search_edge(&mut self, last: bool) {
        let file = self.doc.document.pos.file;
        let marks = (self.doc.document.search.as_ref())
            .filter(|m| m.file == file && self.doc.layout.split)
            .map(|m| Arc::clone(&m.matches));
        let by_row = marks.and_then(|matches| {
            let rows = self.match_rows(file, &matches);
            if last {
                rows.iter().max()
            } else {
                rows.iter().min()
            }
            .map(|&(_, i)| i)
        });
        self.land_search(by_row.unwrap_or(if last { usize::MAX } else { 0 }));
    }

    /// Each of `matches`' row and index, those of `file` shown: side by
    /// side, a removed line and the added line beside it share a row, so
    /// the diff's order isn't the rows'.
    fn match_rows(&mut self, file: usize, matches: &Matches) -> Vec<(Pos, usize)> {
        (0..matches.len())
            .filter_map(|i| Some((self.doc.hit_pos(file, &matches.get(i)?)?, i)))
            .collect()
    }

    /// Goes to match `i` of the file shown (its last when past the end).
    fn land_search(&mut self, i: usize) {
        let file = self.doc.document.pos.file;
        let Some(marks) = self
            .doc
            .document
            .search
            .as_mut()
            .filter(|m| m.file == file && !m.matches.is_empty())
        else {
            return;
        };
        let i = i.min(marks.matches.len() - 1);
        marks.current = Some(i);
        let hit = marks.matches.get(i);
        if let Some(pos) = hit.and_then(|hit| self.doc.hit_pos(file, &hit)) {
            self.doc.goto_match(pos);
        }
    }

    /// `n`/`N` while searching: the next or previous match, on into the
    /// next file that has one. With `here`, a match on the row at the
    /// top counts as the next one (`enter` after typing). Before the file's
    /// matches have arrived, it waits for them.
    pub(super) fn search_step(&mut self, forward: bool, here: bool) {
        let file = self.doc.document.pos.file;
        let top = Pos {
            offset: 0,
            ..self.doc.document.pos
        };
        let marks = self
            .doc
            .document
            .search
            .as_ref()
            .filter(|m| m.file == file)
            .map(|m| (Arc::clone(&m.matches), m.current));
        // Its diff is coming, or here to scan, or it's a page of comments;
        // not a commit, a failed diff or a combined diff shown as it came,
        // which have no matches.
        let scanned = self.doc.document.diff_of(file).is_some()
            || (self.doc.document.files().get(file)).is_some_and(|e| e.wants_diff() || e.is_page());
        match marks {
            Some((matches, current)) if self.doc.layout.split => {
                // Each step finds every match's row, left to right on one.
                let rows = self.match_rows(file, &matches);
                let from = current.and_then(|c| rows.iter().find(|(_, i)| *i == c).copied());
                let next = match (from, forward) {
                    (Some(from), true) => rows.iter().filter(|r| **r > from).min(),
                    (Some(from), false) => rows.iter().filter(|r| **r < from).max(),
                    (None, true) => (rows.iter())
                        .filter(|(p, _)| *p > top || here && *p == top)
                        .min(),
                    (None, false) => rows.iter().filter(|(p, _)| *p < top).max(),
                };
                if let Some(&(_, i)) = next {
                    self.land_search(i);
                    return;
                }
            }
            Some((matches, current)) => {
                let n = matches.len();
                // The matches are in the order the rows show them, the
                // comments' among the code's.
                let mut before = |i: usize, or_at: bool| {
                    (matches.get(i))
                        .and_then(|hit| self.doc.hit_pos(file, &hit))
                        .is_none_or(|p| p < top || (or_at && p == top))
                };
                let next = match (current, forward) {
                    (Some(i), true) => (i + 1 < n).then_some(i + 1),
                    (Some(i), false) => i.checked_sub(1),
                    (None, true) => {
                        let at = partition_point(n, |i| before(i, !here));
                        (at < n).then_some(at)
                    }
                    (None, false) => partition_point(n, |i| before(i, false)).checked_sub(1),
                };
                if let Some(i) = next {
                    self.land_search(i);
                    return;
                }
            }
            None if scanned => {
                if let Some(search) = &mut self.search {
                    search.pending = Some((file, Land::Step { forward, here }));
                }
                self.message("searching this file…");
                return;
            }
            None => {}
        }
        let counts = self.search_counts();
        let Some(search) = &mut self.search else {
            return;
        };
        let others: Vec<usize> = if forward {
            (file + 1..counts.len()).collect()
        } else {
            (0..file).rev().collect()
        };
        let target = others
            .iter()
            .copied()
            .find(|&f| counts[f].is_some_and(|n| n > 0));
        let unknown = others.iter().any(|&f| counts[f].is_none());
        match target {
            Some(f) => {
                search.pending = Some((f, if forward { Land::First } else { Land::Last }));
                self.doc.goto_file(f);
                let arrow = if forward {
                    "→ next file"
                } else {
                    "← previous file"
                };
                let status = self.search_status().unwrap_or_default();
                self.message(format!("{arrow} · {status}"));
            }
            None if unknown => self.message("still searching the other files…"),
            None => self.message(if forward {
                "no more matches below"
            } else {
                "no more matches above"
            }),
        }
    }

    /// Each entry's matches, once known: a commit's header has none to
    /// search, nor has the file shown when there's no diff of it to scan
    /// (it failed, or it's shown as it came); a page has its comments';
    /// entries that arrived since the search began are still to be
    /// counted.
    fn search_counts(&self) -> Vec<Option<usize>> {
        let Some(search) = &self.search else {
            return Vec::new();
        };
        let shown = self.doc.document.pos.file;
        let unscanned = |f: usize, e: &view::Entry| {
            f == shown && !e.wants_diff() && self.doc.document.diff_of(f).is_none()
        };
        self.doc
            .document
            .files()
            .iter()
            .enumerate()
            .map(|(f, e)| {
                if e.is_page() {
                    search.counts.get(f).copied().flatten()
                } else if e.is_commit() || unscanned(f, e) {
                    Some(0)
                } else {
                    search.counts.get(f).copied().flatten()
                }
            })
            .collect()
    }

    /// `match 3/17` for the status bar, `17+` while files are still
    /// being counted.
    pub(super) fn search_status(&self) -> Option<String> {
        self.search.as_ref().filter(|s| !s.editing)?;
        let counts = self.search_counts();
        let total: usize = counts.iter().flatten().sum();
        let more = if counts.iter().any(Option::is_none) {
            "+"
        } else {
            ""
        };
        let at = self
            .doc
            .document
            .search
            .as_ref()
            .and_then(|m| m.current.map(|i| (m.file, i)));
        let index = at.map(|(file, i)| {
            counts[..file.min(counts.len())]
                .iter()
                .flatten()
                .sum::<usize>()
                + i
                + 1
        });
        Some(match index {
            Some(i) => format!("match {i}/{total}{more}"),
            None => format!("{total}{more} matches"),
        })
    }

    /// The search's counts of the files not shown, nearest after the one
    /// shown first.
    pub(super) fn search_jobs(&self) -> Vec<Job> {
        let Some(search) = self.search.as_ref().filter(|s| !s.needle.text.is_empty()) else {
            return Vec::new();
        };
        let files = self.doc.document.files();
        let n = files.len();
        let start = self.doc.document.pos.file;
        (1..n)
            .map(|k| (start + k) % n)
            .filter(|&f| {
                (!files[f].is_commit() || files[f].is_page())
                    && search.counts.get(f).is_none_or(Option::is_none)
            })
            .take(SEARCH_AHEAD)
            .map(|file| Job::Search {
                file,
                version: files[file].version(),
                change: (!files[file].is_page()).then(|| files[file].change().clone()),
                piped: match &files[file].source() {
                    Source::Piped(piped) => Some(Arc::clone(piped)),
                    Source::Repo | Source::Commit(_) | Source::Page { .. } => None,
                },
                // Its comments as placed, once they are.
                threads: (self.comments.as_ref())
                    .and_then(|c| {
                        if files[file].is_page() {
                            Some(c.conversation())
                        } else {
                            c.threads(&files[file].change().path)
                        }
                    })
                    .unwrap_or_default(),
                needle: Arc::clone(&search.needle),
                id: search.id,
                live: Arc::clone(&self.live_search),
            })
            .collect()
    }
}

/// The first of `0..n` that `before` is false for, `before` being true for
/// a run of them from 0 and false after.
fn partition_point(n: usize, mut before: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi) = (0, n);
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

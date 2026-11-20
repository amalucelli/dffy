//! Searching the diff (`/`): the jobs that find matches off the main
//! thread, landing on them, and their counts.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dffy_core::document::{Found, Hit, Match, Matches, Needle, Pos, Side, Source};
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
        match marks.and_then(|matches| self.split_edge(file, &matches, last)) {
            Some((pos, i)) => self.land_search_at(i, pos),
            None => self.land_search(if last { usize::MAX } else { 0 }),
        }
    }

    /// Line match `j` of `matches`' list, in `file`: its row and its
    /// index among matches of both kinds.
    fn line_key(&mut self, file: usize, matches: &Matches, j: usize) -> Option<(Pos, usize)> {
        let pos = self.doc.hit_pos(file, &Hit::Line(*matches.list.get(j)?))?;
        Some((pos, matches.of_line(j)))
    }

    /// The row and index of the first of `matches` in the order the rows
    /// show them, or the `last`.
    fn split_edge(&mut self, file: usize, matches: &Matches, last: bool) -> Option<(Pos, usize)> {
        let mut best = None;
        for side in [Side::Old, Side::New] {
            let ids = matches.on_side(side);
            let n = ids.len();
            let key = (0..n).find_map(|k| {
                let k = if last { n - 1 - k } else { k };
                self.line_key(file, matches, ids[k] as usize)
            });
            best = nearer(best, key, !last);
        }
        for (k, b) in matches.blocks.iter().enumerate() {
            let pos = self.doc.hit_pos(file, &Hit::Block(*b));
            best = nearer(best, pos.map(|p| (p, b.after as usize + k)), !last);
        }
        best
    }

    /// Side by side, the match after `mark` in the order the rows show
    /// them (before it, going back), with its row. Each side's matches
    /// are in the order of that side's rows; the sides interleave where
    /// removed and added lines share rows, so the step finds each side's
    /// nearest and takes the nearer. A comment's match is looked up one
    /// by one: comments are few.
    fn split_step(
        &mut self,
        file: usize,
        matches: &Matches,
        mark: Mark,
        forward: bool,
    ) -> Option<(Pos, usize)> {
        let mut best = match mark {
            Mark::Hit(pos, i) => match matches.get(i) {
                Some(Hit::Line(m)) => self.line_step(file, matches, &m, pos, forward),
                _ => self.sides_step(file, matches, mark, forward),
            },
            Mark::Top { .. } => self.sides_step(file, matches, mark, forward),
        };
        for (k, b) in matches.blocks.iter().enumerate() {
            let key = (self.doc.hit_pos(file, &Hit::Block(*b))).map(|p| (p, b.after as usize + k));
            best = nearer(best, key.filter(|&key| mark.passed(key, forward)), forward);
        }
        best
    }

    /// [`App::split_step`] among the code's matches from any `mark`: a
    /// binary search of each side's.
    fn sides_step(
        &mut self,
        file: usize,
        matches: &Matches,
        mark: Mark,
        forward: bool,
    ) -> Option<(Pos, usize)> {
        let mut best = None;
        for side in [Side::Old, Side::New] {
            let ids = matches.on_side(side);
            // The first match after the mark going forward, the first not
            // before it going back; one without a row is passed over.
            let at = partition_point(ids.len(), |k| {
                self.line_key(file, matches, ids[k] as usize)
                    .is_none_or(|key| {
                        if forward {
                            !mark.passed(key, true)
                        } else {
                            mark.passed(key, false)
                        }
                    })
            });
            let key = if forward {
                (at..ids.len()).find_map(|k| self.line_key(file, matches, ids[k] as usize))
            } else {
                (0..at)
                    .rev()
                    .find_map(|k| self.line_key(file, matches, ids[k] as usize))
            };
            best = nearer(best, key, forward);
        }
        best
    }

    /// [`App::split_step`] among the code's matches from `hit`, the code's
    /// match on the row at `pos`. The next on its line is on the same
    /// row; the other side's line on that row places that side's matches
    /// around it; past both, the row of one candidate places the other.
    /// So a step looks up a row or two, not one per match.
    fn line_step(
        &mut self,
        file: usize,
        matches: &Matches,
        hit: &Match,
        pos: Pos,
        forward: bool,
    ) -> Option<(Pos, usize)> {
        let (side, list) = (hit.side, &matches.list);
        let (own, other) = (matches.on_side(side), matches.on_side(side.other()));
        let line = |j: u32| list[j as usize].line as usize;
        let here = own.partition_point(|&j| {
            let o = &list[j as usize];
            (o.line, o.start) < (hit.line, hit.start)
        });
        let at = *own.get(here)?;
        let next = if forward {
            own.get(here + 1)
        } else {
            here.checked_sub(1).map(|k| &own[k])
        };
        if let Some(&j) = next.filter(|&&j| line(j) == line(at)) {
            return Some((pos, matches.of_line(j as usize)));
        }
        let across = if let Some(shown) = self.doc.row_lines(pos)[side.other().index()] {
            let first = other.partition_point(|&j| line(j) < shown);
            let past = first + other[first..].partition_point(|&j| line(j) == shown);
            // A line's matches all come before or all after another line's
            // in the diff.
            if first < past && (other[first] > at) == forward {
                let j = if forward {
                    other[first]
                } else {
                    other[past - 1]
                };
                return Some((pos, matches.of_line(j as usize)));
            }
            if forward {
                other.get(past)
            } else {
                first.checked_sub(1).map(|k| &other[k])
            }
        } else {
            // The row shows this side alone. The other side's matches below
            // it start near where the diff has them around this one: the
            // added lines after a removed line begin on rows above, beside
            // the removed lines before it, and a syntax diff can draw
            // removed lines below an added line that follows them.
            let near = other.partition_point(|&j| j < at);
            let below = gallop(near, other.len(), |k| {
                self.line_key(file, matches, other[k] as usize)
                    .is_none_or(|(p, _)| p <= pos)
            });
            if forward {
                other.get(below)
            } else {
                below.checked_sub(1).map(|k| &other[k])
            }
        };
        let (next, across) = (next.map(|&j| j as usize), across.map(|&j| j as usize));
        let (j, across) = match (next, across) {
            (Some(j), Some(across)) => (j, across),
            (Some(j), None) | (None, Some(j)) => return self.line_key(file, matches, j),
            (None, None) => return None,
        };
        let Some(there) = self.line_key(file, matches, across) else {
            return self.line_key(file, matches, j);
        };
        // This side's lines are in the order of their rows.
        let mine = match self.doc.row_lines(there.0)[side.index()] {
            Some(shown) if list[j].line as usize == shown => (there.0, matches.of_line(j)),
            Some(shown) if (shown < list[j].line as usize) == forward => return Some(there),
            _ => match self.line_key(file, matches, j) {
                Some(key) => key,
                None => return Some(there),
            },
        };
        nearer(Some(mine), Some(there), forward)
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

    /// Goes to match `i` of the file shown, on the row at `pos`.
    fn land_search_at(&mut self, i: usize, pos: Pos) {
        if let Some(marks) = &mut self.doc.document.search {
            marks.current = Some(i);
        }
        self.doc.goto_match(pos);
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
                // Matches go by row, left to right on one.
                let at = current.and_then(|c| Some((self.doc.hit_pos(file, &matches.get(c)?)?, c)));
                let mark = at.map_or(Mark::Top { pos: top, here }, |(pos, c)| Mark::Hit(pos, c));
                if let Some((pos, i)) = self.split_step(file, &matches, mark, forward) {
                    self.land_search_at(i, pos);
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

/// Where a step through the matches starts: the match it's on, at its
/// row, or the row at the top, `here` counting a match on that row as
/// the next.
#[derive(Clone, Copy)]
enum Mark {
    Hit(Pos, usize),
    Top { pos: Pos, here: bool },
}

impl Mark {
    /// Whether the match at `key` (its row and index) comes after the
    /// mark, or before it when not `forward`.
    fn passed(self, key: (Pos, usize), forward: bool) -> bool {
        match (self, forward) {
            (Self::Hit(pos, i), true) => key > (pos, i),
            (Self::Hit(pos, i), false) => key < (pos, i),
            (Self::Top { pos, here }, true) => key.0 > pos || here && key.0 == pos,
            (Self::Top { pos, .. }, false) => key.0 < pos,
        }
    }
}

/// The earlier of two matches (row, then index), or the later when not
/// `first`.
fn nearer(a: Option<(Pos, usize)>, b: Option<(Pos, usize)>, first: bool) -> Option<(Pos, usize)> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if (a < b) == first { a } else { b }),
        (a, b) => a.or(b),
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

/// [`partition_point`], probing outward from `near` at doubling distances
/// first, so an answer near it costs a few calls.
fn gallop(near: usize, n: usize, mut before: impl FnMut(usize) -> bool) -> usize {
    let (mut lo, mut hi, mut step) = (0, n, 1);
    if near < n && before(near) {
        lo = near + 1;
        while lo < hi {
            let probe = (lo + step - 1).min(hi - 1);
            if !before(probe) {
                hi = probe;
                break;
            }
            lo = probe + 1;
            step *= 2;
        }
    } else {
        hi = near.min(n);
        while lo < hi {
            let probe = hi.saturating_sub(step).max(lo);
            if before(probe) {
                lo = probe + 1;
                break;
            }
            hi = probe;
            step *= 2;
        }
    }
    lo + partition_point(hi - lo, |k| before(lo + k))
}

//! The viewer's frame: one border around the file tree and the diff. Its
//! top edge names the file shown and the hunk or function the view is in;
//! its bottom edge is the status bar (with `status-bar = top`, the status
//! goes in the top edge after the title instead). Screens too short for a
//! frame, a phone with its keyboard up, get the same text on a title row
//! and a status row. Either way the chrome takes two rows and the rest is
//! code.
//!
//! When the text doesn't fit, the hunk context shrinks first, then the
//! file's directory, cut from the left so the name and counts stay. In the
//! status the syntax-diff note goes first, then the mode words, the key
//! hints and the hunk position; a note shown in the mode words' place
//! outlasts the hints and is then cut short rather than dropped.

use std::ops::Range;

use unicode_width::UnicodeWidthStr;

use super::tree::{fit_left, fit_right};
use crate::term::Grid;
use crate::theme::{Role, Theme};

/// Screens at least this tall get the frame.
pub(super) const FRAME_MIN_ROWS: usize = 16;
/// The least room the hunk context is shown in, cut short.
const MIN_CONTEXT: usize = 10;
/// The least room a note is cut to before the key hints give way.
const MIN_NOTE: usize = 16;

pub(super) type Spans = Vec<(String, Role)>;

fn width(spans: &[(String, Role)]) -> usize {
    spans.iter().map(|(text, _)| text.width()).sum()
}

/// Which end of a title's flexible part is cut when it doesn't fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cut {
    /// A directory: its last folders are the ones worth keeping.
    Left,
    /// A commit's subject.
    Right,
}

/// The file shown, as the title says it on wide screens:
/// `modified · staged · src/tui/app.rs · +3 -1`, or for a commit,
/// `commit 1fcf726 · Fix the thing`.
pub(super) struct Title {
    /// The state and staging words, or `commit` and the short id; given
    /// up only when nothing else is left.
    pub head: Spans,
    /// What shrinks: the directory, or the commit's subject.
    pub flex: (String, Role, Cut),
    /// The file's name, cut from the left only when nothing else is left.
    pub name: (String, Role),
    pub tail: Spans,
}

impl Title {
    fn full_width(&self) -> usize {
        width(&self.head) + self.flex.0.width() + self.name.0.width() + width(&self.tail)
    }

    /// The title in `room` columns: the flexible part cut first, then the
    /// head dropped, then the name cut.
    fn fit(&self, room: usize) -> Spans {
        let (flex, flex_role, cut) = &self.flex;
        let fixed = width(&self.head) + self.name.0.width() + width(&self.tail);
        let mut out = Vec::new();
        if fixed < room {
            out.extend(self.head.iter().cloned());
            let left = room - fixed;
            let flex = match cut {
                _ if flex.width() <= left => flex.clone(),
                // A lone `…` says nothing.
                _ if left < 2 => String::new(),
                Cut::Left => fit_left(flex, left),
                Cut::Right => fit_right(flex, left),
            };
            out.push((flex, *flex_role));
            out.push(self.name.clone());
            out.extend(self.tail.iter().cloned());
            return out;
        }
        if self.name.0.width() + width(&self.tail) <= room {
            out.push(self.name.clone());
            out.extend(self.tail.iter().cloned());
        } else if room > 0 {
            out.push((fit_left(&self.name.0, room), self.name.1));
        }
        out
    }
}

/// The title on screens under 100 columns: a file switcher whose arrows
/// and name take taps, `‹ 3/12 app.rs ›`, then the counts and the staging
/// as they fit.
pub(super) struct Switcher {
    pub position: String,
    pub name: String,
    pub counts: Option<(usize, usize)>,
    pub staging: Option<&'static str>,
}

impl Switcher {
    /// The switcher in `room` columns, and the columns from its start of
    /// the `‹`, the name and the `›`, each arrow with a cell of padding on
    /// both sides.
    fn fit(&self, room: usize) -> (Spans, [Range<usize>; 3]) {
        let fixed = 3 + self.position.width() + 1 + 3;
        let name = fit_left(&self.name, room.saturating_sub(fixed).max(1));
        let mut out: Spans = vec![
            (" ‹ ".to_owned(), Role::Plain),
            (self.position.clone(), Role::Plain),
            (" ".to_owned(), Role::Plain),
            (name.clone(), Role::FileHeader),
            (" › ".to_owned(), Role::Plain),
        ];
        let name_end = 3 + self.position.width() + 1 + name.width();
        let mut left = room.saturating_sub(fixed + name.width());
        if let Some((added, removed)) = self.counts {
            let (added, removed) = (format!("+{added}"), format!(" -{removed}"));
            if added.len() + removed.len() <= left {
                left -= added.len() + removed.len();
                out.push((added, Role::Added));
                out.push((removed, Role::Removed));
            }
        }
        if let Some(staging) = self.staging {
            let text = format!(" · {staging}");
            if text.width() <= left {
                out.push((text, Role::Dim));
            }
        }
        (out, [0..3, 3..name_end, name_end..name_end + 3])
    }
}

/// The title row's text: the file's title on screens at least 100 columns
/// wide, the switcher under that.
pub(super) enum Heading {
    File(Title),
    Switcher(Switcher),
}

/// What the status says, each part dropped or cut as room runs out.
#[derive(Default)]
pub(super) struct Status {
    /// What's compared: `uncommitted`, `v6.0..v6.1`, `stdin`, `commit 3/57`.
    pub chip: String,
    /// The file of how many: `3/269`.
    pub position: Option<String>,
    /// The hunk at the top of how many: `1/4 hunk`.
    pub hunk: Option<String>,
    /// The mode words (`split syntax wrap watch`), or a note shown in
    /// their place for a moment.
    pub middle: Option<String>,
    /// `middle` is a note: cut short rather than dropped.
    pub note: bool,
    /// Where the diff search is (`match 3/17`) and whether the tree has a
    /// filter: kept longer than the other mode words.
    pub search: Option<String>,
    /// What the syntax diff of the file says, if anything.
    pub extra: Option<String>,
    /// The key hints, longest first; each shorter one stands in when the
    /// one before doesn't fit.
    pub hints: [Option<String>; 3],
}

impl Status {
    fn chip(&self) -> Spans {
        vec![(format!(" {} ", self.chip), Role::StatusTag)]
    }

    /// The position and the hunk: `3/269 · 1/4 hunk`.
    fn place(&self, hunk: bool) -> Spans {
        let mut out = Vec::new();
        if let Some(position) = &self.position {
            out.push((position.clone(), Role::Plain));
        }
        if let Some(h) = self.hunk.as_ref().filter(|_| hunk) {
            if !out.is_empty() {
                out.push((" · ".to_owned(), Role::Dim));
            }
            out.push((h.clone(), Role::Plain));
        }
        out
    }

    fn lead(&self, hunk: bool) -> Spans {
        let mut out = self.chip();
        let place = self.place(hunk);
        if !place.is_empty() {
            out.push((" ".to_owned(), Role::Plain));
            out.extend(place);
        }
        out
    }

    fn middle(&self, room: Option<usize>) -> Option<Spans> {
        let text = self.middle.as_ref()?;
        let role = if self.note { Role::Plain } else { Role::Dim };
        let text = match room {
            Some(room) if text.width() > room => fit_right(text, room),
            _ => text.clone(),
        };
        Some(vec![(text, role)])
    }
}

/// Groups of spans placed along a row: each at its column.
type Placed = Vec<(usize, Spans)>;

/// A group's columns: the spans with a space on each side, so the border
/// shows between groups.
fn group_width(spans: &[(String, Role)]) -> usize {
    width(spans) + 2
}

/// The status in the columns `segments` give: its first groups in the
/// first segment when there are two (the tree's border) and they fit
/// there, the rest in the last, the hints at its right end.
fn place_status(status: &Status, segments: &[Range<usize>]) -> Placed {
    let Some(main) = segments.last().cloned() else {
        return Vec::new();
    };
    let dim = |text: &Option<String>| text.as_ref().map(|t| vec![(t.clone(), Role::Dim)]);
    let [long, medium, short] = &status.hints;
    let hints = [None, dim(short), dim(medium), dim(long)];
    let (extra, search) = (dim(&status.extra), dim(&status.search));
    // (extra, middle whole, hints, search, hunk) kept, from all down to
    // none; the hints shorten before they go. A note outlasts the hints
    // and the search, and is then cut rather than dropped.
    let levels: &[(bool, bool, usize, bool, bool)] = if status.note {
        &[
            (true, true, 3, true, true),
            (false, true, 3, true, true),
            (false, true, 2, true, true),
            (false, true, 1, true, true),
            (false, true, 0, true, true),
            (false, true, 0, false, true),
            (false, false, 0, false, true),
            (false, false, 0, false, false),
        ]
    } else {
        &[
            (true, true, 3, true, true),
            (false, true, 3, true, true),
            (false, true, 2, true, true),
            (false, true, 1, true, true),
            (false, false, 1, true, true),
            (false, false, 0, true, true),
            (false, false, 0, false, true),
            (false, false, 0, false, false),
        ]
    };
    let len = |r: &Range<usize>| r.end.saturating_sub(r.start);
    for &(with_extra, with_middle, with_hints, with_search, with_hunk) in levels {
        let lead = status.lead(with_hunk);
        let mut placed: Placed = Vec::new();
        let mut x = main.start;
        // On the tree's border, the lead if it fits, else the chip.
        let chip = status.chip();
        let place = status.place(with_hunk);
        if segments.len() > 1 && group_width(&lead) <= len(&segments[0]) {
            placed.push((segments[0].start, lead));
        } else if segments.len() > 1 && group_width(&chip) <= len(&segments[0]) {
            placed.push((segments[0].start, chip));
            if !place.is_empty() {
                x += group_width(&place);
                placed.push((main.start, place));
            }
        } else {
            x += group_width(&lead);
            placed.push((main.start, lead));
        }
        let hints = hints[with_hints].clone();
        let right = hints.as_ref().map_or(0, |h| group_width(h));
        let room = main.end.saturating_sub(x + right);
        let others: Vec<Spans> = [
            search.clone().filter(|_| with_search),
            extra.clone().filter(|_| with_extra),
        ]
        .into_iter()
        .flatten()
        .collect();
        let others_width: usize = others.iter().map(|g| group_width(g)).sum();
        let mut groups = Vec::new();
        if with_middle {
            groups.extend(status.middle(None));
        } else if status.note && room >= others_width + MIN_NOTE + 2 {
            groups.extend(status.middle(Some(room - others_width - 2)));
        }
        groups.extend(others);
        let need: usize = groups.iter().map(|g| group_width(g)).sum();
        if x + need + right > main.end {
            continue;
        }
        for group in groups {
            let w = group_width(&group);
            placed.push((x, group));
            x += w;
        }
        placed.extend(hints.map(|h| (main.end - right, h)));
        return placed;
    }
    // Only the chip and the position left, and still too long.
    let text: String = status.lead(false).into_iter().map(|(t, _)| t).collect();
    let cut = fit_right(text.trim(), len(&main).saturating_sub(2));
    vec![(main.start, vec![(cut, Role::StatusTag)])]
}

/// Puts `spans` at `x` on row `y`, a space before and after, up to `end`.
/// Returns the column after.
fn put(
    grid: &mut Grid,
    x: usize,
    y: usize,
    end: usize,
    spans: &[(String, Role)],
    theme: &Theme,
) -> usize {
    let mut x = grid.put_str(x, y, end, " ", theme.style(Role::Plain));
    for (text, role) in spans {
        x = grid.put_str(x, y, end, text, theme.style(*role));
    }
    grid.put_str(x, y, end, " ", theme.style(Role::Plain))
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent facts about one frame"
)]
pub(super) struct Chrome {
    pub framed: bool,
    pub status_top: bool,
    /// The file tree's columns when it's beside the diff; its right
    /// border is the frame's divider.
    pub tree: Option<usize>,
    /// The tree covers everything (a narrow screen): framed, it has the
    /// rows of the frame too.
    pub overlay: bool,
    /// The tree's bottom border shows its filter being typed.
    pub tree_editing: bool,
    pub heading: Option<Heading>,
    pub context: Option<Spans>,
    pub status: Status,
}

impl Chrome {
    /// The rows of the title and of the status: the frame's top and bottom
    /// edges, or with `status-bar = top` both on the top; without a frame,
    /// the first and last rows, swapped for `top`.
    fn rows(&self, height: usize) -> (usize, usize) {
        let last = height.saturating_sub(1);
        if !self.status_top {
            (0, last)
        } else if self.framed {
            (0, 0)
        } else {
            (last, 0)
        }
    }

    /// Draws the frame's lines; the code, the tree and the text go over
    /// them.
    pub(super) fn outline(&self, grid: &mut Grid, theme: &Theme) {
        let (w, h) = (grid.width(), grid.height());
        if !self.framed || self.overlay || w < 2 || h < 2 {
            return;
        }
        let style = theme.style(Role::Dim);
        let start = self.tree.map_or(0, |t| t.saturating_sub(1));
        for (y, left, right) in [(0, "┌", "┐"), (h - 1, "└", "┘")] {
            for x in start..w {
                grid.put_str(x, y, w, "─", style);
            }
            if self.tree.is_none() {
                grid.put_str(0, y, w, left, style);
            }
            grid.put_str(w - 1, y, w, right, style);
        }
        for y in 1..h - 1 {
            if self.tree.is_none() {
                grid.put_str(0, y, w, "│", style);
            }
            grid.put_str(w - 1, y, w, "│", style);
        }
    }

    /// Draws the title, the hunk context and the status. Returns the
    /// switcher's row and the columns of its arrows and name, for taps.
    pub(super) fn text(
        &self,
        grid: &mut Grid,
        theme: &Theme,
    ) -> Option<(usize, [Range<usize>; 3])> {
        let (w, h) = (grid.width(), grid.height());
        if h < 2 || (self.framed && self.overlay) {
            return None;
        }
        let (title_row, status_row) = self.rows(h);
        // Where text goes on the diff's part of each row: inside the frame
        // with a line of border either side, or the whole row without one.
        let (title_room, status_rooms) = if self.framed {
            let main = self.tree.unwrap_or(1) + 1..w.saturating_sub(2);
            let mut bottom = vec![if self.tree.is_none() {
                3..w.saturating_sub(2)
            } else {
                main.clone()
            }];
            if let Some(t) = self.tree.filter(|_| !self.tree_editing) {
                bottom.insert(0, 3..t.saturating_sub(2));
            }
            (main, bottom)
        } else {
            (0..w, std::iter::once(1..w).collect())
        };
        let mut switcher = None;
        let mut title_end = title_room.end;
        if self.framed && self.status_top {
            // The status takes the top edge's right part: what the whole
            // title leaves, or when that's short of the chip and the
            // position, all but the title's name.
            let (full, least) = match &self.heading {
                Some(Heading::File(t)) => {
                    (t.full_width() + 2, t.name.0.width() + width(&t.tail) + 2)
                }
                Some(Heading::Switcher(s)) => {
                    let least = s.position.width() + 12;
                    (least + s.name.width(), least)
                }
                None => (0, 0),
            };
            let lead = group_width(&self.status.lead(false));
            let title = if title_room.len().saturating_sub(full) >= lead {
                full
            } else {
                least
            };
            let room = title_room.start + title.min(title_room.len())..title_room.end;
            let placed = place_status(&self.status, std::slice::from_ref(&room));
            let used: usize = placed.iter().map(|(_, s)| group_width(s)).sum();
            let start = title_room.end.saturating_sub(used);
            let mut x = start;
            for (_, spans) in &placed {
                x = put(grid, x, status_row, title_room.end, spans, theme);
            }
            title_end = start;
        } else {
            for (x, spans) in place_status(&self.status, &status_rooms) {
                let end = status_rooms.last().map_or(w, |r| r.end);
                put(grid, x, status_row, end.max(x), &spans, theme);
            }
        }
        let room = title_end.saturating_sub(title_room.start);
        // The title's room, after the context if both fit, else the
        // context cut or gone.
        let (heading, taps) = match &self.heading {
            Some(Heading::File(title)) => {
                let full = title.full_width() + 2;
                let context_room = room.saturating_sub(full);
                // A line of border stays between them.
                let context = self
                    .context
                    .as_ref()
                    .filter(|_| context_room >= MIN_CONTEXT + 3)
                    .and_then(|c| cut_spans(c, context_room - 3));
                let context_width = context.as_ref().map_or(0, |c| group_width(c) + 1);
                let spans = title.fit(room.saturating_sub(context_width + 2));
                (Some((spans, context)), None)
            }
            Some(Heading::Switcher(s)) => {
                let (spans, taps) = s.fit(room.saturating_sub(2));
                let full = width(&spans) + 2;
                let context_room = room.saturating_sub(full);
                let context = self
                    .context
                    .as_ref()
                    .filter(|_| context_room >= MIN_CONTEXT + 3)
                    .and_then(|c| cut_spans(c, context_room - 3));
                (Some((spans, context)), Some(taps))
            }
            None => (None, None),
        };
        if let Some((spans, context)) = heading {
            // The switcher has its own padding; a title gets a space.
            let x = title_room.start;
            let after = if taps.is_some() {
                let mut x = x;
                for (text, role) in &spans {
                    x = grid.put_str(x, title_row, title_end, text, theme.style(*role));
                }
                x
            } else {
                put(grid, x, title_row, title_end, &spans, theme)
            };
            if let Some(taps) = taps {
                switcher = Some((title_row, taps.map(|r| r.start + x..r.end + x)));
            }
            if let Some(context) = context {
                let start = title_end.saturating_sub(group_width(&context)).max(after);
                put(grid, start, title_row, title_end, &context, theme);
            }
        }
        switcher
    }
}

/// `spans` within `room` columns, cut on the right; hunk numbers cut
/// short would read as other numbers, so they go whole or not at all.
fn cut_spans(spans: &[(String, Role)], room: usize) -> Option<Spans> {
    let mut out = Vec::new();
    let mut left = room;
    for (text, role) in spans {
        if text.width() <= left {
            left -= text.width();
            out.push((text.clone(), *role));
        } else {
            if *role != Role::HunkHeader {
                out.push((fit_right(text, left), *role));
            }
            break;
        }
    }
    (width(&out) > 0).then_some(out)
}

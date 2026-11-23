//! Searching a file's diff for literal text, and the words a double click
//! selects.

use std::ops::Range;
use std::sync::{Arc, LazyLock};

use gix::bstr::BString;

use super::{Attach, Block, BlockId, Side, gaps, line_on};
use crate::diff::{Hunk, LineKind, Text};
use crate::git::{FileChange, FileDiff, Status};
use crate::view::{Kind, Row, display_text, side_rows};

/// What a search looks for: a literal string, case ignored unless it has a
/// capital letter (smart case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Needle {
    /// The string as typed.
    pub text: String,
    fold: bool,
}

impl Needle {
    /// A search for `text`.
    #[must_use]
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            fold: !text.chars().any(char::is_uppercase),
        }
    }

    /// Where it occurs in `line`, not overlapping, as byte ranges.
    #[must_use]
    pub fn find_all(&self, line: &[u8]) -> Vec<Range<usize>> {
        let needle = self.text.as_bytes();
        let mut out = Vec::new();
        if needle.is_empty() || needle.len() > line.len() {
            return out;
        }
        let mut i = 0;
        while i + needle.len() <= line.len() {
            let window = &line[i..i + needle.len()];
            let hit = if self.fold {
                window.eq_ignore_ascii_case(needle)
            } else {
                window == needle
            };
            if hit {
                out.push(i..i + needle.len());
                i += needle.len();
            } else {
                i += 1;
            }
        }
        out
    }
}

/// A match of a search: the side and line (0-based) it's on, and its
/// bytes within the line. Kept small: a one-letter search of a large file
/// has a match on most lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    /// The side whose text matched.
    pub side: Side,
    /// The line of that side.
    pub line: u32,
    /// First byte of the match within the line's text.
    pub start: u32,
    /// Past its last byte.
    pub end: u32,
}

/// A match of a search in a block's text (see [`super::Block`]): the
/// block, the line of its text, its bytes within the line, and where it
/// comes among the matches in the diff's lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockMatch {
    /// The block whose text matched.
    pub block: BlockId,
    /// The line of its text, from 0.
    pub line: u32,
    /// First byte of the match within the line.
    pub start: u32,
    /// Past its last byte.
    pub end: u32,
    /// How many of [`Matches::list`] come before it.
    pub after: u32,
}

/// A match of either kind, as [`Matches::get`] orders them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hit {
    /// In a line of the diff.
    Line(Match),
    /// In a block's text.
    Block(BlockMatch),
}

/// How a file is read alone, for [`find`]: the side, the hunks opened in
/// place, which show both sides, and the syntax diff's hunks when they show
/// (see [`super::Document::reading_in`]).
#[derive(Debug, Clone)]
pub struct Reading {
    /// The side read.
    pub side: Side,
    /// The hunks opened in place, in order.
    pub opened: Vec<usize>,
    /// The syntax diff's hunks, which pair the hidden lines the rows stand
    /// for otherwise than the line diff's; `None` while the line diff shows.
    pub structural: Option<Arc<[Hunk]>>,
}

/// The same syntax diff is the same allocation: hunks aren't compared.
impl PartialEq for Reading {
    fn eq(&self, other: &Self) -> bool {
        self.side == other.side
            && self.opened == other.opened
            && match (&self.structural, &other.structural) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for Reading {}

/// A file's matches in the order the diff shows them (unified), and for
/// each side the matches on it by line, to find a row's matches while
/// drawing. Matches in blocks' text are a second list, each placed among
/// the first; [`Matches::get`] goes through both in order.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Matches {
    /// Every match in a line of the diff, in order.
    pub list: Vec<Match>,
    by_side: [Vec<u32>; 2],
    /// Every match in a block's text, in order.
    pub blocks: Vec<BlockMatch>,
    /// Matches on lines hidden while a side is read alone, the other
    /// side's: counted, never listed or visited.
    pub other_side: usize,
}

impl Matches {
    /// The matches on `line` of `side`, as indices into `list`.
    pub fn on(&self, side: Side, line: usize) -> impl Iterator<Item = usize> + '_ {
        let ids = &self.by_side[side.index()];
        let line_of = |i: &u32| self.list[*i as usize].line as usize;
        let from = ids.partition_point(|i| line_of(i) < line);
        ids[from..]
            .iter()
            .take_while(move |i| line_of(i) == line)
            .map(|&i| i as usize)
    }

    /// The matches on `side`, as indices into `list`, in order: by line,
    /// then left to right.
    #[must_use]
    pub fn on_side(&self, side: Side) -> &[u32] {
        &self.by_side[side.index()]
    }

    /// Matches of both kinds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.list.len() + self.blocks.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Match `i` of both kinds in the order the diff and its blocks show
    /// them; `None` past the last.
    #[must_use]
    pub fn get(&self, i: usize) -> Option<Hit> {
        // Block match `k` is match `after + k` of both, which grows with `k`.
        let at = |k: usize| self.blocks[k].after as usize + k;
        let (mut k, mut end) = (0, self.blocks.len());
        while k < end {
            let mid = k + (end - k) / 2;
            if at(mid) < i {
                k = mid + 1;
            } else {
                end = mid;
            }
        }
        if k < self.blocks.len() && at(k) == i {
            return Some(Hit::Block(self.blocks[k]));
        }
        self.list.get(i - k).copied().map(Hit::Line)
    }

    /// Where match `i` of [`Matches::list`] comes among matches of both
    /// kinds, as [`Matches::get`] numbers them.
    #[must_use]
    pub fn of_line(&self, i: usize) -> usize {
        i + (self.blocks).partition_point(|b| b.after as usize <= i)
    }

    /// The matches in block `id`'s text.
    pub fn in_block(&self, id: BlockId) -> impl Iterator<Item = &BlockMatch> + '_ {
        self.blocks.iter().filter(move |m| m.block == id)
    }
}

/// Every match of `needle` on the lines a file's diff shows: removed
/// lines' old text, added and unchanged lines' new text (an unchanged line
/// counts once), in the order the diff shows them; and in the text of
/// `blocks`, each after the matches of the line it hangs under. `hidden`
/// has the new lines the gaps of unchanged lines hide under their
/// separators, in order (see [`super::Document::hidden_lines`]); without
/// it, only the hunks' lines count, and the blocks in gaps come where their
/// gap is. `blocks` are as [`super::Document::search_blocks`] gives them.
///
/// With `reading`, a side read alone (see
/// [`super::Document::reading_in`]), every line of that side counts once,
/// in order, unchanged ones with that side's text, but for lines `hidden`
/// still hides (a piped file's gaps never open). The other side's lines
/// only count toward [`Matches::other_side`]; blocks on them come where
/// the row standing for them is, as the reading's hunks pair them. A hunk
/// opened in place counts as the diff counts it.
///
/// `None` once `cancelled` says so: it's asked every 4096 lines, so a
/// search that's been typed past stops.
pub fn find(
    diff: &FileDiff,
    hidden: Option<&[Range<usize>]>,
    reading: Option<&Reading>,
    blocks: &[Block],
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    let mut scan = Scan {
        matches: Matches::default(),
        lines: 0,
        diff,
        blocks,
        needle,
        cancelled,
    };
    let mut on_lines: [Vec<(usize, u32)>; 2] = [Vec::new(), Vec::new()];
    for (b, i) in blocks.iter().zip(0u32..) {
        if let Attach::Line { side, line } = b.at {
            on_lines[side.index()].push((line, i));
        }
    }
    for side in &mut on_lines {
        side.sort_unstable();
    }
    // The blocks on lines `old` and `new`, old side first, then each in
    // the host's order, as rows hang them.
    let on = |old: Range<usize>, new: Range<usize>| {
        let mut found: Vec<(u8, u32)> = Vec::new();
        for (side, range) in [(0, old), (1, new)] {
            let on = &on_lines[side as usize];
            let from = on.partition_point(|&(l, _)| l < range.start);
            let to = on.partition_point(|&(l, _)| l < range.end).max(from);
            found.extend(on[from..to].iter().map(|&(_, i)| (side, i)));
        }
        found.sort_unstable();
        found.into_iter().map(|(_, i)| i)
    };
    let edge = |at: Attach| {
        (blocks.iter().zip(0u32..))
            .filter(move |(b, _)| b.at == at)
            .map(|(_, i)| i)
    };
    // Blocks on the whole entry hang under its first row, so they come
    // after its line when it shows one: the first gap's first line, shown,
    // or a whole added or deleted file's first line.
    let gaps = gaps(diff);
    let whole = diff.hunks.len() == 1
        && gaps.iter().all(|g| g.len == 0)
        && (diff.old.is_empty() || diff.new.is_empty());
    let mut lead = match (gaps.first(), hidden) {
        (Some(g), Some(hidden)) if g.len > 0 => !hidden.iter().any(|r| r.contains(&g.new)),
        (Some(g), _) => g.len == 0 && whole,
        (None, _) => false,
    };
    if !lead {
        scan.blocks(edge(Attach::Start))?;
    }
    let gap_side = reading.map_or(Side::New, |r| r.side);
    // The hunks the rows show; both lists cover the same lines.
    let hunks = (reading.and_then(|r| r.structural.as_deref())).unwrap_or(&diff.hunks);
    // A hunk's rows read alone, built anew in one buffer for each.
    let mut rows = Vec::new();
    let mut runs = hidden.map(|h| h.iter().peekable());
    for (g, gap) in gaps.iter().enumerate() {
        let to_old = |new: usize| new - gap.new + gap.old;
        if let Some(runs) = &mut runs {
            let (mut line, end) = (gap.new, gap.new + gap.len);
            while line < end {
                while runs.next_if(|r| r.end <= line).is_some() {}
                if let Some(run) = runs.peek().filter(|r| r.start <= line) {
                    let run = line..run.end.min(end);
                    scan.blocks(on(to_old(run.start)..to_old(run.end), run.clone()))?;
                    line = run.end;
                    continue;
                }
                scan.line(gap_side, [to_old(line), line][gap_side.index()])?;
                if !blocks.is_empty() {
                    if std::mem::take(&mut lead) {
                        scan.blocks(edge(Attach::Start))?;
                    }
                    scan.blocks(on(to_old(line)..to_old(line) + 1, line..line + 1))?;
                }
                line += 1;
            }
        } else {
            let new = gap.new..gap.new + gap.len;
            scan.blocks(on(gap.old..gap.old + gap.len, new))?;
        }
        let Some(hunk) = hunks.get(g) else {
            continue;
        };
        if let Some(r) = reading.filter(|r| r.opened.binary_search(&g).is_err()) {
            read_hunk(&mut scan, (hunk, r, &mut rows), &mut lead, (&on, &edge))?;
            continue;
        }
        for l in &hunk.lines {
            let (side, line, old, new) = match l.kind {
                LineKind::Removed => (Side::Old, l.old, l.old..l.old + 1, 0..0),
                LineKind::Added => (Side::New, l.new, 0..0, l.new..l.new + 1),
                LineKind::Context => (Side::New, l.new, l.old..l.old + 1, l.new..l.new + 1),
            };
            scan.line(side, line)?;
            // Most files have no blocks; their lines skip the lookup.
            if blocks.is_empty() {
                continue;
            }
            if std::mem::take(&mut lead) {
                scan.blocks(edge(Attach::Start))?;
            }
            scan.blocks(on(old, new))?;
        }
    }
    scan.blocks(edge(Attach::End))?;
    Some(scan.matches)
}

/// [`find`]'s scan of `hunk` while a side is read alone, as `reading`
/// says, its rows built in `rows`: `lead` says the blocks under the first
/// row are still to come, `on` gives the blocks on ranges of old and new
/// lines in the order rows hang them, and `edge` those at either end.
fn read_hunk<C, O, OI, E, EI>(
    scan: &mut Scan<'_, C>,
    (hunk, reading, rows): (&Hunk, &Reading, &mut Vec<Row>),
    lead: &mut bool,
    (on, edge): (&O, &E),
) -> Option<()>
where
    C: Fn() -> bool,
    O: Fn(Range<usize>, Range<usize>) -> OI,
    OI: IntoIterator<Item = u32>,
    E: Fn(Attach) -> EI,
    EI: IntoIterator<Item = u32>,
{
    let side = reading.side;
    let any_blocks = !scan.blocks.is_empty();
    // The blocks on `this` lines of the side read and `other` lines of the
    // other side, hung under one row.
    let mut hang = |scan: &mut Scan<'_, C>, this: Range<usize>, other: Range<usize>| {
        if std::mem::take(lead) {
            scan.blocks(edge(Attach::Start))?;
        }
        let (old, new) = match side {
            Side::Old => (this, other),
            Side::New => (other, this),
        };
        scan.blocks(on(old, new))
    };
    // The rows read alone lay out: a hidden line's blocks hang under the
    // line it pairs with, or the row of the lines left unpaired.
    let kind = |i: usize| {
        let l = &hunk.lines[i];
        let kind = match l.kind {
            LineKind::Context => Kind::Context,
            LineKind::Removed => Kind::Removed,
            LineKind::Added => Kind::Added,
        };
        (kind, l.block_start)
    };
    rows.clear();
    side_rows(side, hunk.lines.len(), kind, rows);
    for &row in rows.iter() {
        match row {
            Row::One { line, paired } => {
                let l = &hunk.lines[line];
                let this = line_on(l, side);
                let other = match (l.kind, paired) {
                    (LineKind::Context, _) => Some(line_on(l, side.other())),
                    (_, Some(p)) => {
                        let hid = line_on(&hunk.lines[p], side.other());
                        scan.count(side.other(), hid)?;
                        Some(hid)
                    }
                    (_, None) => None,
                };
                scan.line(side, this)?;
                if any_blocks {
                    hang(scan, this..this + 1, other.map_or(0..0, |o| o..o + 1))?;
                }
            }
            Row::Hidden { from, n } => {
                let start = line_on(&hunk.lines[from], side.other());
                let hid = start..start + n as usize;
                for line in hid.clone() {
                    scan.count(side.other(), line)?;
                }
                if any_blocks {
                    hang(scan, 0..0, hid)?;
                }
            }
            Row::Header | Row::Line(_) | Row::Pair(..) | Row::Marker => {}
        }
    }
    Some(())
}

/// [`find`] in an entry without lines, a sheet: only in its `blocks`, those
/// under its first row, then those under its last.
pub fn find_blocks(
    blocks: &[Block],
    needle: &Needle,
    cancelled: impl Fn() -> bool,
) -> Option<Matches> {
    static NO_LINES: LazyLock<FileDiff> = LazyLock::new(|| FileDiff {
        change: FileChange {
            path: BString::default(),
            status: Status::Modified,
            old: None,
            new: None,
            staging: None,
        },
        hunks: Vec::new(),
        old: Text::new(Vec::new()),
        new: Text::new(Vec::new()),
        binary: false,
        too_large: false,
        note: None,
    });
    find(&NO_LINES, None, None, blocks, needle, cancelled)
}

/// A search under way (see [`find`]).
struct Scan<'a, C> {
    matches: Matches,
    /// Lines scanned so far, of the diff's and of blocks' text.
    lines: usize,
    diff: &'a FileDiff,
    blocks: &'a [Block],
    needle: &'a Needle,
    cancelled: C,
}

impl<C: Fn() -> bool> Scan<'_, C> {
    /// Counts a line about to be scanned; `None` once the search is
    /// cancelled.
    fn tick(&mut self) -> Option<()> {
        let stop = self.lines.is_multiple_of(4096) && (self.cancelled)();
        self.lines += 1;
        (!stop).then_some(())
    }

    /// Adds the matches on line `line` of `side` of the diff.
    fn line(&mut self, side: Side, line: usize) -> Option<()> {
        self.tick()?;
        let text = match side {
            Side::Old => self.diff.old.line(line),
            Side::New => self.diff.new.line(line),
        };
        let text = display_text(text.strip_suffix(b"\n").unwrap_or(text));
        let line = u32::try_from(line).unwrap_or(u32::MAX);
        let matches = &mut self.matches;
        for bytes in self.needle.find_all(text) {
            let id = u32::try_from(matches.list.len()).unwrap_or(u32::MAX);
            matches.by_side[side.index()].push(id);
            matches.list.push(Match {
                side,
                line,
                start: u32::try_from(bytes.start).unwrap_or(u32::MAX),
                end: u32::try_from(bytes.end).unwrap_or(u32::MAX),
            });
        }
        Some(())
    }

    /// Counts the matches on line `line` of `side`, the side not read, in
    /// [`Matches::other_side`].
    fn count(&mut self, side: Side, line: usize) -> Option<()> {
        self.tick()?;
        let text = match side {
            Side::Old => self.diff.old.line(line),
            Side::New => self.diff.new.line(line),
        };
        let text = display_text(text.strip_suffix(b"\n").unwrap_or(text));
        self.matches.other_side += self.needle.find_all(text).len();
        Some(())
    }

    /// Adds the matches in the text of each of `blocks`, by index, in order.
    fn blocks(&mut self, blocks: impl IntoIterator<Item = u32>) -> Option<()> {
        for i in blocks {
            let block = &self.blocks[i as usize];
            let after = u32::try_from(self.matches.list.len()).unwrap_or(u32::MAX);
            for (line, text) in block.text.split('\n').enumerate() {
                self.tick()?;
                for bytes in self.needle.find_all(text.as_bytes()) {
                    self.matches.blocks.push(BlockMatch {
                        block: block.id,
                        line: u32::try_from(line).unwrap_or(u32::MAX),
                        start: u32::try_from(bytes.start).unwrap_or(u32::MAX),
                        end: u32::try_from(bytes.end).unwrap_or(u32::MAX),
                        after,
                    });
                }
            }
        }
        Some(())
    }
}

/// What a double click groups characters by.
#[derive(PartialEq)]
enum Class {
    Word,
    Blank,
    Other,
}

/// The word around byte `at` of `text`, as byte offsets: letters, digits
/// and `_`, or a run of blanks, or the one other character there. A line
/// that isn't valid UTF-8 gives the byte at `at`.
#[must_use]
pub fn word(text: &[u8], at: usize) -> (usize, usize) {
    let s = String::from_utf8_lossy(text);
    // Lossy text has the same offsets when the line is valid UTF-8.
    if s.len() != text.len() {
        return (at, (at + 1).min(text.len()));
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c == ' ' || c == '\t' {
            Class::Blank
        } else {
            Class::Other
        }
    };
    let Some(here) = s.get(at..).and_then(|rest| rest.chars().next()) else {
        return (at, at);
    };
    let kind = class(here);
    if kind == Class::Other {
        return (at, at + here.len_utf8());
    }
    let start = s[..at]
        .char_indices()
        .rev()
        .take_while(|&(_, c)| class(c) == kind)
        .last()
        .map_or(at, |(i, _)| i);
    let end = s[at..]
        .char_indices()
        .find(|&(_, c)| class(c) != kind)
        .map_or(s.len(), |(i, _)| at + i);
    (start, end)
}

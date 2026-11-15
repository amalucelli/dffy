//! Line diffing (histogram with a Myers fallback, as git does) and the hunk
//! model the renderers consume. Everything here is plain data over byte
//! slices: no git, no terminal, no shared state.

mod histogram;
mod intraline;
mod myers;

use std::ops::Range;

use rustc_hash::FxHashMap;

pub(crate) use intraline::paired_emphasis;
pub use intraline::{
    Emphasis, emphasize, emphasize_lines, emphasize_with, pair, similar, tokenize,
};

/// Lines longer than this get no intra-line emphasis; minified files would
/// otherwise spend most of the diff time on token diffs nobody can read.
pub const MAX_INTRALINE_LINE_BYTES: usize = 4096;

/// A file's bytes split into lines. Each line keeps its terminator, so a
/// missing newline at end of file is a real difference, as in git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Text {
    bytes: Vec<u8>,
    /// Start offset of every line plus a final entry at `bytes.len()`.
    starts: Vec<usize>,
}

/// Empty: zero lines. A derived default would break the final-entry
/// invariant, and [`Text::len`] would underflow on binary files' sides.
impl Default for Text {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl Text {
    /// Indexes the line starts of `bytes` in one pass. Empty bytes are zero
    /// lines; a final line without a newline still counts.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        let mut starts = Vec::with_capacity(bytes.len() / 32 + 2);
        starts.push(0);
        starts.extend(memchr::memchr_iter(b'\n', &bytes).map(|i| i + 1));
        if starts.last() != Some(&bytes.len()) {
            starts.push(bytes.len());
        }
        Self { bytes, starts }
    }

    /// The whole text, terminators included.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Number of lines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.starts.len() - 1
    }

    /// No lines, which means no bytes either.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Line `i`, including its terminator if it has one.
    ///
    /// # Panics
    ///
    /// If `i >= self.len()`.
    #[must_use]
    pub fn line(&self, i: usize) -> &[u8] {
        &self.bytes[self.starts[i]..self.starts[i + 1]]
    }

    /// The byte offset where line `i` starts; `self.len()` gives the end.
    ///
    /// # Panics
    ///
    /// If `i > self.len()`.
    #[must_use]
    pub fn offset(&self, i: usize) -> usize {
        self.starts[i]
    }

    /// Every line in order, each with its terminator as in [`Text::line`].
    #[must_use]
    pub fn lines(&self) -> impl ExactSizeIterator<Item = &[u8]> + '_ {
        self.starts.windows(2).map(|w| &self.bytes[w[0]..w[1]])
    }
}

/// How whitespace takes part in line equality (git's `-w` and `-b`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Whitespace {
    /// Lines are equal only byte for byte, git's default.
    #[default]
    Exact,
    /// `-w`: lines are equal if they match after removing all whitespace.
    IgnoreAll,
    /// `-b`: runs of whitespace compare equal to each other, and whitespace
    /// at the end of a line is ignored.
    IgnoreChange,
}

/// The git flags that shape a file diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Options {
    /// Lines of context around each change (`-U`).
    pub context: usize,
    /// How whitespace takes part in line equality (`-w`, `-b`).
    pub whitespace: Whitespace,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            context: 3,
            whitespace: Whitespace::Exact,
        }
    }
}

/// Per-line change flags: `removed[i]` for line `i` of the old text, `added[j]`
/// for line `j` of the new text. Unflagged lines pair up in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// One flag per old line.
    pub removed: Vec<bool>,
    /// One flag per new line.
    pub added: Vec<bool>,
}

/// Which side a hunk line comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    /// On both sides, unchanged.
    Context,
    /// Only in the old text.
    Removed,
    /// Only in the new text.
    Added,
}

/// One displayed line of a hunk. On the side a line doesn't belong to, its
/// index is the insertion point: the index of the next line there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The side the line comes from.
    pub kind: LineKind,
    /// 0-based index into the old [`Text`], or the insertion point for an
    /// added line.
    pub old: usize,
    /// 0-based index into the new [`Text`], or the insertion point for a
    /// removed line.
    pub new: usize,
    /// Byte ranges within the line to emphasize, relative to the line start.
    /// Empty for context lines and for changed lines with no similar partner.
    pub emphasis: Vec<Range<usize>>,
    /// Set only by the structural diff ([`crate::structural`]), on changed
    /// lines whose code wasn't edited.
    pub unchanged: Unchanged,
    /// A new block of changed lines starts at this line: the removed lines
    /// before it aren't paired with the added lines after it, by [`pair`]
    /// or in split view's rows. The line diff never sets it; the structural
    /// diff does to keep lines whose code matched side by side.
    pub block_start: bool,
    /// A changed line paired with one on the other side as its edited
    /// version, rather than wholly removed or added. [`hunks`] sets it by
    /// [`emphasize`]'s pairing, the structural diff by matched tokens; a hunk
    /// built without emphasis gets it from [`pair`].
    pub paired: bool,
}

/// Whether a changed line's code is the same as on the other side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Unchanged {
    /// Edited, or not judged: every line of the line diff.
    #[default]
    No,
    /// The same tokens in the same order sit on the other side, in the same
    /// block of changed lines: only whitespace or line breaks changed.
    /// Renderers dim it.
    Layout,
    /// A whole statement or item that moved: a changed line on the other
    /// side, elsewhere in the file or reordered within its block, has the
    /// same tokens in the same order. Renderers mark it apart from both edits
    /// and [`Unchanged::Layout`]: unlike a reformat, a move can change what
    /// the code does.
    Moved,
}

/// A run of changes with surrounding context. Starts are 0-based line
/// indices; for an empty range the start is where the lines would go. In
/// git's 1-based header that is `start + 1` when the length is non-zero and
/// `start` otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// First old line covered, 0-based.
    pub old_start: usize,
    /// Old lines covered: context plus removed.
    pub old_len: usize,
    /// First new line covered, 0-based.
    pub new_start: usize,
    /// New lines covered: context plus added.
    pub new_len: usize,
    /// In display order: in each block of changes, the removed lines come
    /// before the added ones.
    pub lines: Vec<Line>,
}

/// Line-level changes between `old` and `new` using histogram diff.
#[must_use]
pub fn changes(old: &Text, new: &Text, whitespace: Whitespace) -> Changes {
    diff_with(old, new, whitespace, histogram::diff)
}

/// Hunks between `old` and `new`, with intra-line emphasis on paired lines.
#[must_use]
pub fn hunks(old: &Text, new: &Text, options: &Options) -> Vec<Hunk> {
    let mut hunks = hunks_without_emphasis(old, new, options);
    for hunk in &mut hunks {
        let paired = paired_emphasis(old, new, hunk, None, None);
        for (line, (emphasis, paired)) in hunk.lines.iter_mut().zip(paired) {
            line.emphasis = emphasis;
            line.paired = paired;
        }
    }
    hunks
}

/// Hunks between `old` and `new` when each side is cut into units that
/// change only whole, such as the blocks of a rendered document: one
/// `(identity, lines)` pair per unit, in order from the side's first line.
/// Units are matched by identity with the line diff's algorithm; a matched
/// pair of the same length is unchanged, and every other unit's lines are
/// removed or added. Lines past the last unit count as changed. Lines carry
/// no emphasis, as in [`crate::git::FileDiff::hunks`].
#[must_use]
pub fn unit_hunks(
    old: &Text,
    new: &Text,
    units: [&[(u64, usize)]; 2],
    context: usize,
) -> Vec<Hunk> {
    let mut ids: FxHashMap<u64, u32> = FxHashMap::default();
    let mut intern = |side: &[(u64, usize)]| -> Vec<u32> {
        side.iter()
            .map(|&(id, _)| {
                let next = u32::try_from(ids.len()).unwrap_or(u32::MAX);
                *ids.entry(id).or_insert(next)
            })
            .collect()
    };
    let (a, b) = (intern(units[0]), intern(units[1]));
    let starts = |side: &[(u64, usize)]| {
        side.iter()
            .scan(0, |at, &(_, len)| {
                let start = *at;
                *at += len;
                Some(start..*at)
            })
            .collect::<Vec<_>>()
    };
    let (old_lines, new_lines) = (starts(units[0]), starts(units[1]));
    let mut changes = Changes {
        removed: vec![true; old.len()],
        added: vec![true; new.len()],
    };
    for (i, j) in common(&a, &b, ids.len()) {
        let (o, n) = (old_lines[i].clone(), new_lines[j].clone());
        if o.len() == n.len() && o.end <= old.len() && n.end <= new.len() {
            changes.removed[o].fill(false);
            changes.added[n].fill(false);
        }
    }
    build_hunks(old, &changes, context)
}

pub(crate) fn hunks_without_emphasis(old: &Text, new: &Text, options: &Options) -> Vec<Hunk> {
    let changes = changes(old, new, options.whitespace);
    build_hunks(old, &changes, options.context)
}

/// Index pairs of a longest common subsequence of two id sequences (ids
/// below `tokens`), found as for lines: histogram with a Myers fallback.
pub(crate) fn common(a: &[u32], b: &[u32], tokens: usize) -> Vec<(usize, usize)> {
    let prefix = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suffix = a[prefix..]
        .iter()
        .rev()
        .zip(b[prefix..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let mut removed = vec![false; a.len()];
    let mut added = vec![false; b.len()];
    let (ma, mb) = (prefix..a.len() - suffix, prefix..b.len() - suffix);
    histogram::diff(
        &a[ma.clone()],
        &b[mb.clone()],
        tokens,
        &mut removed[ma],
        &mut added[mb],
    );
    let mut pairs = Vec::with_capacity(a.len().min(b.len()));
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        match (removed[i], added[j]) {
            (true, _) => i += 1,
            (false, true) => j += 1,
            (false, false) => {
                pairs.push((i, j));
                i += 1;
                j += 1;
            }
        }
    }
    pairs
}

/// Runs `algorithm` on the interned lines of `old` and `new` after trimming
/// the common prefix and suffix. The algorithm gets the token count so it can
/// size per-token tables.
fn diff_with(
    old: &Text,
    new: &Text,
    whitespace: Whitespace,
    algorithm: impl FnOnce(&[u32], &[u32], usize, &mut [bool], &mut [bool]),
) -> Changes {
    match whitespace {
        Whitespace::Exact => diff_keys(
            [old.len(), new.len()],
            |i| old.line(i),
            |j| new.line(j),
            algorithm,
        ),
        mode => {
            let mut arena = Vec::with_capacity(old.bytes.len() + new.bytes.len());
            let old_ranges = normalize(old, mode, &mut arena);
            let new_ranges = normalize(new, mode, &mut arena);
            diff_keys(
                [old.len(), new.len()],
                |i| &arena[old_ranges[i].clone()],
                |j| &arena[new_ranges[j].clone()],
                algorithm,
            )
        }
    }
}

/// [`diff_with`] over each side's lines as compared, `old(i)` and `new(j)`.
/// The common prefix and suffix are trimmed before interning: a large file
/// with a small change then hashes only the lines around the change.
fn diff_keys<'a>(
    [old_len, new_len]: [usize; 2],
    old: impl Fn(usize) -> &'a [u8],
    new: impl Fn(usize) -> &'a [u8],
    algorithm: impl FnOnce(&[u32], &[u32], usize, &mut [bool], &mut [bool]),
) -> Changes {
    let shorter = old_len.min(new_len);
    let prefix = (0..shorter).take_while(|&i| old(i) == new(i)).count();
    let suffix = (1..=shorter - prefix)
        .take_while(|&k| old(old_len - k) == new(new_len - k))
        .count();
    let (old_mid, new_mid) = (prefix..old_len - suffix, prefix..new_len - suffix);
    let interned = intern(old_mid.clone().map(old), new_mid.clone().map(new));
    let mut changes = Changes {
        removed: vec![false; old_len],
        added: vec![false; new_len],
    };
    algorithm(
        &interned.old,
        &interned.new,
        interned.tokens,
        &mut changes.removed[old_mid],
        &mut changes.added[new_mid],
    );
    changes
}

struct Interned {
    old: Vec<u32>,
    new: Vec<u32>,
    tokens: usize,
}

#[expect(
    clippy::expect_used,
    reason = "2^32 distinct lines take at least 4 GiB of text; sides read from git stop at MAX_DIFF_BYTES (64 MiB), other callers aren't capped"
)]
fn intern<'a>(
    old: impl ExactSizeIterator<Item = &'a [u8]>,
    new: impl ExactSizeIterator<Item = &'a [u8]>,
) -> Interned {
    let mut map: FxHashMap<&[u8], u32> = FxHashMap::default();
    map.reserve(old.len() + new.len());
    let mut id = |line: &'a [u8]| {
        let next = u32::try_from(map.len()).expect("fewer than 2^32 distinct lines");
        *map.entry(line).or_insert(next)
    };
    let old = old.map(&mut id).collect();
    let new = new.map(&mut id).collect();
    Interned {
        old,
        new,
        tokens: map.len(),
    }
}

/// Git's whitespace class (`isspace` in the C locale).
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// Appends each line of `text` to `arena` in normalized form and returns the
/// ranges of the normalized lines.
fn normalize(text: &Text, mode: Whitespace, arena: &mut Vec<u8>) -> Vec<Range<usize>> {
    text.lines()
        .map(|line| {
            let start = arena.len();
            match mode {
                Whitespace::Exact => arena.extend_from_slice(line),
                Whitespace::IgnoreAll => {
                    arena.extend(line.iter().copied().filter(|&b| !is_space(b)));
                }
                Whitespace::IgnoreChange => {
                    let end = line
                        .iter()
                        .rposition(|&b| !is_space(b))
                        .map_or(0, |i| i + 1);
                    let mut in_space = false;
                    for &b in &line[..end] {
                        if is_space(b) {
                            if !in_space {
                                arena.push(b' ');
                            }
                            in_space = true;
                        } else {
                            arena.push(b);
                            in_space = false;
                        }
                    }
                }
            }
            start..arena.len()
        })
        .collect()
}

/// Groups change flags into hunks with `context` lines around each change,
/// merging changes whose gap is at most twice the context, as git does.
fn build_hunks(old: &Text, changes: &Changes, context: usize) -> Vec<Hunk> {
    let groups = change_groups(changes);
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < groups.len() {
        let mut j = i;
        while j + 1 < groups.len() && groups[j + 1].0.start - groups[j].0.end <= 2 * context {
            j += 1;
        }
        let first = &groups[i];
        let last = &groups[j];
        let lead = context.min(first.0.start);
        let trail = context.min(old.len() - last.0.end);
        let old_start = first.0.start - lead;
        let new_start = first.1.start - lead;
        let mut hunk = Hunk {
            old_start,
            old_len: last.0.end + trail - old_start,
            new_start,
            new_len: last.1.end + trail - new_start,
            lines: Vec::new(),
        };
        let (mut o, mut n) = (old_start, new_start);
        for (old_range, new_range) in &groups[i..=j] {
            push_context(&mut hunk.lines, &mut o, &mut n, old_range.start);
            hunk.lines.extend(old_range.clone().map(|old| Line {
                kind: LineKind::Removed,
                old,
                new: new_range.start,
                emphasis: Vec::new(),
                unchanged: Unchanged::No,
                block_start: false,
                paired: false,
            }));
            hunk.lines.extend(new_range.clone().map(|new| Line {
                kind: LineKind::Added,
                old: old_range.end,
                new,
                emphasis: Vec::new(),
                unchanged: Unchanged::No,
                block_start: false,
                paired: false,
            }));
            o = old_range.end;
            n = new_range.end;
        }
        push_context(&mut hunk.lines, &mut o, &mut n, last.0.end + trail);
        hunks.push(hunk);
        i = j + 1;
    }
    hunks
}

fn push_context(lines: &mut Vec<Line>, old: &mut usize, new: &mut usize, old_end: usize) {
    while *old < old_end {
        lines.push(Line {
            kind: LineKind::Context,
            old: *old,
            new: *new,
            emphasis: Vec::new(),
            unchanged: Unchanged::No,
            block_start: false,
            paired: false,
        });
        *old += 1;
        *new += 1;
    }
}

/// Maximal runs of changed lines as (old range, new range) pairs.
fn change_groups(changes: &Changes) -> Vec<(Range<usize>, Range<usize>)> {
    let (removed, added) = (&changes.removed, &changes.added);
    let mut groups = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < removed.len() || j < added.len() {
        let changed_here = removed.get(i) == Some(&true) || added.get(j) == Some(&true);
        if !changed_here {
            i += 1;
            j += 1;
            continue;
        }
        let (i0, j0) = (i, j);
        while removed.get(i) == Some(&true) {
            i += 1;
        }
        while added.get(j) == Some(&true) {
            j += 1;
        }
        groups.push((i0..i, j0..j));
    }
    groups
}

#[cfg(test)]
mod tests;

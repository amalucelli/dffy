//! Syntax-aware diffing: which tokens changed, judged by the syntax trees
//! of both sides rather than by lines.
//!
//! The result is still the line diff. [`diff`] takes a file's line hunks
//! and returns the same hunks (same ranges, so applying them still turns
//! the old text into the new one) with three things changed in each block
//! of removed and added lines:
//! - emphasis covers tokens that have no equal on the other side, and
//!   tokens whose equal is out of order (swapped arguments);
//! - a line whose code wasn't edited gets no emphasis and is flagged
//!   [`Line::unchanged`]: [`Unchanged::Layout`] when the same tokens sit in
//!   the same order and place on the other side, [`Unchanged::Moved`] when
//!   it's a whole statement or item found elsewhere. Anything short of that
//!   (a token inserted mid-line, a reordered argument on a line of its own)
//!   shows as a change: a reader must never take an edit for a reformat;
//! - removed and added lines whose tokens matched each other are put side
//!   by side, splitting the block where needed ([`Line::block_start`]).
//!
//! Matching anchors top-level nodes first and only looks inside the regions
//! that changed, each under its own budget. A region over budget, and every
//! block touching it, keeps the token-level emphasis of the line diff
//! ([`crate::diff::emphasize`]) instead of failing the file. Parse errors
//! don't stop it either: an error subtree is matched as one opaque piece,
//! or token by token against another one.
//!
//! The line diff has the last word on the lines it kept: matching anchors
//! on them, and afterwards a token on a kept line is matched only to the
//! token it was kept as. Otherwise, with repeated code, a line shown as
//! removed could match a copy kept elsewhere and look unchanged.

mod matcher;
mod stored;
mod tree;

use std::ops::Range;
use std::time::Duration;

use rustc_hash::FxHashMap;

use crate::cache::Cache;
use crate::diff::{
    Hunk, Line, LineKind, MAX_INTRALINE_LINE_BYTES, Text, Unchanged, common, paired_emphasis,
};
use crate::git::ObjectId;
use crate::highlight::{Language, parse_tree};
use matcher::{LeafMatch, Mark, Matching, NONE};
use tree::Tree;

pub(crate) use matcher::Budget;
pub use matcher::Regions;
pub use stored::CACHE_MIN_BYTES;

/// Sides larger than this aren't diffed structurally. It's four times the
/// highlighting cap: the largest generated tables in the kernel (1.1MB)
/// parse in about 120ms and match within the budget.
pub const MAX_BYTES: usize = 2 << 20;

/// A side that takes longer than this to parse gets the line diff.
const PARSE_BUDGET: Duration = Duration::from_millis(400);

/// Why a file has no structural diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unavailable {
    /// No bundled grammar for the file's language.
    Language,
    /// A side is over [`MAX_BYTES`].
    TooLarge,
    /// A side took too long to parse.
    Parse,
}

impl Unavailable {
    /// A short note for the user.
    #[must_use]
    pub fn note(self) -> &'static str {
        match self {
            Self::Language => "no syntax diff: unknown language",
            Self::TooLarge => "no syntax diff: file too large",
            Self::Parse => "no syntax diff: parsing took too long",
        }
    }
}

/// A file's structural diff.
#[derive(Debug, Clone)]
pub struct Structural {
    /// The line hunks it was given, rearranged and annotated (see the
    /// module docs).
    pub hunks: Vec<Hunk>,
    /// How many changed regions were matched and how many fell back.
    pub regions: Regions,
    /// No budget ran out, so any run gets the same result and it can be
    /// cached.
    complete: bool,
}

impl Structural {
    /// A note for the user when some regions fell back to the token diff.
    #[must_use]
    pub fn note(&self) -> Option<String> {
        let r = &self.regions;
        (r.fell_back > 0).then(|| {
            format!(
                "syntax diff: {} of {} changed regions too large, shown token by token",
                r.fell_back,
                r.matched + r.fell_back
            )
        })
    }
}

/// The structural diff of the file at `path` (repository-relative, for the
/// language), given its line `hunks` from [`crate::diff::hunks`] or
/// [`crate::git::Repo::file_diff`].
///
/// # Errors
///
/// [`Unavailable`] when the language has no grammar, a side is too large,
/// or parsing runs past its time budget.
pub fn diff(
    path: &[u8],
    old: &Text,
    new: &Text,
    hunks: &[Hunk],
) -> Result<Structural, Unavailable> {
    diff_with(path, old, new, hunks, Budget::DEFAULT)
}

/// [`diff`] through the disk cache: when both sides are known blobs `ids`
/// and one is at least [`CACHE_MIN_BYTES`], the result is read from
/// `cache`, or computed and stored there. A result cut short by a budget
/// isn't stored, since another run may get further.
///
/// # Errors
///
/// As [`diff`].
pub fn diff_cached(
    path: &[u8],
    old: &Text,
    new: &Text,
    hunks: &[Hunk],
    ids: [Option<ObjectId>; 2],
    cache: Option<&Cache>,
) -> Result<Structural, Unavailable> {
    let key = (|| {
        let cache = cache?;
        let [Some(old_id), Some(new_id)] = ids else {
            return None;
        };
        if old.bytes().len().max(new.bytes().len()) < CACHE_MIN_BYTES {
            return None;
        }
        let head = &new.bytes()[..new.bytes().len().min(256)];
        let lang = Language::detect(path, head)?;
        Some((cache, stored::key(lang, [old_id, new_id], hunks)))
    })();
    if let Some((cache, key)) = &key
        && let Some(s) = stored::get(cache, key)
    {
        return Ok(s);
    }
    let s = diff(path, old, new, hunks)?;
    if let Some((cache, key)) = key
        && s.complete
    {
        stored::put(cache, &key, &s);
    }
    Ok(s)
}

pub(crate) fn diff_with(
    path: &[u8],
    old: &Text,
    new: &Text,
    hunks: &[Hunk],
    budget: Budget,
) -> Result<Structural, Unavailable> {
    let head = &new.bytes()[..new.bytes().len().min(256)];
    let lang = Language::detect(path, head).ok_or(Unavailable::Language)?;
    if old.bytes().len() > MAX_BYTES || new.bytes().len() > MAX_BYTES {
        return Err(Unavailable::TooLarge);
    }
    // An added or deleted file has no lines to pair.
    if old.is_empty() || new.is_empty() || hunks.is_empty() {
        return Ok(Structural {
            hunks: hunks.to_vec(),
            regions: Regions::default(),
            complete: true,
        });
    }
    let side = |text: &Text| {
        let parsed = parse_tree(lang, text.bytes(), PARSE_BUDGET)?;
        Some(Tree::build(&parsed, text.bytes()))
    };
    let (old_tree, new_tree, lines) = std::thread::scope(|s| {
        let old_tree = s.spawn(|| side(old));
        let lines = s.spawn(|| LineMap::new(old, new, hunks));
        let new_tree = side(new);
        let lines = lines
            .join()
            .unwrap_or_else(|e| std::panic::resume_unwind(e));
        (old_tree.join().ok().flatten(), new_tree, lines)
    });
    let (Some(old_tree), Some(new_tree)) = (old_tree, new_tree) else {
        return Err(Unavailable::Parse);
    };
    let mut m = matcher::run(
        &old_tree,
        &new_tree,
        [old.bytes(), new.bytes()],
        &lines,
        budget,
    );
    reconcile(&lines, [&old_tree, &new_tree], &mut m);
    let [displaced_old, displaced_new] = displaced(&lines, [&old_tree, &new_tree], &m);
    let sides = [
        SideRef::new(
            &lines.old,
            &old_tree,
            &m.old,
            &m.words[0],
            displaced_old,
            lang.indentation_is_syntax(),
        ),
        SideRef::new(
            &lines.new,
            &new_tree,
            &m.new,
            &m.words[1],
            displaced_new,
            lang.indentation_is_syntax(),
        ),
    ];
    Ok(Structural {
        hunks: hunks.iter().map(|h| hunk(&sides, h)).collect(),
        regions: m.regions,
        complete: !m.cut_short,
    })
}

/// The line holding any byte of a text, found in a few steps rather than by
/// a binary search over the line starts: the matching asks for millions of
/// tokens on a large file.
struct LineIndex<'a> {
    text: &'a Text,
    /// The line holding the first byte of each run of `1 << SHIFT` bytes.
    buckets: Vec<u32>,
}

impl<'a> LineIndex<'a> {
    const SHIFT: u32 = 6;

    fn new(text: &'a Text) -> Self {
        let mut lines = Self {
            text,
            buckets: Vec::with_capacity((text.bytes().len() >> Self::SHIFT) + 1),
        };
        let mut line = 0;
        for bucket in 0..=text.bytes().len() >> Self::SHIFT {
            line = lines.scan(line, bucket << Self::SHIFT);
            lines.buckets.push(index(line));
        }
        lines
    }

    /// The last line starting at or before `offset`, from `line` on: the
    /// line holding it, or past the last line for the text's end.
    fn scan(&self, mut line: usize, offset: usize) -> usize {
        while line < self.text.len() && self.text.offset(line + 1) <= offset {
            line += 1;
        }
        line
    }

    fn line_of(&self, offset: usize) -> usize {
        let bucket = (offset >> Self::SHIFT).min(self.buckets.len() - 1);
        self.scan(self.buckets[bucket] as usize, offset)
    }
}

/// The lines the line diff kept, from old to new.
struct LineMap<'a> {
    old: LineIndex<'a>,
    new: LineIndex<'a>,
    /// Per old line, the run of kept lines it's in; [`NONE`] if removed.
    run: Vec<u32>,
    /// Per run, its first old line and the new line that one is kept as.
    runs: Vec<(usize, usize)>,
    /// Per new line, whether it's kept.
    new_kept: Vec<bool>,
}

impl<'a> LineMap<'a> {
    fn new(old: &'a Text, new: &'a Text, hunks: &[Hunk]) -> Self {
        let mut map = LineMap {
            old: LineIndex::new(old),
            new: LineIndex::new(new),
            run: vec![NONE; old.len()],
            runs: Vec::new(),
            new_kept: vec![false; new.len()],
        };
        let mut keep = |o: usize, n: usize| {
            let follows = o > 0
                && map.run[o - 1] != NONE
                && map.runs.last().is_some_and(|&(o0, n0)| n0 + (o - o0) == n);
            if !follows {
                map.runs.push((o, n));
            }
            map.run[o] = index(map.runs.len() - 1);
            map.new_kept[n] = true;
        };
        let mut o = 0;
        for h in hunks {
            // Lines between hunks are kept as they are.
            let delta = h.old_start.abs_diff(h.new_start);
            let shift = |o: usize| {
                if h.new_start >= h.old_start {
                    o + delta
                } else {
                    o - delta
                }
            };
            for line in o..h.old_start {
                keep(line, shift(line));
            }
            for line in &h.lines {
                if line.kind == LineKind::Context {
                    keep(line.old, line.new);
                }
            }
            o = h.old_start + h.old_len;
        }
        if let Some(h) = hunks.last() {
            let (end_old, end_new) = (h.old_start + h.old_len, h.new_start + h.new_len);
            for line in end_old..old.len() {
                keep(line, line - end_old + end_new);
            }
        }
        map
    }

    /// Where the old text `start..end` starts in the new text, when all of
    /// it is on lines the line diff kept together.
    fn kept(&self, start: usize, end: usize) -> Option<usize> {
        let first = self.old.line_of(start);
        let run = self.run[first];
        if run == NONE || self.run[self.old.line_of(end.saturating_sub(1).max(start))] != run {
            return None;
        }
        let (o0, n0) = self.runs[run as usize];
        let line = n0 + (first - o0);
        Some(self.new.text.offset(line) + (start - self.old.text.offset(first)))
    }

    /// Whether all of the new text `start..end` is on kept lines.
    fn new_is_kept(&self, start: usize, end: usize) -> bool {
        let lines = self.new.line_of(start)..=self.new.line_of(end.saturating_sub(1).max(start));
        self.new_kept[lines].iter().all(|&k| k)
    }

    /// The block of changed lines the text at `offset` of side `side` is in.
    /// Blocks are numbered by the runs of kept lines before them, so a
    /// block has the same number on both sides.
    fn block(&self, side: usize, offset: usize) -> usize {
        if side == 0 {
            let line = self.old.line_of(offset);
            self.runs.partition_point(|&(o, _)| o < line)
        } else {
            let line = self.new.line_of(offset);
            self.runs.partition_point(|&(_, n)| n < line)
        }
    }
}

/// Per side and leaf, whether a matched leaf on a changed line is out of
/// order: its partner is in another block of changed lines, or it's left
/// out of the heaviest chain of matches in order on both sides. The chain
/// favors punctuation, so swapped arguments are what's out of order, not
/// the commas between them.
fn displaced(lines: &LineMap, trees: [&Tree; 2], m: &Matching) -> [Vec<bool>; 2] {
    let [to, tn] = trees;
    let mut out = [vec![false; to.leaves.len()], vec![false; tn.leaves.len()]];
    // (old leaf, new leaf, weight), in old order.
    let mut pairs: Vec<(usize, usize, u64)> = Vec::new();
    for (i, s) in m.old.iter().enumerate() {
        let l = to.leaves[i];
        if s.mark != Mark::Same
            || s.partner == NONE
            || lines.kept(l.start as usize, l.end as usize).is_some()
        {
            continue;
        }
        let j = s.partner as usize;
        if lines.block(0, l.start as usize) == lines.block(1, tn.leaves[j].start as usize) {
            pairs.push((i, j, if l.informative { 1 } else { 2 }));
        } else {
            out[0][i] = true;
        }
    }
    // Heaviest chain with non-decreasing new leaves (partners can be many
    // to one), through a Fenwick tree of the best chain ending at or before
    // each new leaf.
    let mut best = vec![(0u64, NONE); tn.leaves.len() + 1];
    let mut prev = vec![NONE; pairs.len()];
    let mut end = (0u64, NONE);
    for (k, &(_, j, weight)) in pairs.iter().enumerate() {
        let mut found = (0u64, NONE);
        let mut x = j + 1;
        while x > 0 {
            if best[x].0 > found.0 {
                found = best[x];
            }
            x &= x - 1;
        }
        prev[k] = found.1;
        let here = (found.0 + weight, index(k));
        if here.0 > end.0 {
            end = here;
        }
        let mut x = j + 1;
        while x < best.len() {
            if here.0 > best[x].0 {
                best[x] = here;
            }
            x += x & x.wrapping_neg();
        }
    }
    let mut chain = vec![false; pairs.len()];
    let mut k = end.1;
    while k != NONE {
        chain[k as usize] = true;
        k = prev[k as usize];
    }
    for (&(i, _, _), in_chain) in pairs.iter().zip(chain) {
        out[0][i] = !in_chain;
    }
    for (j, s) in m.new.iter().enumerate() {
        let l = tn.leaves[j];
        if s.mark != Mark::Same
            || s.partner == NONE
            || lines.new_is_kept(l.start as usize, l.end as usize)
        {
            continue;
        }
        let i = s.partner as usize;
        out[1][j] = out[0][i]
            || lines.block(0, to.leaves[i].start as usize) != lines.block(1, l.start as usize);
    }
    out
}

/// Makes the matching agree with the line diff: a token on a kept line is
/// matched to the token it was kept as, and never to one on a changed line.
/// Otherwise a changed line could match code the line diff kept elsewhere
/// and look unchanged, while its real counterpart sits unseen in context.
/// Tokens of changed lines that lose their partner this way are matched
/// among themselves again, in order.
fn reconcile(lines: &LineMap, trees: [&Tree; 2], m: &mut Matching) {
    fn unlink(m: &mut Matching, freed: &mut [Vec<usize>; 2], side: usize, leaf: usize) {
        let states = if side == 0 { &mut m.old } else { &mut m.new };
        let partner = states[leaf].partner;
        if states[leaf].mark == Mark::Fallback {
            return;
        }
        states[leaf] = LeafMatch {
            partner: NONE,
            mark: Mark::Novel,
        };
        m.words[side].remove(&index(leaf));
        freed[side].push(leaf);
        if partner != NONE {
            let other = if side == 0 { &mut m.new } else { &mut m.old };
            other[partner as usize] = LeafMatch {
                partner: NONE,
                mark: Mark::Novel,
            };
            m.words[1 - side].remove(&partner);
            freed[1 - side].push(partner as usize);
        }
    }
    let [to, tn] = trees;
    let mut freed = [Vec::new(), Vec::new()];
    for (i, leaf) in to.leaves.iter().enumerate() {
        if m.old[i].mark == Mark::Fallback {
            continue;
        }
        let partner = m.old[i].partner;
        let Some(at) = lines.kept(leaf.start as usize, leaf.end as usize) else {
            if partner != NONE {
                let p = tn.leaves[partner as usize];
                if lines.new_is_kept(p.start as usize, p.end as usize) {
                    unlink(m, &mut freed, 0, i);
                }
            }
            continue;
        };
        let j = tn.leaves.partition_point(|l| (l.start as usize) < at);
        let twin = tn.leaves.get(j).filter(|l| {
            l.start as usize == at && l.hash == leaf.hash && m.new[j].mark != Mark::Fallback
        });
        if twin.is_some() {
            if partner as usize != j {
                unlink(m, &mut freed, 0, i);
                unlink(m, &mut freed, 1, j);
                m.old[i] = LeafMatch {
                    partner: index(j),
                    mark: Mark::Same,
                };
                m.new[j] = LeafMatch {
                    partner: index(i),
                    mark: Mark::Same,
                };
            }
        } else if partner != NONE {
            let p = tn.leaves[partner as usize];
            if !lines.new_is_kept(p.start as usize, p.end as usize) {
                unlink(m, &mut freed, 0, i);
            }
        }
    }
    let [mut freed_old, mut freed_new] = freed;
    freed_old.retain(|&i| {
        let l = to.leaves[i];
        m.old[i].mark == Mark::Novel
            && m.old[i].partner == NONE
            && lines.kept(l.start as usize, l.end as usize).is_none()
    });
    freed_new.retain(|&j| {
        let l = tn.leaves[j];
        m.new[j].mark == Mark::Novel
            && m.new[j].partner == NONE
            && !lines.new_is_kept(l.start as usize, l.end as usize)
    });
    for list in [&mut freed_old, &mut freed_new] {
        list.sort_unstable();
        list.dedup();
    }
    let mut ids: FxHashMap<u64, u32> = FxHashMap::default();
    let mut id = |h: u64| {
        let next = index(ids.len());
        *ids.entry(h).or_insert(next)
    };
    let ia: Vec<u32> = freed_old.iter().map(|&i| id(to.leaves[i].hash)).collect();
    let ib: Vec<u32> = freed_new.iter().map(|&j| id(tn.leaves[j].hash)).collect();
    for (a, b) in common(&ia, &ib, ids.len()) {
        let (i, j) = (freed_old[a], freed_new[b]);
        m.old[i] = LeafMatch {
            partner: index(j),
            mark: Mark::Same,
        };
        m.new[j] = LeafMatch {
            partner: index(i),
            mark: Mark::Same,
        };
    }
}

/// One side with its matching.
struct SideRef<'a> {
    text: &'a Text,
    lines: &'a LineIndex<'a>,
    tree: &'a Tree,
    states: &'a [LeafMatch],
    words: &'a FxHashMap<u32, Vec<Range<u32>>>,
    displaced: Vec<bool>,
    /// Displaced leaves before each leaf.
    displaced_before: Vec<u32>,
    indentation_is_syntax: bool,
}

impl<'a> SideRef<'a> {
    fn new(
        lines: &'a LineIndex<'a>,
        tree: &'a Tree,
        states: &'a [LeafMatch],
        words: &'a FxHashMap<u32, Vec<Range<u32>>>,
        displaced: Vec<bool>,
        indentation_is_syntax: bool,
    ) -> Self {
        let mut displaced_before = Vec::with_capacity(displaced.len() + 1);
        let mut count = 0;
        displaced_before.push(0);
        for &d in &displaced {
            count += u32::from(d);
            displaced_before.push(count);
        }
        SideRef {
            text: lines.text,
            lines,
            tree,
            states,
            words,
            displaced,
            displaced_before,
            indentation_is_syntax,
        }
    }

    /// The whitespace line `i` starts with.
    fn indentation(&self, i: usize) -> &[u8] {
        let line = self.text.line(i);
        let len = line
            .iter()
            .position(|b| !b.is_ascii_whitespace())
            .unwrap_or(line.len());
        &line[..len]
    }

    /// Whether `leaf` is the first thing on its line.
    fn starts_line(&self, leaf: usize) -> bool {
        let at = self.tree.leaves[leaf].start as usize;
        let line = self.lines.line_of(at);
        self.text.offset(line) + self.indentation(line).len() == at
    }

    /// The leaves with some text on line `i` (besides its terminator).
    fn line_leaves(&self, i: usize) -> Range<usize> {
        let start = self.text.offset(i);
        let end = start + content_len(self.text.line(i));
        let leaves = &self.tree.leaves;
        leaves.partition_point(|l| l.end as usize <= start)
            ..leaves.partition_point(|l| (l.start as usize) < end)
    }

    /// Whether the code that moved with `leaf` is a whole statement or item
    /// rather than an argument, element or field, whose order is part of
    /// the code around it: the largest node around it that moved whole
    /// doesn't declare a field and has no comma beside it among its
    /// siblings.
    fn moved_whole(&self, leaf: usize) -> bool {
        let tree = self.tree;
        let mut node = tree.leaves[leaf].node;
        loop {
            let parent = tree.node(node).parent;
            if parent == tree::NO_PARENT {
                return true;
            }
            let r = tree.leaves(parent);
            let moved = self.displaced_before[r.end] - self.displaced_before[r.start];
            if (moved as usize) < r.len() {
                let comma = |k: Option<usize>| {
                    k.and_then(|k| tree.leaves.get(k)).is_some_and(|l| {
                        tree.node(l.node).parent == parent
                            && &self.text.bytes()[l.start as usize..l.end as usize] == b","
                    })
                };
                let own = tree.leaves(node);
                return !tree.is_field(node)
                    && !comma(own.start.checked_sub(1))
                    && !comma(Some(own.end));
            }
            node = parent;
        }
    }
}

/// When all of `leaves` (on one side's line) are matched, each to the
/// same or a later partner than the one before, and nothing else sits
/// between their partners, the partners as a range of the other side's
/// leaves: that line's code appears there, in order and in one piece.
fn counterpart(sides: &[SideRef; 2], side: usize, leaves: Range<usize>) -> Option<Range<usize>> {
    let (this, other) = (&sides[side], &sides[1 - side]);
    let (mut lo, mut hi) = (usize::MAX, 0);
    for g in leaves.clone() {
        let s = this.states[g];
        if s.mark != Mark::Same || s.partner == NONE || (s.partner as usize) < hi {
            return None;
        }
        lo = lo.min(s.partner as usize);
        hi = s.partner as usize;
    }
    // A token cut into several on the other side matches all of them.
    let into = |m: usize| {
        let s = other.states[m];
        s.mark == Mark::Same && leaves.contains(&(s.partner as usize))
    };
    while lo > 0 && into(lo - 1) {
        lo -= 1;
    }
    while hi + 1 < other.states.len() && into(hi + 1) {
        hi += 1;
    }
    (lo..=hi).all(into).then_some(lo..hi + 1)
}

/// The line on the other side when line `i` of side `side` is code that
/// moved whole: every token is out of order (see [`displaced`]), and the
/// other line holds exactly the same tokens in the same order.
fn moved_twin(sides: &[SideRef; 2], side: usize, i: usize) -> Option<usize> {
    let (this, other) = (&sides[side], &sides[1 - side]);
    let leaves = this.line_leaves(i);
    if leaves.is_empty() || !leaves.clone().all(|g| this.displaced[g]) {
        return None;
    }
    let r = counterpart(sides, side, leaves.clone())?;
    let (first, last) = (other.tree.leaves[r.start], other.tree.leaves[r.end - 1]);
    let twin = other.lines.line_of(first.start as usize);
    if other.lines.line_of(last.end as usize - 1) != twin
        || other.line_leaves(twin) != r
        || (this.indentation_is_syntax && this.indentation(i) != other.indentation(twin))
    {
        return None;
    }
    leaves
        .into_iter()
        .all(|g| this.moved_whole(g))
        .then_some(twin)
}

/// What matching says about one changed line.
#[derive(Debug, Default)]
struct LineInfo {
    emphasis: Vec<Range<usize>>,
    unchanged: Unchanged,
    /// The line on the other side most of its tokens matched.
    partner: Option<usize>,
    /// Part of a region that fell back to the token diff.
    fallback: bool,
    /// Blank, or only whitespace.
    blank: bool,
    /// A token on the line changed, as when only the whitespace inside a
    /// multi-line string did.
    edited: bool,
}

fn hunk(sides: &[SideRef; 2], h: &Hunk) -> Hunk {
    let mut lines = Vec::with_capacity(h.lines.len());
    let mut i = 0;
    while i < h.lines.len() {
        if h.lines[i].kind == LineKind::Context {
            lines.push(h.lines[i].clone());
            i += 1;
            continue;
        }
        let start = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Removed {
            i += 1;
        }
        let plus = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Added {
            i += 1;
        }
        let block = &h.lines[start..i];
        let removed = match block[..plus - start] {
            [] => block[0].old..block[0].old,
            [ref first, .., ref last] => first.old..last.old + 1,
            [ref only] => only.old..only.old + 1,
        };
        let added = match block[plus - start..] {
            [] => block[0].new..block[0].new,
            [ref first, .., ref last] => first.new..last.new + 1,
            [ref only] => only.new..only.new + 1,
        };
        change_block(sides, removed, added, block, &mut lines);
    }
    Hunk { lines, ..*h }
}

/// Lays out one block of removed and added lines; `block` is the line
/// diff's version of it.
fn change_block(
    sides: &[SideRef; 2],
    removed: Range<usize>,
    added: Range<usize>,
    block: &[Line],
    out: &mut Vec<Line>,
) {
    let mut infos_r: Vec<LineInfo> = removed.clone().map(|i| line_info(sides, 0, i)).collect();
    let mut infos_a: Vec<LineInfo> = added.clone().map(|j| line_info(sides, 1, j)).collect();
    if infos_r.iter().chain(&infos_a).any(|l| l.fallback) {
        let tokens = Hunk {
            old_start: removed.start,
            old_len: removed.len(),
            new_start: added.start,
            new_len: added.len(),
            lines: block.to_vec(),
        };
        let emphasis = paired_emphasis(sides[0].text, sides[1].text, &tokens, None, None);
        out.extend(
            block
                .iter()
                .zip(emphasis)
                .map(|(line, (emphasis, paired))| Line {
                    emphasis,
                    paired,
                    ..line.clone()
                }),
        );
        return;
    }
    // A blank line is unchanged only in a block that is all reformatted or
    // all moved code; among new or removed code it's part of the change. A
    // blank line inside a string whose whitespace changed is itself the
    // change, so it keeps its own look.
    let follows = |l: &LineInfo| l.blank && !l.edited;
    let mut looks = infos_r
        .iter()
        .chain(&infos_a)
        .filter(|l| !follows(l))
        .map(|l| l.unchanged);
    let first = looks.next().unwrap_or(Unchanged::Layout);
    let blank = if looks.all(|l| l == first) {
        first
    } else {
        Unchanged::No
    };
    for info in infos_r.iter_mut().chain(&mut infos_a) {
        if follows(info) {
            info.unchanged = blank;
        }
    }
    let candidates: Vec<(usize, usize)> = removed
        .clone()
        .zip(&infos_r)
        .filter_map(|(r, info)| {
            let a = info.partner.filter(|a| added.contains(a))?;
            (infos_a[a - added.start].partner == Some(r)).then_some((r, a))
        })
        .collect();
    // Lines whose tokens matched each other share a row in split view: a
    // pair that the plain in-order pairing wouldn't put together starts a
    // new segment.
    let mut segments = Vec::new();
    let (mut so, mut sn) = (removed.start, added.start);
    for (r, a) in increasing(&candidates) {
        if r - so != a - sn {
            segments.push((so..r, sn..a));
            (so, sn) = (r, a);
        }
    }
    segments.push((so..removed.end, sn..added.end));
    let paired_r = |r: usize| candidates.iter().any(|&(c, _)| c == r);
    let paired_a = |a: usize| candidates.iter().any(|&(_, c)| c == a);
    let mut infos_r = infos_r.into_iter();
    let mut infos_a = infos_a.into_iter();
    for (k, (rs, ads)) in segments.into_iter().enumerate() {
        let mut first = k > 0;
        for (old, info) in rs.clone().zip(infos_r.by_ref()) {
            out.push(Line {
                kind: LineKind::Removed,
                old,
                new: ads.start,
                emphasis: info.emphasis,
                unchanged: info.unchanged,
                block_start: std::mem::take(&mut first),
                paired: paired_r(old),
            });
        }
        for (new, info) in ads.zip(infos_a.by_ref()) {
            out.push(Line {
                kind: LineKind::Added,
                old: rs.end,
                new,
                emphasis: info.emphasis,
                unchanged: info.unchanged,
                block_start: std::mem::take(&mut first),
                paired: paired_a(new),
            });
        }
    }
}

/// Emphasis, flags and partner of line `i` of side `side`.
///
/// A line is [`Unchanged::Layout`] only when its tokens appear on the other
/// side in the same order and in one piece, with nothing inserted between
/// them: a line that gained a token in its middle was edited, even if all
/// its own tokens are still there. Tokens that matched out of order (see
/// [`displaced`]) are emphasized, unless the whole line is a statement or
/// item that moved ([`Unchanged::Moved`]).
fn line_info(sides: &[SideRef; 2], side: usize, i: usize) -> LineInfo {
    let (this, other) = (&sides[side], &sides[1 - side]);
    let text = this.text;
    let start = text.offset(i);
    let line = text.line(i);
    let end = start + content_len(line);
    let leaves = this.line_leaves(i);
    let mut info = LineInfo {
        blank: line[..end - start].iter().all(u8::is_ascii_whitespace),
        ..LineInfo::default()
    };
    // Over the intra-line cap a line keeps its flags but gets no emphasis or
    // partner, as in the line diff: a minified file's one line can hold
    // tokens matched on thousands of lines.
    let long = end - start > MAX_INTRALINE_LINE_BYTES;
    let mut votes: Vec<(usize, usize)> = Vec::new();
    let (mut in_place, mut edited, mut out_of_order) = (false, false, false);
    let clip = |r: Range<usize>| {
        let r = r.start.max(start)..r.end.min(end);
        (r.start < r.end && !long).then(|| r.start - start..r.end - start)
    };
    for g in leaves.clone() {
        let leaf = this.tree.leaves[g];
        let state = this.states[g];
        match state.mark {
            Mark::Fallback => {
                info.fallback = true;
                return info;
            }
            Mark::Same if this.displaced[g] => {
                out_of_order = true;
                info.emphasis
                    .extend(clip(leaf.start as usize..leaf.end as usize));
            }
            Mark::Same => in_place = true,
            Mark::Words => {
                in_place = true;
                edited = true;
                let words = this.words.get(&index(g)).map_or(&[][..], Vec::as_slice);
                info.emphasis.extend(
                    words
                        .iter()
                        .filter_map(|w| clip(w.start as usize..w.end as usize)),
                );
            }
            Mark::Novel => {
                edited = true;
                info.emphasis
                    .extend(clip(leaf.start as usize..leaf.end as usize));
            }
        }
        if state.partner != NONE && !long {
            let p = other.tree.leaves[state.partner as usize];
            let line = other.lines.line_of(p.start as usize);
            let weight = (leaf.end - leaf.start) as usize;
            match votes.iter_mut().find(|(l, _)| *l == line) {
                Some((_, w)) => *w += weight,
                None => votes.push((line, weight)),
            }
        }
    }
    info.edited = edited;
    info.partner = votes
        .iter()
        .rev()
        .max_by_key(|&&(_, w)| w)
        .map(|&(line, _)| line);
    if !edited && !leaves.is_empty() {
        let first = leaves.start;
        let laid_out = |r: Range<usize>| {
            // Where indentation is syntax, re-indenting a line can move it
            // into or out of a block.
            !this.indentation_is_syntax
                || (this.starts_line(first) == other.starts_line(r.start)
                    && this.indentation(i)
                        == other.indentation(
                            other
                                .lines
                                .line_of(other.tree.leaves[r.start].start as usize),
                        ))
        };
        if !out_of_order && counterpart(sides, side, leaves).is_some_and(laid_out) {
            info.unchanged = Unchanged::Layout;
        } else if !in_place
            && moved_twin(sides, side, i).is_some_and(|t| moved_twin(sides, 1 - side, t) == Some(i))
        {
            info.unchanged = Unchanged::Moved;
        }
    }
    // A line of nothing but new code gets no emphasis, as an unpaired line
    // in the line diff: its color already says it's new.
    if in_place && info.unchanged == Unchanged::No {
        let content = &line[..end - start];
        info.emphasis = merge(std::mem::take(&mut info.emphasis), content);
    } else {
        info.emphasis.clear();
    }
    info
}

/// Sorted ranges merged where only whitespace separates them.
fn merge(ranges: Vec<Range<usize>>, line: &[u8]) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last)
                if last.end <= r.start
                    && line[last.end..r.start].iter().all(u8::is_ascii_whitespace) =>
            {
                last.end = r.end;
            }
            _ => out.push(r),
        }
    }
    out
}

/// The longest run of `pairs` (in order of their first element) whose
/// second elements increase too.
fn increasing(pairs: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut tails: Vec<usize> = Vec::new();
    let mut prev = vec![usize::MAX; pairs.len()];
    for (k, &(_, b)) in pairs.iter().enumerate() {
        let at = tails.partition_point(|&t| pairs[t].1 < b);
        if at > 0 {
            prev[k] = tails[at - 1];
        }
        if at == tails.len() {
            tails.push(k);
        } else {
            tails[at] = k;
        }
    }
    let mut out = Vec::with_capacity(tails.len());
    let mut k = tails.last().copied().unwrap_or(usize::MAX);
    while k != usize::MAX {
        out.push(pairs[k]);
        k = prev[k];
    }
    out.reverse();
    out
}

fn content_len(line: &[u8]) -> usize {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line).len()
}

/// A node id, byte offset or count as the arenas store it.
#[expect(
    clippy::expect_used,
    reason = "diff_with refuses sides over MAX_BYTES (2 MiB), so every id, offset and count is far below 2^32"
)]
fn index(n: usize) -> u32 {
    u32::try_from(n).expect("sides are capped at MAX_BYTES")
}

#[cfg(test)]
mod tests;

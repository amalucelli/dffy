//! Matching the tokens of two flattened trees.
//!
//! Anchoring comes first: nodes on lines the line diff kept match the node
//! they were kept as, and the rest of the top-level nodes are diffed as
//! sequences of hashes, so unchanged functions, declarations and the like
//! match without being looked into. What's left are changed regions. A
//! changed node too large to be a region by itself (a function, the one
//! array of a generated table) is paired with its counterpart and anchored
//! again on its children, so regions stay small however large the file.
//!
//! Inside a region, matching recurses: children are diffed by hash, the
//! nodes left over are paired by kind and matched inside in turn, and pairs
//! that end up sharing too little are taken apart again. Subtrees still
//! unmatched are then looked up by hash among each other, which finds code
//! that moved and code that gained or lost a wrapper. In a region with
//! parse errors, the tokens still unmatched are diffed as flat text last.
//! A final pass looks for whole lines that moved anywhere in the file.
//!
//! Uninformative nodes (punctuation alone, see [`super::tree`]) take no
//! part in anchoring, pairing by kind or the moved-code lookups: an empty
//! `;` error recovery left on each side would otherwise pair across the
//! code between them and split that code from its counterpart. They're
//! matched last, in order: among the leftovers of a node pair or region,
//! or by the flat pass in a region with parse errors.
//!
//! Each region has a work and a time budget. One that runs over is undone
//! and left to the line-level token diff; the rest of the file still gets
//! its structural result. The file as a whole has a deadline too, after
//! which every remaining region takes the token diff.

use std::ops::Range;
use std::time::{Duration, Instant};

use rustc_hash::{FxHashMap, FxHashSet};

use super::tree::{Leaf, Tree};
use super::{LineMap, index};
use crate::diff::{common, tokenize};

pub(super) const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mark {
    /// No equal token on the other side.
    Novel,
    /// Equal to its partner.
    Same,
    /// Paired with a token of the same kind whose text differs in some
    /// words; [`Matching::words`] has the changed ones.
    Words,
    /// In a region that ran over its budget.
    Fallback,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct LeafMatch {
    /// The leaf on the other side this one corresponds to, or [`NONE`].
    /// [`Mark::Novel`] leaves can have one: a renamed identifier keeps its
    /// partner so the two lines line up.
    pub partner: u32,
    pub mark: Mark,
}

impl LeafMatch {
    fn free(self) -> bool {
        self.mark == Mark::Novel && self.partner == NONE
    }
}

const UNMATCHED: LeafMatch = LeafMatch {
    partner: NONE,
    mark: Mark::Novel,
};

/// The limits matching works under; see the module docs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    /// Nodes and tokens one region may visit.
    pub region_work: u64,
    pub region_time: Duration,
    pub file_time: Duration,
}

impl Budget {
    pub(crate) const DEFAULT: Self = Self {
        region_work: 1_000_000,
        region_time: Duration::from_millis(50),
        file_time: Duration::from_millis(250),
    };
}

/// How many changed regions there were and how they went.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Regions {
    /// Regions matched structurally.
    pub matched: usize,
    /// Regions that only add or only delete nodes, with nothing to match.
    pub one_sided: usize,
    /// Regions that ran over their budget and use the token diff.
    pub fell_back: usize,
}

pub(super) struct Matching {
    pub old: Vec<LeafMatch>,
    pub new: Vec<LeafMatch>,
    /// Changed byte ranges of [`Mark::Words`] leaves, by side and leaf.
    pub words: [FxHashMap<u32, Vec<Range<u32>>>; 2],
    pub regions: Regions,
    /// Some budget ran out, so another run may match more.
    pub cut_short: bool,
}

/// A region (or the file) ran out of budget.
struct Over;

/// A changed node pair with more solid bytes than this is anchored on its
/// children instead of being matched as part of one region.
const SPLIT_BYTES: u32 = 2048;

/// The fewest tokens a subtree needs to match across the file by hash
/// alone (see [`Scope::File`]).
const MIN_FILE_MOVE_LEAVES: usize = 4;

/// Pairs of at most this many tokens in all skip the similarity check.
const SMALL_PAIR_LEAVES: usize = 8;

/// Gaps up to this many node pairs are paired by similarity; larger ones
/// by kind alone.
const MAX_PAIRING_CELLS: usize = 4096;

/// Deeper nesting is left to the token diff rather than risk the stack.
const MAX_DEPTH: u32 = 400;

/// Work done between checks of the clock.
const CLOCK_EVERY: u64 = 1 << 12;

struct Matcher<'a> {
    trees: [&'a Tree; 2],
    sources: [&'a [u8]; 2],
    lines: &'a LineMap<'a>,
    m: Matching,
    budget: Budget,
    work_left: u64,
    next_clock: u64,
    deadline: Instant,
    file_deadline: Instant,
    depth: u32,
}

/// Matches the leaves of `old` against `new`, parsed from `sources`, where
/// `lines` holds the lines the line diff left unchanged.
pub(super) fn run(
    old: &Tree,
    new: &Tree,
    sources: [&[u8]; 2],
    lines: &LineMap,
    budget: Budget,
) -> Matching {
    let file_deadline = Instant::now() + budget.file_time;
    let mut m = Matcher {
        trees: [old, new],
        sources,
        lines,
        m: Matching {
            old: vec![UNMATCHED; old.leaves.len()],
            new: vec![UNMATCHED; new.leaves.len()],
            words: [FxHashMap::default(), FxHashMap::default()],
            regions: Regions::default(),
            cut_short: false,
        },
        budget,
        work_left: u64::MAX,
        next_clock: u64::MAX - CLOCK_EVERY,
        deadline: file_deadline,
        file_deadline,
        depth: 0,
    };
    m.anchor(&old.top, &new.top);
    // Past the file deadline this stops early; the matches made stand.
    let _ = m.across_file();
    m.m
}

impl Matcher<'_> {
    fn spend(&mut self, work: usize) -> Result<(), Over> {
        let work = work as u64;
        if work >= self.work_left {
            self.m.cut_short = true;
            return Err(Over);
        }
        self.work_left -= work;
        if self.work_left <= self.next_clock {
            self.next_clock = self.work_left.saturating_sub(CLOCK_EVERY);
            if Instant::now() >= self.deadline {
                self.m.cut_short = true;
                return Err(Over);
            }
        }
        Ok(())
    }

    fn state(&self, side: usize, leaf: usize) -> LeafMatch {
        if side == 0 {
            self.m.old[leaf]
        } else {
            self.m.new[leaf]
        }
    }

    fn set(&mut self, side: usize, leaf: usize, state: LeafMatch) {
        if side == 0 {
            self.m.old[leaf] = state;
        } else {
            self.m.new[leaf] = state;
        }
    }

    fn link(&mut self, i: usize, j: usize, mark: Mark) {
        self.m.old[i] = LeafMatch {
            partner: index(j),
            mark,
        };
        self.m.new[j] = LeafMatch {
            partner: index(i),
            mark,
        };
    }

    /// Anchors the sequences `a` (old) and `b` (new) by hash, then works
    /// through the changed regions between the anchors. Returns the solid
    /// bytes of `a` matched, counting regions that fell back as matched.
    fn anchor(&mut self, a: &[u32], b: &[u32]) -> u32 {
        self.depth += 1;
        let mut matched = 0;
        let gaps = self.gaps_by_hash(a, b, &mut matched);
        matched += self.moved_siblings(&gaps).unwrap_or(0);
        let mut loose = (Vec::new(), Vec::new());
        for (ga, gb) in gaps {
            let (ga, gb) = (self.unmatched(0, ga), self.unmatched(1, gb));
            if ga.is_empty() || gb.is_empty() {
                if !(ga.is_empty() && gb.is_empty()) {
                    self.m.regions.one_sided += 1;
                }
                loose.0.extend(ga);
                loose.1.extend(gb);
                continue;
            }
            let (pairs, mut unpaired_old, mut unpaired_new) = self.pair_in_gap(&ga, &gb);
            let mut small = Vec::new();
            for (o, n) in pairs {
                let (to, tn) = (self.trees[0], self.trees[1]);
                let large = !to.is_leaf(o)
                    && !tn.is_leaf(n)
                    && to.node(o).solid + tn.node(n).solid > SPLIT_BYTES
                    && self.depth < MAX_DEPTH;
                if !large {
                    small.push((o, n));
                    continue;
                }
                let inner = self.anchor(to.children(o), tn.children(n));
                if self.similar(inner, o, n) {
                    matched += inner;
                } else {
                    self.undo(0, o);
                    self.undo(1, n);
                    unpaired_old.push(o);
                    unpaired_new.push(n);
                }
            }
            matched += self.region(&small, &unpaired_old, &unpaired_new);
        }
        matched += self.moved(&loose.0, &loose.1, Scope::Subtrees).unwrap_or(0);
        self.depth -= 1;
        matched
    }

    /// The nodes of `nodes` nothing has matched yet.
    fn unmatched(&self, side: usize, nodes: &[u32]) -> Vec<u32> {
        let tree = self.trees[side];
        nodes
            .iter()
            .copied()
            .filter(|&n| self.state(side, tree.leaves(n).start).free())
            .collect()
    }

    /// Matches nodes of the `gaps` between anchors that are equal to one
    /// node of another gap: siblings that moved.
    fn moved_siblings(&mut self, gaps: &[(&[u32], &[u32])]) -> Result<u32, Over> {
        let a: Vec<u32> = gaps.iter().flat_map(|g| g.0).copied().collect();
        let b: Vec<u32> = gaps.iter().flat_map(|g| g.1).copied().collect();
        self.moved(&a, &b, Scope::Siblings)
    }

    /// Matches one changed region, `pairs` of same-kind nodes plus the
    /// nodes left unpaired on each side, under a fresh budget. When it runs
    /// over, its matches are undone and its leaves marked
    /// [`Mark::Fallback`]. Returns the old side's solid bytes matched, or
    /// all of them after a fallback.
    fn region(&mut self, pairs: &[(u32, u32)], unpaired_old: &[u32], unpaired_new: &[u32]) -> u32 {
        if pairs.is_empty() && (unpaired_old.is_empty() || unpaired_new.is_empty()) {
            if !unpaired_old.is_empty() || !unpaired_new.is_empty() {
                self.m.regions.one_sided += 1;
            }
            return 0;
        }
        let now = Instant::now();
        self.work_left = self.budget.region_work;
        self.next_clock = self.work_left.saturating_sub(CLOCK_EVERY);
        self.deadline = (now + self.budget.region_time).min(self.file_deadline);
        let result = if now >= self.file_deadline {
            self.m.cut_short = true;
            Err(Over)
        } else {
            self.match_region(pairs, unpaired_old, unpaired_new)
        };
        self.work_left = u64::MAX;
        self.next_clock = u64::MAX - CLOCK_EVERY;
        self.deadline = self.file_deadline;
        if let Ok(m) = result {
            self.m.regions.matched += 1;
            return m;
        }
        self.m.regions.fell_back += 1;
        let nodes = pairs
            .iter()
            .flat_map(|&(o, n)| [(0, o), (1, n)])
            .chain(unpaired_old.iter().map(|&o| (0, o)))
            .chain(unpaired_new.iter().map(|&n| (1, n)));
        let mut solid = 0;
        for (side, node) in nodes {
            if side == 0 {
                solid += self.trees[0].node(node).solid;
            }
            for leaf in self.trees[side].leaves(node) {
                self.m.words[side].remove(&index(leaf));
                self.set(
                    side,
                    leaf,
                    LeafMatch {
                        partner: NONE,
                        mark: Mark::Fallback,
                    },
                );
            }
        }
        solid
    }

    fn match_region(
        &mut self,
        pairs: &[(u32, u32)],
        unpaired_old: &[u32],
        unpaired_new: &[u32],
    ) -> Result<u32, Over> {
        let mut matched = 0;
        // The unpaired nodes plus the pairs that didn't match.
        let (mut left_old, mut left_new) = (unpaired_old.to_vec(), unpaired_new.to_vec());
        for &(o, n) in pairs {
            if let Some(m) = self.pair(o, n)? {
                matched += m;
            } else {
                left_old.push(o);
                left_new.push(n);
            }
        }
        matched += self.moved(&left_old, &left_new, Scope::Subtrees)?;
        let a: Vec<u32> = pairs
            .iter()
            .map(|p| p.0)
            .chain(unpaired_old.iter().copied())
            .collect();
        let b: Vec<u32> = pairs
            .iter()
            .map(|p| p.1)
            .chain(unpaired_new.iter().copied())
            .collect();
        let errors =
            |side: usize, nodes: &[u32]| nodes.iter().any(|&n| self.trees[side].node(n).has_error);
        // The flat pass sees punctuation in order with every other token
        // left; matched among themselves first, recovery's stray `;`s would
        // pair across the code between them.
        if errors(0, &a) || errors(1, &b) {
            matched += self.residue(&a, &b)?;
        } else {
            matched += self.leftover(&left_old, &left_new)?;
        }
        Ok(matched)
    }

    /// Matches the tokens of `a` and `b` that nothing matched yet as flat
    /// sequences. Parse errors need it: the parser recovers from the same
    /// broken code differently depending on what's around it, even on
    /// whitespace, so equal code can come out as unequal trees.
    ///
    /// The sequences are of the words and punctuation in the tokens, not
    /// of the tokens: recovery can also cut the same code into tokens
    /// differently (a macro body is one token, but three when the parser
    /// takes the `#define` for part of a broken declaration). A token
    /// matches when all of it does, and so does every token its pieces
    /// matched into; its partner is the token its first piece matched
    /// into, so partners made here can be many to one.
    fn residue(&mut self, a: &[u32], b: &[u32]) -> Result<u32, Over> {
        // Novel tokens paired only to line up their lines count as free.
        // `reconcile` expects partners on kept lines to be mutual, which
        // those made here aren't.
        let free = |m: &Self, side: usize, nodes: &[u32]| {
            let tree = m.trees[side];
            let mut leaves: Vec<usize> = nodes
                .iter()
                .flat_map(|&n| tree.leaves(n))
                .filter(|&l| {
                    let leaf = tree.leaves[l];
                    let (start, end) = (leaf.start as usize, leaf.end as usize);
                    let kept = if side == 0 {
                        m.lines.kept(start, end).is_some()
                    } else {
                        m.lines.new_is_kept(start, end)
                    };
                    m.state(side, l).mark == Mark::Novel && !kept
                })
                .collect();
            leaves.sort_unstable();
            leaves
        };
        let (fa, fb) = (free(self, 0, a), free(self, 1, b));
        // By text alone: the same token can get another kind in another
        // recovery (`s` a type name on one side, an identifier on the
        // other). Comments go whole by their hash instead, which ignores
        // reflowing and re-indented continuation lines. Operators go whole
        // too: split into characters, `i++ + j` would equal `i + ++j`.
        let (trees, sources) = (self.trees, self.sources);
        let pieces = |side: usize, leaves: &[usize]| {
            let (mut owner, mut keys) = (Vec::new(), Vec::new());
            for (k, &l) in leaves.iter().enumerate() {
                let leaf = trees[side].leaves[l];
                let text = &sources[side][leaf.start as usize..leaf.end as usize];
                if leaf.comment {
                    owner.push(k);
                    keys.push((leaf.hash, &b""[..]));
                    continue;
                }
                if text.iter().all(u8::is_ascii_punctuation) {
                    owner.push(k);
                    keys.push((0, text));
                    continue;
                }
                for r in tokenize(text) {
                    owner.push(k);
                    keys.push((0, &text[r]));
                }
            }
            (owner, keys)
        };
        let ((oa, ka), (ob, kb)) = (pieces(0, &fa), pieces(1, &fb));
        self.spend(ka.len() + kb.len())?;
        let (ia, ib, tokens) = intern(ka.into_iter(), kb.into_iter());
        let pairs = common(&ia, &ib, tokens);
        let [wa, wb] = whole_tokens(&pairs, [&oa, &ob], [fa.len(), fb.len()]);
        let leaves = [&fa, &fb];
        // All old partners go first: one may be a token linked here.
        for (side, whole) in [(0, &wa), (1, &wb)] {
            for &(k, _) in whole {
                let p = self.state(side, leaves[side][k]).partner;
                if p != NONE {
                    self.set(1 - side, p as usize, UNMATCHED);
                }
            }
        }
        for (side, whole) in [(0, &wa), (1, &wb)] {
            for &(k, p) in whole {
                let state = LeafMatch {
                    partner: index(leaves[1 - side][p]),
                    mark: Mark::Same,
                };
                self.set(side, leaves[side][k], state);
            }
        }
        Ok(wa
            .iter()
            .map(|&(k, _)| solid(self.text(0, self.trees[0].leaves[fa[k]])))
            .sum())
    }

    /// Matches inside two nodes of the same kind. `None` when they share
    /// too little to count as one changed node.
    fn pair(&mut self, o: u32, n: u32) -> Result<Option<u32>, Over> {
        self.spend(1)?;
        let (to, tn) = (self.trees[0], self.trees[1]);
        let (no, nn) = (to.node(o), tn.node(n));
        if no.hash == nn.hash {
            return Ok(Some(self.same(o, n)));
        }
        let matched = match (no.atom, nn.atom, to.is_leaf(o), tn.is_leaf(n)) {
            (true, true, ..) => self.tokens(o, n)?,
            (false, false, true, true) => {
                self.leaf_pair(o, n);
                return Ok(Some(0));
            }
            (false, false, false, false) => {
                if self.depth >= MAX_DEPTH {
                    return Err(Over);
                }
                self.depth += 1;
                let m = self.sequence(to.children(o), tn.children(n));
                self.depth -= 1;
                m?
            }
            _ => return Ok(None),
        };
        if self.similar(matched, o, n) {
            Ok(Some(matched))
        } else {
            self.undo(0, o);
            self.undo(1, n);
            Ok(None)
        }
    }

    /// Diffs the children `a` and `b` of a node pair.
    fn sequence(&mut self, a: &[u32], b: &[u32]) -> Result<u32, Over> {
        self.spend(a.len() + b.len())?;
        let mut matched = 0;
        let (mut la, mut lb) = (Vec::new(), Vec::new());
        let gaps = self.gaps_by_hash(a, b, &mut matched);
        matched += self.moved_siblings(&gaps)?;
        for (ga, gb) in gaps {
            let (ga, gb) = (self.unmatched(0, ga), self.unmatched(1, gb));
            if ga.is_empty() || gb.is_empty() {
                la.extend(ga);
                lb.extend(gb);
                continue;
            }
            self.spend((ga.len() * gb.len()).min(MAX_PAIRING_CELLS) + ga.len() + gb.len())?;
            let (pairs, ua, ub) = self.pair_in_gap(&ga, &gb);
            for (o, n) in pairs {
                if let Some(m) = self.pair(o, n)? {
                    matched += m;
                } else {
                    la.push(o);
                    lb.push(n);
                }
            }
            la.extend(ua);
            lb.extend(ub);
        }
        matched += self.moved(&la, &lb, Scope::Subtrees)?;
        Ok(matched + self.leftover(&la, &lb)?)
    }

    /// Matches `a` against `b` in order, adding the solid bytes matched to
    /// `matched`, and returns the unmatched runs between matches.
    ///
    /// Nodes on lines the line diff kept are matched to the node they were
    /// kept as first, and only the rest by hash. Otherwise, with repeated
    /// code, a removed line could match a copy the line diff kept, and the
    /// line shown as removed would look unchanged.
    fn gaps_by_hash<'s>(
        &mut self,
        a: &'s [u32],
        b: &'s [u32],
        matched: &mut u32,
    ) -> Vec<(&'s [u32], &'s [u32])> {
        let (to, tn) = (self.trees[0], self.trees[1]);
        let start = |tree: &Tree, node: u32| tree.leaves[tree.leaves(node).start].start as usize;
        let mut kept = Vec::new();
        let mut from = 0;
        for (i, &o) in a.iter().enumerate() {
            let Some(at) = self.lines.kept(start(to, o), to.node_end(o)) else {
                continue;
            };
            let j = from + b[from..].partition_point(|&n| start(tn, n) < at);
            if b.get(j)
                .is_some_and(|&n| start(tn, n) == at && tn.node(n).hash == to.node(o).hash)
            {
                kept.push((i, j));
                from = j + 1;
            }
        }
        kept.push((a.len(), b.len()));
        let mut gaps = Vec::new();
        let (mut i, mut j) = (0, 0);
        for (ki, kj) in kept {
            let (sa, sb) = (&a[i..ki], &b[j..kj]);
            // As an anchor, a node on kept lines matched to one on changed
            // lines would also put their real counterparts in different
            // gaps. Uninformative nodes get keys of their own, so they are
            // never anchors.
            let key = |side: usize, k: usize, node: u32| {
                let tree = self.trees[side];
                let own = if tree.node(node).informative {
                    0
                } else {
                    k * 2 + side + 1
                };
                (tree.node(node).hash, self.on_kept_lines(side, node), own)
            };
            let (ia, ib, tokens) = intern(
                sa.iter().enumerate().map(|(k, &o)| key(0, k, o)),
                sb.iter().enumerate().map(|(k, &n)| key(1, k, n)),
            );
            let (mut gi, mut gj) = (0, 0);
            for (pi, pj) in common(&ia, &ib, tokens) {
                gaps.push((&sa[gi..pi], &sb[gj..pj]));
                *matched += self.same(sa[pi], sb[pj]);
                (gi, gj) = (pi + 1, pj + 1);
            }
            gaps.push((&sa[gi..], &sb[gj..]));
            if ki < a.len() {
                *matched += self.same(a[ki], b[kj]);
            }
            (i, j) = (ki + 1, kj + 1);
        }
        // The same punctuation between two anchors (the `,` between equal
        // array elements) is matched in place rather than made a region.
        gaps.retain(|&(ga, gb)| {
            let same = ga.len() == gb.len()
                && ga.iter().zip(gb).all(|(&o, &n)| {
                    let (no, nn) = (to.node(o), tn.node(n));
                    !no.informative
                        && no.hash == nn.hash
                        && self.on_kept_lines(0, o) == self.on_kept_lines(1, n)
                });
            if same {
                for (&o, &n) in ga.iter().zip(gb) {
                    *matched += self.same(o, n);
                }
            }
            !same && (!ga.is_empty() || !gb.is_empty())
        });
        gaps
    }

    /// Whether all of `node` is on lines the line diff kept. Matching by
    /// hash keeps such nodes to each other: `reconcile` undoes a match
    /// between kept and changed lines, and the changed side's real
    /// counterpart, passed over for it, stays unmatched.
    fn on_kept_lines(&self, side: usize, node: u32) -> bool {
        let tree = self.trees[side];
        let start = tree.leaves[tree.leaves(node).start].start as usize;
        let end = tree.node_end(node);
        if side == 0 {
            self.lines.kept(start, end).is_some()
        } else {
            self.lines.new_is_kept(start, end)
        }
    }

    /// Pairs informative nodes of `a` and `b` of the same kind, in order,
    /// preferring pairs that share more children. Returns the pairs and the
    /// nodes left unpaired on each side.
    fn pair_in_gap(&self, a: &[u32], b: &[u32]) -> (Vec<(u32, u32)>, Vec<u32>, Vec<u32>) {
        let (to, tn) = (self.trees[0], self.trees[1]);
        let (a, mut rest_a): (Vec<u32>, Vec<u32>) =
            a.iter().partition(|&&o| to.node(o).informative);
        let (b, mut rest_b): (Vec<u32>, Vec<u32>) =
            b.iter().partition(|&&n| tn.node(n).informative);
        let pairs = if a.len() * b.len() <= MAX_PAIRING_CELLS {
            let children = |tree: &Tree, node: u32| {
                let mut hashes: Vec<u64> = tree
                    .children(node)
                    .iter()
                    .map(|&c| tree.node(c).hash)
                    .collect();
                hashes.sort_unstable();
                hashes
            };
            let ca: Vec<Vec<u64>> = a.iter().map(|&o| children(to, o)).collect();
            let cb: Vec<Vec<u64>> = b.iter().map(|&n| children(tn, n)).collect();
            let ka: Vec<bool> = a.iter().map(|&o| self.on_kept_lines(0, o)).collect();
            let kb: Vec<bool> = b.iter().map(|&n| self.on_kept_lines(1, n)).collect();
            let score = |i: usize, j: usize| -> u32 {
                if to.node(a[i]).kind != tn.node(b[j]).kind || ka[i] != kb[j] {
                    return 0;
                }
                let total = ca[i].len() + cb[j].len();
                if total == 0 {
                    return 1;
                }
                1 + index(200 * shared(&ca[i], &cb[j]) / total)
            };
            weighted_common(a.len(), b.len(), score)
        } else {
            let (ia, ib, tokens) = intern(
                a.iter()
                    .map(|&o| (to.node(o).kind, self.on_kept_lines(0, o))),
                b.iter()
                    .map(|&n| (tn.node(n).kind, self.on_kept_lines(1, n))),
            );
            common(&ia, &ib, tokens)
        };
        let (mut used_a, mut used_b) = (vec![false; a.len()], vec![false; b.len()]);
        for &(i, j) in &pairs {
            used_a[i] = true;
            used_b[j] = true;
        }
        let rest = |nodes: &[u32], used: &[bool], rest: &mut Vec<u32>| {
            rest.extend(
                nodes
                    .iter()
                    .zip(used)
                    .filter(|&(_, &u)| !u)
                    .map(|(&n, _)| n),
            );
        };
        rest(&a, &used_a, &mut rest_a);
        rest(&b, &used_b, &mut rest_b);
        (
            pairs.into_iter().map(|(i, j)| (a[i], b[j])).collect(),
            rest_a,
            rest_b,
        )
    }

    /// Matches the uninformative nodes of `a` and `b` that nothing matched
    /// yet by hash, in order: they're never paired by kind or looked up as
    /// moved code.
    fn leftover(&mut self, a: &[u32], b: &[u32]) -> Result<u32, Over> {
        let free = |side: usize, nodes: &[u32]| {
            let tree = self.trees[side];
            let mut free: Vec<u32> = nodes
                .iter()
                .copied()
                .filter(|&n| {
                    !tree.node(n).informative && tree.leaves(n).all(|l| self.state(side, l).free())
                })
                .collect();
            free.sort_unstable_by_key(|&n| tree.leaves(n).start);
            free
        };
        let (fa, fb) = (free(0, a), free(1, b));
        if fa.is_empty() || fb.is_empty() {
            return Ok(0);
        }
        self.spend(fa.len() + fb.len())?;
        let (to, tn) = (self.trees[0], self.trees[1]);
        let (ia, ib, tokens) = intern(
            fa.iter()
                .map(|&o| (to.node(o).hash, self.on_kept_lines(0, o))),
            fb.iter()
                .map(|&n| (tn.node(n).hash, self.on_kept_lines(1, n))),
        );
        let mut matched = 0;
        for (i, j) in common(&ia, &ib, tokens) {
            matched += self.same(fa[i], fb[j]);
        }
        Ok(matched)
    }

    /// Whether a pair whose insides matched `matched` solid bytes (of the
    /// old side) shares enough to stay a pair: a fifth of both sides' solid
    /// bytes, the threshold intra-line emphasis pairs lines at. Small pairs
    /// always stay, as long as something matched: a renamed variable in
    /// `count + 1` is most of it, and a wrong pairing of a few tokens costs
    /// little.
    fn similar(&self, matched: u32, o: u32, n: u32) -> bool {
        let (to, tn) = (self.trees[0], self.trees[1]);
        if matched > 0 && to.leaves(o).len() + tn.leaves(n).len() <= SMALL_PAIR_LEAVES {
            return true;
        }
        let total = u64::from(to.node(o).solid) + u64::from(tn.node(n).solid);
        u64::from(matched) * 5 >= total
    }

    /// Pairs the leaves of two equal subtrees. Returns `o`'s solid bytes.
    fn same(&mut self, o: u32, n: u32) -> u32 {
        let (lo, ln) = (self.trees[0].leaves(o), self.trees[1].leaves(n));
        for (i, j) in lo.zip(ln) {
            self.link(i, j, Mark::Same);
        }
        self.trees[0].node(o).solid
    }

    /// Two tokens of the same kind with different text. Text with spaces
    /// (comments, strings) gets its changed words; anything else is changed
    /// whole.
    fn leaf_pair(&mut self, o: u32, n: u32) {
        let (i, j) = (self.trees[0].leaves(o).start, self.trees[1].leaves(n).start);
        let (lo, ln) = (self.trees[0].leaves[i], self.trees[1].leaves[j]);
        if !(lo.spaced && ln.spaced) {
            self.link(i, j, Mark::Novel);
            return;
        }
        let (to, tn) = (self.text(0, lo), self.text(1, ln));
        let (ro, rn) = (tokenize(to), tokenize(tn));
        let (ia, ib, tokens) = intern(
            ro.iter().map(|r| &to[r.clone()]),
            rn.iter().map(|r| &tn[r.clone()]),
        );
        let pairs = common(&ia, &ib, tokens);
        let (mut keep_o, mut keep_n) = (vec![false; ro.len()], vec![false; rn.len()]);
        for &(a, b) in &pairs {
            keep_o[a] = true;
            keep_n[b] = true;
        }
        let words_o = changed_words(&ro, &keep_o, lo.start);
        let words_n = changed_words(&rn, &keep_n, ln.start);
        self.m.words[0].insert(index(i), words_o);
        self.m.words[1].insert(index(j), words_n);
        self.link(i, j, Mark::Words);
    }

    fn text(&self, side: usize, leaf: Leaf) -> &[u8] {
        &self.sources[side][leaf.start as usize..leaf.end as usize]
    }

    /// How many solid bytes two parse-error atoms share, their tokens
    /// diffed as flat sequences. The tokens are left unmatched for
    /// [`Matcher::residue`], which sees them in order with the rest of the
    /// region: an atom can hold code whose counterpart is outside the other
    /// atom (the old side's first `if (x)`, the new side's `if (x)` that
    /// parsed fine), and matched between the atoms alone it would take the
    /// partner of a later copy.
    fn tokens(&mut self, o: u32, n: u32) -> Result<u32, Over> {
        let (to, tn) = (self.trees[0], self.trees[1]);
        let (lo, ln) = (to.leaves(o), tn.leaves(n));
        self.spend(lo.len() + ln.len())?;
        let (ia, ib, tokens) = intern(
            to.leaves[lo.clone()].iter().map(|l| l.hash),
            tn.leaves[ln].iter().map(|l| l.hash),
        );
        Ok(common(&ia, &ib, tokens)
            .into_iter()
            .map(|(i, _)| solid(self.text(0, to.leaves[lo.start + i])))
            .sum())
    }

    /// Forgets every match inside `node`, except a fallback's marks.
    fn undo(&mut self, side: usize, node: u32) {
        for leaf in self.trees[side].leaves(node) {
            match self.state(side, leaf).mark {
                Mark::Fallback => {}
                Mark::Words => {
                    self.m.words[side].remove(&index(leaf));
                    self.set(side, leaf, UNMATCHED);
                }
                Mark::Novel | Mark::Same => self.set(side, leaf, UNMATCHED),
            }
        }
    }

    /// Matches subtrees of the unmatched nodes `a` and `b` (the nodes
    /// themselves, or their subtrees too, as `scope` says) that are equal
    /// and occur as many times on each side, in order, largest first: moved
    /// code, and code that gained or lost a wrapper.
    fn moved(&mut self, a: &[u32], b: &[u32], scope: Scope) -> Result<u32, Over> {
        if a.is_empty() || b.is_empty() {
            return Ok(0);
        }
        let (to, tn) = (self.trees[0], self.trees[1]);
        // (hash, on kept lines, side, document position, node, whether it
        // was given)
        let mut found: Vec<(u64, bool, usize, u32, u32, bool)> = Vec::new();
        // Tokens inside atoms, which have no nodes to be counted by.
        let mut hidden = FxHashSet::default();
        let mut stack = Vec::new();
        for (side, roots) in [(0, a), (1, b)] {
            let tree = self.trees[side];
            stack.extend(roots.iter().rev().map(|&n| (n, true)));
            while let Some((node, root)) = stack.pop() {
                self.spend(1)?;
                let leaves = tree.leaves(node);
                let candidate = tree.node(node).informative
                    && (scope != Scope::File
                        || (leaves.len() >= MIN_FILE_MOVE_LEAVES && self.whole_lines(side, node)));
                if candidate {
                    let kept = self.on_kept_lines(side, node);
                    found.push((
                        tree.node(node).hash,
                        kept,
                        side,
                        index(leaves.start),
                        node,
                        root,
                    ));
                }
                if tree.node(node).atom {
                    hidden.extend(tree.leaves[leaves].iter().map(|l| l.hash));
                } else if scope != Scope::Siblings {
                    stack.extend(tree.children(node).iter().rev().map(|&c| (c, false)));
                }
            }
        }
        found.sort_unstable();
        let mut pairs = Vec::new();
        for group in found.chunk_by(|x, y| (x.0, x.1) == (y.0, y.1)) {
            let split = group.partition_point(|x| x.2 == 0);
            let (olds, news) = group.split_at(split);
            // Code that may have a copy inside an atom can occur once on each
            // side outside atoms without those two being each other's copies
            // (a comment in a parse error on one side and parsed fine on the
            // other, and its twin the other way around). Within a region
            // it's left to the flat pass, which goes in order. Whether a copy
            // is there is judged by its tokens alone, so this also passes
            // over some code that only moved.
            let (tree, node) = (self.trees[group[0].2], group[0].4);
            if scope != Scope::File
                && tree.leaves[tree.leaves(node)]
                    .iter()
                    .all(|l| hidden.contains(&l.hash))
            {
                continue;
            }
            if olds.len() == news.len() {
                pairs.extend(
                    olds.iter()
                        .zip(news)
                        // Within a region, a match needs a whole node on
                        // one side (wrapped or unwrapped): pieces of two
                        // rewritten statements, `(`, `a`, `;`, would
                        // otherwise match one by one.
                        .filter(|(o, n)| scope != Scope::Subtrees || o.5 || n.5)
                        .map(|(o, n)| (o.4, n.4)),
                );
            }
        }
        pairs.sort_unstable_by_key(|&(o, _)| std::cmp::Reverse((to.node(o).solid, o)));
        let mut matched = 0;
        for (o, n) in pairs {
            let (mut lo, mut ln) = (to.leaves(o), tn.leaves(n));
            self.spend(lo.len() + ln.len())?;
            if lo.len() == ln.len()
                && lo.all(|l| self.m.old[l].free())
                && ln.all(|l| self.m.new[l].free())
            {
                matched += self.same(o, n);
            }
        }
        Ok(matched)
    }

    /// Moves across the file: subtrees still unmatched anywhere (outside
    /// regions that fell back), looked up by hash.
    fn across_file(&mut self) -> Result<u32, Over> {
        let mut roots: [Vec<u32>; 2] = [Vec::new(), Vec::new()];
        for (side, roots) in roots.iter_mut().enumerate() {
            let tree = self.trees[side];
            let states = if side == 0 { &self.m.old } else { &self.m.new };
            // Free leaves before each leaf, so a subtree is checked in O(1).
            let mut before = Vec::with_capacity(states.len() + 1);
            before.push(0);
            let mut count = 0;
            for s in states {
                count += usize::from(s.free());
                before.push(count);
            }
            let mut stack: Vec<u32> = tree.top.iter().rev().copied().collect();
            while let Some(node) = stack.pop() {
                let leaves = tree.leaves(node);
                let free = before[leaves.end] - before[leaves.start];
                if free == leaves.len() {
                    roots.push(node);
                } else if free > 0 && !tree.node(node).atom {
                    stack.extend(tree.children(node).iter().rev());
                }
            }
        }
        self.moved(&roots[0], &roots[1], Scope::File)
    }

    /// Whether `node` has its lines to itself, besides whitespace.
    fn whole_lines(&self, side: usize, node: u32) -> bool {
        let tree = self.trees[side];
        let source = self.sources[side];
        let start = tree.leaves[tree.leaves(node).start].start as usize;
        let end = tree.node_end(node);
        let before = source[..start].iter().rev().take_while(|&&b| b != b'\n');
        let after = source[end..].iter().take_while(|&&b| b != b'\n');
        before.chain(after).all(u8::is_ascii_whitespace)
    }
}

/// Where [`Matcher::moved`] looks for moved code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Only the nodes given: siblings that moved.
    Siblings,
    /// The nodes and their subtrees: also code that gained or lost a
    /// wrapper.
    Subtrees,
    /// Subtrees anywhere in the file, but only whole lines of at least
    /// [`MIN_FILE_MOVE_LEAVES`] tokens: smaller pieces (`return 0;`, a
    /// parameter) recur everywhere and would pair unrelated code.
    File,
}

/// How many of the sorted `a` and `b` are shared, counting repeats.
fn shared(a: &[u64], b: &[u64]) -> usize {
    let (mut i, mut j, mut count) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                count += 1;
                i += 1;
                j += 1;
            }
        }
    }
    count
}

/// Index pairs in order maximizing the total `score` of the pairs taken;
/// pairs scoring 0 are never taken. Quadratic, for small gaps.
fn weighted_common(n: usize, m: usize, score: impl Fn(usize, usize) -> u32) -> Vec<(usize, usize)> {
    let width = m + 1;
    let mut best = vec![0u32; (n + 1) * width];
    for i in 1..=n {
        for j in 1..=m {
            let s = score(i - 1, j - 1);
            let take = if s > 0 {
                best[(i - 1) * width + j - 1] + s
            } else {
                0
            };
            best[i * width + j] = take
                .max(best[(i - 1) * width + j])
                .max(best[i * width + j - 1]);
        }
    }
    let mut pairs = Vec::new();
    let (mut i, mut j) = (n, m);
    while i > 0 && j > 0 {
        let here = best[i * width + j];
        if here == best[(i - 1) * width + j] {
            i -= 1;
        } else if here == best[i * width + j - 1] {
            j -= 1;
        } else {
            pairs.push((i - 1, j - 1));
            i -= 1;
            j -= 1;
        }
    }
    pairs.reverse();
    pairs
}

fn solid(text: &[u8]) -> u32 {
    index(text.iter().filter(|b| !b.is_ascii_whitespace()).count())
}

/// The ranges of `tokens` not kept, merged where they touch, offset by
/// `base`.
fn changed_words(tokens: &[Range<usize>], keep: &[bool], base: u32) -> Vec<Range<u32>> {
    let mut out: Vec<Range<u32>> = Vec::new();
    for (r, _) in tokens.iter().zip(keep).filter(|&(_, &k)| !k) {
        let r = base + index(r.start)..base + index(r.end);
        match out.last_mut() {
            Some(last) if last.end == r.start => last.end = r.end,
            _ => out.push(r),
        }
    }
    out
}

/// The tokens that matched whole in a diff of their pieces: `pairs` of
/// matched pieces, in order, and per side the token each piece belongs to
/// and how many tokens there are. A token matches whole when all its
/// pieces matched and so did every token they matched into. Returns per
/// side each such token with the token its first piece matched into.
fn whole_tokens(
    pairs: &[(usize, usize)],
    owners: [&[usize]; 2],
    tokens: [usize; 2],
) -> [Vec<(usize, usize)>; 2] {
    let mut left = tokens.map(|n| vec![0usize; n]);
    let mut partner = tokens.map(|n| vec![usize::MAX; n]);
    for side in 0..2 {
        for &k in owners[side] {
            left[side][k] += 1;
        }
    }
    for &(i, j) in pairs {
        let (a, b) = (owners[0][i], owners[1][j]);
        left[0][a] -= 1;
        left[1][b] -= 1;
        if partner[0][a] == usize::MAX {
            partner[0][a] = b;
        }
        if partner[1][b] == usize::MAX {
            partner[1][b] = a;
        }
    }
    // The pairs are in order on both sides, so the tokens tied together by
    // them form runs of consecutive pairs sharing a token.
    let mut whole = [Vec::new(), Vec::new()];
    let mut start = 0;
    for end in 1..=pairs.len() {
        if end < pairs.len() {
            let ((i0, j0), (i1, j1)) = (pairs[end - 1], pairs[end]);
            if owners[0][i0] == owners[0][i1] || owners[1][j0] == owners[1][j1] {
                continue;
            }
        }
        let run = &pairs[start..end];
        if run
            .iter()
            .all(|&(i, j)| left[0][owners[0][i]] == 0 && left[1][owners[1][j]] == 0)
        {
            for &(i, j) in run {
                for (side, k) in [(0, owners[0][i]), (1, owners[1][j])] {
                    if whole[side].last().is_none_or(|&(last, _)| last != k) {
                        whole[side].push((k, partner[side][k]));
                    }
                }
            }
        }
        start = end;
    }
    whole
}

/// Dense ids for two sequences of keys, and how many there are.
fn intern<K: std::hash::Hash + Eq>(
    a: impl Iterator<Item = K>,
    b: impl Iterator<Item = K>,
) -> (Vec<u32>, Vec<u32>, usize) {
    let mut ids: FxHashMap<K, u32> = FxHashMap::default();
    let mut id = |k: K| {
        let next = index(ids.len());
        *ids.entry(k).or_insert(next)
    };
    let ia = a.map(&mut id).collect();
    let ib = b.map(&mut id).collect();
    (ia, ib, ids.len())
}

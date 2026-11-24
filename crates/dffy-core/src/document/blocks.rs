//! Blocks: rows a host inserts among a file's rows. A block has an id, the
//! place it hangs (under the file's first row, under the row showing a
//! line, or under the file's last row), a height the host measures and
//! plain text to search. Its rows are rows like any other: a [`super::Pos`]
//! addresses each, moving, scrolling and search go through them, and
//! [`super::Content::Block`] says which block and which of its rows. What a
//! block shows, and how, is the host's. Blocks hung at one place come one
//! under the other, parted by the blank rows the host asks for
//! ([`super::Content::Parting`]).
//!
//! A block on a line a gap hides hangs under the gap's separator, and moves
//! under the line once it's revealed. A block on a line no row shows (in a
//! binary file, or past the end of the side) has no rows.
//!
//! Heights are asked of the host, through [`Arrange::measure`], only for the
//! parts of a file a call touches, and kept per width until the host says
//! a block looks different ([`super::Document::remeasure`]).

use std::cell::{Ref, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::Side;
use crate::view::{Arrangement, HunkHeader};

/// A block's id, the host's. Unique among the blocks of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// Where a block hangs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// Under the entry's first row, before the blocks on the lines it
    /// shows.
    Start,
    /// Under the row showing line `line` (0-based) of `side`, as the line
    /// diff numbers it; a rendered view puts it under the last rendered
    /// line drawn from it.
    Line {
        /// The side `line` is on.
        side: Side,
        /// The line.
        line: usize,
    },
    /// Under the entry's last row.
    End,
}

/// A block of rows inserted among a file's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// The host's.
    pub id: BlockId,
    /// Where it hangs.
    pub at: Attach,
    /// What search looks in, by line.
    pub text: Arc<str>,
}

/// How a host lays out a file's rows: each hunk's arrangement, and the
/// rows each block takes. Calls take it by value, so it's a handle: a
/// reference to the host's state, or any `Fn(&HunkHeader) -> Arrangement`
/// that's `Copy`, whose blocks take a row each.
pub trait Arrange: Copy {
    /// The arrangement of a hunk with these numbers (and no heading).
    fn arrangement(&self, header: &HunkHeader<'_>) -> Arrangement;

    /// The width the host measures blocks at. Heights are kept per width:
    /// a new width measures them again, so every call on a file with blocks
    /// passes the same layout until it changes.
    fn width(&self) -> usize {
        0
    }

    /// Rows block `id` takes at `width`; 0 counts as 1. Asked once per
    /// block and width until [`super::Document::remeasure`], and only for
    /// the parts a call touches.
    fn measure(&self, id: BlockId, width: usize) -> usize {
        let _ = (id, width);
        1
    }

    /// The row of block `id`, at `width`, showing line `line` of its text,
    /// for a match of a search there; past its last row counts as its last.
    fn locate(&self, id: BlockId, width: usize, line: usize) -> usize {
        let _ = (id, width, line);
        0
    }

    /// Blank rows between two blocks hung at one place: rows of neither.
    fn parting(&self) -> usize {
        0
    }
}

impl<F: Fn(&HunkHeader<'_>) -> Arrangement + Copy> Arrange for F {
    #[inline]
    fn arrangement(&self, header: &HunkHeader<'_>) -> Arrangement {
        self(header)
    }
}

/// A placement of the blocks made for one generation is never taken for
/// another's, even across entries and documents.
static GENERATIONS: AtomicU64 = AtomicU64::new(1);

/// An entry's blocks, and their heights at the width last measured.
pub(super) struct Blocks {
    pub list: Arc<[Block]>,
    /// New with every change to the list or a height.
    pub generation: u64,
    /// New with every change to the list.
    pub version: u64,
    heights: RefCell<Heights>,
    /// For an entry without a diff to place them in, the blocks at its
    /// edges, placed.
    edges: RefCell<Option<Edges>>,
}

/// The blocks at an entry's edges placed at a generation, width and count
/// of own rows (see [`Blocks::edges`]).
struct Edges {
    key: (u64, usize, usize),
    placed: Placed,
}

#[derive(Default)]
struct Heights {
    width: usize,
    rows: HashMap<BlockId, usize>,
}

impl Blocks {
    pub(super) fn new(list: Vec<Block>) -> Self {
        let generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
        Self {
            list: list.into(),
            generation,
            version: generation,
            heights: RefCell::default(),
            edges: RefCell::default(),
        }
    }

    /// Takes `list` in place of the blocks, keeping no heights: an id may
    /// stand for another block now. The same list keeps its version.
    pub(super) fn replace(&mut self, list: Vec<Block>) {
        if *self.list == list[..] {
            self.remeasure(None);
        } else {
            *self = Self::new(list);
        }
    }

    /// Forgets block `id`'s height, or with `None` every block's.
    pub(super) fn remeasure(&mut self, id: Option<BlockId>) {
        let heights = self.heights.get_mut();
        match id {
            Some(id) => {
                heights.rows.remove(&id);
            }
            None => heights.rows.clear(),
        }
        self.generation = GENERATIONS.fetch_add(1, Ordering::Relaxed);
    }

    /// Rows block `id` takes, as `arrange` measures it.
    pub(super) fn height<A: Arrange>(&self, id: BlockId, arrange: &A) -> usize {
        let width = arrange.width();
        let mut heights = self.heights.borrow_mut();
        if heights.width != width {
            heights.rows.clear();
            heights.width = width;
        }
        *(heights.rows)
            .entry(id)
            .or_insert_with(|| arrange.measure(id, width).max(1))
    }

    pub(super) fn get(&self, id: BlockId) -> Option<&Block> {
        self.list.iter().find(|b| b.id == id)
    }

    /// The blocks under the first and the last of `own` rows, the only
    /// ones an entry without a diff shows, placed once per generation,
    /// width and `own`.
    pub(super) fn edges<A: Arrange>(&self, own: usize, arrange: &A) -> Option<Ref<'_, Placed>> {
        let key = (self.generation, arrange.width(), own);
        if self.edges.borrow().as_ref().is_none_or(|e| e.key != key) {
            let placed = Placed::edges(own, true, true, self, arrange);
            *self.edges.borrow_mut() = Some(Edges { key, placed });
        }
        Ref::filter_map(self.edges.borrow(), |e| e.as_ref().map(|e| &e.placed)).ok()
    }
}

/// A block placed in a part: under how many of the part's own rows, the
/// part's row its first row is, how many it takes, and how many blank rows
/// part it from a block above it at the same place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Insert {
    pub after: usize,
    pub top: usize,
    pub rows: usize,
    pub id: BlockId,
    pub parted: usize,
}

impl Insert {
    /// The part's rows from its first to this block's last, less the
    /// part's own.
    fn added(self) -> usize {
        self.top + self.rows - self.after
    }
}

/// What a part's row shows, once its blocks are placed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum At {
    /// The part's own row `n`: code, a separator, a header, a sheet's row.
    Own(usize),
    /// Row `line` of block `id`, which hangs under `after` of the part's
    /// own rows.
    Block {
        id: BlockId,
        line: usize,
        after: usize,
    },
    /// A blank row parting block `id` from the block above it, both under
    /// `after` of the part's own rows.
    Parting { id: BlockId, after: usize },
}

impl At {
    /// The part's own row this is, or the one a block's row hangs under
    /// (the first, above it, for a block above the part's first row).
    pub(super) fn own(self) -> usize {
        match self {
            Self::Own(row) => row,
            Self::Block { after, .. } | Self::Parting { after, .. } => after.saturating_sub(1),
        }
    }
}

/// The shape a part's blocks were placed for: when it changes, they're
/// placed again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Key {
    pub width: usize,
    pub structural: bool,
    pub layout: Option<super::Layout>,
    /// For a gap: lines shown from the top, from the bottom, and between.
    pub gap: Option<(usize, usize, Vec<Range<usize>>)>,
}

/// A part's blocks, placed among its own rows.
#[derive(Debug, Clone, Default)]
pub(super) struct Placed {
    pub key: Option<Key>,
    /// The part's own rows.
    pub own: usize,
    /// In row order.
    pub inserts: Vec<Insert>,
    /// A hunk's rows that start a block of changes, counting block rows.
    pub starts: Vec<usize>,
}

impl Placed {
    /// Places blocks under the part's own rows: `hung` has each block's
    /// place (the own rows above it), its order among blocks at that
    /// place, and its id.
    pub(super) fn new<A: Arrange>(
        own: usize,
        mut hung: Vec<(usize, (u8, u32), BlockId)>,
        blocks: &Blocks,
        arrange: &A,
    ) -> Self {
        hung.sort_unstable_by_key(|&(after, order, _)| (after, order));
        let parting = arrange.parting();
        let mut added = 0;
        let mut place = None;
        let inserts = (hung.into_iter())
            .map(|(after, _, id)| {
                let parted = if place == Some(after) { parting } else { 0 };
                place = Some(after);
                added += parted;
                let rows = blocks.height(id, arrange);
                let insert = Insert {
                    after,
                    top: after + added,
                    rows,
                    id,
                    parted,
                };
                added += rows;
                insert
            })
            .collect();
        Self {
            key: None,
            own,
            inserts,
            starts: Vec::new(),
        }
    }

    /// The blocks at an entry's start and end, for a part of `own` rows
    /// that's the first, the last, or both.
    pub(super) fn edges<A: Arrange>(
        own: usize,
        first: bool,
        last: bool,
        blocks: &Blocks,
        arrange: &A,
    ) -> Self {
        let hung = (blocks.list.iter().zip(0u32..))
            .filter_map(|(b, i)| match b.at {
                Attach::Start if first => Some((own.min(1), (0, i), b.id)),
                Attach::End if last => Some((own, (3, i), b.id)),
                _ => None,
            })
            .collect();
        Self::new(own, hung, blocks, arrange)
    }

    /// Rows in the part, its blocks' included.
    pub(super) fn rows(&self) -> usize {
        self.own + self.inserts.last().map_or(0, |i| i.added())
    }

    /// What the part's row `row` shows.
    pub(super) fn at(&self, row: usize) -> At {
        let k = self.inserts.partition_point(|i| i.top - i.parted <= row);
        let Some(&above) = k.checked_sub(1).map(|k| &self.inserts[k]) else {
            return At::Own(row);
        };
        if row < above.top {
            return At::Parting {
                id: above.id,
                after: above.after,
            };
        }
        if row < above.top + above.rows {
            return At::Block {
                id: above.id,
                line: row - above.top,
                after: above.after,
            };
        }
        At::Own(row - above.added())
    }

    /// The part's row showing its own row `own`.
    pub(super) fn row(&self, own: usize) -> usize {
        let k = self.inserts.partition_point(|i| i.after <= own);
        own + k.checked_sub(1).map_or(0, |k| self.inserts[k].added())
    }

    pub(super) fn block(&self, id: BlockId) -> Option<Insert> {
        self.inserts.iter().find(|i| i.id == id).copied()
    }
}

/// The order of blocks hung at one place: those on the whole entry, then
/// the old side's, the new side's, and those under the last row; each in
/// the host's order.
pub(super) fn order(at: Attach, index: u32) -> (u8, u32) {
    let rank = match at {
        Attach::Start => 0,
        Attach::Line {
            side: Side::Old, ..
        } => 1,
        Attach::Line {
            side: Side::New, ..
        } => 2,
        Attach::End => 3,
    };
    (rank, index)
}

#[cfg(test)]
mod tests;

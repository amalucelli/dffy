use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use proptest::prelude::*;

use super::*;
use crate::diff::{Options, Text, Whitespace, hunks};
use crate::document::{
    BlockStep, Content, Document, File, Hit, Needle, Origin, Pos, Settle, Source, find,
};
use crate::git::{FileChange, FileDiff, Mode, Side as GitSide, Status};
use crate::highlight::Registry;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

/// A host's layout: one arrangement for every hunk, fixed heights, and the
/// rows parting blocks at one place.
struct Measured {
    split: bool,
    heights: HashMap<BlockId, usize>,
    parting: usize,
    calls: Cell<usize>,
}

impl Arrange for &Measured {
    fn arrangement(&self, _: &HunkHeader<'_>) -> Arrangement {
        if self.split {
            Arrangement::Split
        } else {
            Arrangement::Unified
        }
    }

    fn width(&self) -> usize {
        80
    }

    fn measure(&self, id: BlockId, _: usize) -> usize {
        self.calls.set(self.calls.get() + 1);
        self.heights[&id]
    }

    fn locate(&self, _: BlockId, _: usize, line: usize) -> usize {
        line
    }

    fn parting(&self) -> usize {
        self.parting
    }
}

/// [`Measured`] at another width.
#[derive(Clone, Copy)]
struct Wider<'a>(&'a Measured);

impl Arrange for Wider<'_> {
    fn arrangement(&self, h: &HunkHeader<'_>) -> Arrangement {
        (&self.0).arrangement(h)
    }

    fn width(&self) -> usize {
        100
    }

    fn measure(&self, id: BlockId, width: usize) -> usize {
        (&self.0).measure(id, width)
    }

    fn parting(&self) -> usize {
        self.0.parting
    }
}

/// `n` lines, and the same with `edits` made: each a line and whether it's
/// replaced, has one inserted before it, or is deleted.
fn texts(n: usize, edits: &[(usize, u8)]) -> (String, String) {
    let old: Vec<String> = (0..n).map(|i| format!("line {i}\n")).collect();
    let mut new = old.clone();
    for &(at, kind) in edits {
        if new.is_empty() {
            new.push("new\n".to_owned());
            continue;
        }
        let at = at % new.len();
        match kind {
            0 => new[at] = format!("changed {at}\n"),
            1 => new.insert(at, format!("inserted {at}\n")),
            _ => {
                new.remove(at);
            }
        }
    }
    (old.concat(), new.concat())
}

fn diff(old: &str, new: &str) -> FileDiff {
    diff_at("notes.txt", old, new)
}

fn diff_at(path: &str, old: &str, new: &str) -> FileDiff {
    let side = Some(GitSide::Worktree {
        mode: Mode::File,
        id: None,
    });
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    FileDiff {
        change: FileChange {
            path: path.into(),
            status: Status::Modified,
            old: side,
            new: side,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    }
}

fn document(old: &str, new: &str) -> Document<()> {
    document_at("notes.txt", old, new)
}

fn document_at(path: &str, old: &str, new: &str) -> Document<()> {
    let mut doc = Document::default();
    doc.push(diff_at(path, old, new).change, Source::Repo);
    let file =
        File::prepare(&GRAMMARS, None).file(diff_at(path, old, new), Whitespace::Exact, true);
    let _ = doc.load(0, Ok(Box::new(file)));
    doc
}

/// What a row shows, without borrowing the document.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Shows {
    Own(String),
    Block(BlockId, usize),
    Parting(BlockId),
}

fn shows(doc: &mut Document<()>, pos: Pos, m: &Measured) -> Shows {
    match doc.row(pos, m) {
        Some(Content::Block { id, line }) => Shows::Block(id, line),
        Some(Content::Parting { id }) => Shows::Parting(id),
        Some(Content::Start(h)) => Shows::Own(format!("{}: start {}", pos.part, h.numbers())),
        Some(Content::Hunk { row, .. }) => Shows::Own(format!("{}: {row:?}", pos.part)),
        Some(Content::Separator { hidden, next }) => {
            Shows::Own(format!("{}: hides {hidden} {}", pos.part, next.is_some()))
        }
        Some(Content::Gap { old, new, .. }) => Shows::Own(format!("{}: {old} {new}", pos.part)),
        Some(Content::Sheet(row)) => Shows::Own(format!("sheet {row}")),
        None => Shows::Own("none".to_owned()),
    }
}

fn top(doc: &Document<()>) -> Pos {
    Pos {
        part: doc.first_part(0),
        ..Pos::default()
    }
}

/// Every row of the file from its top, by moving down.
fn walk(doc: &mut Document<()>, m: &Measured) -> Vec<(Pos, Shows)> {
    let mut pos = top(doc);
    let mut out = vec![(pos, shows(doc, pos, m))];
    while doc.next_row(&mut pos, m) {
        out.push((pos, shows(doc, pos, m)));
    }
    out
}

#[derive(Debug, Clone)]
enum Reveal {
    Expand(usize),
    Lines(bool, usize),
    Full,
}

fn reveal(doc: &mut Document<()>, op: &Reveal, m: &Measured) {
    match *op {
        Reveal::Expand(gap) => {
            doc.expand(0, gap, m);
        }
        Reveal::Lines(old, line) => {
            let side = if old { Side::Old } else { Side::New };
            doc.reveal_lines(0, side, line..line + 1, 1, m);
        }
        Reveal::Full => {
            doc.toggle_full(m);
        }
    }
}

/// A file, its blocks with their heights and the rows parting them, how
/// much of it is revealed, and its arrangement.
#[derive(Debug, Clone)]
struct Case {
    old: String,
    new: String,
    blocks: Vec<(Attach, usize)>,
    parting: usize,
    reveals: Vec<Reveal>,
    split: bool,
}

impl Case {
    fn measured(&self) -> Measured {
        let heights = (self.blocks.iter().zip(0u32..))
            .map(|(&(_, h), i)| (BlockId(i), h))
            .collect();
        Measured {
            split: self.split,
            heights,
            parting: self.parting,
            calls: Cell::new(0),
        }
    }

    fn blocks(&self) -> Vec<Block> {
        (self.blocks.iter().zip(0u32..))
            .map(|(&(at, _), i)| Block {
                id: BlockId(i),
                at,
                text: format!("block {i} one\nmiddle\nblock {i} two").into(),
            })
            .collect()
    }

    /// The document with its blocks, revealed as the case says.
    fn document(&self, m: &Measured, blocks: bool) -> Document<()> {
        let mut doc = document(&self.old, &self.new);
        if blocks {
            doc.set_blocks(0, self.blocks(), m);
        }
        for op in &self.reveals {
            reveal(&mut doc, op, m);
        }
        doc
    }
}

fn case() -> impl Strategy<Value = Case> {
    let attach = (0..6u8, any::<bool>(), 0..90usize).prop_map(|(kind, old, line)| match kind {
        0 => Attach::Start,
        1 => Attach::End,
        _ => Attach::Line {
            side: if old { Side::Old } else { Side::New },
            line,
        },
    });
    let reveal = prop_oneof![
        (0..5usize).prop_map(Reveal::Expand),
        (any::<bool>(), 0..90usize).prop_map(|(old, line)| Reveal::Lines(old, line)),
        Just(Reveal::Full),
    ];
    (
        1..80usize,
        prop::collection::vec((0..80usize, 0..3u8), 0..6),
        prop::collection::vec((attach, 1..5usize), 0..14),
        0..3usize,
        prop::collection::vec(reveal, 0..3),
        (any::<bool>(), prop::bool::weighted(0.1)),
    )
        .prop_map(|(n, edits, blocks, parting, reveals, (split, added))| {
            let (old, new) = texts(n, &edits);
            // A whole added file: its first row is a line.
            let old = if added { String::new() } else { old };
            Case {
                old,
                new,
                blocks,
                parting,
                reveals,
                split,
            }
        })
}

/// Whether the own row at `pos` shows line `line` of `side`, or is the
/// separator hiding it.
fn holds(doc: &mut Document<()>, pos: Pos, side: Side, line: usize, m: &Measured) -> bool {
    if doc.row_lines(pos, m)[side.index()] == Some(line) {
        return true;
    }
    let Some(Content::Separator { .. }) = doc.row(pos, m) else {
        return false;
    };
    let Some(crate::document::Segment::Gap(g)) = doc.segment(0, pos.part) else {
        return false;
    };
    let gap = doc.file(0).expect("loaded").gaps()[g];
    let start = match side {
        Side::Old => gap.old,
        Side::New => gap.new,
    };
    // A block's rows are counted in the part's, the separator's own row is
    // what `shown` knows: find it among the part's own rows.
    let own = (0..pos.row)
        .filter(|&r| {
            let at = Pos { row: r, ..pos };
            matches!(shows(doc, at, m), Shows::Own(_))
        })
        .count();
    let shown = doc.shown(0, g).expect("loaded");
    (line.checked_sub(start)).is_some_and(|l| shown.hides(own).is_some_and(|h| h.contains(&l)))
}

/// The rows of `doc`, with blocks, against those of `plain`, the same
/// without: see [`positions_round_trip_through_rows_with_blocks`].
fn rows_hold(
    doc: &mut Document<()>,
    m: &Measured,
    plain: &mut Document<()>,
    plain_m: &Measured,
) -> Result<(), TestCaseError> {
    let rows = walk(doc, m);
    prop_assert!(rows.windows(2).all(|w| w[0].0 < w[1].0));
    let parts: usize = (doc.first_part(0)..doc.parts(0))
        .map(|p| doc.rows(0, p, m))
        .sum();
    prop_assert_eq!(rows.len(), parts);
    let mut pos = rows.last().expect("a row").0;
    for (at, _) in rows.iter().rev().skip(1) {
        prop_assert!(doc.prev_row(&mut pos, m));
        prop_assert_eq!(pos, *at);
    }
    prop_assert!(!doc.prev_row(&mut pos, m));

    let own: Vec<Shows> = (rows.iter())
        .filter(|(_, s)| matches!(s, Shows::Own(_)))
        .map(|(_, s)| s.clone())
        .collect();
    let plain_rows: Vec<Shows> = (walk(plain, plain_m).into_iter()).map(|(_, s)| s).collect();
    prop_assert_eq!(own, plain_rows);

    // On the lines of the diff shown, a rendered view's.
    let blocks: HashMap<BlockId, Attach> = (doc.search_blocks(0).into_iter())
        .map(|b| (b.id, b.at))
        .collect();
    let mut seen = 0;
    let mut k = 0;
    while k < rows.len() {
        let Shows::Block(id, 0) = rows[k].1 else {
            k += 1;
            continue;
        };
        let height = m.heights[&id];
        for line in 0..height {
            prop_assert_eq!(&rows[k + line].1, &Shows::Block(id, line));
        }
        // Under another block at its place, parted from it; else right
        // under its own row.
        let parted = (rows[..k].iter().rev())
            .take_while(|(_, s)| *s == Shows::Parting(id))
            .count();
        let over = k.checked_sub(parted + 1).map(|j| &rows[j].1);
        if parted > 0 {
            prop_assert_eq!(parted, m.parting);
            prop_assert!(matches!(over, Some(Shows::Block(other, _)) if *other != id));
        } else if m.parting > 0 {
            prop_assert!(matches!(over, Some(Shows::Own(_))), "{id:?} under {over:?}");
        }
        let above = rows[..k]
            .iter()
            .rposition(|(_, s)| matches!(s, Shows::Own(_)));
        match blocks[&id] {
            Attach::Start => prop_assert_eq!(above, Some(0)),
            Attach::End => prop_assert!(
                (rows[k + height..].iter())
                    .all(|(_, s)| matches!(s, Shows::Block(..) | Shows::Parting(_)))
            ),
            Attach::Line { side, line } => {
                let above = rows[above.expect("a row above")].0;
                prop_assert!(holds(doc, above, side, line, m), "{id:?} under {above:?}");
            }
        }
        seen += 1;
        k += height;
    }
    for w in rows.windows(2) {
        if let Shows::Parting(id) = w[0].1 {
            prop_assert!(matches!(w[1].1, Shows::Block(i, 0) | Shows::Parting(i) if i == id));
        }
    }
    let shown = doc.file(0).expect("loaded").diff();
    let (old, new) = (shown.old.len(), shown.new.len());
    let changed = doc.parts(0) > 1;
    let placed = (blocks.values())
        .filter(|at| match at {
            Attach::Start | Attach::End => true,
            Attach::Line {
                side: Side::Old,
                line,
            } => changed && *line < old,
            Attach::Line {
                side: Side::New,
                line,
            } => changed && *line < new,
        })
        .count();
    prop_assert_eq!(seen, placed);
    Ok(())
}

/// Each change start, first to last and back, and the first past `line`.
fn changes(doc: &mut Document<()>, m: &Measured, line: usize) -> Vec<Shows> {
    let mut out = Vec::new();
    for last in [false, true] {
        let mut at = doc.edge(0, last, m);
        let mut seen = Vec::new();
        while let Some(pos) = at {
            seen.push(shows(doc, pos, m));
            doc.pos = pos;
            let step = if last {
                doc.prev_change(m)
            } else {
                doc.next_change(m)
            };
            at = match step {
                Some(Settle::Land(pos)) => Some(pos),
                _ => None,
            };
        }
        out.extend(seen);
    }
    let after = doc.change_after(0, line, m);
    out.extend(after.map(|pos| shows(doc, pos, m)));
    out
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Moving down from the top and back up visits every row, block rows
    /// included, in order; without the block rows they're the rows of the
    /// file without blocks; and each block's rows run unbroken, as tall as
    /// it was measured, under the row showing its line (or the separator
    /// hiding it), above the first row or under the last.
    #[test]
    fn positions_round_trip_through_rows_with_blocks(case in case()) {
        let (m, plain_m) = (case.measured(), case.measured());
        let mut doc = case.document(&m, true);
        let mut plain = case.document(&plain_m, false);
        rows_hold(&mut doc, &m, &mut plain, &plain_m)?;
    }

    /// Changes land on the rows they land on without blocks: the first,
    /// each next and previous, the last, and the first past a line.
    #[test]
    fn changes_land_on_their_rows_with_blocks(case in case(), line in 0..90usize) {
        let (m, plain_m) = (case.measured(), case.measured());
        let mut doc = case.document(&m, true);
        let mut plain = case.document(&plain_m, false);
        prop_assert_eq!(changes(&mut doc, &m, line), changes(&mut plain, &plain_m, line));
    }

    /// Scrolling down `n` rows and back up `n` returns to the same row.
    #[test]
    fn scrolling_by_n_then_back_returns(case in case(), from in any::<prop::sample::Index>(), n in 0..60usize) {
        let m = case.measured();
        let mut doc = case.document(&m, true);
        let rows = walk(&mut doc, &m);
        let start = rows[from.index(rows.len())].0;
        let mut pos = start;
        let moved = (0..n).take_while(|_| doc.next_row(&mut pos, &m)).count();
        for _ in 0..moved {
            prop_assert!(doc.prev_row(&mut pos, &m));
        }
        prop_assert_eq!(pos, start);
    }

    /// Search finds text in blocks as well as lines: in the order the rows
    /// show them, a line's match on the row showing its line and a block's
    /// on the row of the block the host locates its line on.
    #[test]
    fn search_results_land_on_their_rows(mut case in case()) {
        case.split = false;
        let m = case.measured();
        let mut doc = case.document(&m, true);
        let diff = Arc::clone(doc.diff_of(0).expect("loaded"));
        let hidden = doc.hidden_lines(0).expect("loaded");
        let blocks = doc.search_blocks(0);
        let found = find(&diff, Some(&hidden), &blocks, &Needle::new("e"), || false).expect("not cancelled");
        let mut last = None;
        for i in 0..found.len() {
            let hit = found.get(i).expect("in range");
            let pos = doc.hit_pos(0, &hit, &m).expect("shown");
            prop_assert!(last <= Some(pos), "{hit:?} at {pos:?} after {last:?}");
            last = Some(pos);
            match hit {
                Hit::Line(l) => {
                    prop_assert_eq!(doc.row_lines(pos, &m)[l.side.index()], Some(l.line as usize));
                }
                Hit::Block(b) => {
                    let line = (b.line as usize).min(m.heights[&b.block] - 1);
                    prop_assert_eq!(shows(&mut doc, pos, &m), Shows::Block(b.block, line));
                }
            }
        }
        prop_assert!(found.get(found.len()).is_none());
        for (i, &m) in found.list.iter().enumerate() {
            prop_assert_eq!(found.get(found.of_line(i)), Some(Hit::Line(m)));
        }
    }

    /// Revealing lines, a gap or the full text keeps a position on what it
    /// showed: a block's row, a line, or the line a separator hid first.
    #[test]
    fn reveal_and_expand_keep_a_position_with_blocks_in_gaps(
        case in case(),
        at in any::<prop::sample::Index>(),
        ops in prop::collection::vec(prop_oneof![
            (0..5usize).prop_map(Reveal::Expand),
            (any::<bool>(), 0..90usize).prop_map(|(old, line)| Reveal::Lines(old, line)),
            Just(Reveal::Full),
        ], 1..4),
    ) {
        let m = case.measured();
        let mut doc = case.document(&m, true);
        let rows = walk(&mut doc, &m);
        doc.pos = rows[at.index(rows.len())].0;
        for op in &ops {
            let pos = doc.pos;
            let before = shows(&mut doc, pos, &m);
            let lines = doc.row_lines(pos, &m);
            let hid = (lines == [None, None]).then(|| doc.line_of(pos, &m));
            reveal(&mut doc, op, &m);
            let now = doc.pos;
            match before {
                Shows::Block(..) => prop_assert_eq!(shows(&mut doc, now, &m), before),
                Shows::Parting(id) => {
                    let after = shows(&mut doc, now, &m);
                    prop_assert!(after == before || after == Shows::Block(id, 0), "{after:?}");
                }
                Shows::Own(_) if hid.is_none() => {
                    for side in [Side::Old, Side::New] {
                        if let Some(line) = lines[side.index()] {
                            prop_assert!(holds(&mut doc, now, side, line, &m), "{:?} then {:?}", pos, now);
                        }
                    }
                }
                Shows::Own(_) => prop_assert_eq!(Some(doc.line_of(now, &m)), hid),
            }
        }
    }

    /// Stepping between blocks goes through them in the order their rows
    /// show, both ways.
    #[test]
    fn stepping_goes_through_blocks_in_row_order(case in case()) {
        let m = case.measured();
        let mut doc = case.document(&m, true);
        let order: Vec<(Pos, BlockId)> = (walk(&mut doc, &m).into_iter())
            .filter_map(|(pos, s)| match s {
                Shows::Block(id, 0) => Some((pos, id)),
                _ => None,
            })
            .collect();
        let mut stepped = Vec::new();
        let mut at = doc.edge_block(0, false, &m);
        while let Some((pos, id)) = at {
            stepped.push((pos, id));
            at = match doc.step_block(pos, true, &m) {
                Some(BlockStep::At { pos, id }) => Some((pos, id)),
                _ => None,
            };
        }
        prop_assert_eq!(&stepped, &order);
        let mut back = Vec::new();
        let mut at = doc.edge_block(0, true, &m);
        while let Some((pos, id)) = at {
            back.push((pos, id));
            at = match doc.step_block(pos, false, &m) {
                Some(BlockStep::At { pos, id }) => Some((pos, id)),
                _ => None,
            };
        }
        back.reverse();
        prop_assert_eq!(back, order);
    }
}

/// A block is measured once per width, again when the host says it looks
/// different, and only when a call reaches the part it hangs in.
#[test]
fn blocks_are_measured_once_per_width_and_when_told() {
    let (old, new) = texts(80, &[(3, 0), (60, 0)]);
    let case = Case {
        old,
        new,
        blocks: vec![
            (
                Attach::Line {
                    side: Side::New,
                    line: 3,
                },
                2,
            ),
            (
                Attach::Line {
                    side: Side::New,
                    line: 60,
                },
                3,
            ),
        ],
        parting: 0,
        reveals: Vec::new(),
        split: false,
    };
    let m = case.measured();
    let mut doc = case.document(&m, true);
    let first = top(&doc);
    doc.rows(0, first.part + 1, &m);
    assert_eq!(m.calls.get(), 1, "only the first hunk's");
    walk(&mut doc, &m);
    walk(&mut doc, &m);
    assert_eq!(m.calls.get(), 2);
    doc.remeasure(0, Some(BlockId(1)), &m);
    walk(&mut doc, &m);
    assert_eq!(m.calls.get(), 3, "the one told");

    let mut pos = first;
    while doc.next_row(&mut pos, Wider(&m)) {}
    assert_eq!(m.calls.get(), 5, "both again at a new width");
}

/// The same blocks set again keep their version, so matches found in them
/// stay current, but are measured again: the host may draw them otherwise.
/// Others move it on.
#[test]
fn the_same_blocks_set_again_keep_their_version() {
    let (old, new) = texts(20, &[(3, 0)]);
    let (blocks, m) = on_lines(&[(Side::New, 3)]);
    let mut doc = document(&old, &new);
    doc.set_blocks(0, blocks.clone(), &m);
    walk(&mut doc, &m);
    let (version, calls) = (doc.blocks_version(0), m.calls.get());
    doc.set_blocks(0, blocks.clone(), &m);
    assert_eq!(doc.blocks_version(0), version);
    walk(&mut doc, &m);
    assert_eq!(m.calls.get(), calls + 1, "measured again");
    let mut other = blocks;
    other[0].text = Arc::from("changed");
    doc.set_blocks(0, other, &m);
    assert_ne!(doc.blocks_version(0), version);
}

/// Past a file's last block, a step goes on into the next entry the host
/// holds blocks for, though they aren't given yet, both ways; with none
/// held there's nowhere to go.
#[test]
fn a_step_past_the_last_block_goes_to_a_file_holding_blocks() {
    let (old, new) = texts(20, &[(3, 0)]);
    let (blocks, m) = on_lines(&[(Side::New, 3)]);
    let mut doc = document(&old, &new);
    for path in ["b.txt", "c.txt"] {
        doc.push(diff_at(path, &old, &new).change, Source::Repo);
    }
    doc.set_blocks(0, blocks, &m);
    let (last, _) = doc.edge_block(0, true, &m).expect("a block");
    assert_eq!(doc.step_block(last, true, &m), None);
    doc.set_held_blocks(&[0, 2]);
    assert_eq!(doc.step_block(last, true, &m), Some(BlockStep::File(2)));
    let start = Pos {
        file: 2,
        part: 0,
        row: 0,
        offset: 0,
    };
    assert_eq!(doc.step_block(start, false, &m), Some(BlockStep::File(0)));
    doc.set_held_blocks(&[]);
    assert_eq!(
        doc.step_block(start, false, &m),
        Some(BlockStep::File(0)),
        "given"
    );
}

/// Blocks on lines `at`, each two rows tall.
fn on_lines(at: &[(Side, usize)]) -> (Vec<Block>, Measured) {
    let blocks: Vec<Block> = (at.iter().zip(0u32..))
        .map(|(&(side, line), i)| Block {
            id: BlockId(i),
            at: Attach::Line { side, line },
            text: Arc::from(""),
        })
        .collect();
    let m = Measured {
        split: false,
        heights: blocks.iter().map(|b| (b.id, 2)).collect(),
        parting: 0,
        calls: Cell::new(0),
    };
    (blocks, m)
}

/// With the syntax diff shown, blocks hang under the rows showing their
/// lines as they do in the line diff.
#[test]
fn blocks_hang_in_the_syntax_diff() {
    let source = |changed: &str| -> String {
        let functions: Vec<String> = (0..12)
            .map(|i| {
                let body = if i == 6 { changed } else { "1" };
                format!("fn f{i}() -> u32 {{\n    {body}\n}}\n\n")
            })
            .collect();
        functions.concat()
    };
    let (old, new) = (source("1"), source("1 + 2"));
    let structural = |doc: &mut Document<()>| {
        let diff = Arc::clone(doc.diff_of(0).expect("loaded"));
        let result = File::prepare(&GRAMMARS, None).structural(&diff);
        assert!(doc.set_structural(0, &diff, result));
        doc.structural = true;
    };
    let (blocks, m) = on_lines(&[(Side::New, 25), (Side::Old, 25), (Side::New, 2)]);
    let (_, plain_m) = on_lines(&[]);
    let mut doc = document_at("a.rs", &old, &new);
    structural(&mut doc);
    doc.set_blocks(0, blocks, &m);
    assert!(doc.file(0).expect("loaded").uses_structural(true));
    let mut plain = document_at("a.rs", &old, &new);
    structural(&mut plain);
    rows_hold(&mut doc, &m, &mut plain, &plain_m).expect("rows hold");
}

/// In a rendered view, a block hangs under the last rendered line drawn
/// from its line, and search looks for it there.
#[test]
fn blocks_in_a_rendered_view_hang_under_the_lines_drawn_from_theirs() {
    let (old, new) = texts(20, &[(10, 0)]);
    // Every two source lines render as one.
    let pairs = |text: &str| -> String {
        let lines: Vec<&str> = text.lines().collect();
        let rendered: Vec<String> = (lines.chunks(2))
            .map(|c| format!("{}\n", c.join(" ")))
            .collect();
        rendered.concat()
    };
    let origins = || -> Vec<Origin> {
        (0..10u32)
            .map(|k| Origin {
                start: 2 * k,
                end: 2 * k + 2,
                first: true,
            })
            .collect()
    };
    let rendered = || {
        let mut doc = document(&old, &new);
        let _ = doc.set_rendered_mode(true);
        let source = Arc::clone(doc.diff_of(0).expect("loaded"));
        let view = File::rendered_view(
            Arc::clone(&source),
            diff(&pairs(&old), &pairs(&new)),
            [origins(), origins()],
            [vec![Vec::new(); 10], vec![Vec::new(); 10]],
            80,
            "same",
        );
        let _ = doc.set_rendered(0, &source, Ok(Box::new(view)));
        assert!(doc.file(0).expect("loaded").rendered().is_some());
        doc
    };
    let at = [
        (Side::New, 10),
        (Side::Old, 3),
        (Side::New, 19),
        (Side::Old, 25),
    ];
    let (blocks, m) = on_lines(&at);
    let (_, plain_m) = on_lines(&[]);
    let mut doc = rendered();
    doc.set_blocks(0, blocks, &m);
    let mut plain = rendered();
    rows_hold(&mut doc, &m, &mut plain, &plain_m).expect("rows hold");
    let line = |side, line| Attach::Line { side, line };
    let searched: Vec<Attach> = doc.search_blocks(0).into_iter().map(|b| b.at).collect();
    assert_eq!(
        searched,
        [line(Side::New, 5), line(Side::Old, 1), line(Side::New, 9)]
    );
}

/// A page has no lines: its blocks hang under its first row and under its
/// last, among the rows the host draws, those at one place parted; a
/// line's block has no rows there.
#[test]
fn a_sheet_holds_blocks_at_its_edges() {
    let m = Measured {
        split: false,
        heights: [
            (BlockId(0), 2),
            (BlockId(1), 3),
            (BlockId(2), 1),
            (BlockId(3), 1),
        ]
        .into(),
        parting: 1,
        calls: Cell::new(0),
    };
    let mut doc = Document::<()>::default();
    doc.push(diff("", "").change, Source::Page { rows: 2 });
    let block = |id, at| Block {
        id: BlockId(id),
        at,
        text: Arc::from(""),
    };
    let on_line = Attach::Line {
        side: Side::New,
        line: 0,
    };
    let blocks = vec![
        block(0, Attach::End),
        block(1, Attach::Start),
        block(2, on_line),
        block(3, Attach::End),
    ];
    doc.set_blocks(0, blocks, &m);
    assert_eq!(doc.rows(0, 0, &m), 9);
    let shown: Vec<Shows> = walk(&mut doc, &m).into_iter().map(|(_, s)| s).collect();
    let own = |row: usize| Shows::Own(format!("sheet {row}"));
    let (start, end, last) = (BlockId(1), BlockId(0), BlockId(3));
    assert_eq!(
        shown,
        [
            own(0),
            Shows::Block(start, 0),
            Shows::Block(start, 1),
            Shows::Block(start, 2),
            own(1),
            Shows::Block(end, 0),
            Shows::Block(end, 1),
            Shows::Parting(last),
            Shows::Block(last, 0),
        ]
    );
    assert_eq!(doc.block_pos(0, BlockId(2), &m), None);
    assert_eq!(doc.block_pos(0, end, &m).map(|p| p.row), Some(5));
    assert_eq!(doc.block_pos(0, last, &m).map(|p| p.row), Some(8));
}

/// The line the row at `pos` shows, the new side's first, or the first
/// line in the next few own rows with how many own rows down it is.
fn line_at(doc: &mut Document<()>, pos: Pos, m: &Measured) -> Option<(Side, usize, usize)> {
    let mut at = pos;
    let mut down = 0;
    while down < 3 {
        match doc.row_lines(at, m) {
            [_, Some(new)] => return Some((Side::New, new, down)),
            [Some(old), None] => return Some((Side::Old, old, down)),
            [None, None] => {
                down += usize::from(matches!(shows(doc, at, m), Shows::Own(_)));
                if !doc.next_row(&mut at, m) {
                    return None;
                }
            }
        }
    }
    None
}

/// Laid out anew (another arrangement, boxes of other heights, the syntax
/// diff on or off), the row at the top shows what it showed: its line, a
/// block's row, or the header or separator over the same line.
#[test]
fn relayout_keeps_the_top_on_what_it_showed() {
    let (old, new) = texts(60, &[(10, 0), (11, 1), (12, 2), (40, 0)]);
    let line = |side, line| Attach::Line { side, line };
    let case = Case {
        old,
        new,
        blocks: vec![
            (line(Side::Old, 10), 3),
            (line(Side::New, 10), 2),
            (Attach::Start, 1),
            (line(Side::New, 40), 2),
        ],
        parting: 1,
        reveals: vec![Reveal::Expand(1)],
        split: false,
    };
    let unified = case.measured();
    let mut split = Case {
        split: true,
        ..case.clone()
    }
    .measured();
    for h in split.heights.values_mut() {
        *h += 2;
    }
    let mut doc = case.document(&unified, true);
    for (pos, before) in walk(&mut doc, &unified) {
        doc.pos = pos;
        let shown = line_at(&mut doc, pos, &unified);
        doc.relayout(&unified, &split, |_| {});
        let now = doc.pos;
        match before {
            Shows::Block(..) => assert_eq!(shows(&mut doc, now, &split), before, "{pos:?}"),
            Shows::Parting(id) => assert_eq!(shows(&mut doc, now, &split), Shows::Block(id, 0)),
            // The separator after the last hunk: left for the host.
            Shows::Own(_) if shown.is_none() => {
                let mut at = pos;
                while doc.next_row(&mut at, &unified) {
                    assert_eq!(doc.row_lines(at, &unified), [None, None], "{pos:?}");
                }
            }
            Shows::Own(_) => {
                let (side, line, down) = shown.expect("a line");
                let mut at = now;
                let mut own = 0;
                loop {
                    let is_own = matches!(shows(&mut doc, at, &split), Shows::Own(_));
                    if is_own && own == down {
                        break;
                    }
                    own += usize::from(is_own);
                    assert!(doc.next_row(&mut at, &split));
                }
                assert_eq!(
                    doc.row_lines(at, &split)[side.index()],
                    Some(line),
                    "{pos:?}"
                );
            }
        }
        doc.relayout(&split, &unified, |_| {});
    }

    let source = |changed: &str| -> String {
        let functions: Vec<String> = (0..12)
            .map(|i| {
                let body = if i == 6 { changed } else { "1" };
                format!("fn f{i}() -> u32 {{\n    {body}\n}}\n\n")
            })
            .collect();
        functions.concat()
    };
    let mut doc = document_at("a.rs", &source("1"), &source("1 + 2"));
    let diff = Arc::clone(doc.diff_of(0).expect("loaded"));
    let result = File::prepare(&GRAMMARS, None).structural(&diff);
    assert!(doc.set_structural(0, &diff, result));
    doc.toggle_full(&unified);
    let mut pos = top(&doc);
    while doc.row_lines(pos, &unified)[1] != Some(30) {
        assert!(doc.next_row(&mut pos, &unified));
    }
    doc.pos = pos;
    doc.relayout(&unified, &unified, |doc| doc.structural = true);
    assert!(doc.file(0).expect("loaded").uses_structural(true));
    assert_eq!(doc.row_lines(doc.pos, &unified)[1], Some(30));
}

/// Blocks set, measured again or laid out anew while the view stays where
/// a change it couldn't put at the top left it keep that change as the one
/// `n` counts from: the next is the one after it.
#[test]
fn the_change_landed_on_outlives_blocks_changing() {
    let (old, new) = texts(80, &[(10, 0), (40, 0), (70, 0)]);
    let line = |line| Attach::Line {
        side: Side::New,
        line,
    };
    let case = Case {
        old,
        new,
        blocks: vec![(line(2), 2), (line(11), 3), (line(41), 2)],
        parting: 1,
        reveals: Vec::new(),
        split: false,
    };
    let m = case.measured();
    let mut doc = case.document(&m, true);
    let first = doc.edge(0, false, &m).expect("a change");
    let land = |doc: &mut Document<()>| {
        let mut view = first;
        assert!(doc.prev_row(&mut view, &m));
        doc.pos = view;
        doc.landed(first);
    };
    let next = |doc: &mut Document<()>| match doc.next_change(&m) {
        Some(Settle::Land(pos)) => doc.row_lines(pos, &m),
        other => panic!("{other:?}"),
    };
    land(&mut doc);
    let after = next(&mut doc);
    assert_ne!(after, doc.row_lines(first, &m));
    land(&mut doc);
    doc.remeasure(0, None, &m);
    assert_eq!(next(&mut doc), after, "measured again");
    land(&mut doc);
    doc.set_blocks(0, case.blocks()[1..].to_vec(), &m);
    assert_eq!(next(&mut doc), after, "set again");
    doc.set_blocks(0, case.blocks(), &m);
    land(&mut doc);
    doc.relayout(&m, &m, |_| {});
    assert_eq!(next(&mut doc), after, "laid out anew");
}

/// A gap's separator alone on screen asks for no syntax of its lines, the
/// rows of a block hung under it included; a line of it on screen does.
#[test]
fn missing_counts_a_gaps_own_rows() {
    let (old, new) = texts(80, &[(3, 0), (60, 0)]);
    let case = Case {
        old,
        new,
        blocks: vec![(
            Attach::Line {
                side: Side::New,
                line: 30,
            },
            3,
        )],
        parting: 0,
        reveals: Vec::new(),
        split: false,
    };
    let m = case.measured();
    let mut doc = document_at("a.rs", &case.old, &case.new);
    doc.set_blocks(0, case.blocks(), &m);
    let gap_syntax = |doc: &mut Document<()>, from: Pos, to: Pos| {
        (doc.missing(from, to))
            .filter(|w| matches!(w, crate::document::Missing::GapSyntax(_)))
            .count()
    };
    let rows = walk(&mut doc, &m);
    let k = (rows.iter())
        .position(|(_, s)| *s == Shows::Block(BlockId(0), 0))
        .expect("placed");
    assert!(matches!(&rows[k - 1].1, Shows::Own(s) if s.contains("hides")));
    assert_eq!(gap_syntax(&mut doc, rows[k - 1].0, rows[k + 2].0), 0);
    assert!(doc.expand(0, 1, &m));
    let rows = walk(&mut doc, &m);
    let k = (rows.iter())
        .position(|(_, s)| *s == Shows::Block(BlockId(0), 0))
        .expect("placed");
    let separator = rows[k - 1].0;
    assert_eq!(gap_syntax(&mut doc, separator, rows[k + 2].0), 0);
    let revealed = Pos {
        row: 0,
        ..separator
    };
    assert!(doc.row_lines(revealed, &m)[1].is_some());
    assert_eq!(gap_syntax(&mut doc, revealed, rows[k + 2].0), 1);
}

/// A position on a block's row stays on it when other blocks come and go
/// above it, and one on a line stays on that line.
#[test]
fn set_blocks_keeps_the_position_on_its_row() {
    let (old, new) = texts(20, &[(10, 0)]);
    let line = |line| Attach::Line {
        side: Side::New,
        line,
    };
    let mut case = Case {
        old,
        new,
        blocks: vec![(line(8), 2), (line(10), 3)],
        parting: 0,
        reveals: Vec::new(),
        split: false,
    };
    let m = case.measured();
    let mut doc = case.document(&m, true);
    let on_block = doc.block_pos(0, BlockId(1), &m).expect("placed");
    doc.pos = Pos {
        row: on_block.row + 2,
        ..on_block
    };
    case.blocks[0].1 = 4;
    let m = case.measured();
    doc.remeasure(0, None, &m);
    let pos = doc.pos;
    assert_eq!(shows(&mut doc, pos, &m), Shows::Block(BlockId(1), 2));
    doc.set_blocks(0, case.blocks()[1..].to_vec(), &m);
    let pos = doc.pos;
    assert_eq!(shows(&mut doc, pos, &m), Shows::Block(BlockId(1), 2));
    doc.set_blocks(0, Vec::new(), &m);
    assert_eq!(doc.row_lines(doc.pos, &m)[1], Some(10), "under its line");
}

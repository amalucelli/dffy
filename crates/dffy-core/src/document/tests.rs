use std::sync::LazyLock;

use super::*;
use crate::diff::{Options, Text, hunks};
use crate::git::Side as GitSide;
use crate::highlight::Registry;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

/// A capital letter makes the search match case.
#[test]
fn smart_case() {
    assert_eq!(Needle::new("needle").find_all(b"Needle needle").len(), 2);
    assert_eq!(Needle::new("Needle").find_all(b"Needle needle").len(), 1);
}

fn unified(_: &HunkHeader<'_>) -> Arrangement {
    Arrangement::Unified
}

/// 80 lines of `path`, changed at lines 3 and 60: the first hunk's `@@`
/// row, then a gap of 50 lines between the hunks and one of 17 after the
/// last, so parts 1 to 5 are start, hunk, gap, hunk, gap.
fn long_diff(path: &str) -> FileDiff {
    let old = (1..=80)
        .map(|i| format!("int line_{i};\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("int line_3;\n", "int line_three;\n")
        .replace("int line_60;\n", "int line_sixty;\n");
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

fn prepared(fd: FileDiff) -> Box<File> {
    Box::new(File::prepare(&GRAMMARS, None).file(fd, Whitespace::Exact, true))
}

fn at(file: usize, part: usize, row: usize) -> Pos {
    Pos {
        file,
        part,
        row,
        offset: 0,
    }
}

/// The work missing for drawn rows: emphasis of the hunks among them, syntax
/// runs of a gap only once rows with its lines are among them, then the
/// syntax diff when it shows; nothing across files, nothing done twice.
#[test]
fn missing_names_the_work_for_the_rows_shown() {
    let mut doc = Document::<()>::default();
    doc.push(long_diff("long.c").change, Source::Repo);
    let _ = doc.load(0, Ok(prepared(long_diff("long.c"))));
    let missing = |doc: &Document<()>, first, last| doc.missing(first, last).collect::<Vec<_>>();
    let (top, bottom) = (at(0, 1, 0), at(0, 5, 0));
    assert_eq!(
        missing(&doc, top, bottom),
        [Missing::Emphasis(0), Missing::Emphasis(1)],
        "the gaps show only their separators"
    );
    assert_eq!(missing(&doc, top, at(1, 1, 0)), Vec::<Missing>::new());

    let diff = Arc::clone(doc.diff_of(0).expect("loaded"));
    let (emphasis, syntax) = File::prepare(&GRAMMARS, None).emphasis(&diff, 0, None);
    doc.set_emphasis(0, &diff, 0, emphasis, syntax);
    assert_eq!(missing(&doc, top, bottom), [Missing::Emphasis(1)]);

    // Ten lines each side of the separator: rows 0..10, 10, 11..21.
    assert!(doc.expand(0, 1));
    assert_eq!(doc.shown(0, 1).map(Shown::rows), Some(21));
    assert_eq!(
        missing(&doc, at(0, 3, 10), at(0, 3, 10)),
        Vec::<Missing>::new()
    );
    assert_eq!(
        missing(&doc, at(0, 3, 10), at(0, 3, 11)),
        [Missing::GapSyntax(1)]
    );
    assert_eq!(
        missing(&doc, at(0, 2, 0), at(0, 3, 0)),
        [Missing::GapSyntax(1)]
    );

    doc.structural = true;
    assert_eq!(
        missing(&doc, top, bottom),
        [
            Missing::Emphasis(1),
            Missing::GapSyntax(1),
            Missing::Structural
        ]
    );
}

/// Lines revealed inside a gap show between two separators, each counting
/// the lines it hides, the hunk's numbers only on the last; search scans
/// them. A position on a separator stays on its line, one on a line moves
/// with it, and so does the place another file was left at. A window
/// reaching an edge or another window joins it, and `x` opens the rest as
/// before.
#[test]
fn lines_revealed_inside_a_gap_show_between_two_separators() {
    let mut doc = Document::<()>::default();
    doc.push(long_diff("long.c").change, Source::Repo);
    doc.push(long_diff("other.c").change, Source::Repo);
    let _ = doc.load(0, Ok(prepared(long_diff("long.c"))));
    doc.open(0, unified);
    let separator = |doc: &mut Document<()>, row| match doc.row(at(0, 3, row), unified) {
        Some(Content::Separator { hidden, next }) => Some((hidden, next.is_some())),
        _ => None,
    };
    // Gap 1 holds lines 7 to 56; line 30 is its 24th, so offset 23.
    doc.pos = at(0, 3, 0);
    assert!(doc.reveal_lines(0, Side::New, 29..30, 2));
    assert!(!doc.reveal_lines(0, Side::New, 29..30, 2), "already shown");
    let shown = doc.shown(0, 1).expect("loaded");
    let lines: Vec<Option<usize>> = (0..shown.rows()).map(|r| shown.line(r)).collect();
    assert_eq!(
        lines,
        [None, Some(21), Some(22), Some(23), Some(24), Some(25), None]
    );
    assert_eq!((shown.row(10), shown.row(23), shown.row(40)), (0, 3, 6));
    assert_eq!(separator(&mut doc, 0), Some((21, false)));
    assert_eq!(separator(&mut doc, 6), Some((24, true)));
    assert_eq!(
        doc.pos,
        at(0, 3, 0),
        "its first hidden line is still hidden"
    );
    assert_eq!(doc.hidden_lines(0), Some(vec![6..27, 32..56, 63..80]));

    // On line 24 of the gap, then lines next to the top edge join it.
    doc.pos = at(0, 3, 4);
    assert!(doc.reveal_lines(0, Side::Old, 7..8, 2));
    let shown = doc.shown(0, 1).expect("loaded");
    assert_eq!((shown.top, shown.rows()), (4, 4 + 1 + 5 + 1));
    assert_eq!(doc.pos, at(0, 3, 4 + 1 + 3), "on the same line");

    // 41 hidden lines: `x` reveals 20 from the edges.
    assert!(doc.expand(0, 1));
    assert_eq!(doc.shown(0, 1).map(Shown::hidden), Some(21));
    assert_eq!(doc.pos, at(0, 3, 14 + 1 + 3));

    // Left for another file, then lines just above the window join it.
    let _ = doc.goto_file(1);
    assert!(doc.reveal_lines(0, Side::New, 24..25, 2));
    assert_eq!(doc.goto_file(0), Some(Settle::Keep));
    assert_eq!(doc.pos, at(0, 3, 14 + 1 + 8));

    // 16 hidden lines: `x` opens them whole.
    assert!(doc.expand(0, 1));
    assert_eq!(
        doc.shown(0, 1).map(|s| (s.hidden(), s.rows())),
        Some((0, 50))
    );
    assert_eq!(doc.pos, at(0, 3, 24));
}

/// An edge `x` reveals past a window takes the window in, keeping every
/// line it reached.
#[test]
fn an_edge_revealed_past_a_window_keeps_its_lines() {
    let mut doc = Document::<()>::default();
    doc.push(long_diff("long.c").change, Source::Repo);
    let _ = doc.load(0, Ok(prepared(long_diff("long.c"))));
    // Line 53, the gap's 47th: 49 lines stay hidden, so ten from each edge.
    assert!(doc.reveal_lines(0, Side::New, 52..53, 0));
    assert!(doc.expand(0, 1));
    let shown = doc.shown(0, 1).expect("loaded");
    assert_eq!((shown.top, shown.bottom, shown.windows), (10, 10, &[][..]));
}

/// `n` lands on each change of the file shown, then shows the next file;
/// one not diffed yet waits, and opens landing on its first change once
/// its diff is loaded. Each file keeps the place it was left at.
#[test]
fn changes_lead_across_files_and_wait_for_a_diff() {
    let mut doc = Document::<()>::default();
    doc.push(long_diff("a.c").change, Source::Repo);
    doc.push(long_diff("b.c").change, Source::Repo);
    assert_eq!(
        doc.load(0, Ok(prepared(long_diff("a.c")))),
        Some(Settle::Open {
            file: 0,
            landing: false
        })
    );
    let first = doc.open(0, unified).expect("a change");
    assert_eq!(doc.pos, at(0, 1, 0), "opened at its top");

    let mut landed = Vec::new();
    let past = loop {
        match doc.next_change(unified) {
            Some(Settle::Land(change)) => {
                doc.pos = change;
                doc.landed(change);
                landed.push(change);
            }
            other => break other,
        }
    };
    assert_eq!(landed.len(), 2);
    assert_eq!(landed[0], first);
    assert_eq!(
        past,
        Some(Settle::Wait {
            file: 1,
            last: false
        })
    );
    assert_eq!(doc.pos.file, 1, "past the last change, the next file");

    doc.pending_land = Some((doc.pos, false));
    assert_eq!(
        doc.load(1, Ok(prepared(long_diff("b.c")))),
        Some(Settle::Open {
            file: 1,
            landing: true
        })
    );
    assert_eq!(doc.pending_land, None);

    assert_eq!(doc.goto_file(0), Some(Settle::Keep));
    assert_eq!(doc.pos, landed[1], "back where it was left");
    assert_eq!(doc.goto_file(0), None, "already shown");
}

/// Eviction drops the least recently shown diffs first, never one in the
/// range kept, and a dropped file wants its diff again.
#[test]
fn eviction_drops_the_least_recently_shown_outside_the_kept_range() {
    let mut doc = Document::<()>::default();
    for (i, path) in ["a.c", "b.c"].into_iter().enumerate() {
        doc.push(long_diff(path).change, Source::Repo);
        let _ = doc.load(i, Ok(prepared(long_diff(path))));
    }
    doc.touch(0, 2);
    doc.touch(1, 1);
    let total = doc.loaded_bytes();
    doc.evict(total - 1, &(0..0));
    assert!(doc.file(0).is_some());
    assert!(doc.file(1).is_none() && doc.files()[1].wants_diff());
    doc.evict(0, &(0..1));
    assert!(doc.file(0).is_some(), "the kept range stays");
    assert_eq!(doc.loaded_bytes(), doc.file(0).map_or(0, File::bytes));
}

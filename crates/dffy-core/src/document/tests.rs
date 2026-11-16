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

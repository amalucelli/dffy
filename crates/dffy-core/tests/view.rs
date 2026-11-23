//! The document model driven the way an embedder would, through the core's
//! public API alone: a fixture repository's changes and a piped patch
//! become a [`Document`], prepared with [`File::prepare`] and completed with
//! the work [`Document::missing`] names.
#![expect(
    clippy::expect_used,
    reason = "fixture helpers fail the test at the step that broke"
)]

use std::fmt::Write as _;
use std::ops::ControlFlow;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, LazyLock};

use dffy_core::diff::{Options, Whitespace};
use dffy_core::document::{
    self, Content, Document, File, Match, Missing, Needle, Piped, Pos, Segment, Settle, Side,
    Source, find,
};
use dffy_core::git::{FileChange, Repo, Status};
use dffy_core::highlight::Registry;
use dffy_core::patch::{self, Builder, Item, Parser};
use dffy_core::view::{Arrangement, FileStatus, HunkHeader, LineState, Row};

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn write(dir: &Path, path: &str, content: &str) {
    let full = dir.join(path);
    std::fs::create_dir_all(full.parent().expect("has parent")).expect("mkdir");
    std::fs::write(full, content).expect("write");
}

const MOVED_OLD: &str = "static int add(int a, int b)\n{\n\treturn a + b;\n}\n\n\
static int sub(int a, int b)\n{\n\treturn a - b;\n}\n\n\
int main(void)\n{\n\treturn add(1, 2) + sub(3, 4);\n}\n";
const MOVED_NEW: &str = "static int sub(int a, int b)\n{\n\treturn a - b;\n}\n\n\
static int add(int a, int b)\n{\n\treturn a + b;\n}\n\n\
int main(void)\n{\n\treturn add(2, 1) + sub(4, 3);\n}\n";

/// 80 lines; changed, line 3 gets an initializer and line 60 is renamed.
fn long(changed: bool) -> String {
    let old = (1..=80).fold(String::new(), |mut out, i| {
        let _ = writeln!(out, "int line_{i};");
        out
    });
    if changed {
        old.replace("int line_3;\n", "int line_3 = 3;\n")
            .replace("int line_60;\n", "int LINE_60 = 60;\n")
    } else {
        old
    }
}

/// A repository whose last commit moves a function in `moved.c`, changes
/// two far-apart lines of `long.c` and adds `notes.txt`.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    write(d, "moved.c", MOVED_OLD);
    write(d, "long.c", &long(false));
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "base"]);
    write(d, "moved.c", MOVED_NEW);
    write(d, "long.c", &long(true));
    write(d, "notes.txt", "first\nsecond\n");
    git(d, &["add", "-A"]);
    git(d, &["commit", "-q", "-m", "change"]);
    dir
}

fn unified(_: &HunkHeader<'_>) -> Arrangement {
    Arrangement::Unified
}

fn split(_: &HunkHeader<'_>) -> Arrangement {
    Arrangement::Split
}

fn at(file: usize, part: usize, row: usize) -> Pos {
    Pos {
        file,
        part,
        row,
        offset: 0,
    }
}

/// Entry `file`'s diff as a host's worker prepares it: paired and
/// highlighted, nothing else computed.
fn prepared(repo: &Repo, doc: &Document<()>, file: usize) -> Box<File> {
    let change = doc.files()[file].change().clone();
    let diff = repo.file_diff(change, &Options::default()).expect("diff");
    Box::new(File::prepare(&GRAMMARS, None).file(diff, Whitespace::Exact, true))
}

/// The last commit's changes as a document, every diff loaded. The
/// position is on the first entry, which hasn't been opened.
fn document(dir: &Path) -> (Repo, Document<()>) {
    let repo = Repo::discover(dir).expect("repo");
    let spec = repo
        .resolve(&["HEAD~1", "HEAD"], false, false)
        .expect("spec");
    let mut changes = Vec::new();
    repo.for_each_change(&spec, &[], |c| {
        changes.push(c);
        ControlFlow::Continue(())
    })
    .expect("enumerates");
    let mut doc = Document::default();
    for change in repo.detect_renames(changes, None).expect("renames") {
        doc.push(change, Source::Repo);
    }
    for file in 0..doc.files().len() {
        assert!(doc.files()[file].wants_diff());
        let _ = doc.load(file, Ok(prepared(&repo, &doc, file)));
    }
    (repo, doc)
}

/// The entry for `path`.
fn entry<P>(doc: &Document<P>, path: &str) -> usize {
    doc.files()
        .iter()
        .position(|e| e.change().path == path)
        .expect("listed")
}

/// The last row of `file`.
fn end<P>(doc: &mut Document<P>, file: usize) -> Pos {
    let part = doc.parts(file) - 1;
    at(file, part, doc.rows(file, part, unified) - 1)
}

/// Runs the work `missing` names for the rows from `first` to `last`, as a
/// host's pool would, and stores each result, until none is missing: the
/// first highlighting leaves the definitions to find after it.
fn fill(doc: &mut Document<()>, first: Pos, last: Pos) {
    let prepare = File::prepare(&GRAMMARS, None);
    loop {
        let missing: Vec<Missing> = doc.missing(first, last).collect();
        if missing.is_empty() {
            return;
        }
        for work in missing {
            let file = doc.file(first.file).expect("loaded");
            let diff = Arc::clone(file.diff());
            match work {
                Missing::Emphasis(hunk) => {
                    let (emphasis, syntax) = prepare.emphasis(&diff, hunk, file.syntax());
                    doc.set_emphasis(first.file, &diff, hunk, emphasis, syntax);
                }
                Missing::GapSyntax(gap, side) => {
                    let slot = Arc::clone(file.syntax().expect("highlighted"));
                    let spans = prepare.gap_syntax(&diff, &slot, side, file.gap_lines(gap, side));
                    doc.set_gap_syntax(first.file, &diff, gap, side, spans);
                }
                Missing::Scopes => prepare.scopes(&diff, file.syntax().expect("highlighted")),
                Missing::Structural => {
                    let result = prepare.structural(&diff);
                    assert!(doc.set_structural(first.file, &diff, result));
                }
                Missing::Render => unreachable!("rendered views are off"),
            }
        }
    }
}

/// Every row of hunk part `part` of `file`, with the state of each line it
/// shows.
fn rows<P>(
    doc: &mut Document<P>,
    file: usize,
    part: usize,
    arrange: fn(&HunkHeader<'_>) -> Arrangement,
) -> Vec<(Row, Vec<LineState>)> {
    let n = doc.rows(file, part, arrange);
    (0..n)
        .map(|r| match doc.row(at(file, part, r), arrange) {
            Some(Content::Hunk { row, lines, .. }) => {
                let states = match row {
                    Row::Line(i) | Row::One { line: i, .. } => vec![lines.line(i).state],
                    Row::Pair(old, new) => old
                        .into_iter()
                        .chain(new)
                        .map(|i| lines.line(i).state)
                        .collect(),
                    Row::Header | Row::Marker | Row::Hidden { .. } => Vec::new(),
                };
                (row, states)
            }
            other => unreachable!("not a hunk row: {other:?}"),
        })
        .collect()
}

/// The states of every line `part` of `file` shows, in order.
fn states<P>(doc: &mut Document<P>, file: usize, part: usize) -> Vec<LineState> {
    rows(doc, file, part, unified)
        .into_iter()
        .flat_map(|(_, states)| states)
        .collect()
}

#[test]
fn a_host_fills_the_work_missing_for_the_rows_it_shows() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let l = entry(&doc, "long.c");
    doc.pos = at(l, 1, 0);
    assert!(doc.expand(l, 1, unified));
    doc.structural = true;
    let (first, last) = (at(l, 1, 0), end(&mut doc, l));
    assert_eq!(
        doc.missing(first, last).collect::<Vec<_>>(),
        [
            Missing::Emphasis(0),
            Missing::Emphasis(1),
            Missing::GapSyntax(1, Side::New),
            Missing::Structural
        ],
        "the last gap shows only its separator"
    );

    fill(&mut doc, first, last);
    assert_eq!(doc.missing(first, last).count(), 0);
    let Some(Content::Hunk { lines, .. }) = doc.row(at(l, 2, 2), unified) else {
        unreachable!("a hunk row");
    };
    let removed = lines.line(2);
    assert_eq!(removed.state, LineState::RemovedPaired);
    assert!(removed.syntax.is_some());
    assert!(!lines.line(3).emphasis.is_empty(), "the added initializer");
    let Some(Content::Gap { line, .. }) = doc.row(at(l, 3, 0), unified) else {
        unreachable!("a revealed gap line");
    };
    assert!(line.syntax.is_some());
}

#[test]
fn unified_rows_are_lines_and_split_rows_pair_removed_with_added() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let l = entry(&doc, "long.c");
    let context = || vec![LineState::Context];
    let both = || vec![LineState::Context; 2];
    assert_eq!(
        rows(&mut doc, l, 2, unified),
        [
            (Row::Line(0), context()),
            (Row::Line(1), context()),
            (Row::Line(2), vec![LineState::RemovedPaired]),
            (Row::Line(3), vec![LineState::AddedPaired]),
            (Row::Line(4), context()),
            (Row::Line(5), context()),
            (Row::Line(6), context()),
        ]
    );
    assert_eq!(
        rows(&mut doc, l, 2, split),
        [
            (Row::Pair(Some(0), Some(0)), both()),
            (Row::Pair(Some(1), Some(1)), both()),
            (
                Row::Pair(Some(2), Some(3)),
                vec![LineState::RemovedPaired, LineState::AddedPaired]
            ),
            (Row::Pair(Some(4), Some(4)), both()),
            (Row::Pair(Some(5), Some(5)), both()),
            (Row::Pair(Some(6), Some(6)), both()),
        ]
    );
    let n = entry(&doc, "notes.txt");
    assert_eq!(
        rows(&mut doc, n, 1, split),
        [
            (Row::Pair(None, Some(0)), vec![LineState::Added]),
            (Row::Pair(None, Some(1)), vec![LineState::Added]),
        ],
        "an added file's lines have no old side to pair with"
    );
}

#[test]
fn revealing_a_gap_keeps_positions_in_hunks_and_on_revealed_lines() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let l = entry(&doc, "long.c");
    let in_hunk = at(l, 4, 3);
    doc.pos = in_hunk;
    let lines = doc.row_lines(in_hunk, unified);
    assert_eq!(lines, [Some(59), None]);
    assert!(doc.expand(l, 1, unified));
    assert!(doc.normalize(unified));
    assert_eq!(doc.pos, in_hunk);
    assert!(doc.toggle_full(unified));
    assert!(doc.normalize(unified));
    assert_eq!(doc.pos, in_hunk);
    assert_eq!(doc.row_lines(doc.pos, unified), lines);
    assert!(doc.toggle_full(unified));

    // Ten lines each side of the separator; row 11 is the first of the
    // bottom ten, gap line 40.
    assert_eq!(doc.shown(l, 1).map(|s| (s.top, s.bottom)), Some((10, 10)));
    doc.pos = at(l, 3, 11);
    let lines = doc.row_lines(doc.pos, unified);
    assert_eq!(lines, [Some(46), Some(46)]);
    assert!(doc.toggle_full(unified));
    assert_eq!(doc.pos, at(l, 3, 40), "the whole gap shows above it");
    assert_eq!(doc.row_lines(doc.pos, unified), lines);
    assert!(doc.toggle_full(unified));
    assert_eq!(doc.pos, at(l, 3, 11));
    assert!(doc.expand(l, 1, unified));
    assert_eq!(doc.pos, at(l, 3, 40), "the rest of the gap opened");
    assert_eq!(doc.row_lines(doc.pos, unified), lines);
}

/// An unchanged line shown in a gap is found from either side's number.
#[test]
fn a_gap_line_is_found_by_its_old_number_too() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let l = entry(&doc, "long.c");
    doc.pos = at(l, 1, 0);
    assert!(doc.toggle_full(unified));
    let rows = [Side::Old, Side::New].map(|side| {
        let m = Match {
            side,
            line: 20,
            start: 0,
            end: 0,
        };
        doc.match_pos(l, &m, unified)
    });
    let row = rows[1].expect("shown");
    assert_eq!(doc.row_lines(row, unified), [Some(20), Some(20)]);
    assert_eq!(rows[0], Some(row));
}

#[test]
fn the_syntax_diff_toggle_keeps_the_part_and_marks_moved_lines() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let m = entry(&doc, "moved.c");
    let moved = [LineState::RemovedMoved, LineState::AddedMoved];
    let paired = at(m, 2, 17);
    doc.pos = paired;
    let lines = doc.row_lines(paired, unified);
    assert!(
        !states(&mut doc, m, 2).iter().any(|s| moved.contains(s)),
        "the line diff knows nothing moved"
    );

    doc.structural = true;
    let last = end(&mut doc, m);
    fill(&mut doc, at(m, 1, 0), last);
    for _ in 0..2 {
        assert!(doc.normalize(unified));
        assert_eq!(doc.pos, paired);
        assert_eq!(doc.segment(m, doc.pos.part), Some(Segment::Hunk(0)));
        assert_eq!(doc.row_lines(doc.pos, unified), lines);
        let shown = states(&mut doc, m, 2);
        assert_eq!(
            moved.map(|s| shown.contains(&s)),
            [doc.structural; 2],
            "{shown:?}"
        );
        doc.structural = !doc.structural;
    }
}

#[test]
fn search_counts_matches_with_smart_case_and_finds_their_rows() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let l = entry(&doc, "long.c");
    doc.pos = at(l, 1, 0);
    let diff = Arc::clone(doc.diff_of(l).expect("loaded"));
    let search = |doc: &Document<()>, needle: &str| {
        let hidden = doc.hidden_lines(l).expect("loaded");
        find(
            &diff,
            Some(&hidden),
            None,
            &[],
            &Needle::new(needle),
            || false,
        )
        .expect("not cancelled")
    };

    let folded = search(&doc, "line_60");
    let rows: Vec<Pos> = folded
        .list
        .iter()
        .map(|m| doc.match_pos(l, m, unified).expect("shown"))
        .collect();
    assert_eq!(rows, [at(l, 4, 3), at(l, 4, 4)], "old line_60, new LINE_60");
    assert_eq!(folded.list[1].start..folded.list[1].end, 4..11);
    assert_eq!(
        search(&doc, "LINE_60").list.len(),
        1,
        "a capital matches case"
    );

    assert_eq!(search(&doc, "line_7").list.len(), 0, "only in gaps");
    assert!(doc.toggle_full(unified));
    let full = search(&doc, "line_7");
    assert_eq!(full.list.len(), 11, "line_7 and line_70 to line_79");
    assert_eq!(
        doc.match_pos(l, &full.list[0], unified),
        Some(at(l, 3, 0)),
        "the first line of the gap after the first hunk"
    );
}

#[test]
fn changes_and_files_lead_across_entries_and_say_how_to_settle() {
    let dir = fixture();
    let (_repo, mut doc) = document(dir.path());
    let (l, m, n) = (
        entry(&doc, "long.c"),
        entry(&doc, "moved.c"),
        entry(&doc, "notes.txt"),
    );
    assert_eq!((l, m, n), (0, 1, 2), "path order");
    let land = |doc: &mut Document<()>, change: Pos| {
        doc.pos = change;
        doc.landed(change);
    };
    let first = doc.open(l, unified).expect("a change");
    assert_eq!(first, at(l, 2, 2));
    land(&mut doc, first);
    assert_eq!(doc.next_change(unified), Some(Settle::Land(at(l, 4, 3))));
    land(&mut doc, at(l, 4, 3));
    assert_eq!(
        doc.next_change(unified),
        Some(Settle::Open {
            file: m,
            landing: true
        })
    );
    let first = doc.open(m, unified).expect("a change");
    assert_eq!(first, at(m, 2, 0));
    land(&mut doc, first);
    assert_eq!(
        doc.prev_change(unified),
        Some(Settle::Edge {
            file: l,
            last: true
        })
    );
    assert_eq!(doc.pos, at(l, 1, 0), "the file's top, until the host lands");
    assert_eq!(doc.edge(l, true, unified), Some(at(l, 4, 3)));

    assert_eq!(doc.goto_file(m), Some(Settle::Keep));
    assert_eq!(doc.pos, at(m, 2, 0), "where it was left");
    assert_eq!(
        doc.goto_file(n),
        Some(Settle::Open {
            file: n,
            landing: false
        })
    );
    assert_eq!(doc.open(n, unified), Some(at(n, 1, 0)));
    assert_eq!(doc.goto_file(n + 1), None, "past the last entry");
    assert_eq!(doc.goto_file(m), Some(Settle::Keep));
    assert_eq!(doc.pos.file, m);

    let last = end(&mut doc, n);
    assert_eq!(doc.missing(at(n, 1, 0), last).count(), 1);
    fill(&mut doc, at(n, 1, 0), last);
    assert_eq!(doc.missing(at(n, 1, 0), last).count(), 0);
}

#[test]
fn an_evicted_entry_wants_its_diff_again_and_keeps_its_place() {
    let dir = fixture();
    let (repo, mut doc) = document(dir.path());
    let (l, m) = (entry(&doc, "long.c"), entry(&doc, "moved.c"));
    let _ = doc.open(l, unified);
    assert!(doc.expand(l, 1, unified));
    let left = at(l, 4, 3);
    doc.pos = left;
    let lines = doc.row_lines(left, unified);
    doc.touch(l, 1);
    assert!(doc.goto_file(m).is_some());
    let _ = doc.open(m, unified);
    doc.touch(m, 2);

    let budget = doc.file(m).expect("loaded").bytes();
    doc.evict(budget, &(m..m + 1));
    assert!(doc.file(l).is_none());
    assert!(doc.files()[l].wants_diff());
    assert!(doc.file(m).is_some(), "the entry shown stays");

    assert_eq!(doc.goto_file(l), Some(Settle::Keep));
    assert_eq!(doc.pos, left);
    let file = prepared(&repo, &doc, l);
    assert_eq!(doc.load(l, Ok(file)), Some(Settle::Keep));
    assert_eq!(doc.pos, left);
    assert_eq!(doc.row_lines(left, unified), lines);
    assert_eq!(
        doc.shown(l, 1).map(|s| (s.top, s.bottom)),
        Some((10, 10)),
        "revealed lines outlive the diff"
    );
}

const PATCH: &str = r#"commit 80f116fc5dc99b9bc38a6e38344657fdf68f1cb4
Author: Ada Lovelace <ada@example.com>
Date:   Fri Mar 1 12:00:00 2024 +0100

    Widen calc to long

diff --git a/calc.c b/calc.c
index 6ff3ef8..f331ca1 100644
--- a/calc.c
+++ b/calc.c
@@ -9,6 +9,7 @@ static int add(int a, int b)
 int main(void)
 {
-    int total = add(1, 2);
-    printf("total: %d\n", total);
+    long total = add(1, 2);
+    printf("total: %ld\n", total);
+    printf("done\n");
     return 0;
 }
diff --git a/added.md b/added.md
new file mode 100644
index 0000000..133ab09
--- /dev/null
+++ b/added.md
@@ -0,0 +1 @@
+a brand new file
"#;

/// `PATCH` read the way a pager host reads a pipe: line by line through
/// the parser into the builder's items, each one an entry.
fn piped() -> Document<Vec<u8>> {
    let mut doc = Document::default();
    let mut parser = Parser::default();
    let mut builder = Builder::default();
    let mut push = |item| {
        match item {
            Item::Commit(commit) => {
                let change = FileChange {
                    path: commit.label().into(),
                    status: Status::Modified,
                    old: None,
                    new: None,
                    staging: None,
                };
                doc.push(change, Source::Commit(Arc::new(commit)));
            }
            Item::File(f) => {
                let file = Piped {
                    status: f.info.status,
                    binary: f.info.binary,
                    counts: document::Counts {
                        added: f.added,
                        removed: f.removed,
                    },
                    combined: f.combined,
                    patch: f.hunks,
                };
                doc.push(document::file_change(f.info), Source::Piped(Arc::new(file)));
            }
        }
        Ok::<(), ()>(())
    };
    for line in PATCH.as_bytes().split(|&b| b == b'\n') {
        parser
            .feed(line, &mut |event| builder.event(event, &mut push))
            .expect("fed");
    }
    parser
        .finish(&mut |event| builder.event(event, &mut push))
        .expect("finished");
    builder.finish(&mut push).expect("finished");
    for file in 0..doc.files().len() {
        let entry = &doc.files()[file];
        let Source::Piped(piped) = entry.source() else {
            continue;
        };
        let sides = patch::sides(&piped.patch).expect("sides");
        let loaded = File::piped(entry.change().clone(), sides, piped.binary);
        let _ = doc.load(file, Ok(Box::new(loaded)));
    }
    doc
}

#[test]
fn a_piped_patch_becomes_entries_whose_hunks_have_rows_but_no_gaps_to_reveal() {
    let mut doc = piped();
    let paths: Vec<_> = doc
        .files()
        .iter()
        .map(|e| e.change().path.clone())
        .collect();
    assert_eq!(paths[1..], ["calc.c", "added.md"]);
    assert_eq!(doc.commits(), [0]);
    assert_eq!(doc.commit_files(0), 1..3);
    assert_eq!(doc.files()[2].status(), FileStatus::Added);

    let calc = 1;
    assert_eq!(doc.segment(calc, 1), Some(Segment::Gap(0)));
    let Some(Content::Separator { hidden, next }) = doc.row(at(calc, 1, 0), unified) else {
        unreachable!("the separator of the lines before the hunk");
    };
    assert_eq!(hidden, 8);
    assert_eq!(
        next.map(|h| h.heading),
        Some(&b"static int add(int a, int b)"[..])
    );
    assert!(
        !doc.expand(calc, 0, unified),
        "a piped file has only its hunks"
    );

    let removed = [LineState::RemovedPaired, LineState::AddedPaired];
    let context = || vec![LineState::Context; 2];
    assert_eq!(
        rows(&mut doc, calc, 2, split),
        [
            (Row::Pair(Some(0), Some(0)), context()),
            (Row::Pair(Some(1), Some(1)), context()),
            (Row::Pair(Some(2), Some(4)), removed.to_vec()),
            (Row::Pair(Some(3), Some(5)), removed.to_vec()),
            (Row::Pair(None, Some(6)), vec![LineState::Added]),
            (Row::Pair(Some(7), Some(7)), context()),
            (Row::Pair(Some(8), Some(8)), context()),
        ]
    );
    assert_eq!(rows(&mut doc, calc, 2, unified).len(), 9);
    assert_eq!(
        rows(&mut doc, 2, 1, unified),
        [(Row::Line(0), vec![LineState::Added])]
    );
}

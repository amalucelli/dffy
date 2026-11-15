use std::fmt::Write as _;

use dffy_core::diff::{Options, Text, emphasize, hunks};
use dffy_core::git::{FileChange, FileDiff, Mode, Side, Status};

use super::*;
use crate::config::Indicators;
use crate::theme::Role;

const OLD: &str = "fn main() {\n    let total = count + 1;\n    println!(\"{total}\");\n}\n\
    // a comment long enough to wrap in a narrow split column, and then some more\n";
const NEW: &str = "fn main() {\n    let total = items.len() + 1;\n    println!(\"{total}\");\n    eprintln!(\"done\");\n}\n\
    // a comment long enough to wrap in a narrow split column, and then some more!";

fn file_diff(path: &str, old: &str, new: &str) -> FileDiff {
    let side = Some(Side::Worktree {
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

fn diff() -> FileDiff {
    file_diff("src/main.rs", OLD, NEW)
}

fn layout(split: bool, wrap: bool) -> Layout {
    Layout {
        split,
        width: Some(60),
        wrap,
        words: false,
        tab_width: 4,
        indicators: Indicators::Classic,
        one_sided: false,
        digits: 0,
        inline_signs: false,
        scroll: 0,
        line_numbers: true,
        hunk_headers: true,
        modified_lines: true,
    }
}

/// A document showing `fd` loaded, with emphasis.
fn doc_of(fd: FileDiff, layout: Layout, height: usize) -> Doc {
    let emphasis: Vec<_> = fd
        .hunks
        .iter()
        .map(|h| emphasize(&fd.old, &fd.new, h))
        .collect();
    let mut doc = Doc::new(layout, height);
    doc.push(fd.change.clone(), Source::Repo);
    load(&mut doc, 0, fd);
    let diff = Arc::clone(doc.diff_of(0).expect("loaded"));
    for (hunk, e) in emphasis.into_iter().enumerate() {
        doc.set_emphasis(0, &diff, hunk, e, None);
    }
    doc
}

fn doc(layout: Layout, height: usize) -> Doc {
    doc_of(diff(), layout, height)
}

/// Print mode's rows of the file, without its header and rule.
fn print_rows(layout: Layout) -> Vec<String> {
    let mut c = Canvas::default();
    print::file(&layout, &diff(), None, None, &mut c);
    c.rows()
        .skip(2)
        .map(|row| {
            row.map(|(_, text)| text)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn grid_rows(doc: &mut Doc, theme: &Theme) -> (Grid, Vec<String>) {
    let mut grid = Grid::new(60, doc.height);
    doc.draw(&mut grid, 0, 60, 0, theme, 1);
    let rows = (0..doc.height)
        .map(|y| grid.row_text(y).trim_end().to_owned())
        .collect();
    (grid, rows)
}

fn screen(doc: &mut Doc) -> String {
    grid_rows(doc, &Theme::default_theme()).1.join("\n")
}

/// The rows print mode writes for the file, less its two header rows: the
/// viewer names the file in its frame instead.
#[test]
fn screen_rows_are_print_mode_rows() {
    let theme = Theme::default_theme();
    for (split, wrap) in [(false, false), (true, false), (false, true), (true, true)] {
        let expected = print_rows(layout(split, wrap));
        let mut doc = doc(layout(split, wrap), expected.len());
        let (_, rows) = grid_rows(&mut doc, &theme);
        assert_eq!(rows, expected, "split {split} wrap {wrap}");
    }
}

#[test]
fn split_grid() {
    let mut doc = doc(layout(true, false), 7);
    // Classic signs sit in the gutter, left of the rule: right of it is
    // code only.
    insta::assert_snapshot!(screen(&mut doc), @r#"
    @@ -1,5 +1,6 @@
    1  │ fn main() {             │1  │ fn main() {
    2 -│     let total = count +…│2 +│     let total = items.le…
    3  │     println!("{total}");│3  │     println!("{total}");
       │                         │4 +│     eprintln!("done");
    4  │ }                       │5  │ }
    5 -│ // a comment long enoug…│6 +│ // a comment long enough…
    "#);
}

/// A hunk drawn split and unified keeps the rows of both: switching back
/// and forth draws what a document opened in either draws, the change
/// blocks `n` stops at follow, and the second set of rows counts toward
/// the cache once.
#[test]
fn rows_are_kept_for_each_arrangement() {
    let starts = |doc: &mut Doc| -> Vec<usize> {
        (1..doc.parts(0)).flat_map(|p| doc.starts(0, p)).collect()
    };
    let opened = |split: bool| {
        let mut doc = doc(layout(split, false), 7);
        (screen(&mut doc), starts(&mut doc))
    };
    let (split, unified) = (opened(true), opened(false));
    assert_ne!(split.1, unified.1, "the arrangements' rows differ");
    let mut doc = doc(layout(true, false), 7);
    assert_eq!((screen(&mut doc), starts(&mut doc)), split);
    let before = doc.loaded_bytes;
    let mut sizes = Vec::new();
    for on in [false, true, false, true] {
        doc.layout.split = on;
        doc.normalize();
        let expected = if on { &split } else { &unified };
        assert_eq!(
            (screen(&mut doc), starts(&mut doc)),
            *expected,
            "split {on}"
        );
        sizes.push(doc.loaded_bytes);
    }
    assert!(sizes[0] > before);
    assert!(sizes.iter().all(|&s| s == sizes[0]), "{sizes:?}");
    assert_eq!(doc.loaded(0).map(|l| l.bytes), Some(sizes[0]));
}

#[test]
fn cells_take_the_theme_style_of_their_role() {
    let theme = Theme::default_theme();
    let mut doc = doc(layout(false, false), 12);
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let y = rows
        .iter()
        .position(|r| r.contains("items.len()"))
        .expect("added line");
    let byte = rows[y].find("items").expect("emphasized word");
    let x = rows[y][..byte].chars().count();
    assert_eq!(grid.row(y)[x].style(), theme.style(Role::AddedEmph));
    // It's an edit of the line above: the rest of it is modified.
    assert_eq!(grid.row(y)[x - 2].style(), theme.style(Role::Modified));
    let y = rows
        .iter()
        .position(|r| r.contains("eprintln"))
        .expect("added line");
    let x = rows[y].find("eprintln").expect("text");
    assert_eq!(grid.row(y)[x].style(), theme.style(Role::Added));
}

/// `keep 2` and its neighbors: `int a` edited into `long a`, a line wholly
/// removed after it, and one wholly added further down.
fn edits() -> FileDiff {
    file_diff(
        "edits.c",
        "keep\nint a = 1;\nremoved line here\nkeep 2\n",
        "keep\nlong a = 1;\nkeep 2\nadded line\n",
    )
}

/// Each row of `grid`, then a row of letters for how each cell looks: `m`
/// the modified color, `a` and `r` added and removed, `A` and `R` their
/// emphasis, `.` anything else.
fn looks(grid: &Grid, rows: &[String], theme: &Theme) -> String {
    let looks = [
        (Role::Modified, 'm'),
        (Role::Added, 'a'),
        (Role::Removed, 'r'),
        (Role::AddedEmph, 'A'),
        (Role::RemovedEmph, 'R'),
    ];
    let mut out = String::new();
    for (y, row) in rows.iter().enumerate() {
        let marks: String = grid.row(y)[..row.chars().count()]
            .iter()
            .map(|cell| {
                looks
                    .iter()
                    .find(|(role, _)| theme.style(*role) == cell.style())
                    .map_or('.', |(_, c)| *c)
            })
            .collect();
        let _ = writeln!(out, "{row}\n{marks}");
    }
    out
}

/// A removed line and the added line it was edited into both show in the
/// modified color, with only their changed tokens red and green; lines
/// wholly removed or added stay red and green. Unified view keeps the
/// diff's order: a block's removed lines, then its added ones.
#[test]
fn edited_lines_show_as_modified_with_only_their_changed_tokens_colored() {
    let theme = Theme::default_theme();
    for (split, name) in [(true, "modified_split"), (false, "modified_unified")] {
        let mut doc = doc_of(edits(), layout(split, false), 8);
        let (grid, rows) = grid_rows(&mut doc, &theme);
        insta::assert_snapshot!(name, looks(&grid, &rows, &theme));
    }
}

/// `modified-lines = added-removed` draws edited lines as before: red and
/// green, like lines wholly removed or added.
#[test]
fn added_removed_keeps_edited_lines_red_and_green() {
    let theme = Theme::default_theme();
    let layout = Layout {
        modified_lines: false,
        ..layout(true, false)
    };
    let mut doc = doc_of(edits(), layout, 8);
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let looks = looks(&grid, &rows, &theme);
    let marks = looks.lines().skip(1).step_by(2);
    assert!(marks.clone().all(|l| !l.contains('m')), "{looks}");
    let edited = looks
        .lines()
        .skip_while(|l| !l.contains("int a = 1;"))
        .nth(1)
        .unwrap_or_default();
    assert!(
        edited.contains(".RRRrrr") && edited.contains(".AAAAaaa"),
        "{looks}"
    );
}

#[test]
fn scrolling_moves_one_screen_row_at_a_time_through_wrapped_rows() {
    let theme = Theme::default_theme();
    let expected = print_rows(layout(true, true));
    let height = 3;
    let mut doc = doc(layout(true, true), height);
    for top in 0..=expected.len() - height {
        let (_, rows) = grid_rows(&mut doc, &theme);
        assert_eq!(rows, expected[top..top + height], "scrolled {top}");
        doc.scroll(1);
    }
    // At the end, scrolling further keeps the screen full.
    let (_, rows) = grid_rows(&mut doc, &theme);
    assert_eq!(rows, expected[expected.len() - height..]);
    doc.scroll(-100);
    assert_eq!(
        doc.pos,
        Pos {
            part: 1,
            ..Pos::default()
        }
    );
}

#[test]
fn an_empty_document_ignores_moves_and_draws_nothing() {
    let mut doc = Doc::new(layout(false, false), 5);
    doc.scroll(3);
    doc.bottom();
    doc.next_change();
    doc.prev_change();
    doc.prev_file();
    doc.expand_target();
    doc.toggle_full();
    doc.normalize();
    assert_eq!(doc.line_at_top(), 1);
    let mut grid = Grid::new(60, 5);
    assert_eq!(
        doc.draw(&mut grid, 0, 60, 0, &Theme::default_theme(), 1),
        0..0
    );
    assert_eq!(doc.pos, Pos::default());
}

#[test]
fn change_and_file_jumps() {
    let mut doc = doc(layout(false, false), 3);
    doc.push(diff().change, Source::Repo);
    doc.next_change();
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("-│     let total"), "{rows:?}");
    doc.next_change();
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("+│     eprintln"), "{rows:?}");
    assert_eq!(doc.next_change(), None);
    // Past the last change, the next file opens; it isn't diffed yet, so
    // at its top until it is.
    assert_eq!(doc.next_change(), Some(1));
    assert_eq!((doc.pos.file, doc.pos.part, doc.pos.row), (1, 0, 0));
    // Back, the previous file opens at its last change.
    assert_eq!(doc.prev_change(), Some(0));
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("// a comment"), "{rows:?}");
    // Each file keeps its place.
    doc.next_file();
    assert_eq!(doc.pos.file, 1);
    doc.prev_file();
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("// a comment"), "{rows:?}");
    doc.top();
    assert_eq!((doc.pos.file, doc.pos.part), (0, 1), "the first hunk");
    assert_eq!(doc.line_at_top(), 1);
}

/// `N` into a file not diffed yet waits at its top, then lands on its
/// last change when the diff arrives, so the next `N` doesn't skip the
/// file's changes.
#[test]
fn shift_n_into_a_file_not_diffed_yet_lands_on_its_last_change_when_it_arrives() {
    let mut doc = Doc::new(layout(false, false), 3);
    doc.push(diff().change, Source::Repo);
    doc.push(file_diff("src/zz.rs", OLD, NEW).change, Source::Repo);
    let load = |doc: &mut Doc, file: usize| {
        let fd = file_diff(&doc.files[file].change.path.to_string(), OLD, NEW);
        load(doc, file, fd);
    };
    load(&mut doc, 1);
    doc.goto_file(1);
    assert_eq!(doc.next_change(), None);
    assert_eq!(doc.prev_change(), Some(0));
    assert_eq!((doc.pos.file, doc.pos.part, doc.pos.row), (0, 0, 0));
    load(&mut doc, 0);
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("-│ // a comment"), "{rows:?}");
    assert_eq!(doc.prev_change(), None);
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("+│     eprintln"), "{rows:?}");
}

#[test]
fn scrolling_stops_at_the_edges_of_the_file_shown() {
    let mut doc = doc(layout(false, false), 4);
    doc.push(diff().change, Source::Repo);
    doc.scroll(1000);
    assert_eq!(doc.pos.file, 0);
    doc.goto_file(1);
    doc.scroll(-1000);
    assert_eq!(
        doc.pos,
        Pos {
            file: 1,
            ..Pos::default()
        }
    );
    // A file without hunks yet shows its note in the middle of the pane,
    // and nothing else.
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert_eq!(rows, ["", "", "                          loading…", ""]);
}

/// 80 lines, changed at lines 3 and 60: a gap of 50 lines between the
/// hunks and one of 17 after the last.
fn long_diff() -> FileDiff {
    let old = (1..=80)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("line 3\n", "line three\n")
        .replace("line 60\n", "line sixty\n");
    file_diff("long.txt", &old, &new)
}

fn separators(doc: &mut Doc) -> Vec<String> {
    let (_, rows) = grid_rows(doc, &Theme::default_theme());
    rows.into_iter().filter(|r| r.starts_with('⋯')).collect()
}

#[test]
fn a_one_sided_file_is_one_full_width_pane_in_split_view() {
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let new = Text::new(
        b"fn added() {\n    let a_long_line_that_uses_the_whole_width = 1;\n}\n".to_vec(),
    );
    let fd = FileDiff {
        change: FileChange {
            path: "src/added.rs".into(),
            status: Status::Untracked,
            old: None,
            new: side,
            staging: None,
        },
        hunks: hunks(&Text::new(Vec::new()), &new, &Options::default()),
        old: Text::new(Vec::new()),
        new,
        binary: false,
        too_large: false,
        note: None,
    };
    let mut doc = doc_of(fd, layout(true, false), 6);
    insta::assert_snapshot!("one_sided_split", screen(&mut doc));
}

#[test]
fn gaps_show_as_separators_and_open_twenty_lines_then_whole() {
    let mut doc = doc_of(long_diff(), layout(false, false), 40);
    insta::assert_snapshot!("separators", separators(&mut doc).join("\n"));
    // The first separator on screen is what `x` opens: ten lines on each
    // side of it.
    assert!(doc.expand_target());
    insta::assert_snapshot!("separators_after_x", separators(&mut doc).join("\n"));
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows.iter().any(|r| r == "16 16  │ line 16"), "{rows:#?}");
    assert!(!rows.iter().any(|r| r == "17 17  │ line 17"), "{rows:#?}");
    assert!(!rows.iter().any(|r| r == "46 46  │ line 46"), "{rows:#?}");
    assert!(rows.iter().any(|r| r == "47 47  │ line 47"), "{rows:#?}");
    // Thirty hidden lines are few enough to open at once.
    assert!(doc.expand_target());
    doc.bottom();
    insta::assert_snapshot!("separators_after_x_twice", separators(&mut doc).join("\n"));
}

#[test]
fn clicking_a_separator_opens_it() {
    let mut doc = doc_of(long_diff(), layout(false, false), 40);
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    let y = rows
        .iter()
        .rposition(|r| r.starts_with('⋯'))
        .expect("separator");
    assert!(rows[y].starts_with("⋯ 17"), "{rows:#?}");
    // Seventeen lines open at once.
    doc.click(y);
    assert_eq!(separators(&mut doc).len(), 1);
}

#[test]
fn the_separator_x_opens_is_highlighted() {
    let theme = Theme::default_theme();
    let mut doc = doc_of(long_diff(), layout(false, false), 8);
    doc.scroll(3);
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let marked: Vec<String> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.starts_with('⋯'))
        .map(|(y, r)| {
            let target = grid.row(y)[0].style() == theme.style(Role::Hint);
            format!("{}{r}", if target { "x> " } else { "   " })
        })
        .collect();
    insta::assert_snapshot!("target", marked.join("\n"));
    // Scrolled past it, the next one is the target.
    doc.scroll(20);
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let y = rows
        .iter()
        .position(|r| r.starts_with('⋯'))
        .expect("separator");
    assert!(rows[y].starts_with("⋯ 17"), "{rows:#?}");
    assert_eq!(grid.row(y)[0].style(), theme.style(Role::Hint));
}

#[test]
fn full_text_shows_every_line_in_both_layouts_and_toggles_back() {
    for split in [false, true] {
        let mut doc = doc_of(long_diff(), layout(split, false), 12);
        doc.toggle_full();
        assert_eq!(separators(&mut doc), Vec::<String>::new());
        let rows = screen(&mut doc);
        assert!(rows.contains("line 1") && rows.contains("line 2"), "{rows}");
        // `n` lands exactly on the changed lines.
        doc.next_change();
        doc.next_change();
        let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
        assert!(rows[0].contains("line 60"), "{rows:#?}");
        if split {
            insta::assert_snapshot!("full_split", rows[..4].join("\n"));
        } else {
            insta::assert_snapshot!("full_unified", rows[..4].join("\n"));
        }
        doc.toggle_full();
        assert!(!separators(&mut doc).is_empty(), "{split}");
    }
}

/// The top row after `step`, as `step: row`.
fn top_after(doc: &mut Doc, step: &str) -> String {
    let (_, rows) = grid_rows(doc, &Theme::default_theme());
    format!("{step}: {}", rows[0])
}

/// With a hunk's line at the top, `X` there and back and `x` leave it
/// there, `x` revealing lines above it as well as below.
#[test]
fn revealing_lines_keeps_a_hunks_line_at_the_top() {
    let mut doc = doc_of(long_diff(), layout(false, false), 4);
    doc.scroll(11);
    let mut tops = vec![top_after(&mut doc, "start")];
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X on"));
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X off"));
    assert!(
        separators(&mut doc).is_empty(),
        "x opens the gaps around the hunk"
    );
    assert!(doc.expand_target());
    tops.push(top_after(&mut doc, "x"));
    doc.scroll(-4);
    tops.push(top_after(&mut doc, "up 4"));
    insta::assert_snapshot!("hunk_line_kept", tops.join("\n"));
}

/// With a line of a gap at the top, more of the gap revealed above it
/// (`x`, or `X` on) keeps it there, and `X` off brings it back to where it
/// was; `X` off where the line is hidden again puts its separator at the
/// top.
#[test]
fn revealing_lines_keeps_a_gap_line_at_the_top() {
    let to_line = |doc: &mut Doc, number: &str| {
        for _ in 0..100 {
            if top_after(doc, "").starts_with(&format!(": {number} {number} ")) {
                return;
            }
            doc.scroll(1);
        }
        panic!("line {number} never reached the top");
    };
    // Line 50 is below the separator that one `x` leaves in the gap.
    let opened = || {
        let mut doc = doc_of(long_diff(), layout(false, false), 8);
        screen(&mut doc);
        assert!(doc.expand_target());
        to_line(&mut doc, "50");
        doc
    };
    let mut doc = opened();
    let mut tops = vec![top_after(&mut doc, "x")];
    let row = doc.pos.row;
    assert!(doc.expand_target());
    tops.push(top_after(&mut doc, "x again"));
    assert_ne!(doc.pos.row, row, "the line moved down the gap's rows");

    let mut doc = opened();
    tops.push(top_after(&mut doc, "x"));
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X on"));
    assert_ne!(doc.pos.row, row, "the line moved down the gap's rows");
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X off"));
    assert_eq!(doc.pos.row, row);

    let mut doc = doc_of(long_diff(), layout(false, false), 8);
    doc.toggle_full();
    to_line(&mut doc, "30");
    tops.push(top_after(&mut doc, "X on"));
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X off"));
    insta::assert_snapshot!("gap_line_kept", tops.join("\n"));
}

/// 500 lines, changed at lines 400 and 450.
fn late_diff() -> FileDiff {
    let old = (1..=500)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("line 400\n", "line four hundred\n")
        .replace("line 450\n", "line 450!\n");
    file_diff("late.txt", &old, &new)
}

fn load(doc: &mut Doc, file: usize, fd: FileDiff) {
    assert!(doc.load(
        file,
        Ok(Box::new(Loaded::new(fd, Whitespace::Exact, false)))
    ));
}

/// The first time a file shows, in its full text, the view starts a few
/// rows above its first change, whether the diff came before the file was
/// opened or after. Back on it later, the view is where it was left.
#[test]
fn a_file_opens_at_its_first_change_and_keeps_its_place_after() {
    for loaded_first in [false, true] {
        let mut doc = Doc::new(layout(false, false), 20);
        doc.full = true;
        doc.push(diff().change, Source::Repo);
        doc.push(late_diff().change, Source::Repo);
        load(&mut doc, 0, diff());
        if loaded_first {
            load(&mut doc, 1, late_diff());
            doc.goto_file(1);
        } else {
            doc.goto_file(1);
            assert_eq!(doc.line_at_top(), 1, "at the top until it's diffed");
            load(&mut doc, 1, late_diff());
        }
        let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
        assert!(rows[0].starts_with("397 397"), "{rows:#?}");
        assert!(rows[3].contains("-│ line 400"), "{rows:#?}");
        // `n` goes on to the next change, not the one in view.
        assert_eq!(doc.next_change(), None);
        assert_eq!(doc.line_at_top(), 450);
        doc.scroll(30);
        let left = doc.pos;
        doc.prev_file();
        doc.next_file();
        assert_eq!(doc.pos, left, "restored");
        // A reload (a refresh, the cache cap) doesn't move it either.
        doc.files[1].stale = true;
        load(&mut doc, 1, late_diff());
        assert_eq!(doc.pos, left, "not moved by a reload");
        doc.top();
        assert_eq!(doc.line_at_top(), 1, "g still goes to the top");
    }
}

/// A change on the first screen leaves the file at its top: changes-only
/// files, short ones and new files open as they always did.
#[test]
fn a_change_on_the_first_screen_keeps_the_top() {
    let new = (1..=60)
        .map(|i| format!("new {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = Text::new(new.into());
    let added = FileDiff {
        change: FileChange {
            path: "src/new.rs".into(),
            status: Status::Untracked,
            old: None,
            new: Some(Side::Worktree {
                mode: Mode::File,
                id: None,
            }),
            staging: None,
        },
        hunks: hunks(&Text::new(Vec::new()), &new, &Options::default()),
        old: Text::new(Vec::new()),
        new,
        binary: false,
        too_large: false,
        note: None,
    };
    for (fd, full) in [(late_diff(), false), (added, true), (long_diff(), true)] {
        let mut doc = Doc::new(layout(false, false), 20);
        doc.full = full;
        doc.push(fd.change.clone(), Source::Repo);
        load(&mut doc, 0, fd);
        let top = Pos {
            part: doc.first_part(0),
            ..Pos::default()
        };
        assert_eq!(doc.pos, top, "full {full}");
    }
}

#[test]
fn the_overview_marks_changes_and_the_lines_on_screen() {
    let mut doc = doc_of(long_diff(), layout(false, false), 20);
    doc.overview = true;
    doc.toggle_full();
    let mut grid = Grid::new(60, 20);
    doc.draw(&mut grid, 0, 60, 0, &Theme::default_theme(), 1);
    let column: String = (0..20).map(|y| grid.row(y)[59].text().to_owned()).collect();
    insta::assert_snapshot!("overview", column);
    // A click on the mark near the bottom brings that change to the top.
    let y = column
        .chars()
        .collect::<Vec<_>>()
        .iter()
        .rposition(|&c| c == '▐')
        .expect("a mark");
    doc.overview_click(y);
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("line 60"), "{rows:#?}");
}

/// Rows with each run of cells sharing a background bracketed by it.
fn backgrounds(grid: &Grid, rows: Range<usize>) -> String {
    let mut out = String::new();
    for y in rows {
        let mut current = None;
        for cell in grid.row(y) {
            let bg = cell.style().bg;
            if bg != current {
                if current.is_some() {
                    out.push(']');
                }
                if let Some(crate::theme::Color::Rgb(r, g, b)) = bg {
                    let _ = write!(out, "[{r:02x}{g:02x}{b:02x} ");
                }
                current = bg;
            }
            out.push_str(cell.text());
        }
        if current.is_some() {
            out.push(']');
        }
        out.push('\n');
    }
    out
}

#[test]
fn background_tints_fill_changed_lines_with_a_stronger_gutter() {
    let mut layout = layout(false, false);
    layout.indicators = Indicators::Background;
    for (bg, name) in [
        (crate::theme::DARK_BACKGROUND, "tints_dark"),
        (crate::theme::LIGHT_BACKGROUND, "tints_light"),
    ] {
        let mut theme = Theme::default_theme();
        theme.tint(bg);
        let mut doc = doc(layout, 9);
        let mut grid = Grid::new(60, 9);
        doc.draw(&mut grid, 0, 60, 0, &theme, 1);
        insta::assert_snapshot!(name, backgrounds(&grid, 1..7));
    }
}

#[test]
fn background_tints_give_moved_code_its_own_hue() {
    let old = "int a(void) { return 1; }\nint b(void) { return 2; }\n";
    let new = "int b(void) { return 2; }\nint a(void) { return 1; }\n";
    let fd = file_diff("f.c", old, new);
    let structural = dffy_core::structural::diff(b"f.c", &fd.old, &fd.new, &fd.hunks);
    let mut layout = layout(false, false);
    layout.indicators = Indicators::Background;
    let mut theme = Theme::default_theme();
    theme.tint(crate::theme::DARK_BACKGROUND);
    let mut doc = doc_of(fd, layout, 6);
    let State::Loaded(loaded) = &doc.files[0].state else {
        panic!("loaded");
    };
    let diff = Arc::clone(&loaded.diff);
    assert!(doc.set_structural(0, &diff, Some(structural)));
    doc.structural = true;
    let mut grid = Grid::new(60, 6);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    insta::assert_snapshot!("tints_moved", backgrounds(&grid, 1..4));
}

/// Rows with each run of cells whose style has `attribute` bracketed,
/// against a theme where nothing has it by itself.
fn marked(grid: &Grid, attribute: Attributes) -> String {
    let mut out = String::new();
    for y in 0..grid.height() {
        let mut on = false;
        let mut row = String::new();
        for cell in grid.row(y) {
            let now = cell.style().attributes.contains(attribute);
            if now != on {
                row.push(if now { '[' } else { ']' });
                on = now;
            }
            row.push_str(cell.text());
        }
        if on {
            row.push(']');
        }
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// The default theme without reverse video, so only the selection has it.
fn plain_theme() -> Theme {
    Theme::parse(
        "plain",
        "added-emph = green bold\nremoved-emph = red bold\n",
        &mut Vec::new(),
    )
}

fn copied(doc: &mut Doc) -> String {
    String::from_utf8(doc.selected(false).expect("a selection").text).expect("utf-8")
}

/// `v` starts on the first line of code on screen, on its new side; `j`
/// extends it a line of that side at a time; `s` moves it to the other
/// side. What's copied is that side's text as the file has it.
#[test]
fn a_selection_shows_reversed_on_its_side_and_copies_that_sides_code() {
    let theme = plain_theme();
    let mut doc = doc(layout(true, false), 7);
    let mut grid = Grid::new(60, 7);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    assert!(doc.select(None));
    doc.select_by(2);
    let mut grid = Grid::new(60, 7);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    insta::assert_snapshot!(marked(&grid, Attributes::REVERSE), @r#"
    @@ -1,5 +1,6 @@
    1  │ fn main() {             │[1  │ fn main() {              ]
    2 -│     let total = count +…│[2 +│     let total = items.le…]
    3  │     println!("{total}");│[3  │     println!("{total}"); ]
       │                         │4 +│     eprintln!("done");
    4  │ }                       │5  │ }
    5 -│ // a comment long enoug…│6 +│ // a comment long enough…
    "#);
    assert_eq!(
        copied(&mut doc),
        "fn main() {\n    let total = items.len() + 1;\n    println!(\"{total}\");"
    );
    // With decorations, each line's number and sign, as a review quotes
    // them.
    let decorated = doc.selected(true).expect("a selection");
    assert_eq!(
        String::from_utf8_lossy(&decorated.text),
        "1  fn main() {\n2 +    let total = items.len() + 1;\n3      println!(\"{total}\");"
    );
    assert!(decorated.decorated);
    assert!(doc.select_other_side());
    let mut grid = Grid::new(60, 7);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    insta::assert_snapshot!(marked(&grid, Attributes::REVERSE), @r#"
    @@ -1,5 +1,6 @@
    [1  │ fn main() {             ]│1  │ fn main() {
    [2 -│     let total = count +…]│2 +│     let total = items.le…
    [3  │     println!("{total}");]│3  │     println!("{total}");
       │                         │4 +│     eprintln!("done");
    4  │ }                       │5  │ }
    5 -│ // a comment long enoug…│6 +│ // a comment long enough…
    "#);
    assert_eq!(
        copied(&mut doc),
        "fn main() {\n    let total = count + 1;\n    println!(\"{total}\");"
    );
}

/// In the unified view a selection of the new side passes over removed
/// lines; lines a separator hides between its ends are copied and
/// counted.
#[test]
fn a_unified_selection_skips_the_other_sides_lines_and_counts_hidden_ones() {
    let theme = plain_theme();
    let mut doc = doc_of(long_diff(), layout(false, false), 40);
    let mut grid = Grid::new(60, 40);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    assert!(doc.select(None));
    // Lines 1 and 2, then "line three": the removed "line 3" is passed over.
    doc.select_by(2);
    let mut grid = Grid::new(60, 40);
    doc.draw(&mut grid, 0, 60, 0, &theme, 1);
    let rows: Vec<String> = marked(&grid, Attributes::REVERSE)
        .lines()
        .take(6)
        .map(str::to_owned)
        .collect();
    insta::assert_snapshot!(rows.join("\n"), @r"
    @@ -1,6 +1,6 @@
    [ 1  1  │ line 1                                             ]
    [ 2  2  │ line 2                                             ]
     3    -│ line 3
    [    3 +│ line three                                         ]
     4  4  │ line 4
    ");
    assert_eq!(copied(&mut doc), "line 1\nline 2\nline three");
    // On past the separator: the lines it hides are in the copy.
    doc.select_by(5);
    let copy = doc.selected(false).expect("a selection");
    assert_eq!((copy.lines, copy.hidden), (58, 50));
    assert!(String::from_utf8_lossy(&copy.text).contains("line 30\n"));
}

/// The title's context follows the top row: the hunk it's in, the hunk
/// above at a separator, and in revealed unchanged lines the heading
/// above them. Nothing takes a row of the pane for it.
#[test]
fn the_context_is_the_hunk_or_heading_the_top_row_is_in() {
    let mut doc = doc_of(long_diff(), layout(false, false), 8);
    let numbers = |doc: &mut Doc| doc.context().and_then(|c| c.numbers);
    assert_eq!(numbers(&mut doc), Some("@@ -1,6 +1,6 @@".to_owned()));
    assert!(screen(&mut doc).starts_with("@@ -1,6 +1,6 @@"));
    doc.scroll(5);
    assert!(
        screen(&mut doc).starts_with(" 4  4  │ line 4"),
        "{}",
        screen(&mut doc)
    );
    assert_eq!(numbers(&mut doc), Some("@@ -1,6 +1,6 @@".to_owned()));
    doc.scroll(3);
    // The separator before the second hunk: still the first.
    assert!(screen(&mut doc).starts_with("⋯ 50"), "{}", screen(&mut doc));
    assert_eq!(numbers(&mut doc), Some("@@ -1,6 +1,6 @@".to_owned()));
    doc.scroll(1);
    assert_eq!(
        doc.context(),
        Some(Context {
            numbers: Some("@@ -57,7 +57,7 @@".to_owned()),
            heading: "line 56".to_owned(),
        })
    );
    // Revealed unchanged lines: git's kind of heading, the nearest line
    // above that starts with a letter.
    doc.toggle_full();
    doc.top();
    doc.scroll(20);
    assert!(
        screen(&mut doc).starts_with("19 19  │ line 19"),
        "{}",
        screen(&mut doc)
    );
    assert_eq!(
        doc.context(),
        Some(Context {
            numbers: None,
            heading: "line 18".to_owned(),
        })
    );
}

/// Every `j` and `k` moves the code by exactly one row, in a file's
/// changes and in its full text, and back up lands on the first row.
#[test]
fn each_scroll_step_moves_the_code_one_row() {
    const HEIGHT: usize = 8;
    let theme = plain_theme();
    let frame = |doc: &mut Doc| {
        let mut grid = Grid::new(60, HEIGHT);
        doc.draw(&mut grid, 0, 60, 0, &theme, 1);
        let rows: Vec<String> = (0..HEIGHT).map(|y| grid.row_text(y)).collect();
        (rows, doc.pos)
    };
    for full in [false, true] {
        let mut doc = doc_of(long_diff(), layout(false, false), HEIGHT);
        if full {
            doc.toggle_full();
        }
        let top = doc.pos;
        let mut moves = 0;
        for delta in [1, -1] {
            let mut before = frame(&mut doc);
            for press in 0..100 {
                doc.scroll(delta);
                let after = frame(&mut doc);
                if after.1 == before.1 {
                    break;
                }
                moves += 1;
                for y in 0..HEIGHT {
                    let Some(from) = y.checked_add_signed(delta).filter(|&f| f < HEIGHT) else {
                        continue;
                    };
                    assert_eq!(
                        after.0[y], before.0[from],
                        "press {press} by {delta}, row {y}, full {full}"
                    );
                }
                before = after;
            }
        }
        assert!(
            moves > if full { 100 } else { 6 },
            "{moves} moves, full {full}"
        );
        assert_eq!(doc.pos, top, "back at the top, full {full}");
    }
}

/// Scrolled sideways, both halves of a split row lose the same columns
/// and say so with `…`; the numbers and signs stay.
#[test]
fn horizontal_scroll_moves_both_sides_and_keeps_the_gutter() {
    let old = "\tshort\n\tlet value = compute_something(first_argument, second_argument);\n";
    let new = "\tshort\n\tlet value = compute_something(first_argument, other_argument);\n";
    let mut layout = layout(true, false);
    layout.scroll = 16;
    let mut doc = doc_of(file_diff("wide.rs", old, new), layout, 4);
    // The tab and `let value = ` are the sixteen columns gone; `short`
    // went whole.
    insta::assert_snapshot!(screen(&mut doc), @r"
    @@ -1,2 +1,2 @@
    1  │ …                       │1  │ …
    2 -│ …compute_something(firs…│2 +│ …compute_something(first…
    ");
}

/// A refresh that drops the file shown puts the one in its place on
/// screen, opened as a file is the first time; reloading it later doesn't
/// move the view.
#[test]
fn the_file_shown_in_place_of_one_gone_keeps_its_place_on_reload() {
    use std::ops::ControlFlow;
    use std::process::Command;

    use dffy_core::git::{ChangeSet, Repo, Revisions};

    let dir = tempfile::tempdir().expect("tempdir");
    let d = dir.path();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(d)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "A")
            .env("GIT_AUTHOR_EMAIL", "a@example.com")
            .env("GIT_COMMITTER_NAME", "A")
            .env("GIT_COMMITTER_EMAIL", "a@example.com")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}");
    };
    let late = late_diff();
    let (old, new) = (late.old.bytes().to_vec(), late.new.bytes().to_vec());
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(d.join("a.txt"), "one\n").expect("write");
    std::fs::write(d.join("z.txt"), &old).expect("write");
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "x"]);
    std::fs::write(d.join("a.txt"), "two\n").expect("write");
    std::fs::write(d.join("z.txt"), &new).expect("write");

    let repo = Repo::discover(d).expect("repo");
    let spec = repo.resolve(&[], false, false).expect("spec");
    let mut set = ChangeSet::new(repo.clone(), spec, Revisions::default(), &[], false);
    let mut doc = Doc::new(layout(false, false), 20);
    doc.full = true;
    set.enumerate(|change| {
        doc.push(change, Source::Repo);
        ControlFlow::Continue(())
    })
    .expect("enumerate");
    let diff = |file: usize, doc: &Doc| {
        let change = doc.files[file].change.clone();
        repo.file_diff(change, &Options::default()).expect("diff")
    };
    for file in [0, 1] {
        let fd = diff(file, &doc);
        load(&mut doc, file, fd);
    }
    assert_eq!(doc.pos.file, 0);

    std::fs::write(d.join("a.txt"), "one\n").expect("write");
    let update = set
        .refresh(&["a.txt".into()])
        .expect("refresh")
        .expect("changed");
    doc.replace(&update);
    assert_eq!(doc.files[doc.pos.file].change.path, "z.txt");
    assert_eq!(doc.line_at_top(), 397, "opened at its first change");
    doc.scroll(20);
    let left = doc.pos;
    doc.files[0].stale = true;
    let fd = diff(0, &doc);
    load(&mut doc, 0, fd);
    assert_eq!(doc.pos, left);
}

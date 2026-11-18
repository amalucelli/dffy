use std::fmt::Write as _;

use dffy_core::diff::{Options, Text, Whitespace, emphasize, hunks};
use dffy_core::document::State;
use dffy_core::git::{FileChange, FileDiff, Mode, Side, Status};

use super::*;
use crate::config::Indicators;
use crate::print;
use crate::review;
use crate::theme::Role;
use crate::tui::textbox::TextBox;

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
        unmerged: false,
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
    doc.document.push(fd.change.clone(), Source::Repo);
    load(&mut doc, 0, fd);
    let diff = Arc::clone(doc.document.diff_of(0).expect("loaded"));
    for (hunk, e) in emphasis.into_iter().enumerate() {
        doc.document.set_emphasis(0, &diff, hunk, e, None);
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
    doc.draw(&mut grid, None, (0, 60, 0), theme, 1);
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
    let starts = |doc: &mut Doc| -> Vec<Pos> {
        let (layout, shown) = (doc.file_layout(0), doc.document.pos);
        doc.document.top();
        let mut starts = Vec::new();
        while let Some(Settle::Land(at)) = doc.document.next_change(arrange(&layout)) {
            doc.document.pos = at;
            doc.document.landed(at);
            starts.push(at);
        }
        doc.document.pos = shown;
        starts
    };
    let opened = |split: bool| {
        let mut doc = doc(layout(split, false), 7);
        (screen(&mut doc), starts(&mut doc))
    };
    let (split, unified) = (opened(true), opened(false));
    assert_ne!(split.1, unified.1, "the arrangements' rows differ");
    let mut doc = doc(layout(true, false), 7);
    assert_eq!((screen(&mut doc), starts(&mut doc)), split);
    let before = doc.document.loaded_bytes();
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
        sizes.push(doc.document.loaded_bytes());
    }
    assert!(sizes[0] > before);
    assert!(sizes.iter().all(|&s| s == sizes[0]), "{sizes:?}");
    assert_eq!(doc.document.file(0).map(File::bytes), Some(sizes[0]));
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
        doc.document.pos,
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
        doc.draw(&mut grid, None, (0, 60, 0), &Theme::default_theme(), 1),
        (0..0, None)
    );
    assert_eq!(doc.document.pos, Pos::default());
}

#[test]
fn change_and_file_jumps() {
    let mut doc = doc(layout(false, false), 3);
    doc.document.push(diff().change, Source::Repo);
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
    assert_eq!(
        (
            doc.document.pos.file,
            doc.document.pos.part,
            doc.document.pos.row
        ),
        (1, 0, 0)
    );
    // Back, the previous file opens at its last change.
    assert_eq!(doc.prev_change(), Some(0));
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("// a comment"), "{rows:?}");
    // Each file keeps its place.
    doc.next_file();
    assert_eq!(doc.document.pos.file, 1);
    doc.prev_file();
    let (_, rows) = grid_rows(&mut doc, &Theme::default_theme());
    assert!(rows[0].contains("// a comment"), "{rows:?}");
    doc.document.top();
    assert_eq!(
        (doc.document.pos.file, doc.document.pos.part),
        (0, 1),
        "the first hunk"
    );
    assert_eq!(doc.line_at_top(), 1);
}

/// `N` into a file not diffed yet waits at its top, then lands on its
/// last change when the diff arrives, so the next `N` doesn't skip the
/// file's changes.
#[test]
fn shift_n_into_a_file_not_diffed_yet_lands_on_its_last_change_when_it_arrives() {
    let mut doc = Doc::new(layout(false, false), 3);
    doc.document.push(diff().change, Source::Repo);
    doc.document
        .push(file_diff("src/zz.rs", OLD, NEW).change, Source::Repo);
    let load = |doc: &mut Doc, file: usize| {
        let fd = file_diff(
            &doc.document.files()[file].change().path.to_string(),
            OLD,
            NEW,
        );
        load(doc, file, fd);
    };
    load(&mut doc, 1);
    doc.goto_file(1);
    assert_eq!(doc.next_change(), None);
    assert_eq!(doc.prev_change(), Some(0));
    assert_eq!(
        (
            doc.document.pos.file,
            doc.document.pos.part,
            doc.document.pos.row
        ),
        (0, 0, 0)
    );
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
    doc.document.push(diff().change, Source::Repo);
    doc.scroll(1000);
    assert_eq!(doc.document.pos.file, 0);
    doc.goto_file(1);
    doc.scroll(-1000);
    assert_eq!(
        doc.document.pos,
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
    let row = doc.document.pos.row;
    assert!(doc.expand_target());
    tops.push(top_after(&mut doc, "x again"));
    assert_ne!(
        doc.document.pos.row, row,
        "the line moved down the gap's rows"
    );

    let mut doc = opened();
    tops.push(top_after(&mut doc, "x"));
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X on"));
    assert_ne!(
        doc.document.pos.row, row,
        "the line moved down the gap's rows"
    );
    doc.toggle_full();
    tops.push(top_after(&mut doc, "X off"));
    assert_eq!(doc.document.pos.row, row);

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
    let loaded = File::prepare(crate::grammars(), None).file(fd, Whitespace::Exact, false);
    assert!(doc.load(file, Ok(Box::new(loaded))));
}

/// The first time a file shows, in its full text, the view starts a few
/// rows above its first change, whether the diff came before the file was
/// opened or after. Back on it later, the view is where it was left.
#[test]
fn a_file_opens_at_its_first_change_and_keeps_its_place_after() {
    for loaded_first in [false, true] {
        let mut doc = Doc::new(layout(false, false), 20);
        doc.document.full = true;
        doc.document.push(diff().change, Source::Repo);
        doc.document.push(late_diff().change, Source::Repo);
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
        let left = doc.document.pos;
        doc.prev_file();
        doc.next_file();
        assert_eq!(doc.document.pos, left, "restored");
        // A reload (a refresh, the cache cap) doesn't move it either.
        doc.document.restale();
        load(&mut doc, 1, late_diff());
        assert_eq!(doc.document.pos, left, "not moved by a reload");
        doc.document.top();
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
        doc.document.full = full;
        doc.document.push(fd.change.clone(), Source::Repo);
        load(&mut doc, 0, fd);
        let top = Pos {
            part: doc.document.first_part(0),
            ..Pos::default()
        };
        assert_eq!(doc.document.pos, top, "full {full}");
    }
}

#[test]
fn the_overview_marks_changes_and_the_lines_on_screen() {
    let mut doc = doc_of(long_diff(), layout(false, false), 20);
    doc.overview = true;
    doc.toggle_full();
    let mut grid = Grid::new(60, 20);
    doc.draw(&mut grid, None, (0, 60, 0), &Theme::default_theme(), 1);
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
                match bg {
                    Some(crate::theme::Color::Rgb(r, g, b)) => {
                        let _ = write!(out, "[{r:02x}{g:02x}{b:02x} ");
                    }
                    Some(crate::theme::Color::Ansi(n)) => {
                        let _ = write!(out, "[ansi{n} ");
                    }
                    None => {}
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
        doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
        insta::assert_snapshot!(name, backgrounds(&grid, 1..7));
    }
}

/// A thread's mark sits in the cell after the line numbers, on its side's
/// half of a split row, and keeps the tint of a changed line's gutter.
#[test]
fn comment_marks_go_after_the_line_numbers_of_their_side() {
    let marker = |side, line: usize, look| Thread {
        root: format!("{side:?}"),
        at: At::Line {
            side,
            lines: line..line + 1,
        },
        look,
        resolved: false,
        outdated: false,
        url: None,
        notes: vec![note("x", "", "11:00", "Hm", true)],
        lines: Vec::new(),
        snippet: None,
        local: false,
    };
    let markers: Arc<[_]> = Arc::new([
        marker(document::Side::New, 1, Look::Draft),
        marker(document::Side::Old, 0, Look::Resolved),
    ]);
    let mut split = doc(layout(true, false), 6);
    split.set_threads(0, Arc::clone(&markers));
    split.fold.toggle_all();
    let (_, rows) = grid_rows(&mut split, &Theme::default_theme());
    insta::assert_snapshot!(rows.join("\n"), @r#"
    @@ -1,5 +1,6 @@
    1○ │ fn main() {             │1  │ fn main() {
       │  │ ▸ you · just now … │ │
    2 -│     let total = count +…│2◆+│     let total = items.le…
                                 │   │  │ ▸ you · just now ·… │
    3  │     println!("{total}");│3  │     println!("{total}");
    "#);

    let mut layout = layout(false, false);
    layout.indicators = Indicators::Background;
    let mut theme = Theme::default_theme();
    theme.tint(crate::theme::DARK_BACKGROUND);
    let mut unified = doc(layout, 7);
    unified.set_threads(0, markers);
    unified.fold.toggle_all();
    let mut grid = Grid::new(60, 7);
    unified.draw(&mut grid, None, (0, 60, 0), &theme, 1);
    let tints = backgrounds(&grid, 1..6);
    let tints: Vec<&str> = tints.lines().map(str::trim_end).collect();
    insta::assert_snapshot!(tints.join("\n"), @"
     1 1○│ fn main() {
         │  │[ansi8  ▸ you · just now · draft · Hm                   ]│
    [2c385b  2   │][262d49      let total = ][603740 count][262d49  + 1;                           ]
    [2c385b    2◆│][262d49      let total = ][2d4d3d items.len()][262d49  + 1;                     ]
         │  │[ansi8  ▸ you · just now · draft · Hm                   ]│
    ");
}

#[test]
fn background_tints_give_moved_code_its_own_hue() {
    let old = "int a(void) { return 1; }\nint b(void) { return 2; }\n";
    let new = "int b(void) { return 2; }\nint a(void) { return 1; }\n";
    let fd = file_diff("f.c", old, new);
    let structural =
        dffy_core::structural::diff(crate::grammars(), b"f.c", &fd.old, &fd.new, &fd.hunks);
    let mut layout = layout(false, false);
    layout.indicators = Indicators::Background;
    let mut theme = Theme::default_theme();
    theme.tint(crate::theme::DARK_BACKGROUND);
    let mut doc = doc_of(fd, layout, 6);
    let State::Loaded(loaded) = doc.document.files()[0].state() else {
        panic!("loaded");
    };
    let diff = Arc::clone(loaded.diff());
    assert!(doc.document.set_structural(0, &diff, Some(structural)));
    doc.document.structural = true;
    let mut grid = Grid::new(60, 6);
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
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
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
    assert!(doc.select(None));
    doc.select_by(2);
    let mut grid = Grid::new(60, 7);
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
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
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
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
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
    assert!(doc.select(None));
    // Lines 1 and 2, then "line three": the removed "line 3" is passed over.
    doc.select_by(2);
    let mut grid = Grid::new(60, 40);
    doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
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
    doc.document.top();
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
        doc.draw(&mut grid, None, (0, 60, 0), &theme, 1);
        let rows: Vec<String> = (0..HEIGHT).map(|y| grid.row_text(y)).collect();
        (rows, doc.document.pos)
    };
    for full in [false, true] {
        let mut doc = doc_of(long_diff(), layout(false, false), HEIGHT);
        if full {
            doc.toggle_full();
        }
        let top = doc.document.pos;
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
                for (y, row) in after.0.iter().enumerate().take(HEIGHT) {
                    let Some(from) = y.checked_add_signed(delta).filter(|&f| f < HEIGHT) else {
                        continue;
                    };
                    assert_eq!(
                        row, &before.0[from],
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
        assert_eq!(doc.document.pos, top, "back at the top, full {full}");
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
    doc.document.full = true;
    set.enumerate(|change| {
        doc.document.push(change, Source::Repo);
        ControlFlow::Continue(())
    })
    .expect("enumerate");
    let diff = |file: usize, doc: &Doc| {
        let change = doc.document.files()[file].change().clone();
        repo.file_diff(change, &Options::default()).expect("diff")
    };
    for file in [0, 1] {
        let fd = diff(file, &doc);
        load(&mut doc, file, fd);
    }
    assert_eq!(doc.document.pos.file, 0);

    std::fs::write(d.join("a.txt"), "one\n").expect("write");
    let update = set
        .refresh(&["a.txt".into()])
        .expect("refresh")
        .expect("changed");
    doc.replace(&update);
    assert_eq!(
        doc.document.files()[doc.document.pos.file].change().path,
        "z.txt"
    );
    assert_eq!(doc.line_at_top(), 397, "opened at its first change");
    doc.scroll(20);
    let left = doc.document.pos;
    doc.document.restale();
    let fd = diff(0, &doc);
    load(&mut doc, 0, fd);
    assert_eq!(doc.document.pos, left);
}

/// Noon on 2026-11-17, UTC: what the threads' ages count from.
const NOW: u64 = 1_794_916_800;

fn note(id: &str, author: &str, at: &str, text: &str, draft: bool) -> comments::Note {
    comments::Note {
        id: id.to_owned(),
        author: author.to_owned(),
        created: format!("2026-11-17T{at}:00Z"),
        text: text.to_owned(),
        suggestion: None,
        standing: if draft {
            comments::Standing::Draft
        } else {
            comments::Standing::Posted
        },
    }
}

/// A draft on the whole file; a resolved thread on removed old line 2; an
/// outdated posted one on added new line 2, long enough to wrap, with a
/// suggestion and your draft reply; and an outdated one with a reply whose
/// lines are gone. The resolved one and the lost one start folded.
fn threads() -> Arc<[Thread]> {
    let thread = |root: &str, at: At, look: Look, notes: Vec<comments::Note>| Thread {
        root: root.to_owned(),
        at,
        look,
        resolved: look == Look::Resolved,
        outdated: false,
        url: None,
        notes,
        lines: Vec::new(),
        snippet: None,
        local: false,
    };
    let mut asked = note(
        "n",
        "octocat",
        "10:00",
        "Why len() here and not a running count, kept as the items arrive?",
        false,
    );
    asked.suggestion = Some("    let total = items.len();".to_owned());
    vec![
        thread(
            "f",
            At::File,
            Look::Draft,
            vec![note("f", "", "11:59", "On the whole file", true)],
        ),
        thread(
            "o",
            At::Line {
                side: document::Side::Old,
                lines: 1..2,
            },
            Look::Resolved,
            vec![note("o", "ana", "09:00", "Was count right?", false)],
        ),
        Thread {
            outdated: true,
            lines: vec!["    let total = items.len() + 1;".to_owned()],
            ..thread(
                "n",
                At::Line {
                    side: document::Side::New,
                    lines: 1..2,
                },
                Look::Draft,
                vec![asked, note("r", "", "11:30", "Simpler.", true)],
            )
        },
        Thread {
            outdated: true,
            ..thread(
                "l",
                At::Lost,
                Look::Open,
                vec![
                    note("l", "ana", "08:00", "Moved away", false),
                    note("l2", "octocat", "08:30", "It did", false),
                ],
            )
        },
    ]
    .into()
}

fn commented(layout: Layout, height: usize) -> Doc {
    let mut doc = doc(layout, height);
    doc.now = NOW;
    doc.set_threads(0, threads());
    doc
}

/// Every row of the document, its threads' included, as one tall screen
/// draws them.
fn all_rows(layout: Layout) -> Vec<String> {
    let mut doc = commented(layout, 200);
    tall_rows(&mut doc)
}

/// Every row `doc` draws on a screen as wide as its layout, less the
/// empty ones at the end.
fn tall_rows(doc: &mut Doc) -> Vec<String> {
    let width = doc.layout.width.unwrap_or(60);
    let mut grid = Grid::new(width, doc.height);
    doc.draw(&mut grid, None, (0, width, 0), &Theme::default_theme(), 1);
    let mut rows: Vec<String> = (0..doc.height)
        .map(|y| grid.row_text(y).trim_end().to_owned())
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    rows
}

/// Threads show as boxes under the row of their last line, a cell into
/// the code's column: the first comment's author, age and state in the top
/// border, each reply's on a row over it after a blank one, text wrapped
/// inside, and a suggestion as a small diff. A thread with a draft has round corners.
/// Resolved and outdated threads without a draft start folded to one row.
/// The file's comments hang under its first row and the lost ones under
/// its last. Split, a thread takes its side's half.
#[test]
fn threads_hang_under_their_lines() {
    insta::assert_snapshot!("threads_unified", all_rows(layout(false, false)).join("\n"));
    insta::assert_snapshot!("threads_split", all_rows(layout(true, false)).join("\n"));
    insta::assert_snapshot!("threads_wrapped", all_rows(layout(false, true)).join("\n"));
    let narrow = Layout {
        width: Some(34),
        ..layout(false, false)
    };
    insta::assert_snapshot!("threads_narrow", all_rows(narrow).join("\n"));
}

/// Threads whose lines weren't found, with the code they were on: one by
/// dffy on added new line 145, between its context, a tab and a line long
/// enough to cut in it; one from a host on old lines 140–141, outdated,
/// with no context after.
fn lost() -> Arc<[Thread]> {
    let lines = |l: &[&str]| l.iter().map(|&s| s.to_owned()).collect::<Vec<_>>();
    let thread = |root: &str, comment: review::Comment, text: &str| Thread {
        root: root.to_owned(),
        at: At::Lost,
        look: Look::Open,
        resolved: false,
        outdated: comment.side == review::Side::Old,
        url: None,
        notes: vec![note(root, "ana", "09:00", text, false)],
        lines: Vec::new(),
        snippet: comments::snippet(&comment, true),
        local: false,
    };
    let added = review::Comment {
        path: "src/main.rs".to_owned(),
        start: Some(review::LineNumbers {
            old: None,
            new: Some(144),
        }),
        end: Some(review::LineNumbers {
            old: None,
            new: Some(144),
        }),
        anchor: Some(review::Anchor {
            before: lines(&["let request = Request {", "\turl,"]),
            lines: lines(&[
                "    headers: vec![(\"accept\", \"application/json\"), (\"x-trace\", id)],",
            ]),
            after: lines(&["    retries: 3,", "};"]),
        }),
        ..review::Comment::default()
    };
    let removed = review::Comment {
        path: "src/main.rs".to_owned(),
        side: review::Side::Old,
        start: Some(review::LineNumbers {
            old: Some(139),
            new: None,
        }),
        end: Some(review::LineNumbers {
            old: Some(140),
            new: None,
        }),
        anchor: Some(review::Anchor {
            before: lines(&["fn count(items: &[u8]) -> usize {"]),
            lines: lines(&["    let n = items.len();", "    n"]),
            after: Vec::new(),
        }),
        ..review::Comment::default()
    };
    vec![
        thread("a", added, "Accept anything here?"),
        thread("b", removed, "Why inline it?"),
    ]
    .into()
}

/// A thread whose lines weren't found shows the line it was on in its
/// heading, folded too, and the code at the top of its box: the commented
/// lines with their numbers and the side's sign, their context dimmed
/// around them, each line cut to the box like a suggestion's.
#[test]
fn lost_threads_show_the_code_they_were_on() {
    let rows = |width: usize| {
        let mut doc = doc(
            Layout {
                width: Some(width),
                ..layout(false, false)
            },
            200,
        );
        doc.now = NOW;
        doc.set_threads(0, lost());
        let folded = tall_rows(&mut doc);
        doc.fold.toggle("b");
        doc.refit();
        let rows = tall_rows(&mut doc);
        let from = rows
            .iter()
            .position(|r| r.contains("▾ ana"))
            .expect("a box");
        (folded, rows[from..].join("\n"))
    };
    let (folded, unified) = rows(60);
    insta::assert_snapshot!("lost_snippets", unified);
    insta::assert_snapshot!("lost_snippets_narrow", rows(34).1);
    let card = folded
        .iter()
        .find(|r| r.contains("▸ ana"))
        .expect("a folded row");
    assert!(card.contains("outdated · old lines 140–141"), "{card}");
}

/// Every row's height is the screen rows it draws, its threads' boxes
/// with their code included, at any width and wrapped or not.
#[test]
fn heights_count_the_rows_of_snippets() {
    for (width, wrap) in [(60, false), (34, false), (60, true)] {
        let layout = Layout {
            width: Some(width),
            ..layout(false, wrap)
        };
        let mut doc = doc(layout, 200);
        doc.set_threads(0, lost());
        doc.fold.toggle("b");
        doc.scroll(-1000);
        let mut pos = Pos {
            offset: 0,
            ..doc.document.pos
        };
        let layout = doc.file_layout(0);
        loop {
            let mut c = Canvas::default();
            doc.render(pos, &layout, &mut c);
            assert_eq!(c.len(), doc.height(pos, &layout), "{pos:?} at {width}");
            if !doc.next_row(&mut pos, &layout) {
                break;
            }
        }
    }
}

/// A snippet is highlighted once, when its thread is placed: drawing and
/// scrolling through it don't parse its code again.
#[test]
fn snippets_are_highlighted_once() {
    let before = comments::painted_count();
    let threads = lost();
    assert_eq!(comments::painted_count(), before + 2);
    let code = &threads[0].snippet.as_ref().expect("a snippet").rows[2];
    assert!(
        code.pieces.iter().any(|(ink, _)| ink.syntax.is_some()),
        "{code:?}"
    );
    let theme = Theme::default_theme();
    let mut doc = doc(layout(false, false), 6);
    doc.set_threads(0, threads);
    doc.fold.toggle("b");
    for _ in 0..40 {
        grid_rows(&mut doc, &theme);
        doc.scroll(1);
    }
    assert_eq!(comments::painted_count(), before + 2);
}

/// A posted thread on new line 2 with two replies, then another on the
/// same line.
fn conversation() -> Arc<[Thread]> {
    let on_line_2 = |root: &str, notes| Thread {
        root: root.to_owned(),
        at: At::Line {
            side: document::Side::New,
            lines: 1..2,
        },
        look: Look::Open,
        resolved: false,
        outdated: false,
        url: None,
        notes,
        lines: Vec::new(),
        snippet: None,
        local: false,
    };
    vec![
        on_line_2(
            "a",
            vec![
                note(
                    "a",
                    "ana",
                    "09:00",
                    "Should this count the items as they arrive, rather than call len() once at the end?",
                    false,
                ),
                note("a2", "octocat", "10:00", "It could, but the list is short.", false),
                note("a3", "ana", "11:30", "Fair, leaving it.", false),
            ],
        ),
        on_line_2(
            "b",
            vec![note("b", "octocat", "10:30", "Name it count?", false)],
        ),
    ]
    .into()
}

/// A thread reads as one conversation: each reply under a blank row and a
/// line of its own heading it, inside the box, its author bold as the first
/// comment's is in the border. Threads on one line are a blank row apart,
/// a row that belongs to neither, and scrolling counts it.
#[test]
fn replies_read_as_one_conversation() {
    let theme = Theme::default_theme();
    let threads = conversation();
    let width = 70;
    let commented = |height| {
        let mut doc = doc(
            Layout {
                width: Some(width),
                ..layout(false, false)
            },
            height,
        );
        doc.now = NOW;
        doc.set_threads(0, Arc::clone(&threads));
        doc.set_focus(Some("a".to_owned()));
        doc
    };
    let draw = |doc: &mut Doc| {
        let mut grid = Grid::new(width, doc.height);
        doc.draw(&mut grid, None, (0, width, 0), &theme, 1);
        let rows: Vec<String> = (0..doc.height)
            .map(|y| grid.row_text(y).trim_end().to_owned())
            .collect();
        (grid, rows)
    };
    let mut tall = commented(40);
    let (grid, mut expected) = draw(&mut tall);
    while expected.last().is_some_and(String::is_empty) {
        expected.pop();
    }
    let at = expected
        .iter()
        .position(|r| r.contains("items.len()"))
        .expect("the line");
    let end = expected
        .iter()
        .position(|r| r.contains("println!"))
        .expect("the next line");
    insta::assert_snapshot!("threads_conversation", expected[at..end].join("\n"));
    let bold = |y: usize, name: &str| {
        let x = expected[y].find(name).expect("the author");
        let x = expected[y][..x].chars().count();
        grid.row(y)[x].style().attributes.contains(Attributes::BOLD)
    };
    let reply = expected
        .iter()
        .position(|r| r.contains("octocat · 2h ago"))
        .expect("the reply's heading");
    assert!(bold(at + 1, "ana") && bold(reply, "octocat"));
    assert!(!bold(reply + 1, "It could"), "the text isn't");
    let gap = expected
        .iter()
        .position(|r| r.contains("▾ octocat"))
        .expect("the second thread")
        - 1;
    assert_eq!(tall.block_at(gap), None, "{expected:#?}");
    assert_eq!(tall.block_row_at(gap + 1), Some((Block::Thread(1), true)));
    let height = 4;
    let mut doc = commented(height);
    for top in 0..=expected.len() - height {
        assert_eq!(
            draw(&mut doc).1,
            expected[top..top + height],
            "scrolled {top}"
        );
        doc.scroll(1);
    }
}

/// A thread's rows count in the row's height: scrolling a row at a time
/// goes through them, a page moves a screen of them, and the bottom shows
/// the last thread's last row.
#[test]
fn scrolling_goes_through_the_rows_of_threads() {
    let theme = Theme::default_theme();
    for (split, wrap) in [(false, false), (true, false), (false, true)] {
        let expected = all_rows(layout(split, wrap));
        let height = 4;
        let mut doc = commented(layout(split, wrap), height);
        for top in 0..=expected.len() - height {
            let (_, rows) = grid_rows(&mut doc, &theme);
            assert_eq!(rows, expected[top..top + height], "scrolled {top}");
            doc.scroll(1);
        }
        let (_, rows) = grid_rows(&mut doc, &theme);
        assert_eq!(rows, expected[expected.len() - height..], "kept full");
        doc.scroll(-100);
        doc.scroll(isize::try_from(height).expect("small"));
        assert_eq!(grid_rows(&mut doc, &theme).1, expected[height..2 * height]);
        doc.bottom();
        assert_eq!(
            grid_rows(&mut doc, &theme).1,
            expected[expected.len() - height..],
            "split {split} wrap {wrap}"
        );
    }
}

/// A thread whose text grew in a reload of the comments file takes the
/// rows it wraps to now, though its box was measured before.
#[test]
fn a_reloaded_threads_rows_count_its_new_text() {
    let theme = Theme::default_theme();
    let longer: Arc<[Thread]> = threads()
        .iter()
        .map(|t| {
            let mut t = t.clone();
            if t.root == "n" {
                t.notes[0].text = t.notes[0].text.repeat(3);
            }
            t
        })
        .collect();
    let mut tall = commented(layout(false, false), 200);
    tall.set_threads(0, Arc::clone(&longer));
    let expected = tall_rows(&mut tall);
    let height = 4;
    let mut doc = commented(layout(false, false), height);
    doc.scroll(100);
    doc.set_threads(0, longer);
    doc.scroll(-100);
    for top in 0..=expected.len() - height {
        assert_eq!(
            grid_rows(&mut doc, &theme).1,
            expected[top..top + height],
            "scrolled {top}"
        );
        doc.scroll(1);
    }
}

/// `z` folds a thread to one row that sums it up, or unfolds one that
/// started folded; `Z` folds every one, then unfolds them all; a position
/// inside rows that went away moves up to the row.
#[test]
fn folded_threads_take_one_row() {
    let theme = Theme::default_theme();
    let mut doc = commented(layout(false, false), 40);
    let full = grid_rows(&mut doc, &theme).1;
    doc.fold.toggle("n");
    doc.refit();
    let folded = grid_rows(&mut doc, &theme).1;
    let summary = folded
        .iter()
        .find(|r| r.contains("▸ octocat"))
        .expect("a folded row");
    insta::assert_snapshot!(summary, @"     │  │ ▸ octocat · 2h ago · outdated · Why l…  1 reply │");
    assert_eq!(
        folded.iter().filter(|r| !r.is_empty()).count() + 9,
        full.iter().filter(|r| !r.is_empty()).count()
    );
    doc.fold.toggle("o");
    doc.refit();
    assert!(screen(&mut doc).contains("│ Was count right?"));
    assert!(doc.fold.toggle_all());
    let all = grid_rows(&mut doc, &theme).1;
    assert_eq!(
        all.iter().filter(|r| r.contains('▸')).count(),
        4,
        "{all:#?}"
    );
    assert!(!doc.fold.toggle_all(), "folded already: all unfold");
    assert!(!doc.fold.collapsed("o"), "resolved, unfolded with the rest");
    let mut doc = commented(layout(false, false), 4);
    doc.bottom();
    doc.fold.toggle_all();
    doc.refit();
    assert_eq!(doc.document.pos.offset, 0);
}

/// A fold is kept by the thread's root as folded or not, so it outlives
/// the comments file reloading, the thread reopened or resolved under it
/// included; a thread no one folded follows its state.
#[test]
fn folds_outlive_reloads_of_the_comments_file() {
    let mut doc = commented(layout(false, false), 40);
    assert!(doc.fold.collapsed("o") && doc.fold.collapsed("l"));
    assert!(!doc.fold.collapsed("n") && !doc.fold.collapsed("f"));
    doc.fold.toggle("o");
    // Reopened, and no longer outdated on the host.
    let open: Vec<Thread> = threads()
        .iter()
        .map(|t| Thread {
            resolved: false,
            outdated: false,
            look: t.look.max(Look::Open),
            ..t.clone()
        })
        .collect();
    doc.set_threads(0, open.into());
    assert!(!doc.fold.collapsed("o") && !doc.fold.collapsed("l"));
    doc.set_threads(0, threads());
    assert!(!doc.fold.collapsed("o"), "unfolded by z, resolved again");
    assert!(doc.fold.collapsed("l"), "outdated again");
    // A local review's comments are all drafts: resolving still folds.
    let local: Vec<Thread> = threads()
        .iter()
        .map(|t| Thread {
            resolved: t.root == "f",
            ..t.clone()
        })
        .collect();
    doc.set_threads(0, local.into());
    assert!(doc.fold.collapsed("f"));
    let rows = tall_rows(&mut doc);
    assert!(
        rows.iter().any(|r| r.contains("│ Was count right?")),
        "{rows:#?}"
    );
}

/// The focused thread's border is heavy, in the hint color, or in the
/// draft color when a draft is in it; the background stays the code's.
#[test]
fn a_focused_threads_border_is_heavy() {
    let theme = Theme::default_theme();
    let mut doc = commented(layout(false, false), 40);
    doc.fold.toggle("o");
    doc.set_focus(Some("o".to_owned()));
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let at = rows
        .iter()
        .position(|r| r.contains("▾ ana"))
        .expect("the focused thread");
    insta::assert_snapshot!(rows[at - 1..at + 4].join("\n"), @"
    2  ○-│     let total = count + 1;
         │  ┏ ▾ ana · 3h ago · resolved ━━━━━━━━━━━━━━━━━━━━━━┓
         │  ┃ Was count right?                                ┃
         │  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
      2◆+│     let total = items.len() + 1;
    ");
    assert_eq!(grid.row(at)[8].style(), theme.style(Role::Hint));
    assert_eq!(grid.row(at + 1)[20].style().bg, None);
    doc.set_focus(Some("n".to_owned()));
    let (grid, rows) = grid_rows(&mut doc, &theme);
    let at = rows
        .iter()
        .position(|r| r.contains("▾ octocat"))
        .expect("the focused draft");
    assert!(
        rows[at].contains('┏') && rows[at + 1].contains('┃'),
        "{rows:#?}"
    );
    assert_eq!(grid.row(at)[8].style(), theme.style(Role::Warn));
    doc.set_focus(Some("l".to_owned()));
    let rows = grid_rows(&mut doc, &theme).1;
    assert!(rows.iter().any(|r| r.contains("┃ ▸ ana")), "{rows:#?}");
}

/// A folded thread is one row drawn as a closed card: the open box's sides
/// at its columns, in the warning's role for a draft, heavy while it has
/// the focus, its text cut to fit and the reply count at its right end
/// while there's room for it.
#[test]
fn a_folded_thread_is_a_card_of_one_row() {
    let cards = |layout: Layout, focus: Option<&str>| {
        let mut doc = commented(layout, 200);
        doc.fold.toggle_all();
        doc.refit();
        doc.set_focus(focus.map(str::to_owned));
        let rows = tall_rows(&mut doc);
        rows.into_iter()
            .filter(|r| r.contains('▸'))
            .collect::<Vec<_>>()
            .join("\n")
    };
    insta::assert_snapshot!(cards(layout(false, false), None), @"
    │  │ ▸ you · 1m ago · draft · on the file · On the … │
    │  │ ▸ ana · 3h ago · resolved · Was count right?    │
    │  │ ▸ octocat · 2h ago · outdated · Why l…  1 reply │
    │  │ ▸ ana · 4h ago · outdated · lines not…  1 reply │
    ");
    insta::assert_snapshot!(cards(layout(false, false), Some("l")), @"
    │  │ ▸ you · 1m ago · draft · on the file · On the … │
    │  │ ▸ ana · 3h ago · resolved · Was count right?    │
    │  │ ▸ octocat · 2h ago · outdated · Why l…  1 reply │
    │  ┃ ▸ ana · 4h ago · outdated · lines not…  1 reply ┃
    ");
    insta::assert_snapshot!(cards(layout(false, false), Some("n")), @"
    │  │ ▸ you · 1m ago · draft · on the file · On the … │
    │  │ ▸ ana · 3h ago · resolved · Was count right?    │
    │  ┃ ▸ octocat · 2h ago · outdated · Why l…  1 reply ┃
    │  │ ▸ ana · 4h ago · outdated · lines not…  1 reply │
    ");
    insta::assert_snapshot!(cards(layout(true, false), None), @"
    │  │ ▸ you · 1m ago · draft · on the file · On the wh… │
    │  │ ▸ ana · 3h ago · … │ │
                              │   │  │ ▸ octocat · 2h ago… │
    │  │ ▸ ana · 4h ago · outdated · lines not f…  1 reply │
    ");
    let narrow = Layout {
        width: Some(34),
        ..layout(false, false)
    };
    insta::assert_snapshot!(cards(narrow, None), @"
    │  │ ▸ you · 1m ago · dra… │
    │  │ ▸ ana · 3h ago · res… │
    │  │ ▸ octocat ·…  1 reply │
    │  │ ▸ ana · 4h …  1 reply │
    ");
}

/// Inside its sides a folded thread takes the inactive selection's
/// background, the selection's while it has the focus, so it stands out
/// from the code; code rows and open boxes keep none. A theme whose
/// selection has no background (mono) tints nothing.
#[test]
fn a_folded_thread_is_tinted_inside_its_sides() {
    let theme = Theme::parse(
        "tinted",
        "selection = on #404040\nselection-inactive = on #202020",
        &mut Vec::new(),
    );
    let mut unified = commented(layout(false, false), 26);
    unified.set_focus(Some("l".to_owned()));
    let mut grid = Grid::new(60, 26);
    unified.draw(&mut grid, None, (0, 60, 0), &theme, 1);
    let tints = backgrounds(&grid, 0..26);
    let tints: Vec<&str> = tints.lines().map(str::trim_end).collect();
    insta::assert_snapshot!(tints.join("\n"), @r#"
    @@ -1,5 +1,6 @@
         │  ╭ ▾ you · 1m ago · draft · on the file ───────────╮
         │  │ On the whole file                               │
         │  ╰─────────────────────────────────────────────────╯
    1 1  │ fn main() {
    2  ○-│     let total = count + 1;
         │  │[202020  ▸ ana · 3h ago · resolved · Was count right?    ]│
      2◆+│     let total = items.len() + 1;
         │  ╭ ▾ octocat · 2h ago · outdated ──────────────────╮
         │  │ Why len() here and not a running count, kept as │
         │  │ the items arrive?                               │
         │  │ suggested change:                               │
         │  │ -     let total = items.len() + 1;              │
         │  │ +     let total = items.len();                  │
         │  │                                                 │
         │  │ you · 30m ago · draft                           │
         │  │ Simpler.                                        │
         │  ╰─────────────────────────────────────────────────╯
    3 3  │     println!("{total}");
      4 +│     eprintln!("done");
    4 5  │ }
    5   -│ // a comment long enough to wrap in a narrow split c…
      6 +│ // a comment long enough to wrap in a narrow split c…
         │\ No newline at end of file
         │  ┃[404040  ▸ ana · 4h ago · outdated · lines not…  1 reply ]┃
    "#);

    // Split, in a pane right of the tree: a second card under the same
    // line, past the blank row parting it from the first.
    let mut split = doc(layout(true, false), 8);
    split.now = NOW;
    split.set_threads(0, conversation());
    split.fold.toggle_all();
    split.refit();
    let mut grid = Grid::new(65, 8);
    split.draw(&mut grid, None, (5, 65, 0), &theme, 1);
    let tints = backgrounds(&grid, 2..6);
    let tints: Vec<&str> = tints.lines().map(str::trim_end).collect();
    insta::assert_snapshot!(tints.join("\n"), @"
    2 -│     let total = count +…│2●+│     let total = items.le…
                                 │   │  │[202020  ▸ ana · 3h ago · S… ]│
                                 │   │
                                 │   │  │[202020  ▸ octocat · 1h ago… ]│
    ");

    let mono = Theme::parse("m", "inherit = mono", &mut Vec::new());
    let mut grid = Grid::new(60, 26);
    unified.draw(&mut grid, None, (0, 60, 0), &mono, 1);
    assert!(
        (0..26).all(|y| grid.row(y).iter().all(|c| c.style().bg.is_none())),
        "{}",
        backgrounds(&grid, 0..26)
    );
}

/// The text box is drawn as a box too: a reply in its thread's box under a
/// heading of its own, a new comment in a draft's box under its lines. Each
/// counts in its row's height, and revealing it shows its cursor.
#[test]
fn the_text_box_is_boxed_where_the_thread_sits() {
    let new_box = || Writing {
        at: Writes::New {
            side: document::Side::New,
            lines: 3..4,
        },
        text: TextBox::new("New"),
    };
    let mut doc = commented(layout(false, false), 40);
    doc.writing = Some(Writing {
        at: Writes::Reply("n".to_owned()),
        text: TextBox::new("Sure, and a reply long enough to wrap in its box"),
    });
    doc.refit();
    let rows = tall_rows(&mut doc);
    let at = rows
        .iter()
        .position(|r| r.contains("Simpler."))
        .expect("the reply");
    insta::assert_snapshot!(rows[at..at + 6].join("\n"), @"
         │  ┃ Simpler.                                        ┃
         │  ┃                                                 ┃
         │  ┃ you · reply · esc saves · ctrl+g editor         ┃
         │  ┃ Sure, and a reply long enough to wrap in its    ┃
         │  ┃ box▏                                            ┃
         │  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
    ");
    doc.writing = Some(new_box());
    doc.refit();
    let rows = tall_rows(&mut doc);
    let at = rows
        .iter()
        .position(|r| r.contains("eprintln!"))
        .expect("the line");
    insta::assert_snapshot!(rows[at..at + 4].join("\n"), @"
      4 +│     eprintln!(\"done\");
         │  ┏ ▾ you · new comment · esc saves, empty discard… ┓
         │  ┃ New▏                                            ┃
         │  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
    ");
    let mut doc = commented(layout(false, false), 4);
    doc.writing = Some(new_box());
    doc.reveal_block(Block::New, true);
    let shown = screen(&mut doc);
    assert!(shown.contains("New▏"), "{shown}");
}

/// A frame after a scroll draws whole while threads are on screen, the
/// same as drawing it fresh: their rows change without a scroll.
#[test]
fn frames_with_threads_on_screen_are_drawn_whole() {
    let theme = Theme::default_theme();
    let mut doc = commented(layout(false, false), 6);
    let mut before = Grid::new(60, 6);
    doc.draw(&mut before, None, (0, 60, 0), &theme, 1);
    doc.scroll(1);
    doc.set_focus(Some("f".to_owned()));
    let mut after = Grid::new(60, 6);
    let (_, scrolled) = doc.draw(&mut after, Some(&before), (0, 60, 0), &theme, 2);
    assert_eq!(scrolled, None, "not copied");
    let mut fresh = Grid::new(60, 6);
    doc.draw(&mut fresh, None, (0, 60, 0), &theme, 3);
    let text = |g: &Grid| (0..6).map(|y| g.row_text(y)).collect::<Vec<_>>();
    assert_eq!(text(&after), text(&fresh));
    // Without threads on screen, the rows still on it are copied.
    let mut plain = self::doc(layout(false, false), 3);
    let mut before = Grid::new(60, 3);
    plain.draw(&mut before, None, (0, 60, 0), &theme, 1);
    plain.scroll(1);
    let mut after = Grid::new(60, 3);
    let (_, scrolled) = plain.draw(&mut after, Some(&before), (0, 60, 0), &theme, 2);
    assert!(scrolled.is_some());
}

/// A thread's rows are found by screen row for a tap, and never hit code:
/// a press there selects nothing.
#[test]
fn a_thread_row_is_found_by_row_and_never_hits_code() {
    let theme = Theme::default_theme();
    let mut doc = commented(layout(false, false), 40);
    let (_, rows) = grid_rows(&mut doc, &theme);
    let y = rows
        .iter()
        .position(|r| r.contains("Simpler."))
        .expect("the reply");
    assert_eq!(doc.block_at(y), Some(Block::Thread(2)));
    assert_eq!(doc.hit(y, 20, None), None);
    let code = rows
        .iter()
        .position(|r| r.contains("items.len()"))
        .expect("the line");
    assert_eq!(doc.block_at(code), None);
    assert!(doc.hit(code, 20, None).is_some());
}

/// `m` goes through the file's threads in the order the diff shows them,
/// from the top row when none is focused, focusing each and putting it on
/// screen; `M` goes back.
#[test]
fn threads_are_stepped_through_in_order() {
    let mut doc = commented(layout(false, false), 8);
    let mut seen = Vec::new();
    while doc.step_thread(true) {
        let focused = doc.focused().expect("focused").root.clone();
        let shown = screen(&mut doc);
        assert!(shown.contains(['┏', '┃']), "{focused} on screen:\n{shown}");
        seen.push(focused);
    }
    assert_eq!(seen, ["f", "o", "n", "l"]);
    assert!(doc.step_thread(false));
    assert_eq!(doc.focused().map(|t| t.root.as_str()), Some("n"));
    doc.set_focus(None);
    doc.document.top();
    assert!(!doc.step_thread(false), "none above the top");
    assert!(doc.edge_thread(true));
    assert_eq!(doc.focused().map(|t| t.root.as_str()), Some("l"));
    assert!(doc.go_to_thread("o"));
    assert_eq!(doc.focused().map(|t| t.root.as_str()), Some("o"));
}

/// An added file of `n` lines, `line 1` to `line n`, with an open thread
/// under each line of `on` (1-based), its root the line's number.
fn long_commented(n: usize, on: &[usize], height: usize) -> Doc {
    let new = (1..=n)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let mut doc = doc_of(
        file_diff("src/long.rs", "", &new),
        layout(false, false),
        height,
    );
    doc.now = NOW;
    let threads: Vec<Thread> = (on.iter())
        .map(|&line| Thread {
            root: line.to_string(),
            at: At::Line {
                side: document::Side::New,
                lines: line - 1..line,
            },
            look: Look::Open,
            resolved: false,
            outdated: false,
            url: None,
            notes: vec![note(&line.to_string(), "ana", "09:00", "Here?", false)],
            lines: Vec::new(),
            snippet: None,
            local: false,
        })
        .collect();
    doc.set_threads(0, threads.into());
    doc
}

/// A thread gone to lands with its box a third of the way down the pane,
/// the line it's on right above, unless the whole box shows already; near
/// either end of the file the position stops there.
#[test]
fn a_thread_gone_to_lands_a_third_from_the_top() {
    let mut doc = long_commented(100, &[6, 50, 54, 99], 30);
    let top = doc.first_row(0);
    assert_eq!(doc.document.pos, top);
    assert!(doc.go_to_thread("6"));
    assert_eq!(doc.document.pos, top, "on screen already");
    assert!(doc.go_to_thread("50"));
    let rows = grid_rows(&mut doc, &Theme::default_theme()).1;
    let border = rows.iter().position(|r| r.contains('┏')).expect("the box");
    assert_eq!(border, 10, "{}", rows.join("\n"));
    assert!(rows[border - 1].contains("line 50"), "{}", rows.join("\n"));
    let there = doc.document.pos;
    assert!(doc.go_to_thread("54"));
    assert_eq!(doc.document.pos, there, "54's box shows below 50's");
    assert!(doc.go_to_thread("99"));
    let end = doc.document.pos;
    doc.bottom();
    assert_eq!(end, doc.document.pos, "stopped at the end");
    assert!(doc.go_to_thread("6"));
    assert_eq!(doc.document.pos, top, "stopped at the start");
    // A box too tall for a third goes higher to fit, its line above it.
    let mut doc = long_commented(100, &[50], 12);
    let mut threads = doc.threads().to_vec();
    threads[0].notes[0].text = "a\nb\nc\nd\ne\nf\ng\nh".to_owned();
    doc.set_threads(0, threads.into());
    assert!(doc.go_to_thread("50"));
    let rows = grid_rows(&mut doc, &Theme::default_theme()).1;
    let border = rows.iter().position(|r| r.contains('┏')).expect("the box");
    assert_eq!(border, 2, "{}", rows.join("\n"));
    assert!(rows[1].contains("line 50"), "{}", rows.join("\n"));
    assert!(rows[11].contains('┗'), "{}", rows.join("\n"));
}

/// A peek shows a folded thread whole until it moves on, and the rows'
/// heights follow: a box's rows are kept by whether it's folded.
#[test]
fn a_peeked_thread_shows_whole_until_the_peek_moves_on() {
    let mut doc = commented(layout(false, false), 200);
    let heights = |doc: &mut Doc| {
        let layout = doc.file_layout(0);
        let mut pos = doc.first_row(0);
        loop {
            let mut c = Canvas::default();
            doc.render(pos, &layout, &mut c);
            assert_eq!(c.len(), doc.height(pos, &layout), "{pos:?}");
            if !doc.next_row(&mut pos, &layout) {
                break;
            }
        }
    };
    assert!(doc.fold.collapsed("o") && doc.fold.collapsed("l"));
    heights(&mut doc);
    doc.peek(Some("o"));
    assert!(!doc.fold.collapsed("o"));
    heights(&mut doc);
    doc.peek(Some("l"));
    assert!(doc.fold.collapsed("o") && !doc.fold.collapsed("l"));
    heights(&mut doc);
    doc.peek(None);
    assert!(doc.fold.collapsed("l"));
    heights(&mut doc);
    // A tap focusing another thread ends the peek; one on it keeps it.
    doc.peek(Some("o"));
    doc.set_focus(Some("o".to_owned()));
    assert!(!doc.fold.collapsed("o"));
    doc.set_focus(Some("n".to_owned()));
    assert!(doc.fold.collapsed("o"));
}

/// `z` on a peeked thread folds it for good, as on any other, and a thread
/// `z` unfolded stays so when the peek moves on; `Z` ends the peek, so a
/// second `Z` unfolds every thread.
#[test]
fn z_on_a_peeked_thread_is_the_users_choice() {
    let mut doc = commented(layout(false, false), 40);
    doc.peek(Some("o"));
    doc.fold.toggle("o");
    assert!(doc.fold.collapsed("o"));
    doc.peek(Some("l"));
    doc.peek(Some("o"));
    assert!(doc.fold.collapsed("o"), "folded by z");
    doc.peek(None);
    doc.fold.toggle("l");
    doc.peek(Some("l"));
    doc.peek(None);
    assert!(!doc.fold.collapsed("l"), "unfolded by z");
    let mut doc = commented(layout(false, false), 40);
    doc.peek(Some("o"));
    assert!(doc.fold.toggle_all());
    assert!(doc.fold.collapsed("o"), "Z ended the peek");
    doc.peek(Some("o"));
    assert!(!doc.fold.toggle_all(), "the peeked one doesn't count");
}

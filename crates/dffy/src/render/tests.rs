use dffy_core::diff::{LineKind, Options, Text, Unchanged, hunks};
use dffy_core::view::{Staging, Syntax};

use super::*;

fn layout(split: bool, width: Option<usize>, wrap: bool) -> Layout {
    Layout {
        split,
        width,
        wrap,
        words: false,
        tab_width: 4,
        indicators: Indicators::Classic,
        one_sided: None,
        unmerged: false,
        digits: 0,
        inline_signs: true,
        scroll: 0,
        line_numbers: true,
        hunk_headers: true,
        modified_lines: true,
        read_alone: false,
    }
}

/// Renders a diff of two texts through the core's hunks, the way print
/// mode does.
fn render(old: &str, new: &str, layout: Layout) -> String {
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    render_hunks(&old, &new, &hunks(&old, &new, &Options::default()), layout)
}

fn render_hunks(old: &Text, new: &Text, hunks: &[dffy_core::diff::Hunk], layout: Layout) -> String {
    let mut c = Canvas::default();
    for hunk in hunks {
        let lines: Vec<Line> = hunk
            .lines
            .iter()
            .map(|l| {
                let (kind, text) = match l.kind {
                    LineKind::Context => (Kind::Context, new.line(l.new)),
                    LineKind::Removed => (Kind::Removed, old.line(l.old)),
                    LineKind::Added => (Kind::Added, new.line(l.new)),
                };
                Line {
                    state: LineState::of(l),
                    old: (kind != Kind::Added).then_some(l.old + 1),
                    new: (kind != Kind::Removed).then_some(l.new + 1),
                    text: text.strip_suffix(b"\n").unwrap_or(text),
                    emphasis: &l.emphasis,
                    syntax: None,
                    paint: &[],
                    block_start: l.block_start,
                }
            })
            .collect();
        let header = HunkHeader {
            old_start: hunk.old_start + 1,
            old_len: hunk.old_len,
            new_start: hunk.new_start + 1,
            new_len: hunk.new_len,
            heading: b"fn main()",
            hidden: 0,
        };
        layout.hunk(&header, true, &lines, &mut c);
    }
    c.markup()
}

const OLD: &str = "fn main() {\n    let total = count + 1;\n    println!(\"{total}\");\n}\n";
const NEW: &str = "fn main() {\n    let total = items.len() + 1;\n    println!(\"{total}\");\n    eprintln!(\"done\");\n}\n";

#[test]
fn unified_layout() {
    insta::assert_snapshot!(render(OLD, NEW, layout(false, None, false)));
}

#[test]
fn split_layout() {
    insta::assert_snapshot!(render(OLD, NEW, layout(true, Some(90), false)));
}

#[test]
fn split_truncates_and_wraps_each_side() {
    let old = "a = \"the quick brown fox jumps over the lazy dog\"\n";
    let new = "a = \"the quick brown cat jumps over the lazy dog and keeps going\"\n";
    insta::assert_snapshot!(
        "split_truncate",
        render(old, new, layout(true, Some(60), false))
    );
    insta::assert_snapshot!("split_wrap", render(old, new, layout(true, Some(60), true)));
}

#[test]
fn split_falls_back_to_unified_when_too_narrow() {
    let out = render(OLD, NEW, layout(true, Some(30), false));
    assert!(!out.contains("<dim>│</>"), "{out}");
}

#[test]
fn unified_wraps_under_a_blank_gutter() {
    let new = format!("{}\n", "word ".repeat(12));
    insta::assert_snapshot!(render("short\n", &new, layout(false, Some(40), true)));
}

/// The rows of a line of `text` wrapped between words in `width` columns
/// of code, with its marks shown, less the gutter.
fn word_rows(text: &str, paint: &[Paint], width: usize) -> Vec<String> {
    let layout = Layout {
        words: true,
        line_numbers: false,
        ..layout(false, Some(width + 3), true)
    };
    let line = Line {
        state: LineState::Context,
        old: Some(1),
        new: Some(1),
        text: text.as_bytes(),
        emphasis: &[],
        syntax: None,
        paint,
        block_start: false,
    };
    let mut c = Canvas::default();
    unified::line(&layout, 0, &line, &mut c);
    c.markup()
        .lines()
        .map(|row| row.strip_prefix("<dim> │</> ").unwrap_or(row).to_owned())
        .collect()
}

#[test]
fn word_wrap_breaks_between_words_and_keeps_styles() {
    let paint = [Paint {
        start: 4,
        end: 15,
        syntax: Some(Syntax::Keyword),
        attributes: Attributes::NONE,
    }];
    assert_eq!(
        word_rows("the quick brown fox jumps over", &paint, 12),
        [
            "the <:keyword>quick </>",
            "<:keyword>brown</> fox ",
            "jumps over",
        ]
    );
}

/// A word wider than the row breaks inside, as a hyphenated or slashed
/// one does after its hyphens and slashes.
#[test]
fn word_wrap_breaks_inside_only_a_word_longer_than_the_row() {
    assert_eq!(
        word_rows("a supercalifragilisticexpialidocious word", &[], 10),
        ["a ", "supercalif", "ragilistic", "expialidoc", "ious word"]
    );
    assert_eq!(
        word_rows("see path/to/some-long-file", &[], 10),
        ["see path/", "to/some-", "long-file"]
    );
    assert_eq!(
        word_rows("a very well-known word", &[], 12),
        ["a very ", "well-known ", "word"]
    );
}

/// Wide characters count two columns and a row may break beside any of
/// them, so text without spaces still fills its rows.
#[test]
fn word_wrap_counts_wide_characters_twice() {
    assert_eq!(
        word_rows("日本語のテキストです", &[], 7),
        ["日本語", "のテキ", "ストで", "す"]
    );
}

#[test]
fn display_width_handles_tabs_wide_combining_controls_and_bad_utf8() {
    let mut c = Canvas::default();
    let text = b"\tx\xe6\x97\xa5\xe6\x9c\xac e\xcc\x81 \x1b[31m \xff end";
    let mut cursor = Cursor::default();
    let used = put_text(
        &mut c,
        text,
        NO_MARKS,
        (Role::Plain, Role::Plain),
        4,
        100,
        &mut cursor,
    );
    c.end_line();
    assert_eq!(c.markup(), "    x日本 e\u{301} ^[[31m \u{fffd} end\n");
    // 4 (tab) + 1 + 4 (two wide) + 1 + 1 (e + combining) + 1 + 6 + 1 + 1 + 1 + 3
    assert_eq!(used, 24);
}

/// Bidirectional overrides and isolates would reorder the text around them
/// on screen; they show as their code points, in content and in paths.
#[test]
fn bidi_controls_show_as_their_code_points() {
    let text = "if x {\u{202e} } \u{2066}admin\u{2069}".as_bytes();
    let mut c = Canvas::default();
    let mut cursor = Cursor::default();
    let used = put_text(
        &mut c,
        text,
        NO_MARKS,
        (Role::Plain, Role::Plain),
        4,
        100,
        &mut cursor,
    );
    c.end_line();
    assert_eq!(c.markup(), "if x {<U+202E> } <U+2066>admin<U+2069>\n");
    assert_eq!(used, width(text, 4));
    assert_eq!(used, 6 + 8 + 3 + 8 + 5 + 8);
    assert_eq!(escape("a\u{202d}b".as_bytes()), "a<U+202D>b");
}

#[test]
fn wide_glyphs_never_straddle_the_edge() {
    let mut c = Canvas::default();
    let layout = layout(false, Some(10), false);
    let used = layout.put_fitted(
        &mut c,
        "日本語日本語".as_bytes(),
        NO_MARKS,
        (Role::Plain, Role::Plain),
        7,
    );
    c.end_line();
    assert_eq!(c.markup(), "日本語<dim>…</>\n");
    assert_eq!(used, 7);
}

#[test]
fn emphasis_ranges_switch_roles_mid_line() {
    let mut c = Canvas::default();
    let mut cursor = Cursor::default();
    put_text(
        &mut c,
        b"let x = 1;",
        (std::slice::from_ref(&(8..9)), &[], &[]),
        (Role::Added, Role::AddedEmph),
        4,
        100,
        &mut cursor,
    );
    c.end_line();
    assert_eq!(
        c.markup(),
        "<added>let x = </><added-emph>1</><added>;</>\n"
    );
}

#[test]
fn file_headers_name_the_change() {
    let mut c = Canvas::default();
    let l = layout(false, Some(80), false);
    l.file_header(
        &FileHeader {
            status: FileStatus::Renamed,
            old_path: b"src/old.rs",
            new_path: b"src/new.rs",
            old_mode: Some("100644"),
            new_mode: Some("100755"),
            similarity: Some(91),
            note: None,
            staging: None,
        },
        &mut c,
    );
    l.file_header(
        &FileHeader {
            status: FileStatus::Modified,
            old_path: b"logo.png",
            new_path: b"logo.png",
            old_mode: None,
            new_mode: None,
            similarity: None,
            note: Some("binary files differ"),
            staging: None,
        },
        &mut c,
    );
    insta::assert_snapshot!(c.markup(), @r"
    <file-header>renamed: src/old.rs → src/new.rs</><dim> (91% similar) (mode 100644 → 100755)</>
    <file-header>─────────────────────────────────────────────────────────────────────</>
    <file-header>modified: logo.png</>
    <file-header>──────────────────</>
    <dim>binary files differ</>
    ");
}

/// In the default diff the header words the file's staging after its
/// status; an untracked file can't be staged, so it gets no word.
#[test]
fn file_headers_word_the_staging() {
    use Staging;
    let mut c = Canvas::default();
    let l = layout(false, Some(80), false);
    for (status, path, staging) in [
        (FileStatus::Added, b"new.rs", Staging::Staged),
        (FileStatus::Modified, b"lib.rs", Staging::Unstaged),
        (FileStatus::Modified, b"app.rs", Staging::Partly),
        (FileStatus::Untracked, b"tmp.rs", Staging::Unstaged),
    ] {
        l.file_header(
            &FileHeader {
                status,
                old_path: path,
                new_path: path,
                old_mode: None,
                new_mode: None,
                similarity: None,
                note: None,
                staging: Some(staging),
            },
            &mut c,
        );
    }
    insta::assert_snapshot!(c.markup(), @r"
    <file-header>new file · staged: new.rs</>
    <file-header>─────────────────────────</>
    <file-header>modified · unstaged: lib.rs</>
    <file-header>───────────────────────────</>
    <file-header>modified · partly staged: app.rs</>
    <file-header>────────────────────────────────</>
    <file-header>untracked file: tmp.rs</>
    <file-header>──────────────────────</>
    ");
}

#[test]
fn ansi_writer_switches_styles_and_resets_each_row() {
    let theme = Theme::default_theme();
    let mut c = Canvas::default();
    c.push(Role::LineNumber, "1");
    c.push(Role::Plain, " ");
    c.push(Role::Added, "+a");
    c.push(Role::AddedEmph, "b");
    c.end_line();
    c.line(Role::Plain, "plain");
    let mut out = Vec::new();
    c.write_to(&mut out, Some(&theme)).expect("vec");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "\x1b[0;2m1\x1b[0m \x1b[0;32m+a\x1b[0;7;32mb\x1b[0m\nplain\n"
    );
    let mut plain = Vec::new();
    c.write_to(&mut plain, None).expect("vec");
    assert_eq!(plain, b"1 +ab\nplain\n");
}

#[test]
fn highlighted_lines_keep_the_change_in_the_gutter_and_the_emphasis() {
    let run = |start, end, syntax| Run { start, end, syntax };
    let added = [run(0, 3, Syntax::Keyword), run(8, 9, Syntax::Number)];
    let context = [run(0, 2, Syntax::Keyword)];
    let lines = [
        Line {
            state: LineState::Context,
            old: Some(1),
            new: Some(1),
            text: b"fn f() {",
            emphasis: &[],
            syntax: Some(&context),
            paint: &[],
            block_start: false,
        },
        Line {
            state: LineState::Added,
            old: None,
            new: Some(2),
            text: b"let x = 1;",
            emphasis: std::slice::from_ref(&(8..9)),
            syntax: Some(&added),
            paint: &[],
            block_start: false,
        },
    ];
    let header = HunkHeader {
        old_start: 1,
        old_len: 1,
        new_start: 1,
        new_len: 2,
        heading: b"",
        hidden: 0,
    };
    let mut c = Canvas::default();
    layout(false, Some(40), false).hunk(&header, false, &lines, &mut c);
    insta::assert_snapshot!(c.markup(), @r"
    <line-number>1</> <line-number>1</><dim> │</> <:keyword>fn</> f() {
      <added-number>2</><dim> │</><added>+</><added-line:keyword>let</><added-line> x = </><added-emph:number>1</><added-line>;</>
    ");
    assert_eq!(c.fill(0), None);
    assert_eq!(c.fill(1), Some(Role::AddedLine.into()));
}

#[test]
fn syntax_diff_dims_unchanged_code_and_pairs_matched_lines() {
    let old = "int f(void)\n{\n\tint unused = 0;\n\treturn compute(a,\n\t\tb);\n}\n";
    let new = "int f(void)\n{\n\treturn compute(a, c);\n}\n";
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let line = hunks(&old, &new, &Options::default());
    let s = dffy_core::structural::diff(crate::grammars(), b"f.c", &old, &new, &line)
        .expect("syntax diff");
    // The matched line sits beside its counterpart, not beside the removed
    // line before it.
    insta::assert_snapshot!(render_hunks(&old, &new, &s.hunks, layout(true, Some(70), false)), @r"
    <hunk-header>@@ -1,6 +1,4 @@</> fn main()
    <line-number>1</><dim> │</> int f(void)                   <dim>│</><line-number>1</><dim> │</> int f(void)
    <line-number>2</><dim> │</> {                             <dim>│</><line-number>2</><dim> │</> {
    <removed-number>3</><dim> │</><removed>-    int unused = 0;           </><dim>│</> <dim> │</>
    <dim>4 │~    return compute(a,         │</><modified-number>3</><dim> │</><modified>+    return compute(a, </><added-emph>c</><modified>);</>
    <removed-number>5</><dim> │</><removed>-        </><removed-emph>b</><removed>);                   </><dim>│</> <dim> │</>
    <line-number>6</><dim> │</> }                             <dim>│</><line-number>4</><dim> │</> }
    ");
}

#[test]
fn syntax_diff_marks_moved_code_apart_from_reformatted_code() {
    let old = "int a(void) { return 1; }\nint b(void) { return 2; }\nint c = f(x,\n\ty);\n";
    let new = "int b(void) { return 2; }\nint a(void) { return 1; }\nint c = f(x, y);\n";
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let line = hunks(&old, &new, &Options::default());
    let s = dffy_core::structural::diff(crate::grammars(), b"f.c", &old, &new, &line)
        .expect("syntax diff");
    insta::assert_snapshot!(render_hunks(&old, &new, &s.hunks, layout(false, Some(60), false)), @r"
    <hunk-header>@@ -1,4 +1,3 @@</> fn main()
    <moved>1</>  <dim> │</><moved>»int a(void) { return 1; }</>
    <line-number>2</> <line-number>1</><dim> │</> int b(void) { return 2; }
      <moved>2</><dim> │</><moved>»int a(void) { return 1; }</>
    <dim>3</>  <dim> │~int c = f(x,</>
    <dim>4</>  <dim> │~    y);</>
      <dim>3 │~int c = f(x, y);</>
    ");
}

#[test]
fn change_indicators_bars_classic_and_none() {
    for (indicators, name) in [
        (Indicators::Bars, "indicators_bars"),
        (Indicators::Classic, "indicators_classic"),
        (Indicators::None, "indicators_none"),
        (Indicators::Background, "indicators_background"),
    ] {
        let mut layout = layout(true, Some(90), false);
        layout.indicators = indicators;
        // With color: classic signs get their own column.
        layout.inline_signs = false;
        insta::assert_snapshot!(name, render(OLD, NEW, layout));
    }
}

/// Without color, classic signs go right before the text, so the output
/// still reads as a diff; with color they sit left of the gutter's rule.
/// Either way the code itself starts in the same column on every row.
#[test]
fn classic_signs_lead_the_text_without_color_and_have_a_column_with_it() {
    for (inline, expected) in [
        (
            true,
            [
                "1 1 │ fn main() {",
                "2   │-    let total = count + 1;",
                "  2 │+    let total = items.len() + 1;",
            ],
        ),
        (
            false,
            [
                "1 1  │ fn main() {",
                "2   -│     let total = count + 1;",
                "  2 +│     let total = items.len() + 1;",
            ],
        ),
    ] {
        let mut layout = layout(false, Some(60), false);
        layout.inline_signs = inline;
        let out = render(OLD, NEW, layout);
        let rows: Vec<String> = out
            .lines()
            .skip(1)
            .take(3)
            .map(|l| {
                let mut plain = String::new();
                let mut tag = false;
                for c in l.chars() {
                    match c {
                        '<' => tag = true,
                        '>' if tag => tag = false,
                        c if !tag => plain.push(c),
                        _ => {}
                    }
                }
                plain.trim_end().to_owned()
            })
            .collect();
        assert_eq!(rows, expected, "inline {inline}");
    }
}

#[test]
fn moved_and_reformatted_lines_keep_their_marks_as_bars() {
    let old = "int a(void) { return 1; }\nint b(void) { return 2; }\nint c = f(x,\n\ty);\n";
    let new = "int b(void) { return 2; }\nint a(void) { return 1; }\nint c = f(x, y);\n";
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let line = hunks(&old, &new, &Options::default());
    let s = dffy_core::structural::diff(crate::grammars(), b"f.c", &old, &new, &line)
        .expect("syntax diff");
    let mut layout = layout(false, Some(60), false);
    layout.indicators = Indicators::Bars;
    insta::assert_snapshot!("moved_bars", render_hunks(&old, &new, &s.hunks, layout));
}

/// A file diff of `old` to `new` as print mode lays it out.
fn print_file(status: dffy_core::git::Status, old: &str, new: &str, layout: Layout) -> String {
    use dffy_core::git::{FileChange, FileDiff, Mode, Side, Status};
    let side = Some(Side::Worktree {
        mode: Mode::File,
        id: None,
    });
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let fd = FileDiff {
        change: FileChange {
            path: "f.txt".into(),
            old: side.filter(|_| status != Status::Added),
            new: side.filter(|_| status != Status::Deleted),
            status,
            staging: None,
        },
        hunks: hunks(&old, &new, &Options::default()),
        old,
        new,
        binary: false,
        too_large: false,
        note: None,
    };
    let mut c = Canvas::default();
    crate::print::file(&layout, &fd, None, None, &mut c);
    c.markup()
}

#[test]
fn print_mode_shows_one_sided_files_as_one_pane_even_split() {
    use dffy_core::git::Status;
    let text = "first line\na second line that is long enough to need more than half the width\n";
    insta::assert_snapshot!(
        "one_sided_added",
        print_file(Status::Added, "", text, layout(true, Some(60), false))
    );
    insta::assert_snapshot!(
        "one_sided_deleted",
        print_file(Status::Deleted, text, "", layout(true, Some(60), false))
    );
}

#[test]
fn print_mode_stands_separators_for_unchanged_lines() {
    let old = (1..=30)
        .map(|i| format!("line {i}\n"))
        .collect::<Vec<_>>()
        .concat();
    let new = old
        .replace("line 5\n", "line five\n")
        .replace("line 22\n", "line 22b\n");
    insta::assert_snapshot!(
        "separators",
        print_file(
            dffy_core::git::Status::Modified,
            &old,
            &new,
            layout(false, Some(60), false)
        )
    );
}

#[test]
fn conflict_markers_stand_out_in_unmerged_files_only() {
    use dffy_core::git::Status;
    let ours = "fn a() {}\n";
    let worktree = "<<<<<<< ours\nfn a() {}\n||||||| base\nfn b() {}\n=======\nfn c() {}\n\
                    >>>>>>> theirs\n======= not a marker\n<<<<<<<< eight\n";
    insta::assert_snapshot!(
        "conflict_markers",
        print_file(
            Status::Unmerged,
            ours,
            worktree,
            layout(false, Some(60), false)
        )
    );
    let modified = print_file(
        Status::Modified,
        ours,
        worktree,
        layout(true, Some(80), false),
    );
    assert!(!modified.contains("<warn>"), "{modified}");
}

/// The cells a mapped canvas traces back to code, one per glyph:
/// `row:col+width side bytes` (side 0 old, 1 new), then the columns each
/// side's code starts at.
fn traced(layout: Layout, rows: &[(Option<&str>, Option<&str>)]) -> String {
    use std::fmt::Write as _;
    let line = |kind, text: &'static str, n| Line {
        state: LineState::new(kind, false, Unchanged::No),
        old: (kind == Kind::Removed).then_some(n),
        new: (kind == Kind::Added).then_some(n),
        text: text.as_bytes(),
        emphasis: &[],
        syntax: None,
        paint: &[],
        block_start: false,
    };
    let mut c = Canvas::mapped();
    for (n, &(old, new)) in rows.iter().enumerate() {
        let leak = |s: Option<&str>| s.map(|s| &*Box::leak(s.to_owned().into_boxed_str()));
        let (old, new) = (leak(old), leak(new));
        let lines = [
            old.map(|t| line(Kind::Removed, t, n + 1)),
            new.map(|t| line(Kind::Added, t, n + 1)),
        ];
        if layout.split {
            split::row(&layout, 1, [lines[0].as_ref(), lines[1].as_ref()], &mut c);
        } else {
            for l in lines.iter().flatten() {
                unified::line(&layout, 1, l, &mut c);
            }
        }
    }
    let map = c.map().expect("mapped");
    let mut out = String::new();
    for g in &map.glyphs {
        let _ = writeln!(
            out,
            "{}:{}+{} {} {:?}",
            g.row, g.col, g.width, g.side, g.bytes
        );
    }
    for a in &map.areas {
        let _ = writeln!(out, "area {}:{} {}", a.row, a.col, a.side);
    }
    out
}

/// A tab takes the columns to its stop, a wide character two, and the
/// bytes are those of the line, not of what's drawn.
#[test]
fn mapped_cells_count_tabs_and_wide_characters() {
    let layout = Layout {
        one_sided: Some(Side::New),
        ..layout(false, Some(40), false)
    };
    insta::assert_snapshot!(traced(layout, &[(None, Some("a\t漢x"))]), @r"
    0:4+1 1 0..1
    0:5+3 1 1..2
    0:8+2 1 2..5
    0:10+1 1 5..6
    area 0:4 1
    ");
}

/// Each glyph the map has is where the grid draws it, for text whose
/// measures have disagreed: unified, split, wrapped in any narrow pane and
/// scrolled through. Wrapped, every byte is drawn, and no pane is overrun.
#[test]
fn mapped_cells_are_where_the_grid_draws_them() {
    for s in columns::TRICKY {
        let text = format!("a{s}b{s}c");
        let line = Line {
            state: LineState::new(Kind::Added, false, Unchanged::No),
            old: None,
            new: Some(1),
            text: text.as_bytes(),
            emphasis: &[],
            syntax: None,
            paint: &[],
            block_start: false,
        };
        let one_sided = Layout {
            one_sided: Some(Side::New),
            ..layout(false, Some(40), false)
        };
        // From two columns of code: in one, a wide glyph draws blank.
        let wrapped = (8..16).flat_map(|width| {
            [false, true].map(|words| Layout {
                words,
                ..layout(false, Some(width), true)
            })
        });
        let scrolled = (1..10).map(|scroll| Layout {
            scroll,
            ..one_sided
        });
        let layouts = [one_sided, layout(true, Some(41), false)]
            .into_iter()
            .chain(wrapped)
            .chain(scrolled);
        for layout in layouts {
            let width = layout.width.expect("set");
            let mut c = Canvas::mapped();
            if layout.split {
                split::row(&layout, 1, [None, Some(&line)], &mut c);
            } else {
                unified::line(&layout, 1, &line, &mut c);
            }
            let map = c.map().expect("mapped");
            let mut drawn = String::new();
            for (y, row) in c.rows().enumerate() {
                // A column past the pane, to see it overrun.
                let mut grid = crate::term::Grid::new(width + 1, 1);
                let mut x = 0;
                for (_, piece) in row {
                    x = grid.put_str(x, 0, width + 1, piece, crate::theme::Style::PLAIN);
                }
                assert!(x <= width, "{layout:?}: {:?}", grid.row_text(0));
                for g in map.glyphs.iter().filter(|g| g.row == y) {
                    let source = &text[g.bytes.clone()];
                    let cells = &grid.row(0)[g.col..g.col + g.width];
                    let shown: String = cells.iter().flat_map(|c| c.text().chars()).collect();
                    assert_eq!(shown, source, "{layout:?}: {:?}", grid.row_text(0));
                    drawn.push_str(source);
                }
            }
            if layout.scroll == 0 {
                assert_eq!(drawn, text, "{layout:?}");
            }
        }
    }
}

/// In one column of code an emoji draws blank, as any wide glyph does, and
/// the line goes on past it.
#[test]
fn an_emoji_wider_than_its_row_is_blank() {
    let line = Line {
        state: LineState::new(Kind::Added, false, Unchanged::No),
        old: None,
        new: Some(1),
        text: "a\u{26a0}\u{fe0f}b".as_bytes(),
        emphasis: &[],
        syntax: None,
        paint: &[],
        block_start: false,
    };
    let mut c = Canvas::mapped();
    unified::line(&layout(false, Some(7), true), 1, &line, &mut c);
    let rows: Vec<String> = c
        .rows()
        .map(|row| row.map(|(_, text)| text).collect())
        .collect();
    let code: Vec<char> = rows.iter().filter_map(|r| r.chars().last()).collect();
    assert_eq!(code, ['a', ' ', 'b'], "{rows:?}");
    let bytes: Vec<_> = (c.map().expect("mapped").glyphs.iter())
        .map(|g| g.bytes.clone())
        .collect();
    assert_eq!(bytes, [0..1, 1..7, 7..8]);
}

/// Every character U+FE0F widens is one glyph with it, whichever range of
/// Unicode it's in.
#[test]
fn every_selectable_character_takes_its_selector() {
    let all: Vec<char> = ('\0'..=char::MAX)
        .filter(|&c| columns::selectable(c))
        .collect();
    assert!(all.len() > 200, "{}", all.len());
    for c in all {
        let text = format!("{c}\u{fe0f}");
        assert_eq!(width(text.as_bytes(), 4), 2, "{text:?}");
    }
}

/// Printed lines keep every byte: a change that only asks for a
/// character's emoji form shows, in full, wrapped and in a free line of
/// text. Emphasis on the selector alone marks the emoji, one cell.
#[test]
fn a_selector_is_written_and_its_change_emphasized() {
    for layout in [
        layout(false, None, false),
        layout(false, Some(80), false),
        layout(false, Some(80), true),
        layout(true, Some(80), false),
    ] {
        let printed = render("warn \u{26a0} foo\n", "warn \u{26a0}\u{fe0f} foo\n", layout);
        assert_eq!(printed.matches('\u{26a0}').count(), 2, "{printed}");
        assert_eq!(printed.matches('\u{fe0f}').count(), 1, "{printed}");
    }
    let mut c = Canvas::default();
    layout(false, Some(80), false).text_line(Role::Plain, "1\u{fe0f}\u{20e3}".as_bytes(), &mut c);
    assert_eq!(c.markup(), "1\u{fe0f}\u{20e3}\n");
    assert_eq!(escape("\u{26a0}\u{fe0f}".as_bytes()), "\u{26a0}\u{fe0f}");
    let line = Line {
        state: LineState::new(Kind::Added, false, Unchanged::No),
        old: None,
        new: Some(1),
        text: "\u{26a0}\u{fe0f}x".as_bytes(),
        emphasis: std::slice::from_ref(&(3..6)),
        syntax: None,
        paint: &[],
        block_start: false,
    };
    let mut c = Canvas::default();
    unified::line(&layout(false, Some(40), false), 1, &line, &mut c);
    let emphasized = format!("<{}>\u{26a0}\u{fe0f}</>", Role::AddedEmph.key());
    assert!(c.markup().contains(&emphasized), "{}", c.markup());
}

/// Scrolled sideways, cells start after the `…` with the first glyph
/// still shown; wrapped, the next row goes on from the byte the last left.
#[test]
fn mapped_cells_follow_scroll_and_wrapping() {
    let scrolled = Layout {
        one_sided: Some(Side::New),
        scroll: 2,
        ..layout(false, Some(40), false)
    };
    insta::assert_snapshot!(traced(scrolled, &[(None, Some("abcdef"))]), @r"
    0:5+1 1 2..3
    0:6+1 1 3..4
    0:7+1 1 4..5
    0:8+1 1 5..6
    area 0:4 1
    ");
    let wrapped = Layout {
        one_sided: Some(Side::New),
        ..layout(false, Some(8), true)
    };
    insta::assert_snapshot!(traced(wrapped, &[(None, Some("abcde"))]), @r"
    0:4+1 1 0..1
    0:5+1 1 1..2
    0:6+1 1 2..3
    0:7+1 1 3..4
    1:4+1 1 4..5
    area 0:4 1
    area 1:4 1
    ");
}

/// In split view each half's glyphs carry their side, so a selection can
/// keep to the side it started on.
#[test]
fn mapped_cells_in_split_view_know_their_side() {
    insta::assert_snapshot!(traced(layout(true, Some(41), false), &[(Some("ab"), Some("cd"))]), @r"
    0:4+1 0 0..1
    0:5+1 0 1..2
    0:25+1 1 0..1
    0:26+1 1 1..2
    area 0:4 0
    area 0:25 1
    ");
}

/// Which cells of a background-tinted rendering are tinted: `N` the
/// number cell's tint, `|` the rule (`S` in the gutter's tint), `L` the
/// line's, `E` a changed token's, `.` untinted; `~` after a row that's
/// tinted to the pane's edge.
fn tinted_cells(c: &Canvas) -> String {
    let mut out = String::new();
    for (i, row) in c.rows().enumerate() {
        for (ink, text) in row {
            for ch in text.chars() {
                out.push(match (ink.role, ch) {
                    (Role::AddedNumber | Role::RemovedNumber, '│') => 'S',
                    (Role::AddedNumber | Role::RemovedNumber, _) => 'N',
                    (Role::AddedLine | Role::RemovedLine, _) => 'L',
                    (Role::AddedEmph | Role::RemovedEmph, _) => 'E',
                    (_, '│') => '|',
                    _ => '.',
                });
            }
        }
        if c.fill(i).is_some() {
            out.push('~');
        }
        out.push('\n');
    }
    out
}

/// Under background tints both sides of a split row tint the same
/// columns: the number cell with its padding, the rule in the gutter's
/// tint, the code to the edge. A wrapped line's next rows keep the
/// gutter's tint; the empty side of a row stays untinted; context rows
/// keep the rule too.
#[test]
fn background_tints_cover_the_same_cells_on_both_sides() {
    let layout = Layout {
        indicators: Indicators::Background,
        ..layout(true, Some(41), true)
    };
    let line = |kind, old, new, text: &'static str| Line {
        state: LineState::new(kind, false, Unchanged::No),
        old,
        new,
        text: text.as_bytes(),
        emphasis: &[],
        syntax: None,
        paint: &[],
        block_start: false,
    };
    let mut c = Canvas::default();
    let context = line(Kind::Context, Some(1), Some(1), "ctx");
    let removed = line(
        Kind::Removed,
        Some(2),
        None,
        "a removed line long enough to wrap",
    );
    let added = line(Kind::Added, None, Some(2), "short");
    let extra = line(Kind::Added, None, Some(3), "only new");
    split::row(&layout, 1, [Some(&context), Some(&context)], &mut c);
    split::row(&layout, 1, [Some(&removed), Some(&added)], &mut c);
    split::row(&layout, 1, [None, Some(&extra)], &mut c);
    insta::assert_snapshot!(tinted_cells(&c), @r"
    ...|................|...|....
    NNNSLLLLLLLLLLLLLLLL|NNNSLLLLLL~
    NNNSLLLLLLLLLLLLLLLL|NNNSL~
    NNNSLLLLLLLLLLLLLLLL|NNNSL~
    ...|................|NNNSLLLLLLLLL~
    ");
}

#[test]
fn line_numbers_and_hunk_numbers_can_be_left_out() {
    let layout = Layout {
        line_numbers: false,
        hunk_headers: false,
        ..layout(false, None, false)
    };
    insta::assert_snapshot!(render(OLD, NEW, layout), @r#"
    <dim>fn main()</>
    <dim> │</> fn main() {
    <dim> │</><modified>-    let total = </><removed-emph>count</><modified> + 1;</>
    <dim> │</><modified>+    let total = </><added-emph>items.len()</><modified> + 1;</>
    <dim> │</>     println!("{total}");
    <dim> │</><added>+    eprintln!("done");</>
    <dim> │</> }
    "#);
}

/// In file view, in every indicators style: a changed line of the side
/// read carries a heavy bar for its rule in its sign's color, an unchanged
/// line none, and each run of the other side's unpaired lines is a rule
/// across the pane in the hidden side's color that counts them and names
/// the key that opens them, or closes them once open. In an opened change
/// the other side's lines have no bar. The number shown is the side read's.
#[test]
fn file_view_marks_changes_and_hidden_runs_in_every_style() {
    use std::fmt::Write as _;
    let header = HunkHeader {
        old_start: 3,
        old_len: 1,
        new_start: 4,
        new_len: 1,
        heading: b"",
        hidden: 0,
    };
    let kept = Line {
        state: LineState::Context,
        old: Some(3),
        new: Some(4),
        text: b"kept",
        emphasis: &[],
        syntax: None,
        paint: &[],
        block_start: false,
    };
    // The line an edit was made into, on the new side.
    let edited = Line {
        state: LineState::AddedPaired,
        old: None,
        new: Some(5),
        text: b"edited",
        ..kept
    };
    // The line it was edited from, as an opened change shows it.
    let was = Line {
        state: LineState::RemovedPaired,
        old: Some(5),
        new: None,
        text: b"was",
        ..kept
    };
    let lines = [kept, edited, was];
    let mut out = String::new();
    for indicators in [
        Indicators::Background,
        Indicators::Bars,
        Indicators::Classic,
        Indicators::None,
    ] {
        for side in [Side::New, Side::Old] {
            let layout = Layout {
                indicators,
                inline_signs: false,
                one_sided: Some(side),
                read_alone: true,
                ..layout(false, Some(40), false)
            };
            let mut c = Canvas::default();
            layout.hidden_run(2, false, Some("x"), &mut c);
            for row in [
                view::Row::One {
                    line: 0,
                    paired: None,
                },
                view::Row::One {
                    line: 1,
                    paired: Some(2),
                },
                view::Row::Hidden { from: 2, n: 1 },
            ] {
                layout.hunk_row(&header, row, |i| lines[i], &mut c);
            }
            layout.hidden_run(1, true, None, &mut c);
            layout.hunk_row(&header, view::Row::Line(2), |i| lines[i], &mut c);
            let _ = writeln!(out, "{indicators:?} {side:?}");
            out += &c.markup();
        }
    }
    insta::assert_snapshot!(out);
}

/// In file view, the row above a change at a file's first line shows the
/// hunk's heading, or nothing: no `⋯`, since nothing is hidden there.
#[test]
fn file_view_start_row_is_its_heading_or_blank() {
    let header = |heading: &'static [u8]| HunkHeader {
        old_start: 1,
        old_len: 2,
        new_start: 1,
        new_len: 2,
        heading,
        hidden: 0,
    };
    let layout = Layout {
        one_sided: Some(Side::New),
        hunk_headers: false,
        ..layout(false, Some(40), false)
    };
    let mut c = Canvas::default();
    layout.read_start(&header(b""), &mut c);
    layout.read_start(&header(b"fn main()"), &mut c);
    assert_eq!(c.markup(), "\n<dim>fn main()</>\n");
}

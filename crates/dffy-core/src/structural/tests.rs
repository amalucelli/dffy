use proptest::prelude::*;

use super::*;
use crate::diff::{Options, hunks};

fn texts(old: &str, new: &str) -> (Text, Text) {
    (Text::new(old.into()), Text::new(new.into()))
}

fn line_hunks(old: &Text, new: &Text) -> Vec<Hunk> {
    crate::diff::hunks_without_emphasis(old, new, &Options::default())
}

fn structural(path: &str, old: &Text, new: &Text) -> Structural {
    diff(path.as_bytes(), old, new, &line_hunks(old, new)).expect("structural diff")
}

/// Rebuilds the new text from the old one and the hunks, checking that
/// each side's lines come in order and the headers add up.
fn apply(old: &Text, new: &Text, hunks: &[Hunk]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut o = 0;
    for h in hunks {
        assert!(h.old_start >= o, "hunks ordered and disjoint");
        while o < h.old_start {
            out.extend_from_slice(old.line(o));
            o += 1;
        }
        let mut n = h.new_start;
        for line in &h.lines {
            match line.kind {
                LineKind::Context => {
                    assert_eq!((line.old, line.new), (o, n));
                    assert_eq!(old.line(o), new.line(n));
                    out.extend_from_slice(new.line(n));
                    o += 1;
                    n += 1;
                }
                LineKind::Removed => {
                    assert_eq!((line.old, line.new), (o, n));
                    o += 1;
                }
                LineKind::Added => {
                    assert_eq!((line.old, line.new), (o, n));
                    out.extend_from_slice(new.line(n));
                    n += 1;
                }
            }
        }
        assert_eq!(
            (o - h.old_start, n - h.new_start),
            (h.old_len, h.new_len),
            "header lengths"
        );
    }
    while o < old.len() {
        out.extend_from_slice(old.line(o));
        o += 1;
    }
    out
}

/// The changed lines as `-`/`+`, or `~` when reformatted and `»` when
/// moved, with emphasis in brackets and `|` where split view starts a new
/// block.
fn show(old: &Text, new: &Text, hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for line in hunks.iter().flat_map(|h| &h.lines) {
        let (sign, text) = match line.kind {
            LineKind::Context => continue,
            LineKind::Removed => ('-', old.line(line.old)),
            LineKind::Added => ('+', new.line(line.new)),
        };
        if line.block_start {
            out.push_str("|\n");
        }
        out.push(match line.unchanged {
            Unchanged::No => sign,
            Unchanged::Layout => '~',
            Unchanged::Moved => '»',
        });
        let text = String::from_utf8_lossy(text.strip_suffix(b"\n").unwrap_or(text));
        let mut at = 0;
        for r in &line.emphasis {
            out.push_str(&text[at..r.start]);
            out.push('[');
            out.push_str(&text[r.clone()]);
            out.push(']');
            at = r.end;
        }
        out.push_str(&text[at..]);
        out.push('\n');
    }
    out
}

/// Changed lines shown as changes (not flagged unchanged), and emphasized
/// tokens (words and punctuation, not whitespace).
fn counts(old: &Text, new: &Text, hunks: &[Hunk]) -> (usize, usize) {
    let (mut lines, mut tokens) = (0, 0);
    for line in hunks.iter().flat_map(|h| &h.lines) {
        let text = match line.kind {
            LineKind::Context => continue,
            LineKind::Removed => old.line(line.old),
            LineKind::Added => new.line(line.new),
        };
        lines += usize::from(line.unchanged == Unchanged::No);
        tokens += crate::diff::tokenize(text)
            .into_iter()
            .filter(|t| !text[t.clone()].iter().all(u8::is_ascii_whitespace))
            .filter(|t| {
                line.emphasis
                    .iter()
                    .any(|e| e.start <= t.start && t.end <= e.end)
            })
            .count();
    }
    (lines, tokens)
}

fn check(path: &str, old: &str, new: &str) -> Structural {
    let (old, new) = texts(old, new);
    let s = structural(path, &old, &new);
    assert_eq!(apply(&old, &new, &s.hunks), new.bytes(), "{path}");
    s
}

#[test]
fn hunks_reproduce_new_in_every_fixture_language() {
    let fixtures = [
        (
            "a.c",
            "int f(int a)\n{\n\treturn a + 1;\n}\n\nint g(void) { return 2; }\n",
            "int g(void) { return 2; }\n\nint f(int a, int b)\n{\n\treturn a + b;\n}\n",
        ),
        (
            "a.rs",
            "fn main() {\n    let x = 1;\n    println!(\"{x}\");\n}\n",
            "fn main() {\n    let y = 1;\n    if y > 0 {\n        println!(\"{y}\");\n    }\n}\n",
        ),
        (
            "a.py",
            "def f(a, b):\n    return a + b\n\nclass C:\n    pass\n",
            "class C:\n    x = 1\n\ndef f(b, a):\n    return a + b\n",
        ),
        (
            "a.js",
            "function f(a) {\n  return [a, 1, 2];\n}\nconst x = f(3);\n",
            "const x = f(3);\nfunction f(a) {\n  return [1, 2, a];\n}\n",
        ),
        (
            "a.ts",
            "interface P { x: number; y: number }\nconst p: P = { x: 1, y: 2 };\n",
            "interface P {\n  x: number;\n  y: number;\n  z?: string;\n}\nconst p: P = { y: 2, x: 1 };\n",
        ),
        (
            "a.json",
            "{\"a\": [1, 2, 3], \"b\": {\"c\": true}}\n",
            "{\n  \"b\": {\"c\": false},\n  \"a\": [1, 2, 3, 4]\n}\n",
        ),
        // Syntax errors on both sides.
        (
            "broken.c",
            "int f( {\n\treturn 1 +;\n}\nint ok(void) { return 0; }\n",
            "int f( {\n\treturn 2 +;\n}\n}}}\nint ok(void) { return 1; }\n",
        ),
        (
            "broken.rs",
            "fn f() { let = ; }\nfn g() {}\n",
            "fn g() {}\nfn f() { let x = ; }\n",
        ),
        ("no-newline.c", "int a;\nint b;", "int b;\nint a;"),
    ];
    for (path, old, new) in fixtures {
        check(path, old, new);
    }
}

#[test]
fn unknown_languages_and_oversized_sides_are_unavailable() {
    let (old, new) = texts("a\n", "b\n");
    let h = line_hunks(&old, &new);
    assert_eq!(
        diff(b"notes.unknown", &old, &new, &h).err(),
        Some(Unavailable::Language)
    );
    let big = Text::new(vec![b'x'; MAX_BYTES + 1]);
    assert_eq!(
        diff(b"big.c", &old, &big, &h).err(),
        Some(Unavailable::TooLarge)
    );
}

#[test]
fn moved_functions_are_marked_and_swapped_arguments_emphasized() {
    let old = "static int add(int a, int b)\n{\n\treturn a + b;\n}\n\n\
               static int sub(int a, int b)\n{\n\treturn a - b;\n}\n\n\
               int main(void)\n{\n\treturn add(1, 2) + sub(3, 4);\n}\n";
    let new = "static int sub(int a, int b)\n{\n\treturn a - b;\n}\n\n\
               static int add(int a, int b)\n{\n\treturn a + b;\n}\n\n\
               int main(void)\n{\n\treturn add(2, 1) + sub(4, 3);\n}\n";
    let s = check("a.c", old, new);
    let (old, new) = texts(old, new);
    // A whole function that moved is marked so; arguments that swapped
    // places change what the call does, so they're emphasized, not the
    // commas between them.
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    »static int add(int a, int b)
    »{
    »	return a + b;
    »}
    »
    »static int add(int a, int b)
    »{
    »	return a + b;
    »}
    »
    -	return add([1], [2]) + sub([3], [4]);
    +	return add([2], [1]) + sub([4], [3]);
    ");
    let line = hunks(&old, &new, &Options::default());
    assert_eq!(counts(&old, &new, &line), (12, 8));
    assert_eq!(counts(&old, &new, &s.hunks), (2, 8));
}

#[test]
fn reordered_parameters_elements_and_fields_are_changes_not_moves() {
    // Each parameter, element or field is a whole line that moved, but its
    // order is part of the code: those lines show as changes. A statement
    // reordered in its block is marked moved.
    let fixtures = [
        (
            "a.c",
            "struct s {\n\tint a;\n\tint b;\n\tint c;\n};\n",
            "struct s {\n\tint b;\n\tint a;\n\tint c;\n};\n",
        ),
        (
            "a.c",
            "int f(\n\tint a,\n\tint b,\n\tint c\n);\n",
            "int f(\n\tint b,\n\tint a,\n\tint c\n);\n",
        ),
        (
            "a.c",
            "int t[] = {\n\tfirst,\n\tsecond,\n\tthird,\n};\n",
            "int t[] = {\n\tsecond,\n\tfirst,\n\tthird,\n};\n",
        ),
        (
            "a.rs",
            "struct P {\n    x: u32,\n    y: u32,\n    z: u32,\n}\n",
            "struct P {\n    y: u32,\n    x: u32,\n    z: u32,\n}\n",
        ),
    ];
    for (path, old, new) in fixtures {
        let s = check(path, old, new);
        let (o, n) = texts(old, new);
        assert!(
            s.hunks
                .iter()
                .flat_map(|h| &h.lines)
                .filter(|l| l.kind != LineKind::Context)
                .all(|l| l.unchanged == Unchanged::No),
            "{path}\n{}",
            show(&o, &n, &s.hunks)
        );
    }
    let old = "void f(void)\n{\n\tprepare();\n\tcheck(a);\n\tfinish();\n}\n";
    let new = "void f(void)\n{\n\tcheck(a);\n\tprepare();\n\tfinish();\n}\n";
    let s = check("a.c", old, new);
    let (o, n) = texts(old, new);
    insta::assert_snapshot!(show(&o, &n, &s.hunks), @r"
    »	prepare();
    »	prepare();
    ");
}

#[test]
fn reformatting_is_unchanged() {
    let old =
        "fn area(w: u32, h: u32) -> u32 { w * h }\n\nfn main() {\n    let a = area(2, 3);\n}\n";
    let new = "fn area(\n    w: u32,\n    h: u32,\n) -> u32 {\n    w * h\n}\n\nfn main() {\n\tlet a = area(2,\n\t\t3);\n}\n";
    let s = check("a.rs", old, new);
    let (old, new) = texts(old, new);
    // Only the trailing comma is new code. The old one-line function isn't
    // dimmed, though: the comma landed in the middle of it.
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    -fn area(w: u32, h: u32) -> u32 { w * h }
    ~fn area(
    ~    w: u32,
    +    h: u32[,]
    ~) -> u32 {
    ~    w * h
    ~}
    ~    let a = area(2, 3);
    ~	let a = area(2,
    ~		3);
    ");
    let line = hunks(&old, &new, &Options::default());
    assert_eq!(counts(&old, &new, &line), (10, 3));
    assert_eq!(counts(&old, &new, &s.hunks), (2, 1));
}

#[test]
fn reindenting_is_a_change_where_indentation_is_syntax() {
    // Same tokens in the same order, but `b(2)` left the `if`.
    let old = "def f(x):\n    if x:\n        a(1)\n        b(2)\n";
    let new = "def f(x):\n    if x:\n        a(1)\n    b(2)\n";
    let s = check("a.py", old, new);
    let (o, n) = texts(old, new);
    insta::assert_snapshot!(show(&o, &n, &s.hunks), @r"
    -        b(2)
    +    b(2)
    ");
}

#[test]
fn a_renamed_identifier_is_the_only_emphasis() {
    let old =
        "fn total(items: &[u32]) -> u32 {\n    let count = items.len() as u32;\n    count + 1\n}\n";
    let new =
        "fn total(items: &[u32]) -> u32 {\n    let size = items.len() as u32;\n    size + 1\n}\n";
    let s = check("a.rs", old, new);
    let (old, new) = texts(old, new);
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    -    let [count] = items.len() as u32;
    -    [count] + 1
    +    let [size] = items.len() as u32;
    +    [size] + 1
    ");
    // The line diff can't pair `count + 1` with `size + 1`: too little of
    // the line is shared.
    let line = hunks(&old, &new, &Options::default());
    assert_eq!(counts(&old, &new, &line), (4, 2));
    assert_eq!(counts(&old, &new, &s.hunks), (4, 4));
}

#[test]
fn deep_nesting_falls_back_instead_of_recursing() {
    let nest = |inner: &str| format!("int x = {}{inner}{};\n", "(".repeat(1000), ")".repeat(1000));
    let (old, new) = (nest("1"), nest("2"));
    let s = check("deep.c", &old, &new);
    assert_eq!(s.regions.fell_back, 1);
}

#[test]
fn an_added_wrapper_dims_the_code_it_wraps() {
    let old = "void f(void)\n{\n\tprepare();\n\tfoo(x, y);\n\tdone();\n}\n";
    let new = "void f(void)\n{\n\tprepare();\n\tif (ready) {\n\t\tfoo(x, y);\n\t}\n\tdone();\n}\n";
    let s = check("a.c", old, new);
    let (old, new) = texts(old, new);
    // The wrapper's lines are all new, so they need no emphasis; the call
    // inside it only moved.
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    +	if (ready) {
    |
    ~	foo(x, y);
    ~		foo(x, y);
    +	}
    ");
    let line = hunks(&old, &new, &Options::default());
    assert_eq!(counts(&old, &new, &line), (4, 0));
    assert_eq!(counts(&old, &new, &s.hunks), (2, 0));
}

#[test]
fn rewritten_statements_get_no_scattered_emphasis() {
    let old = "int f(void)\n{\n\tx = foo(a);\n\ty = 1;\n}\n";
    let new = "int f(void)\n{\n\treturn bar(a, b);\n\ty = 1;\n}\n";
    let s = check("a.c", old, new);
    let (old, new) = texts(old, new);
    // Different statements: `(`, `a`, `)` and `;` don't match one by one.
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    -	x = foo(a);
    +	return bar(a, b);
    ");
}

#[test]
fn code_tokenized_differently_by_error_recovery_still_matches() {
    // Inside the unclosed `f(` the macro body `(x)` is three tokens; after
    // re-indenting, the parser recovers before the `#define` and the body
    // is one token.
    let old = "int f( {\n}\n/* a comment */\n#define M(x) (x)\nint a = 1;\n";
    let new = "int f( {\n}\n /* a comment */\n  #define M(x) (x)\nint a = 1;\n";
    let s = check("a.c", old, new);
    let (o, n) = texts(old, new);
    insta::assert_snapshot!(show(&o, &n, &s.hunks), @r"
    ~/* a comment */
    ~#define M(x) (x)
    ~ /* a comment */
    ~  #define M(x) (x)
    ");
    // A real change to the body still shows, the whole token on the side
    // where it is one.
    let new = new.replace("(x)\n", "(y)\n");
    let s = check("a.c", old, &new);
    let (o, n) = texts(old, &new);
    insta::assert_snapshot!(show(&o, &n, &s.hunks), @r"
    ~/* a comment */
    -#define M(x) [(x)]
    ~ /* a comment */
    +  #define M(x) [(y)]
    ");
    // Operators made of the same characters are still different tokens.
    let (old, new) = ("int f( {\nx = i++ + j;\n}\n", "int f( {\nx = i + ++j;\n}\n");
    let s = check("a.c", old, new);
    let (o, n) = texts(old, new);
    insta::assert_snapshot!(show(&o, &n, &s.hunks), @r"
    -x = i[++] + j;
    +x = i + [++]j;
    ");
}

#[test]
fn parse_errors_match_in_order_with_the_rest_of_the_region() {
    // The old side is one parse error; on the new side only the last two
    // lines are. Diffed against that error alone, the old first `if (x)`
    // would take the tokens of the last one, and the declarations between
    // would be out of order.
    let old = "/* a comment */\nif (x)\n{\n}\nint  a = 1;\nint a = 1;\n\
               #define M(x) (x)\nif (x) {\nint f( {\n";
    let new = "/* a comment */\n if (x)\n{\n}\n int  a = 1;\n int a = 1;\n\
               #define M(x) (x)\n if (x) {\nint f( {\n";
    let s = check("a.c", old, new);
    let (o, n) = texts(old, new);
    assert!(reformatted(&s), "{}", show(&o, &n, &s.hunks));
}

#[test]
fn whitespace_changed_inside_a_multi_line_string_is_a_change() {
    let old = "s = \"\"\"a\n\nb\"\"\"\n";
    let new = "s = \"\"\"a\n   \nb\"\"\"\n";
    let s = check("a.py", old, new);
    let (o, n) = texts(old, new);
    let changed = s.hunks.iter().flat_map(|h| &h.lines);
    assert!(
        changed.clone().any(|l| l.kind != LineKind::Context)
            && changed.clone().all(|l| l.unchanged == Unchanged::No),
        "{}",
        show(&o, &n, &s.hunks)
    );
}

/// Whether every changed line is flagged as reformatted.
fn reformatted(s: &Structural) -> bool {
    s.hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != LineKind::Context)
        .all(|l| l.unchanged == Unchanged::Layout)
}

// The property test found this one: with every line re-indented none is
// kept, and error recovery leaves an empty `;` statement on each side, in
// different places. As anchors, or paired by kind, the two would split the
// struct from its counterpart.
#[test]
fn every_line_reindented_around_a_parse_error_stays_unchanged() {
    let ids = [11, 9, 15, 13, 8, 1, 2, 10, 4, 0, 3, 7, 12, 14, 16, 6];
    let old = source(&ids);
    let mut new = String::new();
    for line in old.lines() {
        new.push(' ');
        new.push_str(&line.replace(", ", ",  "));
        new.push('\n');
    }
    let (old, new) = texts(&old, &new);
    let s = structural("a.c", &old, &new);
    assert_eq!(apply(&old, &new, &s.hunks), new.bytes());
    assert!(reformatted(&s), "{}", show(&old, &new, &s.hunks));
}

#[test]
fn code_on_kept_lines_only_matches_code_on_kept_lines() {
    // Error recovery leaves an empty statement on each side: the `;` of the
    // kept `while` line on the old side, the `;` after the re-indented
    // struct on the new side. Matched to each other, they'd split the
    // struct from its counterpart.
    let old = "int f( {\nx = \"s t\";\n#define M(x) (x)\nf(b, a);\n}\n\
               while (n--) g(n);\n/*  a\n comment */\nstruct s { int a; int b; };\n";
    let new = "int f( {\nx = \"s t\";\n#define M(x) (x)\n  f(b,  a);\n  }\n\
               while (n--) g(n);\n/*  a\n comment */\n struct s { int a; int b; };\n";
    let s = check("a.c", old, new);
    let (o, n) = texts(old, new);
    assert!(reformatted(&s), "{}", show(&o, &n, &s.hunks));
}

#[test]
fn code_repeated_inside_parse_errors_matches_its_own_copy() {
    // Each side has one copy in a parse error and the other parsed fine.
    let fixtures = [
        // Two equal comments, the other way around on each side.
        (
            "int f( {\n\nstruct s { int a; int b; };\n}\n/* a comment */\n\
             #define M(x) (x)\nf(a, b);\n/*  a\n comment */\nif (x)\n{\n",
            "int f( {\n \n struct s { int a; int b; };\n }\n  /* a comment */\n\
             #define M(x) (x)\nf(a,  b);\n/*  a\n  comment */\nif (x)\n{\n",
        ),
        // Two equal declarations: only the last one parses on the old side,
        // only the first on the new.
        (
            "int f( {\n/*  a\n comment */\nint a = 1;\n}\nwhile (n--) g(n);\n\tint a=1;\n",
            "int f( {\n/*  a\n comment */\n int a = 1;\n }\nwhile (n--) g(n);\n \tint a=1;\n",
        ),
    ];
    for (old, new) in fixtures {
        let s = check("a.c", old, new);
        let (o, n) = texts(old, new);
        assert!(reformatted(&s), "{}", show(&o, &n, &s.hunks));
    }
}

#[test]
fn matched_lines_share_a_row_after_unmatched_ones() {
    let old = "int f(void)\n{\n\tint unrelated_old = 0;\n\treturn compute(a, b);\n}\n";
    let new = "int f(void)\n{\n\treturn compute(a, c);\n}\n";
    let s = check("a.c", old, new);
    let (old, new) = texts(old, new);
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    -	int unrelated_old = 0;
    |
    -	return compute(a, [b]);
    +	return compute(a, [c]);
    ");
}

#[test]
fn a_region_over_budget_falls_back_alone() {
    let terms: Vec<String> = (0..40).map(|i| format!("a{i}")).collect();
    let sum = terms.join(" + ");
    let padding = "int pad;\n".repeat(8);
    // The change sits at the bottom of a left-leaning tree of sums: finding
    // it takes a visit to every level.
    let old =
        format!("int f(void) {{ return 2 + {sum}; }}\n{padding}int g(void) {{ return x + 3; }}\n");
    let new =
        format!("int f(void) {{ return 5 + {sum}; }}\n{padding}int g(void) {{ return x + 4; }}\n");
    let (old, new) = texts(&old, &new);
    let line = hunks(&old, &new, &Options::default());
    let budget = Budget {
        region_work: 150,
        ..Budget::DEFAULT
    };
    let s = diff_with(b"a.c", &old, &new, &line_hunks(&old, &new), budget).expect("diff");
    assert_eq!(apply(&old, &new, &s.hunks), new.bytes());
    assert_eq!((s.regions.matched, s.regions.fell_back), (1, 1));
    assert!(s.note().is_some());
    // The large function keeps the line diff's token emphasis; the small
    // one gets its structural emphasis.
    assert_eq!(s.hunks[0].lines, line[0].lines);
    insta::assert_snapshot!(show(&old, &new, &s.hunks[1..]), @r"
    -int g(void) { return x + [3]; }
    +int g(void) { return x + [4]; }
    ");

    // Past the file's deadline every region falls back.
    let none = Budget {
        file_time: Duration::ZERO,
        ..Budget::DEFAULT
    };
    let s = diff_with(b"a.c", &old, &new, &line_hunks(&old, &new), none).expect("diff");
    assert_eq!(s.regions.fell_back, 2);
    assert_eq!(s.hunks, line);
}

/// C-ish lines, some differing only in whitespace, for random edits.
const POOL: &[&str] = &[
    "int a = 1;\n",
    "int  a = 1;\n",
    "\tint a=1;\n",
    "f(a, b);\n",
    "f(b, a);\n",
    "if (x) {\n",
    "if (x)\n{\n",
    "}\n",
    "\n",
    "/* a comment */\n",
    "/*  a\n comment */\n",
    "return a + b;\n",
    "while (n--) g(n);\n",
    "x = \"s t\";\n",
    "#define M(x) (x)\n",
    "int f( {\n",
    "struct s { int a; int b; };\n",
];

fn source(ids: &[usize]) -> String {
    ids.iter().map(|&i| POOL[i]).collect()
}

fn edited() -> impl Strategy<Value = (Vec<usize>, Vec<usize>)> {
    let old = prop::collection::vec(0..POOL.len(), 0..40);
    (old, prop::collection::vec((0..4u8, 0..POOL.len()), 0..12)).prop_map(|(old, edits)| {
        let mut new = old.clone();
        for (k, (op, v)) in edits.into_iter().enumerate() {
            let at = if new.is_empty() {
                0
            } else {
                (k * 7919 + v) % new.len()
            };
            match op {
                0 => new.insert(at, v),
                1 if !new.is_empty() => {
                    new.remove(at);
                }
                2 if !new.is_empty() => new[at] = v,
                _ => new.push(v),
            }
        }
        (old, new)
    })
}

fn solid_bytes(text: &Text) -> Vec<u8> {
    let mut bytes: Vec<u8> = text
        .bytes()
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    bytes.sort_unstable();
    bytes
}

/// The tokens of `text` besides whitespace, as the line diff cuts them.
fn words(text: &[u8]) -> Vec<&[u8]> {
    crate::diff::tokenize(text)
        .into_iter()
        .map(|r| &text[r])
        .filter(|t| !t.iter().all(u8::is_ascii_whitespace))
        .collect()
}

/// Checks what the unchanged flags claim against the texts alone, without
/// the matching that set them:
/// - a reformatted line's tokens appear in one piece among the other side's
///   changed lines of its block, and those pieces come in the order of the
///   lines;
/// - a moved line has a moved line with the same tokens on the other side;
/// - neither is emphasized.
fn check_claims(old: &Text, new: &Text, hunks: &[Hunk]) -> Result<(), String> {
    let text = |l: &Line| match l.kind {
        LineKind::Removed => old.line(l.old),
        _ => new.line(l.new),
    };
    let changed: Vec<&Line> = hunks
        .iter()
        .flat_map(|h| &h.lines)
        .filter(|l| l.kind != LineKind::Context)
        .collect();
    for l in &changed {
        if l.unchanged != Unchanged::No && !l.emphasis.is_empty() {
            return Err(format!("emphasis on an unchanged line {l:?}"));
        }
        if l.unchanged == Unchanged::Moved
            && !words(text(l)).is_empty()
            && !changed.iter().any(|t| {
                t.kind != l.kind
                    && t.unchanged == Unchanged::Moved
                    && words(text(t)) == words(text(l))
            })
        {
            return Err(format!("moved line without a twin {l:?}"));
        }
    }
    for h in hunks {
        for block in h.lines.split(|l| l.kind == LineKind::Context) {
            for side in [LineKind::Removed, LineKind::Added] {
                let other: Vec<&[u8]> = block
                    .iter()
                    .filter(|l| l.kind != side)
                    .flat_map(|l| words(text(l)))
                    .collect();
                let mut at = 0;
                for l in block
                    .iter()
                    .filter(|l| l.kind == side && l.unchanged == Unchanged::Layout)
                {
                    let mine = words(text(l));
                    if mine.is_empty() {
                        continue;
                    }
                    let Some(k) = (at..=other.len().saturating_sub(mine.len()))
                        .find(|&k| other.get(k..k + mine.len()) == Some(&mine[..]))
                    else {
                        return Err(format!("reformatted line not found in order {l:?}"));
                    };
                    at = k + mine.len();
                }
            }
        }
    }
    Ok(())
}

#[test]
fn claims_check_rejects_a_dimmed_reorder() {
    let (old, new) = texts("add(1, 2);\n", "add(2, 1);\n");
    let mut h = line_hunks(&old, &new);
    for l in &mut h[0].lines {
        l.unchanged = Unchanged::Layout;
    }
    // Both sides hold the same bytes, so a byte count can't tell.
    assert_eq!(solid_bytes(&old), solid_bytes(&new));
    assert!(check_claims(&old, &new, &h).is_err());
    let s = structural("a.c", &old, &new);
    check_claims(&old, &new, &s.hunks).expect("claims hold");
    insta::assert_snapshot!(show(&old, &new, &s.hunks), @r"
    -add([1], [2]);
    +add([2], [1]);
    ");
}

proptest! {
    #[test]
    fn structural_hunks_reproduce_new((o, n) in edited(), lang in 0..3usize) {
        let path = ["a.c", "a.rs", "a.js"][lang];
        let (old, new) = texts(&source(&o), &source(&n));
        let s = structural(path, &old, &new);
        prop_assert_eq!(apply(&old, &new, &s.hunks), new.bytes());
        if let Err(e) = check_claims(&old, &new, &s.hunks) {
            return Err(TestCaseError::fail(format!("{e}\n{}", show(&old, &new, &s.hunks))));
        }
        // If every changed line is flagged, both sides hold the same code.
        let changed = s.hunks.iter().flat_map(|h| &h.lines).filter(|l| l.kind != LineKind::Context);
        if changed.clone().all(|l| l.unchanged != Unchanged::No) {
            prop_assert_eq!(solid_bytes(&old), solid_bytes(&new));
        }
    }

    // Distinct lines: with repeated ones the line diff may keep a different
    // copy than the one that was re-indented, and the tokens left on
    // changed lines then really differ in order.
    #[test]
    fn whitespace_only_edits_are_all_unchanged(
        ids in prop::sample::subsequence((0..POOL.len()).collect::<Vec<_>>(), 1..POOL.len())
            .prop_shuffle(),
        spaces in prop::collection::vec(0..3usize, 1..30),
    ) {
        let old = source(&ids);
        let mut new = String::new();
        for (line, &s) in old.lines().zip(spaces.iter().cycle()) {
            new.push_str(&" ".repeat(s));
            new.push_str(&line.replace(", ", ",  "));
            new.push('\n');
        }
        let (old, new) = texts(&old, &new);
        let s = structural("a.c", &old, &new);
        prop_assert_eq!(apply(&old, &new, &s.hunks), new.bytes());
        prop_assert!(reformatted(&s), "{}", show(&old, &new, &s.hunks));
    }

    /// Lines of every length, short and longer than a bucket, with and
    /// without a final newline; the end of the text is past the last line.
    #[test]
    fn line_index_finds_the_line_holding_each_byte(
        lines in prop::collection::vec(0..150usize, 0..40),
        last_newline: bool,
    ) {
        let mut bytes: Vec<u8> = lines.iter().flat_map(|&n| [vec![b'x'; n], vec![b'\n']]).flatten().collect();
        if !last_newline {
            bytes.pop();
        }
        let text = Text::new(bytes);
        let index = LineIndex::new(&text);
        for offset in 0..=text.bytes().len() {
            let holding = (0..=text.len()).rev().find(|&i| text.offset(i) <= offset);
            prop_assert_eq!(Some(index.line_of(offset)), holding, "offset {}", offset);
        }
    }
}

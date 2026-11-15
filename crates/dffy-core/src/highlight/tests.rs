use std::fmt::Write as _;

use super::*;

/// The text with each run wrapped as `<role>text</>`, line by line.
fn markup(text: &Text, spans: &Spans) -> String {
    let mut out = String::new();
    for i in spans.lines() {
        let line = text.line(i);
        let line = &line[..content_len(line)];
        let mut pos = 0;
        for run in spans.line(i) {
            let (s, e) = (run.start as usize, run.end as usize);
            out.push_str(&String::from_utf8_lossy(&line[pos..s]));
            let _ = write!(
                out,
                "<{}>{}</>",
                run.syntax.key(),
                String::from_utf8_lossy(&line[s..e])
            );
            pos = e;
        }
        out.push_str(&String::from_utf8_lossy(&line[pos..]));
        out.push('\n');
    }
    out
}

fn highlight(lang: Language, source: &str) -> String {
    let text = Text::new(source.into());
    let parsed = parse(lang, text.bytes()).expect("parses");
    markup(&text, &parsed.spans(&text, 0..text.len()))
}

#[test]
fn every_bundled_query_compiles() {
    for lang in Language::ALL {
        assert!(compiled(lang).is_some(), "{}", lang.name());
    }
}

#[test]
fn detects_by_name_extension_and_shebang() {
    let detect = |path: &str, content: &str| Language::detect(path.as_bytes(), content.as_bytes());
    assert_eq!(detect("kernel/sched/fair.c", ""), Some(Language::C));
    assert_eq!(detect("include/linux/sched.h", ""), Some(Language::C));
    assert_eq!(detect("src/lib.rs", ""), Some(Language::Rust));
    assert_eq!(detect("web/App.tsx", ""), Some(Language::Tsx));
    assert_eq!(detect("Cargo.lock", ""), Some(Language::Toml));
    assert_eq!(detect("a/Gemfile", ""), Some(Language::Ruby));
    assert_eq!(detect("scripts/checkpatch", "#!/usr/bin/env perl\n"), None);
    assert_eq!(
        detect("scripts/run", "#!/usr/bin/env -S python3 -u\nprint()\n"),
        Some(Language::Python)
    );
    assert_eq!(detect("tools/build", "#!/bin/sh\n"), Some(Language::Bash));
    assert_eq!(detect("Makefile", "all:\n"), None);
    assert_eq!(detect("README", "text"), None);
}

#[test]
fn rust_roles() {
    insta::assert_snapshot!(highlight(
        Language::Rust,
        "/// Adds.\n#[inline]\npub fn add(a: u32, b: u32) -> u32 {\n    let s = \"x\\n\";\n    Some(a + b).unwrap_or(0) // done\n}\n"
    ), @r#"
    <comment>/// Adds.</>
    <attribute>#</><punctuation>[</><attribute>inline</><punctuation>]</>
    <keyword>pub</> <keyword>fn</> <function>add</><punctuation>(</>a<punctuation>:</> <type>u32</><punctuation>,</> b<punctuation>:</> <type>u32</><punctuation>)</> -> <type>u32</> <punctuation>{</>
        <keyword>let</> s = <string>"x</><escape>\n</><string>"</><punctuation>;</>
        <function>Some</><punctuation>(</>a + b<punctuation>).</><function>unwrap_or</><punctuation>(</><constant>0</><punctuation>)</> <comment>// done</>
    <punctuation>}</>
    "#);
}

#[test]
fn c_roles() {
    insta::assert_snapshot!(highlight(
        Language::C,
        "#include <stdio.h>\n#define MAX 10\nstatic int f(struct task *t)\n{\n\treturn t->prio > MAX ? 0 : printf(\"%d\", 1);\n}\n"
    ), @r#"
    <keyword>#include</> <string><stdio.h></>
    <keyword>#define</> <constant>MAX</> 10
    <keyword>static</> <type>int</> <function>f</>(<keyword>struct</> <type>task</> <operator>*</>t)
    {
    	<keyword>return</> t<operator>-></><property>prio</> <operator>></> <constant>MAX</> ? <number>0</> : <function>printf</>(<string>"%d"</>, <number>1</>)<punctuation>;</>
    }
    "#);
}

#[test]
fn python_and_json_roles() {
    insta::assert_snapshot!(highlight(
        Language::Python,
        "def f(x):\n    return len(x) + 1  # n\n"
    ), @r"
    <keyword>def</> <function>f</>(x):
        <keyword>return</> <function>len</>(x) <operator>+</> <number>1</>  <comment># n</>
    ");
    insta::assert_snapshot!(highlight(Language::Json, "{\"a\": [1, true, null]}\n"), @r#"
    {<string>"a"</>: [<number>1</>, <constant>true</>, <constant>null</>]}
    "#);
}

/// Grammars whose query extends another's, concatenated in order.
#[test]
fn layered_queries() {
    insta::assert_snapshot!(
        highlight(Language::Cpp, "namespace n { class A : public B { virtual int f() const; }; }\n")
            + &highlight(Language::Tsx, "const x: number = <div className=\"a\">{y}</div>;\n"),
        @r#"
    <keyword>namespace</> n { <keyword>class</> <type>A</> : <keyword>public</> <type>B</> { <keyword>virtual</> <type>int</> <function>f</>() <keyword>const</><punctuation>;</> }<punctuation>;</> }
    <keyword>const</> x: <type>number</> <operator>=</> <punctuation><</><tag>div</> <attribute>className</><operator>=</><string>"a"</><punctuation>>{</>y<punctuation>}</</><tag>div</><punctuation>>;</>
    "#
    );
}

const C_FILE: &str = "/* header\n * comment */\n#include <linux/sched.h>\n\nstatic int f(int a)\n{\n\tconst char *s = \"multi\\\n\tline\";\n\tif (a > 0)\n\t\treturn g(a - 1, s);\n\treturn 0;\n}\n\nint main(void) { return f(3); }\n";

#[test]
fn range_highlighting_equals_the_slice_of_the_whole_file() {
    let text = Text::new(C_FILE.into());
    let parsed = parse(Language::C, text.bytes()).expect("parses");
    let full = parsed.spans(&text, 0..text.len());
    for start in 0..text.len() {
        for end in start..=text.len() {
            let part = parsed.spans(&text, start..end);
            assert_eq!(part, full.slice(&text, start..end), "lines {start}..{end}");
            for i in start..end {
                assert_eq!(part.line(i), full.line(i), "line {i} of {start}..{end}");
                assert_eq!(part.tokens(&text, i), full.tokens(&text, i), "line {i}");
            }
        }
    }
}

#[test]
fn leaf_tokens_tile_the_line_and_split_strings_into_words() {
    let text = Text::new(b"\tx->count += foo(\"two words\", 0x1f); // a note\n".to_vec());
    let parsed = parse(Language::C, text.bytes()).expect("parses");
    let spans = parsed.spans(&text, 0..1);
    let line = &text.line(0)[..text.line(0).len() - 1];
    let tokens = spans.tokens(&text, 0);
    let mut pos = 0;
    for t in &tokens {
        assert_eq!(t.start, pos, "{tokens:?}");
        pos = t.end;
    }
    assert_eq!(pos, line.len());
    let words: Vec<&str> = tokens
        .iter()
        .map(|t| std::str::from_utf8(&line[t.clone()]).expect("ascii"))
        .collect();
    assert_eq!(
        words,
        [
            "\t", "x", "->", "count", " ", "+=", " ", "foo", "(", "\"", "two", " ", "words", "\"",
            ",", " ", "0x1f", ")", ";", " ", "/", "/", " ", "a", " ", "note"
        ]
    );
}

/// The emphasized text of each changed line between two one-hunk C files,
/// with plain and with leaf-node tokens.
fn emphasized(old: &str, new: &str) -> (Vec<String>, Vec<String>) {
    use crate::diff::{Options, emphasize, emphasize_with, hunks};
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let hunk = &hunks(&old, &new, &Options::default())[0];
    let spans = |text: &Text| {
        parse(Language::C, text.bytes())
            .expect("parses")
            .spans(text, 0..text.len())
    };
    let (old_spans, new_spans) = (spans(&old), spans(&new));
    let words = |emphasis: Vec<crate::diff::Emphasis>| -> Vec<String> {
        hunk.lines
            .iter()
            .zip(emphasis)
            .flat_map(|(line, ranges)| {
                let text = match line.kind {
                    crate::diff::LineKind::Removed => old.line(line.old),
                    _ => new.line(line.new),
                };
                ranges
                    .into_iter()
                    .map(|r| String::from_utf8_lossy(&text[r]).into_owned())
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    (
        words(emphasize(&old, &new, hunk)),
        words(emphasize_with(
            &old,
            &new,
            hunk,
            Some(&old_spans),
            Some(&new_spans),
        )),
    )
}

#[test]
fn leaf_tokens_emphasize_whole_operators_and_words_in_strings() {
    let mut table = String::new();
    for (old, new) in [
        ("x += 1;\n", "x -= 1;\n"),
        ("if (a <= max) f();\n", "if (a < max) f();\n"),
        ("log(\"too long now\");\n", "log(\"too short now\");\n"),
        ("p->next = q;\n", "p.next = q;\n"),
        ("n = 0x1f;\n", "n = 0x2f;\n"),
    ] {
        let (plain, syntax) = emphasized(old, new);
        let _ = writeln!(table, "{plain:?} -> {syntax:?}");
    }
    // Plain tokens split operators into single bytes, so `<=` to `<`
    // emphasizes only the `=` that went away.
    insta::assert_snapshot!(table, @r#"
    ["+", "-"] -> ["+=", "-="]
    ["="] -> ["<=", "<"]
    ["long", "short"] -> ["long", "short"]
    ["->", "."] -> ["->", "."]
    ["0x1f", "0x2f"] -> ["0x1f", "0x2f"]
    "#);
}

#[test]
fn spans_round_trip_through_their_encoding() {
    let text = Text::new(C_FILE.into());
    let parsed = parse(Language::C, text.bytes()).expect("parses");
    let full = parsed.spans(&text, 0..text.len());
    let bytes = full.encode();
    assert_eq!(Spans::decode(&bytes), Some(full));
    assert_eq!(Spans::decode(&bytes[..bytes.len() - 1]), None);
    let mut extra = bytes;
    extra.push(0);
    assert_eq!(Spans::decode(&extra), None);
}

#[test]
fn oversized_sides_are_not_parsed() {
    let big = vec![b'x'; MAX_HIGHLIGHT_BYTES + 1];
    assert!(parse(Language::C, &big).is_none());
}

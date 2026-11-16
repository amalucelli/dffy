use std::fmt::Write as _;
use std::sync::LazyLock;

use super::*;

static GRAMMARS: LazyLock<Registry> = LazyLock::new(Registry::builtin);

fn lang(name: &str) -> &'static Language {
    GRAMMARS.named(name).expect("a bundled language")
}

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

fn highlight(name: &str, source: &str) -> String {
    let text = Text::new(source.into());
    let parsed = parse(lang(name), text.bytes()).expect("parses");
    markup(&text, &parsed.spans(&text, 0..text.len()))
}

fn detect(path: &str, content: &str) -> Option<&'static str> {
    GRAMMARS
        .detect(path.as_bytes(), content.as_bytes())
        .map(Language::name)
}

#[test]
fn every_bundled_query_compiles() {
    let grammars = Registry::builtin();
    assert_eq!(grammars.languages().len(), 30);
    for lang in grammars.languages() {
        assert!(lang.compiled().is_some(), "{}", lang.name());
    }
}

#[test]
fn prepare_compiles_a_query_once_however_many_threads_ask() {
    let grammars = Registry::builtin();
    let swift = grammars.named("swift").expect("swift");
    let start = std::sync::Barrier::new(4);
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| {
                start.wait();
                swift.prepare();
            });
        }
    });
    swift.prepare();
    assert_eq!(swift.compiles(), 1);
    assert!(swift.is_compiled());
    parse(swift, b"let x = 1\n").expect("parses");
    assert_eq!(swift.compiles(), 1, "the first parse compiled it again");
}

#[test]
fn a_grammar_loads_on_first_use_not_when_matching_files() {
    let grammars = Registry::builtin();
    let rust = grammars.detect(b"src/lib.rs", b"").expect("rust");
    grammars.named("swift").expect("swift");
    grammars.detect(b"run", b"#!/bin/sh\n").expect("bash");
    let loaded = || {
        grammars
            .languages()
            .iter()
            .filter(|l| l.is_loaded())
            .map(Language::name)
            .collect::<Vec<_>>()
    };
    assert!(loaded().is_empty(), "{:?}", loaded());
    rust.prepare();
    assert_eq!(loaded(), ["rust"]);
}

#[test]
fn detects_by_name_extension_and_shebang() {
    assert_eq!(detect("kernel/sched/fair.c", ""), Some("c"));
    assert_eq!(detect("include/linux/sched.h", ""), Some("c"));
    assert_eq!(detect("src/lib.rs", ""), Some("rust"));
    assert_eq!(detect("web/App.tsx", ""), Some("tsx"));
    assert_eq!(detect("Cargo.lock", ""), Some("toml"));
    assert_eq!(detect("a/Gemfile", ""), Some("ruby"));
    assert_eq!(detect("scripts/checkpatch", "#!/usr/bin/env perl\n"), None);
    assert_eq!(
        detect("scripts/run", "#!/usr/bin/env -S python3 -u\nprint()\n"),
        Some("python")
    );
    assert_eq!(detect("tools/build", "#!/bin/sh\n"), Some("bash"));
    assert_eq!(detect("README", "text"), None);
}

#[test]
fn csharp_roles() {
    assert_eq!(detect("src/App.cs", ""), Some("csharp"));
    insta::assert_snapshot!(highlight(
        "csharp",
        "// note\nclass A { string F() { return \"x\"; } }\n"
    ), @r#"
    <comment>// note</>
    <keyword>class</> <type>A</> <punctuation>{</> <type>string</> <function>F</><punctuation>()</> <punctuation>{</> <keyword>return</> <string>"x"</><punctuation>;</> <punctuation>}</> <punctuation>}</>
    "#);
}

#[test]
fn dockerfile_roles() {
    assert_eq!(detect("Dockerfile", ""), Some("dockerfile"));
    assert_eq!(detect("build/Containerfile", ""), Some("dockerfile"));
    assert_eq!(detect("ci/web.dockerfile", ""), Some("dockerfile"));
    insta::assert_snapshot!(highlight(
        "dockerfile",
        "# note\nFROM alpine AS build\nENV MODE=\"fast\"\nRUN make\n"
    ), @r#"
    <comment># note</>
    <keyword>FROM</> alpine <keyword>AS</> build
    <keyword>ENV</> <property>MODE</>=<string>"fast"</>
    <keyword>RUN</> make
    "#);
}

#[test]
fn elixir_roles() {
    assert_eq!(detect("lib/app.ex", ""), Some("elixir"));
    assert_eq!(detect("bin/run", "#!/usr/bin/env elixir\n"), Some("elixir"));
    insta::assert_snapshot!(highlight("elixir", "# note\ndef f(x), do: \"x\"\n"), @r#"
    <comment># note</>
    <keyword>def</> <function>f</><punctuation>(</>x<punctuation>),</> <string>do: "x"</>
    "#);
}

/// Only Terraform's named values (`var`, `local`, …) are builtins: a
/// resource reference like `aws_s3_bucket.a` keeps its first name plain.
#[test]
fn hcl_roles() {
    assert_eq!(detect("infra/main.tf", ""), Some("hcl"));
    assert_eq!(detect("prod.tfvars", ""), Some("hcl"));
    insta::assert_snapshot!(highlight(
        "hcl",
        r#"# note
resource "aws_s3_bucket" "b" {
  bucket = "${var.prefix}-logs"
  count  = 2
  tags   = merge(local.tags, { Name = aws_s3_bucket.a.id })
  policy = <<-EOT
    %{ if true }on%{ endif }
  EOT
  lifecycle {
    prevent_destroy = false
  }
}
"#
    ), @r#"
    <comment># note</>
    <keyword>resource</> <string>"aws_s3_bucket"</> <string>"b"</> <punctuation>{</>
      <property>bucket</> = <string>"</><punctuation>${</><variable-builtin>var</><punctuation>.</><property>prefix</><punctuation>}</><string>-logs"</>
      <property>count</>  = <number>2</>
      <property>tags</>   = <function>merge</><punctuation>(</><variable-builtin>local</><punctuation>.</><property>tags</><punctuation>,</> <punctuation>{</> <property>Name</> = aws_s3_bucket<punctuation>.</><property>a</><punctuation>.</><property>id</> <punctuation>})</>
      <property>policy</> = <string><<-EOT</>
        <punctuation>%{</> <keyword>if</> <constant>true</> <punctuation>}</><string>on</><punctuation>%{</> <keyword>endif</> <punctuation>}</>
      <string>EOT</>
      <type>lifecycle</> <punctuation>{</>
        <property>prevent_destroy</> = <constant>false</>
      <punctuation>}</>
    <punctuation>}</>
    "#);
}

/// The jump patterns after the crate's query leave the returned value's
/// identifiers plain.
#[test]
fn kotlin_roles() {
    assert_eq!(detect("src/Main.kt", ""), Some("kotlin"));
    assert_eq!(detect("build.gradle.kts", ""), Some("kotlin"));
    insta::assert_snapshot!(highlight(
        "kotlin",
        "// note\nfun f(s: String): String {\n    return if (s > \"a\") s else \"b\"\n}\n"
    ), @r#"
    <comment>// note</>
    <keyword>fun</> <function>f</><punctuation>(</>s<punctuation>:</> <type>String</><punctuation>):</> <type>String</> <punctuation>{</>
        <keyword>return</> <keyword>if</> <punctuation>(</>s <operator>></> <string>"a"</><punctuation>)</> s <keyword>else</> <string>"b"</>
    <punctuation>}</>
    "#);
}

#[test]
fn make_roles() {
    assert_eq!(detect("Makefile", "all:\n"), Some("make"));
    assert_eq!(detect("GNUmakefile", ""), Some("make"));
    assert_eq!(detect("scripts/rules.mk", ""), Some("make"));
    insta::assert_snapshot!(highlight(
        "make",
        "# note\nCC := cc\nall: main.o\n\t$(CC) -o $@ $^\ninclude deps.mk\n"
    ), @"
    <comment># note</>
    CC <operator>:=</> <string>cc</>
    <constant>all</><punctuation>:</> main.o
    	<punctuation>$(</>CC<punctuation>)</> -o <punctuation>$</><operator>@</> <punctuation>$^</>
    <keyword>include</> deps.mk
    ");
}

#[test]
fn php_roles() {
    assert_eq!(detect("web/index.php", ""), Some("php"));
    assert_eq!(detect("bin/console", "#!/usr/bin/env php\n"), Some("php"));
    insta::assert_snapshot!(highlight(
        "php",
        "<?php\n// note\nfunction f($x) { return \"x\"; }\n"
    ), @r#"
    <tag><?php</>
    <comment>// note</>
    <keyword>function</> <function>f</>(<operator>$</>x) { <keyword>return</> <string>"x"</>; }
    "#);
}

/// The query captures comments as `@spell @comment`, in that order.
#[test]
fn scala_roles() {
    assert_eq!(detect("src/A.scala", ""), Some("scala"));
    assert_eq!(detect("build.sbt", ""), Some("scala"));
    insta::assert_snapshot!(highlight(
        "scala",
        "// note\nobject A { def f: String = \"x\" }\n"
    ), @r#"
    <comment>// note</>
    <keyword>object</> <type>A</> <punctuation>{</> <keyword>def</> <function>f</>: <type>String</> = <string>"x"</> <punctuation>}</>
    "#);
}

#[test]
fn sql_roles() {
    assert_eq!(detect("db/schema.sql", ""), Some("sql"));
    insta::assert_snapshot!(highlight(
        "sql",
        "-- note\nSELECT a FROM t WHERE b = 'x';\n"
    ), @"
    <comment>-- note</>
    <keyword>SELECT</> <property>a</> <keyword>FROM</> <type>t</> <keyword>WHERE</> <property>b</> <operator>=</> <string>'x'</><punctuation>;</>
    ");
}

#[test]
fn swift_roles() {
    assert_eq!(detect("Sources/App.swift", ""), Some("swift"));
    insta::assert_snapshot!(highlight(
        "swift",
        "// note\nfunc f() -> String { return \"x\" }\n"
    ), @r#"
    <comment>// note</>
    <keyword>func</> <function>f</><punctuation>()</> <operator>-></> <type>String</> <punctuation>{</> <keyword>return</> <string>"x"</> <punctuation>}</>
    "#);
}

#[test]
fn xml_roles() {
    assert_eq!(detect("pom.xml", ""), Some("xml"));
    assert_eq!(detect("icons/a.svg", ""), Some("xml"));
    assert_eq!(detect("Info.plist", ""), Some("xml"));
    insta::assert_snapshot!(highlight("xml", "<!-- note -->\n<a b=\"c\">text</a>\n"), @r#"
    <comment><!-- note --></>
    <punctuation><</><tag>a</> <property>b</><operator>=</><punctuation>"</><string>c</><punctuation>"></>text<punctuation></</><tag>a</><punctuation>></>
    "#);
}

/// Identifiers stay plain: the query's `#lua-match?` patterns for
/// constants and types are read as `#match?`.
#[test]
fn zig_roles() {
    assert_eq!(detect("src/main.zig", ""), Some("zig"));
    assert_eq!(detect("build.zig.zon", ""), Some("zig"));
    insta::assert_snapshot!(highlight(
        "zig",
        "// note\nconst std = @import(\"std\");\npub fn f(x: u32) u32 { return x; }\n"
    ), @r#"
    <comment>// note</>
    <keyword>const</> std <operator>=</> <keyword>@import</><punctuation>(</><string>"std"</><punctuation>);</>
    <keyword>pub</> <keyword>fn</> <function>f</><punctuation>(</>x<punctuation>:</> <type>u32</><punctuation>)</> <type>u32</> <punctuation>{</> <keyword>return</> x<punctuation>;</> <punctuation>}</>
    "#);
}

/// Cache keys carry a language's identity: stable for one grammar, apart
/// for two.
#[test]
fn identity_is_per_grammar() {
    let (c, cpp) = (lang("c"), lang("cpp"));
    assert_eq!(
        c.identity(),
        Registry::builtin().named("c").expect("c").identity()
    );
    assert_ne!(c.identity(), cpp.identity());
}

#[test]
fn rust_roles() {
    insta::assert_snapshot!(highlight(
        "rust",
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
        "c",
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
        "python",
        "def f(x):\n    return len(x) + 1  # n\n"
    ), @r"
    <keyword>def</> <function>f</>(x):
        <keyword>return</> <function>len</>(x) <operator>+</> <number>1</>  <comment># n</>
    ");
    insta::assert_snapshot!(highlight("json", "{\"a\": [1, true, null]}\n"), @r#"
    {<string>"a"</>: [<number>1</>, <constant>true</>, <constant>null</>]}
    "#);
}

/// Grammars whose query extends another's, concatenated in order.
#[test]
fn layered_queries() {
    insta::assert_snapshot!(
        highlight("cpp", "namespace n { class A : public B { virtual int f() const; }; }\n")
            + &highlight("tsx", "const x: number = <div className=\"a\">{y}</div>;\n"),
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
    let parsed = parse(lang("c"), text.bytes()).expect("parses");
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
    let parsed = parse(lang("c"), text.bytes()).expect("parses");
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
        parse(lang("c"), text.bytes())
            .expect("parses")
            .spans(text, 0..text.len())
    };
    let (old_spans, new_spans) = (spans(&old), spans(&new));
    let words = |emphasis: Vec<Emphasis>| -> Vec<String> {
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
    let parsed = parse(lang("c"), text.bytes()).expect("parses");
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
    assert!(parse(lang("c"), &big).is_none());
}

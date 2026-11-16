use std::fmt::Write as _;
use std::sync::LazyLock;

use crate::diff::{Options, Whitespace};

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
                    LineKind::Removed => old.line(line.old),
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

/// A small file in each bundled language, without parse errors, using the
/// constructs its grammar's external scanner handles (heredocs, raw and
/// template strings, indentation, comments that nest).
const SOURCES: [(&str, &str); 30] = [
    (
        "bash",
        "#!/bin/sh\n# build it\nset -eu\nname=\"${1:-dffy}\"\nfor f in *.rs; do\n\techo \"$f: $(wc -l < \"$f\")\"\ndone\ncat <<EOF\nhello $name\nEOF\nif [ -n \"$name\" ]; then\n  exit 0\nfi\n",
    ),
    (
        "c",
        "/* header\n * comment */\n#include <stdio.h>\n#define MAX(a, b) ((a) > (b) ? (a) : (b))\n\nstruct point { int x, y; };\n\nstatic int f(const struct point *p)\n{\n\tconst char *s = \"multi\\\n\tline\";\n\tif (p->x > 0)\n\t\treturn MAX(p->x, p->y);\n\tswitch (p->y) {\n\tcase 1: return 1;\n\tdefault: break;\n\t}\n\treturn printf(\"%s\\n\", s);\n}\n\nint main(void) { return f(&(struct point){ 1, 2 }); }\n",
    ),
    (
        "cpp",
        "#include <vector>\nnamespace n {\ntemplate <typename T>\nclass A : public B<T> {\npublic:\n  virtual int f() const override { return R\"x(raw \"string\")x\".size(); }\n  std::vector<T> items;\n};\n}  // namespace n\nauto g = [](int x) -> int { return x * 2; };\n",
    ),
    (
        "csharp",
        "using System;\n// note\nnamespace App {\n  public class A {\n    public string F(int x) {\n      var s = $\"value {x}\";\n      return @\"verbatim \"\"quoted\"\"\" + s;\n    }\n  }\n}\n",
    ),
    (
        "css",
        "/* note */\n@media (max-width: 600px) {\n  .a > b:hover::after {\n    content: \"x\";\n    color: #fff;\n    margin: 0 calc(1em + 2px);\n  }\n}\n:root { --gap: 4px; }\n",
    ),
    (
        "dockerfile",
        "# note\nFROM alpine:3 AS build\nENV MODE=\"fast\" \\\n    LEVEL=2\nRUN apk add --no-cache make && \\\n    make\nCOPY --from=build /out /app\nCMD [\"/app/run\", \"--x\"]\n",
    ),
    (
        "elixir",
        "# note\ndefmodule App do\n  @doc \"\"\"\n  Heredoc doc.\n  \"\"\"\n  def f(x) when is_integer(x), do: x + 1\n  def g(list) do\n    Enum.map(list, fn y -> \"v#{y}\" end)\n  end\nend\n",
    ),
    (
        "go",
        "package main\n\nimport \"fmt\"\n\n// f adds.\nfunc f(a, b int) (int, error) {\n\ts := `raw\nstring`\n\tif a > b {\n\t\treturn a, nil\n\t}\n\tfmt.Println(s)\n\treturn b, nil\n}\n",
    ),
    (
        "hcl",
        "# note\nresource \"aws_s3_bucket\" \"b\" {\n  bucket = \"${var.prefix}-logs\"\n  count  = 2\n  policy = <<-EOT\n    %{ if true }on%{ endif }\n  EOT\n  lifecycle {\n    prevent_destroy = false\n  }\n}\n",
    ),
    (
        "html",
        "<!DOCTYPE html>\n<!-- note -->\n<html>\n<head>\n<style>\nbody { color: red; }\n</style>\n<script>\nconst x = \"<b>\";\n</script>\n</head>\n<body class=\"a\">\n<p>text <em>more</em></p>\n<br/>\n</body>\n</html>\n",
    ),
    (
        "java",
        "package app;\n\n/** Doc. */\npublic class A<T> extends B implements C {\n  @Override\n  public String f(int x) {\n    var s = \"\"\"\n        text block\n        \"\"\";\n    return x > 0 ? s : \"none\";\n  }\n}\n",
    ),
    (
        "javascript",
        "// note\nimport { a } from \"./a.js\";\nconst f = async (x) => {\n  const s = `value ${x + 1} and ${`nested ${a}`}`;\n  return /ab+c/g.test(s) ? <div className=\"x\">{s}</div> : null;\n};\nexport default f;\n",
    ),
    (
        "json",
        "{\n  \"name\": \"dffy\",\n  \"list\": [1, 2.5, -3e4, true, false, null],\n  \"nested\": { \"a\": \"\\u00e9\\n\" }\n}\n",
    ),
    (
        "kotlin",
        "// note\npackage app\n\nfun f(s: String): String {\n    val t = \"\"\"\n        raw $s\n    \"\"\"\n    return if (s > \"a\") \"${s.length}\" else t\n}\n\nclass A(val x: Int) {\n    fun g() = x * 2\n}\n",
    ),
    (
        "lua",
        "-- note\n--[[ long\ncomment ]]\nlocal function f(x)\n  local s = [[long\nstring]]\n  if x > 0 then\n    return s .. tostring(x)\n  end\n  return nil\nend\nreturn { f = f }\n",
    ),
    (
        "make",
        "# note\nCC := cc\nCFLAGS += -O2\n\nall: main.o util.o\n\t$(CC) -o $@ $^\n\n%.o: %.c\n\t$(CC) $(CFLAGS) -c $<\n\ninclude deps.mk\n.PHONY: all\n",
    ),
    (
        "markdown",
        "# Title\n\nSome *text* with `code`.\n\n- item one\n- item two\n  continued\n\n```rust\nfn main() {}\n```\n\n> quote\n\n1. first\n2. second\n",
    ),
    (
        "php",
        "<?php\n// note\nnamespace App;\n\nfunction f($x) {\n    $s = <<<EOT\n    value {$x}\n    EOT;\n    return \"x: $s\";\n}\n\nclass A { public function g(): int { return 1; } }\n",
    ),
    (
        "python",
        "\"\"\"Module doc.\"\"\"\nimport os\n\n\ndef f(x, *args, **kw):\n    if x:\n        return f\"{x!r} {os.sep}\"\n    for y in args:\n        print(y)\n    return None\n\n\nclass A:\n    def g(self):\n        return [i * 2 for i in range(3)]\n",
    ),
    (
        "ruby",
        "# note\nrequire \"set\"\n\nclass A < B\n  def f(x)\n    s = <<~EOS\n      value #{x}\n    EOS\n    x > 0 ? s : %w[a b]\n  end\nend\n",
    ),
    (
        "rust",
        "/// Adds.\n#[inline]\npub fn add(a: u32, b: u32) -> u32 {\n    let s = r#\"raw \"string\"\"#;\n    /* nested /* block */ comment */\n    match a.checked_add(b) {\n        Some(n) => n,\n        None => s.len() as u32,\n    }\n}\n\nimpl<'a> Iterator for Words<'a> {\n    type Item = &'a str;\n    fn next(&mut self) -> Option<Self::Item> { None }\n}\n",
    ),
    (
        "scala",
        "// note\npackage app\n\nobject A {\n  def f(x: Int): String =\n    if x > 0 then s\"value $x\"\n    else \"\"\"raw\n    string\"\"\"\n\n  case class P(a: Int, b: String)\n}\n",
    ),
    (
        "sql",
        "-- note\nCREATE TABLE t (\n  id INTEGER PRIMARY KEY,\n  name TEXT NOT NULL\n);\n\nSELECT a, count(*) AS n\nFROM t\nWHERE b = 'x' AND c > 2\nGROUP BY a\nORDER BY n DESC;\n",
    ),
    (
        "swift",
        "// note\nimport Foundation\n\nstruct A {\n    var x: Int\n    func f() -> String {\n        let s = \"\"\"\n            multi \\(x)\n            \"\"\"\n        return x > 0 ? s : \"none\"\n    }\n}\n",
    ),
    (
        "toml",
        "# note\n[package]\nname = \"dffy\"\nversion = \"0.1.0\"\ndescription = \"\"\"\nmulti\nline\"\"\"\n\n[dependencies]\nmemchr = { version = \"2\", features = [\"std\"] }\n\n[[bin]]\nname = 'dffy'\n",
    ),
    (
        "tsx",
        "// note\nimport React from \"react\";\n\ntype P = { name: string };\n\nexport function A({ name }: P) {\n  const s = `hi ${name}`;\n  return <div className=\"a\">{s}<B<P> x={1} /></div>;\n}\n",
    ),
    (
        "typescript",
        "// note\ninterface P<T> { value: T; next?: P<T> }\n\nexport async function f<T>(p: P<T>): Promise<T[]> {\n  const out: T[] = [];\n  for (let q: P<T> | undefined = p; q; q = q.next) out.push(q.value);\n  return `${out.length}` as unknown as T[];\n}\n",
    ),
    (
        "xml",
        "<?xml version=\"1.0\"?>\n<!-- note -->\n<root a=\"1\">\n  <item b='2'>text &amp; more</item>\n  <![CDATA[raw <data>]]>\n  <empty/>\n</root>\n",
    ),
    (
        "yaml",
        "# note\nname: dffy\nlist:\n  - one\n  - \"two\"\n  - three: 3\nblock: |\n  literal\n  text\nfolded: >\n  folded\n  text\nanchor: &a { x: 1 }\nref: *a\n",
    ),
    (
        "zig",
        "// note\nconst std = @import(\"std\");\n\npub fn f(x: u32) u32 {\n    const s =\n        \\\\multi\n        \\\\line\n    ;\n    _ = s;\n    return if (x > 0) x else 0;\n}\n",
    ),
];

/// Every node of `a` and `b` in document order, compared by what a reader
/// of the tree sees. The parse state is left out: a reused node keeps the
/// one it was built in.
fn same_tree(a: &Tree, b: &Tree) -> Result<(), String> {
    type Key = (
        u16,
        Range<usize>,
        [Point; 2],
        [bool; 4],
        u32,
        Option<std::num::NonZeroU16>,
    );
    fn key(c: &tree_sitter::TreeCursor) -> Key {
        let n = c.node();
        (
            n.kind_id(),
            n.byte_range(),
            [n.start_position(), n.end_position()],
            [n.is_named(), n.is_missing(), n.is_extra(), n.has_error()],
            n.child_count(),
            c.field_id(),
        )
    }
    let (mut x, mut y) = (a.walk(), b.walk());
    loop {
        if key(&x) != key(&y) {
            return Err(format!("{:?} != {:?}", key(&x), key(&y)));
        }
        if x.goto_first_child() {
            y.goto_first_child();
            continue;
        }
        while !x.goto_next_sibling() {
            if !x.goto_parent() {
                return Ok(());
            }
            y.goto_parent();
        }
        y.goto_next_sibling();
    }
}

/// Checks [`Parsed::reparse`] of `new` from `old` in language `name`
/// against a parse from scratch: the same spans always, and when no error
/// rules reuse out, the same tree node for node, from the reparse and from
/// the edited tree alone.
fn check_reparse(name: &str, old: &str, new: &str, options: &Options) -> Result<(), String> {
    let (old, new) = (Text::new(old.into()), Text::new(new.into()));
    let hunks = crate::diff::hunks(&old, &new, options);
    let parsed = parse(lang(name), old.bytes()).ok_or("old side doesn't parse")?;
    let fresh = parse(lang(name), new.bytes()).ok_or("new side doesn't parse")?;
    let reparsed = parsed.reparse(&old, &new, &hunks).ok_or("reparse failed")?;
    if reparsed.spans(&new, 0..new.len()) != fresh.spans(&new, 0..new.len()) {
        return Err("spans differ".to_owned());
    }
    let mut edited = parsed.tree.clone();
    let exact = edit(&mut edited, &old, &new, &hunks);
    if options.whitespace == Whitespace::Exact && !exact {
        return Err("exact hunks don't account for every byte".to_owned());
    }
    let budget = Duration::from_secs(10);
    if exact
        && !parsed.tree.root_node().has_error()
        && let Some(reused) = parse_tree(lang(name), new.bytes(), budget, Some(&edited))
        && !reused.root_node().has_error()
    {
        same_tree(&reused, &fresh.tree)?;
        same_tree(&reparsed.tree, &fresh.tree)?;
    }
    Ok(())
}

#[test]
fn every_language_has_a_reparse_source_without_errors() {
    for l in GRAMMARS.languages() {
        assert!(
            SOURCES.iter().any(|(name, _)| *name == l.name()),
            "{} has no source",
            l.name()
        );
    }
    for (name, source) in SOURCES {
        let parsed = parse(lang(name), source.as_bytes()).expect("parses");
        assert!(
            !parsed.tree.root_node().has_error(),
            "{name}: {}",
            parsed.tree.root_node().to_sexp()
        );
    }
}

#[test]
fn reparsing_at_the_edges_equals_parsing_from_scratch() {
    let base = "fn a() {}\n\tlet x = \"é😀\";\nfn b() {}\n";
    let crlf = base.replace('\n', "\r\n");
    let unterminated = base.trim_end_matches('\n');
    let cases = [
        (base, ""),
        ("", base),
        (base, &*format!("// first\n{base}")),
        (base, &*format!("{base}fn c() {{}}\n")),
        (base, unterminated),
        (unterminated, base),
        (unterminated, &*format!("{unterminated} // end")),
        (base, &*crlf),
        (&*crlf, &*crlf.replace("a()", "aa()")),
        (base, &*base.replace("é😀", "\t日本\t")),
        (base, "\n"),
    ];
    for (old, new) in cases {
        for name in ["rust", "c"] {
            check_reparse(name, old, new, &Options::default())
                .unwrap_or_else(|e| panic!("{name} {old:?} -> {new:?}: {e}"));
        }
    }
}

/// [`Highlighter::after`] gives the new side the spans it gets prepared
/// alone, whether or not it's parsed from the old side's tree.
#[test]
fn a_side_prepared_after_the_old_one_matches_it_prepared_alone() {
    let old = Text::new(C_FILE.into());
    let new = Text::new(C_FILE.replace("a - 1", "a - 2").into());
    let hunks = crate::diff::hunks(&old, &new, &Options::default());
    let side = |path: &'static str, text| Source {
        path: path.as_bytes(),
        text,
        id: None,
    };
    let old_h = Highlighter::for_side(&GRAMMARS, side("a.c", &old), None, false).expect("c");
    assert!(old_h.can_seed());
    for path in ["a.c", "a.rs"] {
        let new_side = side(path, &new);
        let after = Highlighter::after(
            &GRAMMARS,
            new_side,
            Some((&old_h, &old, &hunks)),
            None,
            false,
        );
        let alone = Highlighter::for_side(&GRAMMARS, new_side, None, false);
        let all = 0..new.len();
        assert_eq!(
            after.map(|h| h.spans(&new, all.clone())),
            alone.map(|h| h.spans(&new, all.clone())),
            "{path}"
        );
    }
}

/// Lines dropped into a file by the property test: none, blank, CRLF,
/// multi-byte, and the openers and closers that move a parser's state.
const INSERTED: [&str; 11] = [
    "",
    "\n",
    "\t\tx = y;\r\n",
    "é ü 日本語 😀 \"quoted\"\n",
    "}\n",
    "{\n",
    "(\n",
    "\"open\n",
    "/* open\n",
    "# comment\n",
    "    indented: 1\n",
];

/// Text typed into a line by the property test.
const TYPED: [&str; 6] = ["x", "42", " ", "é", "\t", "_a"];

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(1024))]

    /// Each case edits a language's source a few times, maybe drops the
    /// final newline or swaps the sides, and diffs the two with any
    /// context and whitespace mode. An edit replaces a run of lines with
    /// lines from the file itself or from [`INSERTED`], or inserts one of
    /// [`TYPED`] into a line or deletes a character of it: most of those
    /// keep the file free of errors, where the trees must match.
    #[test]
    fn reparsing_equals_parsing_from_scratch(
        which in 0..SOURCES.len(),
        edits in proptest::collection::vec(
            (0usize..64, 0u8..3, 0usize..64, proptest::collection::vec(0usize..64, 0..4)),
            1..4,
        ),
        unterminated: bool,
        swap: bool,
        context in 0usize..4,
        whitespace in proptest::sample::select(&[
            Whitespace::Exact,
            Whitespace::Exact,
            Whitespace::IgnoreAll,
            Whitespace::IgnoreChange,
        ][..]),
    ) {
        let (name, source) = SOURCES[which];
        let pool: Vec<&str> = source.split_inclusive('\n').collect();
        let mut lines: Vec<String> = pool.iter().map(|l| (*l).to_owned()).collect();
        for (at, op, arg, inserted) in edits {
            let at = at % (lines.len() + 1);
            let Some(line) = lines.get_mut(at).filter(|_| op > 0) else {
                let removed = (arg % 4).min(lines.len() - at);
                let inserted = inserted.iter().map(|&i| match INSERTED.get(i) {
                    Some(line) => (*line).to_owned(),
                    None => pool[i % pool.len()].to_owned(),
                });
                lines.splice(at..at + removed, inserted);
                continue;
            };
            let content = line.trim_end_matches(['\n', '\r']).len();
            let chars: Vec<usize> = line[..content].char_indices().map(|(i, _)| i).collect();
            if op == 1 {
                let i = chars.get(arg % (chars.len() + 1)).copied().unwrap_or(content);
                line.insert_str(i, TYPED[arg % TYPED.len()]);
            } else if let Some(&i) = chars.get(arg % chars.len().max(1)) {
                line.remove(i);
            }
        }
        let mut edited = lines.concat();
        if unterminated {
            edited.truncate(edited.trim_end_matches(['\n', '\r']).len());
        }
        let (old, new) = if swap { (&*edited, source) } else { (source, &*edited) };
        let options = Options { context, whitespace };
        if let Err(e) = check_reparse(name, old, new, &options) {
            proptest::prop_assert!(false, "{name} {old:?} -> {new:?}: {e}");
        }
    }
}

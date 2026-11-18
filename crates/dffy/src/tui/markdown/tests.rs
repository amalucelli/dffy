use std::fmt::Write as _;

use super::*;

/// The rows with each styled span as `<role:syntax+attributes>text</>`:
/// `b` bold, `d` dim, `i` italic, `u` underline.
fn markup(body: &Body) -> String {
    let mut out = String::new();
    for row in body.rows.rows() {
        for (ink, text) in row {
            let mut tag = String::new();
            if ink.role != Role::Plain {
                tag.push_str(ink.role.key());
            }
            if let Some(syntax) = ink.syntax {
                let _ = write!(tag, ":{}", syntax.key());
            }
            let letters: String = [
                (Attributes::BOLD, 'b'),
                (Attributes::DIM, 'd'),
                (Attributes::ITALIC, 'i'),
                (Attributes::UNDERLINE, 'u'),
            ]
            .iter()
            .filter(|(a, _)| ink.attributes.contains(*a))
            .map(|(_, l)| *l)
            .collect();
            if !letters.is_empty() {
                let _ = write!(tag, "+{letters}");
            }
            if tag.is_empty() {
                out.push_str(text);
            } else {
                let _ = write!(out, "<{tag}>{text}</>");
            }
        }
        out.push('\n');
    }
    out
}

fn text(body: &Body) -> Vec<String> {
    body.rows
        .rows()
        .map(|row| {
            row.map(|(_, t)| t)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

const EVERYTHING: &str = "\
# Title
## Section
### Smaller

Some **bold**, _italic_, ~~struck~~ and `code`, with [a link](https://example.com/x) \
long enough to wrap.
A line break stays.

- one
- two
  - nested, long enough that it wraps under its own text
1. first
2. second

- [x] done
- [ ] to do

> quoted
> > nested

---

| name | count |
|------|------:|
| apples | 3 |
| `pears` | 12 |

```rust
fn main() {
    println!(\"hi\");
}
```

Inline <b>html</b> and an image ![alt](x.png).

<details>
<summary>More</summary>
</details>
";

/// Every construct, in the inks the box draws them in, wrapped to a wide
/// box and a narrow one.
#[test]
fn every_construct_has_its_style() {
    insta::assert_snapshot!("everything_wide", markup(&render(EVERYTHING, 72, true)));
    insta::assert_snapshot!("everything_narrow", markup(&render(EVERYTHING, 28, true)));
}

/// What a bot keeps in a comment's HTML comments never shows, in a block
/// of its own, inline, or across lines, and leaves no blank row; other
/// HTML shows as written.
#[test]
fn html_comments_vanish() {
    let body = "<!-- cr-status -->\n<!-- cr-state\n{\"sha\":\"03f1ba\",\"verdict\":\"needs-changes\"} -->\n\n\
@ana **Needs changes** at 03f1ba: two blockers.<!-- inline -->\n\n\
<!-- What type of change is this? -->\n\n- [X] Feature\n- [ ] Bug Fix\n\n\
## Testing <!-- how? -->\n\n<details>\n<!-- c --> kept\n</details>\n";
    let rows = text(&render(body, 60, true));
    assert_eq!(
        rows,
        [
            "@ana Needs changes at 03f1ba: two blockers.",
            "",
            "☑ Feature",
            "☐ Bug Fix",
            "",
            "Testing",
            "",
            "<details>",
            " kept",
            "</details>",
        ]
    );
    assert_eq!(text(&render("<!-- only -->\n", 60, true)), [""; 0]);
}

/// A fence naming a language dffy knows is highlighted, by name or by
/// extension; unknown ones, and all with highlighting off, take the
/// inline code's ink. The `<!--` in code stays.
#[test]
fn fenced_code_is_highlighted_in_its_language() {
    let code = |fence: &str, highlight: bool| {
        let body = format!("```{fence}\nfn main() {{}} // <!-- x -->\n```\n");
        markup(&render(&body, 60, highlight))
    };
    assert!(
        code("rust", true).contains("<:keyword>fn</>"),
        "{}",
        code("rust", true)
    );
    assert!(code("rs", true).contains("<:keyword>fn</>"));
    let plain = "  <:string>fn main() {} // <!-- x --></>\n";
    assert_eq!(code("nope", true), plain);
    assert_eq!(code("rust", false), plain);
}

/// Text wraps at spaces, a word wider than the box breaks inside, and
/// a line break stays one, whichever line ending the host sent.
#[test]
fn text_wraps_at_spaces_and_breaks_long_words() {
    assert_eq!(
        text(&render("one two three", 7, true)),
        ["one two", "three"]
    );
    assert_eq!(text(&render("abcdefghij", 4, true)), ["abcd", "efgh", "ij"]);
    assert_eq!(
        text(&render("a\r\nb\r\n\r\nc", 4, true)),
        ["a", "b", "", "c"]
    );
}

/// Hosts decode entities, and so does the parser: what they stand for is
/// escaped as any text is.
#[test]
fn decoded_entities_are_escaped() {
    let rows = text(&render("a &#27;[31m b &#x202e; c", 60, true));
    assert_eq!(rows, ["a ^[[31m b <U+202E> c"]);
}

/// Quote bars and list indentation wider than the box still leave a
/// column a row, so the text wraps a character a row rather than spins.
#[test]
fn a_box_narrower_than_its_indentation_still_wraps() {
    let rows = text(&render("> > - - deep words", 3, true));
    assert_eq!(rows.len(), "deepwords".len(), "{rows:?}");
    assert_eq!(
        rows[0], "│ │ • • d",
        "an item opening with a list keeps its bullet"
    );
    assert_eq!(rows[1], "│ │     e");
}

/// A folded thread's line: the first line of text, past HTML comments and
/// without its markup.
#[test]
fn summaries_are_the_first_line_of_text() {
    assert_eq!(
        summary("<!-- cr-status -->\n<!-- cr-state\n{} -->\n\n@ana **Needs changes** at x.\nmore"),
        "@ana Needs changes at x."
    );
    assert_eq!(summary("## Title\n\nbody"), "Title");
    assert_eq!(summary("- [x] Feature\n- [ ] Bug"), "☑ Feature");
    assert_eq!(summary("```rust\nfn main() {}\n```"), "fn main() {}");
    assert_eq!(summary("plain\r\nsecond"), "plain");
    assert_eq!(summary("a\u{1b}[31m"), "a^[[31m");
    assert_eq!(summary("<!-- nothing else -->"), "");
}

const REVIEW: &str = "<!-- cr-status -->\n\n## Needs changes\n\n\
@ana **two blockers** at `03f1ba`, see [the guide](https://example.com/guide):\n\n\
- [x] tests pass\n- [ ] `scope.ts:382` reads every project\n\n\
> Default to the chat's own project.\n\n```rust\nlet scope = Scope::Own;\n```\n\na &amp; b\n";

/// The cell where `needle` first shows in `body`'s rows: its first
/// character's, or with `last` its last's.
fn cell(body: &Body, needle: &str, last: bool) -> (usize, usize) {
    for (row, text) in text(body).iter().enumerate() {
        if let Some(at) = text.find(needle) {
            let end = if last {
                at + needle.len() - needle.chars().last().map_or(0, char::len_utf8)
            } else {
                at
            };
            return (row, text[..end].width());
        }
    }
    panic!("{needle} not in {:?}", text(body));
}

/// What a drag from the first character of `from` to the last of `to`
/// copies of `source` drawn `width` columns wide.
fn dragged<'a>(source: &'a str, width: usize, from: &str, to: &str) -> &'a str {
    let body = render(source, width, true);
    let (a, b) = (cell(&body, from, false), cell(&body, to, true));
    let range = body
        .map
        .span(&body.map.at(a.0, a.1), &body.map.at(b.0, b.1));
    source[range].trim_end_matches(['\n', '\r'])
}

/// A selection copies the Markdown it was drawn from: within plain text,
/// what's selected; an element whose whole text it covers, with its
/// markup: bold, inline code, a list item (from its checkbox too), a
/// quote, a code block. Part of one is its text alone.
#[test]
fn a_selection_copies_the_markdown_it_was_drawn_from() {
    for (from, to, copied) in [
        ("Default", "chat's", "Default to the chat's"),
        ("two", "two", "two"),
        ("two", "blockers", "**two blockers**"),
        ("@ana", "blockers", "@ana **two blockers**"),
        ("03f1ba", "03f1ba", "`03f1ba`"),
        ("03f", "03f", "03f"),
        ("the", "guide", "[the guide](https://example.com/guide)"),
        ("tests", "pass", "- [x] tests pass"),
        ("☑", "pass", "- [x] tests pass"),
        (
            "tests",
            "reads",
            "- [x] tests pass\n- [ ] `scope.ts:382` reads",
        ),
        ("scope =", "Own", "scope = Scope::Own"),
        ("let", "Own;", "```rust\nlet scope = Scope::Own;\n```"),
        ("Needs", "changes", "## Needs changes"),
        ("&", "&", "&amp;"),
    ] {
        assert_eq!(dragged(REVIEW, 60, from, to), copied, "{from}…{to}");
    }
    // Dragged back, the same.
    let body = render(REVIEW, 60, true);
    let (a, b) = (cell(&body, "two", false), cell(&body, "blockers", true));
    let range = body
        .map
        .span(&body.map.at(b.0, b.1), &body.map.at(a.0, a.1));
    assert_eq!(&REVIEW[range], "**two blockers**");
}

/// Across wrapped rows, the source runs on as written, the spaces a wrap
/// takes included.
#[test]
fn a_selection_runs_on_across_wrapped_rows() {
    let body = render(REVIEW, 16, true);
    assert_eq!(
        cell(&body, "own", false).0,
        cell(&body, "Default", false).0 + 1
    );
    assert_eq!(
        dragged(REVIEW, 16, "Default", "own"),
        "Default to the chat's own"
    );
    assert_eq!(
        dragged(REVIEW, 16, "Default", "project."),
        "> Default to the chat's own project."
    );
}

/// A cell past a row's text stands for the end of that row's last glyph,
/// and one with no glyph before it on its row (a bullet, a blank row) for
/// the start of the next glyph.
#[test]
fn cells_without_text_stand_for_the_nearest_glyph() {
    let body = render(REVIEW, 60, true);
    let (row, _) = cell(&body, "tests pass", false);
    let end = REVIEW.find("tests pass").expect("in it") + "tests pass".len();
    assert_eq!(body.map.at(row, 50), end..end);
    let start = REVIEW.find("tests").expect("in it");
    assert_eq!(body.map.at(row, 0), start..start);
    assert_eq!(body.map.at(row - 1, 3), start..start, "the blank row above");
    let last = REVIEW.len() - 1;
    assert_eq!(body.map.at(99, 0), last..last, "below the text");
}

use std::fmt::Write as _;

use super::*;

/// Each block as a `kind a..b` line followed by its lines, as plain text.
pub(crate) fn shown(blocks: &[Block]) -> String {
    let mut out = String::new();
    for block in blocks {
        let (kind, source) = (block.kind, &block.source);
        let _ = writeln!(out, "{kind} {}..{}", source.start, source.end);
        for line in &block.lines {
            let text: String = line.iter().map(|s| s.text.as_str()).collect();
            let _ = writeln!(out, "  {text}");
        }
    }
    out
}

/// `source` drawn at `width`, as [`shown`] has it.
pub(crate) fn drawn(source: &str, width: usize) -> String {
    shown(&render(source, width).expect("the diagram draws"))
}

#[test]
fn other_diagrams_and_empty_sources_draw_nothing() {
    for source in [
        "",
        "%% only a comment\n",
        "pie title Pets\n  \"Dogs\" : 386\n",
        "classDiagram\n  Animal <|-- Duck\n",
        "stateDiagram-v2\n  [*] --> Still\n",
    ] {
        assert_eq!(render(source, 80), None, "{source}");
    }
}

#[test]
fn parse_errors_draw_nothing() {
    for source in [
        "graph XY\n  A --> B\n",
        "graph TD\n  A --> [broken\n",
        "graph TD\n  A -- no end B\n",
        "graph TD\n  subgraph one\n  A --> B\n",
        "graph TD\n  end\n",
        "sequenceDiagram\n  Alice says hi\n",
        "sequenceDiagram\n  loop forever\n  A->>B: hi\n",
        "sequenceDiagram\n  else nothing\n",
    ] {
        assert_eq!(render(source, 80), None, "{source}");
    }
}

#[test]
fn front_matter_comments_and_a_missing_final_newline() {
    let source = "---\ntitle: Flow\n---\n%% a comment\ngraph LR\n  A --> B";
    let blocks = render(source, 80).expect("draws");
    assert_eq!(blocks.len(), 1);
    assert_eq!(blocks[0].source, 0..6);
    let sequence = render("sequenceDiagram\n  A->>B: hi", 80).expect("draws");
    assert_eq!(sequence.last().map(|b| b.source.clone()), Some(1..2));
}

#[test]
fn labels_lose_quotes_and_break_at_br() {
    assert_eq!(clean(" \"a<br/>b\" "), "a\nb");
    assert_eq!(clean("`md`"), "md");
    assert_eq!(wrap("one two three", 7), ["one two", "three"]);
    assert_eq!(wrap("a\nbb", 9), ["a", "bb"]);
}

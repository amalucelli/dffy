use super::*;

/// Each block as `kind a..b | line | line`, styled spans as
/// `<style,style>text</>`.
fn shown(source: &str, width: usize) -> String {
    render(source, width)
        .iter()
        .map(|block| {
            let lines: Vec<String> = block
                .lines
                .iter()
                .map(|line| {
                    line.iter()
                        .map(|span| match span.style.as_slice() {
                            [] => span.text.clone(),
                            style => format!("<{}>{}</>", style.join(","), span.text),
                        })
                        .collect()
                })
                .collect();
            format!(
                "{} {}..{} | {}",
                block.kind,
                block.source.start,
                block.source.end,
                lines.join(" | ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn headings_paragraphs_and_inline_styles() {
    let source = "# Title\n\nSome **bold**, *soft*\nwrapped `code` and ~~gone~~.\n\n## Next\n";
    assert_eq!(
        shown(source, 80),
        "heading 0..1 | <heading>Title</>\n\
         gap 1..1 | \n\
         paragraph 2..4 | Some <bold>bold</>, <italic>soft</> wrapped <code>code</> and <strikethrough,dim>gone</>.\n\
         gap 4..4 | \n\
         heading 5..6 | <heading>Next</>\n\
         gap 6..6 | "
    );
    let blocks = render(source, 80);
    assert_eq!(blocks[0].lines[0][0].level, Some(1));
    assert_eq!(blocks[4].lines[0][0].level, Some(2));
}

#[test]
fn lists_get_bullets_numbers_indentation_and_task_marks() {
    let source = "- one\n- two\n  - nested\n\n3. three\n4. four\n\n- [x] done\n- [ ] todo\n";
    assert_eq!(
        shown(source, 80),
        "list-item 0..1 | <dim>• </>one\n\
         list-item 1..2 | <dim>• </>two\n\
         list-item 2..3 | <dim>  • </>nested\n\
         gap 3..3 | \n\
         list-item 4..5 | <dim>3. </>three\n\
         list-item 5..6 | <dim>4. </>four\n\
         gap 6..6 | \n\
         list-item 7..8 | <dim>• ☑ </>done\n\
         list-item 8..9 | <dim>• </>☐ todo\n\
         gap 9..9 | "
    );
}

/// dffy diffs blocks, so the item that was last must stay the same block
/// when another is appended.
#[test]
fn appending_a_list_item_leaves_the_other_blocks_alone() {
    let old = render("- one\n- two\n\nAfter.\n", 80);
    let new = render("- one\n- two\n- three\n\nAfter.\n", 80);
    let identity = |b: &Block| (b.kind, b.lines.clone());
    let added: Vec<_> = new
        .iter()
        .map(identity)
        .filter(|b| !old.iter().map(identity).any(|o| o == *b))
        .collect();
    assert_eq!(
        old[..2],
        new[..2],
        "the items before keep their lines and sources"
    );
    assert_eq!(added.len(), 1, "{new:?}");
    assert_eq!(new[2].kind, "list-item");
    assert_eq!(new[2].source, 2..3);
}

#[test]
fn quotes_have_a_bar_and_code_keeps_its_text_and_language() {
    let source = "> quoted\n> text\n\n```rust\nfn main() {\n    run();\n}\n```\n";
    assert_eq!(
        shown(source, 80),
        "paragraph 0..2 | <dim>│ </>quoted text\n\
         gap 2..2 | \n\
         code 3..8 | <code>fn main() {</> | <code>    run();</> | <code>}</>\n\
         gap 8..8 | "
    );
    assert_eq!(render(source, 80)[2].language.as_deref(), Some("rust"));
}

#[test]
fn mermaid_fences_are_drawn_and_broken_ones_stay_code() {
    let source = "```mermaid\nsequenceDiagram\n  A->>B: hi\n```\n\n```mermaid\npie\n```\n";
    assert_eq!(
        shown(source, 80),
        "participants 0..2 | <dim>┌───┐</> <dim>┌───┐</> | <dim>│</> <bold>A</> <dim>│</> <dim>│</> <bold>B</> <dim>│</> | <dim>└─┬─┘</> <dim>└─┬─┘</>\n\
         message 2..4 |   <dim>│</> hi  <dim>│</> |   <syntax-operator>├────▶</><dim>│</>\n\
         gap 4..4 | \n\
         code 5..8 | <code>pie</>\n\
         gap 8..8 | "
    );
    let blocks = render(source, 80);
    assert_eq!(blocks[0].language, None);
    assert_eq!(blocks[3].language.as_deref(), Some("mermaid"));
}

#[test]
fn links_images_html_and_rules() {
    let source = "See [docs](https://x.dev) ![a cat](cat.png)\n\n<div>\nraw\n</div>\n\n---\n";
    let blocks = render(source, 10);
    assert_eq!(
        shown(source, 10),
        "paragraph 0..1 | See <link>docs</> <dim>[image: a cat]</>\n\
         gap 1..1 | \n\
         html 2..5 | <dim><div></> | <dim>raw</> | <dim></div></>\n\
         gap 5..5 | \n\
         rule 6..7 | <dim>──────────</>\n\
         gap 7..7 | "
    );
    assert_eq!(blocks[0].lines[0][1].url.as_deref(), Some("https://x.dev"));
}

#[test]
fn tables_are_boxed_aligned_and_wrapped_to_the_width() {
    let source =
        "| Name | Notes |\n|:-----|------:|\n| a | short |\n| b | a much longer note here |\n";
    let text = shown(source, 24);
    assert_eq!(
        text,
        "table-border 0..1 | <dim>┌──────┬───────────────┐</>\n\
         table-header 0..1 | <dim>│</> <bold>Name</> <dim>│</>         <bold>Notes</> <dim>│</>\n\
         table-border 0..1 | <dim>├──────┼───────────────┤</>\n\
         table-row 2..3 | <dim>│</> a    <dim>│</>         short <dim>│</>\n\
         table-row 3..4 | <dim>│</> b    <dim>│</> a much longer <dim>│</> | <dim>│</>      <dim>│</>     note here <dim>│</>\n\
         table-border 3..4 | <dim>└──────┴───────────────┘</>\n\
         gap 4..4 | "
    );
    for block in render(source, 24) {
        for line in block.lines {
            let width: usize = line.iter().map(|s| s.text.width()).sum();
            assert!(width <= 24, "{line:?}");
        }
    }
}

#[test]
fn footnotes_and_sources_at_the_end_without_a_newline() {
    let source = "Text[^1].\n\n[^1]: The note";
    assert_eq!(
        shown(source, 80),
        "paragraph 0..1 | Text<dim>[^1]</>.\n\
         gap 1..1 | \n\
         footnote 2..3 | <dim>[^1]: </>The note\n\
         gap 3..3 | "
    );
}

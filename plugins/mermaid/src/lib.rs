//! Mermaid diagrams drawn as text, as blocks of styled lines for dffy's
//! renderer protocol (see docs/plugins.md in the dffy repository). The
//! `dffy-render-mermaid` binary answers the protocol with it, and the
//! Markdown renderer draws ```` ```mermaid ```` fences with it.
//!
//! Flowcharts and sequence diagrams are drawn; any other diagram, or one
//! that doesn't parse or is too large to draw, gets `None` so the caller
//! can show its source instead.

mod canvas;
pub mod columns;
mod flowchart;
mod sequence;

use std::ops::Range;

/// Past this many cells (columns times rows) a drawing isn't made and the
/// source shows instead: drawing and sending it would take longer than
/// dffy waits for a renderer.
const MAX_CELLS: usize = 1_000_000;

/// A stretch of a line in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    /// Style names from the protocol's vocabulary.
    pub style: &'static [&'static str],
}

pub type Line = Vec<Span>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    pub kind: &'static str,
    /// Lines of the diagram's source, 0-based, end exclusive.
    pub source: Range<usize>,
    pub lines: Vec<Line>,
}

/// The diagram `source` holds, drawn to fit `width` columns when it can.
/// A drawing that can't fit keeps its full width rather than being cut.
///
/// A flowchart is one block covering the whole source: its layout moves
/// as a whole, and dffy pairs the old and new drawings line by line. A
/// sequence diagram is a block per statement (the participants, each
/// message, note and frame border), so an edit shows as that statement
/// alone.
///
/// `None` for any other diagram type, a parse error, or a diagram past
/// the size limits: 300 nodes, 600 edges, 100 subgraphs or 3000 nodes and
/// edge bends in a flowchart, 60 participants or 2000 statements in a
/// sequence diagram, or a drawing of more than a million cells.
#[must_use]
pub fn render(source: &str, width: usize) -> Option<Vec<Block>> {
    let statements = statements(source);
    let &(_, first) = statements.first()?;
    let keyword = first
        .split(|c: char| c.is_whitespace() || c == ';')
        .next()
        .unwrap_or_default();
    match keyword {
        "graph" | "flowchart" | "flowchart-elk" => {
            let lines = flowchart::render(&statements, width)?;
            Some(vec![Block {
                kind: "flowchart",
                source: 0..source.lines().count(),
                lines,
            }])
        }
        "sequenceDiagram" => sequence::render(&statements, width),
        _ => None,
    }
}

/// The lines of `source` that say something, trimmed, with their line
/// numbers: no blank lines, `%%` comments or `---` front matter.
fn statements(source: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut front_matter = false;
    for (i, line) in source.lines().enumerate() {
        let line = line.trim();
        if i == 0 && line == "---" {
            front_matter = true;
        } else if front_matter {
            front_matter = line != "---";
        } else if !line.is_empty() && !line.starts_with("%%") {
            out.push((i, line));
        }
    }
    out
}

/// A label's text: quotes and Markdown-string backticks removed, `<br>`
/// as a line break, other control characters as spaces.
fn clean(text: &str) -> String {
    let text = text.trim();
    let text = text
        .strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(text);
    let text = text
        .strip_prefix('`')
        .and_then(|t| t.strip_suffix('`'))
        .unwrap_or(text);
    let mut out = text.to_owned();
    for br in ["<br/>", "<br />", "<br>", "<BR/>", "<BR />", "<BR>"] {
        out = out.replace(br, "\n");
    }
    out.chars()
        .map(|c| if c.is_control() && c != '\n' { ' ' } else { c })
        .collect()
}

/// `text`'s lines, each broken into lines of at most `width` columns, at
/// spaces where it can and inside words where it must.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split_whitespace() {
            let w = canvas::width(word);
            if used > 0 && used + 1 + w > width {
                lines.push(std::mem::take(&mut line));
                used = 0;
            } else if used > 0 {
                line.push(' ');
                used += 1;
            }
            for (piece, w) in columns::clusters(word) {
                if used > 0 && used + w > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push_str(piece);
                used += w;
            }
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests;

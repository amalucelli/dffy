//! Markdown (the `CommonMark` spec with GitHub's tables, strikethrough,
//! task lists and footnotes) to blocks of styled lines. Paragraphs and list items stay
//! one line each, for dffy to wrap and diff word by word; tables are drawn
//! with box characters and wrapped to the width they're given.

use std::ops::Range;

use dffy_render_mermaid::columns;
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// A stretch of a line in one style.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Span {
    pub text: String,
    /// Style names from the protocol's vocabulary.
    pub style: Vec<&'static str>,
    /// For a link.
    pub url: Option<String>,
    /// For a heading, 1 to 6.
    pub level: Option<u8>,
}

impl Span {
    fn plain(text: impl Into<String>) -> Self {
        Self::styled(text, &[])
    }

    fn styled(text: impl Into<String>, style: &[&'static str]) -> Self {
        Self {
            text: text.into(),
            style: style.to_vec(),
            url: None,
            level: None,
        }
    }
}

pub(crate) type Line = Vec<Span>;

/// The kind of the blank line between top-level blocks.
const GAP: &str = "gap";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Block {
    pub kind: &'static str,
    /// Lines of the source, 0-based, end exclusive.
    pub source: Range<usize>,
    pub lines: Vec<Line>,
    /// A code block's fence language.
    pub language: Option<String>,
}

pub(crate) fn render(source: &str, width: usize) -> Vec<Block> {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;
    let mut renderer = Renderer::new(source, width);
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        renderer.event(event, range);
    }
    renderer.flush();
    renderer.blocks
}

/// An inline style that's open.
enum Inline {
    Bold,
    Italic,
    Strike,
    Link(String),
    Image,
    Heading(u8),
}

/// A block being filled with text.
struct Open {
    kind: &'static str,
    range: Range<usize>,
    lines: Vec<Line>,
    /// What starts each line after the first: the quote bars and the
    /// list indentation.
    rest: Line,
}

struct Table {
    aligns: Vec<Alignment>,
    rows: Vec<Row>,
    cell: Option<String>,
}

struct Row {
    range: Range<usize>,
    header: bool,
    cells: Vec<String>,
}

struct Renderer {
    width: usize,
    /// Where each source line starts, then the end, as dffy counts lines.
    starts: Vec<usize>,
    blocks: Vec<Block>,
    quotes: usize,
    /// Each open list's next number; `None` for a bulleted one.
    lists: Vec<Option<u64>>,
    /// What the open list item or footnote starts with, until its first
    /// block takes it.
    marker: Option<String>,
    /// The open list item's range, for text a tight list leaves outside a
    /// paragraph.
    item: Option<Range<usize>>,
    footnote: bool,
    styles: Vec<Inline>,
    open: Option<Open>,
    code: Option<(Range<usize>, Option<String>, String)>,
    html: Option<(Range<usize>, String)>,
    table: Option<Table>,
}

impl Renderer {
    fn new(source: &str, width: usize) -> Self {
        let mut starts = vec![0];
        starts.extend(source.match_indices('\n').map(|(i, _)| i + 1));
        if starts.last() != Some(&source.len()) {
            starts.push(source.len());
        }
        Self {
            width,
            starts,
            blocks: Vec::new(),
            quotes: 0,
            lists: Vec::new(),
            marker: None,
            item: None,
            footnote: false,
            styles: Vec::new(),
            open: None,
            code: None,
            html: None,
            table: None,
        }
    }

    /// The source lines `range` (bytes) covers.
    fn lines(&self, range: &Range<usize>) -> Range<usize> {
        let count = self.starts.len() - 1;
        let line = |offset: usize| {
            self.starts
                .partition_point(|&s| s <= offset)
                .saturating_sub(1)
        };
        let start = line(range.start).min(count);
        let end = if range.end > range.start {
            line(range.end - 1) + 1
        } else {
            start + 1
        };
        start..end.min(count).max(start)
    }

    fn push_block(&mut self, kind: &'static str, range: &Range<usize>, lines: Vec<Line>) {
        let source = self.lines(range);
        self.blocks.push(Block {
            kind,
            source,
            lines,
            language: None,
        });
    }

    /// Ends the block being filled, if any.
    fn flush(&mut self) {
        if let Some(open) = self.open.take() {
            let mut lines = open.lines;
            if lines
                .last()
                .is_some_and(|l| l.iter().all(|s| s.text.trim().is_empty()))
                && lines.len() > 1
            {
                lines.pop();
            }
            self.push_block(open.kind, &open.range, lines);
        }
    }

    /// Adds a blank line after a top-level block, as the source has between
    /// paragraphs; list items and quoted lines stay together. The blank line
    /// is its own block with no source lines: folded into the block before,
    /// appending a list item would also change the item that was last.
    fn gap(&mut self) {
        if self.lists.is_empty()
            && self.quotes == 0
            && let Some(block) = self.blocks.last()
            && block.kind != GAP
        {
            let at = block.source.end;
            self.blocks.push(Block {
                kind: GAP,
                source: at..at,
                lines: vec![Vec::new()],
                language: None,
            });
        }
    }

    /// The quote bars every line inside a quote starts with.
    fn bars(&self) -> Line {
        (0..self.quotes)
            .map(|_| Span::styled("│ ", &["dim"]))
            .collect()
    }

    /// Starts a block, its first line led by the quote bars and the list
    /// item's marker or its indentation.
    fn open(&mut self, kind: &'static str, range: Range<usize>) {
        self.flush();
        let depth = self.lists.len();
        let lead = match self.marker.take() {
            Some(marker) => marker,
            None if depth > 0 => "  ".repeat(depth),
            None => String::new(),
        };
        let mut first = self.bars();
        let mut rest = self.bars();
        if !lead.is_empty() {
            rest.push(Span::plain(" ".repeat(columns::width(&lead))));
            first.push(Span::styled(lead, &["dim"]));
        }
        let kind = match (depth, self.footnote) {
            (_, true) => "footnote",
            (0, false) => kind,
            _ if kind == "paragraph" => "list-item",
            _ => kind,
        };
        self.open = Some(Open {
            kind,
            range,
            lines: vec![first],
            rest,
        });
    }

    /// Appends `text` in the open inline styles plus `extra`, opening a
    /// paragraph (or list item) when text comes outside one.
    fn text(&mut self, text: &str, extra: &[&'static str], range: &Range<usize>) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = &mut table.cell {
                cell.push_str(text);
            }
            return;
        }
        // A tight list's items hold text without a paragraph: the block
        // runs from the item's start to the end of its text, short of any
        // list nested in it.
        if self.open.is_none() {
            let start = self.item.as_ref().map_or(range.start, |item| item.start);
            self.open("paragraph", start..range.end);
        }
        if let Some(open) = &mut self.open {
            open.range.end = open.range.end.max(range.end);
        }
        let mut span = Span::styled(text, extra);
        for inline in &self.styles {
            match inline {
                Inline::Bold => span.style.push("bold"),
                Inline::Italic => span.style.push("italic"),
                Inline::Strike => span.style.extend(["strikethrough", "dim"]),
                Inline::Image => span.style.push("dim"),
                Inline::Link(url) => {
                    span.style.push("link");
                    span.url = Some(url.clone());
                }
                Inline::Heading(level) => {
                    span.style.push("heading");
                    span.level = Some(*level);
                }
            }
        }
        span.style.dedup();
        let Some(line) = self.open.as_mut().and_then(|o| o.lines.last_mut()) else {
            return;
        };
        match line.last_mut() {
            Some(last)
                if last.style == span.style && last.url == span.url && last.level == span.level =>
            {
                last.text.push_str(&span.text);
            }
            _ => line.push(span),
        }
    }

    fn new_line(&mut self) {
        if let Some(open) = &mut self.open {
            open.lines.push(open.rest.clone());
        }
    }

    fn event(&mut self, event: Event<'_>, range: Range<usize>) {
        if let Some((_, _, code)) = &mut self.code {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag, range),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, &[], &range),
            Event::Code(text) => self.text(&text, &["code"], &range),
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                self.text(&text, &["code"], &range);
            }
            Event::InlineHtml(html) => self.text(&html, &["dim"], &range),
            Event::Html(html) => match &mut self.html {
                Some((_, text)) => text.push_str(&html),
                None => self.text(&html, &["dim"], &range),
            },
            Event::FootnoteReference(label) => {
                self.text(&format!("[^{label}]"), &["dim"], &range);
            }
            Event::SoftBreak => self.text(" ", &[], &range),
            Event::HardBreak => self.new_line(),
            Event::Rule => {
                self.flush();
                let mut line = self.bars();
                line.push(Span::styled("─".repeat(self.width.clamp(3, 80)), &["dim"]));
                self.push_block("rule", &range, vec![line]);
                self.gap();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "☑ " } else { "☐ " };
                self.text(mark, if done { &["dim"] } else { &[] }, &range);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>, range: Range<usize>) {
        match tag {
            Tag::Paragraph => self.open("paragraph", range),
            Tag::Heading { level, .. } => {
                self.open("heading", range);
                self.styles
                    .push(Inline::Heading(u8::try_from(level as usize).unwrap_or(6)));
            }
            Tag::BlockQuote(_) => {
                self.flush();
                self.quotes += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((range, language, String::new()));
            }
            Tag::HtmlBlock => {
                self.flush();
                self.html = Some((range, String::new()));
            }
            Tag::List(start) => {
                self.flush();
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().max(1);
                let bullet = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        *n += 1;
                        format!("{}.", *n - 1)
                    }
                    _ => "•".to_owned(),
                };
                self.marker = Some(format!("{}{bullet} ", "  ".repeat(depth - 1)));
                self.item = Some(range);
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.footnote = true;
                self.marker = Some(format!("[^{label}]: "));
            }
            Tag::Table(aligns) => {
                self.flush();
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    cell: None,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Row {
                        range,
                        header: matches!(tag, Tag::TableHead),
                        cells: Vec::new(),
                    });
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell = Some(String::new());
                }
            }
            Tag::Emphasis => self.styles.push(Inline::Italic),
            Tag::Strong => self.styles.push(Inline::Bold),
            Tag::Strikethrough => self.styles.push(Inline::Strike),
            Tag::Link { dest_url, .. } => self.styles.push(Inline::Link(dest_url.into_string())),
            Tag::Image { .. } => {
                self.text("[image: ", &["dim"], &range);
                self.styles.push(Inline::Image);
            }
            Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
                self.gap();
            }
            TagEnd::Heading(_) => {
                self.styles.pop();
                self.flush();
                self.gap();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
                self.gap();
            }
            TagEnd::HtmlBlock => {
                if let Some((range, html)) = self.html.take() {
                    let lines = html
                        .lines()
                        .map(|l| {
                            let mut line = self.bars();
                            line.push(Span::styled(l, &["dim"]));
                            line
                        })
                        .collect();
                    self.push_block("html", &range, lines);
                    self.gap();
                }
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                self.gap();
            }
            TagEnd::Item => {
                self.flush();
                self.marker = None;
                self.item = None;
            }
            TagEnd::FootnoteDefinition => {
                self.flush();
                self.footnote = false;
                self.marker = None;
                self.gap();
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some(cell) = table.cell.take()
                    && let Some(row) = table.rows.last_mut()
                {
                    row.cells.push(cell.trim().to_owned());
                }
            }
            TagEnd::Table => self.end_table(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::Image => {
                self.styles.pop();
                self.text("]", &["dim"], &(0..0));
            }
            TagEnd::CodeBlock
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn end_code(&mut self) {
        let Some((range, language, code)) = self.code.take() else {
            return;
        };
        // A diagram's lines start on the line after the opening fence; its
        // first and last blocks take the fences too, as a code block would.
        // One that can't be drawn stays a code block.
        if language.as_deref() == Some("mermaid")
            && let Some(blocks) = dffy_render_mermaid::render(&code, self.width)
        {
            let fence = self.lines(&range);
            let count = blocks.len();
            for (i, block) in blocks.into_iter().enumerate() {
                let start = if i == 0 {
                    fence.start
                } else {
                    fence.start + 1 + block.source.start
                };
                let end = if i + 1 == count {
                    fence.end
                } else {
                    fence.start + 1 + block.source.end
                };
                self.blocks.push(Block {
                    kind: block.kind,
                    source: start..end.min(fence.end),
                    lines: block
                        .lines
                        .into_iter()
                        .map(|line| {
                            line.into_iter()
                                .map(|s| Span::styled(s.text, s.style))
                                .collect()
                        })
                        .collect(),
                    language: None,
                });
            }
            self.gap();
            return;
        }
        let mut lines: Vec<Line> = code
            .lines()
            .map(|l| vec![Span::styled(l, &["code"])])
            .collect();
        if lines.is_empty() {
            lines.push(vec![Span::styled("", &["code"])]);
        }
        let source = self.lines(&range);
        self.blocks.push(Block {
            kind: "code",
            source,
            lines,
            language,
        });
        self.gap();
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let widths = column_widths(&table.rows, columns, self.width);
        let border = |left: &str, middle: &str, right: &str| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            vec![Span::styled(
                format!("{left}{}{right}", parts.join(middle)),
                &["dim"],
            )]
        };
        let first = table
            .rows
            .first()
            .map(|r| r.range.clone())
            .unwrap_or_default();
        let last = table
            .rows
            .last()
            .map(|r| r.range.clone())
            .unwrap_or_default();
        self.push_block("table-border", &first, vec![border("┌", "┬", "┐")]);
        for row in &table.rows {
            self.push_block(
                if row.header {
                    "table-header"
                } else {
                    "table-row"
                },
                &row.range,
                row_lines(row, &widths, &table.aligns),
            );
            if row.header {
                self.push_block("table-border", &row.range, vec![border("├", "┼", "┤")]);
            }
        }
        self.push_block("table-border", &last, vec![border("└", "┴", "┘")]);
        self.gap();
    }
}

/// Each column's width: its widest cell, narrowed widest first until the
/// table fits `width` with its borders, but never below 3 columns.
fn column_widths(rows: &[Row], columns: usize, width: usize) -> Vec<usize> {
    let mut widths = vec![1; columns];
    for row in rows {
        for (i, cell) in row.cells.iter().enumerate() {
            widths[i] = widths[i].max(columns::width(cell));
        }
    }
    let frame = 3 * columns + 1;
    let room = width.saturating_sub(frame).max(3 * columns);
    while widths.iter().sum::<usize>() > room {
        let Some(widest) = widths.iter_mut().max().filter(|w| **w > 3) else {
            break;
        };
        *widest -= 1;
    }
    widths
}

/// `text` broken into lines of at most `width` columns, at spaces where it
/// can and inside words where it must.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    let mut line = String::new();
    let mut used = 0;
    for word in text.split_whitespace() {
        if used > 0 && used + 1 + columns::width(word) > width {
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
    lines
}

/// A row as lines of cells between `│`s, each cell wrapped to its column.
fn row_lines(row: &Row, widths: &[usize], aligns: &[Alignment]) -> Vec<Line> {
    let cells: Vec<Vec<String>> = widths
        .iter()
        .enumerate()
        .map(|(i, &w)| wrap(row.cells.get(i).map_or("", String::as_str), w))
        .collect();
    let height = cells.iter().map(Vec::len).max().unwrap_or(1);
    let style: &[&'static str] = if row.header { &["bold"] } else { &[] };
    (0..height)
        .map(|k| {
            let mut line = vec![Span::styled("│", &["dim"])];
            for (i, &w) in widths.iter().enumerate() {
                let text = cells[i].get(k).map_or("", String::as_str);
                let pad = w.saturating_sub(columns::width(text));
                let (left, right) = match aligns.get(i) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                line.push(Span::plain(" ".repeat(left + 1)));
                if !text.is_empty() {
                    line.push(Span::styled(text, style));
                }
                line.push(Span::plain(" ".repeat(right + 1)));
                line.push(Span::styled("│", &["dim"]));
            }
            line
        })
        .collect()
}

#[cfg(test)]
mod tests;

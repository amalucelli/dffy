//! Comment bodies drawn as Markdown (`CommonMark` with GitHub's tables,
//! strikethrough and task lists) within a thread's box. A body is rendered
//! once per box width and kept with the box, so a frame only copies its
//! rows.
//!
//! A line break in a paragraph stays a line break, as typed in the text
//! box, where enter starts a new line; long lines wrap at the box. HTML
//! comments, which bots keep state in, are hidden; other HTML shows dim,
//! as written.
//!
//! The rendering also keeps where each glyph came from in the source, so a
//! selection in the box copies the Markdown rather than what's drawn.

#[cfg(test)]
use std::cell::Cell;
use std::ops::Range;

use dffy_core::diff::Text;
use dffy_core::highlight;
use dffy_core::view::{Attributes, Syntax};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, Parser, Tag, TagEnd};
use unicode_width::{UnicodeWidthChar as _, UnicodeWidthStr as _};

use super::view::rendered::language;
use crate::render::{Canvas, plain};
use crate::review::model::snippet::pieces;
use crate::review::model::text::{checkbox, markdown_options as options, uncomment};
use crate::theme::{Ink, Role};

#[cfg(test)]
thread_local! {
    static RENDERED: Cell<usize> = const { Cell::new(0) };
}

/// Times [`render`] ran on this thread, to tell that drawing doesn't.
#[cfg(test)]
pub(crate) fn rendered_count() -> usize {
    RENDERED.with(Cell::get)
}

/// A comment as its box draws it: its rows, and where their text came from.
#[derive(Debug, Default)]
pub(crate) struct Body {
    pub rows: Canvas,
    pub map: SourceMap,
}

/// Where each glyph of a rendered comment came from in its Markdown.
#[derive(Debug, Default)]
pub(crate) struct SourceMap {
    /// In the order they're drawn, row by row.
    glyphs: Vec<Glyph>,
    /// The source of each element drawn without its markup: an inline
    /// style or link, inline code, a heading, a list item, a quote, a code
    /// block, a table.
    elements: Vec<Range<usize>>,
}

#[derive(Debug)]
struct Glyph {
    row: usize,
    col: usize,
    width: usize,
    /// Its character's bytes, or a whole construct's where what's drawn
    /// isn't in the source as such: an entity, HTML less its comments, an
    /// image's label, a task's box drawn in its text.
    src: Range<usize>,
}

impl SourceMap {
    /// What the cell at `row` and `col` of the rows stands for: its glyph's
    /// source. A cell with no glyph stands for a place between two, as an
    /// empty range: the end of the last glyph left of it on its row, else
    /// the start of the next glyph, else the end of the text.
    pub(crate) fn at(&self, row: usize, col: usize) -> Range<usize> {
        let from = self.glyphs.partition_point(|g| g.row < row);
        let rest = &self.glyphs[from..];
        let on_row = &rest[..rest.partition_point(|g| g.row == row)];
        if let Some(g) = on_row
            .iter()
            .find(|g| (g.col..g.col + g.width).contains(&col))
        {
            return g.src.clone();
        }
        let at = match on_row.iter().rev().find(|g| g.col < col) {
            Some(g) => g.src.end,
            None => rest
                .first()
                .map_or_else(|| self.text().end, |g| g.src.start),
        };
        at..at
    }

    /// The source from the first glyph's start to the last one's end.
    pub(crate) fn text(&self) -> Range<usize> {
        let start = self.glyphs.iter().map(|g| g.src.start).min();
        let end = self.glyphs.iter().map(|g| g.src.end).max();
        start.unwrap_or(0)..end.unwrap_or(0)
    }

    /// The source a selection from `anchor` to `cursor`, what [`Self::at`]
    /// gave for each, copies: from the earlier's start to the later's end,
    /// widened over each element whose whole text it holds, so the
    /// element's markup comes with it.
    pub(crate) fn span(&self, anchor: &Range<usize>, cursor: &Range<usize>) -> Range<usize> {
        let mut span = anchor.start.min(cursor.start)..anchor.end.max(cursor.end);
        let mut elements: Vec<&Range<usize>> = self.elements.iter().collect();
        // Inner ones first: a widened bold can complete its list item.
        elements.sort_by_key(|e| e.len());
        for e in elements {
            let inside = || {
                self.glyphs
                    .iter()
                    .filter(|g| e.start <= g.src.start && g.src.end <= e.end)
            };
            let start = inside().map(|g| g.src.start).min();
            let end = inside().map(|g| g.src.end).max();
            if start.is_some_and(|s| span.start <= s) && end.is_some_and(|e| e <= span.end) {
                span = span.start.min(e.start)..span.end.max(e.end);
            }
        }
        span
    }

    /// The row of the first glyph drawn from byte `at` of the source or a
    /// later one; `None` when none is.
    pub(crate) fn row_of(&self, at: usize) -> Option<usize> {
        self.glyphs.iter().find(|g| g.src.end > at).map(|g| g.row)
    }

    /// The columns of row `row`'s glyphs drawn from bytes of `range`.
    pub(crate) fn cells(
        &self,
        row: usize,
        range: Range<usize>,
    ) -> impl Iterator<Item = Range<usize>> + '_ {
        let from = self.glyphs.partition_point(|g| g.row < row);
        self.glyphs[from..]
            .iter()
            .take_while(move |g| g.row == row)
            .filter(move |g| g.src.start < range.end && range.start < g.src.end)
            .map(|g| g.col..g.col + g.width)
    }
}

/// The source of each character of something drawn, `None` for what the
/// rendering adds (bullets, quote bars, padding).
type Sources = Vec<Option<Range<usize>>>;

/// The source of each character of `text`, which an event of `source[at]`
/// gave: byte by byte where it's that source as is, or found in it for
/// inline code, whose source has its backticks; else all of `at`.
fn sources(source: &str, at: &Range<usize>, text: &str, inside: bool) -> Sources {
    let slice = source.get(at.clone()).unwrap_or_default();
    let base = if slice == text {
        Some(at.start)
    } else if inside {
        slice.find(text).map(|i| at.start + i)
    } else {
        None
    };
    text.char_indices()
        .map(|(i, ch)| Some(base.map_or_else(|| at.clone(), |b| b + i..b + i + ch.len_utf8())))
        .collect()
}

/// `text` escaped as [`plain`] draws it, with the source of each character
/// drawn, `src` giving those of `text`'s: an escape's characters all take
/// the one it stands for.
fn drawn(text: &str, src: &[Option<Range<usize>>]) -> (String, Sources) {
    if text.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return (text.to_owned(), src.to_vec());
    }
    let mut out = String::new();
    let mut sources = Vec::new();
    for (i, ch) in text.chars().enumerate() {
        let shown = plain(ch.encode_utf8(&mut [0; 4]));
        let src = src.get(i).cloned().flatten();
        sources.extend(std::iter::repeat_n(src, shown.chars().count()));
        out.push_str(&shown);
    }
    (out, sources)
}

/// `text`'s rows, its paragraphs wrapped to `width` columns, and where
/// their glyphs came from; code and tables keep their lines whole, for the
/// box to cut. `highlight` colors fenced code in the language its fence
/// names.
pub(crate) fn render(text: &str, width: usize, highlight: bool) -> Body {
    #[cfg(test)]
    RENDERED.with(|n| n.set(n.get() + 1));
    let mut writer = Writer {
        source: text,
        at: 0..0,
        c: Canvas::default(),
        map: SourceMap::default(),
        width: width.max(1),
        highlight,
        quotes: 0,
        lists: Vec::new(),
        marker: None,
        styles: Vec::new(),
        line: Vec::new(),
        line_src: Vec::new(),
        rows: 0,
        gap: None,
        code: None,
        code_src: Vec::new(),
        html: None,
        table: None,
    };
    for (event, at) in Parser::new_ext(text, options()).into_offset_iter() {
        if matches!(
            event,
            Event::Code(_)
                | Event::Start(
                    Tag::Strong
                        | Tag::Emphasis
                        | Tag::Strikethrough
                        | Tag::Link { .. }
                        | Tag::Image { .. }
                        | Tag::Heading { .. }
                        | Tag::Item
                        | Tag::BlockQuote(_)
                        | Tag::CodeBlock(_)
                        | Tag::Table(_)
                )
        ) {
            writer.map.elements.push(at.clone());
        }
        writer.at = at;
        writer.event(event);
    }
    Body {
        rows: writer.c,
        map: writer.map,
    }
}

/// An inline style that's open.
enum Inline {
    Bold,
    Italic,
    Strike,
    Link,
    Image,
    Heading(u8),
}

/// The ink of text under `styles`, as the rendered view draws the markdown
/// renderer plugin's styles of the same names; `code` for inline code.
fn ink(styles: &[Inline], code: bool) -> Ink {
    let (mut syntax, mut attributes) = (None, Attributes::NONE);
    for style in styles {
        match style {
            Inline::Bold => attributes |= Attributes::BOLD,
            Inline::Italic => attributes |= Attributes::ITALIC,
            // Struck text is dim, as in a rendered view: there's no
            // strikethrough attribute.
            Inline::Strike | Inline::Image => attributes |= Attributes::DIM,
            Inline::Link => {
                syntax = syntax.or(Some(Syntax::Function));
                attributes |= Attributes::UNDERLINE;
            }
            Inline::Heading(level) => {
                attributes |= Attributes::BOLD;
                if *level <= 2 {
                    syntax = Some(Syntax::Keyword);
                }
                if *level == 1 {
                    attributes |= Attributes::UNDERLINE;
                }
            }
        }
    }
    if code {
        syntax = Some(Syntax::String);
    }
    Ink {
        role: Role::Plain,
        syntax,
        attributes,
    }
}

const DIM: Ink = Ink {
    role: Role::Dim,
    syntax: None,
    attributes: Attributes::NONE,
};

const PLAIN: Ink = Ink {
    role: Role::Plain,
    syntax: None,
    attributes: Attributes::NONE,
};

type Spans = Vec<(Ink, String)>;

/// Appends `text` in `ink`, to the last span when it has the same.
fn push(spans: &mut Spans, ink: Ink, text: &str) {
    match spans.last_mut() {
        Some((last, s)) if *last == ink => s.push_str(text),
        _ => spans.push((ink, text.to_owned())),
    }
}

fn width_of(spans: &[(Ink, String)]) -> usize {
    spans.iter().map(|(_, s)| s.width()).sum()
}

/// A list that's open.
struct List {
    /// The next item's number; `None` for bullets.
    next: Option<u64>,
    /// The column its open item's text starts at.
    indent: usize,
}

/// An escaped cell and its characters' sources.
type CellText = (String, Sources);

struct Table {
    aligns: Vec<Alignment>,
    /// Row by row; the first row is the header.
    rows: Vec<Vec<CellText>>,
    cell: Option<CellText>,
}

struct Writer<'a> {
    source: &'a str,
    /// The source of the event being written.
    at: Range<usize>,
    c: Canvas,
    map: SourceMap,
    width: usize,
    highlight: bool,
    quotes: usize,
    lists: Vec<List>,
    /// What the open list item's first row starts with, until it's drawn.
    marker: Option<(Ink, String)>,
    styles: Vec<Inline>,
    /// The line of text being filled, up to its next break.
    line: Spans,
    /// The source of each of its characters.
    line_src: Sources,
    /// Rows the open paragraph or heading has drawn.
    rows: usize,
    /// A blank row is due before the next block's first row, in this many
    /// quotes when the next block isn't in fewer.
    gap: Option<usize>,
    /// A code block's language and text.
    code: Option<(Option<String>, String)>,
    /// The source of each character of its text.
    code_src: Sources,
    html: Option<String>,
    table: Option<Table>,
}

impl Writer<'_> {
    /// What a row starts with: the quote bars, then the list item's marker
    /// on its first row, its indentation on the rest.
    fn lead(&mut self) -> Spans {
        let mut lead = Vec::new();
        for _ in 0..self.quotes {
            push(&mut lead, DIM, "│ ");
        }
        match self.marker.take() {
            Some((ink, marker)) => push(&mut lead, ink, &marker),
            None => {
                if let Some(list) = self.lists.last() {
                    push(&mut lead, PLAIN, &" ".repeat(list.indent));
                }
            }
        }
        lead
    }

    /// Draws a row of `spans` after `lead`, under a blank row when one is
    /// due; `src` has the source of each character of `spans`.
    fn row(
        &mut self,
        lead: &[(Ink, String)],
        spans: &[(Ink, String)],
        src: &[Option<Range<usize>>],
    ) {
        if let Some(quotes) = self.gap.take()
            && !self.c.is_empty()
        {
            for _ in 0..quotes.min(self.quotes) {
                self.c.push(Role::Dim, "│ ");
            }
            self.c.end_line();
        }
        for (ink, text) in lead.iter().chain(spans) {
            self.c.push_ink(*ink, text);
        }
        let row = self.c.len();
        let mut col = width_of(lead);
        for (i, ch) in spans.iter().flat_map(|(_, s)| s.chars()).enumerate() {
            let width = ch.width().unwrap_or(0);
            if width > 0
                && let Some(Some(src)) = src.get(i)
            {
                self.map.glyphs.push(Glyph {
                    row,
                    col,
                    width,
                    src: src.clone(),
                });
            }
            col += width;
        }
        self.c.end_line();
    }

    /// A blank row is due after a block, but not between a list's items.
    fn ended(&mut self) {
        if self.lists.is_empty() {
            self.gap = Some(self.quotes);
        }
    }

    /// Draws the line of text, wrapped to the width its lead leaves.
    fn break_line(&mut self) {
        let line = std::mem::take(&mut self.line);
        let src = std::mem::take(&mut self.line_src);
        let first = self.lead();
        let rest = self.lead();
        let room = |lead: &Spans| self.width.saturating_sub(width_of(lead)).max(1);
        let rows = wrap(&line, room(&first), room(&rest));
        // A row's characters are the line's in order, less the spaces
        // wrapping drops.
        let chars: Vec<char> = line.iter().flat_map(|(_, s)| s.chars()).collect();
        let mut k = 0;
        for (i, row) in rows.iter().enumerate() {
            let row_src: Sources = row
                .iter()
                .flat_map(|(_, s)| s.chars())
                .map(|ch| {
                    while chars.get(k).is_some_and(|&c| c != ch) {
                        k += 1;
                    }
                    k += 1;
                    src.get(k - 1).cloned().flatten()
                })
                .collect();
            self.row(if i == 0 { &first } else { &rest }, row, &row_src);
        }
        self.rows += rows.len();
    }

    /// Draws the line of text a block leaves before the next one starts,
    /// as a tight list's item does before a list nested in it.
    fn flush(&mut self) {
        if !self.line.is_empty() {
            self.break_line();
        }
    }

    /// Ends a paragraph or heading; one whose text was all hidden HTML
    /// draws nothing.
    fn end_text(&mut self) {
        if self.rows > 0 || self.line.iter().any(|(_, s)| !s.trim().is_empty()) {
            self.break_line();
            self.ended();
        }
        self.line.clear();
        self.line_src.clear();
        self.rows = 0;
    }

    fn text(&mut self, text: &str, code: bool) {
        self.put(ink(&self.styles, code), text, code);
    }

    /// Adds `text`, the event's, to the line, or unstyled to the table's
    /// open cell. `inside` finds it within the event's source, as inline
    /// code's text is.
    fn put(&mut self, ink: Ink, text: &str, inside: bool) {
        let (text, src) = drawn(text, &sources(self.source, &self.at, text, inside));
        if let Some((cell, cell_src)) = self.table.as_mut().and_then(|t| t.cell.as_mut()) {
            cell.push_str(&text);
            cell_src.extend(src);
        } else {
            push(&mut self.line, ink, &text);
            self.line_src.extend(src);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        if let Some((_, code)) = &mut self.code {
            match event {
                Event::Text(text) => {
                    code.push_str(&text);
                    let src = sources(self.source, &self.at, &text, false);
                    self.code_src.extend(src);
                }
                Event::End(TagEnd::CodeBlock) => self.end_code(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text, false),
            Event::Code(text) => self.text(&text, true),
            Event::InlineHtml(html) => {
                self.put(DIM, &uncomment(&html).replace('\n', " "), false);
            }
            Event::Html(html) => match &mut self.html {
                Some(block) => block.push_str(&html),
                None => self.html = Some(html.into_string()),
            },
            Event::SoftBreak | Event::HardBreak => self.break_line(),
            Event::Rule => {
                self.flush();
                let lead = self.lead();
                let rule = "─".repeat(self.width.saturating_sub(width_of(&lead)).max(1));
                let src = vec![Some(self.at.clone()); rule.chars().count()];
                self.row(&lead, &[(DIM, rule)], &src);
                self.ended();
            }
            Event::TaskListMarker(done) => match &mut self.marker {
                // A task's box takes the place of its bullet.
                Some((ink, marker)) if marker.ends_with("• ") => {
                    marker.truncate(marker.len() - "• ".len());
                    marker.push_str(checkbox(done));
                    *ink = PLAIN;
                }
                _ => self.put(PLAIN, checkbox(done), false),
            },
            Event::InlineMath(_) | Event::DisplayMath(_) | Event::FootnoteReference(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        if matches!(
            tag,
            Tag::Paragraph
                | Tag::Heading { .. }
                | Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::HtmlBlock
                | Tag::List(_)
                | Tag::Table(_)
        ) {
            self.flush();
        }
        match tag {
            Tag::Heading { level, .. } => self.styles.push(Inline::Heading(level as u8)),
            Tag::BlockQuote(_) => self.quotes += 1,
            Tag::CodeBlock(kind) => {
                let language = match kind {
                    CodeBlockKind::Fenced(info) => info
                        .split(|c: char| c.is_whitespace() || c == ',' || c == '{')
                        .next()
                        .filter(|l| !l.is_empty())
                        .map(str::to_owned),
                    CodeBlockKind::Indented => None,
                };
                self.code = Some((language, String::new()));
            }
            Tag::List(start) => {
                let indent = self.lists.last().map_or(0, |l| l.indent);
                self.lists.push(List {
                    next: start,
                    indent,
                });
            }
            Tag::Item => {
                // A nested list's items start at its parent item's text.
                let parent = self
                    .lists
                    .len()
                    .checked_sub(2)
                    .map_or(0, |i| self.lists[i].indent);
                if let Some(list) = self.lists.last_mut() {
                    let bullet = match &mut list.next {
                        Some(n) => {
                            *n += 1;
                            format!("{}. ", *n - 1)
                        }
                        None => "• ".to_owned(),
                    };
                    list.indent = parent + bullet.width();
                    // An item that starts with a list shares its first
                    // row, as `- - a` draws `• • a`.
                    let lead = self
                        .marker
                        .take()
                        .map_or_else(|| " ".repeat(parent), |(_, m)| m);
                    self.marker = Some((DIM, format!("{lead}{bullet}")));
                }
            }
            Tag::Table(aligns) => {
                self.table = Some(Table {
                    aligns,
                    rows: Vec::new(),
                    cell: None,
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(table) = &mut self.table {
                    table.cell = Some((String::new(), Vec::new()));
                }
            }
            Tag::Emphasis => self.styles.push(Inline::Italic),
            Tag::Strong => self.styles.push(Inline::Bold),
            Tag::Strikethrough => self.styles.push(Inline::Strike),
            Tag::Link { .. } => self.styles.push(Inline::Link),
            Tag::Image { .. } => {
                self.styles.push(Inline::Image);
                self.text("[image: ", false);
            }
            Tag::Paragraph
            | Tag::HtmlBlock
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.end_text(),
            TagEnd::Heading(_) => {
                self.end_text();
                self.styles.pop();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quotes = self.quotes.saturating_sub(1);
                self.ended();
            }
            TagEnd::HtmlBlock => self.end_html(),
            TagEnd::List(_) => {
                self.lists.pop();
                self.ended();
            }
            TagEnd::Item => {
                // A tight list's item holds its text outside a paragraph;
                // an empty one still shows its bullet.
                if !self.line.is_empty() || self.marker.is_some() {
                    self.break_line();
                }
                self.rows = 0;
            }
            TagEnd::TableCell => {
                if let Some(table) = &mut self.table
                    && let Some((cell, src)) = table.cell.take()
                    && let Some(row) = table.rows.last_mut()
                {
                    let trimmed = cell.trim();
                    let skip = cell[..cell.len() - cell.trim_start().len()].chars().count();
                    let src = src.iter().skip(skip).take(trimmed.chars().count());
                    row.push((trimmed.to_owned(), src.cloned().collect()));
                }
            }
            TagEnd::Table => self.end_table(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                self.styles.pop();
            }
            TagEnd::Image => {
                self.text("]", false);
                self.styles.pop();
            }
            TagEnd::CodeBlock
            | TagEnd::TableHead
            | TagEnd::TableRow
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    /// Draws an HTML block's lines less its comments, each from where it's
    /// found in the block's source after the line before, else from the
    /// whole block.
    fn end_html(&mut self) {
        let html = uncomment(&self.html.take().unwrap_or_default());
        let block = self.at.clone();
        let mut from = block.start;
        let mut shown = false;
        for line in html.lines().filter(|l| !l.trim().is_empty()) {
            let found = self
                .source
                .get(from..block.end)
                .and_then(|s| s.find(line))
                .map(|i| from + i);
            let at = found.map_or_else(|| block.clone(), |i| i..i + line.len());
            from = at.end.min(block.end);
            let (text, src) = drawn(line, &sources(self.source, &at, line, false));
            let lead = self.lead();
            self.row(&lead, &[(DIM, text)], &src);
            shown = true;
        }
        if shown {
            self.ended();
        }
    }

    /// Draws a code block indented, each line whole: in its language's
    /// colors when its fence names one dffy highlights, else as inline
    /// code is.
    fn end_code(&mut self) {
        let Some((name, code)) = self.code.take() else {
            return;
        };
        let src = std::mem::take(&mut self.code_src);
        let spans = name.filter(|_| self.highlight).and_then(|name| {
            let lang = language(crate::grammars(), &name)?;
            let text = Text::new(code.as_bytes().to_vec());
            Some(highlight::parse(lang, text.bytes())?.spans(&text, 0..text.len()))
        });
        // The characters of the lines before, to find each line's sources.
        let mut before = 0;
        for (i, raw) in code.split_inclusive('\n').enumerate() {
            // As `str::lines` cuts them.
            let line = raw
                .strip_suffix('\n')
                .map_or(raw, |l| l.strip_suffix('\r').unwrap_or(l));
            let n = line.chars().count();
            let (_, line_src) = drawn(line, src.get(before..before + n).unwrap_or_default());
            before += raw.chars().count();
            let mut lead = self.lead();
            push(&mut lead, PLAIN, "  ");
            let row = match &spans {
                Some(spans) => pieces(line, Some(spans.line(i)))
                    .into_iter()
                    .map(|(syntax, text)| {
                        let ink = Ink {
                            syntax,
                            ..Role::Plain.into()
                        };
                        (ink, plain(&text))
                    })
                    .collect(),
                None => vec![(ink(&[], true), plain(line))],
            };
            self.row(&lead, &row, &line_src);
        }
        self.ended();
    }

    /// Draws a table with box characters, each column as wide as its
    /// widest cell, for the box to cut.
    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let mut widths = vec![1; columns];
        for row in &table.rows {
            for (w, (cell, _)) in widths.iter_mut().zip(row) {
                *w = (*w).max(cell.width());
            }
        }
        let border = |ends: [&str; 3]| {
            let parts: Vec<String> = widths.iter().map(|w| "─".repeat(w + 2)).collect();
            let row = vec![(
                DIM,
                format!("{}{}{}", ends[0], parts.join(ends[1]), ends[2]),
            )];
            (row, Vec::new())
        };
        let header = Ink {
            attributes: Attributes::BOLD,
            ..PLAIN
        };
        let mut rows = vec![border(["┌", "┬", "┐"])];
        for (i, cells) in table.rows.iter().enumerate() {
            let mut row = vec![(DIM, "│".to_owned())];
            let mut src: Sources = vec![None];
            for (k, &w) in widths.iter().enumerate() {
                let (cell, cell_src) = cells
                    .get(k)
                    .map_or(("", &[][..]), |(c, s)| (c.as_str(), s.as_slice()));
                let pad = w - cell.width();
                let (left, right) = match table.aligns.get(k) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                push(&mut row, PLAIN, &" ".repeat(left + 1));
                push(&mut row, if i == 0 { header } else { PLAIN }, cell);
                push(&mut row, PLAIN, &" ".repeat(right + 1));
                push(&mut row, DIM, "│");
                src.extend(std::iter::repeat_n(None, left + 1));
                src.extend(cell_src.iter().cloned());
                src.extend(std::iter::repeat_n(None, right + 2));
            }
            rows.push((row, src));
            if i == 0 {
                rows.push(border(["├", "┼", "┤"]));
            }
        }
        rows.push(border(["└", "┴", "┘"]));
        for (row, src) in rows {
            let lead = self.lead();
            self.row(&lead, &row, &src);
        }
        self.ended();
    }
}

/// `spans` broken into rows at spaces, the first `first` columns wide and
/// the rest `rest`, and inside a word wider than its row; a run of spaces
/// is one. No spans make one empty row.
fn wrap(spans: &[(Ink, String)], first: usize, rest: usize) -> Vec<Spans> {
    // Each word, with the ink of the space before it: a link's spaces stay
    // underlined.
    let mut words: Vec<(Ink, Vec<(Ink, &str)>)> = Vec::new();
    let mut space = PLAIN;
    let mut word: Option<(Ink, Vec<(Ink, &str)>)> = None;
    for (ink, text) in spans {
        for (i, part) in text.split(' ').enumerate() {
            if i > 0 {
                words.extend(word.take());
                space = *ink;
            }
            if !part.is_empty() {
                word.get_or_insert_with(|| (space, Vec::new()))
                    .1
                    .push((*ink, part));
            }
        }
    }
    words.extend(word);
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut used = 0;
    let room = |rows: &Vec<Spans>| if rows.is_empty() { first } else { rest };
    for (space, word) in words {
        let w: usize = word.iter().map(|(_, s)| s.width()).sum();
        if used > 0 && used + 1 + w > room(&rows) {
            rows.push(std::mem::take(&mut row));
            used = 0;
        } else if used > 0 {
            push(&mut row, space, " ");
            used += 1;
        }
        for (ink, text) in word {
            if used + text.width() <= room(&rows) {
                push(&mut row, ink, text);
                used += text.width();
                continue;
            }
            for ch in text.chars() {
                let w = ch.width().unwrap_or(0);
                if used > 0 && used + w > room(&rows) {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                push(&mut row, ink, ch.encode_utf8(&mut [0; 4]));
                used += w;
            }
        }
    }
    rows.push(row);
    rows
}

#[cfg(test)]
mod tests;

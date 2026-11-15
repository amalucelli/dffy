//! Syntax highlighting with tree-sitter. A file side is parsed once; its
//! highlight query then runs only over the lines a caller asks for, through a
//! byte-range-limited query cursor, so a large file costs one parse plus the
//! lines on screen. The same pass collects leaf-node boundaries, which
//! intra-line emphasis uses as tokens.
//!
//! Queries are compiled on first use per language and kept for the process:
//! compiling all of them costs about 80ms, one costs 3–35ms.

mod lang;

use std::ops::{ControlFlow, Range};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

pub use lang::Language;

use crate::cache::{self, Cache};
use crate::diff::{Text, tokenize};
use crate::git::ObjectId;

/// Sides larger than this aren't highlighted. A 1.8MB C file takes about
/// 250ms to parse and 86MB to hold; this keeps the parse under about 80ms.
pub const MAX_HIGHLIGHT_BYTES: usize = 512 << 10;

/// A parse that takes longer than this is abandoned and the side stays
/// plain, whatever its size: pathological input, or a slow machine.
const PARSE_BUDGET: Duration = Duration::from_millis(200);

/// Sides at least this large get their whole-file spans stored in the disk
/// cache, where a hit beats the parse it replaces (see [`Highlighter`]).
pub const CACHE_MIN_BYTES: usize = 64 << 10;

/// What a highlighted stretch of text is. Capture names from the bundled
/// queries map onto these; a theme gives each one a style.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Syntax {
    /// Keywords, conditionals, loops, includes and storage classes; also
    /// markup headings.
    Keyword,
    /// String and character literals; also markup code spans.
    String,
    /// Escape sequences inside strings.
    Escape,
    /// Comments of every kind.
    Comment,
    /// Function and method names; also markup links.
    Function,
    /// Type names and constructors.
    Type,
    /// Integer and float literals.
    Number,
    /// Constants and booleans.
    Constant,
    /// Operators.
    Operator,
    /// Brackets, delimiters and separators.
    Punctuation,
    /// Properties, fields and mapping keys.
    Property,
    /// Built-in variables such as `self` or `this`.
    VariableBuiltin,
    /// Attributes and annotations.
    Attribute,
    /// Markup tags.
    Tag,
    /// Labels, as for `goto`.
    Label,
}

impl Syntax {
    /// Every role, in discriminant order. Disk cache keys include each
    /// role's [`Syntax::key`], so changing this invalidates cached spans.
    pub const ALL: [Self; 15] = [
        Self::Keyword,
        Self::String,
        Self::Escape,
        Self::Comment,
        Self::Function,
        Self::Type,
        Self::Number,
        Self::Constant,
        Self::Operator,
        Self::Punctuation,
        Self::Property,
        Self::VariableBuiltin,
        Self::Attribute,
        Self::Tag,
        Self::Label,
    ];

    /// The name themes use, after a `syntax-` prefix.
    #[must_use]
    pub fn key(self) -> &'static str {
        match self {
            Self::Keyword => "keyword",
            Self::String => "string",
            Self::Escape => "escape",
            Self::Comment => "comment",
            Self::Function => "function",
            Self::Type => "type",
            Self::Number => "number",
            Self::Constant => "constant",
            Self::Operator => "operator",
            Self::Punctuation => "punctuation",
            Self::Property => "property",
            Self::VariableBuiltin => "variable-builtin",
            Self::Attribute => "attribute",
            Self::Tag => "tag",
            Self::Label => "label",
        }
    }

    fn from_u8(n: u8) -> Option<Self> {
        Self::ALL.get(usize::from(n)).copied()
    }

    /// The role of a query capture name such as `function.method`. Names
    /// without one (`variable`, `none`, `embedded`) leave the text plain.
    fn from_capture(name: &str) -> Option<Self> {
        let specific = match name {
            "variable.builtin" => Some(Self::VariableBuiltin),
            "string.escape" | "escape" => Some(Self::Escape),
            "string.special.key" => Some(Self::Property),
            "constructor" => Some(Self::Type),
            "text.title" | "markup.heading" => Some(Self::Keyword),
            "text.literal" | "markup.raw" => Some(Self::String),
            "text.uri" | "text.reference" | "markup.link" => Some(Self::Function),
            _ => None,
        };
        specific.or_else(|| {
            Some(match name.split('.').next().unwrap_or(name) {
                "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass" => {
                    Self::Keyword
                }
                "string" | "character" => Self::String,
                "comment" => Self::Comment,
                "function" | "method" => Self::Function,
                "type" => Self::Type,
                "number" | "float" => Self::Number,
                "constant" | "boolean" => Self::Constant,
                "operator" => Self::Operator,
                "punctuation" | "delimiter" => Self::Punctuation,
                "property" | "field" => Self::Property,
                "attribute" => Self::Attribute,
                "tag" => Self::Tag,
                "label" => Self::Label,
                _ => return None,
            })
        })
    }
}

/// A highlighted stretch of one line, as byte offsets from the line's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Run {
    /// First byte, inclusive.
    pub start: u32,
    /// Past the last byte; runs on one line never overlap and come in order.
    pub end: u32,
    /// What the text is.
    pub syntax: Syntax,
}

struct Compiled {
    query: Query,
    /// The role of each capture, by capture index.
    roles: Vec<Option<Syntax>>,
}

/// Where the compiled highlight query of `lang` is kept.
fn query_slot(lang: Language) -> Option<&'static OnceLock<Option<Compiled>>> {
    static QUERIES: [OnceLock<Option<Compiled>>; Language::ALL.len()] =
        [const { OnceLock::new() }; Language::ALL.len()];
    Language::ALL
        .iter()
        .position(|&l| l == lang)
        .map(|index| &QUERIES[index])
}

/// The compiled highlight query of `lang`, compiling it on first use.
/// `None` if the bundled query doesn't compile, which a test rules out.
fn compiled(lang: Language) -> Option<&'static Compiled> {
    query_slot(lang)?
        .get_or_init(|| {
            let query = Query::new(&lang.grammar(), &lang.highlights().concat()).ok()?;
            let roles = query
                .capture_names()
                .iter()
                .map(|name| Syntax::from_capture(name))
                .collect();
            Some(Compiled { query, roles })
        })
        .as_ref()
}

/// A parsed side.
pub struct Parsed {
    tree: Tree,
    lang: Language,
}

/// Parses `source` as `lang`. `None` when it's larger than
/// [`MAX_HIGHLIGHT_BYTES`], the parse runs past its time budget, or the
/// language's query doesn't compile.
#[must_use]
pub fn parse(lang: Language, source: &[u8]) -> Option<Parsed> {
    if source.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let tree = if query_slot(lang)?.get().is_some() {
        compiled(lang)?;
        parse_tree(lang, source, PARSE_BUDGET)?
    } else {
        // The language's first parse compiles its query beside it, rather
        // than waiting the compile out first.
        std::thread::scope(|s| {
            let query = std::thread::Builder::new().spawn_scoped(s, || compiled(lang).is_some());
            let tree = parse_tree(lang, source, PARSE_BUDGET);
            let query = match query {
                Ok(query) => query
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e)),
                // Out of threads: compile here instead of losing the
                // highlighting (or the process, as `Scope::spawn` panics).
                Err(_) => compiled(lang).is_some(),
            };
            query.then_some(tree).flatten()
        })?
    };
    Some(Parsed { tree, lang })
}

/// The syntax tree of `source`, or `None` when parsing takes longer than
/// `budget`.
pub(crate) fn parse_tree(lang: Language, source: &[u8], budget: Duration) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&lang.grammar()).ok()?;
    let started = Instant::now();
    let mut over_budget = |_: &tree_sitter::ParseState| {
        if started.elapsed() > budget {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut over_budget);
    parser.parse_with_options(
        &mut |offset, _| source.get(offset..).unwrap_or_default(),
        None,
        Some(options),
    )
}

impl Parsed {
    /// Syntax runs and leaf tokens for `lines` of `text`, which must be the
    /// text this was parsed from.
    #[must_use]
    pub fn spans(&self, text: &Text, lines: Range<usize>) -> Spans {
        let start = text.offset(lines.start);
        let end = text.offset(lines.end);
        let mut roles = vec![PLAIN; end - start];
        if let Some(compiled) = compiled(self.lang) {
            paint(compiled, &self.tree, text.bytes(), start..end, &mut roles);
        }
        let mut spans = Spans {
            first: lines.start,
            line_runs: Vec::with_capacity(lines.len() + 1),
            runs: Vec::new(),
            leaves: Vec::new(),
        };
        for i in lines {
            spans.line_runs.push(len_u32(spans.runs.len()));
            let from = text.offset(i);
            let content = content_len(text.line(i));
            let mut run: Option<Run> = None;
            for (offset, &role) in roles[from - start..from - start + content]
                .iter()
                .enumerate()
            {
                let at = len_u32(offset);
                match (&mut run, Syntax::from_u8(role)) {
                    (Some(r), Some(s)) if r.syntax == s && r.end == at => r.end = at + 1,
                    (_, s) => {
                        spans.runs.extend(run.take());
                        run = s.map(|syntax| Run {
                            start: at,
                            end: at + 1,
                            syntax,
                        });
                    }
                }
            }
            spans.runs.extend(run);
        }
        spans.line_runs.push(len_u32(spans.runs.len()));
        collect_leaves(&self.tree, start..end, &mut spans.leaves);
        spans
    }
}

const PLAIN: u8 = u8::MAX;

/// Writes each captured node's role into `roles` (one byte per byte of
/// `range`), outer nodes first so inner ones override them. When several
/// patterns capture the same node the last pattern wins: the bundled
/// queries list a catch-all like `(identifier) @variable` first and the
/// specific cases after it. A capture without a role still claims the node
/// but paints nothing, so the enclosing node's role shows through.
fn paint(compiled: &Compiled, tree: &Tree, source: &[u8], range: Range<usize>, roles: &mut [u8]) {
    struct Capture {
        start: usize,
        end: usize,
        pattern: usize,
        order: usize,
        role: Option<Syntax>,
    }
    let mut cursor = QueryCursor::new();
    cursor.set_byte_range(range.clone());
    let mut captures = cursor.captures(&compiled.query, tree.root_node(), source);
    let mut by_node: FxHashMap<usize, usize> = FxHashMap::default();
    let mut found: Vec<Capture> = Vec::new();
    while let Some((m, i)) = captures.next() {
        let capture = m.captures()[*i];
        let node = capture.node;
        let role = compiled.roles[capture.index as usize];
        let pattern = m.pattern_index;
        match by_node.get(&node.id()) {
            Some(&k) if found[k].pattern >= pattern => {}
            Some(&k) => {
                found[k].pattern = pattern;
                found[k].role = role;
            }
            None => {
                by_node.insert(node.id(), found.len());
                found.push(Capture {
                    start: node.start_byte(),
                    end: node.end_byte(),
                    pattern,
                    order: found.len(),
                    role,
                });
            }
        }
    }
    found.sort_unstable_by_key(|c| (c.start, std::cmp::Reverse(c.end), c.order));
    for c in found {
        let Some(role) = c.role else { continue };
        let (from, to) = (c.start.max(range.start), c.end.min(range.end));
        if from < to {
            roles[from - range.start..to - range.start].fill(role as u8);
        }
    }
}

/// Appends the leaf nodes overlapping `range`, in order, clipped to it.
/// Zero-width leaves (tokens the parser inserted to recover) are skipped.
fn collect_leaves(tree: &Tree, range: Range<usize>, out: &mut Vec<(u32, u32)>) {
    let mut cursor = tree.walk();
    'walk: loop {
        let node = cursor.node();
        // Nodes come in document order, so nothing later can overlap.
        if node.start_byte() >= range.end {
            break;
        }
        let overlaps = node.end_byte() > range.start;
        if overlaps && node.child_count() > 0 && cursor.goto_first_child() {
            continue;
        }
        if overlaps && node.child_count() == 0 {
            let (from, to) = (
                node.start_byte().max(range.start),
                node.end_byte().min(range.end),
            );
            out.push((len_u32(from), len_u32(to)));
        }
        while !cursor.goto_next_sibling() {
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }
}

#[expect(
    clippy::expect_used,
    reason = "a Parsed comes only from parse, which refuses sources over MAX_HIGHLIGHT_BYTES (512 KiB), and spans requires that same text, so offsets and run counts stay far below 2^32"
)]
fn len_u32(n: usize) -> u32 {
    u32::try_from(n).expect("highlighted text is under MAX_HIGHLIGHT_BYTES")
}

fn content_len(line: &[u8]) -> usize {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line).len()
}

/// Syntax runs and leaf tokens for a range of lines of one side.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Spans {
    first: usize,
    /// For each line, the index of its first run, plus a final entry.
    line_runs: Vec<u32>,
    runs: Vec<Run>,
    /// Leaf nodes as byte ranges of the whole text, in order, clipped to
    /// the lines.
    leaves: Vec<(u32, u32)>,
}

impl Spans {
    /// The lines covered.
    #[must_use]
    pub fn lines(&self) -> Range<usize> {
        self.first..self.first + self.line_runs.len().saturating_sub(1)
    }

    /// The runs of line `i`; none outside [`Spans::lines`].
    #[must_use]
    pub fn line(&self, i: usize) -> &[Run] {
        if !self.lines().contains(&i) {
            return &[];
        }
        let k = i - self.first;
        &self.runs[self.line_runs[k] as usize..self.line_runs[k + 1] as usize]
    }

    /// The spans of `lines`, which must lie within [`Spans::lines`].
    #[must_use]
    pub fn slice(&self, text: &Text, lines: Range<usize>) -> Self {
        let k = lines.start - self.first..lines.end - self.first;
        let first_run = self.line_runs[k.start];
        let (start, end) = (
            len_u32(text.offset(lines.start)),
            len_u32(text.offset(lines.end)),
        );
        let from = self.leaves.partition_point(|&(_, e)| e <= start);
        let to = self.leaves.partition_point(|&(s, _)| s < end);
        Self {
            first: lines.start,
            line_runs: self.line_runs[k.start..=k.end]
                .iter()
                .map(|&r| r - first_run)
                .collect(),
            runs: self.runs[first_run as usize..self.line_runs[k.end] as usize].to_vec(),
            leaves: self.leaves[from..to]
                .iter()
                .map(|&(s, e)| (s.max(start), e.min(end)))
                .collect(),
        }
    }

    /// Token boundaries for intra-line emphasis on line `i` of `text`
    /// (without its terminator): each leaf node is a token, except that
    /// leaves holding whitespace (strings, comments) and the gaps between
    /// leaves are split further by [`tokenize`]. Lines outside
    /// [`Spans::lines`] are tokenized plainly.
    #[must_use]
    pub fn tokens(&self, text: &Text, i: usize) -> Vec<Range<usize>> {
        let line = text.line(i);
        let content = &line[..content_len(line)];
        if !self.lines().contains(&i) {
            return tokenize(content);
        }
        let start = text.offset(i);
        let end = start + content.len();
        let mut out = Vec::new();
        let mut pos = 0;
        let split = |out: &mut Vec<Range<usize>>, from: usize, to: usize| {
            out.extend(
                tokenize(&content[from..to])
                    .into_iter()
                    .map(|r| r.start + from..r.end + from),
            );
        };
        let first = self.leaves.partition_point(|&(_, e)| e as usize <= start);
        for &(s, e) in &self.leaves[first..] {
            let (s, e) = (s as usize, e as usize);
            if s >= end {
                break;
            }
            let (from, to) = (s.max(start) - start, e.min(end) - start);
            if from >= to || from < pos {
                continue;
            }
            if from > pos {
                split(&mut out, pos, from);
            }
            if content[from..to].iter().any(u8::is_ascii_whitespace) {
                split(&mut out, from, to);
            } else {
                out.push(from..to);
            }
            pos = to;
        }
        if pos < content.len() {
            split(&mut out, pos, content.len());
        }
        out
    }

    /// A compact binary form for the disk cache.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.runs.len() * 4 + self.leaves.len() * 3);
        let mut put = |n: usize| cache::put_varint(&mut out, n as u64);
        put(self.first);
        put(self.line_runs.len());
        let mut previous = 0;
        for &r in &self.line_runs {
            put((r - previous) as usize);
            previous = r;
        }
        for run in &self.runs {
            put(run.start as usize);
            put((run.end - run.start) as usize);
            put(run.syntax as usize);
        }
        put(self.leaves.len());
        let mut previous = 0;
        for &(s, e) in &self.leaves {
            put((s - previous) as usize);
            put((e - s) as usize);
            previous = e;
        }
        out
    }

    /// Reads [`Spans::encode`]'s form; `None` if it's malformed.
    #[must_use]
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let mut input = bytes;
        let mut get = || -> Option<u32> { u32::try_from(cache::get_varint(&mut input)?).ok() };
        let first = get()? as usize;
        let n = get()? as usize;
        let mut line_runs = Vec::with_capacity(n.min(bytes.len()));
        let mut total = 0u32;
        for _ in 0..n {
            total = total.checked_add(get()?)?;
            line_runs.push(total);
        }
        let mut runs = Vec::with_capacity((total as usize).min(bytes.len()));
        for _ in 0..total {
            let start = get()?;
            let end = start.checked_add(get()?)?;
            let syntax = Syntax::from_u8(u8::try_from(get()?).ok()?)?;
            runs.push(Run { start, end, syntax });
        }
        let leaf_count = get()? as usize;
        let mut leaves = Vec::with_capacity(leaf_count.min(bytes.len()));
        let mut previous = 0u32;
        for _ in 0..leaf_count {
            let s = previous.checked_add(get()?)?;
            let e = s.checked_add(get()?)?;
            leaves.push((s, e));
            previous = e;
        }
        let consistent =
            line_runs.windows(2).all(|w| w[0] <= w[1]) && line_runs.first().is_none_or(|&r| r == 0);
        (input.is_empty() && consistent).then_some(Self {
            first,
            line_runs,
            runs,
            leaves,
        })
    }
}

/// One side of a file, ready to give the spans of any of its lines: either
/// its syntax tree, or its whole-file spans from the disk cache (or just
/// computed to go there).
///
/// Whole-file spans are cached for sides of at least [`CACHE_MIN_BYTES`]
/// with a known blob id: below that the parse is cheap, and computing the
/// whole file to store it would cost more than the visible lines.
pub enum Highlighter {
    /// Parsed now; each request queries only the lines it asks for.
    Tree(Parsed),
    /// Every line's spans, from the cache or computed to store there.
    Cached(Spans),
}

impl Highlighter {
    /// Prepares the side `text` (blob `id`, if known) of a `lang` file.
    /// With `store`, a large side's whole-file spans go to `cache` after the
    /// parse; without, only an existing entry is used. `None` when the side
    /// can't be highlighted (see [`parse`]).
    #[must_use]
    pub fn prepare(
        lang: Language,
        text: &Text,
        id: Option<ObjectId>,
        cache: Option<&Cache>,
        store: bool,
    ) -> Option<Self> {
        let bytes = text.bytes();
        if bytes.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        let key = id
            .filter(|_| bytes.len() >= CACHE_MIN_BYTES)
            .and_then(|id| Some((cache?, cache_key(lang, id))));
        if let Some((cache, key)) = &key
            && let Some(spans) = cache
                .get(cache::Kind::Highlight, key)
                .and_then(|b| Spans::decode(&b))
            && spans.lines() == (0..text.len())
        {
            return Some(Self::Cached(spans));
        }
        let parsed = parse(lang, bytes)?;
        match key {
            Some((cache, key)) if store => {
                let spans = parsed.spans(text, 0..text.len());
                cache.put(cache::Kind::Highlight, &key, &spans.encode());
                Some(Self::Cached(spans))
            }
            _ => Some(Self::Tree(parsed)),
        }
    }

    /// The spans of `lines` of `text`, the side this was prepared from.
    #[must_use]
    pub fn spans(&self, text: &Text, lines: Range<usize>) -> Spans {
        match self {
            Self::Tree(parsed) => parsed.spans(text, lines),
            Self::Cached(spans) => spans.slice(text, lines),
        }
    }
}

/// The version of what cached spans mean: bump it when
/// [`Syntax::from_capture`] maps names differently or [`Spans`] change
/// shape. Renumbering [`Syntax`] changes the key by itself.
const SPANS_VERSION: u8 = 1;

/// What a cached whole-file highlight depends on: the content, the language
/// and its query (which changes with the grammar version), and how
/// captures become [`Syntax`] roles.
fn cache_key(lang: Language, id: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(64);
    key.push(SPANS_VERSION);
    for syntax in Syntax::ALL {
        key.extend_from_slice(syntax.key().as_bytes());
        key.push(0);
    }
    key.extend_from_slice(lang.name().as_bytes());
    key.push(0);
    key.extend_from_slice(id.as_bytes());
    for part in lang.highlights() {
        key.extend_from_slice(cache::digest(part.as_bytes()).as_bytes());
    }
    key
}

#[cfg(test)]
mod tests;

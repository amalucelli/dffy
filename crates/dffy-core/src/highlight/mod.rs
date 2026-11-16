//! Syntax highlighting with tree-sitter. A file side is parsed once; its
//! highlight query then runs only over the lines a caller asks for, through a
//! byte-range-limited query cursor, so a large file costs one parse plus the
//! lines on screen. The same pass collects leaf-node boundaries, which
//! intra-line emphasis uses as tokens.
//!
//! Which grammar a file gets comes from the host's [`Registry`]. Queries are
//! compiled on first use per language and kept as long as its registry
//! entry: compiling all of them costs about 80ms, one costs 3–35ms.

mod lang;

use std::ops::{ControlFlow, Range};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator, Tree};

pub use lang::{Grammar, Language, Registry, Words};

use crate::cache::{self, Cache};
use crate::diff::{Emphasis, Hunk, Text, emphasize_with};
use crate::git::ObjectId;
use crate::view::{Run, Spans, Syntax, content_len, len_u32};

/// Sides larger than this aren't highlighted. A 1.8MB C file takes about
/// 250ms to parse and 86MB to hold; this keeps the parse under about 80ms.
pub const MAX_HIGHLIGHT_BYTES: usize = 512 << 10;

/// A parse that takes longer than this is abandoned and the side stays
/// plain, whatever its size: pathological input, or a slow machine.
const PARSE_BUDGET: Duration = Duration::from_millis(200);

/// Sides at least this large get their whole-file spans stored in the disk
/// cache, where a hit beats the parse it replaces (see [`Highlighter`]).
pub const CACHE_MIN_BYTES: usize = 64 << 10;

/// Sides at least this large are parsed on two threads at once by
/// [`FileSyntax::new`]. Smaller ones parse in a few milliseconds, where a
/// second thread gains little.
pub const PARALLEL_PARSE_BYTES: usize = 32 << 10;

/// The role of a query capture name such as `function.method`. Names
/// without one (`variable`, `none`, `embedded`) leave the text plain.
fn role(name: &str) -> Option<Syntax> {
    let specific = match name {
        "variable.builtin" => Some(Syntax::VariableBuiltin),
        "string.escape" | "escape" => Some(Syntax::Escape),
        "string.special.key" => Some(Syntax::Property),
        "constructor" => Some(Syntax::Type),
        "text.title" | "markup.heading" => Some(Syntax::Keyword),
        "text.literal" | "markup.raw" => Some(Syntax::String),
        "text.uri" | "text.reference" | "markup.link" => Some(Syntax::Function),
        _ => None,
    };
    specific.or_else(|| {
        Some(match name.split('.').next().unwrap_or(name) {
            "keyword" | "conditional" | "repeat" | "include" | "exception" | "storageclass" => {
                Syntax::Keyword
            }
            "string" | "character" => Syntax::String,
            "comment" => Syntax::Comment,
            "function" | "method" => Syntax::Function,
            "type" => Syntax::Type,
            "number" | "float" => Syntax::Number,
            "constant" | "boolean" => Syntax::Constant,
            "operator" => Syntax::Operator,
            "punctuation" | "delimiter" => Syntax::Punctuation,
            "property" | "field" => Syntax::Property,
            "attribute" => Syntax::Attribute,
            "tag" => Syntax::Tag,
            "label" => Syntax::Label,
            _ => return None,
        })
    })
}

/// A language's highlight query, compiled.
struct Compiled {
    query: Query,
    /// The role of each capture, by capture index.
    roles: Vec<Option<Syntax>>,
}

impl Compiled {
    /// `None` if `source` doesn't compile against `grammar`.
    fn new(grammar: &tree_sitter::Language, source: &str) -> Option<Self> {
        let query = Query::new(grammar, source).ok()?;
        let roles = query
            .capture_names()
            .iter()
            .map(|name| role(name))
            .collect();
        Some(Self { query, roles })
    }
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
pub fn parse(lang: &Language, source: &[u8]) -> Option<Parsed> {
    if source.len() > MAX_HIGHLIGHT_BYTES {
        return None;
    }
    let tree = if lang.is_compiled() {
        lang.compiled()?;
        parse_tree(lang, source, PARSE_BUDGET)?
    } else {
        // The language's first parse compiles its query beside it, rather
        // than waiting the compile out first.
        std::thread::scope(|s| {
            let query = std::thread::Builder::new().spawn_scoped(s, || lang.compiled().is_some());
            let tree = parse_tree(lang, source, PARSE_BUDGET);
            let query = match query {
                Ok(query) => query
                    .join()
                    .unwrap_or_else(|e| std::panic::resume_unwind(e)),
                // Out of threads: compile here instead of losing the
                // highlighting (or the process, as `Scope::spawn` panics).
                Err(_) => lang.compiled().is_some(),
            };
            query.then_some(tree).flatten()
        })?
    };
    Some(Parsed {
        tree,
        lang: lang.clone(),
    })
}

/// The syntax tree of `source`, or `None` when parsing takes longer than
/// `budget`.
pub(crate) fn parse_tree(lang: &Language, source: &[u8], budget: Duration) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(lang.grammar()).ok()?;
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
        if let Some(compiled) = self.lang.compiled() {
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

impl Spans {
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

/// One side of a file to highlight.
#[derive(Debug, Clone, Copy)]
pub struct Source<'a> {
    /// Repository-relative, for the language.
    pub path: &'a [u8],
    /// The side's content.
    pub text: &'a Text,
    /// The blob id results are cached under: only for content read from
    /// the object database, as a working-tree file may have changed since
    /// its id was computed.
    pub id: Option<ObjectId>,
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
        lang: &Language,
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

    /// [`Highlighter::prepare`] for `side`, in the language `grammars`
    /// detects for it. `None` when it has no language or no lines, or
    /// can't be highlighted.
    #[must_use]
    pub fn for_side(
        grammars: &Registry,
        side: Source,
        cache: Option<&Cache>,
        store: bool,
    ) -> Option<Self> {
        let bytes = side.text.bytes();
        let lang = grammars.detect(side.path, &bytes[..bytes.len().min(256)])?;
        if side.text.is_empty() {
            return None;
        }
        Self::prepare(lang, side.text, side.id, cache, store)
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

/// The version of what cached spans mean: bump it when [`role`] maps
/// names differently or [`Spans`] change shape. Renumbering [`Syntax`]
/// changes the key by itself.
const SPANS_VERSION: u8 = 1;

/// What a cached whole-file highlight depends on: the content, the
/// language's [`Language::identity`], and how captures become [`Syntax`]
/// roles.
fn cache_key(lang: &Language, id: ObjectId) -> Vec<u8> {
    let mut key = Vec::with_capacity(160);
    key.push(SPANS_VERSION);
    for syntax in Syntax::ALL {
        key.extend_from_slice(syntax.key().as_bytes());
        key.push(0);
    }
    key.extend_from_slice(lang.identity().as_bytes());
    key.extend_from_slice(id.as_bytes());
    key
}

/// The two sides of a file, prepared for highlighting where its language
/// is known and each side is small enough.
pub struct FileSyntax {
    old: Option<Highlighter>,
    new: Option<Highlighter>,
}

impl FileSyntax {
    /// Prepares both sides of a file, each in the language `grammars`
    /// detects for it, on two threads when both are at least
    /// [`PARALLEL_PARSE_BYTES`]. With `store`, large sides' spans go to
    /// `cache` (see [`Highlighter::prepare`]).
    #[must_use]
    pub fn new(
        grammars: &Registry,
        [old, new]: [Source; 2],
        cache: Option<&Cache>,
        store: bool,
    ) -> Self {
        let parallel = old.text.bytes().len().min(new.text.bytes().len()) >= PARALLEL_PARSE_BYTES;
        let old = || Highlighter::for_side(grammars, old, cache, store);
        let new = || Highlighter::for_side(grammars, new, cache, store);
        let (old, new) = if parallel {
            std::thread::scope(|s| {
                let spawned = std::thread::Builder::new().spawn_scoped(s, old);
                let new = new();
                let old = match spawned {
                    Ok(spawned) => spawned
                        .join()
                        .unwrap_or_else(|e| std::panic::resume_unwind(e)),
                    // Out of threads: parse here, as `Scope::spawn` would panic.
                    Err(_) => old(),
                };
                (old, new)
            })
        } else {
            (old(), new())
        };
        Self { old, new }
    }

    /// Neither side is highlighted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.old.is_none() && self.new.is_none()
    }

    /// The spans of `lines` of the `new` side, if it's highlighted.
    #[must_use]
    pub fn new_spans(&self, new: &Text, lines: Range<usize>) -> Option<Spans> {
        self.new.as_ref().map(|h| h.spans(new, lines))
    }

    /// The spans of the lines `hunk` shows on each side of `old` and
    /// `new`, the texts this was prepared from.
    #[must_use]
    pub fn hunk(&self, old: &Text, new: &Text, hunk: &Hunk) -> HunkSyntax {
        HunkSyntax {
            old: self
                .old
                .as_ref()
                .map(|h| h.spans(old, hunk.old_start..hunk.old_start + hunk.old_len)),
            new: self
                .new
                .as_ref()
                .map(|h| h.spans(new, hunk.new_start..hunk.new_start + hunk.new_len)),
        }
    }
}

/// Syntax spans of the lines one hunk shows, per side.
#[derive(Debug, Default, Clone)]
pub struct HunkSyntax {
    /// The old side's, if it's highlighted.
    pub old: Option<Spans>,
    /// The new side's, if it's highlighted.
    pub new: Option<Spans>,
}

impl HunkSyntax {
    /// The intra-line emphasis of `hunk` of `old` and `new`, with tokens
    /// from the spans.
    #[must_use]
    pub fn emphasis(&self, old: &Text, new: &Text, hunk: &Hunk) -> Vec<Emphasis> {
        emphasize_with(old, new, hunk, self.old.as_ref(), self.new.as_ref())
    }
}

#[cfg(test)]
mod tests;

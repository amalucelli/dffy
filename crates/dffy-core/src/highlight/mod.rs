//! Syntax highlighting with tree-sitter. A file side is parsed once; its
//! highlight query then runs only over the lines a caller asks for, through a
//! byte-range-limited query cursor, so a large file costs one parse plus the
//! lines on screen. The same pass collects leaf-node boundaries, which
//! intra-line emphasis uses as tokens.
//!
//! Which grammar a file gets comes from the host's [`Registry`]. Queries are
//! compiled on first use per language, or ahead of it when the host calls
//! [`Language::prepare`], and kept as long as its registry entry: compiling all of them costs about 430ms, one costs under 0.1ms
//! (JSON) to 140ms (Swift). The cost is tree-sitter checking each pattern
//! that nests a child under a named node against the grammar's parse
//! table, plus a per-grammar floor (12ms for SQL with an empty query), so
//! dropping patterns dffy doesn't draw saves little.

mod lang;
mod scope;

use std::ops::{ControlFlow, Range};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tree_sitter::{InputEdit, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

pub use lang::{Grammar, Language, LanguageSource, Registry, Words};
pub use scope::Scopes;

use crate::cache::{self, Cache};
use crate::diff::{Emphasis, Hunk, LineKind, Text, emphasize_with};
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
/// second thread gains little, and one after the other the new side
/// reuses the old side's tree.
pub const PARALLEL_PARSE_BYTES: usize = 32 << 10;

/// The role of a query capture name such as `function.method`. Names
/// without one (`variable`, `none`, `embedded`) leave the text plain.
fn role(name: &str) -> Option<Syntax> {
    let specific = match name {
        "variable.builtin" => Some(Syntax::VariableBuiltin),
        "string.escape" | "escape" => Some(Syntax::Escape),
        "string.special.key" | "variable.member" => Some(Syntax::Property),
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
    ///
    /// Queries written for Neovim get two adjustments. tree-sitter skips
    /// predicates it doesn't know, so a pattern guarded by `#lua-match?`
    /// would match every node: it's read as `#match?`, which agrees with
    /// it on the patterns the bundled queries use (Lua's `%` classes and
    /// `-` don't carry over). And `@spell` marks text to spell-check,
    /// often on the same node as a real capture; listed first, it would
    /// claim a comment and leave it plain.
    fn new(grammar: &tree_sitter::Language, source: &str) -> Option<Self> {
        let mut query = Query::new(grammar, &source.replace("lua-match?", "match?")).ok()?;
        query.disable_capture("spell");
        query.disable_capture("nospell");
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
        parse_tree(lang, source, PARSE_BUDGET, None)?
    } else {
        // The language's first parse compiles its query beside it, rather
        // than waiting the compile out first.
        std::thread::scope(|s| {
            let query = std::thread::Builder::new().spawn_scoped(s, || lang.compiled().is_some());
            let tree = parse_tree(lang, source, PARSE_BUDGET, None);
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
/// `budget`. With `old`, a tree of an earlier text edited into `source`'s
/// shape by [`edit`], whose untouched subtrees are reused.
pub(crate) fn parse_tree(
    lang: &Language,
    source: &[u8],
    budget: Duration,
    old: Option<&Tree>,
) -> Option<Tree> {
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
        old,
        Some(options),
    )
}

/// Edits `tree`, parsed from `old`, into the shape of `new`: one edit per
/// run of lines `hunks` change, so a parse of `new` from it parses only
/// those runs again, with the nodes that span them. `false` when `hunks`
/// don't account for every byte: the lines they keep must be the same on
/// both sides, which a diff that ignores whitespace doesn't promise. `tree`
/// is then partly edited and only fit to drop.
pub(crate) fn edit(tree: &mut Tree, old: &Text, new: &Text, hunks: &[Hunk]) -> bool {
    let mut e = Editor {
        tree,
        old,
        new,
        o: 0,
        n: 0,
    };
    for h in hunks {
        let Some(gap) = h.old_start.checked_sub(e.o) else {
            return false;
        };
        if h.new_start.checked_sub(e.n) != Some(gap) || !e.to(e.o, e.n, gap) {
            return false;
        }
        for line in h.lines.iter().filter(|l| l.kind == LineKind::Context) {
            if !e.to(line.old, line.new, 1) {
                return false;
            }
        }
        if !e.to(h.old_start + h.old_len, h.new_start + h.new_len, 0) {
            return false;
        }
    }
    let tail = old.len().saturating_sub(e.o);
    new.len().checked_sub(e.n) == Some(tail) && e.to(e.o, e.n, tail)
}

/// [`edit`]'s walk. Edits go in order, each in the coordinates the ones
/// before it left: everything before line `n` of `new` is already `new`'s.
struct Editor<'a> {
    tree: &'a mut Tree,
    old: &'a Text,
    new: &'a Text,
    /// The first old line not yet kept or edited.
    o: usize,
    /// The first new line not yet kept or edited.
    n: usize,
}

impl Editor<'_> {
    /// Replaces the old lines up to `o` with the new lines up to `n`, then
    /// keeps `kept` lines; `false` when those aren't the same on both
    /// sides, or the lines run backwards or past the end.
    fn to(&mut self, o: usize, n: usize, kept: usize) -> bool {
        let (old, new) = (self.old, self.new);
        if o < self.o || n < self.n || o + kept > old.len() || n + kept > new.len() {
            return false;
        }
        if (o, n) != (self.o, self.n) {
            let removed = &old.bytes()[old.offset(self.o)..old.offset(o)];
            let start_byte = new.offset(self.n);
            let start_position = point(new, self.n);
            self.tree.edit(&InputEdit {
                start_byte,
                old_end_byte: start_byte + removed.len(),
                new_end_byte: new.offset(n),
                start_position,
                old_end_position: advance(start_position, removed),
                new_end_position: point(new, n),
            });
        }
        (self.o, self.n) = (o + kept, n + kept);
        old.bytes()[old.offset(o)..old.offset(o + kept)]
            == new.bytes()[new.offset(n)..new.offset(n + kept)]
    }
}

/// Where line `i` of `text` starts, or for `text.len()` where the text
/// ends, as tree-sitter counts: rows at each `\n`, columns in bytes.
fn point(text: &Text, i: usize) -> Point {
    match i.checked_sub(1) {
        Some(last) if i == text.len() && !text.line(last).ends_with(b"\n") => {
            Point::new(last, text.line(last).len())
        }
        _ => Point::new(i, 0),
    }
}

/// The point after `bytes`, from `from`.
fn advance(from: Point, bytes: &[u8]) -> Point {
    match memchr::memrchr(b'\n', bytes) {
        Some(last) => Point::new(
            from.row + memchr::memchr_iter(b'\n', bytes).count(),
            bytes.len() - last - 1,
        ),
        None => Point::new(from.row, from.column + bytes.len()),
    }
}

impl Parsed {
    /// [`parse`] of `new` in this side's language, from this side's tree of
    /// `old`: only the lines `hunks` change are parsed again, with the
    /// nodes that span them.
    ///
    /// The tree must come out as a parse from scratch makes it, which
    /// tree-sitter's reuse gives only where no error recovery is involved:
    /// recovering differently around reused nodes can lex a token inside
    /// an error as another kind, or end top-level nodes elsewhere. So `new`
    /// is parsed from scratch when this tree has an error, when the result
    /// has one, and when `hunks` don't account for every byte of `new` (see
    /// [`edit`]).
    #[must_use]
    pub fn reparse(&self, old: &Text, new: &Text, hunks: &[Hunk]) -> Option<Self> {
        if new.bytes().len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        // TODO(amalucelli): tree-sitter 0.27.2 skips the keyword fallback on
        // a reused node's first leaf (https://github.com/tree-sitter/tree-sitter/pull/6149,
        // backported in #6152). Once a release has it, upgrade and rerun the
        // per-file incremental-vs-fresh sweep without the error checks here
        // and below; drop them if it finds no mismatch.
        if self.tree.root_node().has_error() {
            return parse(&self.lang, new.bytes());
        }
        let mut tree = self.tree.clone();
        if !edit(&mut tree, old, new, hunks) {
            return parse(&self.lang, new.bytes());
        }
        let tree = parse_tree(&self.lang, new.bytes(), PARSE_BUDGET, Some(&tree))?;
        if tree.root_node().has_error() {
            return parse(&self.lang, new.bytes());
        }
        Some(Self {
            tree,
            lang: self.lang.clone(),
        })
    }
}

impl Parsed {
    /// The definitions of `text`, the text this was parsed from; `None`
    /// when its language has no scope query.
    #[must_use]
    pub fn scopes(&self, text: &Text) -> Option<Scopes> {
        Some(scope::extract(self.lang.scope_query()?, &self.tree, text))
    }

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
/// whole file to store it would cost more than the visible lines. The
/// side's definitions are cached with them, as there's no tree to find
/// them in after a hit.
pub enum Highlighter {
    /// Parsed now; each request queries only the lines it asks for. With
    /// the side's definitions once [`FileSyntax::find_scopes`] found them,
    /// set to none from the start for a language without a scope query.
    Tree(Parsed, OnceLock<Option<Scopes>>),
    /// Every line's spans, from the cache or computed to store there, and
    /// the definitions (see [`Parsed::scopes`]). Boxed so a side holds no
    /// more than a tree does, which is all a side takes when nothing is
    /// cached.
    Cached(Box<(Spans, Option<Scopes>)>),
}

// Every highlighted file's syntax slot holds one from the moment it's
// diffed, parsed or not: two sides of spans, and nothing for definitions.
const _: () = assert!(size_of::<FileSyntax>() <= 160);

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
        match Self::lookup(lang, text, id, cache)? {
            Lookup::Hit(cached) => Some(Self::Cached(cached)),
            Lookup::Miss(key) => Some(Self::finish(parse(lang, text.bytes())?, text, key, store)),
        }
    }

    /// The cache's side of [`Highlighter::prepare`]; `None` when `text` is
    /// too large to highlight.
    fn lookup<'c>(
        lang: &Language,
        text: &Text,
        id: Option<ObjectId>,
        cache: Option<&'c Cache>,
    ) -> Option<Lookup<'c>> {
        let bytes = text.bytes();
        if bytes.len() > MAX_HIGHLIGHT_BYTES {
            return None;
        }
        let key = id
            .filter(|_| bytes.len() >= CACHE_MIN_BYTES)
            .and_then(|id| Some((cache?, cache_key(lang, id))));
        if let Some((cache, key)) = &key
            && let Some((spans, scopes)) = cache
                .get(cache::Kind::Highlight, key)
                .and_then(|b| decode_entry(&b))
            && spans.lines() == (0..text.len())
        {
            return Some(Lookup::Hit(Box::new((spans, scopes))));
        }
        Some(Lookup::Miss(key))
    }

    /// `parsed`, the tree of `text`, with its whole-file spans stored under
    /// `key` when there is one and `store` is set.
    fn finish(parsed: Parsed, text: &Text, key: Option<(&Cache, Vec<u8>)>, store: bool) -> Self {
        match key {
            Some((cache, key)) if store => {
                let spans = parsed.spans(text, 0..text.len());
                let scopes = parsed.scopes(text);
                cache.put(
                    cache::Kind::Highlight,
                    &key,
                    &encode_entry(&spans, scopes.as_ref()),
                );
                Self::Cached(Box::new((spans, scopes)))
            }
            _ => {
                let scopes = if parsed.lang.has_scopes() {
                    OnceLock::new()
                } else {
                    OnceLock::from(None)
                };
                Self::Tree(parsed, scopes)
            }
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
        Self::after(grammars, side, None, cache, store)
    }

    /// [`Highlighter::for_side`] for the new side of a file, given its old
    /// side `old` as prepared and the line diff's `hunks` between their
    /// texts. When `old` is a tree in the new side's language and the new
    /// side isn't cached, the new side is parsed from that tree (see
    /// [`Parsed::reparse`]).
    #[must_use]
    pub fn after(
        grammars: &Registry,
        new: Source,
        old: Option<(&Self, &Text, &[Hunk])>,
        cache: Option<&Cache>,
        store: bool,
    ) -> Option<Self> {
        let bytes = new.text.bytes();
        let lang = grammars.detect(new.path, &bytes[..bytes.len().min(256)])?;
        if new.text.is_empty() {
            return None;
        }
        let key = match Self::lookup(lang, new.text, new.id, cache)? {
            Lookup::Hit(cached) => return Some(Self::Cached(cached)),
            Lookup::Miss(key) => key,
        };
        let parsed = match old {
            Some((Self::Tree(old, _), old_text, hunks)) if old.lang == *lang => {
                old.reparse(old_text, new.text, hunks)
            }
            _ => parse(lang, bytes),
        };
        Some(Self::finish(parsed?, new.text, key, store))
    }

    /// Whether [`Highlighter::after`] can parse the next side from this
    /// one: it's a tree, without the errors that rule reuse out. A host
    /// holding both sides drops one that can't before the next side's
    /// parse.
    #[must_use]
    pub fn can_seed(&self) -> bool {
        matches!(self, Self::Tree(parsed, _) if !parsed.tree.root_node().has_error())
    }

    /// The spans of `lines` of `text`, the side this was prepared from.
    #[must_use]
    pub fn spans(&self, text: &Text, lines: Range<usize>) -> Spans {
        match self {
            Self::Tree(parsed, _) => parsed.spans(text, lines),
            Self::Cached(cached) => cached.0.slice(text, lines),
        }
    }
}

/// A side's whole-file spans and definitions from the cache, or on a miss
/// the entry a parse may fill.
enum Lookup<'c> {
    Hit(Box<(Spans, Option<Scopes>)>),
    Miss(Option<(&'c Cache, Vec<u8>)>),
}

/// The version of what cached spans mean: bump it when [`role`] maps
/// names differently, [`Compiled::new`] reads queries differently, or
/// [`Spans`], [`Scopes`] or labels change shape. Renumbering [`Syntax`]
/// changes the key by itself.
const SPANS_VERSION: u8 = 3;

/// A cache entry: the spans' length and form, then the definitions' form
/// when the language has a scope query.
fn encode_entry(spans: &Spans, scopes: Option<&Scopes>) -> Vec<u8> {
    let spans = spans.encode();
    let mut out = Vec::with_capacity(spans.len() + 8);
    cache::put_varint(&mut out, spans.len() as u64);
    out.extend_from_slice(&spans);
    if let Some(scopes) = scopes {
        scopes.encode(&mut out);
    }
    out
}

/// Reads [`encode_entry`]'s form; `None` if it's malformed.
fn decode_entry(bytes: &[u8]) -> Option<(Spans, Option<Scopes>)> {
    let mut input = bytes;
    let len = usize::try_from(cache::get_varint(&mut input)?).ok()?;
    let spans = Spans::decode(input.get(..len)?)?;
    let mut rest = &input[len..];
    if rest.is_empty() {
        return Some((spans, None));
    }
    let scopes = Scopes::decode(&mut rest)?;
    rest.is_empty().then_some((spans, Some(scopes)))
}

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
/// is known and each side is small enough, and once found each side's
/// definitions, where its language has a scope query.
pub struct FileSyntax {
    old: Option<Highlighter>,
    new: Option<Highlighter>,
}

impl FileSyntax {
    /// Prepares both sides of a file, each in the language `grammars`
    /// detects for it: on two threads from scratch when both are at least
    /// [`PARALLEL_PARSE_BYTES`], otherwise one after the other, the new
    /// side from the old side's tree (see [`Highlighter::after`]; `hunks`
    /// are the line diff's). With `store`, large sides' spans go to `cache`
    /// (see [`Highlighter::prepare`]). A side's definitions come with its
    /// cached spans; a parsed side's wait for [`FileSyntax::find_scopes`],
    /// so the highlighting is ready without them.
    #[must_use]
    pub fn new(
        grammars: &Registry,
        [old, new]: [Source; 2],
        hunks: &[Hunk],
        cache: Option<&Cache>,
        store: bool,
    ) -> Self {
        if old.text.bytes().len().min(new.text.bytes().len()) < PARALLEL_PARSE_BYTES {
            let old_h = Highlighter::for_side(grammars, old, cache, store);
            let seed = old_h.as_ref().map(|h| (h, old.text, hunks));
            let new = Highlighter::after(grammars, new, seed, cache, store);
            return Self { old: old_h, new };
        }
        let old = || Highlighter::for_side(grammars, old, cache, store);
        let new = || Highlighter::for_side(grammars, new, cache, store);
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
            Self { old, new }
        })
    }

    /// Finds the definitions of the parsed sides in their trees, `old` and
    /// `new` being the texts this was prepared from. Sides whose
    /// definitions are known already are left alone, so a second call
    /// does nothing.
    pub fn find_scopes(&self, old: &Text, new: &Text) {
        for (h, text) in [(&self.old, old), (&self.new, new)] {
            if let Some(Highlighter::Tree(parsed, scopes)) = h {
                scopes.get_or_init(|| parsed.scopes(text));
            }
        }
    }

    /// The old and new side's definitions, for a side whose language has
    /// a scope query; `None` until [`FileSyntax::find_scopes`] found those
    /// of the parsed sides.
    #[must_use]
    pub fn scopes(&self) -> Option<[Option<&Scopes>; 2]> {
        let mut sides = [None, None];
        for (side, h) in sides.iter_mut().zip([&self.old, &self.new]) {
            *side = match h {
                Some(Highlighter::Tree(_, scopes)) => scopes.get()?.as_ref(),
                Some(Highlighter::Cached(cached)) => cached.1.as_ref(),
                None => None,
            };
        }
        Some(sides)
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

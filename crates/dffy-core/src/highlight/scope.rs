//! Where a line is in the code's structure: the definitions around it
//! (`impl Error › fn fatal`), found once per side from the tree the
//! highlighter parsed, so a host looks a line up per frame without a
//! tree walk.
//!
//! A language's scope query says what counts. Each pattern captures the
//! definition as `@scope`, its keyword as `@kind` and what it's called as
//! `@name`. Either of the last two may capture several nodes, which then
//! stand for the text from the first to the last. A label is the keyword
//! and the name on the first line, whitespace collapsed (`fn fatal`,
//! `impl Display for Error`); a definition without a keyword (a method,
//! a C function) shows as `name()`, one without a name as its keyword.
//!
//! To cover a language:
//! 1. Write `queries/<language>/scopes.scm` with the patterns above,
//!    leaving out what has no name to show (closures, lambdas).
//! 2. List it in the grammar's `scopes` in [`super::lang`].
//! 3. Add an outline test in `scope/tests.rs`.

use tree_sitter::{Query, QueryCursor, StreamingIterator, Tree};

use crate::cache;
use crate::diff::Text;

/// A compiled scope query and its capture indices.
pub(super) struct ScopeQuery {
    query: Query,
    scope: u32,
    kind: Option<u32>,
    name: Option<u32>,
    /// Whether a pattern starts at a node of each kind, by kind id; `None`
    /// when a pattern's first node isn't a named kind, so any may start
    /// one.
    roots: Option<Vec<bool>>,
}

impl ScopeQuery {
    /// `None` if `source` doesn't compile against `grammar` or has no
    /// `@scope` capture.
    pub(super) fn new(grammar: &tree_sitter::Language, source: &str) -> Option<Self> {
        let query = Query::new(grammar, source).ok()?;
        let mut names = Vec::new();
        for i in 0..query.pattern_count() {
            let name = source[query.start_byte_for_pattern(i)..]
                .strip_prefix('(')
                .map(|s| s.split(|c: char| !c.is_alphanumeric() && c != '_').next());
            names.push(name.flatten().filter(|n| !n.is_empty() && *n != "_"));
        }
        // Grammars give several kinds one name (aliases); a pattern names
        // them all.
        let roots = names.into_iter().collect::<Option<Vec<_>>>().map(|names| {
            (0..grammar.node_kind_count())
                .map(|id| {
                    let id = u16::try_from(id).unwrap_or(u16::MAX);
                    grammar.node_kind_is_named(id)
                        && grammar
                            .node_kind_for_id(id)
                            .is_some_and(|kind| names.contains(&kind))
                })
                .collect()
        });
        Some(Self {
            scope: query.capture_index_for_name("scope")?,
            kind: query.capture_index_for_name("kind"),
            name: query.capture_index_for_name("name"),
            roots,
            query,
        })
    }

    fn may_start(&self, kind: u16) -> bool {
        self.roots
            .as_ref()
            .is_none_or(|roots| roots.get(usize::from(kind)).copied().unwrap_or(false))
    }
}

/// No parent.
const NONE: u32 = u32::MAX;

/// A definition: its lines (0-based, inclusive), the innermost definition
/// around it and its label's bytes in [`Scopes::labels`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Scope {
    start: u32,
    end: u32,
    parent: u32,
    label: (u32, u32),
}

/// The definitions of one side, by first line, each after the ones around
/// it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scopes {
    scopes: Vec<Scope>,
    labels: String,
}

impl Scopes {
    /// The labels of the definitions around 0-based `line`, outermost
    /// first; empty at the top level.
    #[must_use]
    pub fn at(&self, line: usize) -> Vec<&str> {
        let line = u32::try_from(line).unwrap_or(u32::MAX);
        let mut out = Vec::new();
        let mut i = match self.scopes.partition_point(|s| s.start <= line) {
            0 => return out,
            n => to_u32(n - 1),
        };
        // The last definition starting at or before `line` may have ended
        // above it: the first around it is then one of its parents.
        while i != NONE && self.scopes[i as usize].end < line {
            i = self.scopes[i as usize].parent;
        }
        while i != NONE {
            out.push(self.label(i as usize));
            i = self.scopes[i as usize].parent;
        }
        out.reverse();
        out
    }

    fn label(&self, i: usize) -> &str {
        let (start, end) = self.scopes[i].label;
        &self.labels[start as usize..end as usize]
    }

    /// Appends a compact binary form, for the disk cache.
    pub(super) fn encode(&self, out: &mut Vec<u8>) {
        let mut put = |n: usize| cache::put_varint(out, n as u64);
        put(self.scopes.len());
        let mut previous = 0;
        for s in &self.scopes {
            put((s.start - previous) as usize);
            put((s.end - s.start) as usize);
            put(if s.parent == NONE {
                0
            } else {
                s.parent as usize + 1
            });
            put((s.label.1 - s.label.0) as usize);
            previous = s.start;
        }
        put(self.labels.len());
        out.extend_from_slice(self.labels.as_bytes());
    }

    /// Reads [`Scopes::encode`]'s form from the front of `input`; `None`
    /// if it's malformed.
    pub(super) fn decode(input: &mut &[u8]) -> Option<Self> {
        let get =
            |input: &mut &[u8]| -> Option<u32> { u32::try_from(cache::get_varint(input)?).ok() };
        let n = get(input)? as usize;
        let mut scopes = Vec::with_capacity(n.min(input.len()));
        let (mut start, mut label) = (0u32, 0u32);
        for i in 0..n {
            start = start.checked_add(get(input)?)?;
            let end = start.checked_add(get(input)?)?;
            let parent = match get(input)? {
                0 => NONE,
                p if (p as usize) <= i => p - 1,
                _ => return None,
            };
            let label_end = label.checked_add(get(input)?)?;
            scopes.push(Scope {
                start,
                end,
                parent,
                label: (label, label_end),
            });
            label = label_end;
        }
        let len = get(input)? as usize;
        if len != label as usize || input.len() < len {
            return None;
        }
        let (labels, rest) = input.split_at(len);
        let labels = String::from_utf8(labels.to_vec()).ok()?;
        if scopes.iter().any(|s| {
            !labels.is_char_boundary(s.label.0 as usize)
                || !labels.is_char_boundary(s.label.1 as usize)
        }) {
            return None;
        }
        *input = rest;
        Some(Self { scopes, labels })
    }
}

/// The definitions `query` finds in `tree`, parsed from `text`.
///
/// The walk enters only nodes spanning several lines, and runs the query
/// only on nodes a pattern can start at: running it over the whole tree
/// costs about half as much as the parse, this a few percent. What it
/// misses is a definition inside a one-line node, which is on that line
/// and no other.
pub(super) fn extract(query: &ScopeQuery, tree: &Tree, text: &Text) -> Scopes {
    struct Found {
        bytes: (usize, usize),
        lines: (u32, u32),
        label: String,
    }
    let source = text.bytes();
    let mut found = Vec::new();
    let mut cursor = QueryCursor::new();
    cursor.set_max_start_depth(Some(0));
    let mut walk = tree.walk();
    'walk: loop {
        let node = walk.node();
        if query.may_start(node.kind_id()) {
            let mut matches = cursor.matches(&query.query, node, source);
            while let Some(m) = matches.next() {
                let span = |index: Option<u32>| {
                    let (start, end) = m
                        .nodes_for_capture_index(index?)
                        .map(|n| (n.start_byte(), n.end_byte()))
                        .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))?;
                    Some(first_line(&source[start..end]))
                };
                let Some(scope) = m.nodes_for_capture_index(query.scope).next() else {
                    continue;
                };
                let label = match (span(query.kind), span(query.name)) {
                    (Some(kind), Some(name)) => format!("{kind} {name}"),
                    (Some(kind), None) => kind,
                    (None, Some(name)) => format!("{name}()"),
                    (None, None) => continue,
                };
                let (start, end) = (scope.start_position(), scope.end_position());
                // A node ending at a line's start ends on the line before.
                let last = if end.column == 0 && end.row > start.row {
                    end.row - 1
                } else {
                    end.row
                };
                found.push(Found {
                    bytes: (scope.start_byte(), scope.end_byte()),
                    lines: (to_u32(start.row), to_u32(last)),
                    label,
                });
            }
        }
        let lines = node.end_position().row > node.start_position().row;
        if lines && walk.goto_first_child() {
            continue;
        }
        while !walk.goto_next_sibling() {
            if !walk.goto_parent() {
                break 'walk;
            }
        }
    }
    found.sort_by_key(|f| (f.bytes.0, std::cmp::Reverse(f.bytes.1)));
    // Two patterns may match one definition; the first found is kept.
    found.dedup_by_key(|f| f.bytes);
    let mut scopes = Scopes::default();
    // The definitions around the next one: index and end byte.
    let mut open: Vec<(u32, usize)> = Vec::new();
    for f in found {
        while open
            .last()
            .is_some_and(|&(_, end)| end <= f.bytes.0 || end < f.bytes.1)
        {
            open.pop();
        }
        let index = to_u32(scopes.scopes.len());
        let label = to_u32(scopes.labels.len());
        scopes.labels.push_str(&f.label);
        scopes.scopes.push(Scope {
            start: f.lines.0,
            end: f.lines.1,
            parent: open.last().map_or(NONE, |&(p, _)| p),
            label: (label, to_u32(scopes.labels.len())),
        });
        open.push((index, f.bytes.1));
    }
    scopes
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The text up to the first line break, its whitespace runs collapsed to
/// a space.
fn first_line(bytes: &[u8]) -> String {
    let line = bytes.split(|&b| b == b'\n').next().unwrap_or(bytes);
    String::from_utf8_lossy(line)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests;

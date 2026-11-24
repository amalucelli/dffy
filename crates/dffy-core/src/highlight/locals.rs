//! What a name used inside a function names there: a local or a parameter,
//! found by a language's locals query in the asking file alone, so a
//! lookup answers with it before searching any other file.
//!
//! A locals query captures each scope as `@local.scope`, and each name a
//! scope defines as `@local.definition.var` or `@local.definition.parameter`.
//! A definition's capture may be the name, or a pattern or a C declarator
//! the names are bound in (`let (a, mut b) = …`, `int *p`): the names are
//! read from it. A name is resolved in the innermost scope around the use
//! that defines it: there, the last definition before the use (`let x =
//! x + 1`), else, for a parameter or where the language hoists (Python's
//! functions, JavaScript's `var` and `function`), the first after it. What
//! only the file's top scope defines is the index's to answer, but C's,
//! whose index lists no variables.
//!
//! A member's receiver (`x` in `x.p`) is read the same way: the type a
//! local or a parameter is declared with or given by its value (`T{}`,
//! `new T()`, `T::new()`, Python's `T()`), else `self`'s or `this`'s class
//! or impl, else a capitalized name as a type. A value a call gives is
//! left as the [`Call`], for the index to tell the type the called
//! function is declared to return; nothing past that is inferred.
//!
//! To cover a language: write `queries/<language>/locals.scm` with the
//! captures above, list it in the grammar's `locals` in [`super::lang`],
//! and add a fixture to `locals/tests.rs`.

use std::ops::{ControlFlow, Range};

use rustc_hash::FxHashMap;
use tree_sitter::{
    Node, Query, QueryCursor, QueryCursorOptions, QueryCursorState, StreamingIterator, Tree,
};

use super::DefinitionKind;
use super::scope::roots;

/// A compiled locals query and its capture indices.
pub(super) struct LocalsQuery {
    query: Query,
    scope: Option<u32>,
    class: Option<u32>,
    var: Option<u32>,
    hoisted: Option<u32>,
    parameter: Option<u32>,
    global: Option<u32>,
    nonlocal: Option<u32>,
}

impl LocalsQuery {
    /// `None` when `source` doesn't compile against `grammar`.
    pub(super) fn new(grammar: &tree_sitter::Language, source: &str) -> Option<Self> {
        let query = Query::new(grammar, source).ok()?;
        // Each pattern starts with a named node, as the tags query's must.
        roots(grammar, &query, source)?;
        let index = |name| query.capture_index_for_name(name);
        Some(Self {
            scope: index("local.scope"),
            class: index("local.scope.class"),
            var: index("local.definition.var"),
            hoisted: index("local.definition.hoisted"),
            parameter: index("local.definition.parameter"),
            global: index("local.global"),
            nonlocal: index("local.nonlocal"),
            query,
        })
    }
}

/// The definition a use resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Local {
    /// [`DefinitionKind::Local`], [`DefinitionKind::Parameter`], or for a
    /// C file-scope variable [`DefinitionKind::Variable`].
    pub kind: DefinitionKind,
    /// The bytes of the name where it's defined.
    pub name: Range<usize>,
    /// Its line, 0-based.
    pub line: usize,
}

/// A scope found by the query.
struct Scope<'t> {
    node: Node<'t>,
    range: Range<usize>,
    /// A class's body, whose names only what's directly in it sees.
    class: bool,
}

/// What a name's capture says of it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Says {
    Defines(DefinitionKind),
    /// Defined in the function around, whatever block it's in.
    Hoisted,
    Global,
    Nonlocal,
}

/// A name found by the query, in the scope it's in: its node, where its
/// scope's uses see it from (a variable past its declaration, so
/// `let x = x + 1` reads the `x` before), and where past there they don't
/// (the value its pattern is matched with, an `else`).
struct Defined<'t> {
    node: Node<'t>,
    says: Says,
    visible: usize,
    hidden: [Range<usize>; 2],
}

impl Defined<'_> {
    fn seen_from(&self, at: usize) -> bool {
        self.visible <= at && !self.hidden_at(at)
    }

    fn hidden_at(&self, at: usize) -> bool {
        self.hidden.iter().any(|h| h.contains(&at))
    }
}

/// A capture, the node of a name it holds, and what it says of it.
type Captured<'t> = (Node<'t>, Node<'t>, Says);

/// Kinds a name in code is parsed as, where it may name a local.
const NAME_KINDS: &[&str] = &[
    "identifier",
    "shorthand_field_identifier",
    "shorthand_property_identifier_pattern",
    "shorthand_property_identifier",
];

/// Kinds of declaration whose names aren't visible in their own value.
const DECLARATIONS: &[&str] = &[
    "let_declaration",
    "short_var_declaration",
    "var_spec",
    "const_spec",
    "assignment",
    "augmented_assignment",
    "named_expression",
    "variable_declarator",
    "declaration",
];

/// Whether a node of `kind` is a name in `language`'s code that may name a
/// local: PHP's `$x` has its name inside.
fn is_name(kind: &str, language: &str) -> bool {
    NAME_KINDS.contains(&kind)
        || match language {
            "kotlin" | "swift" => kind == "simple_identifier",
            "php" => kind == "name",
            "bash" => kind == "variable_name",
            "csharp" => kind == "implicit_parameter",
            _ => false,
        }
}

/// Whether a node of `kind` is a declaration in `language` whose names
/// aren't visible in their own value.
fn is_declaration(kind: &str, language: &str) -> bool {
    DECLARATIONS.contains(&kind)
        || match language {
            "kotlin" | "swift" => kind == "property_declaration",
            "scala" => matches!(kind, "val_definition" | "var_definition"),
            "lua" => kind == "assignment_statement",
            "zig" => kind == "variable_declaration",
            "php" => kind == "assignment_expression",
            "bash" => kind == "variable_assignment",
            "elixir" => kind == "binary_operator",
            _ => false,
        }
}

/// Kinds that bind a pattern to the value after it, where a name of the
/// pattern is none of the pattern's: conditions, loops, switches. A match's
/// arm isn't one, as what's after its pattern is its body.
fn matches_value(kind: &str, language: &str) -> bool {
    match language {
        "rust" => matches!(kind, "let_condition" | "for_expression"),
        "go" => matches!(
            kind,
            "range_clause" | "type_switch_statement" | "receive_statement"
        ),
        "python" => matches!(kind, "for_statement" | "for_in_clause"),
        "javascript" | "typescript" | "tsx" => kind == "for_in_statement",
        "java" => kind == "enhanced_for_statement",
        "csharp" => kind == "foreach_statement",
        "cpp" => kind == "for_range_loop",
        "kotlin" => matches!(kind, "for_statement" | "when_subject"),
        "scala" => kind == "enumerator",
        "lua" => matches!(kind, "for_generic_clause" | "for_numeric_clause"),
        "swift" => matches!(
            kind,
            "for_statement" | "if_statement" | "guard_statement" | "while_statement"
        ),
        _ => false,
    }
}

/// Where uses in its scope see the name `node` defines by the capture
/// `capture`: from where on, and where past there they don't.
fn visibility(capture: Node, node: Node, says: Says, language: &str) -> (usize, [Range<usize>; 2]) {
    let nowhere = [0..0, 0..0];
    let Some(parent) = capture
        .parent()
        .filter(|_| matches!(says, Says::Defines(DefinitionKind::Local) | Says::Hoisted))
    else {
        return (capture.end_byte(), nowhere);
    };
    if matches!(language, "c" | "cpp") && parent.kind() == "declaration" {
        // Right past its declarator, before its own initializer.
        let declarator = (capture.child_by_field_name("declarator"))
            .filter(|_| capture.kind() == "init_declarator")
            .unwrap_or(capture);
        return (declarator.end_byte(), nowhere);
    }
    if is_declaration(parent.kind(), language) {
        return (parent.end_byte(), nowhere);
    }
    if !matches_value(parent.kind(), language) {
        return (capture.end_byte(), nowhere);
    }
    let mut value = capture.next_named_sibling();
    while let Some(v) = value.filter(Node::is_extra) {
        value = v.next_named_sibling();
    }
    let end = match parent.kind() {
        // Its start, end and step.
        "for_numeric_clause" => parent.end_byte(),
        _ => value.map_or_else(|| capture.end_byte(), |v| v.end_byte()),
    };
    (
        node.end_byte(),
        [capture.end_byte()..end, else_part(parent, language)],
    )
}

/// The part of the condition `binds` is in that runs when it fails, where
/// what it binds isn't: Rust's and Swift's `else`.
fn else_part(binds: Node, language: &str) -> Range<usize> {
    // `a && let b = c && …` chains conditions, in no more levels than code
    // has.
    const CHAINED: usize = 8;
    let found = match language {
        "swift" => {
            let mut cursor = binds.walk();
            (binds.children(&mut cursor).find(|c| c.kind() == "else"))
                .map(|otherwise| otherwise.start_byte()..binds.end_byte())
        }
        "rust" => std::iter::successors(binds.parent(), Node::parent)
            .take(CHAINED)
            .find(|p| p.kind() != "let_chain")
            .filter(|p| p.kind() == "if_expression")
            .and_then(|p| p.child_by_field_name("alternative"))
            .map(|otherwise| otherwise.byte_range()),
        _ => None,
    };
    found.unwrap_or_default()
}

/// Where the name `at` (its bytes in `source`) is a local or a parameter
/// of `language`, parsed as `tree`, by `query`; `None` when `stop` says to
/// before it's found.
pub(super) fn resolve(
    query: &LocalsQuery,
    language: &str,
    tree: &Tree,
    source: &[u8],
    at: Range<usize>,
    stop: &dyn Fn() -> bool,
) -> Option<Local> {
    let root = tree.root_node();
    let used = root.descendant_for_byte_range(at.start, at.end)?;
    let php_function =
        language == "php" && used.parent().is_some_and(|p| p.kind() != "variable_name");
    if used.byte_range() != at
        || !is_name(used.kind(), language)
        || php_function
        || !is_reference(used)
    {
        return None;
    }
    let name = &source[at.clone()];
    // The top-level item around the use: definitions the file's top scope
    // makes are read from it alone, but C's file-scope ones.
    let item = (root.first_child_for_byte(at.start))
        .filter(|c| c.start_byte() <= at.start && at.end <= c.end_byte())?;
    let (mut scopes, captured) = read(query, language, item, source, name, stop)?;
    let mut by_scope = in_scopes(&mut scopes, captured, language, &at, stop)?;
    if language == "c" || language == "cpp" {
        by_scope
            .entry(None)
            .or_default()
            .extend(file_scope(root, source, name));
    }
    let around = (scopes.iter().enumerate())
        .filter(|(_, s)| s.range.start <= at.start && at.end <= s.range.end);
    // Innermost first, then the file's top scope.
    let mut order: Vec<Option<usize>> = around.map(|(i, _)| Some(i)).collect();
    order.reverse();
    order.push(None);
    let hoists = matches!(language, "python" | "javascript" | "typescript" | "tsx");
    // Whether a function is between the use and the scope being looked in.
    let mut inside = false;
    for scope in order {
        let class = scope.is_some_and(|i| scopes[i].class);
        let here = by_scope.get(&scope).map_or(&[][..], Vec::as_slice);
        if class && inside {
            continue;
        }
        inside |= scope.is_some() && !class;
        if here.iter().any(|d| d.says == Says::Global) {
            return None;
        }
        if here.iter().any(|d| d.says == Says::Nonlocal) {
            continue;
        }
        let here = || here.iter().filter(|d| d.node.byte_range() != at);
        let chosen = here()
            .filter(|d| d.seen_from(at.start))
            .max_by_key(|d| d.node.start_byte())
            .or_else(|| {
                here()
                    .filter(|d| {
                        (hoists || d.says == Says::Defines(DefinitionKind::Parameter))
                            && !d.hidden_at(at.start)
                    })
                    .min_by_key(|d| d.node.start_byte())
            });
        let Some(d) = chosen else {
            continue;
        };
        // What the file's top scope or a class defines is the index's.
        if class || (scope.is_none() && !matches!(language, "c" | "cpp")) {
            return None;
        }
        return Some(Local {
            kind: match (scope, d.says) {
                (None, _) => DefinitionKind::Variable,
                (_, Says::Defines(kind)) => kind,
                _ => DefinitionKind::Local,
            },
            name: d.node.byte_range(),
            line: d.node.start_position().row,
        });
    }
    None
}

/// The scopes of `item`, and the captures of `name` in it, each with the
/// name's node, as `query` finds them; `None` when `stop` says to.
fn read<'t>(
    query: &LocalsQuery,
    language: &str,
    item: Node<'t>,
    source: &[u8],
    name: &[u8],
    stop: &dyn Fn() -> bool,
) -> Option<(Vec<Scope<'t>>, Vec<Captured<'t>>)> {
    let (mut scopes, mut captured) = (Vec::new(), Vec::new());
    let mut progress = |_: &QueryCursorState| {
        if stop() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches_with_options(
        &query.query,
        item,
        source,
        QueryCursorOptions::new().progress_callback(&mut progress),
    );
    while let Some(m) = matches.next() {
        for c in m.captures() {
            let is = |index: Option<u32>| Some(c.index) == index;
            let says = if is(query.scope) || is(query.class) {
                scopes.push(Scope {
                    node: c.node,
                    range: c.node.byte_range(),
                    class: is(query.class),
                });
                continue;
            } else if is(query.parameter) {
                Says::Defines(DefinitionKind::Parameter)
            } else if is(query.var) {
                Says::Defines(DefinitionKind::Local)
            } else if is(query.hoisted) {
                Says::Hoisted
            } else if is(query.global) {
                Says::Global
            } else if is(query.nonlocal) {
                Says::Nonlocal
            } else {
                continue;
            };
            if memchr::memmem::find(&source[c.node.byte_range()], name).is_none() {
                continue;
            }
            for node in bound(c.node, language, source) {
                if &source[node.byte_range()] == name {
                    captured.push((c.node, node, says));
                }
            }
        }
    }
    drop(matches);
    (!stop()).then_some((scopes, captured))
}

/// The `captured` names, each with its capture, by the scope they're in
/// (its place in `scopes`, sorted outermost first; `None` for the file's top
/// scope), those of scopes around `at` alone. `None` when `stop` says to.
fn in_scopes<'t>(
    scopes: &mut Vec<Scope<'t>>,
    mut captured: Vec<Captured<'t>>,
    language: &str,
    at: &Range<usize>,
    stop: &dyn Fn() -> bool,
) -> Option<FxHashMap<Option<usize>, Vec<Defined<'t>>>> {
    scopes.sort_by_key(|s| (s.range.start, std::cmp::Reverse(s.range.end)));
    scopes.dedup_by(|b, a| a.range == b.range);
    captured.sort_by_key(|(_, node, _)| node.start_byte());
    let around = |i: usize| {
        let s: &Range<usize> = &scopes[i].range;
        s.start <= at.start && at.end <= s.end
    };
    let mut out: FxHashMap<Option<usize>, Vec<Defined>> = FxHashMap::default();
    // The scopes open where the sweep is, innermost last.
    let (mut open, mut next): (Vec<usize>, usize) = (Vec::new(), 0);
    for (n, (capture, node, says)) in captured.into_iter().enumerate() {
        if n % 256 == 0 && stop() {
            return None;
        }
        let (start, end) = (node.start_byte(), node.end_byte());
        while next < scopes.len() && scopes[next].range.start <= start {
            while open
                .last()
                .is_some_and(|&i| scopes[i].range.end <= scopes[next].range.start)
            {
                open.pop();
            }
            open.push(next);
            next += 1;
        }
        while open.last().is_some_and(|&i| scopes[i].range.end < end) {
            open.pop();
        }
        // A function's name belongs to the scope around the function, and a
        // `var` to the function around its blocks.
        let holds = |i: usize| match says {
            Says::Hoisted => !is_block(scopes[i].node.kind()),
            _ => (scopes[i].node.child_by_field_name("name")).is_none_or(|n| n.id() != node.id()),
        };
        // Only the scopes around the use count: those past the first that
        // isn't one aren't reached.
        let mut within = open.iter().rev().copied();
        let scope = loop {
            match within.next() {
                Some(i) if holds(i) => break Some(i),
                Some(_) => {}
                None => break None,
            }
        };
        if scope.is_none_or(around) {
            let (visible, hidden) = visibility(capture, node, says, language);
            out.entry(scope).or_default().push(Defined {
                node,
                says,
                visible,
                hidden,
            });
        }
    }
    Some(out)
}

/// Whether a scope of `kind` is a block within a JavaScript function.
fn is_block(kind: &str) -> bool {
    matches!(
        kind,
        "statement_block" | "for_statement" | "for_in_statement" | "catch_clause"
    )
}

/// Whether the name `at` in `tree` is a member (`x.p`, `x->p`), a path's
/// last segment (`a::p`), an object's key or a keyword argument's name,
/// not a name of its own.
pub(super) fn is_member(tree: &Tree, at: Range<usize>) -> bool {
    let Some(node) = tree.root_node().descendant_for_byte_range(at.start, at.end) else {
        return false;
    };
    if member_of(node).is_some_and(|(object, _)| object.is_some()) {
        return true;
    }
    let Some(parent) = node.parent() else {
        return false;
    };
    match field_of(node, parent) {
        Some("name") => parent.kind() == "keyword_argument",
        Some("key") => parent.kind() == "pair",
        _ => false,
    }
}

/// The field `node` is of its `parent`.
fn field_of<'t>(node: Node<'t>, parent: Node<'t>) -> Option<&'t str> {
    (0..parent.child_count())
        .find(|&i| parent.child(i).is_some_and(|c| c.id() == node.id()))
        .and_then(|i| parent.field_name_for_child(i))
}

/// Whether `node`, a name, is used as a value or a binding of its own, not
/// a member, a method called, a path's segment or a keyword argument's
/// name.
fn is_reference(node: Node) -> bool {
    if member_of(node).is_some() {
        return false;
    }
    let Some(parent) = node.parent() else {
        return true;
    };
    let path = matches!(
        parent.kind(),
        "scoped_identifier" | "scoped_type_identifier" | "qualified_identifier"
    );
    // A named argument's label is the callee's parameter: Python's `f(x=1)`,
    // C#'s `f(x: 1)`, Kotlin's `f(x = 1)`.
    let label = match parent.kind() {
        "keyword_argument" | "argument" => field_of(node, parent) == Some("name"),
        "value_argument" => node.next_sibling().is_some_and(|next| next.kind() == "="),
        _ => false,
    };
    !(path || label)
}

/// The names a definition's capture binds: itself when it's a name, else
/// those of its declarator or pattern, in order. Walked with a stack of its
/// own: a file may nest them deeper than the thread's.
fn bound<'t>(node: Node<'t>, language: &str, source: &[u8]) -> Vec<Node<'t>> {
    let (mut out, mut left) = (Vec::new(), vec![node]);
    while let Some(node) = left.pop() {
        if is_name(node.kind(), language) {
            // A Rust or Scala pattern's capitalized name is a unit variant
            // or a constant it compares with (`None`, `Nil`), not a binding.
            let variant = matches!(language, "rust" | "scala")
                && source[node.start_byte()].is_ascii_uppercase();
            if !variant {
                out.push(node);
            }
            continue;
        }
        match node.kind() {
            // What a pattern or a declarator holds but doesn't bind.
            "scoped_identifier"
            | "type_identifier"
            | "primitive_type"
            | "field_identifier"
            | "function_declarator"
            | "string_literal"
            | "integer_literal"
            | "number_literal"
            | "attribute"
            | "subscript"
            | "member_expression"
            | "field_expression"
            | "selector_expression"
            | "index_expression" => continue,
            _ => {}
        }
        if let Some(declarator) = node.child_by_field_name("declarator") {
            left.push(declarator);
            continue;
        }
        let first = left.len();
        let mut cursor = node.walk();
        let mut more = cursor.goto_first_child();
        while more {
            let held = matches!(
                cursor.field_name(),
                Some("type" | "value" | "function" | "default_value" | "size")
            );
            if cursor.node().is_named() && !held {
                left.push(cursor.node());
            }
            more = cursor.goto_next_sibling();
        }
        left[first..].reverse();
    }
    out
}

/// The nodes around `node` in `tree`, outermost first, found from the root
/// in one way down: climbing by parents searches from the root at each.
fn ancestors<'t>(tree: &'t Tree, node: Node<'t>) -> Vec<Node<'t>> {
    let mut cursor = tree.walk();
    let mut out = Vec::new();
    while cursor.node().id() != node.id() {
        out.push(cursor.node());
        if cursor
            .goto_first_child_for_byte(node.start_byte())
            .is_none()
            || cursor.node().end_byte() < node.end_byte()
        {
            break;
        }
    }
    out
}

/// The names C's file-scope declarations of variables bind that are
/// `name`: not a function's prototype.
fn file_scope<'t>(root: Node<'t>, source: &[u8], name: &[u8]) -> Vec<Defined<'t>> {
    let mut out = Vec::new();
    let mut items = root.walk();
    for item in root.named_children(&mut items) {
        if item.kind() != "declaration"
            || memchr::memmem::find(&source[item.byte_range()], name).is_none()
        {
            continue;
        }
        let mut cursor = item.walk();
        for declarator in item.children_by_field_name("declarator", &mut cursor) {
            for node in bound(declarator, "c", source) {
                if &source[node.byte_range()] == name {
                    let says = Says::Defines(DefinitionKind::Local);
                    let (visible, hidden) = visibility(declarator, node, says, "c");
                    out.push(Defined {
                        node,
                        says,
                        visible,
                        hidden,
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;

/// What a member's receiver is (`x` in `x.p`, `x->p`, `x::p`), as far as
/// the asking file tells.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Receiver {
    /// The type it has: one a variable or a parameter declares, one a
    /// constructor or a literal gives it, `self`'s or `this`'s class or
    /// impl, or itself when it names a type (`T::new`).
    pub ty: Option<String>,
    /// The call it's the value of, when it is one: what the called
    /// function returns is its type.
    pub call: Option<Call>,
    /// Its name when it's a plain one, which a module or a namespace
    /// imported may be.
    pub name: Option<String>,
}

/// A call a receiver's value comes from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Call {
    /// The function or the method called.
    pub name: String,
    /// The type a method is called on, when the file tells it.
    pub container: Option<String>,
    /// The name a function is called through when it isn't a type's or a
    /// variable's: a module or a namespace imported (`requests.session()`).
    pub module: Option<String>,
}

/// What a receiver's type is read as: a type's name, or a call whose
/// declared return type is the index's to tell.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Typed {
    Name(String),
    /// With the type to take when the index doesn't tell the return type:
    /// the one `T::f()` and `T.new` are called on.
    Call(Call, Option<String>),
}

/// The kinds of a member access: `x.p`, `x->p`, `x::p`, `x:p()`, and the
/// calls that are one (Java's `x.p()`, PHP's `$x->p()`, Ruby's `x.p`).
/// Swift's and Kotlin's `navigation_expression` and Elixir's `dot` are
/// read apart, their fields being others.
const ACCESSES: &[&str] = &[
    "member_expression",
    "field_expression",
    "selector_expression",
    "attribute",
    "scoped_identifier",
    "scoped_type_identifier",
    "qualified_identifier",
    "method_invocation",
    "field_access",
    "member_access_expression",
    "member_call_expression",
    "nullsafe_member_access_expression",
    "nullsafe_member_call_expression",
    "scoped_call_expression",
    "call",
    "dot_index_expression",
    "method_index_expression",
];

/// The fields an access holds its member in.
const MEMBERS: &[&str] = &["property", "field", "member", "attribute", "method", "name"];

/// The fields an access holds its receiver in.
const RECEIVERS: &[&str] = &[
    "value",
    "operand",
    "object",
    "argument",
    "path",
    "scope",
    "expression",
    "receiver",
    "table",
];

/// The receiver and the member of `node` when it's a member access: `x`
/// and `p` of `x.p`; no receiver for a call of a method on nothing named
/// (Java's `p()`, Ruby's `p x`).
fn access(node: Node) -> Option<(Option<Node>, Node)> {
    match node.kind() {
        "navigation_expression" => {
            let suffix = (node.child_by_field_name("suffix"))
                .or_else(|| last_named_of(node, &["navigation_suffix"]))?;
            let name = (suffix.child_by_field_name("suffix")).or_else(|| suffix.named_child(0))?;
            let object = node
                .child_by_field_name("target")
                .or_else(|| node.named_child(0));
            Some((object.filter(|o| o.id() != suffix.id()), name))
        }
        "dot" => Some((
            node.child_by_field_name("left"),
            node.child_by_field_name("right")?,
        )),
        kind if ACCESSES.contains(&kind) => {
            let name = MEMBERS.iter().find_map(|f| node.child_by_field_name(f))?;
            let object = RECEIVERS.iter().find_map(|f| node.child_by_field_name(f));
            Some((object, name))
        }
        _ => None,
    }
}

/// The access `node` is the member of, and its receiver.
fn member_of(node: Node) -> Option<(Option<Node>, Node)> {
    let mut parent = node.parent()?;
    if parent.kind() == "navigation_suffix" {
        parent = parent.parent()?;
    }
    let (object, name) = access(parent)?;
    (name.id() == node.id()).then_some((object, parent))
}

/// The last named child of `node` of one of `kinds`.
fn last_named_of<'t>(node: Node<'t>, kinds: &[&str]) -> Option<Node<'t>> {
    (node.named_children(&mut node.walk()))
        .filter(|c| kinds.contains(&c.kind()))
        .last()
}

/// The receiver of the member `at` (its bytes in `source`) of `language`,
/// parsed as `tree`; `None` when it's no member. A variable's type is
/// read where `query`, the language's locals query, finds it defined, until
/// `stop` says to.
pub(super) fn receiver(
    query: Option<&LocalsQuery>,
    language: &str,
    tree: &Tree,
    source: &[u8],
    at: Range<usize>,
    stop: &dyn Fn() -> bool,
) -> Option<Receiver> {
    let node = (tree.root_node()).descendant_for_byte_range(at.start, at.end)?;
    if node.byte_range() != at {
        return None;
    }
    let object = member_of(node)?.0?;
    let reading = Reading {
        query,
        language,
        tree,
        source,
        stop,
    };
    let name = matches!(
        object.kind(),
        "identifier" | "namespace_identifier" | "simple_identifier"
    )
    .then(|| reading.text(object).to_owned());
    let (ty, call) = match reading.type_of(object, 0) {
        Some(Typed::Name(ty)) => (Some(ty), None),
        Some(Typed::Call(call, otherwise)) => (otherwise, Some(call)),
        None => (None, None),
    };
    Some(Receiver { ty, call, name })
}

/// Kinds of loop, and of a match's arm, whose bindings have no type read
/// from their value: a loop's is an element's, an arm's the matched one's.
const LOOPS: &[&str] = &[
    "match_arm",
    "case_clause",
    "stab_clause",
    "for_expression",
    "for_statement",
    "for_in_statement",
    "for_in_clause",
    "range_clause",
    "enhanced_for_statement",
    "foreach_statement",
    "for_range_loop",
    "for_generic_clause",
];

/// Kinds of a block of statements.
const BLOCKS: &[&str] = &[
    "block",
    "compound_statement",
    "statements",
    "function_body",
    "body_statement",
    "do_block",
    "block_body",
    "template_body",
    "class_body",
    "declaration_list",
];

/// Kinds a declaration's type is parsed as where the grammar names no
/// field (Kotlin's, Swift's).
const UNFIELDED_TYPES: &[&str] = &["user_type", "nullable_type", "type_annotation"];

/// Types that leave the type to the value: `var`, `auto`.
const INFERRED: &[&str] = &["implicit_type", "placeholder_type_specifier"];

/// How deep a variable's type is followed: `let a = b;` reads `b`'s.
const TYPE_DEPTH: usize = 4;

struct Reading<'a> {
    query: Option<&'a LocalsQuery>,
    language: &'a str,
    tree: &'a Tree,
    source: &'a [u8],
    stop: &'a dyn Fn() -> bool,
}

impl Reading<'_> {
    fn text(&self, node: Node) -> &str {
        std::str::from_utf8(&self.source[node.byte_range()]).unwrap_or_default()
    }

    /// The type expression `node` has, as far as the file tells.
    fn type_of(&self, node: Node, depth: usize) -> Option<Typed> {
        if depth > TYPE_DEPTH {
            return None;
        }
        let named = |name: Option<String>| name.map(Typed::Name);
        match node.kind() {
            "self" | "this" | "self_expression" | "this_expression" => named(self.enclosing(node)),
            // PHP's `$x`, its name inside.
            "variable_name" if self.language == "php" => {
                let name = node.named_child(0)?;
                if self.text(name) == "this" {
                    return named(self.enclosing(node));
                }
                self.local_type(name, depth)
            }
            kind if is_name(kind, self.language)
                || matches!(
                    kind,
                    "type_identifier" | "namespace_identifier" | "constant"
                ) =>
            {
                if matches!(self.text(node), "self" | "this" | "Self") {
                    return named(self.enclosing(node));
                }
                self.local_type(node, depth)
            }
            "parenthesized_expression"
            | "reference_expression"
            | "unary_expression"
            | "pointer_expression"
            | "await_expression"
            | "try_expression"
            | "non_null_expression" => self.type_of(node.named_child(0)?, depth + 1),
            "as_expression" | "satisfies_expression" | "type_assertion" => named(
                (node.child_by_field_name("type"))
                    .or_else(|| node.named_child(1))
                    .and_then(|t| type_name(t, self.source)),
            ),
            "call_expression"
            | "call"
            | "method_invocation"
            | "invocation_expression"
            | "function_call"
            | "member_call_expression"
            | "scoped_call_expression"
            | "function_call_expression" => self.called(node, depth),
            "new_expression" => named(type_name(
                node.child_by_field_name("constructor")?,
                self.source,
            )),
            "object_creation_expression" | "composite_literal" | "instance_expression" => named(
                (node.child_by_field_name("type"))
                    .or_else(|| node.named_child(0))
                    .and_then(|t| type_name(t, self.source)),
            ),
            "struct_expression" => named(type_name(node.child_by_field_name("name")?, self.source)),
            _ => None,
        }
    }

    /// The type of the name `node`: a local's or a parameter's, else a
    /// type's own when it's capitalized.
    fn local_type(&self, node: Node, depth: usize) -> Option<Typed> {
        if let Some(query) = self.query
            && let Some(local) = resolve(
                query,
                self.language,
                self.tree,
                self.source,
                node.byte_range(),
                self.stop,
            )
        {
            let defined = (self.tree.root_node())
                .descendant_for_byte_range(local.name.start, local.name.end)?;
            return self.declared(defined, depth);
        }
        let text = self.text(node);
        text.starts_with(|c: char| c.is_ascii_uppercase())
            .then(|| Typed::Name(text.to_owned()))
    }

    /// What the call `node` gives: a type's constructor its type (Python's
    /// `T()`, `T::new()`, Ruby's `T.new`), else the call, with the type a
    /// method is called on or the module a function is called through.
    fn called(&self, node: Node, depth: usize) -> Option<Typed> {
        let (object, member) = if let Some(found) = access(node) {
            found
        } else {
            let callee = ["function", "name", "target"]
                .iter()
                .find_map(|f| node.child_by_field_name(f))
                .or_else(|| node.named_child(0))?;
            access(callee).unwrap_or((None, callee))
        };
        let name = self.text(member).to_owned();
        let capitalized = |s: &str| s.starts_with(|c: char| c.is_ascii_uppercase());
        // Where calling a type constructs one, a capitalized function is a
        // type: not in Go or C#, whose functions are capitalized too.
        let constructs = matches!(
            self.language,
            "python" | "kotlin" | "swift" | "cpp" | "scala" | "rust"
        );
        let Some(object) = object else {
            // Elsewhere a capitalized function may still be a type's (C#'s
            // `Foo()` is a method, Go's `New()` a function), so it's only the
            // guess past what the index tells of it.
            if constructs && capitalized(&name) {
                return Some(Typed::Name(name));
            }
            let otherwise = capitalized(&name).then(|| name.clone());
            return Some(Typed::Call(
                Call {
                    name,
                    ..Call::default()
                },
                otherwise,
            ));
        };
        let path = matches!(
            object.parent().map(|p| p.kind()),
            Some("scoped_identifier" | "qualified_identifier")
        );
        let on = match self.type_of(object, depth + 1) {
            Some(Typed::Name(ty)) => Some(ty),
            _ => None,
        };
        // A capitalized function a module has is a type's constructor
        // (Python's `m.T()`).
        if constructs && on.is_none() && capitalized(&name) {
            return Some(Typed::Name(name));
        }
        let untyped_name = on.is_none()
            && matches!(
                object.kind(),
                "identifier" | "simple_identifier" | "namespace_identifier"
            );
        // `T::f()` and `T.new` are taken to give a `T`.
        let otherwise = on
            .clone()
            .filter(|ty| capitalized(ty) && (path || name == "new"));
        Some(Typed::Call(
            Call {
                name,
                container: on,
                module: untyped_name.then(|| self.text(object).to_owned()),
            },
            otherwise,
        ))
    }

    /// The type of the variable or the parameter whose name is `defined`:
    /// the one declared, else that of the value it's given.
    fn declared<'t>(&self, defined: Node<'t>, depth: usize) -> Option<Typed> {
        // Up to the statement it's in: past it a function's return type
        // and its body are no type of the name's.
        let up = || {
            std::iter::successors(defined.parent(), Node::parent)
                .take_while(|p| {
                    p.child_by_field_name("body").is_none() && !BLOCKS.contains(&p.kind())
                })
                .take(4)
        };
        let within =
            |n: Node| n.start_byte() <= defined.start_byte() && defined.end_byte() <= n.end_byte();
        let declared_type = |p: Node<'t>| -> Option<Node<'t>> {
            (p.child_by_field_name("type"))
                .or_else(|| last_named_of(p, UNFIELDED_TYPES))
                .filter(|t| !within(*t) && !INFERRED.contains(&t.kind()))
                .filter(|t| !matches!(self.text(*t), "var" | "auto" | "val" | "let"))
        };
        if let Some(ty) = up().find_map(declared_type) {
            return type_name(ty, self.source).map(Typed::Name);
        }
        // A loop declaring its variable's type: Java's `for (T x : xs)`,
        // C#'s `foreach (T x in xs)`, C++'s `for (T &x : xs)`.
        let statement = std::iter::successors(defined.parent(), Node::parent)
            .find(|p| p.child_by_field_name("body").is_some() || BLOCKS.contains(&p.kind()));
        if let Some(statement) = statement.filter(|s| LOOPS.contains(&s.kind()))
            && let Some(ty) = declared_type(statement)
        {
            return type_name(ty, self.source).map(Typed::Name);
        }
        // A loop's binding has an element's type, not its collection's.
        let (given, value) = up()
            .take_while(|p| !LOOPS.contains(&p.kind()))
            .find_map(|p| Some((p, self.value_of(p)?)))
            .filter(|(_, v)| !within(*v))?;
        // Go's `a, b := x, y` and Lua's `local a, b = x, y` give each name
        // the value in its place.
        let value = match value.kind() {
            "expression_list" => {
                let names = (given.child_by_field_name("left"))
                    .or_else(|| last_named_of(given, &["variable_list"]))?;
                let place = (names.named_children(&mut names.walk())).position(within)?;
                value.named_child(u32::try_from(place).ok()?)?
            }
            _ => value,
        };
        self.type_of(value, depth + 1)
    }

    /// The value a declaration or an assignment `p` gives.
    fn value_of<'t>(&self, p: Node<'t>) -> Option<Node<'t>> {
        if let Some(value) = ["value", "right"]
            .iter()
            .find_map(|f| p.child_by_field_name(f))
        {
            return Some(value);
        }
        match (self.language, p.kind()) {
            ("lua", "assignment_statement") => last_named_of(p, &["expression_list"]),
            // Where the grammar names no field, the value comes last.
            ("kotlin", "property_declaration")
            | ("csharp", "variable_declarator")
            | ("zig", "variable_declaration") => {
                p.named_child(u32::try_from(p.named_child_count().checked_sub(1)?).ok()?)
            }
            _ => None,
        }
    }

    /// The class or the impl around `node`, as `self` or `this` names it.
    fn enclosing(&self, node: Node) -> Option<String> {
        for p in ancestors(self.tree, node).into_iter().rev() {
            match p.kind() {
                "impl_item" => return type_name(p.child_by_field_name("type")?, self.source),
                "class_declaration"
                | "class"
                | "abstract_class_declaration"
                | "class_definition"
                | "class_specifier"
                | "struct_specifier"
                | "struct_declaration"
                | "record_declaration"
                | "enum_declaration"
                | "interface_declaration"
                | "object_declaration"
                | "object_definition" => {
                    let name = (p.child_by_field_name("name"))
                        .or_else(|| last_named_of(p, &["type_identifier"]))?;
                    return Some(self.text(name).to_owned());
                }
                // C++'s `int Point::area() { … }`.
                "function_definition"
                    if let Some(declared) = (p.child_by_field_name("declarator"))
                        .and_then(|d| d.child_by_field_name("declarator"))
                        && declared.kind() == "qualified_identifier" =>
                {
                    return type_name(declared.child_by_field_name("scope")?, self.source);
                }
                // Lua's `function T:m() … end`.
                "function_declaration"
                    if let Some(name) = p.child_by_field_name("name")
                        && name.kind() == "method_index_expression" =>
                {
                    return Some(self.text(name.child_by_field_name("table")?).to_owned());
                }
                _ => {}
            }
        }
        None
    }
}

/// The name of the type `node`, a type expression, is: past its pointers
/// and references, its package or module, and its arguments, and past a
/// wrapper's to what it wraps (`Option<T>`, `Promise<T>`, Python's
/// `Optional[T]`, a quoted forward reference).
pub(super) fn type_name(node: Node, source: &[u8]) -> Option<String> {
    const NAMES: &[&str] = &[
        "type_identifier",
        "identifier",
        "primitive_type",
        "namespace_identifier",
        "simple_identifier",
        "constant",
        "name",
    ];
    const WRAPPERS: &[&str] = &[
        "Option", "Result", "Box", "Rc", "Arc", "Optional", "Promise",
    ];
    const ARGUMENTS: &[&str] = &[
        "type_arguments",
        "type_argument_list",
        "type_parameter",
        "type_projection",
    ];
    let text = |n: Node| std::str::from_utf8(&source[n.byte_range()]).unwrap_or_default();
    let mut node = node;
    for _ in 0..12 {
        if NAMES.contains(&node.kind()) {
            let name = text(node);
            // A wrapper's argument, beside its name under the generic type.
            if WRAPPERS.contains(&name)
                && let Some(arguments) = node.parent().and_then(|p| last_named_of(p, ARGUMENTS))
                && let Some(first) = arguments.named_child(0)
            {
                node = first;
                continue;
            }
            return Some(name.to_owned());
        }
        if node.kind() == "string" {
            let quoted = text(node).trim_matches(|c| c == '"' || c == '\'');
            return quoted.rsplit('.').next().map(str::to_owned);
        }
        node = (["name", "type"].iter())
            .find_map(|f| node.child_by_field_name(f))
            .or_else(|| {
                (node.named_children(&mut node.walk())).find(|c| {
                    !matches!(
                        c.kind(),
                        "type_arguments" | "lifetime" | "mutable_specifier" | "comment"
                    )
                })
            })?;
    }
    None
}

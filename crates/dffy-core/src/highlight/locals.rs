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
//! or impl, else a capitalized name as a type. Nothing is inferred past
//! the file: a value a function returns has no type.
//!
//! To cover a language: write `queries/<language>/locals.scm` with the
//! captures above, list it in the grammar's `locals` in [`super::lang`],
//! and add a fixture to `locals/tests.rs`.

use std::ops::Range;

use tree_sitter::{Node, Query, QueryCursor, StreamingIterator, Tree};

use super::DefinitionKind;
use super::scope::roots;

/// A compiled locals query and its capture indices.
pub(super) struct LocalsQuery {
    query: Query,
    scope: Option<u32>,
    var: Option<u32>,
    parameter: Option<u32>,
}

impl LocalsQuery {
    /// `None` when `source` doesn't compile against `grammar`.
    pub(super) fn new(grammar: &tree_sitter::Language, source: &str) -> Option<Self> {
        let query = Query::new(grammar, source).ok()?;
        // Each pattern starts with a named node, as the tags query's must.
        roots(grammar, &query, source)?;
        Some(Self {
            scope: query.capture_index_for_name("local.scope"),
            var: query.capture_index_for_name("local.definition.var"),
            parameter: query.capture_index_for_name("local.definition.parameter"),
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

/// A definition found by the query: its name's node, its kind, the byte
/// past which it's visible (a variable past its declaration, so `let x =
/// x + 1` reads the `x` before), and the scope it's in (`None` for the
/// file's top scope).
struct Defined<'t> {
    node: Node<'t>,
    kind: DefinitionKind,
    visible: usize,
    scope: Option<Range<usize>>,
}

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

/// Where the name `at` (its bytes in `source`) is a local or a parameter
/// of `language`, parsed as `tree`, by `query`.
pub(super) fn resolve(
    query: &LocalsQuery,
    language: &str,
    tree: &Tree,
    source: &[u8],
    at: Range<usize>,
) -> Option<Local> {
    let root = tree.root_node();
    let used = root.descendant_for_byte_range(at.start, at.end)?;
    if used.byte_range() != at || !NAME_KINDS.contains(&used.kind()) || !is_reference(used) {
        return None;
    }
    let name = &source[at.clone()];
    // The top-level item around the use: definitions the file's top scope
    // makes are read from it alone, but C's file-scope ones.
    let item = (0..root.child_count())
        .filter_map(|i| root.child(i))
        .find(|c| c.start_byte() <= at.start && at.end <= c.end_byte())?;
    let mut scopes: Vec<Range<usize>> = Vec::new();
    let mut defined: Vec<Defined> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&query.query, item, source);
    while let Some(m) = matches.next() {
        for c in m.captures() {
            let kind = if Some(c.index) == query.scope {
                scopes.push(c.node.byte_range());
                continue;
            } else if Some(c.index) == query.parameter {
                DefinitionKind::Parameter
            } else if Some(c.index) == query.var {
                DefinitionKind::Local
            } else {
                continue;
            };
            // A declaration's names are visible past it; a pattern's (a
            // match arm's, a loop's) past the pattern.
            let visible = (c.node.parent())
                .filter(|p| kind == DefinitionKind::Local && DECLARATIONS.contains(&p.kind()))
                .map_or_else(|| c.node.end_byte(), |p| p.end_byte());
            for node in bound(c.node, language, source) {
                if &source[node.byte_range()] == name {
                    defined.push(Defined {
                        node,
                        kind,
                        visible,
                        scope: None,
                    });
                }
            }
        }
    }
    if language == "c" || language == "cpp" {
        defined.extend(file_scope(root, source, name));
    }
    scopes.sort_by_key(|s| (s.start, std::cmp::Reverse(s.end)));
    scopes.dedup();
    for d in &mut defined {
        d.scope = scope_of(&scopes, d.node);
    }
    let around: Vec<&Range<usize>> = (scopes.iter())
        .filter(|s| s.start <= at.start && at.end <= s.end)
        .collect();
    // Innermost first, then the file's top scope.
    let mut order: Vec<Option<&Range<usize>>> = around.into_iter().rev().map(Some).collect();
    order.push(None);
    let hoists = matches!(language, "python" | "javascript" | "typescript" | "tsx");
    for scope in order {
        let here: Vec<&Defined> = (defined.iter())
            .filter(|d| d.scope.as_ref() == scope && d.node.byte_range() != at)
            .collect();
        let before = (here.iter())
            .filter(|d| d.visible <= at.start)
            .max_by_key(|d| d.node.start_byte());
        let chosen = before.or_else(|| {
            (here.iter())
                .filter(|d| hoists || d.kind == DefinitionKind::Parameter)
                .min_by_key(|d| d.node.start_byte())
        });
        if let Some(d) = chosen {
            if scope.is_none() && !matches!(language, "c" | "cpp") {
                return None;
            }
            return Some(Local {
                kind: if scope.is_none() {
                    DefinitionKind::Variable
                } else {
                    d.kind
                },
                name: d.node.byte_range(),
                line: d.node.start_position().row,
            });
        }
    }
    None
}

/// Whether the name `at` in `tree` is a member (`x.p`, `x->p`), a path's
/// last segment (`a::p`), an object's key or a keyword argument's name,
/// not a name of its own.
pub(super) fn is_member(tree: &Tree, at: Range<usize>) -> bool {
    let Some(node) = tree.root_node().descendant_for_byte_range(at.start, at.end) else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    match field_of(node, parent) {
        Some("attribute" | "field" | "property") => true,
        Some("name") => matches!(
            parent.kind(),
            "scoped_identifier"
                | "scoped_type_identifier"
                | "qualified_identifier"
                | "keyword_argument"
        ),
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
/// a member, a path's segment or a keyword argument's name.
fn is_reference(node: Node) -> bool {
    let Some(parent) = node.parent() else {
        return true;
    };
    let field = field_of(node, parent);
    let member = matches!(field, Some("attribute" | "field" | "property"));
    let path = matches!(
        parent.kind(),
        "scoped_identifier" | "scoped_type_identifier" | "qualified_identifier"
    );
    let argument_name = parent.kind() == "keyword_argument" && field == Some("name");
    !(member || path || argument_name)
}

/// The names a definition's capture binds: itself when it's a name, else
/// those of its declarator or pattern.
fn bound<'t>(node: Node<'t>, language: &str, source: &[u8]) -> Vec<Node<'t>> {
    let mut out = Vec::new();
    collect(node, language, source, &mut out);
    out
}

fn collect<'t>(node: Node<'t>, language: &str, source: &[u8], out: &mut Vec<Node<'t>>) {
    if NAME_KINDS.contains(&node.kind()) {
        // A Rust pattern's capitalized name is a unit variant or a
        // constant it compares with (`None`), not a binding.
        let variant = language == "rust" && source[node.start_byte()].is_ascii_uppercase();
        if !variant {
            out.push(node);
        }
        return;
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
        | "index_expression" => {
            return;
        }
        _ => {}
    }
    if let Some(declarator) = node.child_by_field_name("declarator") {
        collect(declarator, language, source, out);
        return;
    }
    for i in 0..node.named_child_count() {
        let Some(child) = node.named_child(u32::try_from(i).unwrap_or(u32::MAX)) else {
            continue;
        };
        let field = node.field_name_for_named_child(u32::try_from(i).unwrap_or(u32::MAX));
        if matches!(
            field,
            Some("type" | "value" | "function" | "default_value" | "size")
        ) {
            continue;
        }
        collect(child, language, source, out);
    }
}

/// The names C's file-scope declarations of variables bind that are
/// `name`: not a function's prototype.
fn file_scope<'t>(root: Node<'t>, source: &[u8], name: &[u8]) -> Vec<Defined<'t>> {
    let mut out = Vec::new();
    for i in 0..root.named_child_count() {
        let Some(child) = root.named_child(u32::try_from(i).unwrap_or(u32::MAX)) else {
            continue;
        };
        if child.kind() != "declaration" {
            continue;
        }
        let mut cursor = child.walk();
        for declarator in child.children_by_field_name("declarator", &mut cursor) {
            for node in bound(declarator, "c", source) {
                if &source[node.byte_range()] == name {
                    out.push(Defined {
                        node,
                        kind: DefinitionKind::Variable,
                        visible: child.end_byte(),
                        scope: None,
                    });
                }
            }
        }
    }
    out
}

/// The innermost of `scopes` (sorted by start, outer first) that holds
/// `node`, but one whose name `node` is: a function's name belongs to the
/// scope around the function.
fn scope_of(scopes: &[Range<usize>], node: Node) -> Option<Range<usize>> {
    let names_scope = |s: &Range<usize>| {
        node.parent().is_some_and(|p| {
            p.byte_range() == *s
                && p.child_by_field_name("name")
                    .is_some_and(|n| n.id() == node.id())
        })
    };
    (scopes.iter())
        .filter(|s| s.start <= node.start_byte() && node.end_byte() <= s.end && !names_scope(s))
        .max_by_key(|s| s.start)
        .cloned()
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
    /// Its name when it's a plain one, which a module or a namespace
    /// imported may be.
    pub name: Option<String>,
}

/// The fields a member access holds its receiver in.
const RECEIVERS: &[&str] = &["value", "operand", "object", "argument", "path", "scope"];

/// The receiver of the member `at` (its bytes in `source`) of `language`,
/// parsed as `tree`; `None` when it's no member. A variable's type is
/// read where `query`, the language's locals query, finds it defined.
pub(super) fn receiver(
    query: Option<&LocalsQuery>,
    language: &str,
    tree: &Tree,
    source: &[u8],
    at: Range<usize>,
) -> Option<Receiver> {
    let node = tree
        .root_node()
        .descendant_for_byte_range(at.start, at.end)?;
    let parent = node.parent()?;
    let field = field_of(node, parent)?;
    let member = matches!(field, "field" | "property" | "attribute")
        || (field == "name"
            && matches!(
                parent.kind(),
                "scoped_identifier"
                    | "scoped_type_identifier"
                    | "qualified_identifier"
                    | "method_invocation"
                    | "field_access"
            ));
    if node.byte_range() != at || !member {
        return None;
    }
    let object = RECEIVERS
        .iter()
        .find_map(|f| parent.child_by_field_name(f))?;
    let reading = Reading {
        query,
        language,
        tree,
        source,
    };
    let name = matches!(object.kind(), "identifier" | "namespace_identifier")
        .then(|| reading.text(object).to_owned());
    Some(Receiver {
        ty: reading.type_of(object, 0),
        name,
    })
}

/// Kinds of loop, whose bindings have no type read from their value.
const LOOPS: &[&str] = &[
    "for_expression",
    "for_statement",
    "for_in_statement",
    "for_in_clause",
    "range_clause",
    "enhanced_for_statement",
];

/// How deep a variable's type is followed: `let a = b;` reads `b`'s.
const TYPE_DEPTH: usize = 4;

struct Reading<'a> {
    query: Option<&'a LocalsQuery>,
    language: &'a str,
    tree: &'a Tree,
    source: &'a [u8],
}

impl Reading<'_> {
    fn text(&self, node: Node) -> &str {
        std::str::from_utf8(&self.source[node.byte_range()]).unwrap_or_default()
    }

    /// The type expression `node` has, as far as the file tells.
    fn type_of(&self, node: Node, depth: usize) -> Option<String> {
        if depth > TYPE_DEPTH {
            return None;
        }
        let text = self.text(node);
        match node.kind() {
            "self" | "this" => self.enclosing(node),
            "identifier" | "type_identifier" | "namespace_identifier" => {
                if matches!(text, "self" | "this" | "Self") {
                    return self.enclosing(node);
                }
                if let Some(query) = self.query
                    && let Some(local) = resolve(
                        query,
                        self.language,
                        self.tree,
                        self.source,
                        node.byte_range(),
                    )
                {
                    let defined = (self.tree.root_node())
                        .descendant_for_byte_range(local.name.start, local.name.end)?;
                    return self.declared(defined, depth);
                }
                // A name no local has is a type's when it's capitalized.
                text.starts_with(|c: char| c.is_ascii_uppercase())
                    .then(|| text.to_owned())
            }
            "parenthesized_expression"
            | "reference_expression"
            | "unary_expression"
            | "pointer_expression"
            | "await_expression"
            | "non_null_expression" => self.type_of(node.named_child(0)?, depth + 1),
            "as_expression" | "satisfies_expression" | "type_assertion" => (node
                .child_by_field_name("type")
                .or_else(|| node.named_child(1)))
            .and_then(|t| self.type_name(t)),
            // `T::new()`, Python's `T()` and `m.T()`: a capitalized name
            // called is a constructor.
            "call_expression" | "call" => {
                let function = node.child_by_field_name("function")?;
                let called = match function.kind() {
                    "scoped_identifier" => function.child_by_field_name("path")?,
                    "attribute" => function.child_by_field_name("attribute")?,
                    "member_expression" => function.child_by_field_name("property")?,
                    _ => function,
                };
                let name = self.type_name(called)?;
                name.starts_with(|c: char| c.is_ascii_uppercase())
                    .then_some(name)
            }
            "new_expression" => self.type_name(node.child_by_field_name("constructor")?),
            "object_creation_expression" | "composite_literal" => {
                self.type_name(node.child_by_field_name("type")?)
            }
            "struct_expression" => self.type_name(node.child_by_field_name("name")?),
            _ => None,
        }
    }

    /// The type of the variable or the parameter whose name is `defined`:
    /// the one declared, else that of the value it's given.
    fn declared(&self, defined: Node, depth: usize) -> Option<String> {
        let up = || {
            std::iter::successors(defined.parent(), |p| {
                (p.child_by_field_name("body").is_none())
                    .then(|| p.parent())
                    .flatten()
            })
            .take(4)
        };
        let within =
            |n: Node| n.start_byte() <= defined.start_byte() && defined.end_byte() <= n.end_byte();
        if let Some(ty) = up()
            .filter_map(|p| p.child_by_field_name("type"))
            .find(|t| !within(*t))
        {
            return self.type_name(ty);
        }
        // A loop's binding has an element's type, not its collection's.
        let (given, value) = up()
            .take_while(|p| !LOOPS.contains(&p.kind()))
            .find_map(|p| {
                let value = ["value", "right"]
                    .iter()
                    .find_map(|f| p.child_by_field_name(f))?;
                Some((p, value))
            })
            .filter(|(_, v)| !within(*v))?;
        // Go's `a, b := x, y` gives each name the value in its place.
        let value = match value.kind() {
            "expression_list" => {
                let names = given.child_by_field_name("left")?;
                let place = (names.named_children(&mut names.walk())).position(within)?;
                value.named_child(u32::try_from(place).ok()?)?
            }
            _ => value,
        };
        self.type_of(value, depth + 1)
    }

    /// The name of the type `node`, a type expression, is: past its
    /// pointers and references, its package or module, and its arguments.
    fn type_name(&self, node: Node) -> Option<String> {
        let mut node = node;
        for _ in 0..8 {
            if matches!(
                node.kind(),
                "type_identifier" | "identifier" | "primitive_type" | "namespace_identifier"
            ) {
                return Some(self.text(node).to_owned());
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

    /// The class or the impl around `node`, as `self` or `this` names it.
    fn enclosing(&self, node: Node) -> Option<String> {
        let mut at = node.parent();
        while let Some(p) = at {
            match p.kind() {
                "impl_item" => return self.type_name(p.child_by_field_name("type")?),
                "class_declaration"
                | "class"
                | "abstract_class_declaration"
                | "class_definition"
                | "class_specifier"
                | "struct_specifier" => {
                    return Some(self.text(p.child_by_field_name("name")?).to_owned());
                }
                // C++'s `int Point::area() { … }`.
                "function_definition"
                    if let Some(declared) = (p.child_by_field_name("declarator"))
                        .and_then(|d| d.child_by_field_name("declarator"))
                        && declared.kind() == "qualified_identifier" =>
                {
                    return self.type_name(declared.child_by_field_name("scope")?);
                }
                _ => {}
            }
            at = p.parent();
        }
        None
    }
}

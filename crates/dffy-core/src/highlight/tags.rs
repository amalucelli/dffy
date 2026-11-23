//! What a file defines, for go-to-definition: each definition's name, kind
//! and lines, found once per blob and cached by blob id, so a revision's
//! definitions are its blobs' and two revisions share what they share.
//!
//! A language's tags query says what counts. Each pattern captures the
//! definition as `@definition.<kind>` (a [`DefinitionKind::name`]), what
//! it's called as `@name`, for a re-export that renames what it exports
//! the name it had as `@original`, and for a member (a method, a field, an enum
//! member, an interface's or a trait's member) the type it's in as
//! `@container`. Where the name sits at the end of a chain of C
//! declarators (pointers, parentheses, arrays, function parameters), the
//! pattern captures the outermost one as `@declarator` instead, and the
//! name is the identifier the chain ends at; a function's chain must pass
//! a function declarator, and a field's mustn't name a function, though
//! one may point to a function. A C++ name qualified by a scope
//! (`Point::area`) has the scope as its container.
//!
//! Names and containers are what code writes to look them up: a Ruby
//! symbol without its `:`, an instance variable without its `@`, a dotted
//! name by its last part.
//!
//! A definition a macro makes (`SYSCALL_DEFINE3(openat, …)` makes
//! `sys_openat`) is read from the macro's arguments instead, as the grammar
//! parses the same macro in many shapes. What the query finds under a
//! definition macro's own name is such a misparse, and dropped.
//!
//! To cover a language:
//! 1. Write `queries/<language>/tags.scm` with the captures above, leaving
//!    out what only refers to a definition defined elsewhere (prototypes,
//!    declarations, overload signatures) but keeping the members of
//!    interfaces, traits and protocols, which nothing else defines. Every
//!    pattern starts with a named node, and its predicates apply to the
//!    whole pattern, alternatives included.
//! 2. List it in the grammar's `tags` in [`super::lang`].
//! 3. Add its [`Rules`] in [`rules`]: the subtrees that hold no definition
//!    worth listing, such as function bodies, its definition macros, the
//!    attribute macros that hide a definition from the grammar or that it
//!    takes for a name, the suffixes a name drops (Ruby's `?`), and
//!    whether constructors count. All but the macro table are part of the
//!    cache key.
//! 4. Add a fixture, `tags/fixtures/fixture.<extension>`, to `FIXTURES` in
//!    `tags/tests.rs` and a test of what it yields. A change in what the
//!    fixtures yield fails `tags_version_follows_what_extraction_finds`
//!    until [`TAGS_VERSION`] is bumped; a new language's fixture only
//!    changes the digest.
//! 5. Give `navigate::names` its syntax and keywords, and `navigate::imports`
//!    its way of naming another file.

use std::ops::Range;
use std::time::Duration;

use tree_sitter::{Node, Query, QueryCursor, QueryMatch, StreamingIterator, Tree};

use super::scope::{kinds_named, roots};
use super::{Language, parse_tree};
use rustc_hash::FxHashSet;

use crate::cache::{self, SymbolCache};
use crate::git::ObjectId;

/// Sides larger than this aren't read for definitions, as they aren't
/// highlighted: past it are mostly generated tables and register headers.
pub const MAX_TAGS_BYTES: usize = super::MAX_HIGHLIGHT_BYTES;

/// A parse for definitions that takes longer than this is abandoned. It
/// runs in the background, so it gets longer than a highlight's.
pub(crate) const TAGS_PARSE_BUDGET: Duration = Duration::from_secs(2);

/// The version of what cached definitions mean, part of their key: bump it
/// when the same query finds something else (see the module's steps) or
/// the encoding changes. A query's own changes change the key by
/// themselves.
const TAGS_VERSION: u8 = 7;

/// What a definition defines. The numbers are the cached form's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DefinitionKind {
    /// A function with its body.
    Function = 0,
    /// A function in a type: an impl, a class, or for Go the receiver's
    /// type; in an interface, a trait or a protocol, without a body too.
    Method = 1,
    /// A preprocessor macro, a Rust `macro_rules!` or an Elixir `defmacro`.
    Macro = 2,
    /// A typedef or a type alias.
    Type = 3,
    /// A struct with its fields.
    Struct = 4,
    /// A union with its fields.
    Union = 5,
    /// An enum with its variants.
    Enum = 6,
    /// An enum's constant or case.
    Enumerator = 7,
    /// A Rust, Scala or PHP trait.
    Trait = 8,
    /// A constant with its value: a Rust or Go `const`, a module-level
    /// `const` in JavaScript.
    Constant = 9,
    /// A Rust `static` with its value.
    Static = 10,
    /// A variable at file or module scope, or one a C definition macro
    /// makes (`DEFINE_MUTEX(lock)`).
    Variable = 11,
    /// A module or namespace with its body, or a Rust `mod` in a file of
    /// its own.
    Module = 12,
    /// A class, or an object that is one (Kotlin's and Scala's `object`).
    Class = 13,
    /// An interface or protocol.
    Interface = 14,
    /// A field or property of a type, an interface's or a protocol's
    /// included.
    Field = 15,
    /// A name a module exports from another (Rust's `pub use`,
    /// JavaScript's `export { x } from`, a Python package's `__init__.py`
    /// importing it): where it's defined is in the other module.
    Reexport = 16,
    /// A variable a function defines, found in the asking file at the
    /// use; never in the index.
    Local = 17,
    /// A function's parameter, found as [`DefinitionKind::Local`] is.
    Parameter = 18,
    /// The import that brings a name in, in the asking file: one from
    /// outside the repository, or a Go package's; never in the index.
    Import = 19,
}

impl DefinitionKind {
    const ALL: [Self; 20] = [
        Self::Function,
        Self::Method,
        Self::Macro,
        Self::Type,
        Self::Struct,
        Self::Union,
        Self::Enum,
        Self::Enumerator,
        Self::Trait,
        Self::Constant,
        Self::Static,
        Self::Variable,
        Self::Module,
        Self::Class,
        Self::Interface,
        Self::Field,
        Self::Reexport,
        Self::Local,
        Self::Parameter,
        Self::Import,
    ];

    /// Its word, as a tags query captures it after `definition.`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Method => "method",
            Self::Macro => "macro",
            Self::Type => "type",
            Self::Struct => "struct",
            Self::Union => "union",
            Self::Enum => "enum",
            Self::Enumerator => "enumerator",
            Self::Trait => "trait",
            Self::Constant => "constant",
            Self::Static => "static",
            Self::Variable => "variable",
            Self::Module => "module",
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Field => "field",
            Self::Reexport => "reexport",
            Self::Local => "local",
            Self::Parameter => "parameter",
            Self::Import => "import",
        }
    }

    fn from_u8(n: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|k| *k as u8 == n)
    }

    /// The kind whose [`DefinitionKind::name`] is `name`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.name() == name)
    }
}

/// A language's tags query that doesn't compile against its grammar, or
/// lacks what extraction needs. Stated to the user, as the language then
/// has no definitions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the {language} definitions query doesn't compile: {reason}")]
pub struct TagsError {
    language: String,
    reason: String,
}

/// Why a file of a language with a tags query has no definitions listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// Larger than [`MAX_TAGS_BYTES`].
    TooLarge,
    /// Its parse took longer than the time it's given.
    TimedOut,
}

/// What a language's walk skips and which macros make definitions.
struct Rules {
    /// Kinds whose subtrees hold no definition worth listing, such as
    /// function bodies, whose locals no other file can name.
    prune: &'static [&'static str],
    /// Kinds a definition macro's name is parsed as.
    identifiers: &'static [&'static str],
    /// Where a macro's name makes no definition: under a node of the kind,
    /// or with a field, only under that field of it. A macro being
    /// defined, a struct's fields (where it declares a field) and a
    /// preprocessor condition.
    not_under: &'static [(&'static str, Option<&'static str>)],
    /// The kind of a body the grammar parses apart from its macro's
    /// parentheses, as it does `SYSCALL_DEFINE3(…) { … }`.
    body: &'static str,
    /// The kind of a declarator a `@declarator` chain must pass for a
    /// function.
    function_declarator: &'static str,
    macros: fn(&str) -> Option<Macro>,
    /// Attribute macros taking arguments that the grammar can't parse
    /// where code writes them, losing the definition around them: each use
    /// is read as spaces.
    attributes: &'static [&'static str],
    /// What a name may end with that code naming it leaves off before a
    /// lookup sees it: Ruby's `empty?`, `save!` and `name=` are `empty`,
    /// `save` and `name`.
    name_suffixes: &'static [char],
    /// A method named after its type is a constructor, left out as the type
    /// is found instead (C++'s `Point::Point()`).
    constructors: Constructors,
    /// Attribute macros the grammar takes for a definition's name where
    /// they follow it (`struct { … } __packed;`): never a definition.
    attribute_names: &'static [&'static str],
}

/// Whether [`Rules::constructors`] are definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Constructors {
    Kept,
    Dropped,
}

impl Rules {
    /// What the rules are, for the cache key: everything but the
    /// definition macros, whose table is code (a change to it bumps
    /// [`TAGS_VERSION`]).
    fn identity(&self) -> String {
        format!(
            "{:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?} {:?}",
            self.prune,
            self.identifiers,
            self.not_under,
            self.body,
            self.function_declarator,
            self.attributes,
            self.name_suffixes,
            self.constructors,
            self.attribute_names,
        )
    }
}

/// [`Rules::identity`] of `language`'s rules; empty for one without.
pub(super) fn rules_identity(language: &str) -> String {
    rules(language).map(Rules::identity).unwrap_or_default()
}

/// A definition macro: the symbol it makes is `prefix`, its argument
/// `arg` (0-based) and `suffix`.
struct Macro {
    arg: usize,
    prefix: &'static str,
    suffix: &'static str,
    kind: DefinitionKind,
}

fn rules(language: &str) -> Option<&'static Rules> {
    match language {
        "c" => Some(&C),
        "rust" => Some(&RUST),
        "javascript" | "typescript" | "tsx" => Some(&JAVASCRIPT),
        "python" => Some(&PYTHON),
        "go" => Some(&GO),
        "java" => Some(&JAVA),
        "cpp" => Some(&CPP),
        "csharp" => Some(&CSHARP),
        "kotlin" => Some(&KOTLIN),
        "swift" => Some(&SWIFT),
        "ruby" => Some(&RUBY),
        "php" => Some(&PHP),
        "scala" => Some(&SCALA),
        "zig" => Some(&ZIG),
        "lua" => Some(&LUA),
        "elixir" => Some(&ELIXIR),
        "bash" => Some(&BASH),
        _ => None,
    }
}

/// Rules for a language with no definition macros: only what the walk
/// skips.
const fn pruning(prune: &'static [&'static str]) -> Rules {
    Rules {
        prune,
        identifiers: &[],
        not_under: &[],
        body: "",
        function_declarator: "",
        macros: |_| None,
        attributes: &[],
        name_suffixes: &[],
        constructors: Constructors::Kept,
        attribute_names: &[],
    }
}

/// Function bodies are skipped by skipping the functions, as a namespace's
/// body is a block too. Class bodies, enum bodies and module-scope
/// variables are read by patterns starting at the class, the enum and the
/// file.
static JAVASCRIPT: Rules = pruning(&[
    "function_declaration",
    "generator_function_declaration",
    "function_expression",
    "generator_function",
    "arrow_function",
    "method_definition",
    "class",
    "class_body",
    "interface_body",
    "object_type",
    "enum_body",
    "lexical_declaration",
    "variable_declaration",
    "type_alias_declaration",
    "ambient_declaration",
    "call_expression",
    "object",
    "array",
    "template_string",
    "if_statement",
    "for_statement",
    "for_in_statement",
    "while_statement",
    "do_statement",
    "try_statement",
    "switch_statement",
    "jsx_element",
    "jsx_self_closing_element",
]);

/// A class's body is a block like a function's: the functions are skipped
/// instead. Assignments are read by patterns starting at the module and
/// the class.
static PYTHON: Rules = pruning(&[
    "function_definition",
    "lambda",
    "expression_statement",
    "decorator",
    "call",
]);

static GO: Rules = pruning(&[
    "block",
    "field_declaration_list",
    "interface_type",
    "parameter_list",
    "expression_list",
    "func_literal",
    "composite_literal",
    "argument_list",
]);

/// C's walk without its definition macros, which are the kernel's: class
/// bodies are entered for the classes nested in them.
static CPP: Rules = Rules {
    prune: &[
        "compound_statement",
        "initializer_list",
        "argument_list",
        "parameter_list",
        "field_initializer_list",
        "template_parameter_list",
        "lambda_expression",
    ],
    function_declarator: "function_declarator",
    constructors: Constructors::Dropped,
    ..pruning(&[])
};

/// Members are read by patterns starting at their type; the walk only
/// enters type bodies for the types nested in them.
static JAVA: Rules = pruning(&[
    "method_declaration",
    "constructor_declaration",
    "compact_constructor_declaration",
    "field_declaration",
    "constant_declaration",
    "enum_constant",
    "static_initializer",
    "block",
    "modifiers",
    "formal_parameters",
]);

static CSHARP: Rules = pruning(&[
    "method_declaration",
    "constructor_declaration",
    "destructor_declaration",
    "operator_declaration",
    "conversion_operator_declaration",
    "indexer_declaration",
    "property_declaration",
    "field_declaration",
    "event_field_declaration",
    "event_declaration",
    "enum_member_declaration_list",
    "block",
    "parameter_list",
    "attribute_list",
]);

static KOTLIN: Rules = pruning(&[
    "function_body",
    "function_value_parameters",
    "property_declaration",
    "primary_constructor",
    "secondary_constructor",
    "anonymous_initializer",
    "enum_class_body",
    "modifiers",
    "call_expression",
    "lambda_literal",
]);

static RUBY: Rules = Rules {
    name_suffixes: &['?', '!', '='],
    ..pruning(&[
        "method",
        "singleton_method",
        "argument_list",
        "hash",
        "array",
        "lambda",
    ])
};

static PHP: Rules = pruning(&[
    "compound_statement",
    "formal_parameters",
    "arguments",
    "property_declaration",
    "attribute_list",
]);

static SCALA: Rules = pruning(&[
    "function_definition",
    "val_definition",
    "var_definition",
    "given_definition",
    "call_expression",
    "block",
]);

static ZIG: Rules = pruning(&[
    "block",
    "parameters",
    "struct_initializer",
    "call_expression",
    "builtin_function",
]);

static LUA: Rules = pruning(&[
    "block",
    "arguments",
    "table_constructor",
    "function_definition",
]);

/// Definitions are calls, so a function's body is a `do` block like a
/// module's, and the walk goes through it; what a pattern can't start at
/// is skipped.
static ELIXIR: Rules = Rules {
    name_suffixes: &['?', '!'],
    ..pruning(&[
        "arguments",
        "anonymous_function",
        "list",
        "map",
        "keywords",
        "string",
        "sigil",
    ])
};

static BASH: Rules = pruning(&[
    "compound_statement",
    "command",
    "command_substitution",
    "heredoc_body",
    "string",
]);

static SWIFT: Rules = pruning(&[
    "function_body",
    "property_declaration",
    "init_declaration",
    "deinit_declaration",
    "subscript_declaration",
    "protocol_body",
    "modifiers",
    "attribute",
    "call_expression",
    "lambda_literal",
]);

static C: Rules = Rules {
    prune: &[
        "compound_statement",
        "initializer_list",
        "argument_list",
        "parameter_list",
    ],
    identifiers: &["identifier", "type_identifier"],
    not_under: &[
        ("preproc_def", None),
        ("preproc_function_def", None),
        ("field_declaration_list", None),
        ("preproc_if", Some("condition")),
        ("preproc_elif", Some("condition")),
        ("preproc_ifdef", Some("name")),
        ("preproc_elifdef", Some("name")),
    ],
    body: "compound_statement",
    function_declarator: "function_declarator",
    macros: c_macro,
    // The kernel's, written between a function's storage class and its
    // type (`static inline __alloc_size(1, 2) void *kmalloc_array(…)`).
    attributes: &["__alloc_size", "__realloc_size", "__printf", "__scanf"],
    name_suffixes: &[],
    constructors: Constructors::Kept,
    // The kernel's, the most frequent that follow a member's body.
    attribute_names: &[
        "__packed",
        "__aligned",
        "__bpf_aligned",
        "__randomize_layout",
        "__no_randomize_layout",
        "____cacheline_aligned",
        "____cacheline_aligned_in_smp",
        "____cacheline_internodealigned_in_smp",
        "__cacheline_aligned",
        "__cacheline_aligned_in_smp",
    ],
};

static RUST: Rules = Rules {
    prune: &[
        "block",
        "token_tree",
        "field_declaration_list",
        "ordered_field_declaration_list",
        "enum_variant_list",
        "parameters",
        "arguments",
    ],
    body: "block",
    ..pruning(&[])
};

/// The Linux kernel's definition macros, from the names most used at file
/// scope. Subsystems wrap `DEFINE_EVENT` in their own `DEFINE_*_EVENT(name)`,
/// which make `trace_<name>` too; any other `DEFINE_*` is taken to define
/// its first argument, except those that make guards and cleanup classes
/// under generated names.
fn c_macro(name: &str) -> Option<Macro> {
    use DefinitionKind::{Function, Variable};
    let numbered = |prefix| {
        name.strip_prefix(prefix)
            .is_some_and(|n| matches!(n.as_bytes(), [b'0'..=b'6']))
    };
    let (arg, prefix, suffix, kind) = if numbered("SYSCALL_DEFINE") {
        (0, "sys_", "", Function)
    } else if numbered("COMPAT_SYSCALL_DEFINE") {
        (0, "compat_sys_", "", Function)
    } else if matches!(
        name,
        "TRACE_EVENT_FLAGS" | "TRACE_EVENT_PERF_PERM" | "TRACE_EVENT_NOP" | "DEFINE_CLASS"
    ) || name.starts_with("DEFINE_GUARD")
        || name.starts_with("DEFINE_LOCK_GUARD")
    {
        return None;
    } else if name.starts_with("TRACE_EVENT") {
        (0, "trace_", "", Function)
    } else if name.starts_with("DEFINE_EVENT") {
        (1, "trace_", "", Function)
    } else if name == "DEFINE_FREE" {
        (0, "__free_", "", Function)
    } else if name.starts_with("DEFINE_PER_CPU") {
        (1, "", "", Variable)
    } else if matches!(
        name,
        "DEFINE_SHOW_ATTRIBUTE" | "DEFINE_SHOW_STORE_ATTRIBUTE" | "DEFINE_SEQ_ATTRIBUTE"
    ) {
        (0, "", "_fops", Variable)
    } else if name.starts_with("DEFINE_IDTENTRY") {
        (0, "", "", Function)
    } else if name.starts_with("DEFINE_") && name.ends_with("_EVENT") {
        (0, "trace_", "", Function)
    } else if name.starts_with("DEFINE_") {
        (0, "", "", Variable)
    } else {
        return None;
    };
    Some(Macro {
        arg,
        prefix,
        suffix,
        kind,
    })
}

/// A compiled tags query, with its capture indices and its language's
/// [`Rules`] resolved to kind ids.
pub(super) struct TagsQuery {
    query: Query,
    name: Option<u32>,
    declarator: Option<u32>,
    container: Option<u32>,
    /// A renamed re-export's original name.
    original: Option<u32>,
    /// The kind each capture stands for, by capture index.
    kinds: Vec<Option<DefinitionKind>>,
    pub(super) roots: Option<Vec<bool>>,
    /// By kind id: [`Rules::prune`] and [`Rules::identifiers`].
    prune: Vec<bool>,
    identifiers: Vec<bool>,
    rules: Option<&'static Rules>,
}

impl TagsQuery {
    pub(super) fn new(
        grammar: &tree_sitter::Language,
        language: &str,
        source: &str,
    ) -> Result<Self, TagsError> {
        let error = |reason: String| TagsError {
            language: language.to_owned(),
            reason,
        };
        let query = Query::new(grammar, source).map_err(|e| error(e.to_string()))?;
        let mut kinds = Vec::new();
        for capture in query.capture_names() {
            let kind = match capture.strip_prefix("definition.") {
                Some(word) => Some(
                    DefinitionKind::ALL
                        .into_iter()
                        .find(|k| k.name() == word)
                        .ok_or_else(|| error(format!("no definition kind {word}")))?,
                ),
                None => None,
            };
            kinds.push(kind);
        }
        if !kinds.iter().any(Option::is_some) {
            return Err(error("no @definition capture".into()));
        }
        let name = query.capture_index_for_name("name");
        let declarator = query.capture_index_for_name("declarator");
        if name.is_none() && declarator.is_none() {
            return Err(error("no @name or @declarator capture".into()));
        }
        let rules = rules(language);
        let by_kind = |names: fn(&Rules) -> &'static [&'static str]| {
            kinds_named(grammar, rules.map_or(&[], names))
        };
        Ok(Self {
            name,
            declarator,
            container: query.capture_index_for_name("container"),
            original: query.capture_index_for_name("original"),
            kinds,
            roots: roots(grammar, &query, source),
            prune: by_kind(|r| r.prune),
            identifiers: by_kind(|r| r.identifiers),
            rules,
            query,
        })
    }

    fn may_start(&self, kind: usize) -> bool {
        self.roots
            .as_ref()
            .is_none_or(|roots| roots.get(kind).copied().unwrap_or(false))
    }
}

/// No container.
const NONE: u32 = u32::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Def {
    name: u32,
    container: u32,
    /// A renamed re-export's original name; [`NONE`] for any other.
    original: u32,
    kind: DefinitionKind,
    start: u32,
    end: u32,
}

/// The definitions of one file, by first line. Names are stored once per
/// file, sorted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Definitions {
    names: String,
    /// Where each name ends in `names`.
    ends: Vec<u32>,
    defs: Vec<Def>,
}

/// One of [`Definitions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Definition<'a> {
    /// The symbol, as code names it: a macro's makes its own
    /// (`sys_openat` for `SYSCALL_DEFINE3(openat, …)`).
    pub name: &'a str,
    /// What it defines.
    pub kind: DefinitionKind,
    /// Its first line, 0-based.
    pub start: usize,
    /// Its last line, 0-based: a function's closing brace.
    pub end: usize,
    /// For a member, the type it's in: the class, struct, enum, interface
    /// or table, the type an impl is on or a Go receiver's, or the trait
    /// for what the trait declares; the type's name without its path or
    /// parameters (`Point` for `impl<T> crate::geo::Point<T>`). `None`
    /// when the type has no name to give (`impl<T> Draw for &T`).
    pub container: Option<&'a str>,
    /// For a re-export that renames what it exports (`export { x as y }
    /// from`, `pub use a::x as y`), the name it has where it's defined.
    pub original: Option<&'a str>,
}

impl Definitions {
    /// Every definition, by first line, then in the order they start.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = Definition<'_>> {
        self.defs.iter().map(|d| Definition {
            name: self.name(d.name),
            kind: d.kind,
            start: d.start as usize,
            end: d.end as usize,
            container: (d.container != NONE).then(|| self.name(d.container)),
            original: (d.original != NONE).then(|| self.name(d.original)),
        })
    }

    /// How many definitions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether there are none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }

    /// Every name a definition has or is in, once each, sorted.
    pub(crate) fn names(&self) -> impl Iterator<Item = &str> {
        (0..self.ends.len()).map(|i| self.name(to_u32(i)))
    }

    fn name(&self, i: u32) -> &str {
        let i = i as usize;
        let start = i.checked_sub(1).map_or(0, |p| self.ends[p] as usize);
        &self.names[start..self.ends[i] as usize]
    }

    /// The compact binary form the symbol cache stores. Each name shares
    /// its prefix with the one before it, which in a register header's
    /// thousands of `#define`s is most of it. Never empty: an empty entry
    /// is the cache's mark of a blob [`Skipped::TooLarge`].
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.names.len() / 2 + self.defs.len() * 6 + 8);
        let put = |out: &mut Vec<u8>, n: u32| cache::put_varint(out, u64::from(n));
        put(&mut out, to_u32(self.ends.len()));
        let mut previous = "";
        for i in 0..self.ends.len() {
            let name = self.name(to_u32(i));
            let shared = shared_prefix(previous, name);
            put(&mut out, to_u32(shared));
            put(&mut out, to_u32(name.len() - shared));
            out.extend_from_slice(&name.as_bytes()[shared..]);
            previous = name;
        }
        put(&mut out, to_u32(self.defs.len()));
        let mut line = 0;
        for d in &self.defs {
            put(&mut out, d.name);
            out.push(d.kind as u8);
            put(&mut out, d.start - line);
            put(&mut out, d.end - d.start);
            put(&mut out, d.container.wrapping_add(1));
            // Only a re-export has one, so the others pay nothing for it.
            if d.kind == DefinitionKind::Reexport {
                put(&mut out, d.original.wrapping_add(1));
            }
            line = d.start;
        }
        out
    }

    /// Reads [`Definitions::encode`]'s form; `None` if it's malformed,
    /// including names that would expand past what a file under
    /// [`MAX_TAGS_BYTES`] can define.
    #[must_use]
    pub fn decode(mut input: &[u8]) -> Option<Self> {
        // Names are a file's words with a short prefix or suffix added.
        const MAX_NAMES: usize = 4 * MAX_TAGS_BYTES;
        let get =
            |input: &mut &[u8]| -> Option<u32> { u32::try_from(cache::get_varint(input)?).ok() };
        let count = get(&mut input)? as usize;
        let mut names = Vec::new();
        let mut ends = Vec::with_capacity(count.min(input.len()));
        // Where the name before starts.
        let mut previous = 0;
        for _ in 0..count {
            let shared = get(&mut input)? as usize;
            let len = get(&mut input)? as usize;
            let (suffix, rest) = input.split_at_checked(len)?;
            input = rest;
            if shared > names.len() - previous || names.len() + shared + len > MAX_NAMES {
                return None;
            }
            let start = names.len();
            names.extend_from_within(previous..previous + shared);
            names.extend_from_slice(suffix);
            ends.push(to_u32(names.len()));
            previous = start;
        }
        let names = String::from_utf8(names).ok()?;
        if ends
            .iter()
            .any(|&end| !names.is_char_boundary(end as usize))
        {
            return None;
        }
        let count = get(&mut input)? as usize;
        let mut defs = Vec::with_capacity(count.min(input.len()));
        let mut line = 0u32;
        for _ in 0..count {
            let name = get(&mut input)?;
            let (&kind, rest) = input.split_first()?;
            input = rest;
            let kind = DefinitionKind::from_u8(kind)?;
            let start = line.checked_add(get(&mut input)?)?;
            let end = start.checked_add(get(&mut input)?)?;
            let container = get(&mut input)?.wrapping_sub(1);
            let original = match kind {
                DefinitionKind::Reexport => get(&mut input)?.wrapping_sub(1),
                _ => NONE,
            };
            let known = |i: u32| i == NONE || (i as usize) < ends.len();
            if name as usize >= ends.len() || !known(container) || !known(original) {
                return None;
            }
            defs.push(Def {
                name,
                container,
                original,
                kind,
                start,
                end,
            });
            line = start;
        }
        input.is_empty().then_some(Self { names, ends, defs })
    }
}

fn shared_prefix(a: &str, b: &str) -> usize {
    let n = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    (0..=n).rev().find(|&i| b.is_char_boundary(i)).unwrap_or(0)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// The definitions of `source` in `lang`, from a parse made for them, or
/// why there are none. `None` when the language has no tags query.
///
/// # Errors
///
/// When the language's tags query doesn't compile.
pub fn definitions(
    lang: &Language,
    source: &[u8],
) -> Result<Option<Result<Definitions, Skipped>>, TagsError> {
    definitions_within(lang, source, TAGS_PARSE_BUDGET)
}

/// [`definitions`] with a parse abandoned after `budget`.
pub(crate) fn definitions_within(
    lang: &Language,
    source: &[u8],
    budget: Duration,
) -> Result<Option<Result<Definitions, Skipped>>, TagsError> {
    let Some(query) = lang.tags_query().transpose().map_err(Clone::clone)? else {
        return Ok(None);
    };
    if source.len() > MAX_TAGS_BYTES {
        return Ok(Some(Err(Skipped::TooLarge)));
    }
    let blanked = (query.rules).and_then(|r| blank_attributes(r.attributes, source));
    let source = blanked.as_deref().unwrap_or(source);
    Ok(Some(
        parse_tree(lang, source, budget, None)
            .map(|tree| extract(query, &tree, source))
            .ok_or(Skipped::TimedOut),
    ))
}

/// The longest argument list of an attribute macro that's read as spaces:
/// past it, the parenthesis that seems to close it is likely another's.
const MAX_ATTRIBUTE_ARGS: usize = 64;

/// `source` with each use of the attribute macros `names`, the name and its
/// parenthesized arguments, read as spaces, every line kept where it was;
/// `None` when it has none. A `#define` line is left as it is, so the
/// macro's own definition stays, and so is a use whose arguments are long
/// or hold what no attribute's do (`;`, a brace, a string), as when the
/// name is in a comment and the parenthesis that seems to close it isn't
/// its own.
fn blank_attributes(names: &[&str], source: &[u8]) -> Option<Vec<u8>> {
    let word = |b: &u8| b.is_ascii_alphanumeric() || *b == b'_';
    let mut out: Option<Vec<u8>> = None;
    for name in names {
        for at in memchr::memmem::find_iter(source, name.as_bytes()) {
            let end = at + name.len();
            let line = memchr::memrchr(b'\n', &source[..at]).map_or(0, |i| i + 1);
            if (at > 0 && word(&source[at - 1]))
                || source.get(end).is_some_and(word)
                || source[line..at].trim_ascii_start().starts_with(b"#")
            {
                continue;
            }
            let open = skip_space(source, end);
            if source.get(open) != Some(&b'(') {
                continue;
            }
            let Some((_, close)) = macro_args(source, open) else {
                continue;
            };
            let args = &source[open..=close];
            if args.len() > MAX_ATTRIBUTE_ARGS
                || args.iter().any(|b| matches!(b, b';' | b'{' | b'}' | b'"'))
            {
                continue;
            }
            let out = out.get_or_insert_with(|| source.to_vec());
            for b in &mut out[at..=close] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
    }
    out
}

/// [`definitions`] of blob `id`, whose size from its object header is
/// `size`: from `cache`, or on a miss from what `load` reads, then stored
/// there. `load` must return the blob's own bytes, not a working-tree
/// file's converted ones, as the result is kept under the blob's id. It
/// isn't called on a hit, for a blob too large to read, nor for a language
/// without a tags query, and `None` from it means the blob couldn't be
/// read: nothing is stored. A blob too large to read is stored as such;
/// one whose parse timed out isn't, as the next try may finish.
///
/// # Errors
///
/// When the language's tags query doesn't compile. A hit doesn't compile
/// it.
pub fn blob_definitions(
    lang: &Language,
    id: ObjectId,
    size: u64,
    cache: &SymbolCache,
    load: impl FnOnce() -> Option<Vec<u8>>,
) -> Result<Option<Result<Definitions, Skipped>>, TagsError> {
    if !lang.has_tags() {
        return Ok(None);
    }
    let key = pack_key(lang, id);
    let mut pack = Pack::read(cache, key);
    if let Some(found) = pack.get(&key).and_then(cached_definitions) {
        return Ok(Some(found));
    }
    let found = extract_blob(lang, size, TAGS_PARSE_BUDGET, load)?;
    if let Some((entry, found)) = found {
        if let Some(entry) = entry {
            pack.insert(key, entry);
            pack.write(cache);
        }
        return Ok(Some(found));
    }
    Ok(None)
}

/// A cached entry read back: empty is [`Skipped::TooLarge`]. `None` when
/// it's malformed.
pub(crate) fn cached_definitions(entry: &[u8]) -> Option<Result<Definitions, Skipped>> {
    if entry.is_empty() {
        return Some(Err(Skipped::TooLarge));
    }
    Definitions::decode(entry).map(Ok)
}

/// What [`extract_blob`] found: the entry to cache, and the definitions.
pub(crate) type Extracted = (Option<Vec<u8>>, Result<Definitions, Skipped>);

/// The definitions of a blob of `size` bytes in `lang` (which has a tags
/// query), from what `load` reads unless it's too large to read, with the
/// parse abandoned after `budget`, and the entry to cache for them: `None`
/// for a parse that timed out, which the next try may finish. `None`
/// overall when `load` can't read the blob.
pub(crate) fn extract_blob(
    lang: &Language,
    size: u64,
    budget: Duration,
    load: impl FnOnce() -> Option<Vec<u8>>,
) -> Result<Option<Extracted>, TagsError> {
    lang.tags_query().transpose().map_err(Clone::clone)?;
    let found = if u64::try_from(MAX_TAGS_BYTES).is_ok_and(|max| size > max) {
        Err(Skipped::TooLarge)
    } else {
        let Some(source) = load() else {
            return Ok(None);
        };
        match definitions_within(lang, &source, budget)? {
            Some(found) => found,
            None => return Ok(None),
        }
    };
    let entry = match &found {
        Ok(defs) => Some(defs.encode()),
        Err(Skipped::TooLarge) => Some(Vec::new()),
        Err(Skipped::TimedOut) => None,
    };
    Ok(Some((entry, found)))
}

/// How many packs the symbol cache spreads definitions over, by the first
/// byte of their [`pack_key`]: most blobs' definitions take a few hundred
/// bytes, and an entry of their own would take a disk block. One blob is
/// found by reading one pack, and an index build finishes a pack at a time.
pub(crate) const PACKS: usize = 256;

/// The layout of a pack, part of its cache key.
const PACK_FORMAT: u8 = 1;

/// How much of the symbol cache's budget the packs fill, in hundredths:
/// below the cache's own eviction, which would drop whole packs.
const PACKS_FILL_PERCENT: u64 = 90;

/// The key of blob `id`'s definitions in `lang` within the symbol cache: a
/// digest of [`TAGS_VERSION`], the language's tags identity and the id, so
/// every language shares the packs and an entry made by another grammar or
/// query is never read.
pub(crate) fn pack_key(lang: &Language, id: ObjectId) -> ObjectId {
    let mut key = Vec::with_capacity(1 + 2 * id.as_bytes().len());
    key.push(TAGS_VERSION);
    key.extend_from_slice(lang.tags_identity().as_bytes());
    key.extend_from_slice(id.as_bytes());
    cache::digest(&key)
}

/// The pack a [`pack_key`] is in.
pub(crate) fn pack_of(key: &ObjectId) -> usize {
    usize::from(key.as_bytes()[0])
}

/// One of the symbol cache's [`PACKS`]: entries by [`pack_key`], least
/// recently used first, each an encoded [`Definitions`] or empty for a
/// blob [`Skipped::TooLarge`]. A pack is rewritten whole when something
/// was added, merged with what other processes stored in it since it was
/// read, and with the entries read meanwhile moved to the back; a pack
/// only read isn't rewritten. Past its share of the budget it drops its
/// least recently used entries, and an entry larger than the share isn't
/// kept, so the packs together stay within the budget. Two processes
/// writing one pack at the same moment lose one's additions, which costs
/// only extracting them again.
#[derive(Debug, Default)]
pub(crate) struct Pack {
    index: u8,
    entries: Vec<Stored>,
    added: bool,
}

/// A pack's entry: its key, the encoded definitions, and whether it was
/// read or added since the pack was read.
type Stored = (ObjectId, Box<[u8]>, bool);

impl Pack {
    /// The pack holding `key`, as stored in `cache`; empty when it isn't.
    pub(crate) fn read(cache: &SymbolCache, key: ObjectId) -> Self {
        Self::read_index(cache, pack_of(&key))
    }

    /// Pack number `index`, as stored in `cache`; empty when it isn't.
    pub(crate) fn read_index(cache: &SymbolCache, index: usize) -> Self {
        let index = u8::try_from(index).unwrap_or(u8::MAX);
        Self {
            index,
            entries: Self::stored(cache, index),
            added: false,
        }
    }

    fn stored(cache: &SymbolCache, index: u8) -> Vec<Stored> {
        (cache.as_cache())
            .get(cache::Kind::Symbols, &[PACK_FORMAT, index])
            .and_then(|bytes| Self::decode(&bytes))
            .unwrap_or_default()
    }

    fn decode(mut input: &[u8]) -> Option<Vec<Stored>> {
        let mut entries = Vec::new();
        while !input.is_empty() {
            let (key, rest) = input.split_at_checked(20)?;
            input = rest;
            let len = usize::try_from(cache::get_varint(&mut input)?).ok()?;
            let (entry, rest) = input.split_at_checked(len)?;
            input = rest;
            entries.push((ObjectId::try_from(key).ok()?, entry.into(), false));
        }
        Some(entries)
    }

    /// The entry stored under `key`, marked as used.
    pub(crate) fn get(&mut self, key: &ObjectId) -> Option<&[u8]> {
        let e = self.entries.iter_mut().rfind(|(k, ..)| k == key)?;
        e.2 = true;
        Some(&e.1)
    }

    /// Adds `entry` under `key`, as the newest.
    pub(crate) fn insert(&mut self, key: ObjectId, entry: Vec<u8>) {
        self.entries.retain(|(k, ..)| *k != key);
        self.entries.push((key, entry.into_boxed_slice(), true));
        self.added = true;
    }

    /// The bytes this pack may hold. Entries' sizes vary, a large header's
    /// by far, so of the packs' part of the budget a tenth is a pool: each
    /// pack has an even share of the rest, and takes up to as much again
    /// from what of the pool the others haven't, so the packs together
    /// stay within their part while one with large entries keeps them. A
    /// disk block of each share is slack for the cache's header and
    /// rounding, short of half the share for a budget too small for it.
    fn share(&self, cache: &SymbolCache) -> u64 {
        let budget = cache.budget() / 100 * PACKS_FILL_PERCENT;
        let pool = budget / 10;
        let even = (budget - pool) / PACKS as u64;
        let even = even - (even / 2).min(4096);
        let borrowed: u64 =
            (cache.sizes(cache::Kind::Symbols, &[PACK_FORMAT, self.index])).map_or(pool, |sizes| {
                (sizes.iter())
                    .filter(|&&(_, own)| !own)
                    .map(|&(size, _)| size.saturating_sub(even))
                    .sum()
            });
        even + even.min(pool.saturating_sub(borrowed))
    }

    /// Stores the pack in `cache` if anything was added: the entries not
    /// used since it was read, then those stored by others meanwhile, then
    /// the used ones, without the least recently used past its share of
    /// the budget.
    pub(crate) fn write(&mut self, cache: &SymbolCache) {
        if !self.added {
            return;
        }
        // As written below: the key, the length's varint and the entry.
        let size = |e: &Stored| {
            let len = e.1.len() as u64;
            20 + u64::from(len.max(1).ilog2() / 7 + 1) + len
        };
        let ours: FxHashSet<ObjectId> = self.entries.iter().map(|e| e.0).collect();
        let theirs = Self::stored(cache, self.index)
            .into_iter()
            .filter(|e| !ours.contains(&e.0));
        let (used, unused): (Vec<_>, Vec<_>) = std::mem::take(&mut self.entries)
            .into_iter()
            .partition(|e| e.2);
        let mut entries: Vec<_> = unused.into_iter().chain(theirs).chain(used).collect();
        let share = self.share(cache);
        entries.retain(|e| size(e) <= share);
        let mut total: u64 = entries.iter().map(size).sum();
        let drop = (entries.iter())
            .take_while(|e| {
                let over = total > share;
                if over {
                    total -= size(e);
                }
                over
            })
            .count();
        entries.drain(..drop);
        let mut out = Vec::new();
        for (key, entry, _) in &entries {
            out.extend_from_slice(key.as_bytes());
            cache::put_varint(&mut out, entry.len() as u64);
            out.extend_from_slice(entry);
        }
        (cache.as_cache()).put(cache::Kind::Symbols, &[PACK_FORMAT, self.index], &out);
        self.entries = entries;
        for e in &mut self.entries {
            e.2 = false;
        }
        self.added = false;
    }
}

/// A definition as found, before names are shared.
struct Found {
    /// The definition's bytes, and the pattern that found it: of two
    /// finding the same name in the same bytes, the earlier pattern wins.
    bytes: (u32, u32),
    pattern: usize,
    name: String,
    container: Option<String>,
    original: Option<String>,
    kind: DefinitionKind,
    lines: (u32, u32),
}

/// The definitions `query` finds in `tree`, parsed from `source`.
///
/// The walk skips the language's [`Rules::prune`] subtrees, and runs the
/// query only on nodes a pattern can start at: most of a C file is
/// function bodies, which hold no definition another file can name.
fn extract(query: &TagsQuery, tree: &Tree, source: &[u8]) -> Definitions {
    let mut found = Vec::new();
    let mut braces = Braces::default();
    let mut cursor = QueryCursor::new();
    cursor.set_max_start_depth(Some(0));
    let mut walk = tree.walk();
    'walk: loop {
        let node = walk.node();
        // An error node's kind id is past the grammar's kinds.
        let kind = usize::from(node.kind_id());
        if query.may_start(kind) {
            let mut matches = cursor.matches(&query.query, node, source);
            while let Some(m) = matches.next() {
                found.extend(definition_of(query, m, source));
            }
        }
        if let Some(rules) = query.rules
            && query.identifiers.get(kind).copied().unwrap_or(false)
            && let Some(f) = macro_definition(rules, node, tree.root_node(), source, &mut braces)
        {
            found.push(f);
        }
        if !query.prune.get(kind).copied().unwrap_or(false) && walk.goto_first_child() {
            continue;
        }
        while !walk.goto_next_sibling() {
            if !walk.goto_parent() {
                break 'walk;
            }
        }
    }
    found.sort_by(|a, b| {
        (a.lines.0, a.bytes.0, &a.name, a.bytes.1, a.pattern)
            .cmp(&(b.lines.0, b.bytes.0, &b.name, b.bytes.1, b.pattern))
    });
    found.dedup_by(|b, a| a.bytes == b.bytes && a.name == b.name);
    let mut names: Vec<&str> = (found.iter())
        .flat_map(|f| {
            std::iter::once(f.name.as_str())
                .chain(f.container.as_deref())
                .chain(f.original.as_deref())
        })
        .collect();
    names.sort_unstable();
    names.dedup();
    let id = |name: &str| to_u32(names.binary_search(&name).unwrap_or_default());
    let mut defs = Definitions::default();
    for f in &found {
        defs.defs.push(Def {
            name: id(&f.name),
            container: f.container.as_deref().map_or(NONE, id),
            original: f.original.as_deref().map_or(NONE, id),
            kind: f.kind,
            start: f.lines.0,
            end: f.lines.1,
        });
    }
    for name in names {
        defs.names.push_str(name);
        defs.ends.push(to_u32(defs.names.len()));
    }
    defs
}

/// The definition match `m` of `query` captures in `source`, if it names
/// one.
fn definition_of(query: &TagsQuery, m: &QueryMatch, source: &[u8]) -> Option<Found> {
    let text = |node: Node| String::from_utf8_lossy(&source[node.byte_range()]).into_owned();
    let (mut definition, mut name, mut container, mut scope) = (None, None, None, None);
    let mut original = None;
    for c in m.captures() {
        if Some(c.index) == query.name {
            name = Some(c.node);
        } else if Some(c.index) == query.declarator {
            let is =
                |kind| (m.captures().iter()).any(|d| query.kinds[d.index as usize] == Some(kind));
            let chain = if is(DefinitionKind::Function) {
                Chain::Function
            } else if is(DefinitionKind::Field) {
                Chain::Field
            } else {
                Chain::Any
            };
            let declared = (query.rules).and_then(|r| declared_name(c.node, r, chain));
            name = declared.map(|(name, _)| name);
            scope = declared.and_then(|(_, scope)| scope);
        } else if Some(c.index) == query.container {
            container = Some(c.node);
        } else if Some(c.index) == query.original {
            original = Some(c.node);
        } else if let Some(kind) = query.kinds[c.index as usize] {
            definition = Some((c.node, kind));
        }
    }
    let ((definition, mut kind), name) = (definition?, name?);
    // A C++ function defined outside its class (`int Point::area()`).
    if container.is_none()
        && let Some(scope) = scope
    {
        container = Some(scope);
        if kind == DefinitionKind::Function {
            kind = DefinitionKind::Method;
        }
    }
    let mut name = named(text(name));
    let container = container.map(|c| named(text(c)));
    let original = original.map(|o| named(text(o))).filter(|o| *o != name);
    if let Some(rules) = query.rules {
        name.truncate(name.trim_end_matches(rules.name_suffixes).len());
        // A function found in a class body is a method its class's pattern
        // finds, a constructor among them, dropped there.
        let in_class =
            || (definition.parent()).is_some_and(|p| p.kind() == "field_declaration_list");
        if (kind != DefinitionKind::Macro && (rules.macros)(&name).is_some())
            || rules.attribute_names.contains(&name.as_str())
            || (rules.constructors == Constructors::Dropped
                && ((kind == DefinitionKind::Method
                    && container.as_deref() == Some(name.as_str()))
                    || (kind == DefinitionKind::Function && in_class())))
        {
            return None;
        }
    }
    if name.is_empty() {
        return None;
    }
    Some(Found {
        bytes: (
            to_u32(definition.start_byte()),
            to_u32(definition.end_byte()),
        ),
        pattern: m.pattern_index,
        name,
        container,
        original,
        kind,
        lines: (
            to_u32(definition.start_position().row),
            last_line(definition),
        ),
    })
}

/// The name code looks `captured` up by: past a Ruby symbol's `:` or an
/// instance variable's `@`, and a dotted name's last part (Elixir's
/// `Geo.Point` is `Point`).
fn named(captured: String) -> String {
    let start = captured.rfind('.').map_or(0, |dot| dot + 1);
    let name = captured[start..].trim_start_matches([':', '@']);
    if name.len() == captured.len() {
        captured
    } else {
        name.to_owned()
    }
}

/// What a chain of declarators must be to name a definition.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chain {
    /// A function's: it passes a function declarator.
    Function,
    /// A field's: no function declarator names the field itself, which
    /// would declare a C++ method (`int area();`), though one may be
    /// behind parentheses for a pointer to a function (`int (*fn)(int);`).
    Field,
    Any,
}

/// The identifier a chain of declarators from `declarator` ends at, if the
/// chain is what `chain` needs, and for a C++ qualified name
/// (`geo::Point::area`) the name of the scope it's in (`Point`). A C++
/// operator or destructor has no name to look up.
fn declared_name<'t>(
    mut declarator: Node<'t>,
    rules: &Rules,
    chain: Chain,
) -> Option<(Node<'t>, Option<Node<'t>>)> {
    let mut function = false;
    let mut scope = None;
    loop {
        match declarator.kind() {
            "identifier" | "type_identifier" | "field_identifier" => {
                return (function || chain != Chain::Function).then_some((declarator, scope));
            }
            "operator_name" | "destructor_name" => return None,
            "qualified_identifier" => {
                scope = (declarator.child_by_field_name("scope"))
                    .map(|s| s.child_by_field_name("name").unwrap_or(s));
                declarator = declarator.child_by_field_name("name")?;
                continue;
            }
            kind if kind == rules.function_declarator => {
                let named = declarator.child_by_field_name("declarator");
                if chain == Chain::Field
                    && named.is_none_or(|n| n.kind() != "parenthesized_declarator")
                {
                    return None;
                }
                function = true;
            }
            _ => {}
        }
        declarator =
            (declarator.child_by_field_name("declarator")).or_else(|| declarator.named_child(0))?;
    }
}

/// The 0-based line `node` ends on. A node ending at a line's start ends on
/// the line before.
fn last_line(node: Node) -> u32 {
    let (start, end) = (node.start_position(), node.end_position());
    to_u32(if end.column == 0 && end.row > start.row {
        end.row - 1
    } else {
        end.row
    })
}

/// The definition `leaf` makes when it names one of `rules`' macros at file
/// scope and its argument is a plain name. Its lines run from the macro to
/// its closing parenthesis and `;`, or over the body after it when one
/// follows.
fn macro_definition(
    rules: &Rules,
    leaf: Node,
    root: Node,
    source: &[u8],
    braces: &mut Braces,
) -> Option<Found> {
    let word = std::str::from_utf8(&source[leaf.byte_range()]).ok()?;
    let m = (rules.macros)(word)?;
    let open = skip_space(source, leaf.end_byte());
    if source.get(open) != Some(&b'(') {
        return None;
    }
    let (args, close) = macro_args(source, open)?;
    let arg = plain_name(&source[args.get(m.arg)?.clone()])?;
    let (mut child, mut above, mut error) = (leaf, leaf.parent(), None);
    while let Some(node) = above {
        let excluded = rules.not_under.iter().any(|&(kind, field)| {
            node.kind() == kind && field.is_none_or(|f| node.child_by_field_name(f) == Some(child))
        });
        if excluded {
            return None;
        }
        if node.is_error() {
            error = Some(node);
        }
        (child, above) = (node, node.parent());
    }
    // Inside an error, a function's body may be loose braces around the
    // macro rather than a body node the walk skips.
    if let Some(error) = error
        && braces.depth(source, error.start_byte(), leaf.start_byte()) > 0
    {
        return None;
    }
    let row = leaf.start_position().row;
    let row_at = |byte: usize| {
        to_u32(row + memchr::memchr_iter(b'\n', &source[leaf.start_byte()..byte]).count())
    };
    let after = skip_space(source, close + 1);
    let body = (source.get(after) == Some(&b'{'))
        .then(|| root.descendant_for_byte_range(after, after + 1)?.parent())
        .flatten()
        .filter(|b| b.kind() == rules.body && b.start_byte() == after);
    let (end, last) = match body {
        Some(body) => (body.end_byte(), last_line(body)),
        None if source.get(after) == Some(&b';') => (after + 1, row_at(after)),
        None => (close + 1, row_at(close)),
    };
    Some(Found {
        bytes: (to_u32(leaf.start_byte()), to_u32(end)),
        pattern: usize::MAX,
        name: format!("{}{arg}{}", m.prefix, m.suffix),
        container: None,
        original: None,
        kind: m.kind,
        lines: (to_u32(row), last),
    })
}

/// Where the comment, string or character literal at `i` ends, if one
/// starts there.
fn literal_end(source: &[u8], i: usize) -> Option<usize> {
    match (source.get(i)?, source.get(i + 1)) {
        (b'/', Some(b'*')) => Some(
            memchr::memmem::find(&source[i + 2..], b"*/").map_or(source.len(), |e| i + 2 + e + 2),
        ),
        (b'/', Some(b'/')) => {
            Some(memchr::memchr(b'\n', &source[i..]).map_or(source.len(), |e| i + e))
        }
        (&quote @ (b'"' | b'\''), _) => {
            let mut j = i + 1;
            while let Some(&b) = source.get(j) {
                match b {
                    b'\\' => j += 2,
                    b'\n' => return Some(j),
                    _ if b == quote => return Some(j + 1),
                    _ => j += 1,
                }
            }
            Some(source.len())
        }
        _ => None,
    }
}

/// The first byte from `i` that isn't whitespace or in a comment.
fn skip_space(source: &[u8], mut i: usize) -> usize {
    while let Some(&b) = source.get(i) {
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'/' && matches!(source.get(i + 1), Some(b'*' | b'/')) {
            i = literal_end(source, i).unwrap_or(source.len());
        } else {
            break;
        }
    }
    i
}

/// The arguments of the macro call whose `(` is at `open`, split at
/// top-level commas, and the index of its `)`.
fn macro_args(source: &[u8], open: usize) -> Option<(Vec<Range<usize>>, usize)> {
    let mut args = Vec::new();
    let (mut depth, mut start, mut i) = (0usize, open + 1, open + 1);
    while let Some(&b) = source.get(i) {
        if let Some(end) = literal_end(source, i) {
            i = end;
            continue;
        }
        match b {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => {
                args.push(start..i);
                return Some((args, i));
            }
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                args.push(start..i);
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The name `arg` is when it's one, maybe with an array's size after it.
fn plain_name(arg: &[u8]) -> Option<&str> {
    let start = skip_space(arg, 0);
    let len = (arg[start..].iter())
        .position(|&b| !(b.is_ascii_alphanumeric() || b == b'_'))
        .unwrap_or(arg.len() - start);
    let word = &arg[start..start + len];
    let after = skip_space(arg, start + len);
    if word.first().is_none_or(u8::is_ascii_digit) || !matches!(arg.get(after), None | Some(b'[')) {
        return None;
    }
    std::str::from_utf8(word).ok()
}

/// The brace depth at a place in an error node, counted from the node's
/// start as a compiler sees one configuration: only the first branch of
/// each `#if` chain counts, so an `if (…) {` written once per branch opens
/// one block. Places are asked in order within a node, so the count goes
/// on from the last one.
#[derive(Default)]
struct Braces {
    from: usize,
    at: usize,
    depth: isize,
    /// Per open `#if`, whether a later branch (`#else`, `#elif`) is on.
    later: Vec<bool>,
}

impl Braces {
    fn depth(&mut self, source: &[u8], from: usize, to: usize) -> isize {
        if from != self.from || to < self.at || self.at < from {
            *self = Self {
                from,
                at: from,
                ..Self::default()
            };
        }
        let mut i = self.at;
        while i < to {
            if let Some(end) = literal_end(source, i) {
                i = end;
                continue;
            }
            match source[i] {
                b'#' if matches!(
                    source[..i]
                        .iter()
                        .rev()
                        .find(|b| !matches!(b, b' ' | b'\t')),
                    None | Some(b'\n')
                ) =>
                {
                    let word = &source[skip_space(source, i + 1)..];
                    if word.starts_with(b"if") {
                        self.later.push(false);
                    } else if word.starts_with(b"el") {
                        if let Some(later) = self.later.last_mut() {
                            *later = true;
                        }
                    } else if word.starts_with(b"endif") {
                        self.later.pop();
                    }
                    i = memchr::memchr(b'\n', &source[i..]).map_or(source.len(), |e| i + e);
                    continue;
                }
                b'{' if !self.later.contains(&true) => self.depth += 1,
                b'}' if !self.later.contains(&true) => self.depth -= 1,
                _ => {}
            }
            i += 1;
        }
        self.at = i;
        self.depth
    }
}

#[cfg(test)]
mod tests;

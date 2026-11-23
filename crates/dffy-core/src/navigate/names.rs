//! The names on a line of code, for a frontend to offer to look up: what
//! a reader would call an identifier, past keywords, literals, comments
//! and preprocessor lines' words.

use std::ops::{ControlFlow, Range};

/// C's keywords, C23's included, with the reserved `_Xxx` spellings, but
/// not those a header may define as macros (`bool`, `true`, `false`,
/// `static_assert`).
const C_KEYWORDS: &[&str] = &[
    "_Alignas",
    "_Alignof",
    "_Atomic",
    "_BitInt",
    "_Bool",
    "_Complex",
    "_Decimal128",
    "_Decimal32",
    "_Decimal64",
    "_Generic",
    "_Imaginary",
    "_Noreturn",
    "_Static_assert",
    "_Thread_local",
    "alignas",
    "alignof",
    "auto",
    "break",
    "case",
    "char",
    "const",
    "constexpr",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extern",
    "float",
    "for",
    "goto",
    "if",
    "inline",
    "int",
    "long",
    "nullptr",
    "register",
    "restrict",
    "return",
    "short",
    "signed",
    "sizeof",
    "static",
    "struct",
    "switch",
    "thread_local",
    "typedef",
    "typeof",
    "typeof_unqual",
    "union",
    "unsigned",
    "void",
    "volatile",
    "while",
];

/// Rust's strict and reserved keywords.
const RUST_KEYWORDS: &[&str] = &[
    "Self", "abstract", "as", "async", "await", "become", "box", "break", "const", "continue",
    "crate", "do", "dyn", "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if",
    "impl", "in", "let", "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub",
    "ref", "return", "self", "static", "struct", "super", "trait", "true", "try", "type", "typeof",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield",
];

/// C++'s keywords beyond C's, alternative operator spellings included;
/// not `final` and `override`, which are names elsewhere.
const CPP_KEYWORDS: &[&str] = &[
    "and",
    "and_eq",
    "asm",
    "bitand",
    "bitor",
    "catch",
    "char16_t",
    "char32_t",
    "char8_t",
    "class",
    "co_await",
    "co_return",
    "co_yield",
    "compl",
    "concept",
    "const_cast",
    "consteval",
    "constinit",
    "decltype",
    "delete",
    "dynamic_cast",
    "explicit",
    "export",
    "friend",
    "mutable",
    "namespace",
    "new",
    "noexcept",
    "not",
    "not_eq",
    "operator",
    "or",
    "or_eq",
    "private",
    "protected",
    "public",
    "reinterpret_cast",
    "requires",
    "static_cast",
    "template",
    "this",
    "throw",
    "try",
    "typeid",
    "typename",
    "using",
    "virtual",
    "wchar_t",
    "xor",
    "xor_eq",
];

/// JavaScript's reserved words, strict mode's and modules' included, and
/// its literals. TypeScript reserves no more.
const JAVASCRIPT_KEYWORDS: &[&str] = &[
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

/// Python's keywords, not the soft ones (`match`, `case`, `type`).
const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Go's keywords; its predeclared identifiers can be declared again.
const GO_KEYWORDS: &[&str] = &[
    "break",
    "case",
    "chan",
    "const",
    "continue",
    "default",
    "defer",
    "else",
    "fallthrough",
    "for",
    "func",
    "go",
    "goto",
    "if",
    "import",
    "interface",
    "map",
    "package",
    "range",
    "return",
    "select",
    "struct",
    "switch",
    "type",
    "var",
];

/// Java's reserved keywords and literals, not its contextual ones.
const JAVA_KEYWORDS: &[&str] = &[
    "abstract",
    "assert",
    "boolean",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "class",
    "const",
    "continue",
    "default",
    "do",
    "double",
    "else",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "for",
    "goto",
    "if",
    "implements",
    "import",
    "instanceof",
    "int",
    "interface",
    "long",
    "native",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "short",
    "static",
    "strictfp",
    "super",
    "switch",
    "synchronized",
    "this",
    "throw",
    "throws",
    "transient",
    "true",
    "try",
    "void",
    "volatile",
    "while",
];

/// C#'s reserved keywords, not its contextual ones.
const CSHARP_KEYWORDS: &[&str] = &[
    "abstract",
    "as",
    "base",
    "bool",
    "break",
    "byte",
    "case",
    "catch",
    "char",
    "checked",
    "class",
    "const",
    "continue",
    "decimal",
    "default",
    "delegate",
    "do",
    "double",
    "else",
    "enum",
    "event",
    "explicit",
    "extern",
    "false",
    "finally",
    "fixed",
    "float",
    "for",
    "foreach",
    "goto",
    "if",
    "implicit",
    "in",
    "int",
    "interface",
    "internal",
    "is",
    "lock",
    "long",
    "namespace",
    "new",
    "null",
    "object",
    "operator",
    "out",
    "override",
    "params",
    "private",
    "protected",
    "public",
    "readonly",
    "ref",
    "return",
    "sbyte",
    "sealed",
    "short",
    "sizeof",
    "stackalloc",
    "static",
    "string",
    "struct",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "uint",
    "ulong",
    "unchecked",
    "unsafe",
    "ushort",
    "using",
    "virtual",
    "void",
    "volatile",
    "while",
];

/// Kotlin's hard keywords; its soft keywords and modifiers are names
/// elsewhere.
const KOTLIN_KEYWORDS: &[&str] = &[
    "as",
    "break",
    "class",
    "continue",
    "do",
    "else",
    "false",
    "for",
    "fun",
    "if",
    "in",
    "interface",
    "is",
    "null",
    "object",
    "package",
    "return",
    "super",
    "this",
    "throw",
    "true",
    "try",
    "typealias",
    "typeof",
    "val",
    "var",
    "when",
    "while",
];

/// Swift's keywords in declarations, statements, expressions and types,
/// not its contextual ones.
const SWIFT_KEYWORDS: &[&str] = &[
    "Any",
    "Self",
    "as",
    "associatedtype",
    "break",
    "case",
    "catch",
    "class",
    "continue",
    "default",
    "defer",
    "deinit",
    "do",
    "else",
    "enum",
    "extension",
    "fallthrough",
    "false",
    "fileprivate",
    "for",
    "func",
    "guard",
    "if",
    "import",
    "in",
    "init",
    "inout",
    "internal",
    "is",
    "let",
    "nil",
    "operator",
    "private",
    "protocol",
    "public",
    "repeat",
    "rethrows",
    "return",
    "self",
    "static",
    "struct",
    "subscript",
    "super",
    "switch",
    "throw",
    "throws",
    "true",
    "try",
    "typealias",
    "var",
    "where",
    "while",
];

/// Ruby's reserved words.
const RUBY_KEYWORDS: &[&str] = &[
    "BEGIN",
    "END",
    "__ENCODING__",
    "__FILE__",
    "__LINE__",
    "alias",
    "and",
    "begin",
    "break",
    "case",
    "class",
    "def",
    "defined",
    "do",
    "else",
    "elsif",
    "end",
    "ensure",
    "false",
    "for",
    "if",
    "in",
    "module",
    "next",
    "nil",
    "not",
    "or",
    "redo",
    "rescue",
    "retry",
    "return",
    "self",
    "super",
    "then",
    "true",
    "undef",
    "unless",
    "until",
    "when",
    "while",
    "yield",
];

/// PHP's reserved keywords and the type and class names it reserves.
const PHP_KEYWORDS: &[&str] = &[
    "abstract",
    "and",
    "array",
    "as",
    "bool",
    "break",
    "callable",
    "case",
    "catch",
    "class",
    "clone",
    "const",
    "continue",
    "declare",
    "default",
    "do",
    "echo",
    "else",
    "elseif",
    "empty",
    "enddeclare",
    "endfor",
    "endforeach",
    "endif",
    "endswitch",
    "endwhile",
    "enum",
    "extends",
    "false",
    "final",
    "finally",
    "float",
    "fn",
    "for",
    "foreach",
    "function",
    "global",
    "goto",
    "if",
    "implements",
    "include",
    "include_once",
    "instanceof",
    "insteadof",
    "int",
    "interface",
    "isset",
    "iterable",
    "list",
    "match",
    "mixed",
    "namespace",
    "never",
    "new",
    "null",
    "object",
    "or",
    "parent",
    "print",
    "private",
    "protected",
    "public",
    "readonly",
    "require",
    "require_once",
    "return",
    "self",
    "static",
    "string",
    "switch",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "unset",
    "use",
    "var",
    "void",
    "while",
    "xor",
    "yield",
];

/// Scala's hard keywords, not Scala 3's soft ones.
const SCALA_KEYWORDS: &[&str] = &[
    "abstract",
    "case",
    "catch",
    "class",
    "def",
    "do",
    "else",
    "enum",
    "export",
    "extends",
    "false",
    "final",
    "finally",
    "for",
    "given",
    "if",
    "implicit",
    "import",
    "lazy",
    "match",
    "new",
    "null",
    "object",
    "override",
    "package",
    "private",
    "protected",
    "return",
    "sealed",
    "super",
    "then",
    "this",
    "throw",
    "trait",
    "true",
    "try",
    "type",
    "val",
    "var",
    "while",
    "with",
    "yield",
];

/// Zig's keywords and its primitive types and values.
const ZIG_KEYWORDS: &[&str] = &[
    "addrspace",
    "align",
    "allowzero",
    "and",
    "anyerror",
    "anyframe",
    "anyopaque",
    "anytype",
    "asm",
    "async",
    "await",
    "bool",
    "break",
    "callconv",
    "catch",
    "comptime",
    "comptime_float",
    "comptime_int",
    "const",
    "continue",
    "defer",
    "else",
    "enum",
    "errdefer",
    "error",
    "export",
    "extern",
    "f128",
    "f16",
    "f32",
    "f64",
    "f80",
    "false",
    "fn",
    "for",
    "i128",
    "i16",
    "i32",
    "i64",
    "i8",
    "if",
    "inline",
    "isize",
    "linksection",
    "noalias",
    "noinline",
    "noreturn",
    "nosuspend",
    "null",
    "opaque",
    "or",
    "orelse",
    "packed",
    "pub",
    "resume",
    "return",
    "struct",
    "suspend",
    "switch",
    "test",
    "threadlocal",
    "true",
    "try",
    "type",
    "u128",
    "u16",
    "u32",
    "u64",
    "u8",
    "undefined",
    "union",
    "unreachable",
    "usize",
    "usingnamespace",
    "var",
    "void",
    "volatile",
    "while",
];

/// Lua's keywords.
const LUA_KEYWORDS: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while",
];

/// Elixir's reserved words.
const ELIXIR_KEYWORDS: &[&str] = &[
    "after", "and", "catch", "do", "else", "end", "false", "fn", "in", "nil", "not", "or",
    "rescue", "true", "when",
];

/// Bash's reserved words.
const BASH_KEYWORDS: &[&str] = &[
    "case", "coproc", "do", "done", "elif", "else", "esac", "fi", "for", "function", "if", "in",
    "select", "then", "time", "until", "while",
];

/// What a backtick starts.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Backtick {
    /// Nothing that hides names: a quoted name, a command.
    Code,
    /// A string to the next backtick (Go's raw strings).
    Raw,
    /// A template whose `${…}` parts are code (JavaScript's).
    Template,
}

/// Where a `#` that opens a line comment does.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Hash {
    Anywhere,
    /// At a word's start only (Bash's `$#`, `${#a}`).
    AtWord,
    /// Not before `[`, an attribute's (PHP's `#[Attribute]`).
    NotAttribute,
}

/// How a language writes what isn't a name to look up.
struct Syntax {
    /// Words no file of a repository defines.
    keywords: &'static [&'static [&'static str]],
    /// What opens a comment that runs to the line's end.
    line_comments: &'static [&'static [u8]],
    hash: Hash,
    /// A block comment's opening and closing.
    block: Option<(&'static [u8], &'static [u8])>,
    /// `"""` and `'''` open strings closed by the same three quotes.
    triple: bool,
    backtick: Backtick,
    /// Words that, right before a quote, are a string's prefix: Python's
    /// `f` and `rb`, C++'s `R` and `u8`, Scala's `s`.
    prefixes: &'static [&'static [u8]],
    /// A line starting with `#` is a preprocessor directive.
    preprocessor: bool,
    /// Zig's: a word after `@` is a builtin (`@import`), and a line
    /// starting with `\\` is a multiline string's.
    zig: bool,
    forms: Forms,
}

/// The forms of comment and string beyond a block comment, a line comment
/// and quotes that end with their line, which [`Comments`] follows.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Forms(u16);

impl Forms {
    const NONE: Self = Self(0);
    /// Block comments nest (Rust's, Swift's, Kotlin's, Scala's).
    const NESTED: Self = Self(1);
    /// Rust's raw strings, `r"…"` and `r#"…"#`.
    const RUST_RAW: Self = Self(1 << 1);
    /// C++'s raw strings, `R"delimiter(…)delimiter"`.
    const CPP_RAW: Self = Self(1 << 2);
    /// Swift's raw strings, `#"…"#`.
    const SWIFT_RAW: Self = Self(1 << 3);
    /// C#'s verbatim strings, `@"…"`, a quote in them doubled.
    const VERBATIM: Self = Self(1 << 4);
    /// Heredocs, from `<<WORD` (or `<<-`, `<<~`, a quoted word) to the
    /// line holding only the word (Bash's, Ruby's).
    const HEREDOC: Self = Self(1 << 5);
    /// Ruby's `=begin` to `=end` lines.
    const BEGIN_END: Self = Self(1 << 6);
    /// Lua's long brackets, `[[…]]` and `[==[…]==]`, a comment after `--`.
    const LONG_BRACKETS: Self = Self(1 << 7);
    /// `"…"` and `'…'` run over lines (Bash's, Ruby's, PHP's, Elixir's).
    const MULTILINE_QUOTES: Self = Self(1 << 8);

    const fn and(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    const fn has(self, form: Self) -> bool {
        self.0 & form.0 != 0
    }
}

const C_LIKE: Syntax = Syntax {
    keywords: &[],
    line_comments: &[b"//"],
    hash: Hash::Anywhere,
    block: Some((b"/*", b"*/")),
    triple: false,
    backtick: Backtick::Code,
    prefixes: &[],
    preprocessor: false,
    zig: false,
    forms: Forms::NONE,
};

const HASH: Syntax = Syntax {
    line_comments: &[b"#"],
    block: None,
    ..C_LIKE
};

const C: Syntax = Syntax {
    keywords: &[C_KEYWORDS],
    preprocessor: true,
    ..C_LIKE
};

const CPP: Syntax = Syntax {
    keywords: &[C_KEYWORDS, CPP_KEYWORDS],
    preprocessor: true,
    prefixes: &[b"R", b"L", b"u", b"U", b"u8", b"LR", b"uR", b"UR", b"u8R"],
    forms: Forms::CPP_RAW,
    ..C_LIKE
};

const RUST: Syntax = Syntax {
    keywords: &[RUST_KEYWORDS],
    forms: Forms::NESTED.and(Forms::RUST_RAW),
    ..C_LIKE
};

const JAVASCRIPT: Syntax = Syntax {
    keywords: &[JAVASCRIPT_KEYWORDS],
    backtick: Backtick::Template,
    ..C_LIKE
};

const PYTHON: Syntax = Syntax {
    keywords: &[PYTHON_KEYWORDS],
    triple: true,
    prefixes: &[
        b"r", b"u", b"b", b"f", b"t", b"rb", b"br", b"fr", b"rf", b"tr", b"rt", b"R", b"U", b"B",
        b"F", b"T", b"Rb", b"rB", b"RB", b"bR", b"Br", b"BR", b"Fr", b"fR", b"FR", b"rF", b"Rf",
        b"RF",
    ],
    ..HASH
};

const GO: Syntax = Syntax {
    keywords: &[GO_KEYWORDS],
    backtick: Backtick::Raw,
    ..C_LIKE
};

const JAVA: Syntax = Syntax {
    keywords: &[JAVA_KEYWORDS],
    triple: true,
    ..C_LIKE
};

const CSHARP: Syntax = Syntax {
    keywords: &[CSHARP_KEYWORDS],
    triple: true,
    preprocessor: true,
    forms: Forms::VERBATIM,
    ..C_LIKE
};

const KOTLIN: Syntax = Syntax {
    keywords: &[KOTLIN_KEYWORDS],
    triple: true,
    forms: Forms::NESTED,
    ..C_LIKE
};

const SWIFT: Syntax = Syntax {
    keywords: &[SWIFT_KEYWORDS],
    triple: true,
    forms: Forms::NESTED.and(Forms::SWIFT_RAW),
    ..C_LIKE
};

const SCALA: Syntax = Syntax {
    keywords: &[SCALA_KEYWORDS],
    triple: true,
    prefixes: &[b"s", b"f", b"raw"],
    forms: Forms::NESTED,
    ..C_LIKE
};

const PHP: Syntax = Syntax {
    keywords: &[PHP_KEYWORDS],
    line_comments: &[b"//", b"#"],
    hash: Hash::NotAttribute,
    forms: Forms::MULTILINE_QUOTES,
    ..C_LIKE
};

const ZIG: Syntax = Syntax {
    keywords: &[ZIG_KEYWORDS],
    block: None,
    zig: true,
    ..C_LIKE
};

const RUBY: Syntax = Syntax {
    keywords: &[RUBY_KEYWORDS],
    forms: Forms::HEREDOC
        .and(Forms::BEGIN_END)
        .and(Forms::MULTILINE_QUOTES),
    ..HASH
};

const ELIXIR: Syntax = Syntax {
    keywords: &[ELIXIR_KEYWORDS],
    triple: true,
    forms: Forms::MULTILINE_QUOTES,
    ..HASH
};

const BASH: Syntax = Syntax {
    keywords: &[BASH_KEYWORDS],
    hash: Hash::AtWord,
    forms: Forms::HEREDOC.and(Forms::MULTILINE_QUOTES),
    ..HASH
};

const LUA: Syntax = Syntax {
    keywords: &[LUA_KEYWORDS],
    line_comments: &[b"--"],
    block: None,
    forms: Forms::LONG_BRACKETS,
    ..C_LIKE
};

/// The syntax of `language`; C's comments and strings, without keywords,
/// for one this module doesn't know.
fn syntax(language: &str) -> &'static Syntax {
    match language {
        "c" => &C,
        "cpp" => &CPP,
        "rust" => &RUST,
        "javascript" | "typescript" | "tsx" => &JAVASCRIPT,
        "python" => &PYTHON,
        "go" => &GO,
        "java" => &JAVA,
        "csharp" => &CSHARP,
        "kotlin" => &KOTLIN,
        "swift" => &SWIFT,
        "scala" => &SCALA,
        "php" => &PHP,
        "zig" => &ZIG,
        "ruby" => &RUBY,
        "elixir" => &ELIXIR,
        "bash" => &BASH,
        "lua" => &LUA,
        _ => &C_LIKE,
    }
}

fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// The names on `line`, a line of a file in language `language` (as
/// [`crate::highlight::Language::name`] gives it), in the order they first
/// appear, each once: words of letters, digits and `_` that don't start
/// with a digit. Left out: the language's keywords (none for a language
/// without a tags query), its comments (a block comment or a multiline
/// string that starts on an earlier line isn't known), string and
/// character literals with their prefixes (Python's `f"…"`, Rust's
/// `br"…"`, Go's backtick strings; a JavaScript template's `${…}` parts
/// are code), Rust's lifetimes and labels, Zig's builtins after `@`, a
/// preprocessor line's directive, and an `#include`'s file.
#[must_use]
pub fn names(line: &[u8], language: &str) -> Vec<String> {
    let keywords = syntax(language).keywords;
    let mut out: Vec<String> = Vec::new();
    scan(line, language, |word| {
        if let Ok(word) = std::str::from_utf8(word)
            && !keywords.iter().any(|k| k.contains(&word))
            && !out.iter().any(|w| w == word)
        {
            out.push(word.to_owned());
        }
        ControlFlow::Continue(())
    });
    out
}

/// Whether `name` is one of [`names`] of `line`, found without
/// allocating.
pub(crate) fn uses(line: &[u8], language: &str, name: &str) -> bool {
    let mut found = false;
    scan(line, language, |word| {
        found = word == name.as_bytes();
        if found {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    found
}

/// Hands each word of `line` that [`names`] would weigh, keywords
/// included, to `each`, until it breaks.
fn scan(line: &[u8], language: &str, mut each: impl FnMut(&[u8]) -> ControlFlow<()>) {
    let syntax = syntax(language);
    let rust = language == "rust";
    let mut i = 0;
    let trimmed = line.trim_ascii_start();
    if syntax.preprocessor
        && let Some(directive) = trimmed.strip_prefix(b"#")
    {
        let directive = directive.trim_ascii_start();
        let word = directive.iter().take_while(|&&b| is_name_byte(b)).count();
        if matches!(&directive[..word], b"include" | b"include_next" | b"import") {
            return;
        }
        i = line.len() - directive.len() + word;
    }
    if syntax.zig && trimmed.starts_with(b"\\\\") {
        return;
    }
    // Per JavaScript template open around the code, its braces open since.
    let mut templates: Vec<u32> = Vec::new();
    while i < line.len() {
        if let Some(next) = syntax.skip(line, i, rust, &mut templates) {
            i = next;
            continue;
        }
        let start = i;
        while i < line.len() && is_name_byte(line[i]) {
            i += 1;
        }
        let word = &line[start..i];
        // `b"…"`, `r"…"`, `r#"…"#`, `br"…"`, `c"…"`: a string.
        if rust && matches!(word, b"b" | b"r" | b"br" | b"c" | b"cr") {
            let raw = word.contains(&b'r');
            match line.get(i) {
                Some(b'"') if !raw => {
                    i = string_end(line, i);
                    continue;
                }
                Some(b'"' | b'#') if raw => {
                    i = raw_end(line, i);
                    continue;
                }
                Some(b'\'') if word == b"b" => {
                    i = quote_end(line, i, false);
                    continue;
                }
                _ => {}
            }
        }
        if syntax.prefixes.contains(&word) && matches!(line.get(i), Some(b'"' | b'\'')) {
            continue;
        }
        if !word[0].is_ascii_digit() && each(word).is_break() {
            return;
        }
    }
}

impl Syntax {
    /// Where `line` goes on past what starts at `at` when it's no name: a
    /// comment, a literal, a builtin, or one byte; `None` at a name.
    /// `templates` holds the JavaScript templates open around `at`.
    fn skip(&self, line: &[u8], at: usize, rust: bool, templates: &mut Vec<u32>) -> Option<usize> {
        let here = &line[at..];
        if let Some((open, close)) = self.block
            && here.starts_with(open)
        {
            let (mut depth, mut i) = (1, at + open.len());
            while depth > 0 && i < line.len() {
                if line[i..].starts_with(close) {
                    (depth, i) = (depth - 1, i + close.len());
                } else if self.forms.has(Forms::NESTED) && line[i..].starts_with(open) {
                    (depth, i) = (depth + 1, i + open.len());
                } else {
                    i += 1;
                }
            }
            return Some(i);
        }
        // Lua's long brackets: a string, or after `--` a comment.
        if self.forms.has(Forms::LONG_BRACKETS)
            && let Some((len, close)) = long_bracket(here.strip_prefix(b"--").unwrap_or(here))
        {
            let open = len + if here.starts_with(b"--") { 2 } else { 0 };
            let end = memchr::memmem::find(&here[open..], &close);
            return Some(end.map_or(line.len(), |e| at + open + e + close.len()));
        }
        if !hash_is_code(self, line, at) && self.line_comments.iter().any(|c| here.starts_with(c)) {
            return Some(line.len());
        }
        Some(match here[0] {
            quote @ (b'"' | b'\'') if self.triple && here.starts_with(&[quote; 3]) => {
                let end = memchr::memmem::find(&here[3..], &[quote; 3]);
                end.map_or(line.len(), |e| at + 3 + e + 3)
            }
            b'"' => match raw(self.forms, line, at) {
                Some((content, close)) => (memchr::memmem::find(&line[content..], &close))
                    .map_or(line.len(), |e| content + e + close.len()),
                None => string_end(line, at),
            },
            b'\'' => quote_end(line, at, rust),
            b'`' => match self.backtick {
                Backtick::Code => at + 1,
                Backtick::Raw => {
                    memchr::memchr(b'`', &here[1..]).map_or(line.len(), |e| at + e + 2)
                }
                Backtick::Template => template_text(line, at + 1, templates),
            },
            b'{' if !templates.is_empty() => {
                if let Some(open) = templates.last_mut() {
                    *open += 1;
                }
                at + 1
            }
            b'}' if !templates.is_empty() => match templates.last_mut() {
                Some(open) if *open > 0 => {
                    *open -= 1;
                    at + 1
                }
                _ => {
                    templates.pop();
                    template_text(line, at + 1, templates)
                }
            },
            b'@' if self.zig => at + 1 + here[1..].iter().take_while(|&&b| is_name_byte(b)).count(),
            // PHP's opening tag.
            b'<' if here.starts_with(b"<?php") => at + 5,
            b if is_name_byte(b) => return None,
            _ => at + 1,
        })
    }
}

/// Where a JavaScript template's text from `from` ends: past its closing
/// backtick, or past the `${` of a part that is code, which opens in
/// `templates`.
fn template_text(line: &[u8], from: usize, templates: &mut Vec<u32>) -> usize {
    let mut i = from;
    while i < line.len() {
        match line[i] {
            b'\\' => i += 2,
            b'`' => return i + 1,
            b'$' if line.get(i + 1) == Some(&b'{') => {
                templates.push(0);
                return i + 2;
            }
            _ => i += 1,
        }
    }
    line.len()
}

/// What a line of a file starts inside of.
#[derive(Debug, Default)]
enum Open {
    #[default]
    Code,
    /// A block comment, this deep.
    Block(u32),
    /// A string, or a Lua long comment, closed by `close`.
    String {
        close: Box<[u8]>,
        escape: Escape,
        /// A JavaScript template, whose `${` opens code.
        template: bool,
    },
    /// A heredoc, ended by a line holding only `word`, indented when
    /// `indented`.
    Heredoc { word: Box<[u8]>, indented: bool },
    /// Ruby's lines from `=begin` to `=end`.
    Begin,
}

/// How a string escapes its closing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Escape {
    /// It can't (raw strings).
    No,
    /// With a backslash.
    Backslash,
    /// By doubling it (C#'s verbatim strings).
    Doubled,
}

/// The comments and strings that run over lines, as a file's lines go by,
/// so a line that starts inside one is read from where it closes, or not
/// at all: block comments (nested in Rust, Swift, Kotlin and Scala),
/// strings of three quotes, JavaScript templates (their `${…}` parts are
/// code), Go's backtick strings, raw strings (Rust's, C++'s, Swift's), C#'s
/// verbatim strings, heredocs (Bash's, Ruby's), Ruby's `=begin` lines,
/// Lua's long brackets, and the quotes that run over lines in Bash, Ruby,
/// PHP and Elixir. What opens one inside another, as `/*` in a string,
/// opens nothing.
pub(crate) struct Comments {
    syntax: &'static Syntax,
    rust: bool,
    open: Open,
    /// The heredoc a line opened, which starts on the next one.
    pending: Option<(Box<[u8]>, bool)>,
    /// Per JavaScript template whose `${…}` is open, its braces open.
    templates: Vec<u32>,
    /// The bytes that can open something in code.
    stops: Vec<u8>,
}

impl Comments {
    /// Tracking for a file of `language`, from its start.
    pub(crate) fn new(language: &str) -> Self {
        let syntax = syntax(language);
        let mut stops: Vec<u8> = (syntax.line_comments.iter())
            .chain(syntax.block.iter().map(|(open, _)| open))
            .filter_map(|opener| opener.first().copied())
            .chain(*b"\"'")
            .collect();
        if syntax.backtick != Backtick::Code {
            stops.push(b'`');
        }
        if syntax.forms.has(Forms::LONG_BRACKETS) {
            stops.push(b'[');
        }
        if syntax.forms.has(Forms::HEREDOC) {
            stops.push(b'<');
        }
        stops.sort_unstable();
        stops.dedup();
        Self {
            syntax,
            rust: language == "rust",
            open: Open::Code,
            pending: None,
            templates: Vec::new(),
            stops,
        }
    }

    /// The part of `line`, the file's next, past a comment or string open
    /// at its start, or all of it; `None` when it runs past the line.
    /// Moves on past the line.
    pub(crate) fn line<'l>(&mut self, line: &'l [u8]) -> Option<&'l [u8]> {
        self.feed(line).map(|start| &line[start..])
    }

    /// Moves on past `lines`, the file's next whole lines, as
    /// [`Comments::line`] on each would.
    pub(crate) fn skip(&mut self, lines: &[u8]) {
        if lines.is_empty() {
            return;
        }
        let lines = lines.strip_suffix(b"\n").unwrap_or(lines);
        let mut start = 0;
        for end in memchr::memchr_iter(b'\n', lines) {
            self.feed(&lines[start..end]);
            start = end + 1;
        }
        self.feed(&lines[start..]);
    }

    /// Moves on past `line`; where in it code starts, if it does.
    fn feed(&mut self, line: &[u8]) -> Option<usize> {
        match &self.open {
            Open::Heredoc { word, indented } => {
                let body = if *indented {
                    line.trim_ascii_start()
                } else {
                    line
                };
                if body.trim_ascii_end() == &word[..] {
                    self.open = Open::Code;
                }
                return None;
            }
            Open::Begin => {
                if line.starts_with(b"=end") {
                    self.open = Open::Code;
                }
                return None;
            }
            Open::Code
                if self.syntax.forms.has(Forms::BEGIN_END) && line.starts_with(b"=begin") =>
            {
                self.open = Open::Begin;
                return None;
            }
            _ => {}
        }
        let mut code = None;
        let mut i = 0;
        while i < line.len() {
            let next = match std::mem::take(&mut self.open) {
                Open::Code => {
                    code.get_or_insert(i);
                    self.code(line, i)
                }
                Open::Block(depth) => self.block(line, i, depth),
                Open::String {
                    close,
                    escape,
                    template,
                } => self.string(line, i, close, escape, template),
                open @ (Open::Heredoc { .. } | Open::Begin) => {
                    self.open = open;
                    None
                }
            };
            match next {
                Some(next) => i = next,
                None => break,
            }
        }
        if matches!(self.open, Open::Code) && code.is_none() && i >= line.len() {
            code = Some(line.len());
        }
        if let Some((word, indented)) = self.pending.take()
            && matches!(self.open, Open::Code)
        {
            self.open = Open::Heredoc { word, indented };
        }
        code
    }

    /// Goes on in code at `at`: past the next thing that opens, or the
    /// line's end (`None`) at a line comment or with nothing left.
    fn code(&mut self, line: &[u8], at: usize) -> Option<usize> {
        let braces: &[u8] = if self.templates.is_empty() {
            &[]
        } else {
            b"{}"
        };
        let j = at + find_any(&line[at..], &self.stops, braces)?;
        let here = &line[j..];
        let syntax = self.syntax;
        if let Some((open, _)) = syntax.block
            && here.starts_with(open)
        {
            self.open = Open::Block(1);
            return Some(j + open.len());
        }
        if syntax.forms.has(Forms::LONG_BRACKETS) {
            if let Some(rest) = here.strip_prefix(b"--") {
                return long_bracket(rest).map(|(len, close)| {
                    self.open = string(close, Escape::No);
                    j + 2 + len
                });
            }
            if let Some((len, close)) = long_bracket(here) {
                self.open = string(close, Escape::No);
                return Some(j + len);
            }
        }
        if syntax.line_comments.iter().any(|c| here.starts_with(c))
            && !hash_is_code(syntax, line, j)
        {
            return None;
        }
        Some(match here[0] {
            b'"' | b'\'' => self.quote(line, j),
            b'`' => match syntax.backtick {
                Backtick::Code => j + 1,
                Backtick::Raw => {
                    self.open = string(Box::new(*b"`"), Escape::No);
                    j + 1
                }
                Backtick::Template => {
                    self.open = Open::String {
                        close: Box::new(*b"`"),
                        escape: Escape::Backslash,
                        template: true,
                    };
                    j + 1
                }
            },
            b'<' => self.heredoc(line, j),
            b'{' => {
                if let Some(open) = self.templates.last_mut() {
                    *open += 1;
                }
                j + 1
            }
            b'}' => {
                match self.templates.last_mut() {
                    Some(open) if *open > 0 => *open -= 1,
                    _ => {
                        self.templates.pop();
                        self.open = Open::String {
                            close: Box::new(*b"`"),
                            escape: Escape::Backslash,
                            template: true,
                        };
                    }
                }
                j + 1
            }
            _ => j + 1,
        })
    }

    /// Goes on past the quote at `at`: a string that ends on the line, or
    /// one that may not, which opens.
    fn quote(&mut self, line: &[u8], at: usize) -> usize {
        let (quote, forms) = (line[at], self.syntax.forms);
        if self.syntax.triple && line[at..].starts_with(&[quote; 3]) {
            self.open = string(Box::new([quote; 3]), Escape::Backslash);
            return at + 3;
        }
        if quote == b'"' {
            if let Some((content, close)) = raw(forms, line, at) {
                self.open = string(close, Escape::No);
                return content;
            }
            if forms.has(Forms::VERBATIM) && line[..at].ends_with(b"@") {
                self.open = string(Box::new(*b"\""), Escape::Doubled);
                return at + 1;
            }
        }
        if forms.has(Forms::MULTILINE_QUOTES) {
            self.open = string(Box::new([quote]), Escape::Backslash);
            return at + 1;
        }
        if quote == b'"' {
            string_end(line, at)
        } else {
            quote_end(line, at, self.rust)
        }
    }

    /// Goes on past `<<WORD` at `at` when it opens a heredoc, which starts
    /// on the next line.
    fn heredoc(&mut self, line: &[u8], at: usize) -> usize {
        let Some(rest) = line[at..].strip_prefix(b"<<") else {
            return at + 1;
        };
        if rest.starts_with(b"<") {
            return at + 3;
        }
        let (indented, rest) = match rest.first() {
            Some(b'-' | b'~') => (true, &rest[1..]),
            _ => (false, rest),
        };
        let quoted = matches!(rest.first(), Some(b'\'' | b'"'));
        let word = &rest[usize::from(quoted)..];
        let len = word.iter().take_while(|&&b| is_name_byte(b)).count();
        if len == 0 {
            return at + 2;
        }
        self.pending = Some((word[..len].into(), indented));
        line.len() - word.len() + len + usize::from(quoted)
    }

    /// Goes on in a block comment `depth` deep at `at`.
    fn block(&mut self, line: &[u8], at: usize, mut depth: u32) -> Option<usize> {
        let (open, close) = self.syntax.block?;
        let nested = self.syntax.forms.has(Forms::NESTED);
        let mut i = at;
        loop {
            let rest = &line[i..];
            let next = if nested {
                memchr::memchr2(close[0], open[0], rest)
            } else {
                memchr::memchr(close[0], rest)
            };
            let Some(next) = next else {
                self.open = Open::Block(depth);
                return None;
            };
            i += next;
            if line[i..].starts_with(close) {
                i += close.len();
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            } else if nested && line[i..].starts_with(open) {
                i += open.len();
                depth += 1;
            } else {
                i += 1;
            }
        }
    }

    /// Goes on in a string closed by `close` at `at`.
    fn string(
        &mut self,
        line: &[u8],
        at: usize,
        close: Box<[u8]>,
        escape: Escape,
        template: bool,
    ) -> Option<usize> {
        let mut i = at;
        loop {
            let rest = &line[i..];
            let next = if template {
                memchr::memchr3(b'`', b'$', b'\\', rest)
            } else if escape == Escape::Backslash {
                memchr::memchr2(close[0], b'\\', rest)
            } else {
                memchr::memchr(close[0], rest)
            };
            let Some(next) = next else {
                self.open = Open::String {
                    close,
                    escape,
                    template,
                };
                return None;
            };
            i += next;
            match line[i] {
                b'\\' if escape == Escape::Backslash => i += 2,
                b'$' if template => {
                    if line.get(i + 1) == Some(&b'{') {
                        self.templates.push(0);
                        return Some(i + 2);
                    }
                    i += 1;
                }
                _ if line[i..].starts_with(&close) => {
                    if escape == Escape::Doubled && line.get(i + close.len()) == Some(&close[0]) {
                        i += close.len() + 1;
                    } else {
                        return Some(i + close.len());
                    }
                }
                _ => i += 1,
            }
        }
    }
}

/// For the `"` at `at` of `line` when it opens a raw string of `forms`
/// (Rust's `r#"`, Swift's `#"`, C++'s `R"d(`): where its content starts,
/// and what closes it.
fn raw(forms: Forms, line: &[u8], at: usize) -> Option<(usize, Box<[u8]>)> {
    let hashes = line[..at].iter().rev().take_while(|&&b| b == b'#').count();
    let word = &line[..at - hashes];
    let word = &word[word
        .iter()
        .rposition(|&b| !is_name_byte(b))
        .map_or(0, |p| p + 1)..];
    let mut close = vec![b'"'];
    close.extend(std::iter::repeat_n(b'#', hashes));
    if (forms.has(Forms::RUST_RAW) && matches!(word, b"r" | b"br" | b"cr"))
        || (forms.has(Forms::SWIFT_RAW) && hashes > 0 && word.is_empty())
    {
        return Some((at + 1, close.into()));
    }
    if forms.has(Forms::CPP_RAW) && matches!(word, b"R" | b"u8R" | b"uR" | b"UR" | b"LR") {
        let open = line[at + 1..].iter().take(17).position(|&b| b == b'(')?;
        let delimiter = &line[at + 1..at + 1 + open];
        return Some((at + open + 2, [b")", delimiter, b"\""].concat().into()));
    }
    None
}

fn string(close: Box<[u8]>, escape: Escape) -> Open {
    Open::String {
        close,
        escape,
        template: false,
    }
}

/// Whether a `#` at `at` of `line`, in a language whose line comments it
/// may open, is code instead: Bash's `$#` and PHP's `#[…]`.
fn hash_is_code(syntax: &Syntax, line: &[u8], at: usize) -> bool {
    line[at] == b'#'
        && match syntax.hash {
            Hash::Anywhere => false,
            Hash::AtWord => at > 0 && !line[at - 1].is_ascii_whitespace(),
            Hash::NotAttribute => line.get(at + 1) == Some(&b'['),
        }
}

/// The length of a Lua long bracket opening `text`, `[[` or `[==[`, and
/// what closes it.
fn long_bracket(text: &[u8]) -> Option<(usize, Box<[u8]>)> {
    let rest = text.strip_prefix(b"[")?;
    let level = rest.iter().take_while(|&&b| b == b'=').count();
    (rest.get(level) == Some(&b'[')).then(|| {
        let mut close = vec![b']'];
        close.extend(std::iter::repeat_n(b'=', level));
        close.push(b']');
        (level + 2, close.into())
    })
}

/// Where the first byte of `text` in `stops` or `more` is, found as
/// memchr finds a few.
fn find_any(text: &[u8], stops: &[u8], more: &[u8]) -> Option<usize> {
    (stops.chunks(3).chain(more.chunks(3)))
        .filter_map(|set| match *set {
            [a] => memchr::memchr(a, text),
            [a, b] => memchr::memchr2(a, b, text),
            [a, b, c] => memchr::memchr3(a, b, c, text),
            _ => None,
        })
        .min()
}

/// The byte after the string starting with the `"` at `at`.
fn string_end(line: &[u8], at: usize) -> usize {
    let mut i = at + 1;
    while i < line.len() && line[i] != b'"' {
        i += if line[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(line.len())
}

/// The byte after a Rust raw string whose `#`s or `"` start at `at`.
fn raw_end(line: &[u8], at: usize) -> usize {
    let hashes = line[at..].iter().take_while(|&&b| b == b'#').count();
    let open = at + hashes;
    if line.get(open) != Some(&b'"') {
        return open;
    }
    let close: Vec<u8> = std::iter::once(b'"')
        .chain(std::iter::repeat_n(b'#', hashes))
        .collect();
    (memchr::memmem::find(&line[open + 1..], &close))
        .map_or(line.len(), |end| open + 1 + end + close.len())
}

/// The byte after what the `'` at `at` starts: a character literal, or in
/// Rust a lifetime or label, whose name isn't one to look up.
fn quote_end(line: &[u8], at: usize, rust: bool) -> usize {
    let after = &line[at + 1..];
    if rust {
        let word = after.iter().take_while(|&&b| is_name_byte(b)).count();
        let char_literal = after.get(word) == Some(&b'\'') && word > 0;
        if word > 0 && !char_literal && !after[0].is_ascii_digit() {
            return at + 1 + word;
        }
    }
    let mut i = at + 1;
    while i < line.len() && line[i] != b'\'' {
        i += if line[i] == b'\\' { 2 } else { 1 };
    }
    (i + 1).min(line.len())
}

/// The name around byte `at` of `line`, by [`names`]' rule for a word;
/// `None` off one, or on a number.
#[must_use]
pub fn name_at(line: &[u8], at: usize) -> Option<Range<usize>> {
    if !line.get(at).copied().is_some_and(is_name_byte) {
        return None;
    }
    let start = line[..at]
        .iter()
        .rposition(|&b| !is_name_byte(b))
        .map_or(0, |i| i + 1);
    let end = line[at..]
        .iter()
        .position(|&b| !is_name_byte(b))
        .map_or(line.len(), |i| at + i);
    (!line[start].is_ascii_digit()).then_some(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_c_lines_names_leave_out_keywords_literals_and_comments() {
        let line = b"\tvoid *p = kmalloc_array(4, sizeof(*p)); /* x y */ printk(\"no %s\", s, '\\'', 0x1fUL); // q";
        assert_eq!(names(line, "c"), ["p", "kmalloc_array", "printk", "s"]);
        assert_eq!(names(b"#include <linux/slab.h>", "c"), Vec::<String>::new());
        assert_eq!(names(b" # define MAX(a) (a)", "c"), ["MAX", "a"]);
        assert_eq!(names(b"#ifdef CONFIG_SLAB", "c"), ["CONFIG_SLAB"]);
    }

    #[test]
    fn a_rust_lines_names_leave_out_lifetimes_and_byte_and_raw_strings() {
        let line = b"fn get<'a>(m: &'a Map, k: &[u8]) -> Option<&'a str> { m.find(b\"k\", r#\"x \"y\"#, br\"z\", b'q', 'c') }";
        assert_eq!(
            names(line, "rust"),
            ["get", "m", "Map", "k", "u8", "Option", "str", "find"]
        );
        assert_eq!(
            names(b"let v: Vec<u8> = Vec::with_capacity(n);", "rust"),
            ["v", "Vec", "u8", "with_capacity", "n"]
        );
        assert_eq!(
            names(b"'outer: loop { break 'outer; }", "rust"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn another_languages_names_keep_every_word() {
        assert_eq!(
            names(b"if x return y", "markdown"),
            ["if", "x", "return", "y"]
        );
    }

    /// A line of each language, and the names it gives.
    const LINES: &[(&str, &[u8], &[&str])] = &[
        (
            "typescript",
            b"export const f = (x: number): string => `a ${g(x)} b ${ {k: h} } c` + 'q'; // r",
            &["f", "x", "number", "string", "g", "k", "h"],
        ),
        (
            "tsx",
            b"const App = () => <Item key={id} />; /* note */",
            &["App", "Item", "key", "id"],
        ),
        (
            "javascript",
            b"import { a } from \"./b\"; let c = `${a}`;",
            &["a", "from", "c"],
        ),
        (
            "python",
            b"def f(self, x): return g(f\"{x}\", rb'y', \"\"\"doc\"\"\") // h  # comment",
            &["f", "self", "x", "g", "h"],
        ),
        (
            "go",
            b"func (p *Point) Move(dx int) error { return fmt.Errorf(`raw %d`, dx) } // x",
            &["p", "Point", "Move", "dx", "int", "error", "fmt", "Errorf"],
        ),
        (
            "java",
            b"public static List<String> of(int n) { return new ArrayList<>(n); }",
            &["List", "String", "of", "n", "ArrayList"],
        ),
        ("cpp", b"#include <vector>", &[]),
        (
            "cpp",
            b"template <typename T> auto f(T t) -> decltype(t) { return R\"(x)\" + std::move(t); }",
            &["T", "f", "t", "std", "move"],
        ),
        ("csharp", b"#region Fields", &["Fields"]),
        (
            "csharp",
            b"public async Task<int> Run(string s) => await Go($\"{s}\", @\"c:\\x\");",
            &["async", "Task", "Run", "s", "await", "Go"],
        ),
        (
            "kotlin",
            b"override fun area(): Int = value.size /* c */ + \"\"\"x\"\"\".length",
            &["override", "area", "Int", "value", "size", "length"],
        ),
        (
            "swift",
            b"func area() -> Int { guard let s = self.size else { return 0 } } // c",
            &["area", "Int", "s", "size"],
        ),
        (
            "ruby",
            b"def area(x) = x.size unless empty? # comment",
            &["area", "x", "size", "empty"],
        ),
        (
            "php",
            b"<?php public function run(int $x): string { return $this->go('a'); } # c",
            &["run", "x", "go"],
        ),
        (
            "php",
            b"#[Attribute] class Point {}",
            &["Attribute", "Point"],
        ),
        (
            "scala",
            b"def area(x: Int): String = s\"$x\" + raw\"y\" // c",
            &["area", "x", "Int", "String"],
        ),
        (
            "zig",
            b"const std = @import(\"std\"); pub fn main() !void {} // c",
            &["std", "main"],
        ),
        ("zig", b"    \\\\ a multiline string's line", &[]),
        (
            "lua",
            b"local function area(x) return x * 2 end --[[ c ]] -- d",
            &["area", "x"],
        ),
        (
            "elixir",
            b"defp area(x), do: x * 2 # comment",
            &["defp", "area", "x"],
        ),
        (
            "bash",
            b"if [ $# -gt 0 ]; then run \"$1\" ${#arr[@]}; fi # comment",
            &["gt", "run", "arr"],
        ),
    ];

    /// Each language's comments, string forms and keywords, on one line.
    #[test]
    fn each_languages_names_leave_out_its_comments_strings_and_keywords() {
        for (language, line, want) in LINES {
            assert_eq!(
                names(line, language),
                *want,
                "{language}: {}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn a_click_widens_to_the_name_around_it() {
        let line = b"x = kmalloc_array(4, 8);";
        assert_eq!(name_at(line, 8), Some(4..17));
        assert_eq!(name_at(line, 0), Some(0..1));
        assert_eq!(name_at(line, 18), None);
        assert_eq!(name_at(line, 3), None);
    }

    /// A file of each language where a string or a comment that runs over
    /// lines holds what opens another, and the lines using `frob` in code,
    /// 1-based.
    const FILES: &[(&str, &str, &[usize])] = &[
        (
            "c",
            "/* a\n frob */ frob();\nchar *s = \"/*\"; frob();\n",
            &[2, 3],
        ),
        (
            "javascript",
            "const glob = `src/**/*.js\n${frob(1)} more /* `;\nfrob(2);\n// frob\n",
            &[2, 3],
        ),
        ("typescript", "let s = `a /* b\nfrob`;\nfrob();\n", &[3]),
        ("tsx", "const t = `${`/*`}\n`;\nfrob();\n", &[3]),
        ("go", "var re = `a/*b\nfrob c`\nfrob()\n", &[3]),
        (
            "rust",
            "let s = r#\"/* \"quoted\"\nfrob\"#;\nfrob();\n/* a /* b */ frob */\nfrob();\n",
            &[3, 5],
        ),
        (
            "python",
            "x = \"\"\"doc /*\nfrob in doc\n\"\"\"\nfrob()\ny = '# no' ; frob()\n",
            &[4, 5],
        ),
        ("cpp", "auto s = R\"x(/* )\"\nfrob )x\";\nfrob();\n", &[3]),
        (
            "csharp",
            "var s = @\"c:\\dir /*\n\"\"frob\"\"\";\nfrob();\nvar r = \"\"\"\n/* frob\n\"\"\";\nfrob();\n",
            &[3, 7],
        ),
        (
            "java",
            "String s = \"\"\"\n  /* frob\n  \"\"\";\nfrob();\n",
            &[4],
        ),
        ("kotlin", "val s = \"\"\"/*\nfrob\n\"\"\"\nfrob()\n", &[4]),
        (
            "scala",
            "val s = s\"\"\"/* $x\nfrob\n\"\"\"\nfrob()\n",
            &[4],
        ),
        (
            "swift",
            "let s = \"\"\"\n/* frob\n\"\"\"\nfrob()\nlet r = #\"a\"/*\"#; frob()\n",
            &[4, 5],
        ),
        ("php", "$s = 'a\n/* frob';\nfrob();\n# frob\n", &[3]),
        (
            "ruby",
            "s = <<~SQL\n  frob /* x\n  SQL\nfrob\n=begin\nfrob\n=end\nfrob\nt = \"a\nfrob\"; frob\n",
            &[4, 8, 10],
        ),
        (
            "bash",
            "cat <<'EOF'\nfrob \"unclosed\nEOF\nfrob\necho \"a\n# not a comment frob\" ; frob\n",
            &[4, 6],
        ),
        (
            "lua",
            "local s = [==[\n/* frob ]]\n]==]\nfrob()\n--[[ frob\nfrob ]] frob()\n",
            &[4, 6],
        ),
        ("elixir", "s = \"\"\"\nfrob # x\n\"\"\"\nfrob()\n", &[4]),
        ("zig", "const s =\n    \\\\ /* frob\n;\nfrob();\n", &[4]),
    ];

    /// The lines using `name` in code in `text`, 1-based, each line read
    /// in turn.
    fn uses_in(language: &str, text: &str, name: &str) -> Vec<usize> {
        let mut comments = Comments::new(language);
        (text.lines().enumerate())
            .filter_map(|(n, line)| {
                let code = comments.line(line.as_bytes())?;
                uses(code, language, name).then_some(n + 1)
            })
            .collect()
    }

    /// What opens a comment or a string inside another opens nothing, and
    /// one that runs over lines is followed to where it closes, the same
    /// whether the lines before are read one by one or skipped together.
    #[test]
    fn comments_follow_each_languages_strings_over_lines() {
        for (language, text, want) in FILES {
            assert_eq!(
                uses_in(language, text, "frob"),
                *want,
                "{language}:\n{text}"
            );
            let lines: Vec<&str> = text.lines().collect();
            for (n, last) in lines.iter().enumerate() {
                let before = lines[..n]
                    .iter()
                    .fold(String::new(), |all, l| all + l + "\n");
                let mut skipped = Comments::new(language);
                skipped.skip(before.as_bytes());
                let found = (skipped.line(last.as_bytes()))
                    .is_some_and(|code| uses(code, language, "frob"));
                assert_eq!(found, want.contains(&(n + 1)), "{language}, line {}", n + 1);
            }
        }
    }
}

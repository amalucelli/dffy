//! The grammar registry: the languages a host can highlight and syntax-diff,
//! how a file is matched to one, and each one's grammar and query. A host
//! builds one [`Registry`] (dffy's is [`Registry::builtin`]) and passes it
//! wherever a file's language is needed; nothing in the core holds one.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};

use gix::bstr::ByteSlice;

use super::Compiled;
use crate::git::ObjectId;

/// A list of words or query parts: borrowed for text compiled into the
/// host, owned for text it loads at run time.
pub type Words = Cow<'static, [Cow<'static, str>]>;

/// A language as a host describes it to a [`Registry`]: plain data, with no
/// code of its own. Text is borrowed or owned (see [`Words`]); the registry
/// keeps it as given and matching files never copies it.
pub struct Grammar {
    /// Lowercase and stable: code fences name languages by it, and it's
    /// part of the language's [`Language::identity`].
    pub name: Cow<'static, str>,
    /// Whole file names that are this language (`Cargo.lock`, `.bashrc`).
    pub file_names: Words,
    /// Extensions after the last `.` of a file name, without the dot.
    pub extensions: Words,
    /// Programs a `#!` line may name, without a trailing version
    /// (`python` matches `python3.12`).
    pub interpreters: Words,
    /// The tree-sitter grammar.
    pub language: tree_sitter::Language,
    /// The highlight query, as parts concatenated in order. When two
    /// patterns capture the same node the last one wins (as in
    /// tree-sitter's own highlighter), so a query extending another's goes
    /// after it.
    pub highlights: Words,
    /// A line's indentation is syntax: changing it can move code into or
    /// out of a block with the same tokens in the same order, so the
    /// syntax diff never calls such a change only reformatting.
    pub indentation_is_syntax: bool,
}

/// A language of a [`Registry`]: a shared handle to its [`Grammar`], whose
/// highlight query is compiled on first use (or ahead of it, by
/// [`Language::prepare`]) and kept as long as any handle is. Cloning is
/// cheap; two handles are equal when they're the same
/// registry entry.
#[derive(Clone)]
pub struct Language(Arc<Entry>);

struct Entry {
    grammar: Grammar,
    compiled: OnceLock<Option<Compiled>>,
    identity: OnceLock<ObjectId>,
    #[cfg(test)]
    compiles: std::sync::atomic::AtomicUsize,
}

impl PartialEq for Language {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Language {}

impl std::fmt::Debug for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Language").field(&self.name()).finish()
    }
}

impl Language {
    fn new(grammar: Grammar) -> Self {
        Self(Arc::new(Entry {
            grammar,
            compiled: OnceLock::new(),
            identity: OnceLock::new(),
            #[cfg(test)]
            compiles: std::sync::atomic::AtomicUsize::new(0),
        }))
    }

    /// The [`Grammar::name`].
    #[must_use]
    pub fn name(&self) -> &str {
        &self.0.grammar.name
    }

    /// The [`Grammar::indentation_is_syntax`].
    #[must_use]
    pub fn indentation_is_syntax(&self) -> bool {
        self.0.grammar.indentation_is_syntax
    }

    /// What results computed with this language depend on, for disk cache
    /// keys: a digest of its name, the grammar's ABI version, metadata
    /// version and table sizes, and its query text. Updating a grammar or
    /// its query changes it, which orphans the results cached under the old
    /// one. Computed on first use.
    #[must_use]
    pub fn identity(&self) -> &ObjectId {
        self.0.identity.get_or_init(|| {
            let g = &self.0.grammar;
            let mut hasher = gix::hash::hasher(gix::hash::Kind::Sha1);
            let number = |hasher: &mut gix::hash::Hasher, n: usize| {
                hasher.update(&(n as u64).to_le_bytes());
            };
            number(&mut hasher, g.name.len());
            hasher.update(g.name.as_bytes());
            let ts = &g.language;
            number(&mut hasher, ts.abi_version());
            let version = ts.metadata().map_or([u8::MAX; 3], |m| {
                [m.major_version, m.minor_version, m.patch_version]
            });
            hasher.update(&version);
            number(&mut hasher, ts.node_kind_count());
            number(&mut hasher, ts.parse_state_count());
            number(&mut hasher, ts.field_count());
            for part in g.highlights.iter() {
                number(&mut hasher, part.len());
                hasher.update(part.as_bytes());
            }
            hasher
                .try_finalize()
                .unwrap_or_else(|_| ObjectId::null(gix::hash::Kind::Sha1))
        })
    }

    pub(crate) fn grammar(&self) -> &tree_sitter::Language {
        &self.0.grammar.language
    }

    /// Compiles the highlight query now unless that's done, so the
    /// language's first parse doesn't wait for it: call it on a spare
    /// thread once a file of this language is known to be coming. Blocks
    /// while the query compiles (up to about 140ms, Swift's), including
    /// when another thread started it; the query is compiled once however
    /// many threads ask. Afterwards it returns at once.
    pub fn prepare(&self) {
        self.compiled();
    }

    /// The compiled highlight query, compiling it on first use. `None` if
    /// the query doesn't compile against the grammar.
    pub(super) fn compiled(&self) -> Option<&Compiled> {
        self.0
            .compiled
            .get_or_init(|| {
                #[cfg(test)]
                self.0
                    .compiles
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Compiled::new(
                    &self.0.grammar.language,
                    &self.0.grammar.highlights.concat(),
                )
            })
            .as_ref()
    }

    #[cfg(test)]
    pub(super) fn compiles(&self) -> usize {
        self.0.compiles.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Whether the query was compiled (or failed to) already.
    pub(super) fn is_compiled(&self) -> bool {
        self.0.compiled.get().is_some()
    }
}

/// The languages a host knows. Built once and shared; matching a file to a
/// language scans the entries, so look a file's language up once, not per
/// line.
#[derive(Debug)]
pub struct Registry {
    languages: Vec<Language>,
}

impl Registry {
    /// A registry of `grammars`. When two match the same file the earlier
    /// one wins.
    #[must_use]
    pub fn new(grammars: impl IntoIterator<Item = Grammar>) -> Self {
        Self {
            languages: grammars.into_iter().map(Language::new).collect(),
        }
    }

    /// The 30 grammars bundled with dffy-core. `.h` counts as C, the common
    /// case in C codebases.
    #[must_use]
    pub fn builtin() -> Self {
        Self::new(builtin())
    }

    /// Every language, in the order given.
    #[must_use]
    pub fn languages(&self) -> &[Language] {
        &self.languages
    }

    /// The language with [`Grammar::name`] `name`.
    #[must_use]
    pub fn named(&self, name: &str) -> Option<&Language> {
        self.languages.iter().find(|l| l.name() == name)
    }

    /// The language of the file at `path` (repository-relative), by file
    /// name, then extension, then a `#!` line at the start of `content`.
    /// Each step looks through every language before the next one starts.
    #[must_use]
    pub fn detect(&self, path: &[u8], content: &[u8]) -> Option<&Language> {
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
        let find = |words: fn(&Grammar) -> &[Cow<'static, str>], word: &[u8]| {
            self.languages
                .iter()
                .find(|l| words(&l.0.grammar).iter().any(|w| w.as_bytes() == word))
        };
        find(|g| &g.file_names, name)
            .or_else(|| find(|g| &g.extensions, name.rsplit_once_str(".")?.1))
            .or_else(|| find(|g| &g.interpreters, interpreter(content)?))
    }
}

/// The interpreter named by a `#!` line, skipping `env` and its options,
/// without a trailing version.
fn interpreter(content: &[u8]) -> Option<&[u8]> {
    let line = content.strip_prefix(b"#!")?;
    let line = &line[..line.find_byte(b'\n').unwrap_or(line.len())];
    let mut words = line
        .fields()
        .map(|w| w.rsplit(|&b| b == b'/').next().unwrap_or(w));
    let mut program = words.next()?;
    if program == b"env" {
        program = words.find(|w| !w.starts_with(b"-") && !w.contains(&b'='))?;
    }
    Some(program.trim_end_with(|c| c.is_ascii_digit() || c == '.'))
}

/// Borrowed [`Words`] of `&'static str` expressions. The `const` block is
/// what makes the list static: an array of `Cow`s, which have a destructor,
/// isn't promoted to a static on its own.
macro_rules! words {
    ($($word:expr),* $(,)?) => {
        Cow::Borrowed(const { &[$(Cow::Borrowed($word)),*] })
    };
}

/// Patterns after the Kotlin crate's query. It captures a whole `return …`
/// as a keyword, which would paint the plain identifiers inside it as one;
/// these take that node back and capture only the jump keywords.
const KOTLIN_JUMPS: &str = r#"(jump_expression) @none
["return" "return@" "break" "break@" "continue" "continue@"] @keyword.return
"#;

/// The bundled grammars. Queries that extend another grammar's list the
/// base first: TypeScript's `tree-sitter.json` lists its own query first,
/// which loses JSX tags to JavaScript's catch-all `(identifier) @variable`.
///
/// Scala gets [`Grammar::indentation_is_syntax`], as a Scala 3 block can
/// end by dedenting; the cost is that reindented brace code shows as
/// changed. Make doesn't need it: a recipe line that loses its tab parses
/// as something else, so it shows as changed anyway.
#[expect(
    clippy::too_many_lines,
    reason = "a table with an entry per bundled grammar"
)]
fn builtin() -> [Grammar; 30] {
    let grammar = |name, language: tree_sitter::Language| Grammar {
        name: Cow::Borrowed(name),
        file_names: words![],
        extensions: words![],
        interpreters: words![],
        language,
        highlights: words![],
        indentation_is_syntax: false,
    };
    [
        Grammar {
            file_names: words![
                "PKGBUILD",
                "APKBUILD",
                ".bashrc",
                ".bash_profile",
                ".profile",
                ".zshrc",
                ".zprofile",
                ".zshenv",
                "bashrc",
                "zshrc",
            ],
            extensions: words!["sh", "bash", "zsh", "ksh", "bats", "ebuild"],
            interpreters: words!["sh", "bash", "zsh", "dash", "ksh", "ash"],
            highlights: words![tree_sitter_bash::HIGHLIGHT_QUERY],
            ..grammar("bash", tree_sitter_bash::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["c", "h"],
            highlights: words![tree_sitter_c::HIGHLIGHT_QUERY],
            ..grammar("c", tree_sitter_c::LANGUAGE.into())
        },
        Grammar {
            extensions: words![
                "cc", "cpp", "cxx", "c++", "hh", "hpp", "hxx", "h++", "ipp", "tpp", "inl",
            ],
            highlights: words![
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ],
            ..grammar("cpp", tree_sitter_cpp::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["cs"],
            highlights: words![tree_sitter_c_sharp::HIGHLIGHTS_QUERY],
            ..grammar("csharp", tree_sitter_c_sharp::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["css"],
            highlights: words![tree_sitter_css::HIGHLIGHTS_QUERY],
            ..grammar("css", tree_sitter_css::LANGUAGE.into())
        },
        Grammar {
            file_names: words!["Dockerfile", "Containerfile"],
            extensions: words!["dockerfile", "containerfile"],
            highlights: words![tree_sitter_containerfile::HIGHLIGHTS_QUERY],
            ..grammar("dockerfile", tree_sitter_containerfile::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["ex", "exs"],
            interpreters: words!["elixir"],
            highlights: words![tree_sitter_elixir::HIGHLIGHTS_QUERY],
            ..grammar("elixir", tree_sitter_elixir::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["go"],
            highlights: words![tree_sitter_go::HIGHLIGHTS_QUERY],
            ..grammar("go", tree_sitter_go::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["tf", "tfvars", "hcl"],
            highlights: words![include_str!("queries/hcl/highlights.scm")],
            ..grammar("hcl", tree_sitter_hcl::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["html", "htm", "xhtml"],
            highlights: words![tree_sitter_html::HIGHLIGHTS_QUERY],
            ..grammar("html", tree_sitter_html::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["java"],
            highlights: words![tree_sitter_java::HIGHLIGHTS_QUERY],
            ..grammar("java", tree_sitter_java::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["js", "mjs", "cjs", "jsx"],
            interpreters: words!["node", "nodejs", "deno", "bun"],
            highlights: words![
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ],
            ..grammar("javascript", tree_sitter_javascript::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["json", "jsonc", "json5", "geojson", "webmanifest"],
            highlights: words![tree_sitter_json::HIGHLIGHTS_QUERY],
            ..grammar("json", tree_sitter_json::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["kt", "kts"],
            highlights: words![tree_sitter_kotlin_sg::HIGHLIGHTS_QUERY, KOTLIN_JUMPS],
            ..grammar("kotlin", tree_sitter_kotlin_sg::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["lua"],
            interpreters: words!["lua", "luajit"],
            highlights: words![tree_sitter_lua::HIGHLIGHTS_QUERY],
            ..grammar("lua", tree_sitter_lua::LANGUAGE.into())
        },
        Grammar {
            file_names: words!["Makefile", "makefile", "GNUmakefile"],
            extensions: words!["mk"],
            highlights: words![tree_sitter_make::HIGHLIGHTS_QUERY],
            ..grammar("make", tree_sitter_make::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["md", "markdown", "mdx"],
            highlights: words![tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
            indentation_is_syntax: true,
            ..grammar("markdown", tree_sitter_md::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["php"],
            interpreters: words!["php"],
            highlights: words![tree_sitter_php::HIGHLIGHTS_QUERY],
            ..grammar("php", tree_sitter_php::LANGUAGE_PHP.into())
        },
        Grammar {
            extensions: words!["py", "pyi", "pyw"],
            interpreters: words!["python", "pypy"],
            highlights: words![tree_sitter_python::HIGHLIGHTS_QUERY],
            indentation_is_syntax: true,
            ..grammar("python", tree_sitter_python::LANGUAGE.into())
        },
        Grammar {
            file_names: words!["Gemfile", "Rakefile", "Vagrantfile", "Podfile", "Brewfile"],
            extensions: words!["rb", "rake", "gemspec", "ru"],
            interpreters: words!["ruby"],
            highlights: words![tree_sitter_ruby::HIGHLIGHTS_QUERY],
            ..grammar("ruby", tree_sitter_ruby::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["rs"],
            highlights: words![tree_sitter_rust::HIGHLIGHTS_QUERY],
            ..grammar("rust", tree_sitter_rust::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["scala", "sc", "sbt"],
            highlights: words![tree_sitter_scala::HIGHLIGHTS_QUERY],
            indentation_is_syntax: true,
            ..grammar("scala", tree_sitter_scala::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["sql"],
            highlights: words![tree_sitter_sequel::HIGHLIGHTS_QUERY],
            ..grammar("sql", tree_sitter_sequel::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["swift"],
            highlights: words![tree_sitter_swift::HIGHLIGHTS_QUERY],
            ..grammar("swift", tree_sitter_swift::LANGUAGE.into())
        },
        Grammar {
            file_names: words!["Cargo.lock", "Pipfile", "poetry.lock", "uv.lock"],
            extensions: words!["toml"],
            highlights: words![tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
            ..grammar("toml", tree_sitter_toml_ng::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["tsx"],
            highlights: words![
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
            ..grammar("tsx", tree_sitter_typescript::LANGUAGE_TSX.into())
        },
        Grammar {
            extensions: words!["ts", "mts", "cts"],
            highlights: words![
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
            ..grammar(
                "typescript",
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            )
        },
        Grammar {
            extensions: words!["xml", "xsd", "xsl", "svg", "plist"],
            highlights: words![tree_sitter_xml::XML_HIGHLIGHT_QUERY],
            ..grammar("xml", tree_sitter_xml::LANGUAGE_XML.into())
        },
        Grammar {
            extensions: words!["yml", "yaml"],
            highlights: words![tree_sitter_yaml::HIGHLIGHTS_QUERY],
            indentation_is_syntax: true,
            ..grammar("yaml", tree_sitter_yaml::LANGUAGE.into())
        },
        Grammar {
            extensions: words!["zig", "zon"],
            highlights: words![tree_sitter_zig::HIGHLIGHTS_QUERY],
            ..grammar("zig", tree_sitter_zig::LANGUAGE.into())
        },
    ]
}

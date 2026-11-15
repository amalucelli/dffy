//! The bundled grammars and how a file is matched to one.

use gix::bstr::ByteSlice;

/// A bundled grammar; [`Language::detect`] says which files get which.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[expect(
    missing_docs,
    reason = "each variant is the language it names; its files are listed in detect"
)]
pub enum Language {
    Bash,
    C,
    Cpp,
    Css,
    Go,
    Html,
    Java,
    JavaScript,
    Json,
    Lua,
    Markdown,
    Python,
    Ruby,
    Rust,
    Toml,
    Tsx,
    TypeScript,
    Yaml,
}

impl Language {
    /// Every bundled language.
    pub const ALL: [Self; 18] = [
        Self::Bash,
        Self::C,
        Self::Cpp,
        Self::Css,
        Self::Go,
        Self::Html,
        Self::Java,
        Self::JavaScript,
        Self::Json,
        Self::Lua,
        Self::Markdown,
        Self::Python,
        Self::Ruby,
        Self::Rust,
        Self::Toml,
        Self::Tsx,
        Self::TypeScript,
        Self::Yaml,
    ];

    /// Lowercase and stable: it's part of the disk cache's keys, so
    /// changing one orphans that language's cached spans.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::C => "c",
            Self::Cpp => "cpp",
            Self::Css => "css",
            Self::Go => "go",
            Self::Html => "html",
            Self::Java => "java",
            Self::JavaScript => "javascript",
            Self::Json => "json",
            Self::Lua => "lua",
            Self::Markdown => "markdown",
            Self::Python => "python",
            Self::Ruby => "ruby",
            Self::Rust => "rust",
            Self::Toml => "toml",
            Self::Tsx => "tsx",
            Self::TypeScript => "typescript",
            Self::Yaml => "yaml",
        }
    }

    /// Whether a line's indentation is syntax: changing it can move code
    /// into or out of a block with the same tokens in the same order.
    #[must_use]
    pub fn indentation_is_syntax(self) -> bool {
        matches!(self, Self::Python | Self::Yaml | Self::Markdown)
    }

    /// The language of the file at `path` (repository-relative), by file
    /// name, then extension, then a `#!` line at the start of `content`.
    /// `.h` counts as C, the common case in C codebases.
    #[must_use]
    pub fn detect(path: &[u8], content: &[u8]) -> Option<Self> {
        let name = path.rsplit(|&b| b == b'/').next().unwrap_or(path);
        let by_name = match name {
            b"Cargo.lock" | b"Pipfile" | b"poetry.lock" | b"uv.lock" => Some(Self::Toml),
            b"Gemfile" | b"Rakefile" | b"Vagrantfile" | b"Podfile" | b"Brewfile" => {
                Some(Self::Ruby)
            }
            b"PKGBUILD" | b"APKBUILD" | b".bashrc" | b".bash_profile" | b".profile" | b".zshrc"
            | b".zprofile" | b".zshenv" | b"bashrc" | b"zshrc" => Some(Self::Bash),
            _ => None,
        };
        let by_extension = || {
            let (_, ext) = name.rsplit_once_str(".")?;
            Some(match ext {
                b"sh" | b"bash" | b"zsh" | b"ksh" | b"bats" | b"ebuild" => Self::Bash,
                b"c" | b"h" => Self::C,
                b"cc" | b"cpp" | b"cxx" | b"c++" | b"hh" | b"hpp" | b"hxx" | b"h++" | b"ipp"
                | b"tpp" | b"inl" => Self::Cpp,
                b"css" => Self::Css,
                b"go" => Self::Go,
                b"html" | b"htm" | b"xhtml" => Self::Html,
                b"java" => Self::Java,
                b"js" | b"mjs" | b"cjs" | b"jsx" => Self::JavaScript,
                b"json" | b"jsonc" | b"json5" | b"geojson" | b"webmanifest" => Self::Json,
                b"lua" => Self::Lua,
                b"md" | b"markdown" | b"mdx" => Self::Markdown,
                b"py" | b"pyi" | b"pyw" => Self::Python,
                b"rb" | b"rake" | b"gemspec" | b"ru" => Self::Ruby,
                b"rs" => Self::Rust,
                b"toml" => Self::Toml,
                b"tsx" => Self::Tsx,
                b"ts" | b"mts" | b"cts" => Self::TypeScript,
                b"yml" | b"yaml" => Self::Yaml,
                _ => return None,
            })
        };
        by_name
            .or_else(by_extension)
            .or_else(|| from_shebang(content))
    }

    pub(super) fn grammar(self) -> tree_sitter::Language {
        match self {
            Self::Bash => tree_sitter_bash::LANGUAGE.into(),
            Self::C => tree_sitter_c::LANGUAGE.into(),
            Self::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Self::Css => tree_sitter_css::LANGUAGE.into(),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Html => tree_sitter_html::LANGUAGE.into(),
            Self::Java => tree_sitter_java::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::Json => tree_sitter_json::LANGUAGE.into(),
            Self::Lua => tree_sitter_lua::LANGUAGE.into(),
            Self::Markdown => tree_sitter_md::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Toml => tree_sitter_toml_ng::LANGUAGE.into(),
            Self::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Self::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Self::Yaml => tree_sitter_yaml::LANGUAGE.into(),
        }
    }

    /// The highlight query, as the parts to concatenate: a base grammar's
    /// query first, the extending one's after. When two patterns capture the
    /// same node the last one wins (as in tree-sitter's own highlighter), so
    /// the more specific query goes last. TypeScript's `tree-sitter.json`
    /// lists its own query first, which loses JSX tags to JavaScript's
    /// catch-all `(identifier) @variable`.
    pub(crate) fn highlights(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &[tree_sitter_bash::HIGHLIGHT_QUERY],
            Self::C => &[tree_sitter_c::HIGHLIGHT_QUERY],
            Self::Cpp => &[
                tree_sitter_c::HIGHLIGHT_QUERY,
                tree_sitter_cpp::HIGHLIGHT_QUERY,
            ],
            Self::Css => &[tree_sitter_css::HIGHLIGHTS_QUERY],
            Self::Go => &[tree_sitter_go::HIGHLIGHTS_QUERY],
            Self::Html => &[tree_sitter_html::HIGHLIGHTS_QUERY],
            Self::Java => &[tree_sitter_java::HIGHLIGHTS_QUERY],
            Self::JavaScript => &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
            ],
            Self::Json => &[tree_sitter_json::HIGHLIGHTS_QUERY],
            Self::Lua => &[tree_sitter_lua::HIGHLIGHTS_QUERY],
            Self::Markdown => &[tree_sitter_md::HIGHLIGHT_QUERY_BLOCK],
            Self::Python => &[tree_sitter_python::HIGHLIGHTS_QUERY],
            Self::Ruby => &[tree_sitter_ruby::HIGHLIGHTS_QUERY],
            Self::Rust => &[tree_sitter_rust::HIGHLIGHTS_QUERY],
            Self::Toml => &[tree_sitter_toml_ng::HIGHLIGHTS_QUERY],
            Self::Tsx => &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_javascript::JSX_HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
            Self::TypeScript => &[
                tree_sitter_javascript::HIGHLIGHT_QUERY,
                tree_sitter_typescript::HIGHLIGHTS_QUERY,
            ],
            Self::Yaml => &[tree_sitter_yaml::HIGHLIGHTS_QUERY],
        }
    }
}

/// The interpreter named by a `#!` line, skipping `env` and its options.
fn from_shebang(content: &[u8]) -> Option<Language> {
    let line = content.strip_prefix(b"#!")?;
    let line = &line[..line.find_byte(b'\n').unwrap_or(line.len())];
    let mut words = line
        .fields()
        .map(|w| w.rsplit(|&b| b == b'/').next().unwrap_or(w));
    let mut program = words.next()?;
    if program == b"env" {
        program = words.find(|w| !w.starts_with(b"-") && !w.contains(&b'='))?;
    }
    let base = program.trim_end_with(|c| c.is_ascii_digit() || c == '.');
    Some(match base {
        b"sh" | b"bash" | b"zsh" | b"dash" | b"ksh" | b"ash" => Language::Bash,
        b"python" | b"pypy" => Language::Python,
        b"ruby" => Language::Ruby,
        b"node" | b"nodejs" | b"deno" | b"bun" => Language::JavaScript,
        b"lua" | b"luajit" => Language::Lua,
        _ => return None,
    })
}

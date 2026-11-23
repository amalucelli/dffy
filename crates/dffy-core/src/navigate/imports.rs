//! The files a file imports, for the local tier and ranking, in the
//! languages other than C and Rust: each one's way of naming another file
//! of the repository, read from its import lines and mapped onto the
//! side's paths by the language's conventions, not resolved as its
//! compiler would. A package from outside the repository, or one that
//! only a build's configuration can find (path aliases, source sets),
//! names no file and is left out.

use gix::bstr::{BStr, ByteSlice};

use super::{MAX_INCLUDED, Navigator, Related, folder, joined};
use crate::git::Repo;

/// Whether definitions in files of language `of` answer a lookup asked
/// from a file of language `from`: the same language, or one whose files
/// import the other's. TypeScript, TSX and JavaScript import each other,
/// and C++ includes headers, which are C's (`.h`), and that C++ files
/// implement.
pub(super) fn shares_definitions(from: &str, of: &str) -> bool {
    const FAMILIES: [&[&str]; 2] = [&["javascript", "typescript", "tsx"], &["c", "cpp"]];
    from == of
        || FAMILIES
            .iter()
            .any(|f| f.contains(&from) && f.contains(&of))
}

/// Whether a re-export indexed in the file at `path` is one: a Python
/// module's relative imports re-export only in a package's `__init__.py`.
pub(super) fn reexports(path: &BStr) -> bool {
    let name = &path[folder(path).len()..];
    !(name.ends_with(b".py") || name.ends_with(b".pyi")) || name.starts_with(b"__init__.")
}

/// What a script's relative import may leave off: the extensions tried in
/// order, then the folder's index file.
const SCRIPT_EXTENSIONS: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts",
];
const SCRIPT_INDEXES: &[&str] = &["/index.ts", "/index.tsx", "/index.js", "/index.jsx"];

impl Navigator {
    /// The files that are module `name` in `language`, where a module is a
    /// file and no query names it: Python's `name.py` and
    /// `name/__init__.py`.
    pub(super) fn module_files(&self, name: &str, language: &str) -> Vec<u32> {
        if language != "python" {
            return Vec::new();
        }
        let (file, package) = (format!("{name}.py"), format!("{name}/__init__.py"));
        let is = |path: &BStr, end: &str| {
            path.strip_suffix(end.as_bytes())
                .is_some_and(|dir| dir.is_empty() || dir.ends_with(b"/"))
        };
        (self.listed.iter().enumerate())
            .filter(|(_, l)| is(l.path.as_ref(), &file) || is(l.path.as_ref(), &package))
            .map(|(i, _)| super::to_u32(i))
            .collect()
    }

    /// Adds to `related` the files that the files it imports directly
    /// re-export from, as imported too, so a name imported through a
    /// re-export finds its definition: a TypeScript or JavaScript barrel's
    /// `export … from`, a Python package's `__init__.py` imports, a Rust
    /// module's `pub use`.
    pub(super) fn follow_reexports(&self, repo: &Repo, language: &str, related: &mut Related) {
        if !matches!(
            language,
            "javascript" | "typescript" | "tsx" | "python" | "rust"
        ) {
            return;
        }
        let imported: Vec<u32> = (related.files.iter())
            .filter(|&&(_, depth)| depth == 1)
            .map(|&(l, _)| l)
            .collect();
        for l in imported {
            if related.files.len() >= MAX_INCLUDED {
                break;
            }
            let path = self.listed[l as usize].path.clone();
            let path = path.as_ref();
            let Some(text) = self.text(repo, l) else {
                continue;
            };
            let mut out = Found::default();
            match language {
                "javascript" | "typescript" | "tsx" => {
                    for spec in reexported_from(&text) {
                        self.script(path, spec, &mut out);
                    }
                }
                "python" if &path[folder(path).len()..] == b"__init__.py" => {
                    self.python(path, &text, &mut out);
                }
                "rust" => out.files = self.used(path, &public_uses(&text)),
                _ => {}
            }
            for f in out.files {
                if !related.files.iter().any(|&(r, _)| r == f) {
                    related.files.push((f, 1));
                }
            }
        }
    }

    /// The files `text`, the file `from` of language `language`, imports,
    /// in the order its imports name them, up to [`MAX_INCLUDED`]:
    /// - JavaScript, TypeScript and TSX: relative specifiers (`./util`,
    ///   `../lib/index.js`), with the extension or `/index` they leave
    ///   off, and a `.js` written for a TypeScript file.
    /// - Python: modules, relative or from the folders above the file, a
    ///   `src/` or `lib/` among them, as `a/b.py` or `a/b/__init__.py`,
    ///   and in `from a import b` the submodule `b` when it's a file.
    /// - Go: the packages of the module the file is in, by the module
    ///   path of the nearest `go.mod`: every file of the package's folder.
    /// - Java, Kotlin, Scala, C# and PHP: the file of each imported type
    ///   under the folder its package or namespace starts in, worked out
    ///   from where the file's own declared package sits, and every file
    ///   of an imported package's folder (a wildcard, a C# `using`).
    /// - Ruby: `require_relative`, and `require` from a `lib/` above.
    /// - Lua: `require("a.b")` as `a/b.lua` or `a/b/init.lua`.
    /// - Elixir: the modules it aliases, imports, uses or requires, as
    ///   `lib/a/b.ex` under a folder above it.
    /// - Zig: `@import` of a `.zig` file.
    /// - Bash: `source` and `.` of a literal path, relative to the file or
    ///   the root.
    pub(super) fn imported(&self, repo: &Repo, from: u32, language: &str, text: &[u8]) -> Vec<u32> {
        let path = self.listed[from as usize].path.as_ref();
        let mut out = Found::default();
        match language {
            "javascript" | "typescript" | "tsx" => {
                for spec in quoted_after(text, &[b"from", b"import", b"require"]) {
                    self.script(path, spec, &mut out);
                }
            }
            "python" => self.python(path, text, &mut out),
            "go" => self.go(repo, path, text, &mut out),
            "java" => self.packaged(path, text, Packaged::JAVA, &mut out),
            "kotlin" => self.packaged(path, text, Packaged::KOTLIN, &mut out),
            "scala" => self.packaged(path, text, Packaged::SCALA, &mut out),
            "csharp" => self.packaged(path, text, Packaged::CSHARP, &mut out),
            "php" => {
                self.packaged(path, text, Packaged::PHP, &mut out);
                for spec in quoted_after(
                    text,
                    &[b"require", b"require_once", b"include", b"include_once"],
                ) {
                    let spec = spec.strip_prefix(b"/").unwrap_or(spec);
                    out.push(self.relative(path, spec));
                }
            }
            "ruby" => {
                for spec in quoted_after(text, &[b"require_relative"]) {
                    out.push(self.with_extension(folder(path), spec, ".rb"));
                }
                for spec in quoted_after(text, &[b"require"]) {
                    out.push(self.above(path, &[b"lib/", b""], spec, &[".rb"]));
                }
            }
            "lua" => {
                for spec in quoted_after(text, &[b"require"]) {
                    let module = spec.replace(b".", b"/");
                    out.push(self.above(path, &[b"", b"lua/"], &module, &[".lua", "/init.lua"]));
                }
            }
            "elixir" => self.elixir(path, text, &mut out),
            "zig" => {
                for spec in quoted_after(text, &[b"@import"]) {
                    if spec.ends_with(b".zig") {
                        out.push(self.relative(path, spec));
                    }
                }
            }
            "bash" => self.sourced(path, text, &mut out),
            _ => {}
        }
        out.files.retain(|&l| l != from);
        out.files
    }

    /// `spec` relative to the folder of `from`, as written.
    fn relative(&self, from: &BStr, spec: &[u8]) -> Option<u32> {
        self.find(&joined(folder(from), spec)?)
    }

    /// `spec` under `dir`, as written or with `extension` added.
    fn with_extension(&self, dir: &BStr, spec: &[u8], extension: &str) -> Option<u32> {
        let base = joined(dir, spec)?;
        self.find(&base)
            .or_else(|| self.find(&[base.as_bytes(), extension.as_bytes()].concat()))
    }

    /// `module` with one of `endings`, under `bases` of each folder above
    /// `from`, nearest first, then of the root.
    fn above(&self, from: &BStr, bases: &[&[u8]], module: &[u8], endings: &[&str]) -> Option<u32> {
        ancestors(from).find_map(|dir| {
            bases.iter().find_map(|base| {
                let stem = [dir, base, module].concat();
                (endings.iter()).find_map(|e| self.find(&[stem.as_slice(), e.as_bytes()].concat()))
            })
        })
    }

    /// The files directly in folder `dir` (with its trailing `/`, empty for
    /// the root) whose names end with `extension`.
    fn files_in<'a>(&'a self, dir: &'a [u8], extension: &'a str) -> impl Iterator<Item = u32> + 'a {
        let start = self.listed.partition_point(|l| l.path.as_slice() < dir);
        (self.listed[start..].iter().enumerate())
            .take_while(move |(_, l)| l.path.starts_with(dir))
            .filter(move |(_, l)| {
                let name = &l.path[dir.len()..];
                !name.contains(&b'/') && name.ends_with(extension.as_bytes())
            })
            .map(move |(i, _)| super::to_u32(start + i))
    }

    fn script(&self, from: &BStr, spec: &[u8], out: &mut Found) {
        if !(spec.starts_with(b"./") || spec.starts_with(b"../")) {
            return;
        }
        let Some(base) = joined(folder(from), spec) else {
            return;
        };
        let stem = [".js", ".jsx", ".mjs", ".cjs"]
            .iter()
            .find_map(|e| base.strip_suffix(e.as_bytes()));
        let typed = stem.into_iter().flat_map(|stem| {
            [".ts", ".tsx", ".mts", ".cts"].map(|e| [stem, e.as_bytes()].concat())
        });
        let found = std::iter::once(base.to_vec())
            .chain(typed)
            .chain(
                SCRIPT_EXTENSIONS
                    .iter()
                    .map(|e| [base.as_bytes(), e.as_bytes()].concat()),
            )
            .chain(
                SCRIPT_INDEXES
                    .iter()
                    .map(|e| [base.as_bytes(), e.as_bytes()].concat()),
            )
            .find_map(|path| self.find(&path));
        out.push(found);
    }

    fn python(&self, from: &BStr, text: &[u8], out: &mut Found) {
        for line in text.lines() {
            let line = line.trim_start();
            if let Some(rest) = line.strip_prefix(b"import ") {
                for module in rest.split_str(",") {
                    let module = module.fields().next().unwrap_or_default();
                    self.python_module(from, module, out);
                }
            } else if let Some(rest) = line.strip_prefix(b"from ")
                && let Some((module, names)) = rest.split_once_str(" import ")
            {
                let module = module.trim();
                self.python_module(from, module, out);
                let names = names.trim().trim_start_with(|c| c == '(');
                for name in names.split_str(",") {
                    let Some(name) = name.fields().next() else {
                        continue;
                    };
                    if name.iter().all(|&b| super::is_word_byte(b)) {
                        let sub = if module.iter().all(|&b| b == b'.') {
                            [module, name].concat()
                        } else {
                            [module, b".", name].concat()
                        };
                        self.python_module(from, &sub, out);
                    }
                }
            }
        }
    }

    /// Adds the file of Python module `module` imported from `from`.
    fn python_module(&self, from: &BStr, module: &[u8], out: &mut Found) {
        let dots = module.iter().take_while(|&&b| b == b'.').count();
        let path = module[dots..].replace(b".", b"/");
        let endings = [".py", ".pyi", "/__init__.py", "/__init__.pyi"];
        let found = if dots > 0 {
            let mut dir = folder(from).to_owned();
            for _ in 1..dots {
                dir = folder(dir.strip_suffix(b"/").unwrap_or(&dir).as_bstr()).to_owned();
            }
            if path.is_empty() {
                self.find(&[dir.as_bytes(), b"__init__.py"].concat())
            } else {
                (endings.iter())
                    .find_map(|e| self.find(&[dir.as_bytes(), &path, e.as_bytes()].concat()))
            }
        } else if path.is_empty() {
            None
        } else {
            self.above(from, &[b"", b"src/", b"lib/"], &path, &endings)
        };
        out.push(found);
    }

    fn go(&self, repo: &Repo, from: &BStr, text: &[u8], out: &mut Found) {
        let Some((root, module)) = ancestors(from).find_map(|dir| {
            let l = self.find(&[dir, b"go.mod"].concat())?;
            let text = self.text(repo, l)?;
            let module = text.lines().find_map(|line| {
                let rest = line.trim().strip_prefix(b"module")?;
                let module = rest.trim().trim_with(|c| c == '"');
                (!module.is_empty()).then(|| module.to_vec())
            })?;
            Some((dir.to_vec(), module))
        }) else {
            return;
        };
        for import in go_imports(text) {
            let Some(rest) = import.strip_prefix(module.as_slice()) else {
                continue;
            };
            let dir = match rest.strip_prefix(b"/") {
                Some(rest) => [root.as_slice(), rest, b"/"].concat(),
                None if rest.is_empty() => root.clone(),
                None => continue,
            };
            for l in self.files_in(&dir, ".go") {
                if !self.listed[l as usize].path.ends_with(b"_test.go") {
                    out.push(Some(l));
                }
            }
        }
    }

    /// For a language whose files declare a package or namespace and
    /// import others by it.
    fn packaged(&self, from: &BStr, text: &[u8], lang: &Packaged, out: &mut Found) {
        let split = |name: &[u8]| -> Vec<Vec<u8>> {
            (name.split_str(lang.separator))
                .map(|s| s.trim().to_vec())
                .filter(|s| !s.is_empty())
                .collect()
        };
        let declared = text.lines().find_map(|line| {
            let rest = line.trim_start().strip_prefix(lang.package)?;
            let rest = rest.strip_prefix(b" ")?;
            let name = rest.split(|&b| matches!(b, b';' | b'{')).next()?;
            Some(split(name.trim()))
        });
        let Some(package) = declared else {
            return;
        };
        // The folder the file's package starts in, and the leading
        // segments of the package that no folder spells.
        let dir: Vec<&[u8]> = folder(from)
            .split(|&b| b == b'/')
            .filter(|s| !s.is_empty())
            .collect();
        let matched = (0..=package.len().min(dir.len()))
            .rev()
            .find(|&n| {
                package[package.len() - n..]
                    .iter()
                    .zip(&dir[dir.len() - n..])
                    .all(|(a, b)| a.as_slice() == *b)
            })
            .unwrap_or(0);
        let unspelled = &package[..package.len() - matched];
        let root: Vec<u8> = dir[..dir.len() - matched]
            .iter()
            .flat_map(|s| [*s, b"/"].concat())
            .collect();
        for line in text.lines() {
            let Some(rest) = line.trim_start().strip_prefix(lang.import) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(b" ") else {
                continue;
            };
            let mut rest = rest.trim_start();
            for word in lang.skip {
                if let Some(after) = rest.strip_prefix(*word) {
                    rest = after.trim_start();
                }
            }
            // C#'s `using Alias = A.B;`.
            if let Some((_, target)) = rest.split_once_str("=")
                && lang.import == b"using"
            {
                rest = target.trim_start();
            }
            for name in expand_group(import_name(rest), lang.separator) {
                let mut segments = split(&name);
                if !segments.starts_with(unspelled) {
                    continue;
                }
                segments.drain(..unspelled.len());
                let wildcard = segments.last().is_some_and(|s| s == b"*" || s == b"_");
                if wildcard {
                    segments.pop();
                }
                let joined_dir = |segments: &[Vec<u8>]| -> Vec<u8> {
                    let mut dir = root.clone();
                    for s in segments {
                        dir.extend_from_slice(s);
                        dir.push(b'/');
                    }
                    dir
                };
                let package = if !wildcard && lang.types {
                    // The type's file, or its outer type's for a member or
                    // a nested type.
                    let file = (1..=segments.len()).rev().take(2).find_map(|n| {
                        let dir = joined_dir(&segments[..n - 1]);
                        let name = &segments[n - 1];
                        self.find(&[dir.as_slice(), name, lang.extension.as_bytes()].concat())
                    });
                    if file.is_some() {
                        out.push(file);
                        continue;
                    }
                    // A Kotlin function or a PHP function at the package's
                    // top level, in any of its files.
                    &segments[..segments.len().saturating_sub(1)]
                } else {
                    &segments[..]
                };
                for l in self.files_in(&joined_dir(package), lang.extension) {
                    out.push(Some(l));
                }
            }
        }
    }

    fn elixir(&self, from: &BStr, text: &[u8], out: &mut Found) {
        for line in text.lines() {
            let line = line.trim_start();
            let Some(rest) = [&b"alias "[..], b"import ", b"use ", b"require "]
                .iter()
                .find_map(|w| line.strip_prefix(*w))
            else {
                continue;
            };
            for module in expand_group(import_name(rest), ".") {
                let path: Vec<u8> = (module.split_str("."))
                    .map(snake_case)
                    .collect::<Vec<_>>()
                    .join(&b'/');
                out.push(self.above(from, &[b"lib/"], &path, &[".ex"]));
            }
        }
    }

    fn sourced(&self, from: &BStr, text: &[u8], out: &mut Found) {
        for line in text.lines() {
            let line = line.trim_start();
            let Some(rest) = line
                .strip_prefix(b"source ")
                .or_else(|| line.strip_prefix(b". "))
            else {
                continue;
            };
            let word: Vec<u8> = (rest.iter())
                .take_while(|&&b| !matches!(b, b';' | b'#' | b'\r'))
                .filter(|&&b| b != b'"' && b != b'\'')
                .copied()
                .collect();
            let word = word.trim();
            // `$(dirname $0)/x.sh`, `${BASH_SOURCE%/*}/x.sh`, `$DIR/x.sh`:
            // the script's own folder.
            let spec = match word.strip_prefix(b"$") {
                Some(rest) => {
                    let close = match rest.first() {
                        Some(b'(') => rest.find_byte(b')'),
                        Some(b'{') => rest.find_byte(b'}'),
                        _ => Some(rest.iter().take_while(|&&b| super::is_word_byte(b)).count()),
                    };
                    match close
                        .map(|c| &rest[c..])
                        .and_then(|r| r.find_byte(b'/').map(|s| &r[s + 1..]))
                    {
                        Some(spec) => spec,
                        None => continue,
                    }
                }
                None => word,
            };
            if spec.contains(&b' ') {
                continue;
            }
            out.push(self.relative(from, spec).or_else(|| self.find(spec)));
        }
    }
}

/// The files found, each once, in order, up to [`MAX_INCLUDED`].
#[derive(Default)]
struct Found {
    files: Vec<u32>,
}

impl Found {
    fn push(&mut self, file: Option<u32>) {
        if let Some(l) = file
            && self.files.len() < MAX_INCLUDED
            && !self.files.contains(&l)
        {
            self.files.push(l);
        }
    }
}

/// How a language declares its package and imports others.
struct Packaged {
    package: &'static [u8],
    import: &'static [u8],
    /// Words between the import keyword and the name.
    skip: &'static [&'static [u8]],
    separator: &'static str,
    extension: &'static str,
    /// An import names a type, whose file is named after it; else only
    /// a package, whose folder's files count.
    types: bool,
}

impl Packaged {
    const JAVA: &Self = &Self {
        package: b"package",
        import: b"import",
        skip: &[b"static "],
        separator: ".",
        extension: ".java",
        types: true,
    };
    const KOTLIN: &Self = &Self {
        extension: ".kt",
        skip: &[],
        ..*Self::JAVA
    };
    const SCALA: &Self = &Self {
        extension: ".scala",
        skip: &[],
        ..*Self::JAVA
    };
    const CSHARP: &Self = &Self {
        package: b"namespace",
        import: b"using",
        skip: &[b"static "],
        separator: ".",
        extension: ".cs",
        types: false,
    };
    const PHP: &Self = &Self {
        package: b"namespace",
        import: b"use",
        skip: &[b"function ", b"const "],
        separator: "\\",
        extension: ".php",
        types: true,
    };
}

/// Each folder above `path`, nearest first, ending with the root's empty
/// one; each with its trailing `/`.
fn ancestors(path: &BStr) -> impl Iterator<Item = &[u8]> {
    let mut dir = Some(folder(path));
    std::iter::from_fn(move || {
        let here = dir?;
        dir = (!here.is_empty()).then(|| folder(here[..here.len() - 1].as_bstr()));
        Some(here.as_bytes())
    })
}

/// The string literal right after each of `words` in `text`, past spaces
/// and an opening parenthesis: `from "./x"`, `require('x')`,
/// `@import("x.zig")`. A word counts only where it isn't part of another.
fn quoted_after<'t>(text: &'t [u8], words: &[&[u8]]) -> Vec<&'t [u8]> {
    let mut out = Vec::new();
    for word in words {
        for at in memchr::memmem::find_iter(text, word) {
            let end = at + word.len();
            let part_of_word = |b: u8| super::is_word_byte(b) || b == b'@';
            if (at > 0 && part_of_word(text[at - 1]))
                || text.get(end).is_some_and(|&b| super::is_word_byte(b))
            {
                continue;
            }
            let rest = text[end..].trim_start_with(|c| c == ' ' || c == '\t' || c == '(');
            // PHP's `require __DIR__ . '/x.php'`.
            let rest = (rest.strip_prefix(b"__DIR__"))
                .map_or(rest, |r| r.trim_start_with(|c| c == ' ' || c == '.'));
            let Some(&quote @ (b'"' | b'\'' | b'`')) = rest.first() else {
                continue;
            };
            if let Some(close) = rest[1..].find_byte(quote) {
                out.push(&rest[1..=close]);
            }
        }
    }
    out
}

/// The specifiers of the `export … from "…"` statements of script
/// `text`, a statement read to its `;`, or to the next line starting
/// another `export` or an `import` when it has none.
fn reexported_from(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    for at in memchr::memmem::find_iter(text, b"export") {
        let starts = at == 0 || matches!(text[at - 1], b'\n' | b';' | b' ' | b'\t');
        if !starts || text.get(at + 6).is_some_and(|&b| super::is_word_byte(b)) {
            continue;
        }
        let rest = &text[at + 6..];
        let end = [&b";"[..], b"\nexport", b"\nimport"]
            .iter()
            .filter_map(|stop| memchr::memmem::find(rest, stop))
            .min()
            .unwrap_or(rest.len());
        out.extend(quoted_after(&rest[..end], &[b"from"]));
    }
    out
}

/// The `pub use` declarations of Rust source `text`, of any visibility
/// but private, written again as plain `use` ones.
fn public_uses(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = memchr::memmem::find(rest, b"pub") {
        let after = &rest[at + 3..];
        let starts = at == 0 || rest[at - 1].is_ascii_whitespace();
        rest = after;
        if !starts {
            continue;
        }
        // `pub(crate)`, `pub(super)`, `pub(in path)`.
        let after = match after.strip_prefix(b"(") {
            Some(inner) => match inner.find_byte(b')') {
                Some(close) => &inner[close + 1..],
                None => continue,
            },
            None => after,
        };
        let Some(tree) = after.trim_start().strip_prefix(b"use ") else {
            continue;
        };
        let Some(end) = tree.find_byte(b';') else {
            break;
        };
        out.extend_from_slice(b"use ");
        out.extend_from_slice(&tree[..=end]);
        out.push(b'\n');
    }
    out
}

/// The paths of Go's `import "p"`, `import name "p"` and the lines of an
/// `import ( … )` block.
fn go_imports(text: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut block = false;
    for line in text.lines() {
        let line = line.trim();
        let rest = if block {
            if line.starts_with(b")") {
                block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix(b"import") {
            let rest = rest.trim_start();
            if rest.starts_with(b"(") {
                block = true;
                &rest[1..]
            } else {
                rest
            }
        } else {
            continue;
        };
        if let Some(open) = rest.find_byte(b'"')
            && let Some(close) = rest[open + 1..].find_byte(b'"')
        {
            out.push(&rest[open + 1..open + 1 + close]);
        }
    }
    out
}

/// The name an import line starts with, a group's braces included, up to
/// a `;`, a space or a `,`.
fn import_name(rest: &[u8]) -> &[u8] {
    if let Some(open) = rest.find_byte(b'{') {
        let close = rest[open..]
            .find_byte(b'}')
            .map_or(rest.len(), |c| open + c + 1);
        if !rest[..open].contains(&b' ') {
            return &rest[..close];
        }
    }
    let end = (rest.iter())
        .position(|&b| matches!(b, b';' | b' ' | b',' | b'\t' | b'\r'))
        .unwrap_or(rest.len());
    &rest[..end]
}

/// The names a grouped import stands for: Scala's `a.b.{C, D => E}`,
/// PHP's `A\{B, C}` and Elixir's `A.{B, C}` are `a.b.C` and `a.b.D`; any
/// other name is itself.
fn expand_group(name: &[u8], separator: &str) -> Vec<Vec<u8>> {
    let Some(open) = name.find_byte(b'{') else {
        return vec![name.to_vec()];
    };
    let prefix = &name[..open];
    let inner = name[open + 1..]
        .split(|&b| b == b'}')
        .next()
        .unwrap_or_default();
    (inner.split_str(","))
        .filter_map(|item| {
            let item = item.fields().next()?;
            let mut full = prefix.to_vec();
            if !full.is_empty() && !full.ends_with(separator.as_bytes()) {
                full.extend_from_slice(separator.as_bytes());
            }
            full.extend_from_slice(item);
            Some(full)
        })
        .collect()
}

/// An Elixir module's segment as its file names it: `HTTPClient` is
/// `http_client`, `MyApp` is `my_app`.
fn snake_case(segment: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(segment.len() + 4);
    for (i, &b) in segment.iter().enumerate() {
        let upper = b.is_ascii_uppercase();
        let lower_before = i > 0 && segment[i - 1].is_ascii_lowercase();
        let lower_after = segment.get(i + 1).is_some_and(u8::is_ascii_lowercase);
        let upper_before = i > 0 && segment[i - 1].is_ascii_uppercase();
        if upper && i > 0 && (lower_before || (upper_before && lower_after)) {
            out.push(b'_');
        }
        out.push(b.to_ascii_lowercase());
    }
    out
}

#[cfg(test)]
mod tests;

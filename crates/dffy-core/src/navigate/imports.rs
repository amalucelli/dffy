//! The files a file imports, for the local tier and ranking, in the
//! languages other than C and Rust: each one's way of naming another file
//! of the repository, read from its import lines and mapped onto the
//! side's paths by the language's conventions, not resolved as its
//! compiler would. A package from outside the repository, or one that
//! only a build's configuration can find (path aliases, source sets),
//! names no file and is left out.

use gix::bstr::{BStr, BString, ByteSlice};

use crate::highlight::{DefinitionKind, MAX_TAGS_BYTES};

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

/// How many names a file's imports are read for: past what code has, and a
/// bound on what a file of nothing else costs.
const MAX_IMPORTS: usize = 2000;

/// How far a script's import or export statement is read, and how far its
/// line is before a `require`.
const STATEMENT_BYTES: usize = 16 << 10;

/// A name a file brings in by an import, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Imported {
    /// The name the file uses it by.
    pub(super) name: String,
    /// The name it has where it's defined, `default` for a JavaScript
    /// default import; `None` for a module or a package itself (`import *
    /// as ns`, Python's `import a as b`, a Go package), whose members are
    /// looked up in `files`.
    pub(super) original: Option<String>,
    /// None for an import from outside the repository, or one not found.
    pub(super) files: Vec<u32>,
    /// The line of the import, 0-based.
    pub(super) line: usize,
    /// From outside the repository for certain: a Go package outside the
    /// module, a JavaScript package.
    pub(super) external: bool,
}

impl Navigator {
    /// The names `text`, the file `from` of `language`, imports, each with
    /// the files it comes from and its name there:
    /// - JavaScript, TypeScript and TSX: `import x`, `import { a as b }`,
    ///   `import * as ns` and `require`, of relative specifiers.
    /// - Python: `import a.b as c` and `from a import b as c`, `b` a
    ///   submodule or a name of `a`.
    /// - Rust: `use` paths in the repository, `as` renames included, each
    ///   to an item of a module, or to a module itself.
    /// - Go: packages, by their alias or last segment, the module's own
    ///   with their files.
    /// - Java, Kotlin and C#: imported types, a Kotlin `as` rename and a
    ///   C# `using X = A.B` included.
    ///
    /// Up to [`MAX_IMPORTS`] of them, and none of a file larger than
    /// definitions are read of.
    pub(super) fn imported_names(
        &self,
        repo: &Repo,
        from: u32,
        language: &str,
        text: &[u8],
    ) -> Vec<Imported> {
        if text.len() > MAX_TAGS_BYTES {
            return Vec::new();
        }
        let path = self.listed[from as usize].path.clone();
        let path = path.as_ref();
        let mut out = Vec::new();
        match language {
            "javascript" | "typescript" | "tsx" => self.script_names(path, text, &mut out),
            "python" => self.python_names(path, text, &mut out),
            "rust" => self.rust_names(repo, path, text, &mut out),
            "go" => self.go_names(repo, path, text, &mut out),
            "java" => self.packaged_names(path, text, Packaged::JAVA, &mut out),
            "kotlin" => self.packaged_names(path, text, Packaged::KOTLIN, &mut out),
            "csharp" => self.packaged_names(path, text, Packaged::CSHARP, &mut out),
            _ => {}
        }
        out.truncate(MAX_IMPORTS);
        out.retain(|i| !i.name.is_empty() && i.name.bytes().all(super::is_word_byte));
        let first = first_import(text);
        for i in &mut out {
            i.line = word_line(text, first, i.name.as_bytes());
        }
        out
    }

    fn script_names(&self, from: &BStr, text: &[u8], out: &mut Vec<Imported>) {
        let word = |b: &[u8]| -> String { String::from_utf8_lossy(b.trim()).into_owned() };
        for at in memchr::memmem::find_iter(text, b"import") {
            if out.len() >= MAX_IMPORTS {
                return;
            }
            let starts = at == 0 || matches!(text[at - 1], b'\n' | b';' | b' ' | b'\t');
            if !starts || text.get(at + 6).is_some_and(|&b| super::is_word_byte(b)) {
                continue;
            }
            let statement = statement(&text[at + 6..]);
            let Some(from_at) = memchr::memmem::rfind(statement, b"from") else {
                continue;
            };
            let Some(spec) = quoted_after(&statement[from_at..], &[b"from"])
                .first()
                .copied()
            else {
                continue;
            };
            let mut found = Found::default();
            self.script(from, spec, &mut found);
            let clause = statement[..from_at].trim();
            let clause = clause.strip_prefix(b"type ").unwrap_or(clause);
            script_clause(clause, &found.files, is_package(spec), word, out);
        }
        // `const x = require("./m")`, `const { a: b } = require("./m")`.
        for at in memchr::memmem::find_iter(text, b"require(") {
            if out.len() >= MAX_IMPORTS {
                return;
            }
            let line = &text[at.saturating_sub(STATEMENT_BYTES)..at];
            let left = &line[memchr::memrchr(b'\n', line).map_or(0, |i| i + 1)..];
            let Some(eq) = memchr::memrchr(b'=', left) else {
                continue;
            };
            let lhs = left[..eq].trim();
            let lhs = ["const ", "let ", "var "]
                .iter()
                .find_map(|k| lhs.strip_prefix(k.as_bytes()))
                .unwrap_or(lhs);
            let call = statement(&text[at..]);
            let Some(spec) = quoted_after(call, &[b"require"]).first().copied() else {
                continue;
            };
            let mut found = Found::default();
            self.script(from, spec, &mut found);
            let external = is_package(spec);
            match lhs.strip_prefix(b"{") {
                Some(inner) => {
                    let inner = inner.strip_suffix(b"}").unwrap_or(inner);
                    for item in inner.split_str(",") {
                        let (original, name) = match item.split_once_str(":") {
                            Some((a, b)) => (word(a), word(b)),
                            None => (word(item), word(item)),
                        };
                        out.push(Imported {
                            name,
                            original: Some(original),
                            files: found.files.clone(),
                            line: 0,
                            external,
                        });
                    }
                }
                None => out.push(Imported {
                    name: word(lhs),
                    original: None,
                    files: found.files.clone(),
                    line: 0,
                    external,
                }),
            }
        }
    }

    fn python_names(&self, from: &BStr, text: &[u8], out: &mut Vec<Imported>) {
        let word = |b: &[u8]| -> String { String::from_utf8_lossy(b.trim()).into_owned() };
        let module_files = |module: &[u8]| {
            let mut found = Found::default();
            self.python_module(from, module, &mut found);
            found.files
        };
        let lines: Vec<&[u8]> = text.lines().collect();
        let mut i = 0;
        while i < lines.len() {
            let first = lines[i].trim_start();
            i += 1;
            if !(first.starts_with(b"import ") || first.starts_with(b"from ")) {
                continue;
            }
            if out.len() >= MAX_IMPORTS {
                return;
            }
            let uncommented = |line: &[u8]| -> Vec<u8> {
                line[..line.find_byte(b'#').unwrap_or(line.len())].to_vec()
            };
            let mut line = uncommented(first);
            // A parenthesized list over lines.
            if line.contains(&b'(') {
                while i < lines.len() && !line.contains(&b')') && line.len() < STATEMENT_BYTES {
                    line.extend_from_slice(b" ");
                    line.extend_from_slice(&uncommented(lines[i].trim()));
                    i += 1;
                }
            }
            let line: Vec<u8> = line
                .into_iter()
                .filter(|&b| b != b'(' && b != b')')
                .collect();
            if let Some(rest) = line.strip_prefix(b"import ") {
                for item in rest.split_str(",") {
                    let mut parts = item.fields();
                    let Some(module) = parts.next() else {
                        continue;
                    };
                    let (name, files) =
                        if let (Some(b"as"), Some(alias)) = (parts.next(), parts.next()) {
                            (word(alias), module_files(module))
                        } else {
                            let first = module.split(|&b| b == b'.').next().unwrap_or(module);
                            (word(first), module_files(first))
                        };
                    out.push(Imported {
                        name,
                        original: None,
                        files,
                        line: 0,
                        external: false,
                    });
                }
            } else if let Some(rest) = line.strip_prefix(b"from ")
                && let Some((module, names)) = rest.split_once_str(" import ")
            {
                let module = module.trim();
                let files = module_files(module);
                for item in names.split_str(",") {
                    let mut parts = item.fields();
                    let Some(original) = parts.next() else {
                        continue;
                    };
                    if original == b"*" {
                        continue;
                    }
                    let name = match (parts.next(), parts.next()) {
                        (Some(b"as"), Some(alias)) => word(alias),
                        _ => word(original),
                    };
                    let sub = if module.iter().all(|&b| b == b'.') {
                        [module, original].concat()
                    } else {
                        [module, b".", original].concat()
                    };
                    let submodule = module_files(&sub);
                    out.push(if submodule.is_empty() {
                        Imported {
                            name,
                            original: Some(word(original)),
                            files: files.clone(),
                            line: 0,
                            external: false,
                        }
                    } else {
                        Imported {
                            name,
                            original: None,
                            files: submodule,
                            line: 0,
                            external: false,
                        }
                    });
                }
            }
        }
    }

    fn rust_names(&self, repo: &Repo, from: &BStr, text: &[u8], out: &mut Vec<Imported>) {
        let word = |b: &[u8]| -> String { String::from_utf8_lossy(b).into_owned() };
        // A test's, an example's or a bench's `use` of its own crate, by
        // its package's name.
        let own = (self.crate_of(from))
            .and_then(|krate| self.find(&[krate.as_bytes(), b"Cargo.toml"].concat()))
            .and_then(|l| package_name(&self.text(repo, l)?));
        for (mut segments, alias) in aliased_uses(text) {
            if out.len() >= MAX_IMPORTS {
                return;
            }
            if let Some(own) = &own
                && segments.len() > 1
                && segments[0] == own.as_slice()
            {
                segments[0] = "crate".into();
            }
            let Some((last, parent)) = segments.split_last() else {
                continue;
            };
            if matches!(last.as_slice(), b"self" | b"*") {
                continue;
            }
            let files = |segments: &[BString]| {
                let path: Vec<&[u8]> = segments.iter().map(|s| s.as_slice()).collect();
                self.used(from, &[b"use ", &path.join(&b"::"[..])[..], b";"].concat())
            };
            let whole = files(&segments);
            let above = if parent.is_empty() {
                Vec::new()
            } else {
                files(parent)
            };
            let name = word(alias.as_deref().unwrap_or(last));
            out.push(if !whole.is_empty() && whole != above {
                Imported {
                    name,
                    original: None,
                    files: whole,
                    line: 0,
                    external: false,
                }
            } else {
                Imported {
                    name,
                    original: Some(word(last)),
                    files: if above.is_empty() { whole } else { above },
                    line: 0,
                    external: false,
                }
            });
        }
    }

    fn go_names(&self, repo: &Repo, from: &BStr, text: &[u8], out: &mut Vec<Imported>) {
        let module = self.go_module(repo, from);
        for (alias, import) in go_aliased_imports(text) {
            let dir = module.as_ref().and_then(|(root, module)| {
                match import.strip_prefix(module.as_slice())?.strip_prefix(b"/") {
                    Some(rest) => Some([root.as_slice(), rest, b"/"].concat()),
                    None => (import == module.as_slice()).then(|| root.clone()),
                }
            });
            let external = dir.is_none();
            let files: Vec<u32> = (dir.iter())
                .flat_map(|dir| self.files_in(dir, ".go"))
                .filter(|&l| !self.listed[l as usize].path.ends_with(b"_test.go"))
                .collect();
            let name = alias.unwrap_or_else(|| {
                let last = import.rsplit(|&b| b == b'/').next().unwrap_or(import);
                last.to_vec()
            });
            out.push(Imported {
                name: String::from_utf8_lossy(&name).into_owned(),
                original: None,
                files,
                line: 0,
                external,
            });
        }
    }

    fn packaged_names(&self, from: &BStr, text: &[u8], lang: &Packaged, out: &mut Vec<Imported>) {
        let declared = text
            .lines()
            .find(|l| l.trim_start().starts_with(lang.package));
        let Some(declared) = declared else {
            return;
        };
        for line in text.lines() {
            let Some(rest) = line.trim_start().strip_prefix(lang.import) else {
                continue;
            };
            let Some(rest) = rest.strip_prefix(b" ") else {
                continue;
            };
            // C#'s `using X = A.B;` names a type; its other `using`s only
            // namespaces.
            let (alias, rest) = match rest.split_once_str("=") {
                Some((alias, path)) if !lang.types => (Some(alias.trim()), path.trim()),
                _ if !lang.types => continue,
                _ => (None, rest),
            };
            let mut parts = rest.fields();
            let Some(path) = parts.next() else {
                continue;
            };
            let path = path.strip_suffix(b";").unwrap_or(path);
            let (namespace, last) = match path.rsplit_once_str(".") {
                Some((namespace, last)) => (namespace, last),
                None => (&b""[..], path),
            };
            if last == b"*" {
                continue;
            }
            let mut found = Found::default();
            if lang.types {
                self.packaged(from, &[declared, b"\n", line].concat(), lang, &mut found);
            } else {
                // The type's file, by its name, among its namespace's.
                let using = [lang.import, b" ", namespace, b";"].concat();
                self.packaged(from, &[declared, b"\n", &using].concat(), lang, &mut found);
                let file = [last, lang.extension.as_bytes()].concat();
                (found.files).retain(|&l| self.listed[l as usize].path.ends_with(&file));
            }
            let alias = alias.unwrap_or_else(|| match (parts.next(), parts.next()) {
                (Some(b"as"), Some(alias)) => alias.strip_suffix(b";").unwrap_or(alias),
                _ => last,
            });
            out.push(Imported {
                name: String::from_utf8_lossy(alias).into_owned(),
                original: Some(String::from_utf8_lossy(last).into_owned()),
                files: found.files,
                line: 0,
                external: false,
            });
        }
    }

    /// The root folder and module path of the `go.mod` nearest above
    /// `from`.
    fn go_module(&self, repo: &Repo, from: &BStr) -> Option<(Vec<u8>, Vec<u8>)> {
        ancestors(from).find_map(|dir| {
            let l = self.find(&[dir, b"go.mod"].concat())?;
            let text = self.text(repo, l)?;
            let module = text.lines().find_map(|line| {
                let rest = line.trim().strip_prefix(b"module")?;
                let module = rest.trim().trim_with(|c| c == '"');
                (!module.is_empty()).then(|| module.to_vec())
            })?;
            Some((dir.to_vec(), module))
        })
    }

    /// The files the file `l` of `language` re-exports from: a barrel's
    /// `export … from`, an `__init__.py`'s imports, a module's `pub use`.
    pub(super) fn reexport_sources(&self, repo: &Repo, l: u32, language: &str) -> Vec<u32> {
        let path = self.listed[l as usize].path.clone();
        let path = path.as_ref();
        let Some(text) = self.text(repo, l) else {
            return Vec::new();
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
        out.files
    }
}

/// Reads an import clause (what's between `import` and `from`) into `out`,
/// each name from `files`.
fn script_clause(
    clause: &[u8],
    files: &[u32],
    external: bool,
    word: impl Fn(&[u8]) -> String,
    out: &mut Vec<Imported>,
) {
    let push = |out: &mut Vec<Imported>, name: String, original: Option<String>| {
        out.push(Imported {
            name,
            original,
            files: files.to_vec(),
            line: 0,
            external,
        });
    };
    let (outside, braces) = match (clause.find_byte(b'{'), clause.find_byte(b'}')) {
        (Some(open), Some(close)) if open < close => (
            [&clause[..open], &clause[close + 1..]].concat(),
            Some(clause[open + 1..close].to_vec()),
        ),
        _ => (clause.to_vec(), None),
    };
    for part in outside.split_str(",") {
        let part = part.trim();
        if let Some(ns) = part.strip_prefix(b"*") {
            let ns = ns.trim().strip_prefix(b"as").unwrap_or(ns);
            push(out, word(ns), None);
        } else if !part.is_empty() {
            push(out, word(part), Some("default".to_owned()));
        }
    }
    for item in braces.iter().flat_map(|b| b.split_str(",")) {
        let item = item.trim();
        let item = item.strip_prefix(b"type ").unwrap_or(item);
        let (original, name) = match item.split_once_str(" as ") {
            Some((a, b)) => (word(a), word(b)),
            None => (word(item), word(item)),
        };
        if !name.is_empty() {
            push(out, name, Some(original));
        }
    }
}

/// Where the first line that starts an import is in `text`: before it a
/// file's header or doc comment may name what it imports.
fn first_import(text: &[u8]) -> usize {
    const STARTS: &[&[u8]] = &[
        b"import",
        b"from ",
        b"use ",
        b"pub use ",
        b"pub(crate) use ",
        b"using ",
        b"const ",
        b"let ",
        b"var ",
    ];
    let mut at = 0;
    for line in text.lines_with_terminator() {
        if STARTS.iter().any(|s| line.trim_start().starts_with(s)) {
            return at;
        }
        at += line.len();
    }
    0
}

/// The line, 0-based, of the first use of `word` as a whole word in `text`
/// at or past byte `from`: an import's own line, imports coming before
/// what uses them.
pub(super) fn word_line(text: &[u8], from: usize, word: &[u8]) -> usize {
    let whole = |i: usize| {
        (i == 0 || !super::is_word_byte(text[i - 1]))
            && text
                .get(i + word.len())
                .is_none_or(|&b| !super::is_word_byte(b))
    };
    let at = (memchr::memmem::find_iter(&text[from..], word))
        .map(|i| from + i)
        .find(|&i| whole(i))
        .unwrap_or(from);
    memchr::memchr_iter(b'\n', &text[..at]).count()
}

/// Where `text`'s default export is: the line of its `export default`,
/// and the name it exports, or for one with none the kind it is (an
/// anonymous function or class, else a value).
pub(super) fn default_export(text: &[u8]) -> Option<(usize, Result<String, DefinitionKind>)> {
    let at = memchr::memmem::find_iter(text, b"export default")
        .find(|&i| i == 0 || matches!(text[i - 1], b'\n' | b';'))?;
    let line = memchr::memchr_iter(b'\n', &text[..at]).count();
    let words = text[at + 14..].fields().map(|w| {
        let end = w
            .iter()
            .position(|&b| !super::is_word_byte(b))
            .unwrap_or(w.len());
        &w[..end]
    });
    let mut kind = DefinitionKind::Variable;
    for w in words {
        match w {
            b"async" | b"abstract" => {}
            b"function" => kind = DefinitionKind::Function,
            b"class" => kind = DefinitionKind::Class,
            b"" => break,
            name => return Some((line, Ok(String::from_utf8_lossy(name).into_owned()))),
        }
    }
    Some((line, Err(kind)))
}

/// Whether JavaScript import specifier `spec` names a package: not a
/// relative or absolute path, nor one of the aliases a bundler or a
/// `tsconfig.json` maps into the repository (`@/`, `~/`, `#`); an alias
/// of another form is taken for a package.
fn is_package(spec: &[u8]) -> bool {
    !(spec.starts_with(b".")
        || spec.starts_with(b"/")
        || spec.starts_with(b"@/")
        || spec.starts_with(b"~")
        || spec.starts_with(b"#"))
}

/// The `[package]` name of Cargo manifest `text`, as a path names it.
fn package_name(text: &[u8]) -> Option<BString> {
    let mut package = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(b"[") {
            package = line == b"[package]";
        } else if package
            && let Some(rest) = line.strip_prefix(b"name")
            && let Some(value) = rest.trim_start().strip_prefix(b"=")
        {
            let name = value.trim().trim_with(|c| c == '"' || c == '\'');
            return Some(name.replace(b"-", b"_").into());
        }
    }
    None
}

/// The paths of the `use` declarations of Rust source `text`, each with
/// the name an `as` gives it.
fn aliased_uses(text: &[u8]) -> Vec<(Vec<BString>, Option<BString>)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(at) = memchr::memmem::find(rest, b"use ") {
        let starts = at == 0 || matches!(rest[at - 1], b'\n' | b' ' | b'\t' | b')' | b';' | b'{');
        rest = &rest[at + 4..];
        if !starts {
            continue;
        }
        let Some(end) = rest.find_byte(b';') else {
            break;
        };
        if out.len() >= MAX_IMPORTS {
            break;
        }
        super::use_paths(&rest[..end], |path, alias| {
            out.push((
                path.iter().map(|&s| s.into()).collect(),
                alias.map(BString::from),
            ));
        });
        rest = &rest[end..];
    }
    out
}

/// Go's imports, each with its alias: `import u "p"`, the lines of an
/// `import ( … )` block; all before the file's first declaration.
fn go_aliased_imports(text: &[u8]) -> Vec<(Option<Vec<u8>>, &[u8])> {
    const DECLARATIONS: &[&[u8]] = &[b"func ", b"type ", b"var ", b"const "];
    let mut out = Vec::new();
    let mut block = false;
    for line in text.lines() {
        if DECLARATIONS.iter().any(|d| line.starts_with(d)) {
            break;
        }
        let line = line.trim();
        let rest = if block {
            if line.starts_with(b")") {
                block = false;
                continue;
            }
            line
        } else if let Some(rest) = line.strip_prefix(b"import") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix(b"(") {
                block = true;
                rest
            } else {
                rest
            }
        } else {
            continue;
        };
        let Some(open) = rest.find_byte(b'"') else {
            continue;
        };
        let Some(close) = rest[open + 1..].find_byte(b'"') else {
            continue;
        };
        let alias = rest[..open].trim();
        let alias = (!alias.is_empty() && alias != b"_" && alias != b".").then(|| alias.to_vec());
        out.push((alias, &rest[open + 1..open + 1 + close]));
    }
    out
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
    pub(super) fn files_in<'a>(
        &'a self,
        dir: &'a [u8],
        extension: &'a str,
    ) -> impl Iterator<Item = u32> + 'a {
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
        out.extend(quoted_after(statement(&text[at + 6..]), &[b"from"]));
        if out.len() >= MAX_IMPORTS {
            break;
        }
    }
    out
}

/// The script statement at the start of `rest`: to its `;`, or to the next
/// line starting an `import` or an `export` when it has none, or
/// [`STATEMENT_BYTES`].
fn statement(rest: &[u8]) -> &[u8] {
    let mut rest = &rest[..rest.len().min(STATEMENT_BYTES)];
    for stop in [&b";"[..], b"\nimport", b"\nexport"] {
        if let Some(end) = memchr::memmem::find(rest, stop) {
            rest = &rest[..end];
        }
    }
    rest
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

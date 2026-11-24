//! What a name's use in the asking file tells about it, over what the
//! index ranks first: the local or the parameter it names there, the
//! import it came in through, followed to the definition, and for a
//! member the type or the module its receiver has. The asking file is
//! parsed once and kept for the next lookups from it.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Instant;

use gix::bstr::{BStr, BString, ByteSlice};
use rustc_hash::{FxHashMap, FxHashSet};
use tree_sitter::Tree;

use super::imports::{Imported, default_export, shares_definitions, word_line};
use super::{Candidate, Navigator, Origin, Rank, Slot, Use, is_word_byte};
use crate::cache::SymbolCache;
use crate::git::Repo;
use crate::highlight::{self, DefinitionKind, Definitions};

/// How many re-exports an import is followed through.
const HOPS: usize = 4;

/// How many asking files' parses are kept.
const KEPT: usize = 4;

/// An asking file's text and syntax tree, at the navigator's side.
pub(super) struct Asking {
    path: BString,
    text: Vec<u8>,
    /// Its language by its text: a `.h` may be C++.
    language: highlight::Language,
    /// Parsed on the first lookup that asks; `None` when it's larger than
    /// definitions are read for or its parse takes too long.
    tree: OnceLock<Option<Tree>>,
    /// What it imports, read on the first lookup that asks.
    imports: OnceLock<Vec<Imported>>,
}

impl Asking {
    fn tree(&self) -> Option<&Tree> {
        (self.tree.get_or_init(|| {
            (self.text.len() <= highlight::MAX_TAGS_BYTES)
                .then(|| {
                    highlight::parse_tree(&self.language, &self.text, super::INLINE_PARSE, None)
                })
                .flatten()
        }))
        .as_ref()
    }
}

impl std::fmt::Debug for Asking {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Asking")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// A lookup being resolved, and when its work past the index's ends.
struct Ask<'a> {
    repo: &'a Repo,
    cache: &'a SymbolCache,
    origin: &'a Origin,
    cancel: &'a AtomicBool,
    deadline: Instant,
}

impl Ask<'_> {
    /// It's cancelled, or its time is up: what's left of what the asking
    /// file says of the name goes unsaid.
    fn stopped(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || Instant::now() >= self.deadline
    }
}

/// The last asking files parsed, most recent last.
#[derive(Debug, Default)]
pub(super) struct Kept(Mutex<Vec<Arc<Asking>>>);

impl Navigator {
    /// Orders `candidates` of `name` used at `at` in the asking file of
    /// `origin`, ranked, by what the file says of the use, last first:
    /// - a local or a parameter it is;
    /// - a Go `package` clause's package; a Rust `name!` a macro, but one
    ///   the file defines only past the use;
    /// - the definition an import of the name comes from;
    /// - for a member, those of the type its receiver has, or of the module
    ///   it is.
    ///
    /// And a Rust `mod x;` found is the module's file.
    /// All of it ends within the local tier's budget, past reading the file,
    /// or once it's cancelled.
    pub(super) fn resolve(
        &self,
        repo: &Repo,
        cache: &SymbolCache,
        (name, at): (&str, Option<Use>),
        origin: &Origin,
        candidates: &mut Vec<Candidate>,
        cancel: &AtomicBool,
    ) {
        // The parse has a budget of its own, and the language's query is
        // compiled once: neither is this lookup's work.
        if at.is_some()
            && !cancel.load(Ordering::Relaxed)
            && let Some(asking) = self.asking(repo, origin)
            && asking.tree().is_some()
        {
            highlight::prepare_locals(&asking.language);
        }
        let ask = &Ask {
            repo,
            cache,
            origin,
            cancel,
            deadline: Instant::now() + self.local_budget,
        };
        let language = origin.language.name();
        self.generated_last(repo, candidates);
        if language == "rust" {
            self.declared_modules(candidates);
        }
        if let Some(at) = at {
            self.by_receiver(ask, (name, at), candidates);
        }
        let imported = self.imported_definitions(ask, name, at);
        for found in imported.into_iter().rev() {
            pin(candidates, found);
        }
        let Some(at) = at else {
            return;
        };
        match language {
            "rust" => self.macros_first(repo, name, origin, at, candidates),
            "go" => {
                if let Some(package) = self.own_package(repo, name, origin, at) {
                    pin(candidates, package);
                }
            }
            _ => {}
        }
        if let Some(local) = self.local(ask, name, at) {
            pin(candidates, local);
        }
    }

    /// Puts generated code after the rest of `candidates` as near, by the
    /// marker generators put at its top, when there are few enough to read.
    fn generated_last(&self, repo: &Repo, candidates: &mut Vec<Candidate>) {
        // Each candidate's file is read: fine for a handful, not for the
        // thousands a common kernel name has.
        const READ: usize = 16;
        const MARKERS: &[&[u8]] = &[b"Code generated", b"@generated", b"DO NOT EDIT"];
        if candidates.len() > READ {
            return;
        }
        let generated = |c: &Candidate| {
            let text = self.find(&c.path).and_then(|l| self.text(repo, l));
            let top = text.as_deref().map_or(&[][..], |t| &t[..t.len().min(1024)]);
            MARKERS
                .iter()
                .any(|m| memchr::memmem::find(top, m).is_some())
        };
        let mut keyed: Vec<(bool, Candidate)> = (candidates.drain(..))
            .map(|c| (kept_out(c.path.as_ref()) || generated(&c), c))
            .collect();
        keyed.sort_by_key(|(out, c)| (c.kind == Some(DefinitionKind::Reexport), c.rank, *out));
        candidates.extend(keyed.into_iter().map(|(_, c)| c));
    }

    /// The folder of the package `path` is in, by the manifest nearest
    /// above it; `known` keeps what's been worked out by folder.
    pub(super) fn package_of(
        &self,
        path: &BStr,
        known: &mut FxHashMap<BString, Option<BString>>,
    ) -> Option<BString> {
        const MANIFESTS: &[&[u8]] = &[
            b"Cargo.toml",
            b"go.mod",
            b"package.json",
            b"pyproject.toml",
            b"setup.py",
            b"pom.xml",
            b"build.gradle",
            b"build.gradle.kts",
        ];
        let (mut dir, mut below) = (super::folder(path), Vec::new());
        let found = loop {
            if let Some(found) = known.get(dir) {
                break found.clone();
            }
            below.push(dir);
            if (MANIFESTS.iter()).any(|m| self.find(&[dir.as_bytes(), m].concat()).is_some()) {
                break Some(dir.to_owned());
            }
            if dir.is_empty() {
                break None;
            }
            dir = super::folder(dir[..dir.len() - 1].as_bstr());
        };
        known.extend((below.into_iter()).map(|dir| (dir.to_owned(), found.clone())));
        found
    }

    /// Each `mod x;` among Rust `candidates` as the file it's in, `x.rs`
    /// or `x/mod.rs`, where there's one (a `#[path]` isn't read).
    fn declared_modules(&self, candidates: &mut [Candidate]) {
        for c in candidates {
            if c.kind != Some(DefinitionKind::Module) || c.start != c.end {
                continue;
            }
            let Some(line) = self.find(&c.path).and_then(|l| {
                let dir = super::module_folder(self.listed[l as usize].path.as_ref());
                let name = self.declared_module(l, c.start)?;
                let file = [dir.as_slice(), name.as_bytes(), b".rs"].concat();
                let module = [dir.as_slice(), name.as_bytes(), b"/mod.rs"].concat();
                self.find(&file).or_else(|| self.find(&module))
            }) else {
                continue;
            };
            let listed = &self.listed[line as usize];
            c.path = listed.path.clone();
            c.side = listed.side;
            (c.start, c.end) = (0, 0);
        }
    }

    /// The name of the `mod` listed file `l` declares on line `line`.
    fn declared_module(&self, l: u32, line: usize) -> Option<String> {
        let defs = self.definitions_of(l)?;
        let d = defs
            .iter()
            .find(|d| d.kind == DefinitionKind::Module && d.start == line && d.end == line)?;
        Some(d.name.to_owned())
    }

    /// Puts a Rust macro first among `candidates` when `name` is called as
    /// one at `at` (`name!`, not `name != …`), but after the others a macro
    /// the asking file defines past the use, which `macro_rules!` doesn't
    /// reach.
    fn macros_first(
        &self,
        repo: &Repo,
        name: &str,
        origin: &Origin,
        at: Use,
        candidates: &mut [Candidate],
    ) {
        let Some(asking) = self.asking(repo, origin) else {
            return;
        };
        let Some(range) = used(&asking.text, name, at) else {
            return;
        };
        let after = &asking.text[range.end..];
        if after.first() != Some(&b'!') || after.get(1) == Some(&b'=') {
            return;
        }
        candidates.sort_by_key(|c| {
            let macro_ = c.kind == Some(DefinitionKind::Macro);
            (
                !macro_,
                macro_ && c.path == origin.path && c.start > at.line,
            )
        });
    }

    /// The package of a Go `package` clause `name` is used in at `at`: its
    /// clause with the package's doc comment.
    fn own_package(&self, repo: &Repo, name: &str, origin: &Origin, at: Use) -> Option<Candidate> {
        let asking = self.asking(repo, origin)?;
        let line = asking.text.lines().nth(at.line)?;
        let rest = line.trim_ascii_start().strip_prefix(b"package ")?;
        if rest.trim_ascii() != name.as_bytes() {
            return None;
        }
        let files: Vec<u32> = (self.files_in(super::folder(origin.path.as_ref()), ".go"))
            .filter(|&l| !self.listed[l as usize].path.ends_with(b"_test.go"))
            .collect();
        self.package_clause(repo, &files, name)
    }

    /// The asking file of `origin`, read on its first lookup: `None` when
    /// it can't be read.
    fn asking(&self, repo: &Repo, origin: &Origin) -> Option<Arc<Asking>> {
        let mut kept = self.kept.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = kept.iter().position(|a| a.path == origin.path) {
            let found = kept.remove(i);
            kept.push(Arc::clone(&found));
            return Some(found);
        }
        drop(kept);
        let l = self.find(&origin.path)?;
        let text = self.text(repo, l)?;
        let asking = Arc::new(Asking {
            path: origin.path.clone(),
            language: origin.language.of_text(&origin.path, &text).clone(),
            text,
            tree: OnceLock::new(),
            imports: OnceLock::new(),
        });
        let mut kept = self.kept.0.lock().unwrap_or_else(PoisonError::into_inner);
        kept.push(Arc::clone(&asking));
        if kept.len() > KEPT {
            kept.remove(0);
        }
        Some(asking)
    }

    /// The candidate the use `at` of `name` in the asking file resolves to
    /// there: a local, a parameter, or a C file-scope variable.
    fn local(&self, ask: &Ask, name: &str, at: Use) -> Option<Candidate> {
        if ask.stopped() {
            return None;
        }
        let origin = ask.origin;
        let asking = self.asking(ask.repo, origin)?;
        let tree = asking.tree()?;
        let range = used(&asking.text, name, at)?;
        let local = highlight::local_at(&asking.language, tree, &asking.text, range, &|| {
            ask.stopped()
        })?;
        let listed = &self.listed[self.find(&origin.path)? as usize];
        Some(Candidate {
            path: listed.path.clone(),
            side: listed.side,
            kind: Some(local.kind),
            start: local.line,
            end: local.line,
            container: None,
            rank: Rank::SameFile,
            changed: false,
            in_hunk: None,
        })
    }
}

impl Navigator {
    /// What the asking file of `origin` imports, by name.
    fn imports_of(&self, repo: &Repo, origin: &Origin) -> Option<Arc<Asking>> {
        let language = origin.language.name();
        if !matches!(
            language,
            "javascript"
                | "typescript"
                | "tsx"
                | "python"
                | "rust"
                | "go"
                | "java"
                | "kotlin"
                | "csharp"
        ) {
            return None;
        }
        let asking = self.asking(repo, origin)?;
        let l = self.find(&origin.path)?;
        (asking.imports).get_or_init(|| self.imported_names(repo, l, language, &asking.text));
        Some(asking)
    }

    /// The definitions `name`, used at `at`, names when the asking file of
    /// `origin` imports it: of its original name in the files it comes
    /// from, through their re-exports; a module's file; or, for an import
    /// from outside the repository, the import. A Go package is its import
    /// where it's used and, at its import, its `package` clause.
    fn imported_definitions(&self, ask: &Ask, name: &str, at: Option<Use>) -> Vec<Candidate> {
        let &Ask { repo, origin, .. } = ask;
        if ask.stopped() {
            return Vec::new();
        }
        let Some(asking) = self.imports_of(repo, origin) else {
            return Vec::new();
        };
        let imports = asking.imports.get().map_or(&[][..], Vec::as_slice);
        let Some(import) = imports.iter().find(|i| i.name == name) else {
            return Vec::new();
        };
        // `x.p` and `a::p` name a member, not the `p` imported.
        if let Some(at) = at
            && let Some(range) = used(&asking.text, name, at)
            && let Some(tree) = asking.tree()
            && highlight::is_member(tree, range)
        {
            return Vec::new();
        }
        let at_line = |l: u32, line: usize, kind: Option<DefinitionKind>| {
            let listed = &self.listed[l as usize];
            Candidate {
                path: listed.path.clone(),
                side: listed.side,
                kind,
                start: line,
                end: line,
                container: None,
                rank: Rank::Imported,
                changed: false,
                in_hunk: None,
            }
        };
        // Without the use's place a member of the name can't be told from
        // the import, so the import's own line isn't pinned.
        let the_import = || {
            at?;
            let l = self.find(&origin.path)?;
            let found = at_line(l, import.line, Some(DefinitionKind::Import));
            Some(Candidate {
                rank: Rank::SameFile,
                ..found
            })
        };
        let language = origin.language.name();
        let on_import = at.is_some_and(|at| at.line == import.line);
        let Some(original) = &import.original else {
            let module = match (language, import.files.first()) {
                (_, None) if import.external => the_import(),
                (_, None) => None,
                ("go", Some(_)) if on_import => self.package_clause(repo, &import.files, name),
                ("go", Some(_)) => the_import(),
                (_, Some(&l)) => Some(at_line(l, 0, Some(DefinitionKind::Module))),
            };
            return module.into_iter().collect();
        };
        if import.files.is_empty() {
            return (the_import().filter(|_| import.external))
                .into_iter()
                .collect();
        }
        let mut wanted = Vec::new();
        if original == "default" {
            for &l in &import.files {
                let Some((line, exported)) = self.text(repo, l).and_then(|t| default_export(&t))
                else {
                    continue;
                };
                match exported {
                    Ok(name) => wanted.push((vec![l], name)),
                    Err(kind) => return vec![at_line(l, line, Some(kind))],
                }
            }
        } else {
            wanted.push((import.files.clone(), original.clone()));
        }
        self.followed(ask, wanted)
    }

    /// The definitions of each name `wanted` in its files, a re-export
    /// followed to where it's from, up to [`HOPS`] re-exports away.
    fn followed(&self, ask: &Ask, mut wanted: Vec<(Vec<u32>, String)>) -> Vec<Candidate> {
        let &Ask { repo, cache, .. } = ask;
        let language = ask.origin.language.name();
        let (mut seen, mut out) = (FxHashSet::default(), Vec::new());
        // The files named, then those each re-export names.
        for _ in 0..=HOPS {
            let mut next = Vec::new();
            for (files, original) in wanted {
                let indexed: Vec<u32> = files
                    .iter()
                    .filter_map(|&l| self.listed[l as usize].indexed)
                    .collect();
                // Read now when the index hasn't: they're what the name is.
                let until = (None, ask.deadline);
                if (self.fill(repo, cache, &indexed, until, ask.cancel)).is_err() {
                    return out;
                }
                for l in files {
                    if !seen.insert((l, original.clone())) {
                        continue;
                    }
                    let Some(defs) = self.definitions_of(l) else {
                        continue;
                    };
                    // What a module exports isn't a member of a class in it.
                    let exported = |d: &highlight::Definition| {
                        d.name == original
                            && !matches!(d.kind, DefinitionKind::Field | DefinitionKind::Method)
                    };
                    for d in defs.iter().filter(exported) {
                        if d.kind == DefinitionKind::Reexport {
                            next.push(self.reexported(repo, l, &d, language));
                        } else {
                            let listed = &self.listed[l as usize];
                            out.push(Candidate {
                                path: listed.path.clone(),
                                side: listed.side,
                                kind: Some(d.kind),
                                start: d.start,
                                end: d.end,
                                container: d.container.map(str::to_owned),
                                rank: Rank::Imported,
                                changed: false,
                                in_hunk: None,
                            });
                        }
                    }
                }
            }
            if !out.is_empty() || next.is_empty() {
                break;
            }
            wanted = next;
        }
        out
    }

    /// The type the function `call` calls is declared to return, one hop:
    /// of the functions the asking file imports by the name, or the module
    /// it's called through has, when there's such an import; else of the
    /// index's definitions of the name with a return type that the asking
    /// file's language sees, one on the type it's called on, else one in
    /// the asking file, else the first.
    fn returned(&self, ask: &Ask, call: &highlight::Call) -> Option<String> {
        // Each file defining the name is read: a method on a type known
        // is looked for only where the type's name is too, and a name
        // defined in more files than this (`new`, `get`) is given up on.
        const READ: usize = 64;
        let origin = ask.origin;
        if let Some((files, name)) = self.called_import(ask, call) {
            let defined = self.followed(ask, vec![(files, name)]);
            return defined.iter().find_map(|c| {
                let defs = self.definitions_of(self.find(&c.path)?)?;
                let d = defs
                    .iter()
                    .find(|d| (d.start, d.end) == (c.start, c.end) && d.returns.is_some())?;
                d.returns.map(str::to_owned)
            });
        }
        let language = origin.language.name();
        let mut files = self.posting(&call.name);
        if let Some(container) = &call.container {
            let with = self.posting(container);
            files.retain(|f| with.binary_search(f).is_ok());
        }
        files.retain(|&f| {
            let of = &self.languages[usize::from(self.indexed[f as usize].language)];
            shares_definitions(language, of.name())
        });
        if files.len() > READ || ask.stopped() {
            return None;
        }
        // (listed file, container, return type)
        let mut found: Vec<(u32, Option<String>, String)> = Vec::new();
        for f in files {
            let l = self.indexed[f as usize].listed;
            let Some(defs) = self.definitions_of(l) else {
                continue;
            };
            for d in defs.iter().filter(|d| d.name == call.name) {
                if let Some(returns) = d.returns {
                    found.push((l, d.container.map(str::to_owned), returns.to_owned()));
                }
            }
        }
        if let Some(container) = &call.container
            && found.iter().any(|f| f.1.as_ref() == Some(container))
        {
            found.retain(|f| f.1.as_ref() == Some(container));
        }
        let own = self.find(&origin.path);
        found.sort_by_key(|f| Some(f.0) != own);
        found.into_iter().next().map(|f| f.2)
    }

    /// The files the function `call` calls comes from, and its name there,
    /// when the asking file imports it, or the module it's called through.
    fn called_import(&self, ask: &Ask, call: &highlight::Call) -> Option<(Vec<u32>, String)> {
        if call.container.is_some() {
            return None;
        }
        let asking = self.imports_of(ask.repo, ask.origin)?;
        let imports = asking.imports.get()?;
        let through = call.module.as_ref().unwrap_or(&call.name);
        let import = imports.iter().find(|i| i.name == *through)?;
        let name = match (&call.module, &import.original) {
            (Some(_), _) => call.name.clone(),
            (None, Some(original)) => original.clone(),
            (None, None) => return None,
        };
        Some((import.files.clone(), name))
    }

    /// The indexed files the index posts `name` under, ascending; none
    /// while the postings are being written again.
    fn posting(&self, name: &str) -> Vec<u32> {
        let state = self.state();
        (state.posted(self.shared.store(), super::hash(name))).unwrap_or_default()
    }

    /// The files re-export `d` of listed file `l` is from, and the name it
    /// has there: the module the file imports it from, else every one the
    /// file re-exports from.
    fn reexported(
        &self,
        repo: &Repo,
        l: u32,
        d: &highlight::Definition,
        language: &str,
    ) -> (Vec<u32>, String) {
        let original = d.original.unwrap_or(d.name);
        let imported = (self.text(repo, l))
            .map(|text| self.imported_names(repo, l, language, &text))
            .unwrap_or_default();
        match imported
            .into_iter()
            .find(|i| i.name == original && !i.files.is_empty())
        {
            Some(i) => (i.files, i.original.unwrap_or_else(|| original.to_owned())),
            None => (
                self.reexport_sources(repo, l, language),
                original.to_owned(),
            ),
        }
    }

    /// The `package` clause of a Go package of `files`, named `name`
    /// where it's imported: in the file with the package's doc comment,
    /// else in the first.
    fn package_clause(&self, repo: &Repo, files: &[u32], name: &str) -> Option<Candidate> {
        let doc = format!("// Package {name} ");
        let texts = files.iter().filter_map(|&l| Some((l, self.text(repo, l)?)));
        let mut first = None;
        for (l, text) in texts {
            let line = word_line(&text, 0, b"package");
            if memchr::memmem::find(&text, doc.as_bytes()).is_some() {
                first = Some((l, line));
                break;
            }
            first.get_or_insert((l, line));
        }
        let (l, line) = first?;
        let listed = &self.listed[l as usize];
        Some(Candidate {
            path: listed.path.clone(),
            side: listed.side,
            kind: Some(DefinitionKind::Module),
            start: line,
            end: line,
            container: None,
            rank: Rank::Imported,
            changed: false,
            in_hunk: None,
        })
    }

    /// The definitions the index has of listed file `l`, if it has them.
    fn definitions_of(&self, l: u32) -> Option<Definitions> {
        let f = self.listed[l as usize].indexed?;
        let at = match self.state().slots.get(f as usize)? {
            Slot::Found(Some(at)) => *at,
            _ => return None,
        };
        self.read(at)
    }
}

impl Navigator {
    /// Puts first among `candidates` of the member `name` used at `at` in
    /// the asking file of `origin` those its receiver has: of the type the
    /// file tells it has, else of the type the call it's the value of is
    /// declared to return, else those the module or the namespace it is
    /// defines or re-exports, followed to where they're defined.
    fn by_receiver(&self, ask: &Ask, (name, at): (&str, Use), candidates: &mut Vec<Candidate>) {
        let &Ask { repo, origin, .. } = ask;
        if ask.stopped() {
            return;
        }
        let Some(asking) = self.asking(repo, origin) else {
            return;
        };
        let Some(range) = used(&asking.text, name, at) else {
            return;
        };
        let Some(tree) = asking.tree() else {
            return;
        };
        let stop = || ask.stopped();
        let Some(receiver) =
            highlight::receiver_at(&asking.language, tree, &asking.text, range, &stop)
        else {
            return;
        };
        let returned = || {
            let call = receiver.call.as_ref()?;
            self.returned(ask, call)
        };
        let has = |ty: &str| {
            candidates
                .iter()
                .any(|c| c.container.as_deref() == Some(ty))
        };
        // A declared return type comes before the type `T::f()` is guessed
        // to give.
        let guessed = || receiver.ty.clone().filter(|ty| has(ty));
        let ty = if receiver.call.is_some() {
            returned().filter(|ty| has(ty)).or_else(guessed)
        } else {
            guessed()
        };
        if let Some(ty) = ty {
            candidates.sort_by_key(|c| c.container.as_deref() != Some(ty.as_str()));
            return;
        }
        let module = receiver.name.as_deref().and_then(|receiver| {
            let asking = self.imports_of(repo, origin)?;
            let imports = asking.imports.get()?;
            let import = (imports.iter()).find(|i| i.name == receiver && i.original.is_none())?;
            Some(import.files.clone())
        });
        if let Some(files) = module {
            let wanted = vec![(files, name.to_owned())];
            for found in self.followed(ask, wanted).into_iter().rev() {
                pin(candidates, found);
            }
        }
    }
}

/// Whether `path` is vendored or generated code, by its folders (a
/// `vendor/`, a `third_party/`, a `node_modules/`) or its name (Protocol
/// Buffers' and other generators').
pub(super) fn kept_out(path: &BStr) -> bool {
    const FOLDERS: &[&[u8]] = &[b"vendor/", b"third_party/", b"node_modules/"];
    const NAMES: &[&[u8]] = &[
        b"_pb2.py",
        b"_pb2_grpc.py",
        b".pb.go",
        b".pb.cc",
        b".pb.h",
        b"_generated.go",
        b".generated.ts",
        b".g.dart",
    ];
    let dir = super::folder(path);
    let name = &path[dir.len()..];
    (dir.split_inclusive(|&b| b == b'/')).any(|c| FOLDERS.contains(&c))
        || NAMES.iter().any(|n| name.ends_with(n))
        || name.starts_with(b"zz_generated")
}

/// Whether `path` is a test's: in a test folder, or named as one.
pub(super) fn is_test(path: &BStr) -> bool {
    const FOLDERS: &[&[u8]] = &[
        b"test",
        b"tests",
        b"testing",
        b"testdata",
        b"__tests__",
        b"spec",
        b"selftests",
    ];
    let dir = super::folder(path);
    let name = &path[dir.len()..];
    let stem = name.split(|&b| b == b'.').next().unwrap_or(name);
    let named = name
        .split(|&b| b == b'.')
        .skip(1)
        .any(|part| part == b"test" || part == b"spec")
        || [&b"_test"[..], b"_spec", b"Test", b"Tests"]
            .iter()
            .any(|s| stem.ends_with(s))
        || stem.starts_with(b"test_");
    named || dir.split(|&b| b == b'/').any(|c| FOLDERS.contains(&c))
}

/// Puts `pinned` first among `candidates`, in place of a candidate of the
/// same file and line the index found.
pub(super) fn pin(candidates: &mut Vec<Candidate>, pinned: Candidate) {
    candidates.retain(|c| !(c.path == pinned.path && (c.start..=c.end).contains(&pinned.start)));
    candidates.insert(0, pinned);
}

/// The bytes of `text` the use `at` of `name` is: at its column, or the
/// first use of the name as a whole word on its line.
fn used(text: &[u8], name: &str, at: Use) -> Option<Range<usize>> {
    let mut start = 0;
    for _ in 0..at.line {
        start += memchr::memchr(b'\n', &text[start..])? + 1;
    }
    let end = memchr::memchr(b'\n', &text[start..]).map_or(text.len(), |e| start + e);
    let line = &text[start..end];
    let word = name.as_bytes();
    let whole = |i: usize| {
        line.get(i..i + word.len()) == Some(word)
            && (i == 0 || !is_word_byte(line[i - 1]))
            && line.get(i + word.len()).is_none_or(|&b| !is_word_byte(b))
    };
    let column = match at.column {
        Some(column) => whole(column).then_some(column)?,
        None => (0..line.len()).find(|&i| whole(i))?,
    };
    Some(start + column..start + column + word.len())
}

#[cfg(test)]
mod tests;

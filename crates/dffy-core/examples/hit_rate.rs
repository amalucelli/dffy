//! How often go-to-definition's first candidate is where a language server
//! says a name is defined, on a repository's working tree, in any language
//! with a tags query.
//!
//! ```sh
//! cargo run --release -p dffy-core --example hit_rate -- \
//!     <repo> <language> [uses] [seed] -- <server> [server args…]
//! # for example
//! … -- ~/src/cobra go 300 1 -- gopls
//! … -- ~/src/ky typescript 300 1 -- typescript-language-server --stdio
//! ```
//!
//! `LSP_INIT_OPTIONS` is JSON sent as `initializationOptions`, as a server
//! may need: `{"tsserver":{"path":"…/typescript/lib/tsserver.js"}}` for a
//! repository without TypeScript installed. `LSP_TRACE` set prints what the
//! server sends.
//!
//! It samples `uses` (default 300) words of the language's files that the
//! names tokenizer offers, with a fixed seed, asks the server for each
//! one's definition over LSP (`initialize`, `didOpen`,
//! `textDocument/definition`), and looks each up from the file it's in. A
//! use counts as a hit when the first candidate is in the file the server
//! names and its lines hold the server's line, and as found when any
//! candidate does. Uses the server doesn't resolve, or resolves outside the
//! repository or to the use itself (a definition's own name), aren't
//! counted. The misses are grouped by why: the server's definition is in
//! the asking file but not a definition the index lists (a local, a
//! parameter, an import, or a member it doesn't list), or in another file
//! and not listed, or listed but ranked lower.
//!
//! It measures a fully built index: the index is built before the first
//! lookup, so the local tier's time budget never leaves a file out.
//!
//! `HIT_RATE_CASES` names a file of the sampled uses and the server's
//! answers: written when it's missing, and read instead of asking a server
//! when it's there, so runs after a change score the same uses. The named
//! subset (defined in another file, or listed by the index then) is fixed
//! when it's written. With `HIT_RATE_NO_PLACE` set, each use is looked up
//! by its name and file alone, as a host that sends no place does.
#![expect(
    clippy::expect_used,
    reason = "a measuring harness: a failed step leaves nothing to measure"
)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use dffy_core::cache::Cache;
use dffy_core::diff::Options;
use dffy_core::git::Repo;
use dffy_core::highlight::{self, Registry};
use dffy_core::navigate::{
    Candidate, Diff, Navigator, Origin, Shared, Use as Place, name_at, names,
};
use dffy_core::view;
use serde_json::{Value, json};

/// How long a server may take to answer one request.
const ANSWER: Duration = Duration::from_secs(60);

/// How long the first answers may stay empty while a server indexes.
const WARM_UP: Duration = Duration::from_secs(120);

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let split = args.iter().position(|a| a == "--").unwrap_or(args.len());
    let (ours, server) = (&args[..split], args.get(split + 1..).unwrap_or_default());
    let [root, language, rest @ ..] = ours else {
        eprintln!("usage: hit_rate <repo> <language> [uses] [seed] -- <server> [args…]");
        std::process::exit(2);
    };
    let wanted: usize = rest.first().map_or(300, |n| n.parse().expect("a count"));
    let seed: u64 = rest.get(1).map_or(1, |n| n.parse().expect("a seed"));
    let root = PathBuf::from(root).canonicalize().expect("the repository");
    let index = Indexed::build(&root, language);
    let saved = std::env::var_os("HIT_RATE_CASES").map(PathBuf::from);
    let cases = if let Some(path) = saved.as_deref().filter(|p| p.exists()) {
        let text = std::fs::read_to_string(path).expect("the cases");
        Cases::from_json(&serde_json::from_str(&text).expect("cases as JSON"))
    } else {
        if server.is_empty() {
            eprintln!("hit_rate: no server command after --, and no cases to replay");
            std::process::exit(2);
        }
        let cases = record(&index, wanted, seed, server);
        if let Some(path) = saved {
            std::fs::write(path, cases.to_json().to_string()).expect("writes the cases");
        }
        cases
    };
    score(&index, &cases).report(language, seed);
}

/// A word to look up: its file, line and byte columns.
struct Use {
    path: String,
    line: usize,
    start: usize,
    end: usize,
}

/// What a recording keeps of a sample, so later runs score the same uses
/// against the same answers: the counts left out, and each use the server
/// resolved in the repository with its answer.
struct Cases {
    sampled: usize,
    unresolved: usize,
    outside: usize,
    itself: usize,
    uses: Vec<Case>,
}

struct Case {
    path: String,
    line: usize,
    start: usize,
    name: String,
    target_path: String,
    target_line: usize,
    /// Defined in another file, or a definition the index listed when the
    /// cases were recorded: the named subset, fixed at that baseline.
    named: bool,
}

impl Cases {
    fn to_json(&self) -> Value {
        let uses: Vec<Value> = (self.uses.iter())
            .map(|c| {
                json!({
                    "path": c.path, "line": c.line, "start": c.start, "name": c.name,
                    "target_path": c.target_path, "target_line": c.target_line,
                    "named": c.named,
                })
            })
            .collect();
        json!({
            "sampled": self.sampled, "unresolved": self.unresolved,
            "outside": self.outside, "itself": self.itself, "uses": uses,
        })
    }

    fn from_json(v: &Value) -> Self {
        let n =
            |v: &Value, k: &str| usize::try_from(v[k].as_u64().expect("a number")).expect("fits");
        let s = |v: &Value, k: &str| v[k].as_str().expect("a string").to_owned();
        Self {
            sampled: n(v, "sampled"),
            unresolved: n(v, "unresolved"),
            outside: n(v, "outside"),
            itself: n(v, "itself"),
            uses: (v["uses"].as_array().expect("uses").iter())
                .map(|c| Case {
                    path: s(c, "path"),
                    line: n(c, "line"),
                    start: n(c, "start"),
                    name: s(c, "name"),
                    target_path: s(c, "target_path"),
                    target_line: n(c, "target_line"),
                    named: c["named"].as_bool().expect("a bool"),
                })
                .collect(),
        }
    }
}

#[derive(Default)]
struct Tally {
    sampled: usize,
    unresolved: usize,
    outside: usize,
    itself: usize,
    /// Over every use, and over the named subset.
    all: Score,
    named: Score,
    misses: Vec<String>,
}

#[derive(Default)]
struct Score {
    counted: usize,
    first: usize,
    among: usize,
}

impl Tally {
    fn report(&self, language: &str, seed: u64) {
        println!("{language}: {} uses sampled (seed {seed})", self.sampled);
        println!(
            "  not counted: {} unresolved by the server, {} defined outside the repository, \
             {} a definition's own name",
            self.unresolved, self.outside, self.itself
        );
        for (what, s) in [("all", &self.all), ("named", &self.named)] {
            println!(
                "  {what} {}: top-1 {} ({}), among the candidates {} ({})",
                s.counted,
                percent(s.first, s.counted),
                s.first,
                percent(s.among, s.counted),
                s.among
            );
        }
        println!("misses:");
        for line in self.misses.iter().take(60) {
            println!("{line}");
        }
    }
}

fn percent(n: usize, of: usize) -> String {
    let tenths = n * 1000 / of.max(1);
    format!("{}.{}%", tenths / 10, tenths % 10)
}

/// `0..len` shuffled with a 64-bit LCG (Knuth's MMIX constants): the same
/// order per seed.
fn shuffled(len: usize, mut seed: u64) -> Vec<usize> {
    let mut order: Vec<usize> = (0..len).collect();
    for i in (1..len).rev() {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        order.swap(i, usize::try_from(seed >> 33).expect("fits") % (i + 1));
    }
    order
}

/// The LSP language id of a file of `language`.
fn language_id(language: &str) -> &str {
    match language {
        "tsx" => "typescriptreact",
        "javascript" => "javascript",
        "cpp" => "cpp",
        other => other,
    }
}

/// A repository's working tree with its index built, for a language.
struct Indexed {
    root: PathBuf,
    language: String,
    repo: Repo,
    grammars: Registry,
    lang: highlight::Language,
    nav: Navigator,
    cache: dffy_core::cache::SymbolCache,
    _cache_dir: tempfile::TempDir,
    snapshot: dffy_core::git::Snapshot,
}

impl Indexed {
    fn build(root: &Path, language: &str) -> Self {
        let repo = Repo::discover(root).expect("a repository");
        let spec = repo.resolve(&[], false, false).expect("resolves");
        let mut changes = Vec::new();
        repo.for_each_change(&spec, &[], |c| {
            changes.push(c);
            ControlFlow::Continue(())
        })
        .expect("enumerates");
        let grammars = Registry::builtin();
        let lang = grammars
            .named(language)
            .expect("a bundled language")
            .clone();
        let snapshots = spec.snapshots(false).expect("snapshots");
        let diff = Diff {
            snapshots,
            changes: &changes,
            options: Options::default(),
        };
        let nav = Navigator::new(&repo, &grammars, &Shared::new(), &diff, view::Side::New)
            .expect("lists");
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let cache = Cache::new(cache_dir.path().to_owned(), u64::MAX).symbols();
        nav.build(&repo, &cache, 4, &AtomicBool::new(false), |_| {})
            .expect("the queries compile");
        Self {
            root: root.to_owned(),
            language: language.to_owned(),
            repo,
            grammars,
            lang,
            nav,
            cache,
            _cache_dir: cache_dir,
            snapshot: snapshots[1],
        }
    }
}

/// Every word the tokenizer offers in the language's files, and the
/// files' texts by path.
fn every_use(index: &Indexed) -> (Vec<Use>, HashMap<String, String>) {
    let (root, language) = (&index.root, index.language.as_str());
    let mut files: Vec<String> = Vec::new();
    index
        .repo
        .list_paths(index.snapshot, &[], |path, _| {
            files.push(path.to_string());
            ControlFlow::Continue(())
        })
        .expect("lists");
    files.retain(|p| (index.grammars.detect(p.as_bytes(), b"")).is_some_and(|l| *l == index.lang));
    let mut texts: HashMap<String, String> = HashMap::new();
    let mut uses = Vec::new();
    for path in &files {
        let Ok(text) = std::fs::read_to_string(root.join(path)) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for name in names(line.as_bytes(), language) {
                let found = line
                    .match_indices(&name)
                    .find(|(at, _)| name_at(line.as_bytes(), *at) == Some(*at..*at + name.len()));
                if let Some((at, _)) = found {
                    uses.push(Use {
                        path: path.clone(),
                        line: n,
                        start: at,
                        end: at + name.len(),
                    });
                }
            }
        }
        texts.insert(path.clone(), text);
    }
    eprintln!("{} files, {} uses", files.len(), uses.len());
    (uses, texts)
}

/// Samples `wanted` uses with `seed` and asks the server for each one's
/// definition.
fn record(index: &Indexed, wanted: usize, seed: u64, server: &[String]) -> Cases {
    let root = &index.root;
    let language = index.language.as_str();
    let (uses, texts) = every_use(index);
    let mut lsp = Lsp::start(root, server);
    let mut opened = std::collections::HashSet::new();
    let started = Instant::now();
    let mut warm = false;
    let mut cases = Cases {
        sampled: 0,
        unresolved: 0,
        outside: 0,
        itself: 0,
        uses: Vec::new(),
    };
    for i in shuffled(uses.len(), seed) {
        if cases.sampled == wanted {
            break;
        }
        let u = &uses[i];
        let text = &texts[&u.path];
        let line_text = text.lines().nth(u.line).unwrap_or_default();
        let name = &line_text[u.start..u.end];
        let uri = format!("file://{}", root.join(&u.path).display());
        if opened.insert(u.path.clone()) {
            lsp.notify(
                "textDocument/didOpen",
                &json!({"textDocument": {
                    "uri": uri, "languageId": language_id(language), "version": 1, "text": text,
                }}),
            );
        }
        let character = line_text[..u.start].encode_utf16().count();
        let params = json!({
            "textDocument": {"uri": uri},
            "position": {"line": u.line, "character": character},
        });
        // Until the server first answers with a definition it may still be
        // indexing: an empty answer or an error is asked again.
        let mut answer = lsp.request("textDocument/definition", &params);
        while !warm
            && answer
                .as_ref()
                .is_none_or(|a| a.is_null() || *a == json!([]))
            && started.elapsed() < WARM_UP
        {
            std::thread::sleep(Duration::from_secs(2));
            answer = lsp.request("textDocument/definition", &params);
        }
        warm |= answer.as_ref().is_some_and(|a| first_location(a).is_some());
        cases.sampled += 1;
        let Some((target_uri, target_line)) = answer.as_ref().and_then(first_location) else {
            cases.unresolved += 1;
            continue;
        };
        let prefix = format!("file://{}/", root.display());
        let Some(target_path) = target_uri.strip_prefix(&prefix).map(str::to_owned) else {
            cases.outside += 1;
            continue;
        };
        if target_path == u.path && target_line == u.line {
            cases.itself += 1;
            continue;
        }
        let named =
            target_path != u.path || listed(&index.grammars, root, &target_path, target_line, name);
        cases.uses.push(Case {
            path: u.path.clone(),
            line: u.line,
            start: u.start,
            name: name.to_owned(),
            target_path,
            target_line,
            named,
        });
    }
    lsp.stop();
    cases
}

/// Looks each case up from where it's used, against the server's answer.
fn score(index: &Indexed, cases: &Cases) -> Tally {
    let placed = std::env::var_os("HIT_RATE_NO_PLACE").is_none();
    let mut tally = Tally {
        sampled: cases.sampled,
        unresolved: cases.unresolved,
        outside: cases.outside,
        itself: cases.itself,
        ..Tally::default()
    };
    for case in &cases.uses {
        let origin = Origin {
            path: case.path.as_str().into(),
            language: index.lang.clone(),
        };
        let at = placed.then_some(Place {
            line: case.line,
            column: Some(case.start),
        });
        let lookup = (index.nav)
            .lookup_at(
                &index.repo,
                &index.cache,
                &case.name,
                &origin,
                at,
                &AtomicBool::new(false),
            )
            .expect("looks up");
        let holds = |c: &Candidate| {
            c.path == case.target_path.as_str() && (c.start..=c.end).contains(&case.target_line)
        };
        let first = lookup.candidates.first().is_some_and(holds);
        let among = lookup.candidates.iter().any(holds);
        for score in [&mut tally.all]
            .into_iter()
            .chain(case.named.then_some(&mut tally.named))
        {
            score.counted += 1;
            score.first += usize::from(first);
            score.among += usize::from(among);
        }
        if !first {
            let top = lookup.candidates.first().map_or_else(
                || "none".to_owned(),
                |c| {
                    let kind = c.kind.map_or("text", |k| k.name());
                    format!("{}:{} {kind}", c.path, c.start + 1)
                },
            );
            tally.misses.push(format!(
                "  {}{} at {}:{}: first {top} of {}{}, server {}:{}",
                if case.named { "" } else { "(local) " },
                case.name,
                case.path,
                case.line + 1,
                lookup.candidates.len(),
                if among { ", listed lower" } else { "" },
                case.target_path,
                case.target_line + 1,
            ));
        }
    }
    tally
}

/// Whether the index lists a definition of `name` in `path` holding `line`.
fn listed(grammars: &Registry, root: &Path, path: &str, line: usize, name: &str) -> bool {
    let Some(lang) = grammars.detect(path.as_bytes(), b"") else {
        return false;
    };
    let Ok(text) = std::fs::read(root.join(path)) else {
        return false;
    };
    let Ok(Some(Ok(defs))) = highlight::definitions(lang, &text) else {
        return false;
    };
    defs.iter()
        .any(|d| d.name == name && (d.start..=d.end).contains(&line))
}

/// The first location of a definition answer: a `Location`, an array of
/// them or of `LocationLink`s. Its URI and line.
fn first_location(answer: &Value) -> Option<(String, usize)> {
    let first = match answer {
        Value::Array(all) => all.first()?,
        Value::Object(_) => answer,
        _ => return None,
    };
    let uri = first
        .get("uri")
        .or_else(|| first.get("targetUri"))?
        .as_str()?;
    let range = (first.get("range"))
        .or_else(|| first.get("targetSelectionRange"))
        .or_else(|| first.get("targetRange"))?;
    let line = usize::try_from(range["start"]["line"].as_u64()?).ok()?;
    Some((uri.to_owned(), line))
}

/// A language server over stdio.
struct Lsp {
    child: Child,
    stdin: ChildStdin,
    messages: Receiver<Value>,
    next: u64,
}

impl Lsp {
    fn start(root: &Path, command: &[String]) -> Self {
        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("the server starts");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = child.stdout.take().expect("stdout");
        let (tx, messages) = channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut length = 0;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).unwrap_or(0) == 0 {
                        return;
                    }
                    let header = header.trim();
                    if header.is_empty() {
                        break;
                    }
                    if let Some(n) = header.strip_prefix("Content-Length:") {
                        length = n.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0; length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
                if let Ok(message) = serde_json::from_slice(&body)
                    && tx.send(message).is_err()
                {
                    return;
                }
            }
        });
        let mut lsp = Self {
            child,
            stdin,
            messages,
            next: 0,
        };
        let uri = format!("file://{}", root.display());
        let name = root.file_name().map(|n| n.to_string_lossy().into_owned());
        let options: Value = std::env::var("LSP_INIT_OPTIONS").map_or(Value::Null, |o| {
            serde_json::from_str(&o).expect("JSON options")
        });
        lsp.request(
            "initialize",
            &json!({
                "processId": std::process::id(),
                "initializationOptions": options,
                "rootUri": uri,
                "workspaceFolders": [{"uri": uri, "name": name}],
                "capabilities": {
                    "textDocument": {"definition": {"linkSupport": true}},
                    "workspace": {"workspaceFolders": true, "configuration": true},
                    "window": {"workDoneProgress": true},
                },
            }),
        )
        .expect("the server initializes");
        lsp.notify("initialized", &json!({}));
        lsp
    }

    fn send(&mut self, message: &Value) {
        let body = message.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).expect("writes");
        self.stdin.flush().expect("flushes");
    }

    fn notify(&mut self, method: &str, params: &Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// The result of `method`, answering the server's own requests
    /// meanwhile; `None` on an error or a timeout.
    fn request(&mut self, method: &str, params: &Value) -> Option<Value> {
        self.next += 1;
        let id = self.next;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        let deadline = Instant::now() + ANSWER;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let message = match self.messages.recv_timeout(left) {
                Ok(message) => message,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
            };
            if std::env::var_os("LSP_TRACE").is_some() {
                let text = message.to_string();
                eprintln!("<- {}", &text[..text.len().min(300)]);
            }
            match (message.get("id"), message.get("method")) {
                (Some(asked), Some(asked_method)) => {
                    // The server's request: configuration gets one empty
                    // object per item, anything else an empty answer.
                    let result = if asked_method == "workspace/configuration" {
                        let items = message["params"]["items"].as_array().map_or(0, Vec::len);
                        Value::Array(vec![json!({}); items])
                    } else {
                        Value::Null
                    };
                    let reply = json!({"jsonrpc": "2.0", "id": asked, "result": result});
                    self.send(&reply);
                }
                (Some(answered), None) if answered.as_u64() == Some(id) => {
                    if let Some(error) = message.get("error") {
                        eprintln!("{method}: {error}");
                    }
                    return message.get("result").cloned();
                }
                _ => {}
            }
        }
    }

    fn stop(mut self) {
        drop(self.request("shutdown", &Value::Null));
        self.notify("exit", &Value::Null);
        drop(self.child.wait());
    }
}

//! `dffy mcp`: a Model Context Protocol server on stdin and stdout, for
//! coding agents. Its tools return the header and file objects of `--json`
//! (docs/json.md) a page at a time, each result also rendered as plain text
//! for clients that show the model only text. docs/mcp.md is the user's
//! side of it.
//!
//! Two eras of the protocol are served: 2026-07-28, where every request
//! carries its version and the client's capabilities in `_meta`, and the
//! `initialize` handshake of 2025-11-25 and 2025-06-18, which older clients
//! open with. Messages are answered one at a time, in order.
//!
//! The repository is opened once; every call resolves its revisions and
//! lists its changes again, so a moved HEAD or an edited file shows on the
//! next call. Cursors carry a fingerprint of the request and of what it
//! listed or diffed, so a cursor from another request, or from before the
//! diff changed, is refused instead of continuing at the wrong place.

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use dffy_core::diff::Options;
use dffy_core::git::{FileChange, Repo, Revisions};

use crate::config::{Context, DefaultDiff};
use crate::json::{self, Compared, Detail, Files, Window};
use crate::plugin::json::{Value, parse, string};
use crate::print::{Plan, Summary, Target};
use crate::{Ctx, Error};

/// The revision with per-request metadata.
const MODERN: &str = "2026-07-28";
/// The revisions opened with `initialize`, newest first. 2025-03-26 and
/// earlier have no structured tool results.
const LEGACY: [&str; 2] = ["2025-11-25", "2025-06-18"];

const PARSE_ERROR: i32 = -32700;
const INVALID_REQUEST: i32 = -32600;
const METHOD_NOT_FOUND: i32 = -32601;
const INVALID_PARAMS: i32 = -32602;
const INTERNAL_ERROR: i32 = -32603;
const UNSUPPORTED_VERSION: i32 = -32022;

/// Bytes of line text a `file_diff` result, or each file of a `diff`
/// result, holds at most past its first line: a minified file is one long
/// line, so a line count alone doesn't bound a result.
const FILE_BYTES: usize = 64 << 10;
/// Bytes of line text a `diff` result holds before it leaves the files
/// after it to the next page.
const PAGE_BYTES: usize = 64 << 10;

/// How long a client may keep the tool list: it changes only with dffy.
const LIST_TTL_MS: u64 = 3_600_000;

const INSTRUCTIONS: &str = "dffy shows the changes in the git repository it was started in. \
    Use it instead of git diff, git show or git log -p whenever you need to know what changed: \
    uncommitted edits (omit range), a branch (range: main...HEAD), the last commits \
    (range: HEAD~3..HEAD) or any two revisions. Start with changed_files to see which files changed and by how much, then call file_diff \
    for the files that matter. With syntax: true, files whose changes are only formatting or \
    only moved code are classified, so you can skip them. Results are paged: pass next_cursor \
    back as cursor, with the same arguments, to continue.";

/// The tools, as `tools/list` returns them. Written readable here and
/// compacted when listed, since a message is one line.
const TOOLS: &str = r#"[
{
  "name": "changed_files",
  "title": "Changed files",
  "description": "List the files a diff changes, without their content: path, status (added, deleted, modified, renamed, type_changed, unmerged, untracked), lines added and removed, language. Start here: it's cheap even for thousands of files. Then call file_diff for the files that matter. Without range it lists uncommitted changes: HEAD against the working tree, staged and unstaged, untracked files included. Set syntax: true to classify each file as formatting_only (only whitespace or line breaks changed) or moved_only (code moved, not edited), so you can skip files without real edits; it diffs every file of the page. Paged: when next_cursor isn't null, call again with it as cursor and the same arguments. For very large ranges, counts: false lists files without reading them.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "range": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Revisions as git diff takes them: \"main..HEAD\", \"main...HEAD\", \"HEAD~3\", \"v1.0 v2.0\" (a string is split at spaces), or an array of them. Omit for uncommitted changes."},
      "paths": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Only files under these paths, relative to the repository root. No globs."},
      "syntax": {"type": "boolean", "default": false, "description": "Diff by syntax to fill in each file's classification (formatting_only, moved_only) and syntax outcome."},
      "counts": {"type": "boolean", "default": true, "description": "Count added and removed lines. false reads no file content: added and removed are null and binary is false."},
      "limit": {"type": "integer", "minimum": 1, "maximum": 5000, "default": 200, "description": "Files per page."},
      "cursor": {"type": "string", "description": "next_cursor from the previous page."}
    },
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "header": {"type": "object", "description": "What was compared, as in dffy --json (docs/json.md)."},
      "files": {"type": "array", "items": {"type": "object"}, "description": "File objects of dffy --json, hunks null."},
      "total_files": {"type": "integer"},
      "counts": {"type": "boolean"},
      "next_cursor": {"type": ["string", "null"]}
    },
    "required": ["header", "files", "total_files", "counts", "next_cursor"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "file_diff",
  "title": "Diff of one file",
  "description": "Show one file's changes: hunks of removed, added and context lines, with line numbers. Use it after changed_files, for the files you need. A long diff is cut at a hunk or line boundary: when truncated is true, call again with next_cursor as cursor and the same arguments for the rest. With syntax: true, lines whose code wasn't edited are marked moved or reformatted, and the file gets a classification when that's all it has.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "path": {"type": "string", "description": "The file, relative to the repository root, as changed_files lists it. A renamed file's old path works too."},
      "range": {"type": ["string", "array"], "items": {"type": "string"}, "description": "The same range as for changed_files. Omit for uncommitted changes."},
      "syntax": {"type": "boolean", "default": false, "description": "Diff by syntax: marks moved and reformatted lines."},
      "context": {"type": "integer", "minimum": 0, "maximum": 10000, "default": 3, "description": "Unchanged lines around each change."},
      "max_lines": {"type": "integer", "minimum": 1, "maximum": 20000, "default": 400, "description": "Lines per result, across hunks."},
      "cursor": {"type": "string", "description": "next_cursor from the previous result."}
    },
    "required": ["path"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "header": {"type": "object", "description": "What was compared, as in dffy --json (docs/json.md)."},
      "file": {"type": "object", "description": "A file object of dffy --json, with the hunks of this part."},
      "truncated": {"type": "boolean"},
      "remaining_lines": {"type": "integer"},
      "next_cursor": {"type": ["string", "null"]}
    },
    "required": ["header", "file", "truncated", "remaining_lines", "next_cursor"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "diff",
  "title": "Diff of several files",
  "description": "Show the changes of several files at once, with hunks, in path order. For a large change prefer changed_files, then file_diff for the files that matter; diff suits a small change, or everything under a few paths. Each file holds at most max_lines_per_file lines: truncated_files lists the files cut short, each with a cursor for file_diff. When next_cursor isn't null, call again with it as cursor and the same arguments for the next files.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "range": {"type": ["string", "array"], "items": {"type": "string"}, "description": "The same range as for changed_files. Omit for uncommitted changes."},
      "paths": {"type": ["string", "array"], "items": {"type": "string"}, "description": "Only files under these paths, relative to the repository root. No globs."},
      "syntax": {"type": "boolean", "default": false, "description": "Diff by syntax: marks moved and reformatted lines and classifies files."},
      "context": {"type": "integer", "minimum": 0, "maximum": 10000, "default": 3, "description": "Unchanged lines around each change."},
      "max_files": {"type": "integer", "minimum": 1, "maximum": 200, "default": 20, "description": "Files per page."},
      "max_lines_per_file": {"type": "integer", "minimum": 1, "maximum": 20000, "default": 100, "description": "Lines per file, across its hunks."},
      "cursor": {"type": "string", "description": "next_cursor from the previous page."}
    },
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "header": {"type": "object", "description": "What was compared, as in dffy --json (docs/json.md)."},
      "files": {"type": "array", "items": {"type": "object"}, "description": "File objects of dffy --json."},
      "truncated_files": {"type": "array", "items": {"type": "object", "properties": {"path": {"type": "string"}, "remaining_lines": {"type": "integer"}, "cursor": {"type": "string"}}}},
      "total_files": {"type": "integer"},
      "next_cursor": {"type": ["string", "null"]}
    },
    "required": ["header", "files", "truncated_files", "total_files", "next_cursor"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}
}
]"#;

/// Serves the protocol until stdin closes. `dir` is where the repository
/// is looked for, relative to the current directory.
pub(crate) fn serve(dir: Option<&Path>, ctx: &Ctx) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    let mut server = Server {
        repo: Repo::discover(&base).ok(),
        base,
        ctx,
        legacy: false,
    };
    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if stdin.read_until(b'\n', &mut line)? == 0 {
            return Ok(());
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if let Some(reply) = server.message(&line) {
            stdout.write_all(reply.as_bytes())?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
}

struct Server<'a> {
    /// `None` until the repository is found; looked for again on each call
    /// until then.
    repo: Option<Repo>,
    /// Where dffy looks for the repository.
    base: PathBuf,
    ctx: &'a Ctx,
    /// An `initialize` came: requests without the per-request metadata are
    /// served as the legacy revision it chose.
    legacy: bool,
}

/// A JSON-RPC error.
#[derive(Debug, PartialEq)]
struct Fault {
    code: i32,
    message: String,
    /// The `data` member, as JSON.
    data: Option<String>,
}

impl Fault {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Which revision a request is served as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Era {
    Modern,
    Legacy,
}

impl Server<'_> {
    /// The reply to one message, or `None` for a notification or a
    /// response, which get none.
    fn message(&mut self, bytes: &[u8]) -> Option<String> {
        let value = match parse(bytes) {
            Ok(value) => value,
            Err(e) => return Some(reply(None, Err(Fault::new(PARSE_ERROR, e)))),
        };
        let Some(method) = value.get("method").and_then(Value::as_str) else {
            if value.get("result").is_some() || value.get("error").is_some() {
                return None;
            }
            let message = if value.as_array().is_some() {
                "batches aren't supported; send one request per line"
            } else {
                "not a JSON-RPC request"
            };
            return Some(reply(None, Err(Fault::new(INVALID_REQUEST, message))));
        };
        let id = value.get("id")?;
        let id = match id {
            Value::String(_) => compact(id),
            Value::Number(n) if n.fract() == 0.0 => compact(id),
            _ => {
                let fault = Fault::new(INVALID_REQUEST, "id must be a string or an integer");
                return Some(reply(None, Err(fault)));
            }
        };
        if value.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            let fault = Fault::new(INVALID_REQUEST, "jsonrpc must be \"2.0\"");
            return Some(reply(Some(&id), Err(fault)));
        }
        Some(reply(Some(&id), self.request(method, value.get("params"))))
    }

    /// The result of request `method`, as the members of its object.
    fn request(&mut self, method: &str, params: Option<&Value>) -> Result<String, Fault> {
        if method == "initialize" {
            return Ok(self.initialize(params));
        }
        let era = era(params, self.legacy || method == "ping")?;
        let members = match (method, era) {
            ("ping", Era::Legacy) => String::new(),
            ("server/discover", Era::Modern) => {
                let mut s = String::from(r#""supportedVersions":["#);
                for (i, v) in std::iter::once(MODERN).chain(LEGACY).enumerate() {
                    if i > 0 {
                        s.push(',');
                    }
                    string(&mut s, v);
                }
                s.push_str(r#"],"capabilities":{"tools":{}},"instructions":"#);
                string(&mut s, INSTRUCTIONS);
                s.push_str(&cacheable());
                s
            }
            ("tools/list", _) => {
                if params.and_then(|p| p.get("cursor")).is_some() {
                    return Err(Fault::new(
                        INVALID_PARAMS,
                        "invalid cursor: the tools fit one page",
                    ));
                }
                let tools = parse(TOOLS.as_bytes()).map_err(|e| Fault::new(INTERNAL_ERROR, e))?;
                let mut s = format!(r#""tools":{}"#, compact(&tools));
                if era == Era::Modern {
                    s.push_str(&cacheable());
                }
                s
            }
            ("tools/call", _) => self.call(params)?,
            _ => {
                return Err(Fault::new(
                    METHOD_NOT_FOUND,
                    format!("method not found: {method}"),
                ));
            }
        };
        let mut result = format!("{{{members}");
        if era == Era::Modern {
            if !members.is_empty() {
                result.push(',');
            }
            result.push_str(
                r#""resultType":"complete","_meta":{"io.modelcontextprotocol/serverInfo":"#,
            );
            result.push_str(&server_info());
            result.push('}');
        }
        result.push('}');
        Ok(result)
    }

    /// The legacy handshake: the version asked for when it's one of
    /// [`LEGACY`], otherwise the newest of them, as the handshake's
    /// negotiation has it.
    fn initialize(&mut self, params: Option<&Value>) -> String {
        let asked = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(Value::as_str);
        let version = LEGACY
            .into_iter()
            .find(|v| Some(*v) == asked)
            .unwrap_or(LEGACY[0]);
        self.legacy = true;
        let mut s = String::from(r#"{"protocolVersion":"#);
        string(&mut s, version);
        s.push_str(r#","capabilities":{"tools":{}},"serverInfo":"#);
        s.push_str(&server_info());
        s.push_str(r#","instructions":"#);
        string(&mut s, INSTRUCTIONS);
        s.push('}');
        s
    }

    /// `tools/call`: the tool's result, or a tool error the model can act
    /// on. Only a call that names no known tool is a protocol error.
    fn call(&mut self, params: Option<&Value>) -> Result<String, Fault> {
        let name = params
            .and_then(|p| p.get("name"))
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::new(INVALID_PARAMS, "tools/call needs the tool's name"))?;
        let args = match params.and_then(|p| p.get("arguments")) {
            None | Some(Value::Null) => &[][..],
            Some(Value::Object(fields)) => fields,
            Some(_) => {
                return Err(Fault::new(INVALID_PARAMS, "arguments must be an object"));
            }
        };
        let args = Args(args);
        let result = match name {
            "changed_files" => self.changed_files(&args),
            "file_diff" => self.file_diff(&args),
            "diff" => self.diff(&args),
            _ => return Err(Fault::new(INVALID_PARAMS, format!("Unknown tool: {name}"))),
        };
        let mut s = String::from(r#""content":[{"type":"text","text":"#);
        match result {
            Ok(Output { structured, text }) => {
                string(&mut s, &text);
                s.push_str(r#"}],"structuredContent":"#);
                s.push_str(&structured);
                s.push_str(r#","isError":false"#);
            }
            Err(message) => {
                string(&mut s, &message);
                s.push_str(r#"}],"isError":true"#);
            }
        }
        Ok(s)
    }
}

/// How a request without `initialize` before it is served: by its
/// `_meta`, which a modern request must have. `legacy` allows a request
/// without it.
fn era(params: Option<&Value>, legacy: bool) -> Result<Era, Fault> {
    let meta = params.and_then(|p| p.get("_meta"));
    let Some(version) = meta.and_then(|m| m.get("io.modelcontextprotocol/protocolVersion")) else {
        if legacy {
            return Ok(Era::Legacy);
        }
        return Err(Fault::new(
            INVALID_PARAMS,
            format!(
                "missing _meta.io.modelcontextprotocol/protocolVersion: send it on every request \
                 ({MODERN}), or open with initialize ({})",
                LEGACY.join(", ")
            ),
        ));
    };
    let version = version.as_str().unwrap_or_default();
    if version != MODERN {
        let mut data = String::from(r#"{"supported":["#);
        for (i, v) in std::iter::once(MODERN).chain(LEGACY).enumerate() {
            if i > 0 {
                data.push(',');
            }
            string(&mut data, v);
        }
        data.push_str(r#"],"requested":"#);
        string(&mut data, version);
        data.push('}');
        return Err(Fault {
            code: UNSUPPORTED_VERSION,
            message: "Unsupported protocol version".to_owned(),
            data: Some(data),
        });
    }
    if !matches!(
        meta.and_then(|m| m.get("io.modelcontextprotocol/clientCapabilities")),
        Some(Value::Object(_))
    ) {
        return Err(Fault::new(
            INVALID_PARAMS,
            "missing _meta.io.modelcontextprotocol/clientCapabilities",
        ));
    }
    Ok(Era::Modern)
}

fn server_info() -> String {
    format!(
        r#"{{"name":"dffy","version":"{}"}}"#,
        env!("CARGO_PKG_VERSION")
    )
}

/// The caching members a modern list or discovery result must have.
fn cacheable() -> String {
    format!(r#","ttlMs":{LIST_TTL_MS},"cacheScope":"public""#)
}

/// A response line: `id` is the request's, as JSON; `None` when it
/// couldn't be read.
fn reply(id: Option<&str>, result: Result<String, Fault>) -> String {
    let id = id.unwrap_or("null");
    match result {
        Ok(result) => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{result}}}"#),
        Err(fault) => {
            let mut s = format!(
                r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":{},"message":"#,
                fault.code
            );
            string(&mut s, &fault.message);
            if let Some(data) = fault.data {
                s.push_str(r#","data":"#);
                s.push_str(&data);
            }
            s.push_str("}}");
            s
        }
    }
}

/// `value` as one line of JSON.
fn compact(value: &Value) -> String {
    let mut s = String::new();
    write_value(value, &mut s);
    s
}

fn write_value(value: &Value, s: &mut String) {
    match value {
        Value::Null => s.push_str("null"),
        Value::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
        #[expect(
            clippy::cast_possible_truncation,
            reason = "checked: whole and below 2^53, where f64 is exact"
        )]
        Value::Number(n) if n.fract() == 0.0 && n.abs() < 9e15 => {
            s.push_str(&(*n as i64).to_string());
        }
        Value::Number(n) => s.push_str(&n.to_string()),
        Value::String(text) => string(s, text),
        Value::Array(items) => {
            s.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                write_value(item, s);
            }
            s.push(']');
        }
        Value::Object(fields) => {
            s.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    s.push(',');
                }
                string(s, key);
                s.push(':');
                write_value(item, s);
            }
            s.push('}');
        }
    }
}

/// A tool's result: the objects, and their text rendering.
struct Output {
    structured: String,
    text: String,
}

/// A tool call's arguments.
struct Args<'a>(&'a [(String, Value)]);

impl Args<'_> {
    fn get(&self, key: &str) -> Option<&Value> {
        self.0
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .filter(|v| **v != Value::Null)
    }

    /// Refuses arguments not in `known`, so a misspelled one isn't
    /// silently ignored.
    fn only(&self, known: &[&str]) -> Result<(), String> {
        match self.0.iter().find(|(k, _)| !known.contains(&k.as_str())) {
            Some((key, _)) => Err(format!(
                "unknown argument {key:?}; this tool takes {}",
                known.join(", ")
            )),
            None => Ok(()),
        }
    }

    fn bool(&self, key: &str, default: bool) -> Result<bool, String> {
        match self.get(key) {
            None => Ok(default),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(format!("{key} must be true or false")),
        }
    }

    fn number(&self, key: &str, default: usize, range: [usize; 2]) -> Result<usize, String> {
        let Some(value) = self.get(key) else {
            return Ok(default);
        };
        value
            .as_usize()
            .filter(|n| (range[0]..=range[1]).contains(n))
            .ok_or_else(|| {
                format!(
                    "{key} must be a whole number from {} to {}",
                    range[0], range[1]
                )
            })
    }

    fn string(&self, key: &str) -> Result<Option<&str>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::String(s)) => Ok(Some(s)),
            Some(_) => Err(format!("{key} must be a string")),
        }
    }

    /// A string or an array of strings; a string is split at whitespace
    /// when `split`.
    fn strings(&self, key: &str, split: bool) -> Result<Vec<String>, String> {
        let error = || format!("{key} must be a string or an array of strings");
        match self.get(key) {
            None => Ok(Vec::new()),
            Some(Value::String(s)) if split => {
                Ok(s.split_whitespace().map(str::to_owned).collect())
            }
            Some(Value::String(s)) => Ok(vec![s.clone()]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|v| v.as_str().map(str::to_owned).ok_or_else(error))
                .collect(),
            Some(_) => Err(error()),
        }
    }

    fn cursor(&self) -> Result<Option<Cursor>, String> {
        self.string("cursor")?
            .map(|c| Cursor::decode(c).ok_or_else(|| STALE.to_owned()))
            .transpose()
    }
}

const STALE: &str = "this cursor doesn't continue this request: the arguments differ from the \
    call that returned it, or the diff changed since. Call again without cursor to start over.";

/// A position to continue from, with the fingerprint of the request and of
/// the diff it was taken from. Opaque to clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cursor {
    fingerprint: u64,
    /// A file's index in the list, or a hunk and a line in it.
    at: (usize, usize),
}

impl Cursor {
    fn encode(self) -> String {
        format!("{:016x}.{}.{}", self.fingerprint, self.at.0, self.at.1)
    }

    fn decode(s: &str) -> Option<Self> {
        let mut parts = s.split('.');
        let fingerprint = parts.next().filter(|p| p.len() == 16)?;
        let fingerprint = u64::from_str_radix(fingerprint, 16).ok()?;
        let a = parts.next()?.parse().ok()?;
        let b = parts.next()?.parse().ok()?;
        parts.next().is_none().then_some(Self {
            fingerprint,
            at: (a, b),
        })
    }

    /// The position, when the cursor was taken from a request with
    /// `fingerprint`.
    fn check(self, fingerprint: u64) -> Result<(usize, usize), String> {
        if self.fingerprint == fingerprint {
            Ok(self.at)
        } else {
            Err(STALE.to_owned())
        }
    }
}

/// What every tool resolves: the range and the paths.
struct Range {
    revisions: Vec<String>,
    plan: Plan,
}

impl Server<'_> {
    fn repo(&mut self) -> Result<Repo, String> {
        if let Some(repo) = &self.repo {
            return Ok(repo.clone());
        }
        let repo = Repo::discover(&self.base).map_err(|_| {
            format!(
                "{} isn't in a git repository; start dffy mcp inside one, or pass -C <path>",
                self.base.display()
            )
        })?;
        self.repo = Some(repo.clone());
        Ok(repo)
    }

    /// Resolves `revisions` and `paths` (relative to the repository root)
    /// against the repository as it is now.
    fn range(&mut self, revisions: Vec<String>, paths: &[String]) -> Result<Range, String> {
        if revisions.iter().any(|r| r == "-") {
            return Err("range takes revisions; '-' (a diff on stdin) isn't one".to_owned());
        }
        let repo = self.repo()?;
        let root = repo
            .workdir()
            .map_or_else(|| self.base.clone(), Path::to_owned);
        let target = Revisions {
            unstaged: revisions.is_empty() && self.ctx.config.default_diff == DefaultDiff::Unstaged,
            revs: revisions.clone(),
            ..Revisions::default()
        };
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let plan = Plan::resolve(
            repo,
            root,
            &Target::Diff(&target),
            &paths,
            Summary::Diff,
            false,
        )
        .map_err(message)?;
        Ok(Range { revisions, plan })
    }

    /// Every change of `range`, oriented and with renames paired.
    fn changes(&self, range: &Range) -> Result<Vec<FileChange>, String> {
        let mut all = Vec::new();
        range
            .plan
            .for_each_change(self.ctx, &mut |change| {
                all.extend(change);
                Ok(())
            })
            .map_err(message)?;
        Ok(all)
    }

    fn files(&self, context: usize, syntax: bool, detail: Detail) -> Files<'_> {
        let options = Options {
            context,
            whitespace: self.ctx.config.whitespace,
        };
        Files::new(options, syntax, detail, self.ctx.cache.as_ref())
    }

    fn header(&self, range: &Range, context: usize, files: &Files) -> String {
        let compared = Compared {
            revisions: &range.revisions,
            operands: None,
        };
        json::header(
            &range.plan,
            &compared,
            Context::Lines(context),
            self.ctx.config.renames,
            files,
        )
    }

    fn changed_files(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["range", "paths", "syntax", "counts", "limit", "cursor"])?;
        let revisions = args.strings("range", true)?;
        let paths = args.strings("paths", false)?;
        let syntax = args.bool("syntax", false)?;
        let counts = args.bool("counts", true)?;
        let limit = args.number("limit", 200, [1, 5000])?;
        let cursor = args.cursor()?;
        if syntax && !counts {
            return Err("syntax: true reads every file; it needs counts left on".to_owned());
        }
        let range = self.range(revisions, &paths)?;
        let changes = self.changes(&range)?;
        let mut h = request_hash("changed_files", &range);
        (syntax, counts).hash(&mut h);
        list_hash(&changes, &mut h);
        let fingerprint = h.finish();
        let start = cursor.map_or(Ok((0, 0)), |c| c.check(fingerprint))?.0;
        let end = start.saturating_add(limit).min(changes.len());
        let page = changes.get(start..end).ok_or(STALE)?;
        let files = self.files(3, syntax, Detail::Summary);
        let objects = if counts {
            each(&range.plan.repo, page, |repo, change| {
                files.file_object(repo, change, None).map(|(o, _)| o)
            })
            .map_err(message)?
        } else {
            page.iter().map(|c| files.listed(c.clone())).collect()
        };
        let next = (end < changes.len()).then(|| {
            Cursor {
                fingerprint,
                at: (end, 0),
            }
            .encode()
        });
        let header = self.header(&range, 3, &files);
        let mut structured = format!(r#"{{"header":{header},"files":["#);
        structured.push_str(&objects.join(","));
        let _ = write!(
            structured,
            r#"],"total_files":{},"counts":{counts},"next_cursor":"#,
            changes.len()
        );
        optional_string(&mut structured, next.as_deref());
        structured.push('}');
        let mut text = format!(
            "{} files changed in {}",
            changes.len(),
            self.compared(&range)
        );
        if start > 0 || next.is_some() {
            let _ = write!(text, ", showing {}-{end}", start + 1);
        }
        text.push('\n');
        for object in &objects {
            summary_line(&parse(object.as_bytes()).unwrap_or(Value::Null), &mut text);
        }
        next_line(next.as_deref(), "files", &mut text);
        Ok(Output { structured, text })
    }

    fn file_diff(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["path", "range", "syntax", "context", "max_lines", "cursor"])?;
        let path = args.string("path")?.ok_or(
            "path is required: a file from changed_files, relative to the repository root",
        )?;
        let revisions = args.strings("range", true)?;
        let syntax = args.bool("syntax", false)?;
        let context = args.number("context", 3, [0, 10_000])?;
        let max_lines = args.number("max_lines", 400, [1, 20_000])?;
        let cursor = args.cursor()?;
        let range = self.range(revisions, &[])?;
        let wanted = range.plan.relative(Path::new(path)).map_err(message)?;
        let changes = self.changes(&range)?;
        let change = changes
            .iter()
            .find(|c| c.path == wanted)
            .or_else(|| changes.iter().find(|c| c.old_path() == wanted.as_slice()))
            .cloned()
            .ok_or_else(|| {
                let mut dir = wanted.clone();
                dir.push(b'/');
                if changes.iter().any(|c| c.path.starts_with(&dir)) {
                    format!(
                        "{path} is a directory; call changed_files with paths: [\"{path}\"] \
                         to list its files, then file_diff for each"
                    )
                } else {
                    format!("{path} has no changes in {}", self.compared(&range))
                }
            })?;
        let at = cursor.map_or((0, 0), |c| c.at);
        let window = Window {
            hunk: at.0,
            line: at.1,
            max_lines,
            max_bytes: FILE_BYTES,
        };
        let files = self.files(context, syntax, Detail::Hunks);
        let (object, windowed) = files
            .file_object(&range.plan.repo, change.clone(), Some(window))
            .map_err(message)?;
        let windowed = windowed.ok_or("a file diff came back without hunks")?;
        let fingerprint = file_hash(&range, &change, syntax, context, windowed.digest);
        if let Some(cursor) = cursor {
            cursor.check(fingerprint)?;
        }
        let (next, remaining) = match windowed.cut {
            Some(cut) => (
                Some(
                    Cursor {
                        fingerprint,
                        at: cut.at,
                    }
                    .encode(),
                ),
                cut.remaining,
            ),
            None => (None, 0),
        };
        let header = self.header(&range, context, &files);
        let mut structured = format!(
            r#"{{"header":{header},"file":{object},"truncated":{},"remaining_lines":{remaining},"next_cursor":"#,
            next.is_some()
        );
        optional_string(&mut structured, next.as_deref());
        structured.push('}');
        let mut text = String::new();
        if syntax {
            text.push_str(MARKS);
        }
        file_text(&parse(object.as_bytes()).unwrap_or(Value::Null), &mut text);
        if let Some(next) = &next {
            let _ = writeln!(
                text,
                "[truncated: {remaining} more lines; next_cursor: {next} (pass it as cursor, with the same arguments)]"
            );
        }
        Ok(Output { structured, text })
    }

    fn diff(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&[
            "range",
            "paths",
            "syntax",
            "context",
            "max_files",
            "max_lines_per_file",
            "cursor",
        ])?;
        let revisions = args.strings("range", true)?;
        let paths = args.strings("paths", false)?;
        let syntax = args.bool("syntax", false)?;
        let context = args.number("context", 3, [0, 10_000])?;
        let max_files = args.number("max_files", 20, [1, 200])?;
        let max_lines = args.number("max_lines_per_file", 100, [1, 20_000])?;
        let cursor = args.cursor()?;
        let range = self.range(revisions, &paths)?;
        let changes = self.changes(&range)?;
        let mut h = request_hash("diff", &range);
        (syntax, context).hash(&mut h);
        list_hash(&changes, &mut h);
        let fingerprint = h.finish();
        let start = cursor.map_or(Ok((0, 0)), |c| c.check(fingerprint))?.0;
        let page = changes
            .get(start..start.saturating_add(max_files).min(changes.len()))
            .ok_or(STALE)?;
        let files = self.files(context, syntax, Detail::Hunks);
        let window = Window {
            hunk: 0,
            line: 0,
            max_lines,
            max_bytes: FILE_BYTES,
        };
        let objects = each(&range.plan.repo, page, |repo, change| {
            files.file_object(repo, change, Some(window))
        })
        .map_err(message)?;
        let mut text = String::new();
        if syntax {
            text.push_str(MARKS);
        }
        let (mut shown, mut bytes, mut truncated) = (Vec::new(), 0, Vec::new());
        for (change, (object, windowed)) in page.iter().zip(objects) {
            if !shown.is_empty() && bytes >= PAGE_BYTES {
                break;
            }
            let value = parse(object.as_bytes()).unwrap_or(Value::Null);
            bytes += text_bytes(&value);
            file_text(&value, &mut text);
            if let Some(windowed) = windowed
                && let Some(cut) = windowed.cut
            {
                let fingerprint = file_hash(&range, change, syntax, context, windowed.digest);
                let cursor = Cursor {
                    fingerprint,
                    at: cut.at,
                }
                .encode();
                let path = String::from_utf8_lossy(&change.path).into_owned();
                let _ = writeln!(
                    text,
                    "[truncated: {} more lines; continue with file_diff, path {path:?}, cursor {cursor}]",
                    cut.remaining
                );
                let mut entry = String::from(r#"{"path":"#);
                string(&mut entry, &path);
                let _ = write!(entry, r#","remaining_lines":{},"cursor":"#, cut.remaining);
                string(&mut entry, &cursor);
                entry.push('}');
                truncated.push(entry);
            }
            text.push('\n');
            shown.push(object);
        }
        let end = start + shown.len();
        let next = (end < changes.len()).then(|| {
            Cursor {
                fingerprint,
                at: (end, 0),
            }
            .encode()
        });
        let header = self.header(&range, context, &files);
        let mut structured = format!(r#"{{"header":{header},"files":["#);
        structured.push_str(&shown.join(","));
        structured.push_str(r#"],"truncated_files":["#);
        structured.push_str(&truncated.join(","));
        let total = changes.len();
        let _ = write!(structured, r#"],"total_files":{total},"next_cursor":"#);
        optional_string(&mut structured, next.as_deref());
        structured.push('}');
        let from = (start + 1).min(end);
        let compared = self.compared(&range);
        text.insert_str(
            0,
            &format!("{total} files changed in {compared}, showing {from}-{end}\n"),
        );
        next_line(next.as_deref(), "files", &mut text);
        Ok(Output { structured, text })
    }

    /// What `range` compares, in words.
    fn compared(&self, range: &Range) -> String {
        if !range.revisions.is_empty() {
            return range.revisions.join(" ");
        }
        if self.ctx.config.default_diff == DefaultDiff::Unstaged {
            "the unstaged changes".to_owned()
        } else {
            "the uncommitted changes".to_owned()
        }
    }
}

/// The text rendering's legend for the syntax diff's marks.
const MARKS: &str = "marks after the sign: > moved, ~ reformatted (the code wasn't edited)\n";

/// A hash of what every tool's request resolved to: the tool, the range as
/// typed and as resolved, and the paths.
fn request_hash(tool: &str, range: &Range) -> DefaultHasher {
    let mut h = DefaultHasher::new();
    tool.hash(&mut h);
    range.revisions.hash(&mut h);
    range.plan.spec.hash(&mut h);
    range.plan.paths.hash(&mut h);
    h
}

/// Hashes which files `changes` lists: a working tree has no tree id, so a
/// file appearing or going away shows only here.
fn list_hash(changes: &[FileChange], h: &mut DefaultHasher) {
    changes.len().hash(h);
    for change in changes {
        change.path.hash(h);
        change.old_path().hash(h);
        std::mem::discriminant(&change.status).hash(h);
    }
}

/// The fingerprint of `file_diff` on `change` with `digest` (of its
/// sides), shared by the cursors `diff` hands out for files it cut short.
fn file_hash(range: &Range, change: &FileChange, syntax: bool, context: usize, digest: u64) -> u64 {
    let mut h = DefaultHasher::new();
    "file_diff".hash(&mut h);
    range.revisions.hash(&mut h);
    range.plan.spec.hash(&mut h);
    change.path.hash(&mut h);
    change.old_path().hash(&mut h);
    (syntax, context, digest).hash(&mut h);
    h.finish()
}

/// `work` on each of `changes` across the machine's cores, results in
/// order; the first error, if any.
fn each<T: Send>(
    repo: &Repo,
    changes: &[FileChange],
    work: impl Fn(&Repo, FileChange) -> Result<T, Error> + Sync,
) -> Result<Vec<T>, Error> {
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZero::get)
        .min(changes.len());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let mut done: Vec<(usize, Result<T, Error>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                let (repo, next, work) = (repo.without_object_cache(), &next, &work);
                scope.spawn(move || {
                    let mut done = Vec::new();
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let Some(change) = changes.get(i) else {
                            break done;
                        };
                        done.push((i, work(&repo, change.clone())));
                    }
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|w| w.join().unwrap_or_else(|p| std::panic::resume_unwind(p)))
            .collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

fn message(e: Error) -> String {
    match e {
        Error::Fatal {
            message,
            hint: Some(hint),
        } => format!("{message}\nhint: {hint}"),
        Error::Fatal {
            message,
            hint: None,
        } => message,
        Error::BrokenPipe | Error::Interrupted => "interrupted".to_owned(),
    }
}

fn optional_string(s: &mut String, value: Option<&str>) {
    match value {
        Some(v) => string(s, v),
        None => s.push_str("null"),
    }
}

fn next_line(next: Option<&str>, what: &str, text: &mut String) {
    if let Some(next) = next {
        let _ = writeln!(
            text,
            "More {what}: next_cursor {next} (pass it as cursor, with the same arguments)"
        );
    }
}

fn field<'v>(value: &'v Value, key: &str) -> &'v Value {
    value.get(key).unwrap_or(&Value::Null)
}

/// A file object's status as one letter, as `git diff --name-status` has
/// it; `?` for untracked.
fn letter(status: &str) -> &'static str {
    match status {
        "added" => "A",
        "deleted" => "D",
        "renamed" => "R",
        "type_changed" => "T",
        "untracked" => "?",
        "unmerged" => "U",
        _ => "M",
    }
}

/// A file object's line in a text rendering: status, path, counts and
/// what the diff found.
fn summary_line(file: &Value, text: &mut String) {
    let status = field(file, "status").as_str().unwrap_or_default();
    text.push_str(letter(status));
    text.push(' ');
    text.push_str(field(file, "path").as_str().unwrap_or_default());
    if status == "renamed" {
        let from = field(file, "old_path").as_str().unwrap_or_default();
        let similarity = field(file, "similarity").as_usize().unwrap_or_default();
        let _ = write!(text, " (from {from}, {similarity}%)");
    }
    let counts = (
        field(file, "added").as_usize(),
        field(file, "removed").as_usize(),
    );
    if let (Some(added), Some(removed)) = counts {
        let _ = write!(text, " +{added} -{removed}");
    }
    if field(file, "binary") == &Value::Bool(true) {
        text.push_str(" binary");
    }
    if field(file, "too_large") == &Value::Bool(true) {
        text.push_str(" too large");
    }
    if let Some(class) = field(file, "classification").as_str() {
        let _ = write!(text, " [{class}]");
    }
    if let Some(syntax) = field(file, "syntax").as_str().filter(|s| *s != "complete") {
        let _ = write!(text, " (syntax: {syntax})");
    }
    if let Some(note) = field(file, "note").as_str() {
        let _ = write!(text, " (note: {note})");
    }
    text.push('\n');
}

/// A file object with hunks as unified-diff text: a sign, a mark, then
/// the line.
fn file_text(file: &Value, text: &mut String) {
    summary_line(file, text);
    for hunk in field(file, "hunks").as_array().unwrap_or_default() {
        let n = |key| field(hunk, key).as_usize().unwrap_or_default();
        let _ = write!(
            text,
            "@@ -{},{} +{},{} @@",
            n("old_start"),
            n("old_lines"),
            n("new_start"),
            n("new_lines")
        );
        if let Some(heading) = field(hunk, "heading").as_str() {
            text.push(' ');
            text.push_str(heading);
        }
        text.push('\n');
        for line in field(hunk, "lines").as_array().unwrap_or_default() {
            text.push(match field(line, "kind").as_str() {
                Some("removed") => '-',
                Some("added") => '+',
                _ => ' ',
            });
            text.push(match field(line, "mark").as_str() {
                Some("moved") => '>',
                Some("reformatted") => '~',
                _ => ' ',
            });
            text.push_str(field(line, "text").as_str().unwrap_or_default());
            text.push('\n');
            if field(line, "no_newline") == &Value::Bool(true) {
                text.push_str("\\ No newline at end of file\n");
            }
        }
    }
}

/// Bytes of line text in a file object's hunks.
fn text_bytes(file: &Value) -> usize {
    field(file, "hunks")
        .as_array()
        .unwrap_or_default()
        .iter()
        .flat_map(|h| field(h, "lines").as_array().unwrap_or_default())
        .map(|l| field(l, "text").as_str().map_or(0, str::len))
        .sum()
}

#[cfg(test)]
mod tests {
    use dffy_core::git::Status;

    use super::*;

    #[test]
    fn cursors_round_trip_and_refuse_anything_else() {
        let cursor = Cursor {
            fingerprint: 0x0123_4567_89ab_cdef,
            at: (12, 3),
        };
        let encoded = cursor.encode();
        assert_eq!(Cursor::decode(&encoded), Some(cursor));
        for bad in [
            "",
            "0123456789abcdef.12",
            "0123456789abcdef.12.3.4",
            "123.1.2",
            "0123456789abcdeg.1.2",
            "0123456789abcdef.-1.2",
        ] {
            assert_eq!(Cursor::decode(bad), None, "{bad}");
        }
    }

    /// A cursor continues only the request it came from: the same
    /// fingerprint gives its position back, any other is stale.
    #[test]
    fn a_cursor_from_another_request_is_refused() {
        let cursor = Cursor {
            fingerprint: 7,
            at: (200, 0),
        };
        assert_eq!(cursor.check(7), Ok((200, 0)));
        assert_eq!(cursor.check(8), Err(STALE.to_owned()));
    }

    /// The list's fingerprint changes when a file appears, goes away or
    /// changes status, but not when the same files are listed again.
    #[test]
    fn the_list_fingerprint_follows_which_files_changed() {
        let change = |path: &str, status| FileChange {
            path: path.into(),
            status,
            old: None,
            new: None,
            staging: None,
        };
        let hash = |changes: &[FileChange]| {
            let mut h = DefaultHasher::new();
            list_hash(changes, &mut h);
            h.finish()
        };
        let list = [change("a", Status::Modified), change("b", Status::Added)];
        assert_eq!(hash(&list), hash(&list.clone()));
        assert_ne!(hash(&list), hash(&list[..1]));
        let deleted = [change("a", Status::Modified), change("b", Status::Deleted)];
        assert_ne!(hash(&list), hash(&deleted));
    }

    #[test]
    fn the_tool_list_is_one_line_of_json() {
        let tools = compact(&parse(TOOLS.as_bytes()).expect("valid"));
        assert!(!tools.contains('\n'));
        let names: Vec<_> = parse(tools.as_bytes())
            .expect("compacted is valid")
            .as_array()
            .expect("array")
            .iter()
            .filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_owned))
            .collect();
        assert_eq!(names, ["changed_files", "file_diff", "diff"]);
    }
}

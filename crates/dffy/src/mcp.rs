//! `dffy mcp`: a Model Context Protocol server on stdin and stdout, for
//! coding agents. Its tools return the header and file objects of `--json`
//! (docs/json.md), or the review comments (docs/comments.md), a page at a
//! time, each result also rendered as plain text
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
use crate::encode::{self, Compared, Detail, Files, Window};
use crate::plugin::json::{Value, parse, string, write};
use crate::print::{Plan, Summary, Target};
use crate::render::Canvas;
use crate::review::model::At;
use crate::review::model::snapshot::Snapshot;
use crate::{Ctx, Error, control, review};

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
    only moved code are classified, so you can skip them. list_comments returns the review \
    comments left on the code, with their threads, thread_prompt one thread as a prompt to \
    answer, with its lines and the changes near them, and reply_to_thread answers in a thread. \
    Results are paged: pass next_cursor \
    back as cursor, with the same arguments, to continue. When the user has the dffy viewer \
    open here, open_in_viewer shows them a file at a line, focus_thread a comment thread, \
    viewer_selection reads the lines they selected, and apply_suggestion applies a thread's \
    suggested change to the working tree.";

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
      "comments": {"type": "boolean", "default": false, "description": "Add the file's review comment threads, as list_comments lists them by default, as annotations: each thread with the lines it's on in this diff, or lost when they're gone."},
      "session": {"type": "boolean", "default": false, "description": "With comments: true, the threads of the review session the user has open in the viewer, instead of the local ones."},
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
      "comments": {"type": "boolean", "default": false, "description": "Add each file's review comment threads as annotations, as for file_diff."},
      "session": {"type": "boolean", "default": false, "description": "With comments: true, the review session's threads, as for file_diff."},
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
},
{
  "name": "list_comments",
  "title": "Review comments",
  "description": "List the review comments on the code: comments people left on lines or files of the diff, with their replies and whether each thread is resolved. By default the user's local comments on the current branch, and those on no branch; session: true lists the code review the user has open in the viewer instead. Comments come in threads, each thread's first comment then its replies, threads in path and line order. Each comment has id, path, the side (old or new) and start and end lines it was written on (1-based; none for a comment on a whole file or a reply), text, author (empty means the user), created, in_reply_to for a reply, resolved, and anchor, the commented lines' text. placed has, by the id of a thread's first comment, the lines it's on now in range's diff, for threads whose file that diff changes; lost when its lines are gone. It's filled with range, or with placed: true on the uncommitted changes; otherwise it's empty and nothing is diffed. Read-only. Paged: when next_cursor isn't null, call again with it as cursor and the same arguments.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "range": {"type": ["string", "array"], "items": {"type": "string"}, "description": "The diff to place threads on, as for changed_files; placed is filled when it's given."},
      "placed": {"type": "boolean", "default": false, "description": "Place the threads on the uncommitted changes when range is left out."},
      "path": {"type": "string", "description": "Only the comments on this file, relative to the repository root. A renamed file's old path works too."},
      "open": {"type": "boolean", "description": "true for open threads only, false for resolved ones."},
      "resolved": {"type": "boolean", "description": "true for resolved threads only, false for open ones."},
      "branch": {"type": "string", "description": "The threads of this branch, and those on no branch, instead of the current branch's."},
      "all_branches": {"type": "boolean", "default": false, "description": "Every branch's threads."},
      "session": {"type": "boolean", "default": false, "description": "The comments of the review session the user has open in the viewer, such as a pull request's, instead of the local ones."},
      "limit": {"type": "integer", "minimum": 1, "maximum": 1000, "default": 100, "description": "Comments per page."},
      "cursor": {"type": "string", "description": "next_cursor from the previous page."}
    },
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "notice": {"type": ["string", "null"], "description": "A note from the integration that keeps the comments, such as that they're stale."},
      "comments": {"type": "array", "items": {"type": "object"}, "description": "Comments as the comments file holds them (docs/comments.md)."},
      "placed": {"type": "object", "additionalProperties": {"type": "object", "properties": {"side": {"type": "string"}, "start": {"type": ["integer", "null"]}, "end": {"type": ["integer", "null"]}, "lost": {"type": "boolean"}}}, "description": "Where the line threads starting on this page are in range's diff, by first comment id; empty without range or placed."},
      "total_comments": {"type": "integer"},
      "next_cursor": {"type": ["string", "null"]}
    },
    "required": ["notice", "comments", "placed", "total_comments", "next_cursor"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "thread_prompt",
  "title": "A thread as a prompt",
  "description": "One review comment thread as a prompt to answer: where it is (path, lines and side), its comments with their authors, any suggested change, the commented lines with the lines around them, and the file's changes near them. Takes the id of any comment in the thread, from list_comments. The thread is placed on the uncommitted changes, as plain dffy shows them; when they don't change its file, its lines come as written. At most 32 KiB, cut farthest from the thread first, every cut marked in the text. Read-only.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "id": {"type": "string", "description": "A comment's id, the thread's first or a reply."},
      "session": {"type": "boolean", "default": false, "description": "The thread is in the review session the user has open in the viewer, as list_comments with session: true lists them."}
    },
    "required": ["id"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "thread": {"type": "string", "description": "The id of the thread's first comment."},
      "path": {"type": ["string", "null"], "description": "Its file; null for a comment on the review as a whole."},
      "prompt": {"type": "string"},
      "trimmed": {"type": "boolean", "description": "Something was cut to fit 32 KiB; the prompt marks where."}
    },
    "required": ["thread", "path", "prompt", "trimmed"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "reply_to_thread",
  "title": "Reply in a thread",
  "description": "Add a reply to a review comment thread, by the id of any of its comments (list_comments lists them), under your client's name. Use it to say what you changed for a thread you were asked to address. A suggestion is the replacement for the thread's lines, without a final newline. The reply is never posted to a code host as the user's: they make it theirs first. It never asks another agent, whatever it says.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "id": {"type": "string", "description": "A comment's id, the thread's first or a reply."},
      "text": {"type": "string", "description": "The reply, as Markdown."},
      "suggestion": {"type": "string", "description": "Lines to put in place of the thread's lines; empty removes them."},
      "session": {"type": "boolean", "default": false, "description": "The thread is in the review session the user has open in the viewer, as list_comments with session: true lists them."}
    },
    "required": ["id", "text"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "id": {"type": "string", "description": "The reply's id."},
      "thread": {"type": "string", "description": "The id of the thread's first comment."},
      "author": {"type": "string"}
    },
    "required": ["id", "thread", "author"]
  },
  "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": false, "openWorldHint": false}
},
{
  "name": "apply_suggestion",
  "title": "Apply a suggestion in the viewer",
  "description": "Put a thread's suggested change in place of its lines in the working tree, through the dffy viewer the user has open in this repository, as its s key does: the suggestion of the comment id names, or of the thread's last comment with one. It's refused, writing nothing, when the lines changed since the suggestion was written, when the viewer doesn't show the working tree, or on someone else's review that isn't checked out. The result is an unstaged change. Fails, saying so, when no viewer runs here.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "id": {"type": "string", "description": "A comment's id: one with a suggestion, or any of its thread's."}
    },
    "required": ["id"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "view": {"type": "integer"},
      "thread": {"type": "string"},
      "comment": {"type": "string", "description": "The comment whose suggestion went in."},
      "path": {"type": "string"},
      "start": {"type": "integer", "description": "The first line replaced, 1-based, as the file was."},
      "end": {"type": "integer", "description": "The last line replaced, as the file was."},
      "lines": {"type": "integer", "description": "How many lines went in their place; 0 removed them."}
    },
    "required": ["view", "thread", "comment", "path", "start", "end", "lines"]
  },
  "annotations": {"readOnlyHint": false, "destructiveHint": true, "idempotentHint": false, "openWorldHint": false}
},
{
  "name": "open_in_viewer",
  "title": "Show a file in the viewer",
  "description": "Show a file of the diff in the dffy viewer the user has open in this repository, at a line when given, so the user sees what you're talking about. It answers once the file shows. Fails, saying so, when no viewer runs here, or the file isn't in the diff the viewer shows.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "path": {"type": "string", "description": "The file, relative to the repository root, as changed_files lists it."},
      "line": {"type": "integer", "minimum": 1, "description": "The line to show, 1-based, on side's numbering. Omit to show the file from its first change."},
      "side": {"type": "string", "enum": ["old", "new"], "default": "new", "description": "Which version line numbers: new for the file as changed, old for it before."}
    },
    "required": ["path"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "view": {"type": "integer"},
      "path": {"type": "string"},
      "line": {"type": ["integer", "null"]},
      "side": {"type": "string"}
    },
    "required": ["view", "path", "line", "side"]
  },
  "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "focus_thread",
  "title": "Focus a comment thread in the viewer",
  "description": "Show and focus a review comment thread in the dffy viewer the user has open in this repository, by the id of any of its comments (list_comments lists them). Fails, saying so, when no viewer runs here, or the viewer doesn't show the thread.",
  "inputSchema": {
    "type": "object",
    "properties": {
      "id": {"type": "string", "description": "A comment's id, the thread's first or a reply."}
    },
    "required": ["id"],
    "additionalProperties": false
  },
  "outputSchema": {
    "type": "object",
    "properties": {
      "view": {"type": "integer"},
      "thread": {"type": "string", "description": "The id of the thread's first comment."},
      "path": {"type": "string"},
      "focused": {"type": "boolean", "description": "false while the thread's place in the diff is on its way; it's focused once there."}
    },
    "required": ["view", "thread", "path", "focused"]
  },
  "annotations": {"readOnlyHint": false, "destructiveHint": false, "idempotentHint": true, "openWorldHint": false}
},
{
  "name": "viewer_selection",
  "title": "The viewer's selection",
  "description": "The lines the user selected in the dffy viewer they have open in this repository: the file, the side (new or old), the first and last line (1-based) and their text. Use it when the user refers to what they selected or are looking at. selection is null when nothing is selected. Fails, saying so, when no viewer runs here.",
  "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false},
  "outputSchema": {
    "type": "object",
    "properties": {
      "view": {"type": "integer"},
      "selection": {"type": ["object", "null"], "properties": {"path": {"type": "string"}, "side": {"type": "string"}, "start": {"type": "integer"}, "end": {"type": "integer"}, "text": {"type": "string"}}}
    },
    "required": ["view", "selection"]
  },
  "annotations": {"readOnlyHint": true, "idempotentHint": false, "openWorldHint": false}
}
]"#;

/// Serves the protocol until stdin closes. `dir` is where the repository
/// is looked for, relative to the current directory.
/// `comments` is `--comments`, relative to `dir`.
pub(crate) fn serve(dir: Option<&Path>, comments: Option<&Path>, ctx: &Ctx) -> Result<(), Error> {
    let cwd = std::env::current_dir()
        .map_err(|e| Error::fatal(format!("can't read the current directory: {e}")))?;
    let base = dir.map_or_else(|| cwd.clone(), |dir| cwd.join(dir));
    let mut server = Server {
        repo: Repo::discover(&base).ok(),
        comments: comments.map(|file| base.join(file)),
        base,
        ctx,
        legacy: false,
        client: None,
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
    /// The comments file `--comments` names; else the repository's.
    comments: Option<PathBuf>,
    /// Where dffy looks for the repository.
    base: PathBuf,
    ctx: &'a Ctx,
    /// An `initialize` came: requests without the per-request metadata are
    /// served as the legacy revision it chose.
    legacy: bool,
    /// The client's name, as `initialize`'s `clientInfo` gives it: who the
    /// replies it writes are by.
    client: Option<String>,
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
            let info = params.and_then(|p| p.get("clientInfo"));
            // Shown as the author; `you` is how the user's own comments
            // show.
            self.client = (info.and_then(|i| i.get("name")).and_then(Value::as_str))
                .map(|n| n.chars().filter(|c| !c.is_control()).collect::<String>())
                .map(|n| n.trim().chars().take(64).collect::<String>())
                .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("you"));
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
            "list_comments" => self.list_comments(&args),
            "thread_prompt" => self.thread_prompt(&args),
            "reply_to_thread" => self.reply_to_thread(&args),
            "apply_suggestion" => self.apply_suggestion(&args),
            "open_in_viewer" => self.open_in_viewer(&args),
            "focus_thread" => self.focus_thread(&args),
            "viewer_selection" => self.viewer_selection(&args),
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
    write(&mut s, value);
    s
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
        Ok(self.flag(key)?.unwrap_or(default))
    }

    /// A boolean, `None` when it's left out.
    fn flag(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(_) => Err(format!("{key} must be true or false")),
        }
    }

    /// The diff tools' `comments`, and `session` with it: `None` without
    /// comments, else whether they're the review session's.
    fn comments(&self) -> Result<Option<bool>, String> {
        match (self.bool("comments", false)?, self.bool("session", false)?) {
            (false, true) => Err("session picks the comments file; it needs comments: true".into()),
            (comments, session) => Ok(comments.then_some(session)),
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
        encode::header(
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
        args.only(&[
            "path",
            "range",
            "syntax",
            "context",
            "max_lines",
            "comments",
            "session",
            "cursor",
        ])?;
        let path = args.string("path")?.ok_or(
            "path is required: a file from changed_files, relative to the repository root",
        )?;
        let revisions = args.strings("range", true)?;
        let syntax = args.bool("syntax", false)?;
        let context = args.number("context", 3, [0, 10_000])?;
        let max_lines = args.number("max_lines", 400, [1, 20_000])?;
        let comments = args.comments()?;
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
        let snapshot = comments.map(|session| self.snapshot(session)).transpose()?;
        let mut files = self.files(context, syntax, Detail::Hunks);
        files.comments = snapshot.as_ref().map(|s| (s, false));
        let (object, windowed) = files
            .file_object(&range.plan.repo, change.clone(), Some(window))
            .map_err(message)?;
        let windowed = windowed.ok_or("a file diff came back without hunks")?;
        let fingerprint = file_hash(
            &range,
            &change,
            (syntax, comments),
            context,
            windowed.digest,
        );
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

    #[expect(
        clippy::too_many_lines,
        reason = "the tool in order: arguments, the page of files, then its result and text"
    )]
    fn diff(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&[
            "range",
            "paths",
            "syntax",
            "context",
            "max_files",
            "max_lines_per_file",
            "comments",
            "session",
            "cursor",
        ])?;
        let revisions = args.strings("range", true)?;
        let paths = args.strings("paths", false)?;
        let syntax = args.bool("syntax", false)?;
        let context = args.number("context", 3, [0, 10_000])?;
        let max_files = args.number("max_files", 20, [1, 200])?;
        let max_lines = args.number("max_lines_per_file", 100, [1, 20_000])?;
        let comments = args.comments()?;
        let cursor = args.cursor()?;
        let range = self.range(revisions, &paths)?;
        let changes = self.changes(&range)?;
        let mut h = request_hash("diff", &range);
        (syntax, context, comments).hash(&mut h);
        list_hash(&changes, &mut h);
        let fingerprint = h.finish();
        let start = cursor.map_or(Ok((0, 0)), |c| c.check(fingerprint))?.0;
        let page = changes
            .get(start..start.saturating_add(max_files).min(changes.len()))
            .ok_or(STALE)?;
        let snapshot = comments.map(|session| self.snapshot(session)).transpose()?;
        let mut files = self.files(context, syntax, Detail::Hunks);
        files.comments = snapshot.as_ref().map(|s| (s, false));
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
                let fingerprint =
                    file_hash(&range, change, (syntax, comments), context, windowed.digest);
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

    /// The comments file `list_comments` reads: the review session's with
    /// `session`, else `--comments`, else the local review's, where it
    /// is now: this never moves it.
    fn comments_file(&mut self, session: bool) -> Result<PathBuf, String> {
        if session {
            return review::read_session(&self.repo()?)?
                .map(|pointer| pointer.comments)
                .ok_or_else(|| {
                    "no review session: the viewer hasn't opened one in this repository; \
                     the user opens one with the review plugin's command, as in dffy pr"
                        .to_owned()
                });
        }
        if let Some(file) = &self.comments {
            return Ok(file.clone());
        }
        review::local_file_read_only(&self.repo()?)
            .ok_or_else(|| "the repository has no git directory for a comments file".to_owned())
    }

    /// The threads `comments: true` adds to the diff tools' file objects:
    /// those `list_comments` lists with `session` alone.
    fn snapshot(&mut self, session: bool) -> Result<Snapshot, String> {
        let file = review::read(&self.comments_file(session)?)?;
        let current = review::current_branch(&self.repo()?);
        Ok(Snapshot::new(
            &file,
            review::Branches::On(current.as_deref()),
        ))
    }

    /// Where each line thread starting on `page` is in the diff of
    /// `revisions`, by its first comment's id, in page order: the threads
    /// of `file` that `branches` keeps, on the files that diff changes.
    /// Only the threads' files are listed and diffed. Outside a repository
    /// there's no diff, so none is placed.
    fn placements(
        &mut self,
        revisions: Vec<String>,
        page: &[&review::Comment],
        file: &review::Review,
        branches: review::Branches,
    ) -> Result<Vec<(String, At)>, String> {
        let roots: Vec<&review::Comment> = (page.iter().copied())
            .filter(|c| c.in_reply_to.is_none() && c.range().is_some())
            .collect();
        if roots.is_empty() || self.repo().is_err() {
            return Ok(Vec::new());
        }
        let mut paths: Vec<String> = (roots.iter())
            .flat_map(|c| std::iter::once(&c.path).chain(&c.old_path))
            .cloned()
            .collect();
        paths.sort_unstable();
        paths.dedup();
        let asked = !revisions.is_empty();
        let listed =
            (self.range(revisions, &paths)).and_then(|range| Ok((self.changes(&range)?, range)));
        let (changes, range) = match listed {
            Ok(listed) => listed,
            // Unasked, a diff that can't be made, as without a worktree,
            // places nothing rather than failing the listing.
            Err(_) if !asked => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let snapshot = Snapshot::new(file, branches);
        let changes: Vec<FileChange> = (changes.into_iter())
            .filter(|c| snapshot.has(c, false))
            .collect();
        let options = Options {
            context: 3,
            whitespace: self.ctx.config.whitespace,
        };
        let placed = each(&range.plan.repo, &changes, |repo, change| {
            // A file that can't be read places nothing.
            let Ok(diff) = repo.file_diff(change.clone(), &options) else {
                return Ok(Vec::new());
            };
            Ok((snapshot.on(&change, &diff, false).into_iter())
                .map(|h| (h.thread[0].id.clone(), h.at))
                .collect::<Vec<_>>())
        })
        .map_err(message)?;
        let placed: std::collections::HashMap<String, At> = placed.into_iter().flatten().collect();
        Ok((roots.iter())
            .filter_map(|c| Some((c.id.clone(), placed.get(&c.id)?.clone())))
            .collect())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "the tool in order: arguments, the page of threads, then its result and text"
    )]
    fn list_comments(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&[
            "range",
            "path",
            "open",
            "resolved",
            "branch",
            "all_branches",
            "session",
            "placed",
            "limit",
            "cursor",
        ])?;
        let revisions = args.strings("range", true)?;
        // Placing diffs every file on the page with line threads.
        let place = !revisions.is_empty() || args.bool("placed", false)?;
        let path = args.string("path")?;
        let resolved = match (args.flag("open")?, args.flag("resolved")?) {
            (Some(open), Some(resolved)) if open == resolved => {
                return Err("open and resolved contradict each other; give one".to_owned());
            }
            (Some(open), _) => Some(!open),
            (None, resolved) => resolved,
        };
        let branch = args.string("branch")?;
        let all_branches = args.bool("all_branches", false)?;
        if branch.is_some() && all_branches {
            return Err("branch and all_branches contradict each other; give one".to_owned());
        }
        let session = args.bool("session", false)?;
        let limit = args.number("limit", 100, [1, 1000])?;
        let cursor = args.cursor()?;
        let file = review::read(&self.comments_file(session)?)?;
        let repo = self.repo().ok();
        let current = repo.as_ref().and_then(review::current_branch);
        // Outside a repository there's no branch to keep apart.
        let branches = match (branch, all_branches) {
            (Some(branch), false) => review::Branches::On(Some(branch)),
            (None, false) if repo.is_some() => review::Branches::On(current.as_deref()),
            _ => review::Branches::All,
        };
        let listed = review::threads(file.selected(&review::Select {
            resolved,
            path,
            branches,
        }));
        let here = match branches {
            review::Branches::On(branch) => branch,
            review::Branches::All => current.as_deref(),
        };
        let mut h = DefaultHasher::new();
        "list_comments".hash(&mut h);
        (path, resolved, branches, session, &revisions).hash(&mut h);
        for comment in &listed {
            comment.id.hash(&mut h);
        }
        let fingerprint = h.finish();
        let start = cursor.map_or(Ok((0, 0)), |c| c.check(fingerprint))?.0;
        let end = start.saturating_add(limit).min(listed.len());
        let page = listed.get(start..end).ok_or(STALE)?;
        let next = (end < listed.len()).then(|| {
            Cursor {
                fingerprint,
                at: (end, 0),
            }
            .encode()
        });
        let placed = if place {
            self.placements(revisions, page, &file, branches)?
        } else {
            Vec::new()
        };
        let mut structured = String::from(r#"{"notice":"#);
        optional_string(&mut structured, file.notice.as_deref());
        structured.push_str(r#","comments":["#);
        for (i, comment) in page.iter().enumerate() {
            if i > 0 {
                structured.push(',');
            }
            write(&mut structured, &Value::Object(comment.to_fields()));
        }
        structured.push_str(r#"],"placed":"#);
        placed_object(&placed, page, &mut structured);
        let _ = write!(
            structured,
            r#","total_comments":{},"next_cursor":"#,
            listed.len()
        );
        optional_string(&mut structured, next.as_deref());
        structured.push('}');
        let mut text = format!("{} comments", listed.len());
        if let Some(path) = path {
            let _ = write!(text, " on {path}");
        }
        if start > 0 || next.is_some() {
            let _ = write!(text, ", showing {}-{end}", start + 1);
        }
        text.push('\n');
        if let Some(notice) = &file.notice {
            let _ = writeln!(text, "notice: {notice}");
        }
        let mut canvas = Canvas::default();
        crate::print::comments(page, here, &placed, &mut canvas);
        let mut bytes = Vec::new();
        canvas
            .write_to(&mut bytes, None)
            .map_err(|e| e.to_string())?;
        if !bytes.is_empty() {
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&bytes));
        }
        next_line(next.as_deref(), "comments", &mut text);
        Ok(Output { structured, text })
    }

    fn thread_prompt(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["id", "session"])?;
        let id = args
            .string("id")?
            .ok_or("id is needed: a comment's id, from list_comments")?;
        let session = args.bool("session", false)?;
        let file = review::read(&self.comments_file(session)?)?;
        let prompt = crate::print::thread_prompt(self.repo().ok(), &file, id, self.ctx)
            .ok_or_else(|| {
                let elsewhere = if session {
                    ""
                } else {
                    "; a review session's are listed with session: true"
                };
                format!("No comment has the id {id}: list_comments lists them{elsewhere}.")
            })?;
        let root = file.thread(id).and_then(|t| t.first().copied());
        let mut structured = String::from(r#"{"thread":"#);
        string(&mut structured, root.map_or(id, |c| c.id.as_str()));
        structured.push_str(r#","path":"#);
        optional_string(
            &mut structured,
            root.map(|c| c.path.as_str()).filter(|p| !p.is_empty()),
        );
        structured.push_str(r#","prompt":"#);
        string(&mut structured, &prompt.text);
        let _ = write!(structured, r#","trimmed":{}}}"#, prompt.trimmed);
        Ok(Output {
            structured,
            text: prompt.text,
        })
    }

    /// Writes a reply by the client, through the comments file's one write,
    /// as the control API's `comments/add` does: someone else's, so never a
    /// draft, and never an ask, which only a comment typed in the viewer is.
    fn reply_to_thread(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["id", "text", "suggestion", "session"])?;
        let id = (args.string("id")?)
            .ok_or("id is needed: a comment's id, from list_comments")?
            .to_owned();
        let text = (args.string("text")?)
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or("text is needed")?
            .to_owned();
        let suggestion = args.string("suggestion")?.map(str::to_owned);
        let session = args.bool("session", false)?;
        let file = self.comments_file(session)?;
        let author = self
            .client
            .clone()
            .unwrap_or_else(|| control::client::MCP.to_owned());
        let reply = review::Comment {
            text,
            suggestion,
            author: author.clone(),
            ..review::Comment::default()
        };
        let op = review::Op::Add {
            comment: Box::new(reply),
            reply_to: Some(review::Target::Id(id.clone())),
        };
        let written = review::apply(&file, vec![op]).map_err(|e| {
            let elsewhere = if session {
                ""
            } else {
                "; a review session's are listed with session: true"
            };
            format!("{e}{elsewhere}")
        })?;
        let reply = written.into_iter().next().flatten().unwrap_or_default();
        let thread = (review::read(&file)?.thread(&id))
            .and_then(|t| t.first().map(|c| c.id.clone()))
            .unwrap_or_else(|| id.clone());
        let mut structured = String::from(r#"{"id":"#);
        string(&mut structured, &reply);
        structured.push_str(r#","thread":"#);
        string(&mut structured, &thread);
        structured.push_str(r#","author":"#);
        string(&mut structured, &author);
        structured.push('}');
        Ok(Output {
            structured,
            text: format!("Replied in thread {thread} as {author}: comment {reply}."),
        })
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

/// The viewer tools, through the control API (docs/control.md) to the
/// viewer running in the repository's working tree.
impl Server<'_> {
    /// Asks the viewer `method`; its result, or why there's none in words
    /// the model can relay.
    fn viewer(&mut self, method: &str, params: Vec<(&str, Value)>) -> Result<Value, String> {
        let repo = self.repo()?;
        let root = repo
            .workdir()
            .ok_or("a bare repository has no working tree for a viewer to run in")?;
        let (mut client, view) =
            control::client::reach(root, None, control::client::MCP).map_err(|e| {
                match e.as_str() {
                    control::client::NO_VIEWER => format!(
                        "No dffy viewer is running in {}: ask the user to open dffy there; these \
                     tools drive the viewer they look at.",
                        root.display()
                    ),
                    _ => e,
                }
            })?;
        client.ask(&view, method, params)
    }

    fn open_in_viewer(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["path", "line", "side"])?;
        let path = args.string("path")?.ok_or("path is needed")?.to_owned();
        let mut params = vec![("path", control::text(path.clone()))];
        if args.get("line").is_some() {
            let line = args.number("line", 1, [1, 1 << 31])?;
            params.push(("line", control::number(line as u64)));
        }
        if let Some(side) = args.string("side")? {
            params.push(("side", control::text(side)));
        }
        let result = self.viewer("view/open", params)?;
        let text = match result.get("line").and_then(Value::as_usize) {
            Some(line) => format!("The viewer shows {path} at line {line}."),
            None => format!("The viewer shows {path}."),
        };
        Ok(Output {
            structured: compact(&result),
            text,
        })
    }

    fn focus_thread(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["id"])?;
        let id = args.string("id")?.ok_or("id is needed")?.to_owned();
        let result = self.viewer("view/focusThread", vec![("id", control::text(id.clone()))])?;
        let path = result
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let text = if result.get("focused") == Some(&Value::Bool(true)) {
            format!("The viewer focused the thread of comment {id}, on {path}.")
        } else {
            format!(
                "The viewer shows {path}, and focuses the thread of comment {id} once it's placed there."
            )
        };
        Ok(Output {
            structured: compact(&result),
            text,
        })
    }

    fn apply_suggestion(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&["id"])?;
        let id = args.string("id")?.ok_or("id is needed")?.to_owned();
        let result = self.viewer("comments/applySuggestion", vec![("id", control::text(id))])?;
        let field = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default();
        let number = |key: &str| result.get(key).and_then(Value::as_usize).unwrap_or(0);
        let (path, start, end) = (field("path"), number("start"), number("end"));
        let at = if start == end {
            format!("{path}:{start}")
        } else {
            format!("{path}:{start}-{end}")
        };
        let text = match number("lines") {
            0 => format!("Removed {at}, as comment {} suggested.", field("comment")),
            _ => format!(
                "Applied comment {}'s suggestion to {at}: an unstaged change.",
                field("comment")
            ),
        };
        Ok(Output {
            structured: compact(&result),
            text,
        })
    }

    fn viewer_selection(&mut self, args: &Args) -> Result<Output, String> {
        args.only(&[])?;
        let result = self.viewer("view/selection", Vec::new())?;
        let text = match result.get("selection") {
            Some(s @ Value::Object(_)) => {
                let field = |key: &str| s.get(key).and_then(Value::as_str).unwrap_or_default();
                let line = |key: &str| s.get(key).and_then(Value::as_usize).unwrap_or(0);
                format!(
                    "{} lines {}-{} ({} side):\n{}",
                    field("path"),
                    line("start"),
                    line("end"),
                    field("side"),
                    field("text")
                )
            }
            _ => {
                "Nothing is selected in the viewer: the user selects lines there with v.".to_owned()
            }
        };
        Ok(Output {
            structured: compact(&result),
            text,
        })
    }
}

/// `list_comments`' `placed`: each of `placed`'s threads, starting on
/// `page`, by its first comment's id, as an annotation of `--json` says
/// where it is.
fn placed_object(placed: &[(String, At)], page: &[&review::Comment], s: &mut String) {
    s.push('{');
    let roots = placed
        .iter()
        .filter_map(|(id, at)| Some((id, at, page.iter().find(|c| c.id == *id)?)));
    for (i, (id, at, root)) in roots.enumerate() {
        if i > 0 {
            s.push(',');
        }
        string(s, id);
        s.push_str(":{");
        encode::placement(at, root, false, s);
        s.push('}');
    }
    s.push('}');
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
/// `flags` are `syntax` and `comments` (with `session`).
fn file_hash(
    range: &Range,
    change: &FileChange,
    flags: (bool, Option<bool>),
    context: usize,
    digest: u64,
) -> u64 {
    let mut h = DefaultHasher::new();
    "file_diff".hash(&mut h);
    range.revisions.hash(&mut h);
    range.plan.spec.hash(&mut h);
    change.path.hash(&mut h);
    change.old_path().hash(&mut h);
    (flags, context, digest).hash(&mut h);
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
/// the line; then its annotations' threads, when it has any.
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
    let annotations = field(file, "annotations").as_array().unwrap_or_default();
    if !annotations.is_empty() {
        text.push_str("comments:\n");
    }
    for annotation in annotations {
        let id = field(annotation, "id").as_str().unwrap_or_default();
        let side = field(annotation, "side").as_str().unwrap_or_default();
        let _ = match (
            field(annotation, "start").as_usize(),
            field(annotation, "end").as_usize(),
        ) {
            (Some(first), Some(last)) if first == last => {
                writeln!(text, "  {id} on {side} line {first}:")
            }
            (Some(first), Some(last)) => writeln!(text, "  {id} on {side} lines {first}-{last}:"),
            _ => writeln!(text, "  {id} on the {side} side, lines not found:"),
        };
        for comment in field(annotation, "thread").as_array().unwrap_or_default() {
            let author = match field(comment, "author").as_str().unwrap_or_default() {
                "" => "you",
                author => author,
            };
            let created = field(comment, "created").as_str().unwrap_or_default();
            let mut lines = field(comment, "text").as_str().unwrap_or_default().lines();
            let _ = writeln!(
                text,
                "    {author}, {created}: {}",
                lines.next().unwrap_or_default()
            );
            for line in lines {
                let _ = writeln!(text, "      {line}");
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
        assert_eq!(
            names,
            [
                "changed_files",
                "file_diff",
                "diff",
                "list_comments",
                "thread_prompt",
                "reply_to_thread",
                "apply_suggestion",
                "open_in_viewer",
                "focus_thread",
                "viewer_selection"
            ]
        );
    }
}
